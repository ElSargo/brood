use std::{
    collections::BTreeMap,
    env,
    ffi::{OsStr, OsString},
    fmt::{self, Write as _},
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Child, Command},
    time::Duration,
};

use anyhow::{Context, Result, bail};

use crate::state::Manifold;

const DEFAULT_URL: &str = "about:blank";

#[derive(Debug, Default)]
pub struct BrowserManager {
    next_id: u64,
    instances: Vec<BrowserInstance>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct BrowserLaunchOptions {
    pub url: String,
    pub chromium: Option<PathBuf>,
    pub profile_dir: Option<PathBuf>,
    pub extensions: Vec<PathBuf>,
    pub env: BTreeMap<String, String>,
    pub unset_env: Vec<String>,
    pub extra_args: Vec<String>,
    pub app_mode: bool,
    pub platform: BrowserPlatform,
}

#[derive(Debug, Clone)]
pub struct BrowserInstanceSummary {
    pub id: u64,
    pub pid: u32,
    pub url: String,
    pub profile_dir: PathBuf,
    pub devtools_port: u16,
    pub extensions: Vec<PathBuf>,
    pub env: BTreeMap<String, String>,
    pub unset_env: Vec<String>,
    pub platform: BrowserPlatform,
    pub argv: Vec<String>,
}

#[derive(Debug, Clone, Copy, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
#[derive(Default)]
pub enum BrowserPlatform {
    #[default]
    Auto,
    Wayland,
    X11,
}

#[derive(Debug)]
struct BrowserInstance {
    summary: BrowserInstanceSummary,
    child: Child,
}

impl Default for BrowserLaunchOptions {
    fn default() -> Self {
        Self {
            url: DEFAULT_URL.to_string(),
            chromium: None,
            profile_dir: None,
            extensions: Vec::new(),
            env: BTreeMap::new(),
            unset_env: Vec::new(),
            extra_args: Vec::new(),
            app_mode: true,
            platform: BrowserPlatform::Auto,
        }
    }
}

impl fmt::Display for BrowserPlatform {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Auto => formatter.write_str("auto"),
            Self::Wayland => formatter.write_str("wayland"),
            Self::X11 => formatter.write_str("x11"),
        }
    }
}

impl BrowserManager {
    pub fn spawn(
        &mut self,
        options: BrowserLaunchOptions,
        wayland_display: &OsStr,
    ) -> Result<&BrowserInstanceSummary> {
        self.reap_finished();

        let id = self.next_id;
        self.next_id += 1;

        let chromium = resolve_chromium(options.chromium.as_deref())?;
        let profile_dir = options.profile_dir.unwrap_or_else(|| {
            env::temp_dir()
                .join("manifold-browser")
                .join(format!("pane-{id}"))
        });
        fs::create_dir_all(&profile_dir)
            .with_context(|| format!("create browser profile {}", profile_dir.display()))?;

        let extensions = canonical_extensions(&options.extensions)?;
        let devtools_port = reserve_local_port()?;
        let use_wayland = match options.platform {
            BrowserPlatform::Auto => !wayland_display.is_empty(),
            BrowserPlatform::Wayland => true,
            BrowserPlatform::X11 => false,
        };

        let mut command = Command::new(&chromium);
        if use_wayland {
            command
                .env("WAYLAND_DISPLAY", wayland_display)
                .env("XDG_SESSION_TYPE", "wayland")
                .env("GDK_BACKEND", "wayland,x11")
                .env("QT_QPA_PLATFORM", "wayland;xcb");
        } else {
            command.env_remove("WAYLAND_DISPLAY");
        }
        for (key, value) in &options.env {
            command.env(key, value);
        }
        for key in &options.unset_env {
            command.env_remove(key);
        }

        let mut argv = vec![
            format!("--user-data-dir={}", profile_dir.display()),
            "--no-first-run".to_string(),
            "--no-default-browser-check".to_string(),
            "--disable-background-networking".to_string(),
            "--disable-client-side-phishing-detection".to_string(),
            "--disable-sync".to_string(),
            "--hide-crash-restore-bubble".to_string(),
            "--remote-debugging-address=127.0.0.1".to_string(),
            format!("--remote-debugging-port={devtools_port}"),
        ];

        if use_wayland {
            argv.push("--enable-features=UseOzonePlatform".to_string());
            argv.push("--ozone-platform=wayland".to_string());
        }

        if extensions.is_empty() {
            argv.push("--disable-extensions".to_string());
        } else {
            let extension_paths = extensions
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(",");
            argv.push(format!("--disable-extensions-except={extension_paths}"));
            argv.push(format!("--load-extension={extension_paths}"));
        }

        argv.extend(options.extra_args);
        if options.app_mode {
            argv.push(format!("--app={}", options.url));
        } else {
            argv.push("--new-window".to_string());
            argv.push(options.url.clone());
        }
        command.args(&argv);

        let child = command.spawn().with_context(|| {
            format!(
                "spawn Chromium {} with WAYLAND_DISPLAY={}",
                chromium.display(),
                wayland_display.to_string_lossy()
            )
        })?;
        let pid = child.id();

        let summary = BrowserInstanceSummary {
            id,
            pid,
            url: options.url,
            profile_dir,
            devtools_port,
            extensions,
            env: options.env,
            unset_env: options.unset_env,
            platform: options.platform,
            argv,
        };
        self.instances.push(BrowserInstance { summary, child });
        let Some(instance) = self.instances.last() else {
            bail!("inserted browser disappeared");
        };
        Ok(&instance.summary)
    }

    pub fn list(&mut self) -> Vec<&BrowserInstanceSummary> {
        self.reap_finished();
        self.instances
            .iter()
            .map(|instance| &instance.summary)
            .collect()
    }

    pub fn close(&mut self, id: u64) -> Result<BrowserInstanceSummary> {
        self.reap_finished();
        let Some(index) = self
            .instances
            .iter()
            .position(|instance| instance.summary.id == id)
        else {
            bail!("unknown browser id {id}");
        };

        terminate_instance(self.instances.remove(index))
            .with_context(|| format!("close browser {id}"))
    }

    pub fn close_all(&mut self) -> Vec<BrowserInstanceSummary> {
        self.reap_finished();
        self.instances
            .drain(..)
            .map(terminate_instance_lossy)
            .collect()
    }

    pub fn get(&mut self, id: u64) -> Result<&BrowserInstanceSummary> {
        self.reap_finished();
        self.instances
            .iter()
            .find(|instance| instance.summary.id == id)
            .map(|instance| &instance.summary)
            .with_context(|| format!("unknown browser id {id}"))
    }

    pub fn len(&mut self) -> usize {
        self.reap_finished();
        self.instances.len()
    }

    fn reap_finished(&mut self) {
        self.instances
            .retain_mut(|instance| match instance.child.try_wait() {
                Ok(Some(_status)) => false,
                Ok(None) | Err(_) => true,
            });
    }
}

impl Drop for BrowserManager {
    fn drop(&mut self) {
        for instance in self.instances.drain(..) {
            let _summary = terminate_instance_lossy(instance);
        }
    }
}

pub fn handle_control(args: &[String], state: &mut Manifold) -> Result<String> {
    let Some((subcommand, rest)) = args.split_first() else {
        bail!("browser requires a subcommand: open, list, close");
    };

    match subcommand.as_str() {
        "open" | "spawn" => {
            let options = parse_open_options(rest)?;
            let summary = state.spawn_browser_pane(options)?;
            Ok(format_browser_opened(summary))
        }
        "open-config" => {
            let Some(path) = rest.first() else {
                bail!("browser open-config requires a JSON file path");
            };
            let options = read_launch_config(path)?;
            let summary = state.spawn_browser_pane(options)?;
            Ok(format_browser_opened(summary))
        }
        "list" | "status" => Ok(format_browser_list(state.browsers.list())),
        "list-json" => Ok(format_browser_list_json(state.browsers.list())?),
        "inspect" => {
            let browser = browser_from_args(rest, state)?;
            Ok(format_browser_inspect(browser))
        }
        "inspect-json" => {
            let browser = browser_from_args(rest, state)?;
            Ok(format_browser_inspect_json(browser)?)
        }
        "tabs" => {
            let browser = browser_from_args(rest, state)?;
            let response = devtools_request(browser.devtools_port, "GET", "/json/list", "")?;
            Ok(format_devtools_response(&response))
        }
        "new" => {
            let browser = browser_from_args(rest, state)?;
            let url = rest.get(1).map_or(DEFAULT_URL, String::as_str);
            let path = format!("/json/new?{}", percent_encode(url));
            let response = devtools_request(browser.devtools_port, "PUT", &path, "")?;
            Ok(format_devtools_response(&response))
        }
        "activate" => {
            let browser = browser_from_args(rest, state)?;
            let target_id = take_value(rest, 1, "browser activate target id")?;
            let path = format!("/json/activate/{}", percent_encode_path_segment(target_id));
            let response = devtools_request(browser.devtools_port, "GET", &path, "")?;
            Ok(format_devtools_response(&response))
        }
        "close-target" => {
            let browser = browser_from_args(rest, state)?;
            let target_id = take_value(rest, 1, "browser close-target target id")?;
            let path = format!("/json/close/{}", percent_encode_path_segment(target_id));
            let response = devtools_request(browser.devtools_port, "GET", &path, "")?;
            Ok(format_devtools_response(&response))
        }
        "devtools" => {
            let browser = browser_from_args(rest, state)?;
            let method = rest.get(1).map_or("GET", String::as_str);
            let path = rest.get(2).map_or("/json/version", String::as_str);
            let body = rest.get(3).map_or("", String::as_str);
            let response = devtools_request(browser.devtools_port, method, path, body)?;
            Ok(format_devtools_response(&response))
        }
        "close" => {
            let Some(id) = rest.first() else {
                bail!("browser close requires an id");
            };
            let id = id.parse::<u64>().context("browser id must be a number")?;
            let summary = state.browsers.close(id)?;
            Ok(format!(
                "OK browser closed id={} pid={}\n",
                summary.id, summary.pid
            ))
        }
        "close-all" => {
            let summaries = state.browsers.close_all();
            let mut output = format!("OK browsers_closed={}\n", summaries.len());
            for summary in summaries {
                let _format_result =
                    writeln!(&mut output, "browser id={} pid={}", summary.id, summary.pid);
            }
            Ok(output)
        }
        _ => bail!("unknown browser subcommand: {subcommand}"),
    }
}

fn terminate_instance(mut instance: BrowserInstance) -> Result<BrowserInstanceSummary> {
    if instance.child.try_wait()?.is_none() {
        instance.child.kill()?;
    }
    let _wait_result = instance.child.wait();
    Ok(instance.summary)
}

fn terminate_instance_lossy(mut instance: BrowserInstance) -> BrowserInstanceSummary {
    if matches!(instance.child.try_wait(), Ok(None)) {
        let _kill_result = instance.child.kill();
    }
    let _wait_result = instance.child.wait();
    instance.summary
}

fn read_launch_config(path: &str) -> Result<BrowserLaunchOptions> {
    let contents =
        fs::read_to_string(path).with_context(|| format!("read browser config {path}"))?;
    serde_json::from_str(&contents).with_context(|| format!("parse browser config {path}"))
}

fn browser_from_args<'a>(
    args: &[String],
    state: &'a mut Manifold,
) -> Result<&'a BrowserInstanceSummary> {
    let Some(id) = args.first() else {
        bail!("browser command requires a browser id");
    };
    let id = id.parse::<u64>().context("browser id must be a number")?;
    state.browsers.get(id)
}

fn parse_open_options(args: &[String]) -> Result<BrowserLaunchOptions> {
    let mut options = BrowserLaunchOptions::default();
    let mut positional_url = None;
    let mut index = 0;

    while index < args.len() {
        match args[index].as_str() {
            "--url" => {
                index += 1;
                options.url = take_value(args, index, "--url")?.to_string();
            }
            "--chromium" | "--browser" => {
                index += 1;
                options.chromium = Some(PathBuf::from(take_value(args, index, "--chromium")?));
            }
            "--profile-dir" => {
                index += 1;
                options.profile_dir =
                    Some(PathBuf::from(take_value(args, index, "--profile-dir")?));
            }
            "--extension" => {
                index += 1;
                options
                    .extensions
                    .push(PathBuf::from(take_value(args, index, "--extension")?));
            }
            "--env" => {
                index += 1;
                let assignment = take_value(args, index, "--env")?;
                let Some((key, value)) = assignment.split_once('=') else {
                    bail!("--env requires KEY=VALUE");
                };
                options.env.insert(key.to_string(), value.to_string());
            }
            "--unset-env" => {
                index += 1;
                options
                    .unset_env
                    .push(take_value(args, index, "--unset-env")?.to_string());
            }
            "--arg" => {
                index += 1;
                options
                    .extra_args
                    .push(take_value(args, index, "--arg")?.to_string());
            }
            "--ui" => {
                options.app_mode = false;
            }
            "--app" => {
                options.app_mode = true;
            }
            "--platform" => {
                index += 1;
                options.platform = parse_platform(take_value(args, index, "--platform")?)?;
            }
            value if value.starts_with("--") => bail!("unknown browser open option: {value}"),
            value => {
                if positional_url.replace(value.to_string()).is_some() {
                    bail!("browser open accepts at most one positional URL");
                }
            }
        }
        index += 1;
    }

    if let Some(url) = positional_url {
        options.url = url;
    }
    Ok(options)
}

fn parse_platform(value: &str) -> Result<BrowserPlatform> {
    match value {
        "auto" => Ok(BrowserPlatform::Auto),
        "wayland" => Ok(BrowserPlatform::Wayland),
        "x11" => Ok(BrowserPlatform::X11),
        _ => bail!("platform must be one of: auto, wayland, x11"),
    }
}

fn take_value<'a>(args: &'a [String], index: usize, option: &str) -> Result<&'a str> {
    args.get(index)
        .map(String::as_str)
        .with_context(|| format!("{option} requires a value"))
}

fn resolve_chromium(explicit: Option<&Path>) -> Result<OsString> {
    if let Some(path) = explicit {
        return Ok(path.as_os_str().to_os_string());
    }

    for variable in ["MANIFOLD_CHROMIUM", "CHROMIUM"] {
        if let Some(value) = env::var_os(variable).filter(|value| !value.is_empty()) {
            return Ok(value);
        }
    }

    for candidate in [
        "chromium",
        "chromium-browser",
        "google-chrome-stable",
        "google-chrome",
        "chrome",
    ] {
        if path_lookup(candidate).is_some() {
            return Ok(OsString::from(candidate));
        }
    }

    bail!("could not find Chromium; set MANIFOLD_CHROMIUM or pass browser open --chromium <path>")
}

fn path_lookup(binary: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    env::split_paths(&path)
        .map(|dir| dir.join(binary))
        .find(|candidate| candidate.is_file())
}

fn canonical_extensions(paths: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut canonical = Vec::with_capacity(paths.len());
    for path in paths {
        let resolved = fs::canonicalize(path)
            .with_context(|| format!("resolve extension path {}", path.display()))?;
        canonical.push(resolved);
    }
    Ok(canonical)
}

fn reserve_local_port() -> Result<u16> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).context("reserve DevTools port")?;
    Ok(listener.local_addr()?.port())
}

struct DevtoolsResponse {
    status: String,
    body: String,
}

fn devtools_request(port: u16, method: &str, path: &str, body: &str) -> Result<DevtoolsResponse> {
    if !path.starts_with('/') {
        bail!("DevTools path must start with /");
    }

    let mut stream = TcpStream::connect(("127.0.0.1", port))
        .with_context(|| format!("connect DevTools port {port}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .context("set DevTools read timeout")?;
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .context("set DevTools write timeout")?;

    write!(
        stream,
        "{} {} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
        method.to_ascii_uppercase(),
        path,
        port,
        body.len(),
        body
    )
    .context("write DevTools request")?;

    let mut raw = String::new();
    stream
        .read_to_string(&mut raw)
        .context("read DevTools response")?;
    let (head, body) = raw.split_once("\r\n\r\n").unwrap_or((raw.as_str(), ""));
    let status = head.lines().next().unwrap_or("HTTP/1.1 000").to_string();
    Ok(DevtoolsResponse {
        status,
        body: body.to_string(),
    })
}

fn format_devtools_response(response: &DevtoolsResponse) -> String {
    format!(
        "OK devtools_status={}\n{}\n",
        response.status, response.body
    )
}

fn percent_encode(input: &str) -> String {
    input
        .bytes()
        .flat_map(|byte| match byte {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'~'
            | b':'
            | b'/'
            | b'?'
            | b'&'
            | b'='
            | b'#'
            | b'%' => vec![byte as char],
            _ => format!("%{byte:02X}").chars().collect(),
        })
        .collect()
}

fn percent_encode_path_segment(input: &str) -> String {
    input
        .bytes()
        .flat_map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                vec![byte as char]
            }
            _ => format!("%{byte:02X}").chars().collect(),
        })
        .collect()
}

fn format_browser_opened(summary: &BrowserInstanceSummary) -> String {
    format!(
        "OK browser id={} pid={} url={} platform={} devtools=http://127.0.0.1:{} profile={}\n",
        summary.id,
        summary.pid,
        summary.url,
        summary.platform,
        summary.devtools_port,
        summary.profile_dir.display()
    )
}

fn format_browser_list(summaries: Vec<&BrowserInstanceSummary>) -> String {
    if summaries.is_empty() {
        return "OK browsers=0\n".to_string();
    }

    let mut output = format!("OK browsers={}\n", summaries.len());
    for summary in summaries {
        let _format_result = writeln!(
            &mut output,
            "browser id={} pid={} url={} platform={} devtools=http://127.0.0.1:{} profile={} extensions={} env={} unset_env={}",
            summary.id,
            summary.pid,
            summary.url,
            summary.platform,
            summary.devtools_port,
            summary.profile_dir.display(),
            summary.extensions.len(),
            summary.env.len(),
            summary.unset_env.len()
        );
    }
    output
}

fn format_browser_list_json(summaries: Vec<&BrowserInstanceSummary>) -> Result<String> {
    let browsers = summaries
        .into_iter()
        .map(BrowserInstanceView::from)
        .collect::<Vec<_>>();
    Ok(format!("{}\n", serde_json::to_string(&browsers)?))
}

fn format_browser_inspect(summary: &BrowserInstanceSummary) -> String {
    let mut output = format!(
        "OK browser id={} pid={} url={} platform={} devtools=http://127.0.0.1:{} profile={}\n",
        summary.id,
        summary.pid,
        summary.url,
        summary.platform,
        summary.devtools_port,
        summary.profile_dir.display()
    );

    for extension in &summary.extensions {
        let _format_result = writeln!(&mut output, "extension={}", extension.display());
    }
    for key in summary.env.keys() {
        let _format_result = writeln!(&mut output, "env={key}");
    }
    for key in &summary.unset_env {
        let _format_result = writeln!(&mut output, "unset_env={key}");
    }
    for arg in &summary.argv {
        let _format_result = writeln!(&mut output, "arg={arg}");
    }

    output
}

fn format_browser_inspect_json(summary: &BrowserInstanceSummary) -> Result<String> {
    Ok(format!(
        "{}\n",
        serde_json::to_string(&BrowserInstanceView::from(summary))?
    ))
}

#[derive(serde::Serialize)]
#[serde(rename_all = "kebab-case")]
struct BrowserInstanceView {
    id: u64,
    pid: u32,
    url: String,
    profile_dir: String,
    devtools_port: u16,
    devtools_url: String,
    extensions: Vec<String>,
    env_keys: Vec<String>,
    unset_env: Vec<String>,
    platform: BrowserPlatform,
    argv: Vec<String>,
}

impl From<&BrowserInstanceSummary> for BrowserInstanceView {
    fn from(summary: &BrowserInstanceSummary) -> Self {
        Self {
            id: summary.id,
            pid: summary.pid,
            url: summary.url.clone(),
            profile_dir: summary.profile_dir.display().to_string(),
            devtools_port: summary.devtools_port,
            devtools_url: format!("http://127.0.0.1:{}", summary.devtools_port),
            extensions: summary
                .extensions
                .iter()
                .map(|path| path.display().to_string())
                .collect(),
            env_keys: summary.env.keys().cloned().collect(),
            unset_env: summary.unset_env.clone(),
            platform: summary.platform,
            argv: summary.argv.clone(),
        }
    }
}
