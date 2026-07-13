use std::{
    env,
    ffi::OsString,
    fs::{self, File},
    io::ErrorKind,
    os::unix::{ffi::OsStrExt, process::ExitStatusExt as _},
    path::{Path, PathBuf},
    process::{Command, ExitCode, ExitStatus, Stdio, Termination},
    thread,
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};

const CACHE_VERSION: u32 = 1;
const ACTIVATION_SCRIPT: &str = "activate.sh";
const CACHE_MANIFEST: &str = "manifest.json";
const CACHE_PROFILE: &str = "profile";
const LOCK_DIR: &str = ".lock";
const CACHE_NAMESPACE: &str = "nix-develop";
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0100_0000_01b3;

fn main() -> Result<ActivatedChildExit> {
    let args = ActivationArgs::parse()?;
    let activation_script = cached_activation_script(&args)?;

    run_activation_script(&args, &activation_script)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ActivatedChildExit {
    Code(u8),
    Signal(i32),
}

impl ActivatedChildExit {
    fn from_status(status: ExitStatus) -> Result<Self> {
        if let Some(code) = status.code() {
            let code = u8::try_from(code)
                .with_context(|| format!("activated child returned invalid exit code {code}"))?;
            return Ok(Self::Code(code));
        }

        status
            .signal()
            .map(Self::Signal)
            .with_context(|| format!("activated child returned unrecognized exit status {status}"))
    }

    fn shell_code(self) -> u8 {
        match self {
            Self::Code(code) => code,
            Self::Signal(signal) => u8::try_from(128_i32.saturating_add(signal)).unwrap_or(u8::MAX),
        }
    }
}

impl Termination for ActivatedChildExit {
    fn report(self) -> ExitCode {
        ExitCode::from(self.shell_code())
    }
}

struct ActivationArgs {
    project_dir: PathBuf,
    working_dir: PathBuf,
    cache_dir: PathBuf,
    runner_cache_root: PathBuf,
    inner_binary: PathBuf,
}

impl ActivationArgs {
    fn parse() -> Result<Self> {
        let args: Vec<OsString> = env::args_os().skip(1).collect();
        ensure!(
            args.len() == 5,
            "usage: manifold-runner-nix-develop <project-dir> <working-dir> <session-cache-dir> <runner-cache-root> <inner-binary>"
        );

        let Ok(
            [
                project_dir,
                working_dir,
                cache_dir,
                runner_cache_root,
                inner_binary,
            ],
        ) = <Vec<OsString> as TryInto<[OsString; 5]>>::try_into(args)
        else {
            bail!(
                "usage: manifold-runner-nix-develop <project-dir> <working-dir> <session-cache-dir> <runner-cache-root> <inner-binary>"
            );
        };
        Ok(Self {
            project_dir: PathBuf::from(project_dir),
            working_dir: PathBuf::from(working_dir),
            cache_dir: PathBuf::from(cache_dir),
            runner_cache_root: PathBuf::from(runner_cache_root),
            inner_binary: PathBuf::from(inner_binary),
        })
    }
}

fn cached_activation_script(args: &ActivationArgs) -> Result<PathBuf> {
    let cache_dir = project_cache_dir(args);
    fs::create_dir_all(&cache_dir).with_context(|| format!("create {}", cache_dir.display()))?;

    let expected = current_manifest(&args.project_dir)?;
    if cache_is_valid(&cache_dir, &expected) {
        return Ok(cache_dir.join(ACTIVATION_SCRIPT));
    }

    let _lock = CacheLock::acquire(cache_dir.join(LOCK_DIR))?;
    if cache_is_valid(&cache_dir, &expected) {
        return Ok(cache_dir.join(ACTIVATION_SCRIPT));
    }

    rebuild_cache(args, &cache_dir, &expected)
}

fn run_activation_script(
    args: &ActivationArgs,
    activation_script: &Path,
) -> Result<ActivatedChildExit> {
    let status = Command::new("bash")
        .arg("-c")
        .arg(
            "set -e\n\
             activation_script=$1\n\
             project_dir=$2\n\
             working_dir=$3\n\
             session_cache_dir=$4\n\
             inner_binary=$6\n\
             . \"$activation_script\"\n\
             export MANIFOLD_SESSION_PROJECT_DIR=\"$project_dir\"\n\
             export MANIFOLD_SESSION_WORKING_DIR=\"$working_dir\"\n\
             export MANIFOLD_SESSION_CACHE_DIR=\"$session_cache_dir\"\n\
             set +e\n\
             \"$inner_binary\"\n\
             status=$?\n\
             exit \"$status\"",
        )
        .arg("manifold-nix-activate")
        .arg(activation_script)
        .arg(&args.project_dir)
        .arg(&args.working_dir)
        .arg(&args.cache_dir)
        .arg(&args.runner_cache_root)
        .arg(&args.inner_binary)
        .current_dir(&args.working_dir)
        .status()
        .context("run cached nix develop activation")?;

    ActivatedChildExit::from_status(status)
}

fn rebuild_cache(
    args: &ActivationArgs,
    cache_dir: &Path,
    expected: &NixActivationManifest,
) -> Result<PathBuf> {
    let build_dir = args.cache_dir.join("nix-develop-build");
    remove_path_if_exists(&build_dir)?;
    fs::create_dir_all(&build_dir).with_context(|| format!("create {}", build_dir.display()))?;

    let tmp_activation = build_dir.join(ACTIVATION_SCRIPT);
    let tmp_profile = build_dir.join(CACHE_PROFILE);
    let activation_file = File::create(&tmp_activation)
        .with_context(|| format!("create {}", tmp_activation.display()))?;

    let status = Command::new("nix")
        .arg("print-dev-env")
        .arg("--profile")
        .arg(&tmp_profile)
        .arg("--no-write-lock-file")
        .arg(path_flake_ref(&args.project_dir))
        .current_dir(&args.project_dir)
        .stdout(Stdio::from(activation_file))
        .status()
        .context("run nix print-dev-env")?;
    if !status.success() {
        remove_path_if_exists(&build_dir)?;
        bail!("nix print-dev-env exited with {status}");
    }

    ensure!(
        fs::metadata(&tmp_activation)
            .with_context(|| format!("stat {}", tmp_activation.display()))?
            .len()
            > 0,
        "nix print-dev-env produced an empty activation script"
    );

    let tmp_manifest = build_dir.join(CACHE_MANIFEST);
    fs::write(
        &tmp_manifest,
        serde_json::to_vec_pretty(expected).context("encode nix activation cache manifest")?,
    )
    .with_context(|| format!("write {}", tmp_manifest.display()))?;

    let final_activation = cache_dir.join(ACTIVATION_SCRIPT);
    let final_profile = cache_dir.join(CACHE_PROFILE);
    install_profile_links(&tmp_profile, cache_dir)?;
    replace_path(&tmp_activation, &final_activation)?;
    replace_path(&tmp_manifest, &cache_dir.join(CACHE_MANIFEST))?;

    ensure!(
        final_profile.exists(),
        "nix activation cache profile was not installed"
    );
    remove_path_if_exists(&build_dir)?;
    Ok(final_activation)
}

fn install_profile_links(tmp_profile: &Path, cache_dir: &Path) -> Result<()> {
    let profile_target = fs::read_link(tmp_profile)
        .with_context(|| format!("read link {}", tmp_profile.display()))?;
    let tmp_profile_target = tmp_profile
        .parent()
        .context("profile has parent")?
        .join(&profile_target);
    let final_profile_target = cache_dir.join(&profile_target);

    replace_path(&tmp_profile_target, &final_profile_target)?;
    replace_path(tmp_profile, &cache_dir.join(CACHE_PROFILE))
}

fn cache_is_valid(cache_dir: &Path, expected: &NixActivationManifest) -> bool {
    if !cache_dir.join(ACTIVATION_SCRIPT).is_file() || !cache_dir.join(CACHE_PROFILE).exists() {
        return false;
    }
    let Ok(contents) = fs::read(cache_dir.join(CACHE_MANIFEST)) else {
        return false;
    };
    let Ok(actual) = serde_json::from_slice::<NixActivationManifest>(&contents) else {
        return false;
    };
    actual == *expected
}

fn current_manifest(project_dir: &Path) -> Result<NixActivationManifest> {
    let flake_nix = file_checksum(&project_dir.join("flake.nix"))?;
    ensure!(
        flake_nix.present,
        "{} is required for nix-develop runner caching",
        project_dir.join("flake.nix").display()
    );
    Ok(NixActivationManifest {
        version: CACHE_VERSION,
        project_dir: project_dir.as_os_str().as_bytes().to_vec(),
        flake_nix,
        flake_lock: file_checksum(&project_dir.join("flake.lock"))?,
    })
}

fn file_checksum(path: &Path) -> Result<FileChecksum> {
    match fs::read(path) {
        Ok(contents) => Ok(FileChecksum {
            present: true,
            checksum: Some(checksum_bytes(&contents)),
        }),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(FileChecksum {
            present: false,
            checksum: None,
        }),
        Err(err) => Err(err).with_context(|| format!("read {}", path.display())),
    }
}

fn project_cache_dir(args: &ActivationArgs) -> PathBuf {
    args.runner_cache_root
        .clone()
        .join(CACHE_NAMESPACE)
        .join(checksum_bytes(args.project_dir.as_os_str().as_bytes()))
}

fn path_flake_ref(path: &Path) -> OsString {
    OsString::from(format!("path:{}", path.display()))
}

fn replace_path(src: &Path, dst: &Path) -> Result<()> {
    remove_path_if_exists(dst)?;
    fs::rename(src, dst).with_context(|| format!("move {} to {}", src.display(), dst.display()))
}

fn remove_path_if_exists(path: &Path) -> Result<()> {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return Ok(());
    };
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        fs::remove_dir_all(path).with_context(|| format!("remove {}", path.display()))
    } else {
        fs::remove_file(path).with_context(|| format!("remove {}", path.display()))
    }
}

fn checksum_bytes(bytes: &[u8]) -> String {
    let mut hash = FNV_OFFSET;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    format!("{hash:016x}")
}

struct CacheLock {
    path: PathBuf,
}

impl CacheLock {
    fn acquire(path: PathBuf) -> Result<Self> {
        loop {
            match fs::create_dir(&path) {
                Ok(()) => {
                    fs::write(path.join("owner"), std::process::id().to_string())
                        .with_context(|| format!("write {}", path.join("owner").display()))?;
                    return Ok(Self { path });
                }
                Err(err) if err.kind() == ErrorKind::AlreadyExists => {
                    if lock_owner_alive(&path) {
                        thread::sleep(Duration::from_millis(100));
                    } else {
                        remove_path_if_exists(&path)?;
                    }
                }
                Err(err) => return Err(err).with_context(|| format!("create {}", path.display())),
            }
        }
    }
}

impl Drop for CacheLock {
    fn drop(&mut self) {
        let _cleanup_result = fs::remove_dir_all(&self.path);
    }
}

fn lock_owner_alive(path: &Path) -> bool {
    let Ok(owner) = fs::read_to_string(path.join("owner")) else {
        return false;
    };
    let Ok(pid) = owner.trim().parse::<u32>() else {
        return false;
    };
    Path::new("/proc").join(pid.to_string()).exists()
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
struct NixActivationManifest {
    version: u32,
    project_dir: Vec<u8>,
    flake_nix: FileChecksum,
    flake_lock: FileChecksum,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
struct FileChecksum {
    present: bool,
    checksum: Option<String>,
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::let_underscore_must_use,
    reason = "unit tests use direct temp directory setup and best-effort cleanup"
)]
mod tests {
    use super::{
        ActivatedChildExit, ActivationArgs, checksum_bytes, file_checksum, run_activation_script,
    };
    use std::{
        fs,
        path::PathBuf,
        process::Command,
        time::{SystemTime, UNIX_EPOCH},
    };

    #[test]
    fn activation_returns_inner_exit_code_and_exports_session_context() {
        let root = temp_test_dir("activation-exit");
        let working_dir = root.join("work");
        let session_cache_dir = root.join("session-cache");
        fs::create_dir_all(&working_dir).expect("create working directory");
        fs::create_dir_all(&session_cache_dir).expect("create session cache directory");
        let activation = root.join("activate.sh");
        fs::write(&activation, "export RUNNER_ACTIVATED=visible\n").expect("write activation");
        let output = root.join("inner-output");
        let inner = executable_script(
            &root,
            "inner-exit",
            &format!(
                concat!(
                    "printf '%s\\n%s\\n%s\\n%s\\n' ",
                    "\"$RUNNER_ACTIVATED\" ",
                    "\"$MANIFOLD_SESSION_PROJECT_DIR\" ",
                    "\"$MANIFOLD_SESSION_WORKING_DIR\" ",
                    "\"$MANIFOLD_SESSION_CACHE_DIR\" > '{}'\n",
                    "exit 37\n"
                ),
                output.display()
            ),
        );
        let args = activation_args(&root, working_dir.clone(), session_cache_dir.clone(), inner);

        let status = run_activation_script(&args, &activation).expect("run activation");

        assert_eq!(status, ActivatedChildExit::Code(37));
        assert_eq!(
            fs::read_to_string(&output).expect("read inner output"),
            format!(
                "visible\n{}\n{}\n{}\n",
                root.display(),
                working_dir.display(),
                session_cache_dir.display()
            )
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn activation_exposes_signalled_inner_as_shell_status() {
        let root = temp_test_dir("activation-signal");
        let working_dir = root.join("work");
        fs::create_dir_all(&working_dir).expect("create working directory");
        let activation = root.join("activate.sh");
        fs::write(&activation, "export RUNNER_ACTIVATED=visible\n").expect("write activation");
        let inner = executable_script(&root, "inner-signal", "kill -TERM $$\n");
        let args = activation_args(&root, working_dir, root.join("session-cache"), inner);

        let status = run_activation_script(&args, &activation).expect("run activation");

        assert_eq!(status, ActivatedChildExit::Code(143));
        assert_eq!(status.shell_code(), 143);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn direct_signal_status_has_conventional_shell_code() {
        let status = Command::new("sh")
            .arg("-c")
            .arg("kill -TERM $$")
            .status()
            .expect("run signalled process");

        let outcome = ActivatedChildExit::from_status(status).expect("classify signal");

        assert_eq!(outcome, ActivatedChildExit::Signal(15));
        assert_eq!(outcome.shell_code(), 143);
    }

    #[test]
    fn checksum_tracks_missing_and_changed_files() {
        let root = temp_test_dir("checksum");
        let file = root.join("flake.lock");

        let missing = file_checksum(&file).expect("missing checksum");
        assert!(!missing.present);
        assert_eq!(missing.checksum, None);

        fs::write(&file, "one").expect("write first");
        let first = file_checksum(&file).expect("first checksum");
        fs::write(&file, "two").expect("write second");
        let second = file_checksum(&file).expect("second checksum");

        assert!(first.present);
        assert!(second.present);
        assert_ne!(first.checksum, second.checksum);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn checksum_bytes_is_stable() {
        assert_eq!(checksum_bytes(b"flake"), "177aafa985d46da2");
    }

    fn activation_args(
        project_dir: &std::path::Path,
        working_dir: PathBuf,
        cache_dir: PathBuf,
        inner_binary: PathBuf,
    ) -> ActivationArgs {
        ActivationArgs {
            project_dir: project_dir.to_path_buf(),
            working_dir,
            cache_dir,
            runner_cache_root: project_dir.join("runner-cache"),
            inner_binary,
        }
    }

    fn executable_script(root: &std::path::Path, name: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt as _;

        let path = root.join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}")).expect("write executable script");
        let mut permissions = fs::metadata(&path).expect("stat script").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&path, permissions).expect("make script executable");
        path
    }

    fn temp_test_dir(name: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "manifold-nix-runner-{name}-{}-{stamp}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("create temp dir");
        path
    }
}
