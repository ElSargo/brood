//! Content-addressed plans and transactional local installation.
//!
//! Installing records activation declarations for realm init consumers; it never
//! replaces a realm root or equates local storage with remote activation.
use crate::{ArtifactKind, INSTALLED_MANIFEST_FILE, INSTALLED_ROOT_FILE, Manifest};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

const MAX_INPUT_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    pub schema_version: u32,
    pub manifest: String,
    pub inputs: BTreeMap<PathBuf, Vec<u8>>,
    pub executable_digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prebuilt: Option<String>,
    pub previous_selection: Option<String>,
    pub digest: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub dependencies: BTreeMap<String, Box<Plan>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Installation {
    pub schema_version: u32,
    pub package_id: String,
    pub plan_digest: String,
    pub artifact_tree_digest: String,
    pub knowledge_digest: String,
    pub artifacts: BTreeMap<String, String>,
    pub activation: Vec<ActivationOutcome>,
    #[serde(default)]
    pub dependencies: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ActivationOutcome {
    pub artifact: String,
    pub realm: crate::extension::Realm,
    pub status: String,
    pub reason: String,
}

pub fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn selected(store: &Path, package: &str) -> Result<Option<String>> {
    match fs::read_to_string(store.join("selected").join(package)) {
        Ok(value) => {
            validate_digest(value.trim())?;
            Ok(Some(value.trim().to_owned()))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn validate_digest(value: &str) -> Result<()> {
    ensure!(
        value.len() == 64
            && value
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
        "invalid content digest"
    );
    Ok(())
}

impl Plan {
    fn identity(&self) -> Result<String> {
        let mut base = serde_json::to_vec(&(
            self.schema_version,
            &self.manifest,
            &self.inputs,
            &self.executable_digest,
        ))?;
        if let Some(alternative) = &self.prebuilt {
            base = serde_json::to_vec(&(base, alternative))?;
        }
        if self.dependencies.is_empty() {
            return Ok(digest(&base));
        }
        Ok(digest(&serde_json::to_vec(&(
            base,
            self.dependency_digests(),
        ))?))
    }

    fn dependency_digests(&self) -> BTreeMap<String, String> {
        self.dependencies
            .iter()
            .map(|(package, plan)| (package.clone(), plan.digest.clone()))
            .collect()
    }

    pub fn validate(&self) -> Result<Manifest> {
        let nodes = ordered_plans(self)?;
        ensure!(nodes.len() <= 64, "dependency graph exceeds 64 packages");
        let size = nodes
            .iter()
            .flat_map(|plan| plan.inputs.values())
            .try_fold(0usize, |sum, bytes| sum.checked_add(bytes.len()))
            .context("dependency input size overflow")?;
        ensure!(
            size <= MAX_INPUT_BYTES,
            "dependency graph inputs exceed 64 MiB"
        );
        self.validate_node()
    }

    fn validate_node(&self) -> Result<Manifest> {
        ensure!(self.schema_version == 1, "unsupported plan schema");
        ensure!(
            self.digest == self.identity()?,
            "plan content digest mismatch"
        );
        let manifest = Manifest::from_toml(&self.manifest)?;
        let extension = manifest
            .extension
            .as_ref()
            .context("self-describing extension declaration required")?;
        ensure!(
            extension.dependencies.len() == self.dependencies.len()
                && extension
                    .dependencies
                    .iter()
                    .all(|dependency| self.dependencies.contains_key(&dependency.package)),
            "dependency plan set differs from manifest"
        );
        let alternative = self
            .prebuilt
            .as_ref()
            .map(|id| {
                extension
                    .prebuilt
                    .iter()
                    .find(|alternative| &alternative.id == id)
                    .context("selected prebuilt alternative absent from manifest")
            })
            .transpose()?;
        if let Some(alternative) = alternative {
            ensure!(
                self.executable_digest.is_empty(),
                "prebuilt plan must not claim a build executable"
            );
            ensure!(
                brood_package_contract::incompatible_host(&alternative.host_interfaces).is_none(),
                "prebuilt host requirements incompatible"
            );
            for artifact in alternative.artifacts.values() {
                ensure!(
                    self.inputs
                        .get(&artifact.path)
                        .is_some_and(|bytes| digest(bytes) == artifact.sha256),
                    "prebuilt artifact digest mismatch"
                );
            }
        } else {
            validate_digest(&self.executable_digest)?;
        }
        let required = required_inputs(extension, alternative);
        ensure!(
            required.len() == self.inputs.len()
                && required.iter().all(|path| self.inputs.contains_key(path)),
            "plan input set does not match manifest"
        );
        ensure!(
            self.inputs
                .values()
                .try_fold(0usize, |total, bytes| total.checked_add(bytes.len()))
                .is_some_and(|size| size <= MAX_INPUT_BYTES),
            "plan exceeds input limit"
        );
        if let Some(previous) = &self.previous_selection {
            validate_digest(previous)?;
        }
        Ok(manifest)
    }
}

fn required_inputs(
    extension: &crate::extension::Extension,
    alternative: Option<&crate::extension::Prebuilt>,
) -> std::collections::BTreeSet<PathBuf> {
    let sources: Vec<_> = match alternative {
        Some(alternative) => alternative
            .artifacts
            .values()
            .map(|artifact| artifact.path.clone())
            .collect(),
        None => extension.build.inputs.clone(),
    };
    sources
        .into_iter()
        .chain(extension.documents.iter().map(|doc| doc.path.clone()))
        .chain(
            extension
                .interfaces
                .iter()
                .map(|interface| interface.schema.clone()),
        )
        .chain(extension.configuration_schema.iter().cloned())
        .chain(
            extension
                .diagnostics
                .iter()
                .map(|diagnostic| diagnostic.schema.clone()),
        )
        .collect()
}

/// Resolve local source bytes now; applying never rereads the source checkout.
pub fn plan(manifest_path: &Path, store: &Path) -> Result<Plan> {
    let mut budget = 64usize;
    plan_recursive(manifest_path, store, &mut Vec::new(), &mut budget)
}

fn plan_recursive(
    manifest_path: &Path,
    store: &Path,
    stack: &mut Vec<PathBuf>,
    budget: &mut usize,
) -> Result<Plan> {
    ensure!(
        *budget > 0,
        "dependency graph exceeds 64 resolved manifests"
    );
    *budget -= 1;
    let canonical = manifest_path.canonicalize()?;
    ensure!(!stack.contains(&canonical), "dependency manifest cycle");
    stack.push(canonical);
    let source = fs::read_to_string(manifest_path)?;
    let manifest = Manifest::from_toml(&source)?;
    let extension = manifest
        .extension
        .as_ref()
        .context("self-describing extension declaration required")?;
    let root = manifest_path
        .canonicalize()?
        .parent()
        .context("manifest has no parent")?
        .to_owned();
    // Missing alternatives may fall back; corrupt available bytes never do.
    let alternative = extension.prebuilt.iter().find(|alternative| {
        brood_package_contract::incompatible_host(&alternative.host_interfaces).is_none()
            && alternative
                .artifacts
                .values()
                .all(|artifact| root.join(&artifact.path).exists())
    });
    let mut inputs = BTreeMap::new();
    let mut total = 0usize;
    for path in required_inputs(extension, alternative) {
        let source_path = root
            .join(&path)
            .canonicalize()
            .with_context(|| format!("resolve input {}", path.display()))?;
        ensure!(
            source_path.starts_with(&root),
            "input escapes source root: {}",
            path.display()
        );
        let size = fs::metadata(&source_path)?.len();
        ensure!(size <= MAX_INPUT_BYTES as u64, "input exceeds size limit");
        let bytes = fs::read(source_path)?;
        total = total
            .checked_add(bytes.len())
            .context("input size overflow")?;
        ensure!(total <= MAX_INPUT_BYTES, "package inputs exceed 64 MiB");
        inputs.insert(path, bytes);
    }
    let mut dependencies = BTreeMap::new();
    for dependency in &extension.dependencies {
        let path = root.join(&dependency.manifest).canonicalize()?;
        ensure!(
            path.starts_with(&root),
            "dependency manifest escapes source root"
        );
        let dependency_plan = plan_recursive(&path, store, stack, budget)?;
        ensure!(
            dependency_plan.validate_node()?.package.id.as_str() == dependency.package,
            "dependency manifest has a different package identity"
        );
        dependencies.insert(dependency.package.clone(), Box::new(dependency_plan));
    }
    stack.pop();
    let mut plan = Plan {
        schema_version: 1,
        manifest: source,
        prebuilt: alternative.map(|alternative| alternative.id.clone()),
        executable_digest: if alternative.is_some() {
            String::new()
        } else {
            digest(&fs::read(&extension.build.executable).context(
                "build environment unavailable and no compatible local prebuilt alternative",
            )?)
        },
        previous_selection: selected(store, manifest.package.id.as_str())?,
        inputs,
        digest: String::new(),
        dependencies,
    };
    plan.digest = plan.identity()?;
    plan.validate()?;
    Ok(plan)
}

/// Advisory file locks are released by the OS when an apply process exits.
/// The lock file is never unlinked, avoiding concurrent owners of different inodes.
pub(crate) struct StoreLock(fs::File);

impl Drop for StoreLock {
    fn drop(&mut self) {
        // Explicitly release before closing: another thread can fork while this
        // descriptor is open, briefly retaining it until the child's exec.
        let _ = self.0.unlock();
    }
}

pub(crate) fn store_lock(store: &Path, shared: bool) -> Result<StoreLock> {
    fs::create_dir_all(store)?;
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(store.join("apply.lock"))
        .context("open package store lock; a legacy lock directory requires operator inspection")?;
    if shared {
        file.try_lock_shared()
            .context("package selection mutation is in progress")?;
    } else {
        file.try_lock()
            .context("another package mutation owns the store lock")?;
        recover_publication(store)?;
    }
    Ok(StoreLock(file))
}

const PENDING_PUBLICATION: &str = "pending-publication.json";

/// Complete a previously authorized graph publication, verifying every object
/// before changing another selection. All callers hold the exclusive store lock.
fn recover_publication(store: &Path) -> Result<()> {
    let path = store.join(PENDING_PUBLICATION);
    if !path.exists() {
        return Ok(());
    }
    ensure!(
        fs::metadata(&path)?.len() <= 64 * 1024,
        "publication journal exceeds limit"
    );
    let graph: Vec<(String, String)> = serde_json::from_slice(&fs::read(&path)?)?;
    ensure!(
        !graph.is_empty() && graph.len() <= 64,
        "invalid publication graph size"
    );
    let mut verified = Vec::new();
    let mut ids = std::collections::BTreeSet::new();
    for (index, (package, revision)) in graph.iter().enumerate() {
        ensure!(ids.insert(package), "duplicate publication package");
        let installed = inspect_revision(store, package, revision)?;
        for (dependency, digest) in &installed.dependencies {
            ensure!(
                graph
                    .iter()
                    .take(index)
                    .any(|(id, revision)| id == dependency && revision == digest),
                "publication graph omits an exact dependency or has invalid ordering"
            );
        }
        ensure!(
            fs::read_to_string(store.join("built").join(revision))?
                == installed.artifact_tree_digest,
            "publication requires a verified local build receipt"
        );
        let manifest = Manifest::from_path(
            store
                .join("objects")
                .join(revision)
                .join(INSTALLED_MANIFEST_FILE),
        )?;
        verified.push((manifest, revision));
    }
    let selections = store.join("selected");
    fs::create_dir_all(&selections)?;
    for (manifest, revision) in verified {
        prepare_state_record(store, &manifest, revision)?;
        authorize_local_plan(store, revision)?;
        let mut selection = tempfile::NamedTempFile::new_in(&selections)?;
        selection.write_all(revision.as_bytes())?;
        selection.as_file().sync_all()?;
        selection.persist(selections.join(manifest.package.id.as_str()))?;
    }
    fs::File::open(&selections)?.sync_all()?;
    fs::remove_file(path)?;
    fs::File::open(store)?.sync_all()?;
    Ok(())
}

pub fn recover(store: &Path) -> Result<serde_json::Value> {
    let _lock = store_lock(store, false)?;
    Ok(
        serde_json::json!({"publication": "complete", "activation": "pending endpoint/root acknowledgement"}),
    )
}

/// Dependency-first ordering rejects conflicting versions of the same stable ID.
fn ordered_plans(plan: &Plan) -> Result<Vec<&Plan>> {
    fn visit<'a>(
        plan: &'a Plan,
        output: &mut Vec<&'a Plan>,
        identities: &mut BTreeMap<String, String>,
        path: &mut Vec<String>,
        budget: &mut usize,
    ) -> Result<()> {
        ensure!(*budget > 0, "dependency graph traversal limit reached");
        *budget -= 1;
        let manifest = plan.validate_node()?;
        let id = manifest.package.id.to_string();
        ensure!(!path.contains(&id), "package dependency cycle");
        if let Some(previous) = identities.get(&id) {
            ensure!(
                previous == &plan.digest,
                "conflicting dependency plans for {id}"
            );
            return Ok(());
        }
        path.push(id.clone());
        for (expected, dependency) in &plan.dependencies {
            ensure!(
                dependency.validate_node()?.package.id.as_str() == expected,
                "dependency identity mismatch"
            );
            visit(dependency, output, identities, path, budget)?;
        }
        path.pop();
        identities.insert(id, plan.digest.clone());
        output.push(plan);
        Ok(())
    }
    let mut output = Vec::new();
    visit(
        plan,
        &mut output,
        &mut BTreeMap::new(),
        &mut Vec::new(),
        &mut 256,
    )?;
    Ok(output)
}

fn capture_build_log(
    mut input: impl Read + Send + 'static,
    mut output: fs::File,
    limit: u64,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    exceeded: std::sync::Arc<std::sync::atomic::AtomicBool>,
    failed: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> std::thread::JoinHandle<std::io::Result<()>> {
    std::thread::spawn(move || {
        use std::sync::atomic::Ordering;
        let mut written = 0u64;
        let mut buffer = [0u8; 8192];
        loop {
            match input.read(&mut buffer) {
                Ok(0) => return Ok(()),
                Ok(count) => {
                    let keep =
                        usize::try_from((limit - written).min(count as u64)).unwrap_or(count);
                    output
                        .write_all(&buffer[..keep])
                        .inspect_err(|_| failed.store(true, Ordering::Release))?;
                    written += keep as u64;
                    if keep < count {
                        exceeded.store(true, Ordering::Release);
                        return Ok(());
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if stop.load(Ordering::Acquire) {
                        return Ok(());
                    }
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                Err(error) => {
                    failed.store(true, Ordering::Release);
                    return Err(error);
                }
            }
        }
    })
}

/// A cancelled/failed apply must not leave the declared build running.
struct BuildProcess(std::process::Child);
impl Drop for BuildProcess {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Ok(pid) = i32::try_from(self.0.id()) {
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(pid),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Build the entire resolved graph before publishing any selection. Host
/// catalogues reject observations while this mutation lock is held.
pub fn apply(plan: &Plan, store: &Path) -> Result<Installation> {
    plan.validate()?;
    let nodes = ordered_plans(plan)?;
    fs::create_dir_all(store)?;
    let _lock = store_lock(store, false)?;
    for node in &nodes {
        let manifest = node.validate_node()?;
        let current = selected(store, manifest.package.id.as_str())?;
        ensure!(
            current.as_deref() == Some(&node.digest) || current == node.previous_selection,
            "installation conflict: selection changed since planning for {}",
            manifest.package.id
        );
    }
    let mut built = Vec::new();
    for node in nodes {
        let manifest = node.validate_node()?;
        let cached = inspect_revision(store, manifest.package.id.as_str(), &node.digest).ok();
        if let Some(installed) = cached
            && fs::read_to_string(store.join("built").join(&node.digest))
                .ok()
                .as_deref()
                == Some(&installed.artifact_tree_digest)
        {
            ensure!(
                fs::read_to_string(
                    store
                        .join("objects")
                        .join(&node.digest)
                        .join(INSTALLED_MANIFEST_FILE)
                )? == node.manifest
                    && installed.dependencies == node.dependency_digests(),
                "cached build differs from resolved plan"
            );
            built.push((node, manifest, installed));
            continue;
        }
        let attempts = store.join("attempts");
        fs::create_dir_all(&attempts)?;
        let attempt = tempfile::Builder::new()
            .prefix(&format!("{}-", node.digest))
            .tempdir_in(&attempts)?
            .keep();
        fs::write(attempt.join("plan.json"), serde_json::to_vec_pretty(node)?)?;
        let result = build_and_publish(node, &manifest, store, &attempt);
        if let Err(error) = &result {
            fs::write(
                attempt.join("failure.json"),
                serde_json::to_vec_pretty(
                    &serde_json::json!({"schema_version": 1, "phase": if attempt.join("build-success").exists() {"verify-or-publish"} else {"build"}, "message": format!("{error:#}"), "previous_selection": selected(store, manifest.package.id.as_str())?, "activation": "not-attempted", "graph": plan.digest}),
                )?,
            )?;
        }
        let installed = result
            .with_context(|| format!("installation attempt preserved at {}", attempt.display()))?;
        built.push((node, manifest, installed));
    }
    let graph: Vec<_> = built
        .iter()
        .map(|(node, manifest, _)| (manifest.package.id.to_string(), node.digest.clone()))
        .collect();
    let mut journal = tempfile::NamedTempFile::new_in(store)?;
    journal.write_all(&serde_json::to_vec(&graph)?)?;
    journal.as_file().sync_all()?;
    journal.persist(store.join(PENDING_PUBLICATION))?;
    fs::File::open(store)?.sync_all()?;
    recover_publication(store)?;
    built
        .pop()
        .map(|(_, _, installation)| installation)
        .context("empty installation graph")
}

fn build_and_publish(
    plan: &Plan,
    manifest: &Manifest,
    store: &Path,
    attempt: &Path,
) -> Result<Installation> {
    let extension = manifest.extension.as_ref().context("missing extension")?;

    let workspace = attempt.join("work");
    fs::create_dir(&workspace)?;
    for (path, bytes) in &plan.inputs {
        let destination = workspace.join(path);
        fs::create_dir_all(destination.parent().context("input has no parent")?)?;
        fs::write(destination, bytes)?;
    }
    if plan.prebuilt.is_none() {
        ensure!(
            digest(&fs::read(&extension.build.executable)?) == plan.executable_digest,
            "build executable changed after planning"
        );
        let stdout = fs::File::create(attempt.join("build.stdout"))?;
        let stderr = fs::File::create(attempt.join("build.stderr"))?;
        let mut command = Command::new(&extension.build.executable);
        command
            .args(&extension.build.arguments)
            .env_clear()
            .envs(&extension.build.environment)
            .current_dir(&workspace)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let child = command
            .spawn()
            .context("launch declared build environment")?;
        let mut process = BuildProcess(child);
        let output_pipe = process
            .0
            .stdout
            .take()
            .context("build stdout pipe unavailable")?;
        let error_pipe = process
            .0
            .stderr
            .take()
            .context("build stderr pipe unavailable")?;
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            for fd in [output_pipe.as_raw_fd(), error_pipe.as_raw_fd()] {
                let flags = nix::fcntl::fcntl(fd, nix::fcntl::FcntlArg::F_GETFL)?;
                nix::fcntl::fcntl(
                    fd,
                    nix::fcntl::FcntlArg::F_SETFL(
                        nix::fcntl::OFlag::from_bits_truncate(flags)
                            | nix::fcntl::OFlag::O_NONBLOCK,
                    ),
                )?;
            }
        }
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        let stop = Arc::new(AtomicBool::new(false));
        let exceeded = Arc::new(AtomicBool::new(false));
        let failed = Arc::new(AtomicBool::new(false));
        let output_log = capture_build_log(
            output_pipe,
            stdout,
            extension.build.log_limit_bytes,
            stop.clone(),
            exceeded.clone(),
            failed.clone(),
        );
        let error_log = capture_build_log(
            error_pipe,
            stderr,
            extension.build.log_limit_bytes,
            stop.clone(),
            exceeded.clone(),
            failed.clone(),
        );
        let began = std::time::Instant::now();
        let result = (|| -> Result<_> {
            loop {
                ensure!(!failed.load(Ordering::Acquire), "build log capture failed");
                ensure!(
                    !exceeded.load(Ordering::Acquire),
                    "build exceeded declared per-stream log limit of {} bytes",
                    extension.build.log_limit_bytes
                );
                if let Some(status) = process.0.try_wait()? {
                    return Ok(status);
                }
                ensure!(
                    began.elapsed()
                        < std::time::Duration::from_secs(extension.build.timeout_seconds),
                    "build exceeded declared timeout of {} seconds",
                    extension.build.timeout_seconds
                );
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
        })();
        // Retire helpers before draining the final buffered output. Nonblocking
        // readers stop even when a detached process retains an inherited pipe.
        drop(process);
        stop.store(true, Ordering::Release);
        let stdout_result = output_log
            .join()
            .map_err(|_| anyhow::anyhow!("stdout capture panicked"));
        let stderr_result = error_log
            .join()
            .map_err(|_| anyhow::anyhow!("stderr capture panicked"));
        stdout_result??;
        stderr_result??;
        ensure!(
            !exceeded.load(Ordering::Acquire),
            "build exceeded declared per-stream log limit of {} bytes",
            extension.build.log_limit_bytes
        );
        let status = result?;
        ensure!(status.success(), "build failed: {status}");
    } else {
        fs::write(
            attempt.join("prebuilt.json"),
            serde_json::to_vec(&plan.prebuilt)?,
        )?;
    }
    fs::write(attempt.join("build-success"), b"")?;
    let tree = attempt.join("package");
    fs::create_dir(&tree)?;
    let mut artifacts = BTreeMap::new();
    let workspace = workspace.canonicalize()?;
    let mut output_bytes = 0u64;
    for artifact in manifest.installation_order()? {
        let source_path = plan
            .prebuilt
            .as_ref()
            .and_then(|id| {
                extension
                    .prebuilt
                    .iter()
                    .find(|alternative| &alternative.id == id)
            })
            .and_then(|alternative| alternative.artifacts.get(artifact.id.as_str()))
            .map(|prebuilt| &prebuilt.path)
            .unwrap_or(&artifact.source);
        let source = workspace
            .join(source_path)
            .canonicalize()
            .context("declared build output missing")?;
        ensure!(
            source.starts_with(&workspace),
            "build output escapes workspace"
        );
        let metadata = fs::metadata(&source)?;
        ensure!(metadata.is_file(), "build output must be a regular file");
        output_bytes = output_bytes
            .checked_add(metadata.len())
            .context("build output size overflow")?;
        ensure!(
            output_bytes <= MAX_INPUT_BYTES as u64,
            "build artifact outputs exceed 64 MiB"
        );
        let bytes = fs::read(&source)?;
        let component = bytes.get(..8) == Some(b"\0asm\x0d\0\x01\0");
        ensure!(
            component == (artifact.kind == ArtifactKind::WasmComponent),
            "artifact {} container differs from declaration",
            artifact.id
        );
        wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all())
            .validate_all(&bytes)
            .with_context(|| format!("validate artifact {}", artifact.id))?;
        let destination = tree.join(&artifact.placement);
        fs::create_dir_all(destination.parent().context("artifact has no parent")?)?;
        fs::write(destination, &bytes)?;
        artifacts.insert(artifact.id.to_string(), digest(&bytes));
    }
    let documents: Vec<_> = extension.documents.iter().map(|doc| {
        let bytes = plan.inputs.get(&doc.path).context("document absent from plan")?;
        Ok(serde_json::json!({"id": doc.id, "category": doc.category, "references": doc.references, "schemas": doc.schemas, "content": std::str::from_utf8(bytes)?}))
    }).collect::<Result<_>>()?;
    let schemas: BTreeMap<_, _> = extension
        .interfaces
        .iter()
        .map(|interface| {
            let bytes = plan
                .inputs
                .get(&interface.schema)
                .context("interface schema absent")?;
            let schema: serde_json::Value =
                serde_json::from_slice(bytes).context("interface schema must be JSON")?;
            jsonschema::validator_for(&schema).context("invalid interface JSON Schema")?;
            Ok((interface.id.clone(), schema))
        })
        .collect::<Result<_>>()?;
    let diagnostics: BTreeMap<_, _> = extension
        .diagnostics
        .iter()
        .map(|diagnostic| -> Result<_> {
            let schema: serde_json::Value = serde_json::from_slice(
                plan.inputs
                    .get(&diagnostic.schema)
                    .context("diagnostic schema absent")?,
            )?;
            jsonschema::validator_for(&schema).context("invalid diagnostic JSON Schema")?;
            Ok((diagnostic.id.clone(), schema))
        })
        .collect::<Result<_>>()?;
    let configuration = extension
        .configuration_schema
        .as_ref()
        .map(|path| -> Result<serde_json::Value> {
            Ok(serde_json::from_slice(
                plan.inputs
                    .get(path)
                    .context("configuration schema absent")?,
            )?)
        })
        .transpose()?;
    if let Some(schema) = &configuration {
        jsonschema::validator_for(schema).context("invalid configuration JSON Schema")?;
    }
    let knowledge = serde_json::to_vec_pretty(&serde_json::json!({"schema_version": 1,
        "manifest": manifest, "plan_digest": plan.digest, "prebuilt": plan.prebuilt, "artifacts": artifacts,
        "documents": documents, "schemas": schemas, "diagnostic_schemas": diagnostics, "configuration_schema": configuration, "dependencies": plan.dependency_digests()}))?;
    let knowledge_digest = digest(&knowledge);
    fs::write(tree.join("brood-knowledge.json"), &knowledge)?;
    fs::write(tree.join(INSTALLED_MANIFEST_FILE), &plan.manifest)?;
    fs::write(
        tree.join(INSTALLED_ROOT_FILE),
        manifest
            .root_artifact()?
            .placement
            .to_str()
            .context("non-UTF8 placement")?,
    )?;
    let tree_digest = digest(&serde_json::to_vec(&(
        &artifacts,
        &knowledge_digest,
        &plan.manifest,
    ))?);
    let installation = Installation {
        schema_version: 1,
        package_id: manifest.package.id.to_string(),
        plan_digest: plan.digest.clone(),
        artifact_tree_digest: tree_digest,
        dependencies: plan.dependency_digests(),
        knowledge_digest,
        artifacts,
        activation: extension
            .activation
            .iter()
            .map(|activation| ActivationOutcome {
                artifact: activation.artifact.clone(),
                realm: activation.realm,
                status: "pending".into(),
                reason: "awaiting acknowledgement from an authorized realm init actor".into(),
            })
            .collect(),
    };
    fs::write(
        tree.join("installation.json"),
        serde_json::to_vec_pretty(&installation)?,
    )?;
    let objects = store.join("objects");
    fs::create_dir_all(&objects)?;
    let object = objects.join(&plan.digest);
    if object.exists() {
        let previous: Installation =
            serde_json::from_slice(&fs::read(object.join("installation.json"))?)?;
        ensure!(
            previous.artifact_tree_digest == installation.artifact_tree_digest,
            "build is nondeterministic for the same resolved plan"
        );
        verify_object(&object, &previous)?;
    } else {
        fs::rename(&tree, &object)?;
    }
    fs::create_dir_all(store.join("built"))?;
    fs::write(
        store.join("built").join(&plan.digest),
        &installation.artifact_tree_digest,
    )?;
    Ok(installation)
}

fn validate_knowledge(manifest: &Manifest, knowledge: &serde_json::Value) -> Result<()> {
    let described: Manifest = serde_json::from_value(
        knowledge
            .get("manifest")
            .context("knowledge manifest missing")?
            .clone(),
    )?;
    ensure!(
        &described == manifest,
        "knowledge manifest differs from installed manifest"
    );
    let extension = manifest.extension.as_ref().context("missing extension")?;
    let documents = knowledge
        .get("documents")
        .and_then(serde_json::Value::as_array)
        .context("knowledge documents missing")?;
    ensure!(
        documents.len() == extension.documents.len(),
        "knowledge document set differs from manifest"
    );
    let mut ids = std::collections::BTreeSet::new();
    for document in documents {
        let id = document
            .get("id")
            .and_then(serde_json::Value::as_str)
            .context("document ID missing")?;
        ensure!(ids.insert(id), "duplicate installed knowledge document");
        let declared = extension
            .documents
            .iter()
            .find(|declared| declared.id == id)
            .context("undeclared installed document")?;
        ensure!(
            document.get("category").and_then(serde_json::Value::as_str)
                == Some(&declared.category)
                && document
                    .get("content")
                    .is_some_and(serde_json::Value::is_string),
            "document metadata differs from manifest"
        );
        for (key, expected) in [
            ("references", &declared.references),
            ("schemas", &declared.schemas),
        ] {
            let actual: Vec<String> = document
                .get(key)
                .map(|value| serde_json::from_value(value.clone()))
                .transpose()?
                .unwrap_or_default();
            ensure!(&actual == expected, "document {key} differ from manifest");
        }
    }
    for (key, expected) in [
        (
            "schemas",
            extension
                .interfaces
                .iter()
                .map(|interface| interface.id.as_str())
                .collect::<std::collections::BTreeSet<_>>(),
        ),
        (
            "diagnostic_schemas",
            extension
                .diagnostics
                .iter()
                .map(|diagnostic| diagnostic.id.as_str())
                .collect(),
        ),
    ] {
        let actual: std::collections::BTreeSet<_> = knowledge
            .get(key)
            .and_then(serde_json::Value::as_object)
            .into_iter()
            .flat_map(|object| object.keys().map(String::as_str))
            .collect();
        ensure!(
            actual == expected,
            "knowledge {key} set differs from manifest"
        );
    }
    ensure!(
        knowledge
            .get("configuration_schema")
            .is_some_and(|schema| !schema.is_null())
            == extension.configuration_schema.is_some(),
        "configuration schema association differs from manifest"
    );
    Ok(())
}

fn verify_object(object: &Path, installation: &Installation) -> Result<()> {
    let source = fs::read_to_string(object.join(INSTALLED_MANIFEST_FILE))?;
    let manifest = Manifest::from_toml(&source)?;
    ensure!(
        manifest.package.id.as_str() == installation.package_id,
        "installed package identity mismatch"
    );
    ensure!(
        digest(&fs::read(object.join("brood-knowledge.json"))?) == installation.knowledge_digest,
        "knowledge content digest mismatch"
    );
    ensure!(
        fs::read_to_string(object.join(INSTALLED_ROOT_FILE))?
            == manifest
                .root_artifact()?
                .placement
                .to_str()
                .context("non-UTF8 root placement")?,
        "root placement metadata mismatch"
    );
    let knowledge: serde_json::Value =
        serde_json::from_slice(&fs::read(object.join("brood-knowledge.json"))?)?;
    validate_knowledge(&manifest, &knowledge)?;
    ensure!(
        knowledge
            .get("plan_digest")
            .and_then(serde_json::Value::as_str)
            == Some(&installation.plan_digest),
        "knowledge installation identity mismatch"
    );
    let knowledge_artifacts: BTreeMap<String, String> = serde_json::from_value(
        knowledge
            .get("artifacts")
            .context("knowledge artifacts missing")?
            .clone(),
    )?;
    ensure!(
        knowledge_artifacts == installation.artifacts,
        "knowledge artifact identities differ from installation"
    );
    let dependencies: BTreeMap<String, String> = knowledge
        .get("dependencies")
        .map(|value| serde_json::from_value(value.clone()))
        .transpose()?
        .unwrap_or_default();
    ensure!(
        dependencies == installation.dependencies,
        "dependency knowledge differs from installation"
    );
    let declared = &manifest
        .extension
        .as_ref()
        .context("missing extension contract")?
        .dependencies;
    ensure!(
        declared.len() == dependencies.len()
            && declared
                .iter()
                .all(|dependency| dependencies.contains_key(&dependency.package)),
        "installed dependency set differs from manifest"
    );
    for value in dependencies.values() {
        validate_digest(value)?;
    }
    let mut artifacts = BTreeMap::new();
    for artifact in &manifest.artifacts {
        artifacts.insert(
            artifact.id.to_string(),
            digest(&fs::read(object.join(&artifact.placement))?),
        );
    }
    ensure!(
        artifacts == installation.artifacts,
        "artifact content digest mismatch"
    );
    ensure!(
        digest(&serde_json::to_vec(&(
            &artifacts,
            &installation.knowledge_digest,
            &source
        ))?) == installation.artifact_tree_digest,
        "package tree digest mismatch"
    );
    Ok(())
}

/// Inspect selected immutable artifacts even when no runtime is reachable.
pub fn inspect(store: &Path, package: &str) -> Result<Installation> {
    validate_package_id(package)?;
    let selected = selected(store, package)?.context("package is not installed")?;
    inspect_revision(store, package, &selected)
}

/// Exact historical descriptions remain available after replacement or removal.
pub fn inspect_revision(store: &Path, package: &str, selected: &str) -> Result<Installation> {
    validate_package_id(package)?;
    validate_digest(selected)?;
    let object = store.join("objects").join(selected);
    let installation: Installation =
        serde_json::from_slice(&fs::read(object.join("installation.json"))?)?;
    ensure!(
        installation.package_id == package && installation.plan_digest == selected,
        "selected installation identity mismatch"
    );
    verify_object(&object, &installation)?;
    Ok(installation)
}

/// Inspectable plan without expanding source files into JSON byte arrays.
pub fn describe_plan(plan: &Plan) -> Result<serde_json::Value> {
    let manifest = plan.validate()?;
    let inputs: Vec<_> = plan
        .inputs
        .iter()
        .map(|(path, bytes)| {
            serde_json::json!({
                "path": path, "digest": digest(bytes), "bytes": bytes.len()
            })
        })
        .collect();
    Ok(
        serde_json::json!({"schema_version": 1, "plan_digest": plan.digest,
        "host_compatibility": {"interfaces": brood_package_contract::HOST_INTERFACES, "blocked": manifest.extension.as_ref().and_then(|extension| brood_package_contract::incompatible_host(&extension.host_interfaces))},
        "prebuilt": plan.prebuilt, "manifest": manifest, "resolved_inputs": inputs, "executable_digest": plan.executable_digest,
        "dependencies": plan.dependency_digests(), "installation_order": ordered_plans(plan)?.iter().map(|node| node.validate_node().map(|manifest| manifest.package.id.to_string())).collect::<Result<Vec<_>>>()?,
        "previous_selection": plan.previous_selection,
        "build_authority": "local user in the declared build environment",
        "runtime_grants": "requested capabilities require destination authorization",
        "activation": "pending realm init acknowledgement"}),
    )
}

/// Verify artifacts before exposing their exact bundled documentation and schemas.
pub fn knowledge(store: &Path, package: &str) -> Result<serde_json::Value> {
    let installation = inspect(store, package)?;
    knowledge_revision(store, package, &installation.plan_digest)
}

pub fn knowledge_revision(
    store: &Path,
    package: &str,
    revision: &str,
) -> Result<serde_json::Value> {
    let installation = inspect_revision(store, package, revision)?;
    Ok(serde_json::from_slice(&fs::read(
        store
            .join("objects")
            .join(installation.plan_digest)
            .join("brood-knowledge.json"),
    )?)?)
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum SelectionAction {
    Enable,
    Disable,
    Remove,
}

/// Selection changes request convergence from the existing roots. They do not
/// claim that a disconnected host has acknowledged teardown or delete state.
pub fn change_selection(
    store: &Path,
    package: &str,
    action: SelectionAction,
) -> Result<serde_json::Value> {
    validate_package_id(package)?;
    fs::create_dir_all(store)?;
    let _lock = store_lock(store, false)?;
    let current = selected(store, package)?;
    let removed_path = store.join("removed").join(package);
    let digest = match current.as_ref() {
        Some(digest) => {
            if action == SelectionAction::Enable {
                inspect(store, package)?;
            }
            digest.clone()
        }
        None if action == SelectionAction::Remove => fs::read_to_string(&removed_path)
            .context("package has no selected or retained removal record")?,
        None => bail!("package is not selected; apply an installation before enabling it"),
    };
    validate_digest(digest.trim())?;
    let catalogue = crate::catalogue::load_while_locked(store).unwrap_or_else(|error| {
        crate::catalogue::Catalogue {
            failures: vec![format!("dependent inspection unavailable: {error:#}")],
            ..crate::catalogue::Catalogue::default()
        }
    });
    let provided: Vec<_> = catalogue
        .packages
        .iter()
        .filter(|candidate| candidate.manifest.package.id.as_str() == package)
        .flat_map(|candidate| {
            candidate.interfaces.values().filter(|interface| {
                candidate
                    .manifest
                    .extension
                    .as_ref()
                    .is_some_and(|extension| {
                        extension
                            .activation
                            .iter()
                            .any(|activation| activation.provides.contains(&interface.id))
                    })
            })
        })
        .collect();
    let mut dependents: Vec<_> = catalogue.packages.iter().filter(|candidate| candidate.manifest.package.id.as_str() != package).flat_map(|candidate| {
        let provided = &provided;
        candidate.manifest.extension.iter().flat_map(move |extension| extension.activation.iter().filter_map(move |activation| {
            let required: Vec<_> = activation.requires.iter().filter(|id| candidate.interfaces.get(*id).is_some_and(|required| provided.iter().any(|available| available.id == required.id && available.version == required.version && available.schema_digest == required.schema_digest))).collect();
            (!required.is_empty()).then(|| serde_json::json!({"package": candidate.manifest.package.id, "artifact": activation.artifact, "realm": activation.realm, "services": required, "effect": "readiness may become pending; actual peers are reported by live instance queries"}))
        }))
    }).collect();
    dependents.extend(catalogue.packages.iter().filter(|candidate| candidate.installation.dependencies.contains_key(package)).map(|candidate| serde_json::json!({"package": candidate.manifest.package.id, "dependency": package, "effect": "activation remains pending while the exact dependency installation is unavailable or disabled"})));
    let disabled = store.join("disabled").join(package);
    let changed = match action {
        SelectionAction::Enable => match fs::remove_file(&disabled) {
            Ok(()) => true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => return Err(error.into()),
        },
        SelectionAction::Disable => {
            fs::create_dir_all(store.join("disabled"))?;
            if disabled.exists() {
                false
            } else {
                fs::write(&disabled, b"disabled\n")?;
                true
            }
        }
        SelectionAction::Remove => {
            fs::create_dir_all(store.join("removed"))?;
            if current.is_some() {
                let mut record = tempfile::NamedTempFile::new_in(store.join("removed"))?;
                std::io::Write::write_all(&mut record, digest.as_bytes())?;
                record.persist(&removed_path)?;
                fs::remove_file(store.join("selected").join(package))?;
                if disabled.exists() {
                    fs::remove_file(&disabled)?;
                }
                true
            } else {
                false
            }
        }
    };
    Ok(
        serde_json::json!({"schema_version": 1, "package": package, "installation": digest.trim(), "action": action, "changed": changed,
        "selection": match action { SelectionAction::Enable => "enabled", SelectionAction::Disable => "disabled", SelectionAction::Remove => "removed" },
        "activation": "pending endpoint/root acknowledgement; inspect live instances", "potential_dependents": dependents, "inspection_failures": catalogue.failures,
        "retained": {"artifacts": true, "knowledge": true, "build_logs": true, "mutable_state": "retained; selection changes do not roll back or delete plugin state"}}),
    )
}

pub fn selection_status(store: &Path, package: &str) -> Result<&'static str> {
    validate_package_id(package)?;
    Ok(if selected(store, package)?.is_none() {
        "removed"
    } else if store.join("disabled").join(package).exists() {
        "disabled"
    } else {
        "enabled"
    })
}

fn validate_package_id(package: &str) -> Result<()> {
    ensure!(
        !package.is_empty()
            && package.bytes().all(|byte| byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || b".-".contains(&byte))
            && !package.starts_with('.'),
        "invalid package ID"
    );
    Ok(())
}

pub fn revision_status(store: &Path, package: &str, revision: &str) -> Result<&'static str> {
    validate_package_id(package)?;
    validate_digest(revision)?;
    if selected(store, package)?.as_deref() == Some(revision) {
        selection_status(store, package)
    } else {
        Ok("historical")
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Bundle {
    version: u32,
    installation: Installation,
    files: BTreeMap<PathBuf, Vec<u8>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DestinationAuthorization {
    pub realms: Vec<crate::extension::Realm>,
    pub capabilities: Vec<String>,
    pub source: String,
}

/// Export verified artifacts and their exact knowledge together; source/build
/// inputs are deliberately not substituted for already built artifact bytes.
pub fn export_bundle(
    store: &Path,
    package: &str,
    revision: Option<&str>,
    output: &Path,
) -> Result<()> {
    let installation = match revision {
        Some(revision) => inspect_revision(store, package, revision)?,
        None => inspect(store, package)?,
    };
    let root = store.join("objects").join(&installation.plan_digest);
    let manifest = Manifest::from_path(root.join(INSTALLED_MANIFEST_FILE))?;
    let paths = manifest
        .artifacts
        .iter()
        .map(|artifact| artifact.placement.clone())
        .chain([
            PathBuf::from(INSTALLED_MANIFEST_FILE),
            PathBuf::from(INSTALLED_ROOT_FILE),
            PathBuf::from("brood-knowledge.json"),
        ]);
    let mut files = BTreeMap::new();
    for path in paths {
        files.insert(path.clone(), fs::read(root.join(path))?);
    }
    let bundle = serde_json::to_vec(&Bundle {
        version: 1,
        installation,
        files,
    })?;
    ensure!(
        bundle.len() + brood_package_contract::BUNDLE_MAGIC.len() <= 16 * 1024 * 1024,
        "package exceeds artifact transport size limit"
    );
    let mut file = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(output)?;
    file.write_all(brood_package_contract::BUNDLE_MAGIC)?;
    file.write_all(&bundle)?;
    file.sync_all()?;
    Ok(())
}

pub fn destination_authorization(
    store: &Path,
    revision: &str,
) -> Result<Option<DestinationAuthorization>> {
    validate_digest(revision)?;
    match fs::read(store.join("authorization").join(revision)) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// The caller supplies destination-owned authorization from its authenticated
/// artifact exchange, never capability claims from the incoming manifest.
pub fn import_bundle(
    store: &Path,
    expected_package: &str,
    input: &Path,
    authorization: DestinationAuthorization,
) -> Result<serde_json::Value> {
    validate_package_id(expected_package)?;
    ensure!(
        !authorization.realms.is_empty(),
        "destination has not authorized any realms"
    );
    ensure!(
        fs::metadata(input)?.len() <= 16 * 1024 * 1024,
        "package bundle exceeds transport limit"
    );
    let bytes = fs::read(input)?;
    let payload = bytes
        .strip_prefix(brood_package_contract::BUNDLE_MAGIC)
        .context("not a package bundle")?;
    let bundle: Bundle = serde_json::from_slice(payload)?;
    ensure!(
        bundle.version == 1 && bundle.installation.schema_version == 1,
        "unsupported package bundle version"
    );
    ensure!(
        bundle.installation.package_id == expected_package,
        "bundle package differs from destination authorization"
    );
    validate_digest(&bundle.installation.plan_digest)?;
    ensure!(bundle.files.len() <= 259, "too many bundle files");
    let source = std::str::from_utf8(
        bundle
            .files
            .get(Path::new(INSTALLED_MANIFEST_FILE))
            .context("bundle manifest missing")?,
    )?;
    let manifest = Manifest::from_toml(source)?;
    ensure!(
        manifest.package.id.as_str() == expected_package,
        "bundle manifest identity mismatch"
    );
    let expected: std::collections::BTreeSet<_> = manifest
        .artifacts
        .iter()
        .map(|artifact| artifact.placement.clone())
        .chain([
            PathBuf::from(INSTALLED_MANIFEST_FILE),
            PathBuf::from(INSTALLED_ROOT_FILE),
            PathBuf::from("brood-knowledge.json"),
        ])
        .collect();
    ensure!(
        expected.len() == bundle.files.len()
            && bundle.files.keys().all(|path| expected.contains(path)),
        "bundle file set differs from manifest"
    );
    let extension = manifest
        .extension
        .as_ref()
        .context("bundle lacks extension contract")?;
    for activation in &extension.activation {
        if authorization.realms.contains(&activation.realm) {
            let artifact = manifest
                .artifact(&activation.artifact)
                .context("activation artifact missing")?;
            ensure!(
                artifact
                    .capabilities
                    .iter()
                    .all(|capability| authorization.capabilities.contains(&capability.to_string())),
                "destination has not granted requested artifact capabilities"
            );
        }
    }
    fs::create_dir_all(store)?;
    let _lock = store_lock(store, false)?;
    let staging = tempfile::tempdir_in(store)?;
    for (path, bytes) in &bundle.files {
        let target = staging.path().join(path);
        fs::create_dir_all(target.parent().context("bundle member has no parent")?)?;
        fs::write(target, bytes)?;
    }
    fs::write(
        staging.path().join("installation.json"),
        serde_json::to_vec_pretty(&bundle.installation)?,
    )?;
    verify_object(staging.path(), &bundle.installation)?;
    // Validate executable shape and schemas before publishing selection.
    for artifact in &manifest.artifacts {
        let bytes = bundle
            .files
            .get(&artifact.placement)
            .context("artifact bytes absent")?;
        wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all())
            .validate_all(bytes)?;
        ensure!(
            (bytes.get(..8) == Some(b"\0asm\x0d\0\x01\0"))
                == (artifact.kind == ArtifactKind::WasmComponent),
            "received artifact container differs from manifest"
        );
    }
    let knowledge: serde_json::Value = serde_json::from_slice(
        bundle
            .files
            .get(Path::new("brood-knowledge.json"))
            .context("knowledge missing")?,
    )?;
    for interface in &extension.interfaces {
        jsonschema::validator_for(
            knowledge
                .get("schemas")
                .and_then(|schemas| schemas.get(&interface.id))
                .context("interface schema missing")?,
        )?;
    }
    for diagnostic in &extension.diagnostics {
        jsonschema::validator_for(
            knowledge
                .get("diagnostic_schemas")
                .and_then(|schemas| schemas.get(&diagnostic.id))
                .context("diagnostic schema missing")?,
        )?;
    }
    if let Some(schema) = knowledge
        .get("configuration_schema")
        .filter(|schema| !schema.is_null())
    {
        jsonschema::validator_for(schema)?;
    }
    let revision = &bundle.installation.plan_digest;
    let object = store.join("objects").join(revision);
    fs::create_dir_all(store.join("objects"))?;
    if object.exists() {
        let existing = inspect_revision(store, expected_package, revision)?;
        ensure!(
            existing.artifact_tree_digest == bundle.installation.artifact_tree_digest,
            "same installation identity has different artifact contents"
        );
    } else {
        fs::rename(staging.path(), &object)?;
    }
    prepare_state_record(store, &manifest, revision)?;
    let current = selected(store, expected_package)?;
    let mut effective = authorization;
    if let Some(previous) = destination_authorization(store, revision)? {
        effective.realms.extend(previous.realms);
        effective.realms.sort();
        effective.realms.dedup();
        effective.capabilities.extend(previous.capabilities);
        effective.capabilities.sort();
        effective.capabilities.dedup();
    }
    // A pre-existing local selection has local-user authorization. Receiving the
    // same object does not revoke that authority; new remote selections are scoped.
    if current.as_deref() != Some(revision) || destination_authorization(store, revision)?.is_some()
    {
        fs::create_dir_all(store.join("authorization"))?;
        let mut receipt = tempfile::NamedTempFile::new_in(store.join("authorization"))?;
        receipt.write_all(&serde_json::to_vec_pretty(&effective)?)?;
        receipt.persist(store.join("authorization").join(revision))?;
    }
    fs::create_dir_all(store.join("selected"))?;
    let mut selection = tempfile::NamedTempFile::new_in(store.join("selected"))?;
    selection.write_all(revision.as_bytes())?;
    selection.persist(store.join("selected").join(expected_package))?;
    Ok(
        serde_json::json!({"schema_version": 1, "package": expected_package, "installation": revision,
        "artifact_tree_digest": bundle.installation.artifact_tree_digest, "knowledge_digest": bundle.installation.knowledge_digest,
        "installation_status": "verified-and-selected", "activation_status": "pending-root-acknowledgement",
        "authorization": effective, "selection_status": selection_status(store, expected_package)?}),
    )
}

fn authorize_local_plan(store: &Path, revision: &str) -> Result<()> {
    match fs::remove_file(store.join("authorization").join(revision)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// Conservative state authorization survives disable, removal, and host restart.
/// It describes a state format code may have written, not inspected plugin memory.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateRecord {
    pub version: u32,
    pub compatibility: String,
    pub authorized_installation: String,
    pub evidence: String,
}

pub fn state_record(store: &Path, package: &str) -> Result<Option<StateRecord>> {
    validate_package_id(package)?;
    match fs::read(store.join("state").join(package)) {
        Ok(bytes) => {
            let record: StateRecord = serde_json::from_slice(&bytes)?;
            ensure!(record.version == 1, "unsupported package state record");
            Ok(Some(record))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn write_state_record(store: &Path, package: &str, record: &StateRecord) -> Result<()> {
    fs::create_dir_all(store.join("state"))?;
    let mut file = tempfile::NamedTempFile::new_in(store.join("state"))?;
    file.write_all(&serde_json::to_vec_pretty(record)?)?;
    file.as_file().sync_all()?;
    file.persist(store.join("state").join(package))?;
    Ok(())
}

fn prepare_state_record(store: &Path, manifest: &Manifest, revision: &str) -> Result<()> {
    let extension = manifest.extension.as_ref().context("extension required")?;
    let package = manifest.package.id.as_str();
    let previous = match state_record(store, package)? {
        Some(record) => Some(record),
        None => selected(store, package)?
            .map(|previous| -> Result<StateRecord> {
                let old = Manifest::from_path(
                    store
                        .join("objects")
                        .join(&previous)
                        .join(INSTALLED_MANIFEST_FILE),
                )?;
                Ok(StateRecord {
                    version: 1,
                    compatibility: old
                        .extension
                        .context("previous extension missing")?
                        .state_compatibility,
                    authorized_installation: previous,
                    evidence: "previous selected code may have written this state format".into(),
                })
            })
            .transpose()?,
    };
    let record = match previous {
        Some(record) if record.compatibility != "stateless" => record,
        _ => StateRecord {
            version: 1,
            compatibility: extension.state_compatibility.clone(),
            authorized_installation: revision.into(),
            evidence:
                "selected code may write this state format; code rollback does not roll back state"
                    .into(),
        },
    };
    write_state_record(store, package, &record)
}

pub fn migration_status(store: &Path, package: &str) -> Result<serde_json::Value> {
    let installation = inspect(store, package)?;
    let manifest = Manifest::from_path(
        store
            .join("objects")
            .join(&installation.plan_digest)
            .join(INSTALLED_MANIFEST_FILE),
    )?;
    let extension = manifest.extension.context("extension missing")?;
    let record = state_record(store, package)?;
    let pending = record.as_ref().is_some_and(|record| {
        record.compatibility != "stateless"
            && extension.state_compatibility != "stateless"
            && record.compatibility != extension.state_compatibility
    });
    Ok(
        serde_json::json!({"schema_version": 1, "package": package, "installation": installation.plan_digest, "state": record, "target_compatibility": extension.state_compatibility, "migration": extension.migration, "status": if pending {"migration-required"} else {"compatible"}, "state_authority": "conservative host record; mutable state is not inspected or rolled back"}),
    )
}

/// Operator attestation after external/manual migration. No guest or manifest
/// can attest its own migration; the exact selected revision and prior format
/// are required so stale completion cannot release a different replacement.
pub fn complete_migration(
    store: &Path,
    package: &str,
    revision: &str,
    previous: &str,
    evidence: &str,
) -> Result<serde_json::Value> {
    validate_package_id(package)?;
    validate_digest(revision)?;
    ensure!(
        !evidence.trim().is_empty() && evidence.len() <= 4096,
        "migration completion requires bounded evidence"
    );
    fs::create_dir_all(store)?;
    let _lock = store_lock(store, false)?;
    ensure!(
        selected(store, package)?.as_deref() == Some(revision),
        "migration selection changed"
    );
    let manifest = Manifest::from_path(
        store
            .join("objects")
            .join(revision)
            .join(INSTALLED_MANIFEST_FILE),
    )?;
    let extension = manifest.extension.context("extension missing")?;
    ensure!(
        extension.migration == "manual",
        "selected package does not declare a manual migration"
    );
    let prior = state_record(store, package)?.context("no state record to migrate")?;
    ensure!(
        prior.compatibility == previous,
        "migration prior state changed"
    );
    write_state_record(
        store,
        package,
        &StateRecord {
            version: 1,
            compatibility: extension.state_compatibility,
            authorized_installation: revision.into(),
            evidence: evidence.into(),
        },
    )?;
    migration_status(store, package)
}

#[cfg(test)]
mod lock_tests {
    #[test]
    fn guard_release_does_not_wait_for_an_inherited_descriptor_to_close() {
        let store = tempfile::tempdir().unwrap();
        let lock = super::store_lock(store.path(), false).unwrap();
        // A cloned descriptor shares the open file description, as a forked
        // child does before close-on-exec takes effect.
        let inherited = lock.0.try_clone().unwrap();
        assert!(super::store_lock(store.path(), true).is_err());
        drop(lock);
        let reader = super::store_lock(store.path(), true).unwrap();
        drop(inherited);
        drop(reader);
    }
}
