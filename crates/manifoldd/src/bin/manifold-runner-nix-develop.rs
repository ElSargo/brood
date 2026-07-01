use std::{
    env,
    ffi::OsString,
    fs::{self, File},
    io::ErrorKind,
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
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
const FNV_OFFSET: u64 = 0xcbf29ce484222325;
const FNV_PRIME: u64 = 0x100000001b3;

fn main() -> Result<()> {
    let args = ActivationArgs::parse()?;
    let activation_script = cached_activation_script(&args)?;

    run_activation_script(&args, &activation_script)
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

        let mut args = args.into_iter();
        Ok(Self {
            project_dir: PathBuf::from(args.next().expect("project dir arg exists")),
            working_dir: PathBuf::from(args.next().expect("working dir arg exists")),
            cache_dir: PathBuf::from(args.next().expect("session cache dir arg exists")),
            runner_cache_root: PathBuf::from(args.next().expect("runner cache root arg exists")),
            inner_binary: PathBuf::from(args.next().expect("inner binary arg exists")),
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

fn run_activation_script(args: &ActivationArgs, activation_script: &Path) -> Result<()> {
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

    match status.code() {
        Some(code) => std::process::exit(code),
        None => bail!("cached nix develop activation terminated by signal"),
    }
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
        let _ = fs::remove_dir_all(&self.path);
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
    use super::{checksum_bytes, file_checksum};
    use std::{
        fs,
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

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
