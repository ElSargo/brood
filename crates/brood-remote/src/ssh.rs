use std::{
    collections::hash_map::DefaultHasher,
    env, fs,
    hash::{Hash, Hasher},
    io::{BufRead, BufReader},
    net::{IpAddr, SocketAddr, ToSocketAddrs},
    os::unix::{
        ffi::OsStringExt,
        fs::{PermissionsExt, symlink},
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::mpsc,
    thread,
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use clap::Args;
use rcgen::{CertifiedKey, generate_simple_self_signed};
use tempfile::TempDir;

const REMOTE_START_TIMEOUT: Duration = Duration::from_secs(30);

/// Build and deploy a remote backend through SSH, then launch the local client.
#[derive(Debug, Args)]
pub struct SshArgs {
    /// SSH destination understood by the local OpenSSH configuration.
    #[arg(value_name = "USER@HOST")]
    target: String,
    /// Session-init package advertised by the client after connection.
    #[arg(long = "session-package", value_name = "ID=ROOT_WASM_PATH")]
    session_package: Option<String>,
    /// Host or IP carrying the QUIC connection. Defaults to the SSH hostname.
    #[arg(long = "connect-host", value_name = "HOST")]
    connect_host: Option<String>,
    /// Flake used to build deployment artifacts when overrides are omitted.
    #[arg(long, value_name = "PATH", default_value = ".")]
    flake: PathBuf,
    /// Prebuilt portable ssh-backend directory.
    #[arg(long, value_name = "PATH")]
    backend: Option<PathBuf>,
    /// Local brood-client executable.
    #[arg(long, value_name = "PATH")]
    client: Option<PathBuf>,
    /// Additional arguments passed to brood-client.
    #[arg(last = true, allow_hyphen_values = true)]
    client_args: Vec<std::ffi::OsString>,
}

pub fn deploy(args: &SshArgs) -> Result<()> {
    validate_ssh_target(&args.target)?;
    let remote_system = remote_system(&args.target)?;
    let source = DeploymentSource::prepare(&args.flake)?;
    let backend = resolve_backend(args, &remote_system, &source)?;
    let client = resolve_client(args.client.as_deref(), &source)?;
    let (package, root) = args.session_package.as_deref().map_or_else(
        || resolve_default_session_package(&client),
        parse_session_package,
    )?;

    let local_state = local_state_root(&args.target);
    let certificates = Certificates::load_or_create(&local_state)?;
    let deployment = deployment_id(&args.target);
    let remote_state = format!(".cache/brood/ssh/{deployment}");
    prepare_remote_state(&args.target, &remote_state, &certificates)?;
    copy_backend(&args.target, &backend, &remote_state)?;
    let remote_backend = format!("{remote_state}/backend");

    let mut remote = start_remote_backend(&args.target, &remote_backend, &remote_state, &package)?;
    let listener = wait_for_listener(&mut remote)?;
    let connect_host = args
        .connect_host
        .as_deref()
        .map_or_else(|| ssh_hostname(&args.target), str::to_string);
    let connect = resolve_quic_address(&connect_host, listener.port())?;
    eprintln!(
        "ssh-deploy=ready target={} backend={} connect={} package={}",
        args.target, remote_backend, connect, package
    );

    let client_result = Command::new(&client)
        .arg("--remote-connect")
        .arg(connect.to_string())
        .arg("--include-local")
        .arg("--remote-server-name")
        .arg("localhost")
        .arg("--remote-server-certificate")
        .arg(&certificates.server_certificate)
        .arg("--remote-client-certificate")
        .arg(&certificates.client_certificate)
        .arg("--remote-client-private-key")
        .arg(&certificates.client_private_key)
        .arg("--remote-offer-session-package")
        .arg(format!("{package}={}", root.display()))
        .args(&args.client_args)
        .status()
        .with_context(|| format!("launch local presentation client {}", client.display()));

    stop_remote_backend(&args.target, &remote_backend, &remote_state, &mut remote);
    let status = client_result?;
    ensure!(
        status.success(),
        "local presentation client exited with {status}"
    );
    Ok(())
}

fn validate_ssh_target(target: &str) -> Result<()> {
    ensure!(!target.is_empty(), "SSH target must not be empty");
    ensure!(
        target.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'@' | b'.' | b'_' | b'-')
        }),
        "SSH target contains unsupported shell characters; use an OpenSSH host alias"
    );
    Ok(())
}

fn parse_session_package(spec: &str) -> Result<(String, PathBuf)> {
    let (package, root) = spec
        .split_once('=')
        .context("session package must use ID=ROOT_WASM_PATH")?;
    ensure!(
        !package.is_empty()
            && package.len() <= brood_protocol::MAX_ARTIFACT_ID_BYTES
            && package
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')),
        "session package ID is invalid"
    );
    let root = Path::new(root)
        .canonicalize()
        .with_context(|| format!("resolve session package root {root}"))?;
    ensure!(root.is_file(), "session package root is not a file");
    Ok((package.to_string(), root))
}

fn resolve_default_session_package(client: &Path) -> Result<(String, PathBuf)> {
    let packaged_config = client
        .parent()
        .and_then(Path::parent)
        .map(|root| root.join("brood.toml"));
    let config = packaged_config
        .filter(|path| path.is_file())
        .unwrap_or_else(brood_cli::global_config_path);
    let root = brood_cli::realm_init_actor_path(&config, brood_cli::RealmKind::Session)?
        .with_context(|| {
            format!(
                "Brood config {} does not select a session init actor",
                config.display()
            )
        })?
        .canonicalize()
        .with_context(|| {
            format!(
                "resolve configured session init actor from {}",
                config.display()
            )
        })?;
    eprintln!(
        "ssh-deploy=session-package package=default root={} config={}",
        root.display(),
        config.display()
    );
    Ok(("default".to_string(), root))
}

fn remote_system(target: &str) -> Result<String> {
    let output = Command::new("ssh")
        .arg("-T")
        .arg(target)
        .arg("uname")
        .arg("-m")
        .output()
        .with_context(|| format!("query machine architecture on {target}"))?;
    ensure!(
        output.status.success(),
        "remote architecture query failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let architecture =
        String::from_utf8(output.stdout).context("remote architecture is not UTF-8")?;
    let system = match architecture.trim() {
        "x86_64" => "x86_64-linux",
        "aarch64" => "aarch64-linux",
        architecture => bail!("remote architecture {architecture:?} is not supported"),
    };
    eprintln!("ssh-deploy=remote-system target={target} system={system}");
    Ok(system.to_string())
}

struct DeploymentSource {
    path: PathBuf,
    _snapshot: Option<TempDir>,
}

impl DeploymentSource {
    fn prepare(flake: &Path) -> Result<Self> {
        let flake = flake
            .canonicalize()
            .with_context(|| format!("resolve deployment flake {}", flake.display()))?;
        let tracked = Command::new("git")
            .arg("-C")
            .arg(&flake)
            .args([
                "ls-files",
                "--cached",
                "--others",
                "--exclude-standard",
                "-z",
            ])
            .output();
        let Ok(tracked) = tracked else {
            return Ok(Self {
                path: flake,
                _snapshot: None,
            });
        };
        if !tracked.status.success() {
            return Ok(Self {
                path: flake,
                _snapshot: None,
            });
        }

        let snapshot = tempfile::Builder::new()
            .prefix("brood-ssh-source-")
            .tempdir()
            .context("create filtered deployment source snapshot")?;
        for raw_path in tracked.stdout.split(|byte| *byte == 0) {
            if raw_path.is_empty() {
                continue;
            }
            let relative = PathBuf::from(std::ffi::OsString::from_vec(raw_path.to_vec()));
            ensure!(
                relative
                    .components()
                    .all(|component| matches!(component, std::path::Component::Normal(_))),
                "Git returned an unsafe deployment source path"
            );
            let source = flake.join(&relative);
            let destination = snapshot.path().join(&relative);
            let metadata = match fs::symlink_metadata(&source) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("inspect deployment source {}", source.display())
                    });
                }
            };
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent).with_context(|| {
                    format!("create deployment snapshot directory {}", parent.display())
                })?;
            }
            if metadata.file_type().is_symlink() {
                let target = fs::read_link(&source)
                    .with_context(|| format!("read deployment symlink {}", source.display()))?;
                symlink(target, &destination).with_context(|| {
                    format!("copy deployment symlink {}", destination.display())
                })?;
            } else if metadata.is_file() {
                fs::copy(&source, &destination).with_context(|| {
                    format!(
                        "copy deployment source {} to {}",
                        source.display(),
                        destination.display()
                    )
                })?;
            } else {
                bail!(
                    "deployment source entry {} is not a regular file or symlink",
                    source.display()
                );
            }
        }
        ensure!(
            snapshot.path().join("flake.nix").is_file(),
            "deployment source snapshot has no flake.nix"
        );
        eprintln!(
            "ssh-deploy=source source={} snapshot={}",
            flake.display(),
            snapshot.path().display()
        );
        Ok(Self {
            path: snapshot.path().to_path_buf(),
            _snapshot: Some(snapshot),
        })
    }
}

fn resolve_backend(
    args: &SshArgs,
    remote_system: &str,
    source: &DeploymentSource,
) -> Result<PathBuf> {
    if let Some(backend) = &args.backend {
        return backend
            .canonicalize()
            .with_context(|| format!("resolve SSH backend {}", backend.display()));
    }
    build_flake_package(&source.path, remote_system, "ssh-backend", "remote backend")
}

fn build_flake_package(
    flake: &Path,
    system: &str,
    package: &str,
    description: &str,
) -> Result<PathBuf> {
    let attribute = format!("path:{}#packages.{system}.{package}", flake.display());
    eprintln!("ssh-deploy=build attribute={attribute}");
    let output = Command::new("nix")
        .args(["build", "--no-link", "--print-out-paths"])
        .arg(&attribute)
        .stderr(Stdio::inherit())
        .output()
        .with_context(|| format!("build {description} {attribute}"))?;
    ensure!(output.status.success(), "{description} build failed");
    let path = String::from_utf8(output.stdout)
        .context("Nix backend output path is not UTF-8")?
        .lines()
        .next_back()
        .with_context(|| format!("Nix {description} build returned no output path"))?
        .to_string();
    PathBuf::from(path)
        .canonicalize()
        .with_context(|| format!("resolve built {description}"))
}

fn copy_backend(target: &str, backend: &Path, remote_state: &str) -> Result<()> {
    eprintln!(
        "ssh-deploy=copy target={} backend={}",
        target,
        backend.display()
    );
    let remote_backend = format!("{remote_state}/backend");
    let prepare = format!(
        "chmod -R u+w {remote_backend} 2>/dev/null || true; \
         rm -rf {remote_backend}; mkdir -p {remote_backend}"
    );
    let status = Command::new("ssh")
        .arg("-T")
        .arg(target)
        .arg(prepare)
        .status()
        .with_context(|| format!("prepare backend directory on {target}"))?;
    ensure!(
        status.success(),
        "preparing backend directory on {target} failed"
    );
    let destination = format!("{target}:{remote_backend}/");
    let status = Command::new("scp")
        .arg("-q")
        .arg("-r")
        .arg(backend.join("bin"))
        .arg(destination)
        .status()
        .with_context(|| format!("copy portable backend to {target}"))?;
    ensure!(
        status.success(),
        "copying portable backend to {target} failed"
    );
    Ok(())
}

fn local_state_root(target: &str) -> PathBuf {
    let cache = env::var_os("XDG_CACHE_HOME").map_or_else(
        || {
            env::var_os("HOME").map_or_else(
                || env::temp_dir().join("brood"),
                |home| PathBuf::from(home).join(".cache").join("brood"),
            )
        },
        |cache| PathBuf::from(cache).join("brood"),
    );
    cache.join("ssh").join(deployment_id(target))
}

fn deployment_id(target: &str) -> String {
    let mut hasher = DefaultHasher::new();
    target.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

struct Certificates {
    server_certificate: PathBuf,
    server_private_key: PathBuf,
    client_certificate: PathBuf,
    client_private_key: PathBuf,
}

impl Certificates {
    fn load_or_create(root: &Path) -> Result<Self> {
        fs::create_dir_all(root)
            .with_context(|| format!("create SSH deployment state {}", root.display()))?;
        fs::set_permissions(root, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("secure SSH deployment state {}", root.display()))?;
        let certificates = Self {
            server_certificate: root.join("server.der"),
            server_private_key: root.join("server-key.der"),
            client_certificate: root.join("client.der"),
            client_private_key: root.join("client-key.der"),
        };
        if !certificates.paths().iter().all(|path| path.is_file()) {
            write_certificate(
                "localhost",
                &certificates.server_certificate,
                &certificates.server_private_key,
            )?;
            write_certificate(
                "brood-client",
                &certificates.client_certificate,
                &certificates.client_private_key,
            )?;
        }
        for path in certificates.paths() {
            fs::set_permissions(path, fs::Permissions::from_mode(0o600))
                .with_context(|| format!("secure certificate {}", path.display()))?;
        }
        Ok(certificates)
    }

    fn paths(&self) -> [&Path; 4] {
        [
            &self.server_certificate,
            &self.server_private_key,
            &self.client_certificate,
            &self.client_private_key,
        ]
    }
}

fn write_certificate(server_name: &str, certificate: &Path, key: &Path) -> Result<()> {
    let CertifiedKey { cert, signing_key } =
        generate_simple_self_signed(vec![server_name.to_string()])
            .context("generate SSH deployment certificate")?;
    fs::write(certificate, cert.der())
        .with_context(|| format!("write certificate {}", certificate.display()))?;
    fs::write(key, signing_key.serialize_der())
        .with_context(|| format!("write private key {}", key.display()))?;
    Ok(())
}

fn prepare_remote_state(
    target: &str,
    remote_state: &str,
    certificates: &Certificates,
) -> Result<()> {
    let create = format!("umask 077; mkdir -p {remote_state}");
    let status = Command::new("ssh")
        .arg("-T")
        .arg(target)
        .arg(create)
        .status()
        .with_context(|| format!("create remote deployment state on {target}"))?;
    ensure!(status.success(), "creating remote deployment state failed");
    let destination = format!("{target}:{remote_state}/");
    let status = Command::new("scp")
        .arg("-q")
        .arg(&certificates.server_certificate)
        .arg(&certificates.server_private_key)
        .arg(&certificates.client_certificate)
        .arg(destination)
        .status()
        .with_context(|| format!("copy deployment certificates to {target}"))?;
    ensure!(status.success(), "copying deployment certificates failed");
    Ok(())
}

fn start_remote_backend(
    target: &str,
    backend: &str,
    remote_state: &str,
    package: &str,
) -> Result<Child> {
    let helper = format!("{backend}/bin/brood-ssh-backend-session");
    Command::new("ssh")
        .arg("-T")
        .arg(target)
        .arg("dbus-run-session")
        .arg("--")
        .arg(helper)
        .arg("start")
        .arg(remote_state)
        .arg(backend)
        .arg(package)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("start remote backend on {target}"))
}

fn wait_for_listener(remote: &mut Child) -> Result<SocketAddr> {
    let stdout = remote
        .stdout
        .take()
        .context("remote backend stdout was not captured")?;
    let (sender, receiver) = mpsc::sync_channel(1);
    thread::Builder::new()
        .name("brood-ssh-remote-output".to_string())
        .spawn(move || {
            let mut sender = Some(sender);
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else {
                    break;
                };
                eprintln!("[remote] {line}");
                let Some(value) = line
                    .strip_prefix("LISTEN=")
                    .and_then(|value| value.split_whitespace().next())
                    .and_then(|value| value.parse::<SocketAddr>().ok())
                else {
                    continue;
                };
                if let Some(sender) = sender.take()
                    && sender.send(value).is_err()
                {
                    break;
                }
            }
        })
        .context("spawn remote backend output reader")?;
    receiver
        .recv_timeout(REMOTE_START_TIMEOUT)
        .context("remote relay did not publish a listener within 30 seconds")
}

fn ssh_hostname(target: &str) -> String {
    target
        .rsplit_once('@')
        .map_or(target, |(_user, host)| host)
        .to_string()
}

fn resolve_quic_address(host: &str, port: u16) -> Result<SocketAddr> {
    if let Ok(address) = host.parse::<IpAddr>() {
        return Ok(SocketAddr::new(address, port));
    }
    let mut addresses = (host, port)
        .to_socket_addrs()
        .with_context(|| format!("resolve QUIC host {host}"))?
        .collect::<Vec<_>>();
    addresses.sort_by_key(std::net::SocketAddr::is_ipv6);
    addresses
        .into_iter()
        .next()
        .with_context(|| format!("QUIC host {host} resolved to no addresses"))
}

fn resolve_client(configured: Option<&Path>, source: &DeploymentSource) -> Result<PathBuf> {
    if let Some(configured) = configured {
        return configured
            .canonicalize()
            .with_context(|| format!("resolve brood-client {}", configured.display()));
    }
    if let Some(configured) = env::var_os("BROOD_CLIENT_BIN") {
        return PathBuf::from(configured)
            .canonicalize()
            .context("resolve BROOD_CLIENT_BIN");
    }
    let system = match env::consts::ARCH {
        "x86_64" => "x86_64-linux",
        "aarch64" => "aarch64-linux",
        architecture => bail!("local architecture {architecture:?} is not supported"),
    };
    let package = build_flake_package(&source.path, system, "brood", "local client runtime")?;
    let client = package.join("bin/brood-client");
    ensure!(
        client.is_file(),
        "local client runtime has no {}",
        client.display()
    );
    Ok(client)
}

fn stop_remote_backend(target: &str, backend: &str, remote_state: &str, remote: &mut Child) {
    let helper = format!("{backend}/bin/brood-ssh-backend-session");
    drop(
        Command::new("ssh")
            .arg("-T")
            .arg(target)
            .arg(helper)
            .arg("stop")
            .arg(remote_state)
            .status(),
    );
    drop(remote.kill());
    drop(remote.wait());
}
