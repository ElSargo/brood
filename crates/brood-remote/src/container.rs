use std::{
    env,
    ffi::{OsStr, OsString},
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::{IpAddr, SocketAddr, ToSocketAddrs},
    os::unix::{fs::PermissionsExt, process::CommandExt},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::mpsc,
    thread,
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use clap::{Args, ValueEnum};
use rcgen::{CertifiedKey, generate_simple_self_signed};
use serde::{Deserialize, Serialize};
use tempfile::TempDir;

const CONTAINER_RELAY_PORT: u16 = 44_192;
const CONTAINER_START_TIMEOUT: Duration = Duration::from_secs(30);

/// Start a backend inside a local or remote Podman container.
#[derive(Clone, Debug, Args)]
pub struct ContainerArgs {
    /// OCI image used for the backend environment.
    #[arg(value_name = "IMAGE")]
    image: String,
    /// Named Podman system connection. Omit for the local Podman engine.
    #[arg(short = 'c', long, value_name = "CONNECTION")]
    connection: Option<String>,
    /// Host or IP carrying QUIC when the Podman connection cannot be resolved automatically.
    #[arg(long, value_name = "HOST")]
    connect_host: Option<String>,
    /// Stable container name. A generated name is used by default.
    #[arg(long, value_name = "NAME")]
    name: Option<String>,
    /// GPU forwarding policy.
    #[arg(long, value_enum, default_value_t = GpuMode::Auto)]
    gpu: GpuMode,
    /// Bind mount passed to Podman. May be repeated.
    #[arg(short = 'v', long = "volume", value_name = "HOST:CONTAINER[:OPTIONS]")]
    volumes: Vec<String>,
    /// Initial working directory for the container workload.
    #[arg(short = 'w', long = "workdir", value_name = "PATH")]
    workdir: Option<String>,
    /// Preserve the container after the presentation disconnects.
    #[arg(long)]
    keep: bool,
    /// Podman executable used for lifecycle operations.
    #[arg(long, value_name = "PATH")]
    podman: Option<PathBuf>,
    /// Prebuilt portable container-backend directory.
    #[arg(long, value_name = "PATH")]
    backend: Option<PathBuf>,
    /// Local brood-client executable.
    #[arg(long, value_name = "PATH")]
    client: Option<PathBuf>,
    /// Session-init package advertised by the client after connection.
    #[arg(long = "session-package", value_name = "ID=ROOT_WASM_PATH")]
    session_package: Option<String>,
    /// Additional arguments passed to brood-client.
    #[arg(last = true, allow_hyphen_values = true)]
    client_args: Vec<std::ffi::OsString>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
enum GpuMode {
    #[default]
    Auto,
    Always,
    Never,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GpuAccess {
    Cdi,
    Drm,
    None,
}

/// Internal supervisor arguments used inside an augmented container.
#[derive(Debug, Args)]
pub struct ContainerSessionArgs {
    #[arg(long, default_value = "/brood/state")]
    state: PathBuf,
    #[arg(long, default_value = "/brood")]
    backend: PathBuf,
    #[arg(long, default_value = "default")]
    package: String,
    #[arg(long, default_value = "0.0.0.0:44192")]
    listen: SocketAddr,
    #[arg(long)]
    persistent: bool,
}

/// Raw arguments accepted by the packaged Podman replacement.
#[derive(Debug, Args)]
#[command(disable_help_flag = true)]
pub struct PodmanArgs {
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    args: Vec<std::ffi::OsString>,
}

#[derive(Serialize)]
struct ContainerAttachReady<'a> {
    connect: SocketAddr,
    server_name: &'static str,
    server_certificate: &'a Path,
    client_certificate: &'a Path,
    client_private_key: &'a Path,
    session_package: &'a str,
    session_root: &'a Path,
}

#[derive(Deserialize, Serialize)]
struct SavedContainerConnection {
    container: String,
    connect: SocketAddr,
}

struct StartedContainer {
    podman: Podman,
    id: String,
    label: String,
    backend: Child,
    backend_stdin: Option<std::process::ChildStdin>,
    connect: SocketAddr,
    package: String,
    root: PathBuf,
    certificates: Certificates,
    keep: bool,
    _state: TempDir,
    _backend_stage: TempDir,
}

impl StartedContainer {
    fn stop(&mut self) {
        drop(self.backend_stdin.take());
        if let Err(error) = self.backend.wait() {
            eprintln!(
                "could not wait for container backend {}: {error}",
                self.label
            );
        }
        // The direct workflow uses `container-hold` as PID 1, and PID 1 does
        // not receive ordinary default signal handling inside a container.
        // The backend has already shut down cleanly through its stdin above,
        // so terminate only the inert holder without Podman's stop timeout.
        if let Err(error) = self.podman.status(["kill", "--signal", "KILL", &self.id]) {
            eprintln!("could not stop container {}: {error:#}", self.label);
        }
        if !self.keep
            && let Err(error) = self.podman.status(["rm", "--force", &self.id])
        {
            eprintln!("could not remove container {}: {error:#}", self.label);
        }
    }
}

#[derive(Clone)]
struct Podman {
    executable: PathBuf,
    connection: Option<String>,
}

impl Podman {
    fn command(&self) -> Command {
        let mut command = Command::new(&self.executable);
        if let Some(connection) = &self.connection {
            command.arg("--connection").arg(connection);
        }
        command
    }

    fn status<const N: usize>(&self, args: [&str; N]) -> Result<()> {
        let status = self
            .command()
            .args(args)
            .stdout(Stdio::null())
            .status()
            .with_context(|| format!("run {}", self.executable.display()))?;
        ensure!(status.success(), "Podman command exited with {status}");
        Ok(())
    }

    fn output<const N: usize>(&self, args: [&str; N]) -> Result<std::process::Output> {
        self.command()
            .args(args)
            .output()
            .with_context(|| format!("run {}", self.executable.display()))
    }
}

pub fn deploy(args: &ContainerArgs) -> Result<()> {
    let client = resolve_client(args.client.as_deref())?;
    let mut deployment = start_container(args, &client)?;
    let status = brood_launch::command(&client, "client")
        .arg("--remote-connect")
        .arg(deployment.connect.to_string())
        .arg("--include-local")
        .arg("--remote-server-name")
        .arg("localhost")
        .arg("--remote-server-certificate")
        .arg(&deployment.certificates.server_certificate)
        .arg("--remote-client-certificate")
        .arg(&deployment.certificates.client_certificate)
        .arg("--remote-client-private-key")
        .arg(&deployment.certificates.client_private_key)
        .arg("--remote-offer-session-package")
        .arg(format!(
            "{}={}",
            deployment.package,
            deployment.root.display()
        ))
        .args(&args.client_args)
        .status()
        .with_context(|| format!("launch local presentation client {}", client.display()));
    deployment.stop();
    let status = status?;
    ensure!(
        status.success(),
        "local presentation client exited with {status}"
    );
    Ok(())
}

/// Publish connection material and retain the container until stdin closes.
pub fn attach(args: &ContainerArgs) -> Result<()> {
    let client = args
        .client
        .as_deref()
        .context("container attachment requires the requesting brood-client path")?
        .canonicalize()
        .context("resolve requesting brood-client")?;
    let mut deployment = start_container(args, &client)?;
    let ready = ContainerAttachReady {
        connect: deployment.connect,
        server_name: "localhost",
        server_certificate: &deployment.certificates.server_certificate,
        client_certificate: &deployment.certificates.client_certificate,
        client_private_key: &deployment.certificates.client_private_key,
        session_package: &deployment.package,
        session_root: &deployment.root,
    };
    let publish = (|| -> Result<()> {
        let stdout = std::io::stdout();
        let mut stdout = stdout.lock();
        serde_json::to_writer(&mut stdout, &ready)
            .context("encode container attachment details")?;
        writeln!(stdout).context("publish container attachment details")?;
        stdout
            .flush()
            .context("flush container attachment details")?;
        std::io::copy(&mut std::io::stdin().lock(), &mut std::io::sink())
            .context("wait for presentation client container attachment lifetime")?;
        Ok(())
    })();
    deployment.stop();
    publish
}

fn start_container(args: &ContainerArgs, client: &Path) -> Result<StartedContainer> {
    validate_image(&args.image)?;
    let podman = Podman {
        executable: resolve_podman(args.podman.as_deref())?,
        connection: args.connection.clone(),
    };
    let backend = resolve_backend(args.backend.as_deref())?;
    let (backend_stage, staged_backend) = stage_backend(&backend)?;
    let (package, root) = args.session_package.as_deref().map_or_else(
        || super::ssh::resolve_default_session_package(client),
        super::ssh::parse_session_package,
    )?;
    let state = tempfile::Builder::new()
        .prefix("brood-container-")
        .tempdir()
        .context("create container deployment state")?;
    let certificates = Certificates::load_or_create(state.path())?;
    let label = args.name.clone().unwrap_or_else(|| {
        format!(
            "brood-{}-{}",
            std::process::id(),
            state
                .path()
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
        )
    });
    let mut skipped_gpu_access = 0;
    let (id, gpu_access) = loop {
        let (id, gpu_access) = create_container(&podman, args, &label, skipped_gpu_access)?;
        if let Err(error) = copy_into_container(&podman, &id, &staged_backend, state.path()) {
            if !args.keep {
                drop(podman.status(["rm", "--force", &id]));
            }
            return Err(error);
        }
        match podman.status(["start", &id]) {
            Ok(()) => break (id, gpu_access),
            Err(error) if gpu_access != GpuAccess::None => {
                eprintln!(
                    "container-deploy=gpu-start-failed access={gpu_access:?} reason={error:#}"
                );
                drop(podman.status(["rm", "--force", &id]));
                skipped_gpu_access = skipped_gpu_access.saturating_add(1);
            }
            Err(error) => {
                if !args.keep {
                    drop(podman.status(["rm", "--force", &id]));
                }
                return Err(error);
            }
        }
    };
    let result = (|| -> Result<(Child, std::process::ChildStdin, SocketAddr)> {
        let (mut child, stdin) = start_container_backend(&podman, &id, &package)?;
        wait_for_listener(&mut child)?;
        let published_port = published_port(&podman, &id)?;
        let host = resolve_connect_host(&podman, args.connect_host.as_deref())?;
        Ok((child, stdin, SocketAddr::new(host, published_port)))
    })();
    let (backend_child, backend_stdin, connect) = match result {
        Ok(value) => value,
        Err(error) => {
            if !args.keep {
                drop(podman.status(["rm", "--force", &id]));
            }
            return Err(error);
        }
    };
    eprintln!(
        "container-deploy=ready container={label} id={id} connect={connect} gpu={} access={gpu_access:?}",
        args.gpu.as_str(),
    );
    Ok(StartedContainer {
        podman,
        id,
        label,
        backend: backend_child,
        backend_stdin: Some(backend_stdin),
        connect,
        package,
        root,
        certificates,
        keep: args.keep,
        _state: state,
        _backend_stage: backend_stage,
    })
}

impl GpuMode {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Always => "always",
            Self::Never => "never",
        }
    }
}

fn create_container(
    podman: &Podman,
    args: &ContainerArgs,
    label: &str,
    skipped_gpu_access: usize,
) -> Result<(String, GpuAccess)> {
    let create = |gpu: GpuAccess| -> Result<std::process::Output> {
        let mut command = podman.command();
        command
            .arg("create")
            .arg("--name")
            .arg(label)
            .arg("--label")
            .arg("io.brood.backend=true")
            .arg("--publish")
            .arg(format!("{CONTAINER_RELAY_PORT}/udp"));
        match gpu {
            GpuAccess::Cdi => {
                command.args(["--gpus", "all"]);
            }
            GpuAccess::Drm => {
                command.args(["--device", "/dev/dri"]);
            }
            GpuAccess::None => {}
        }
        for volume in &args.volumes {
            command.arg("--volume").arg(volume);
        }
        if let Some(workdir) = &args.workdir {
            command.arg("--workdir").arg(workdir);
        }
        command
            .arg(&args.image)
            .args(["/brood/bin/brood-remote", "container-hold"]);
        command
            .output()
            .with_context(|| format!("create Podman container from {}", args.image))
    };
    let attempts: &[GpuAccess] = match args.gpu {
        GpuMode::Auto => &[GpuAccess::Drm, GpuAccess::Cdi, GpuAccess::None],
        GpuMode::Always => &[GpuAccess::Drm, GpuAccess::Cdi],
        GpuMode::Never => &[GpuAccess::None],
    };
    let mut last_output = None;
    for access in attempts.iter().skip(skipped_gpu_access) {
        let output = create(*access)?;
        if output.status.success() {
            let id = String::from_utf8(output.stdout)
                .context("Podman container ID is not UTF-8")?
                .trim()
                .to_string();
            ensure!(!id.is_empty(), "Podman create returned no container ID");
            eprintln!("container-deploy=gpu-access access={access:?}");
            return Ok((id, *access));
        }
        eprintln!(
            "container-deploy=gpu-attempt-failed access={access:?} reason={}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
        last_output = Some(output);
    }
    let output = last_output.context("container creation made no GPU access attempt")?;
    bail!(
        "creating Podman container failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    )
}

fn copy_into_container(podman: &Podman, id: &str, backend: &Path, state: &Path) -> Result<()> {
    let backend_source = format!("{}/.", backend.display());
    let backend_destination = format!("{id}:/brood");
    let output = podman
        .command()
        .args(["cp", &backend_source, &backend_destination])
        .output()
        .context("copy portable backend into container")?;
    ensure!(
        output.status.success(),
        "copying backend into container failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let state_source = format!("{}/.", state.display());
    let state_destination = format!("{id}:/brood/state");
    let output = podman
        .command()
        .args(["cp", &state_source, &state_destination])
        .output()
        .context("copy container certificates")?;
    ensure!(
        output.status.success(),
        "copying certificates into container failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
}

fn start_container_backend(
    podman: &Podman,
    id: &str,
    package: &str,
) -> Result<(Child, std::process::ChildStdin)> {
    let mut command = podman.command();
    let mut child = command
        .args([
            "exec",
            "--interactive",
            "--env",
            "XKB_CONFIG_ROOT=/brood/share/X11/xkb",
            "--env",
            "__EGL_VENDOR_LIBRARY_FILENAMES=/brood/share/glvnd/egl_vendor.d/50_mesa.json",
            "--env",
            "LIBGL_DRIVERS_PATH=/brood/lib/dri",
            "--env",
            "GBM_BACKENDS_PATH=/brood/lib/gbm",
            "--env",
            "EGL_PLATFORM=surfaceless",
            "--env",
            "BROOD_ALLOW_SOFTWARE_EGL_CONFIG=1",
            id,
        ])
        .args([
            "/brood/bin/brood-remote",
            "container-session",
            "--package",
            package,
            "--listen",
            "0.0.0.0:44192",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .context("start backend inside Podman container")?;
    let stdin = child
        .stdin
        .take()
        .context("container backend stdin was not captured")?;
    Ok((child, stdin))
}

fn wait_for_listener(child: &mut Child) -> Result<()> {
    let stdout = child
        .stdout
        .take()
        .context("container backend stdout was not captured")?;
    let (sender, receiver) = mpsc::sync_channel(1);
    thread::Builder::new()
        .name("brood-container-output".to_string())
        .spawn(move || {
            let mut sender = Some(sender);
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else {
                    break;
                };
                eprintln!("[container] {line}");
                if line.starts_with("LISTEN=")
                    && let Some(sender) = sender.take()
                    && sender.send(()).is_err()
                {
                    break;
                }
            }
        })
        .context("spawn container backend output reader")?;
    receiver
        .recv_timeout(CONTAINER_START_TIMEOUT)
        .context("container relay did not publish a listener within 30 seconds")
}

fn published_port(podman: &Podman, id: &str) -> Result<u16> {
    let output = podman.output(["port", id, "44192/udp"])?;
    ensure!(
        output.status.success(),
        "querying container relay port failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let text = String::from_utf8(output.stdout).context("Podman port output is not UTF-8")?;
    text.lines()
        .filter_map(|line| line.trim().rsplit_once(':'))
        .find_map(|(_host, port)| port.parse::<u16>().ok())
        .context("Podman did not publish the container relay UDP port")
}

fn resolve_connect_host(podman: &Podman, override_host: Option<&str>) -> Result<IpAddr> {
    if let Some(host) = override_host {
        return resolve_ip(host);
    }
    let Some(connection) = &podman.connection else {
        return Ok(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
    };
    let output = Podman {
        executable: podman.executable.clone(),
        connection: None,
    }
    .output(["system", "connection", "list", "--format", "json"])?;
    ensure!(
        output.status.success(),
        "querying Podman connections failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let values: serde_json::Value =
        serde_json::from_slice(&output.stdout).context("decode Podman connection list")?;
    let entries = values
        .as_array()
        .context("Podman connection list is not an array")?;
    let uri = entries
        .iter()
        .find(|entry| {
            entry
                .get("Name")
                .or_else(|| entry.get("name"))
                .and_then(serde_json::Value::as_str)
                == Some(connection)
        })
        .and_then(|entry| entry.get("URI").or_else(|| entry.get("uri")))
        .and_then(serde_json::Value::as_str)
        .with_context(|| format!("Podman connection {connection:?} has no resolvable URI"))?;
    let host = uri_host(uri);
    resolve_ip(host)
}

fn uri_host(uri: &str) -> &str {
    let authority_with_user = uri
        .split_once("://")
        .map_or(uri, |(_scheme, remainder)| remainder)
        .split('/')
        .next()
        .unwrap_or_default();
    let authority = authority_with_user
        .rsplit_once('@')
        .map_or(authority_with_user, |(_user, host)| host);
    if let Some(bracketed) = authority.strip_prefix('[') {
        return bracketed
            .split_once(']')
            .map_or(bracketed, |(host, _port)| host);
    }
    authority
        .rsplit_once(':')
        .map_or(authority, |(host, port)| {
            port.parse::<u16>().map_or(authority, |_| host)
        })
}

fn resolve_ip(host: &str) -> Result<IpAddr> {
    if let Ok(ip) = host.parse() {
        return Ok(ip);
    }
    (host, 0_u16)
        .to_socket_addrs()
        .with_context(|| format!("resolve container connection host {host:?}"))?
        .map(|address| address.ip())
        .next()
        .with_context(|| format!("container connection host {host:?} resolved to no addresses"))
}

fn validate_image(image: &str) -> Result<()> {
    ensure!(
        !image.trim().is_empty(),
        "container image must not be empty"
    );
    ensure!(
        !image.bytes().any(|byte| byte.is_ascii_control()),
        "container image contains control characters"
    );
    Ok(())
}

fn resolve_podman(configured: Option<&Path>) -> Result<PathBuf> {
    configured
        .map(Path::to_path_buf)
        .or_else(|| env::var_os("BROOD_PODMAN_REAL").map(PathBuf::from))
        .or_else(|| find_in_path("podman"))
        .context("Podman is unavailable; install Podman or set BROOD_PODMAN_REAL")
}

fn resolve_backend(configured: Option<&Path>) -> Result<PathBuf> {
    configured
        .map(Path::to_path_buf)
        .or_else(|| env::var_os("BROOD_CONTAINER_BACKEND").map(PathBuf::from))
        .context("container backend bundle is unavailable; set BROOD_CONTAINER_BACKEND")?
        .canonicalize()
        .context("resolve portable container backend")
}

fn stage_backend(source: &Path) -> Result<(TempDir, PathBuf)> {
    let stage = tempfile::Builder::new()
        .prefix("brood-container-backend-")
        .tempdir()
        .context("create portable backend transfer directory")?;
    let destination = stage.path().join("backend");
    copy_tree(source, &destination)?;
    Ok((stage, destination))
}

fn copy_tree(source: &Path, destination: &Path) -> Result<()> {
    fs::create_dir_all(destination)
        .with_context(|| format!("create transfer directory {}", destination.display()))?;
    for entry in fs::read_dir(source)
        .with_context(|| format!("read portable backend directory {}", source.display()))?
    {
        let entry = entry.context("read portable backend entry")?;
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        let metadata = fs::symlink_metadata(&source_path)
            .with_context(|| format!("inspect portable backend entry {}", source_path.display()))?;
        if metadata.is_dir() {
            copy_tree(&source_path, &destination_path)?;
        } else if metadata.file_type().is_symlink() {
            let target = fs::read_link(&source_path)
                .with_context(|| format!("read backend symlink {}", source_path.display()))?;
            std::os::unix::fs::symlink(target, &destination_path)
                .with_context(|| format!("copy backend symlink {}", destination_path.display()))?;
        } else if metadata.is_file() {
            fs::copy(&source_path, &destination_path).with_context(|| {
                format!(
                    "copy portable backend file {} to {}",
                    source_path.display(),
                    destination_path.display()
                )
            })?;
            fs::set_permissions(&destination_path, metadata.permissions()).with_context(|| {
                format!(
                    "preserve backend permissions {}",
                    destination_path.display()
                )
            })?;
        } else {
            bail!(
                "portable backend entry {} is not a file, directory, or symlink",
                source_path.display()
            );
        }
    }
    Ok(())
}

fn resolve_client(configured: Option<&Path>) -> Result<PathBuf> {
    configured
        .map(Path::to_path_buf)
        .or_else(|| env::var_os("BROOD_CLIENT_BIN").map(PathBuf::from))
        .or_else(|| env::current_exe().ok().filter(|client| client.is_file()))
        .context("brood-client is unavailable; set BROOD_CLIENT_BIN")?
        .canonicalize()
        .context("resolve brood-client")
}

fn find_in_path(program: &str) -> Option<PathBuf> {
    env::var_os("PATH").and_then(|path| {
        env::split_paths(&path)
            .map(|directory| directory.join(program))
            .find(|candidate| candidate.is_file())
    })
}

struct Certificates {
    server_certificate: PathBuf,
    server_private_key: PathBuf,
    client_certificate: PathBuf,
    client_private_key: PathBuf,
}

impl Certificates {
    fn load_or_create(root: &Path) -> Result<Self> {
        fs::set_permissions(root, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("secure container state {}", root.display()))?;
        let certificates = Self {
            server_certificate: root.join("server.der"),
            server_private_key: root.join("server-key.der"),
            client_certificate: root.join("client.der"),
            client_private_key: root.join("client-key.der"),
        };
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
        for path in [
            &certificates.server_certificate,
            &certificates.server_private_key,
            &certificates.client_certificate,
            &certificates.client_private_key,
        ] {
            fs::set_permissions(path, fs::Permissions::from_mode(0o600))
                .with_context(|| format!("secure certificate {}", path.display()))?;
        }
        Ok(certificates)
    }
}

fn write_certificate(server_name: &str, certificate: &Path, key: &Path) -> Result<()> {
    let CertifiedKey { cert, signing_key } =
        generate_simple_self_signed(vec![server_name.to_string()])
            .context("generate container deployment certificate")?;
    fs::write(certificate, cert.der())
        .with_context(|| format!("write certificate {}", certificate.display()))?;
    fs::write(key, signing_key.serialize_der())
        .with_context(|| format!("write private key {}", key.display()))?;
    Ok(())
}

pub fn run_session(args: &ContainerSessionArgs) -> Result<()> {
    fs::create_dir_all(args.state.join("runtime")).context("create container runtime directory")?;
    fs::create_dir_all(args.state.join("artifacts")).context("create container artifact cache")?;
    let dbus = start_dbus(&args.backend)?;
    let mut server = start_session_server(args, &dbus.address)?;
    wait_for_server(args, &dbus.address, &mut server)?;
    let mut relay = start_relay(args, &dbus.address)?;
    if args.persistent {
        let status = relay.wait().context("wait for container relay")?;
        drop(server.kill());
        drop(server.wait());
        dbus.stop();
        ensure!(status.success(), "container relay exited with {status}");
        return Ok(());
    }
    let (sender, receiver) = mpsc::sync_channel(1);
    thread::Builder::new()
        .name("brood-container-lifetime".to_string())
        .spawn(move || {
            let mut input = std::io::stdin().lock();
            let mut sink = std::io::sink();
            drop(std::io::copy(&mut input, &mut sink));
            if sender.send(()).is_err() {}
        })
        .context("start container lifetime monitor")?;
    loop {
        if receiver.recv_timeout(Duration::from_millis(100)).is_ok() {
            break;
        }
        if let Some(status) = relay.try_wait().context("poll container relay")? {
            ensure!(status.success(), "container relay exited with {status}");
            break;
        }
    }
    drop(relay.kill());
    drop(relay.wait());
    drop(server.kill());
    drop(server.wait());
    dbus.stop();
    Ok(())
}

struct DbusProcess {
    child: Child,
    address: String,
}

impl DbusProcess {
    fn stop(mut self) {
        drop(self.child.kill());
        drop(self.child.wait());
    }
}

fn start_dbus(backend: &Path) -> Result<DbusProcess> {
    let executable = backend.join("bin/dbus-daemon");
    let mut child = Command::new(&executable)
        .arg("--config-file")
        .arg(backend.join("share/dbus-1/session.conf"))
        .args(["--nofork", "--nopidfile", "--print-address=1"])
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("start bundled {}", executable.display()))?;
    let stdout = child
        .stdout
        .take()
        .context("dbus-daemon stdout was not captured")?;
    let mut address = String::new();
    let bytes = BufReader::new(stdout)
        .read_line(&mut address)
        .context("read container D-Bus address")?;
    ensure!(bytes > 0, "container dbus-daemon returned no bus address");
    Ok(DbusProcess {
        child,
        address: address.trim().to_string(),
    })
}

fn start_session_server(args: &ContainerSessionArgs, dbus: &str) -> Result<Child> {
    let log = fs::File::create(args.state.join("server.log"))
        .context("create container backend server log")?;
    Command::new(args.backend.join("bin/brood-sessiond"))
        .args([
            "--headless",
            "--no-default-modules",
            "--config",
            args.backend.join("brood.toml").to_string_lossy().as_ref(),
            "--dbus-destination",
            "org.brood.Container.Remote",
        ])
        .env("DBUS_SESSION_BUS_ADDRESS", dbus)
        .env("XDG_RUNTIME_DIR", args.state.join("runtime"))
        .env("XKB_CONFIG_ROOT", args.backend.join("share/X11/xkb"))
        .stdout(Stdio::from(
            log.try_clone().context("clone container server log")?,
        ))
        .stderr(Stdio::from(log))
        .spawn()
        .context("start container session backend")
}

fn wait_for_server(args: &ContainerSessionArgs, dbus: &str, server: &mut Child) -> Result<()> {
    for _ in 0..150 {
        let status = Command::new(args.backend.join("bin/brood"))
            .args([
                "ctl",
                "--dbus-destination",
                "org.brood.Container.Remote",
                "status",
            ])
            .env("DBUS_SESSION_BUS_ADDRESS", dbus)
            .env("XDG_RUNTIME_DIR", args.state.join("runtime"))
            .env("XKB_CONFIG_ROOT", args.backend.join("share/X11/xkb"))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .context("probe container session backend")?;
        if status.success() {
            return Ok(());
        }
        if let Some(status) = server
            .try_wait()
            .context("poll container session backend")?
        {
            bail!("container session backend exited during startup with {status}");
        }
        thread::sleep(Duration::from_millis(100));
    }
    bail!("timed out waiting for container session backend")
}

fn start_relay(args: &ContainerSessionArgs, dbus: &str) -> Result<Child> {
    Command::new(args.backend.join("bin/brood-remote"))
        .arg("relay")
        .arg("--listen")
        .arg(args.listen.to_string())
        .arg("--trusted-client-certificate")
        .arg(args.state.join("client.der"))
        .arg("--certificate")
        .arg(args.state.join("server.der"))
        .arg("--private-key")
        .arg(args.state.join("server-key.der"))
        .args([
            "--dbus-destination",
            "org.brood.Container.Remote",
            "--allow-session-package",
            &args.package,
            "--artifact-cache",
        ])
        .arg(args.state.join("artifacts"))
        .env("DBUS_SESSION_BUS_ADDRESS", dbus)
        .env("XDG_RUNTIME_DIR", args.state.join("runtime"))
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .context("start container presentation relay")
}

pub fn hold() -> Result<()> {
    let mut input = std::io::stdin().lock();
    let mut buffer = [0_u8; 8_192];
    loop {
        match input.read(&mut buffer) {
            Ok(0) => thread::park(),
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error).context("hold container backend process"),
        }
    }
}

pub fn podman(args: &PodmanArgs) -> Result<()> {
    let executable = resolve_podman(None)?;
    if args
        .args
        .first()
        .is_some_and(|argument| argument == "connect")
    {
        return connect_container(&executable, &args.args);
    }
    let Some(run_index) = args.args.iter().position(|argument| argument == "run") else {
        let error = Command::new(executable).args(&args.args).exec();
        return Err(error).context("replace process with Podman");
    };
    run_with_backend(&executable, &args.args, run_index)
}

fn run_with_backend(executable: &Path, arguments: &[OsString], run_index: usize) -> Result<()> {
    let backend = resolve_backend(None)?;
    let (_backend_stage, backend) = stage_backend(&backend)?;
    let state = tempfile::Builder::new()
        .prefix("brood-podman-")
        .tempdir()
        .context("create Podman backend state")?;
    let _certificates = Certificates::load_or_create(state.path())?;
    let global = arguments
        .get(..run_index)
        .context("Podman run command index is invalid")?;
    let run_arguments = arguments
        .get(run_index.saturating_add(1)..)
        .context("Podman run arguments are invalid")?;
    let detached = run_arguments.iter().any(|argument| is_detach(argument));
    let host_network = run_arguments.windows(2).any(|pair| {
        pair.first().is_some_and(|argument| argument == "--network")
            && pair.get(1).is_some_and(|argument| argument == "host")
    }) || run_arguments
        .iter()
        .any(|argument| argument == "--network=host");
    let explicit_gpu = run_arguments.iter().any(|argument| {
        argument == OsStr::new("--gpus")
            || argument.to_string_lossy().strip_prefix("--gpus=").is_some()
    });
    let auto_gpu =
        !explicit_gpu && env::var("BROOD_CONTAINER_GPU").map_or(true, |value| value != "never");
    let create = |gpu: GpuAccess| -> Result<std::process::Output> {
        let mut command = Command::new(executable);
        command
            .args(global)
            .arg("create")
            .arg("--label")
            .arg("io.brood.backend=true");
        if !host_network {
            command
                .arg("--publish")
                .arg(format!("{CONTAINER_RELAY_PORT}/udp"));
        }
        match gpu {
            GpuAccess::Cdi => {
                command.args(["--gpus", "all"]);
            }
            GpuAccess::Drm => {
                command.args(["--device", "/dev/dri"]);
            }
            GpuAccess::None => {}
        }
        command.args(without_detach(run_arguments));
        command.output().context("create backend-enabled container")
    };
    let attempts: &[GpuAccess] = if auto_gpu {
        &[GpuAccess::Drm, GpuAccess::Cdi, GpuAccess::None]
    } else {
        &[GpuAccess::None]
    };
    let mut started = None;
    let mut last_failure = String::new();
    for access in attempts {
        let output = create(*access)?;
        if !output.status.success() {
            last_failure = String::from_utf8_lossy(&output.stderr).trim().to_string();
            eprintln!("brood-podman: GPU attempt {access:?} unavailable: {last_failure}");
            continue;
        }
        let id = String::from_utf8(output.stdout)
            .context("Podman container ID is not UTF-8")?
            .trim()
            .to_string();
        ensure!(!id.is_empty(), "Podman create returned no container ID");
        if let Err(error) = copy_raw(executable, global, &id, &backend, state.path()) {
            drop(remove_raw(executable, global, &id));
            return Err(error);
        }
        let start = Command::new(executable)
            .args(global)
            .args([OsStr::new("start"), OsStr::new(&id)])
            .stdout(Stdio::null())
            .status()
            .context("start backend-enabled container")?;
        if start.success() {
            eprintln!("brood-podman: gpu-access={access:?}");
            started = Some(id);
            break;
        }
        last_failure = format!("Podman start exited with {start}");
        eprintln!("brood-podman: GPU attempt {access:?} unavailable: {last_failure}");
        drop(remove_raw(executable, global, &id));
    }
    let id = started.with_context(|| format!("Podman run failed: {last_failure}"))?;
    start_persistent_backend(executable, global, &id)
        .context("container started but its Brood backend did not")?;
    let podman = Podman {
        executable: executable.to_path_buf(),
        connection: podman_connection(global),
    };
    let connect_host = resolve_connect_host(&podman, None)?;
    let connect = if host_network {
        SocketAddr::new(connect_host, CONTAINER_RELAY_PORT)
    } else {
        SocketAddr::new(connect_host, published_port_raw(executable, global, &id)?)
    };
    save_container_connection(&id, connect, state.path())?;
    eprintln!("brood-container: backend ready; connect with `brood container connect {id}`");
    if detached {
        println!("{id}");
        return Ok(());
    }
    let error = Command::new(executable)
        .args(global)
        .args([OsStr::new("attach"), OsStr::new(&id)])
        .exec();
    Err(error).context("replace process with Podman attach")
}

fn podman_connection(global: &[OsString]) -> Option<String> {
    global
        .windows(2)
        .find_map(|pair| {
            if pair
                .first()
                .is_some_and(|argument| argument == "--connection" || argument == "-c")
            {
                pair.get(1)
                    .map(|argument| argument.to_string_lossy().into_owned())
            } else {
                None
            }
        })
        .or_else(|| {
            global.iter().find_map(|argument| {
                argument
                    .to_string_lossy()
                    .strip_prefix("--connection=")
                    .map(str::to_string)
            })
        })
}

fn is_detach(argument: &OsStr) -> bool {
    detach_setting(argument).is_some_and(|enabled| enabled)
        || compact_detach_flags(argument).is_some()
}

fn without_detach(arguments: &[OsString]) -> Vec<OsString> {
    arguments
        .iter()
        .filter_map(|argument| {
            if detach_setting(argument).is_some() {
                return None;
            }
            if let Some(flags) = compact_detach_flags(argument) {
                let retained = flags
                    .chars()
                    .filter(|character| *character != 'd')
                    .collect::<String>();
                return (!retained.is_empty()).then(|| OsString::from(format!("-{retained}")));
            }
            Some(argument.clone())
        })
        .collect()
}

fn detach_setting(argument: &OsStr) -> Option<bool> {
    match argument.to_str()? {
        "-d" | "-d=true" | "--detach" | "--detach=true" => Some(true),
        "-d=false" | "--detach=false" => Some(false),
        _ => None,
    }
}

fn compact_detach_flags(argument: &OsStr) -> Option<&str> {
    let flags = argument.to_str()?.strip_prefix('-')?;
    (!flags.starts_with('-')
        && flags.len() > 1
        && flags
            .chars()
            .all(|character| character.is_ascii_alphabetic())
        && flags.contains('d'))
    .then_some(flags)
}

fn published_port_raw(executable: &Path, global: &[OsString], id: &str) -> Result<u16> {
    let output = Command::new(executable)
        .args(global)
        .args(["port", id, "44192/udp"])
        .output()
        .context("query backend-enabled container relay port")?;
    ensure!(
        output.status.success(),
        "querying container relay port failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let text = String::from_utf8(output.stdout).context("Podman port output is not UTF-8")?;
    text.lines()
        .filter_map(|line| line.trim().rsplit_once(':'))
        .find_map(|(_host, port)| port.parse::<u16>().ok())
        .context("Podman did not publish the container relay UDP port")
}

fn container_connection_root() -> Result<PathBuf> {
    let root = env::var_os("BROOD_CONTAINER_STATE_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            env::var_os("XDG_RUNTIME_DIR")
                .map(PathBuf::from)
                .map(|path| path.join("brood/containers"))
        })
        .or_else(|| {
            env::var_os("XDG_STATE_HOME")
                .map(PathBuf::from)
                .map(|path| path.join("brood/containers"))
        })
        .or_else(|| {
            env::var_os("HOME")
                .map(PathBuf::from)
                .map(|path| path.join(".local/state/brood/containers"))
        })
        .context("no container state directory is available")?;
    fs::create_dir_all(&root)
        .with_context(|| format!("create container connection state {}", root.display()))?;
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("secure container connection state {}", root.display()))?;
    Ok(root)
}

fn save_container_connection(id: &str, connect: SocketAddr, source: &Path) -> Result<()> {
    ensure!(
        !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "Podman returned an invalid container ID"
    );
    let destination = container_connection_root()?.join(id);
    fs::create_dir_all(&destination)
        .with_context(|| format!("create container connection {}", destination.display()))?;
    fs::set_permissions(&destination, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("secure container connection {}", destination.display()))?;
    for name in ["server.der", "client.der", "client-key.der"] {
        let target = destination.join(name);
        fs::copy(source.join(name), &target)
            .with_context(|| format!("retain container credential {}", target.display()))?;
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("secure container credential {}", target.display()))?;
    }
    let manifest = SavedContainerConnection {
        container: id.to_string(),
        connect,
    };
    let encoded = serde_json::to_vec_pretty(&manifest).context("encode container connection")?;
    fs::write(destination.join("connection.json"), encoded)
        .context("write container connection manifest")?;
    Ok(())
}

fn connect_container(executable: &Path, arguments: &[OsString]) -> Result<()> {
    let target = arguments
        .get(1)
        .context("usage: brood container connect CONTAINER [-- CLIENT_ARGS...]")?;
    let root = container_connection_root()?;
    let direct = root.join(target);
    let target_is_id = target.to_str().is_some_and(|value| {
        !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_hexdigit())
    });
    let (id, state) = if target_is_id && direct.join("connection.json").is_file() {
        (target.to_string_lossy().into_owned(), direct)
    } else {
        let output = Command::new(executable)
            .args([
                OsStr::new("inspect"),
                OsStr::new("--format"),
                OsStr::new("{{.Id}}"),
            ])
            .arg(target)
            .output()
            .context("inspect container for Brood connection")?;
        ensure!(
            output.status.success(),
            "Podman could not inspect container {}: {}",
            target.to_string_lossy(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
        let id = String::from_utf8(output.stdout)
            .context("Podman container ID is not UTF-8")?
            .trim()
            .to_string();
        let state = root.join(&id);
        (id, state)
    };
    let manifest: SavedContainerConnection = serde_json::from_slice(
        &fs::read(state.join("connection.json"))
            .with_context(|| format!("container {id} has no saved Brood connection"))?,
    )
    .context("decode container connection manifest")?;
    ensure!(
        manifest.container == id,
        "container connection identity mismatch"
    );
    let client = resolve_client(None)?;
    let (package, root) = super::ssh::resolve_default_session_package(&client)?;
    let client_arguments = arguments.get(2..).unwrap_or_default();
    let client_arguments = client_arguments
        .first()
        .filter(|argument| argument.as_os_str() == "--")
        .map_or(client_arguments, |_| {
            client_arguments.get(1..).unwrap_or_default()
        });
    let status = brood_launch::command(&client, "client")
        .arg("--remote-connect")
        .arg(manifest.connect.to_string())
        .arg("--include-local")
        .arg("--remote-server-name")
        .arg("localhost")
        .arg("--remote-server-certificate")
        .arg(state.join("server.der"))
        .arg("--remote-client-certificate")
        .arg(state.join("client.der"))
        .arg("--remote-client-private-key")
        .arg(state.join("client-key.der"))
        .arg("--remote-offer-session-package")
        .arg(format!("{package}={}", root.display()))
        .args(client_arguments)
        .status()
        .with_context(|| format!("launch local presentation client {}", client.display()))?;
    ensure!(
        status.success(),
        "local presentation client exited with {status}"
    );
    Ok(())
}

fn copy_raw(
    executable: &Path,
    global: &[OsString],
    id: &str,
    backend: &Path,
    state: &Path,
) -> Result<()> {
    let backend_source = format!("{}/.", backend.display());
    let backend_destination = format!("{id}:/brood");
    let output = Command::new(executable)
        .args(global)
        .args(["cp", &backend_source, &backend_destination])
        .output()
        .context("copy Brood backend into Podman container")?;
    ensure!(
        output.status.success(),
        "copying Brood backend failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let state_source = format!("{}/.", state.display());
    let state_destination = format!("{id}:/brood/state");
    let output = Command::new(executable)
        .args(global)
        .args(["cp", &state_source, &state_destination])
        .output()
        .context("copy Brood container credentials")?;
    ensure!(
        output.status.success(),
        "copying Brood container credentials failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
}

fn remove_raw(executable: &Path, global: &[OsString], id: &str) -> Result<()> {
    let status = Command::new(executable)
        .args(global)
        .args([OsStr::new("rm"), OsStr::new("--force"), OsStr::new(id)])
        .stdout(Stdio::null())
        .status()
        .context("remove failed backend-enabled container")?;
    ensure!(status.success(), "Podman rm exited with {status}");
    Ok(())
}

fn start_persistent_backend(executable: &Path, global: &[OsString], id: &str) -> Result<()> {
    let status = Command::new(executable)
        .args(global)
        .args(["exec", "--detach"])
        .args(container_environment())
        .arg(id)
        .args([
            "/brood/bin/brood-remote",
            "container-session",
            "--persistent",
            "--listen",
            "0.0.0.0:44192",
        ])
        .stdout(Stdio::null())
        .status()
        .context("start persistent Brood container backend")?;
    ensure!(status.success(), "Podman exec exited with {status}");
    Ok(())
}

const fn container_environment() -> [&'static str; 12] {
    [
        "--env",
        "XKB_CONFIG_ROOT=/brood/share/X11/xkb",
        "--env",
        "__EGL_VENDOR_LIBRARY_FILENAMES=/brood/share/glvnd/egl_vendor.d/50_mesa.json",
        "--env",
        "LIBGL_DRIVERS_PATH=/brood/lib/dri",
        "--env",
        "GBM_BACKENDS_PATH=/brood/lib/gbm",
        "--env",
        "EGL_PLATFORM=surfaceless",
        "--env",
        "BROOD_ALLOW_SOFTWARE_EGL_CONFIG=1",
    ]
}

#[cfg(test)]
mod tests {
    use std::ffi::{OsStr, OsString};

    use super::{is_detach, podman_connection, uri_host, without_detach};

    #[test]
    fn extracts_hosts_from_podman_connection_uris() {
        assert_eq!(
            uri_host("ssh://builder@example.test:22/run/podman.sock"),
            "example.test"
        );
        assert_eq!(
            uri_host("ssh://builder@[2001:db8::42]:2222/run/podman.sock"),
            "2001:db8::42"
        );
        assert_eq!(uri_host("tcp://192.0.2.10:8080"), "192.0.2.10");
    }

    #[test]
    fn removes_detach_without_losing_compact_run_flags() {
        let arguments = [
            OsString::from("--rm"),
            OsString::from("-dit"),
            OsString::from("alpine"),
        ];
        assert!(is_detach(OsStr::new("-dit")));
        assert_eq!(
            without_detach(&arguments),
            [
                OsString::from("--rm"),
                OsString::from("-it"),
                OsString::from("alpine")
            ]
        );
    }

    #[test]
    fn detach_normalization_preserves_unrelated_values() {
        let arguments = [
            OsString::from("--detach=false"),
            OsString::from("-v/tmp:/data"),
            OsString::from("alpine"),
        ];
        assert!(!is_detach(OsStr::new("--detach=false")));
        assert!(!is_detach(OsStr::new("-v/tmp:/data")));
        assert_eq!(
            without_detach(&arguments),
            [OsString::from("-v/tmp:/data"), OsString::from("alpine")]
        );
    }

    #[test]
    fn finds_named_podman_connection_in_global_arguments() {
        assert_eq!(
            podman_connection(&[OsString::from("--connection"), OsString::from("builder")]),
            Some("builder".to_string())
        );
        assert_eq!(
            podman_connection(&[OsString::from("--connection=builder")]),
            Some("builder".to_string())
        );
    }
}
