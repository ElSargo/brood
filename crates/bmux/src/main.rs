#![allow(
    clippy::too_many_lines,
    clippy::unnecessary_wraps,
    reason = "tmux's compact, flag-oriented command grammar is kept together so aliases share one parser"
)]

use std::{
    env,
    ffi::OsString,
    path::{Path, PathBuf},
    process::ExitCode,
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use brood_cli::{
    BROOD_SESSION_ENV, BROODD_DBUS_ADDRESS_ENV, BROODD_DBUS_DESTINATION_ENV,
    DEFAULT_BROOD_DBUS_NAME,
    control::{
        ControlTarget, ServerLaunch, activate_surface_session, close_surface_session,
        connect_or_spawn, list_surface_sessions, rename_surface_session, request_internal_command,
        session_marker_value, spawn_client,
    },
};
use serde::Deserialize;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const TERMINAL_WAIT: Duration = Duration::from_secs(8);
const TERMINAL_POLL: Duration = Duration::from_millis(40);

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
struct Terminal {
    session: u64,
    ordinal: u64,
    actor: String,
}

impl Terminal {
    const fn pane_id(&self) -> u64 {
        (self.session << 32) | self.ordinal
    }

    const fn pane_index(&self) -> u64 {
        self.ordinal - 1
    }
}

struct Bmux {
    target: ControlTarget,
}

impl Bmux {
    fn connect(target: ControlTarget) -> Result<Self> {
        let mut launch = ServerLaunch::discover()?;
        launch.current_directory = env::current_dir().context("determine current directory")?;
        connect_or_spawn(&target, &launch)?;
        Ok(Self { target })
    }

    fn sessions(&self) -> Result<Vec<brood_cli::control::SurfaceSessionSummary>> {
        list_surface_sessions(&self.target)
    }

    fn terminals(&self) -> Result<Vec<Terminal>> {
        let output = request_internal_command(
            &self.target,
            &["terminal".to_string(), "list-json".to_string()],
            false,
        )?;
        serde_json::from_str(&output).context("decode Brood terminal inventory")
    }

    fn current_session(&self) -> Result<u64> {
        if let Ok(marker) = env::var(BROOD_SESSION_ENV)
            && let Some(id) = session_marker_value(&marker, "surface-session-id")
        {
            return id.parse().context("decode entered Brood session id");
        }
        self.sessions()?
            .into_iter()
            .find(|session| session.active)
            .map(|session| session.id)
            .context("no Brood sessions")
    }

    fn resolve_session(&self, target: Option<&str>) -> Result<u64> {
        let Some(target) = target else {
            return self.current_session();
        };
        let target = target
            .split_once(':')
            .map_or(target, |(session, _)| session)
            .trim_start_matches('$')
            .trim_start_matches('=');
        let sessions = self.sessions()?;
        if let Ok(id) = target.parse::<u64>()
            && sessions.iter().any(|session| session.id == id)
        {
            return Ok(id);
        }
        if let Some(session) = sessions.iter().find(|session| session.name == target) {
            return Ok(session.id);
        }
        let matches = sessions
            .iter()
            .filter(|session| session.name.starts_with(target))
            .collect::<Vec<_>>();
        ensure!(matches.len() == 1, "can't find session: {target}");
        matches
            .first()
            .map(|session| session.id)
            .context("session match disappeared")
    }

    fn resolve_terminal(&self, target: Option<&str>) -> Result<Terminal> {
        let terminals = self.terminals()?;
        let current_session = self.current_session()?;
        let Some(target) = target else {
            return terminals
                .into_iter()
                .filter(|terminal| terminal.session == current_session)
                .max_by_key(|terminal| terminal.ordinal)
                .context("no terminal pane in current session");
        };
        if let Some(encoded) = target.strip_prefix('%') {
            let id: u64 = encoded.parse().context("invalid pane id")?;
            return terminals
                .into_iter()
                .find(|terminal| terminal.pane_id() == id)
                .with_context(|| format!("can't find pane: %{id}"));
        }
        let (session_target, pane_target) = target
            .rsplit_once('.')
            .map_or((None, target), |(session, pane)| (Some(session), pane));
        let session = match session_target {
            Some(session) => self.resolve_session(Some(session))?,
            None => current_session,
        };
        let index = pane_target
            .trim_start_matches(':')
            .parse::<u64>()
            .context("pane target must be an index or %pane-id")?;
        terminals
            .into_iter()
            .find(|terminal| terminal.session == session && terminal.pane_index() == index)
            .with_context(|| format!("can't find pane: {target}"))
    }

    fn activate(&self, session: u64) -> Result<()> {
        ensure!(
            activate_surface_session(&self.target, session)?,
            "can't find session: {session}"
        );
        Ok(())
    }

    fn present(&self) -> Result<()> {
        let output =
            request_internal_command(&self.target, &["focus-presentation".to_string()], false)?;
        if output.contains("focused=false") {
            drop(spawn_client(&self.target)?);
        }
        Ok(())
    }

    fn detach(&self) -> Result<()> {
        request_internal_command(&self.target, &["close-frontend".to_string()], false)?;
        Ok(())
    }

    fn create_session(&self, name: &str, directory: &Path) -> Result<u64> {
        ensure!(
            !self.sessions()?.iter().any(|session| session.name == name),
            "duplicate session: {name}"
        );
        let output = request_internal_command(
            &self.target,
            &[
                "surface-session".to_string(),
                "dir-new".to_string(),
                directory.display().to_string(),
                "keep-default".to_string(),
                name.to_string(),
            ],
            false,
        )?;
        parse_key_u64(&output, "active")
    }

    fn new_terminal(&self, session: u64, command: &str) -> Result<Terminal> {
        let before = self.terminals()?;
        request_internal_command(
            &self.target,
            &[
                "terminal".to_string(),
                "new".to_string(),
                session.to_string(),
                BASE64.encode(command),
            ],
            false,
        )?;
        wait_for_terminal(self, |terminal| {
            terminal.session == session && !before.contains(terminal)
        })
    }

    fn send(&self, terminal: &Terminal, bytes: &[u8]) -> Result<()> {
        request_internal_command(
            &self.target,
            &[
                "terminal".to_string(),
                "send".to_string(),
                terminal.session.to_string(),
                terminal.ordinal.to_string(),
                BASE64.encode(bytes),
            ],
            false,
        )?;
        Ok(())
    }

    fn focus(&self, terminal: &Terminal) -> Result<()> {
        self.activate(terminal.session)?;
        self.present()?;
        request_internal_command(
            &self.target,
            &[
                "terminal".to_string(),
                "focus".to_string(),
                terminal.session.to_string(),
                terminal.ordinal.to_string(),
            ],
            false,
        )?;
        Ok(())
    }

    fn stop(&self, terminal: &Terminal) -> Result<()> {
        request_internal_command(
            &self.target,
            &[
                "terminal".to_string(),
                "stop".to_string(),
                terminal.session.to_string(),
                terminal.ordinal.to_string(),
            ],
            false,
        )?;
        Ok(())
    }
}

fn main() -> ExitCode {
    match run(env::args_os().skip(1).collect()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("bmux: {error:#}");
            ExitCode::from(1)
        }
    }
}

fn run(args: Vec<OsString>) -> Result<()> {
    let (target, commands) = parse_global(args)?;
    if matches!(commands.as_slice(), [command] if command.as_slice() == ["-V"]) {
        println!("bmux {VERSION}");
        return Ok(());
    }
    if matches!(commands.as_slice(), [command] if matches!(command.as_slice(), [flag] if flag == "-h" || flag == "--help"))
    {
        print_help();
        return Ok(());
    }
    let bmux = Bmux::connect(target)?;
    for command in commands {
        execute(&bmux, &command)?;
    }
    Ok(())
}

fn print_help() {
    println!(
        "bmux {VERSION} - tmux-shaped Brood session control\n\
         usage: bmux [-L name|-S name] [command [flags]] [\\; command ...]\n\
         commands: new-session, attach-session, list-sessions, has-session,\n\
         kill-session, rename-session, switch-client, detach-client,\n\
         new-window, split-window, list-windows, select-window,\n\
         list-panes, select-pane, kill-pane, send-keys, display-message, info"
    );
}

fn parse_global(args: Vec<OsString>) -> Result<(ControlTarget, Vec<Vec<String>>)> {
    let mut args = args
        .into_iter()
        .map(|arg| {
            arg.into_string()
                .map_err(|_argument| anyhow::anyhow!("arguments must be UTF-8"))
        })
        .collect::<Result<Vec<_>>>()?;
    if args.iter().any(|arg| arg == "-V") {
        return Ok((
            ControlTarget::new(String::new(), None),
            vec![vec!["-V".to_string()]],
        ));
    }
    let mut destination = env::var(BROODD_DBUS_DESTINATION_ENV)
        .unwrap_or_else(|_| DEFAULT_BROOD_DBUS_NAME.to_string());
    let address = env::var(BROODD_DBUS_ADDRESS_ENV).ok();
    let cursor = 0;
    while cursor < args.len() {
        match args.get(cursor).map(String::as_str) {
            Some("-L" | "-S") => {
                let value = args.get(cursor + 1).context("-L/-S requires a value")?;
                destination = bmux_destination(value);
                args.drain(cursor..=cursor + 1);
            }
            Some("-f" | "-N") => {
                ensure!(args.get(cursor + 1).is_some(), "option requires a value");
                args.drain(cursor..=cursor + 1);
            }
            Some("-v" | "-vv" | "-C") => {
                args.remove(cursor);
            }
            _ => break,
        }
    }
    let mut commands = Vec::new();
    let mut current = Vec::new();
    for arg in args {
        if arg == ";" {
            if !current.is_empty() {
                commands.push(std::mem::take(&mut current));
            }
        } else {
            current.push(arg);
        }
    }
    if !current.is_empty() {
        commands.push(current);
    }
    if commands.is_empty() {
        commands.push(vec!["new-session".to_string()]);
    }
    Ok((ControlTarget::new(destination, address), commands))
}

fn execute(bmux: &Bmux, args: &[String]) -> Result<()> {
    let (command, rest) = args.split_first().context("missing command")?;
    match command.as_str() {
        "new" | "new-session" => new_session(bmux, rest),
        "a" | "at" | "attach" | "attach-session" => attach_session(bmux, rest),
        "ls" | "list-sessions" => list_sessions(bmux, rest),
        "has" | "has-session" => has_session(bmux, rest),
        "kill-session" | "kill-window" => kill_session(bmux, rest),
        "kill-server" => kill_server(bmux),
        "rename" | "rename-session" => rename_session(bmux, rest),
        "switch" | "switch-client" => switch_client(bmux, rest),
        "detach" | "detach-client" => bmux.detach(),
        "lsp" | "list-panes" => list_panes(bmux, rest),
        "splitw" | "split-window" | "neww" | "new-window" => new_pane(bmux, rest),
        "selectp" | "select-pane" => select_pane(bmux, rest),
        "killp" | "kill-pane" => kill_pane(bmux, rest),
        "send" | "send-keys" => send_keys(bmux, rest),
        "lsw" | "list-windows" => list_windows(bmux, rest),
        "selectw" | "select-window" | "last-window" | "next-window" | "previous-window" => {
            select_window(bmux, rest)
        }
        "display" | "display-message" => display_message(bmux, rest),
        "start" | "start-server" => Ok(()),
        "info" => info(bmux),
        unsupported => bail!(
            "unsupported tmux command {unsupported:?}; bmux supports session, window, pane, target, format, and send-keys workflows"
        ),
    }
}

fn new_session(bmux: &Bmux, args: &[String]) -> Result<()> {
    let mut detached = false;
    let mut attach_if_exists = false;
    let mut print = false;
    let mut name = None;
    let mut directory = env::current_dir().context("determine current directory")?;
    let mut format = None;
    let mut command = Vec::new();
    let mut cursor = 0;
    while cursor < args.len() {
        match args.get(cursor).map(String::as_str) {
            Some("-d") => detached = true,
            Some("-A") => attach_if_exists = true,
            Some("-P") => print = true,
            Some("-s") => {
                cursor += 1;
                name = Some(value(args, cursor, "-s")?.to_string());
            }
            Some("-c") => {
                cursor += 1;
                directory = PathBuf::from(value(args, cursor, "-c")?);
            }
            Some("-F") => {
                cursor += 1;
                format = Some(value(args, cursor, "-F")?.to_string());
            }
            Some("-x" | "-y") => cursor += 1,
            Some(option) if option.starts_with('-') => bail!("unknown new-session option {option}"),
            Some(_) => {
                command.extend_from_slice(
                    args.get(cursor..)
                        .context("command cursor exceeded arguments")?,
                );
                break;
            }
            None => break,
        }
        cursor += 1;
    }
    let sessions = bmux.sessions()?;
    let name = if let Some(name) = name {
        name
    } else {
        (0_u64..=u64::MAX)
            .map(|candidate| candidate.to_string())
            .find(|candidate| !sessions.iter().any(|session| session.name == *candidate))
            .context("no numeric session name is available")?
    };
    let existing = sessions.into_iter().find(|session| session.name == name);
    let session = if let Some(existing) = existing {
        ensure!(attach_if_exists, "duplicate session: {name}");
        existing.id
    } else {
        bmux.create_session(&name, &directory)?
    };
    if !command.is_empty() {
        let terminal = wait_for_terminal(bmux, |terminal| {
            terminal.session == session && terminal.ordinal == 1
        })?;
        let mut line = shell_join(&command);
        line.push('\n');
        bmux.send(&terminal, line.as_bytes())?;
    }
    if !detached {
        bmux.activate(session)?;
        bmux.present()?;
    }
    if print {
        let terminal = wait_for_terminal(bmux, |terminal| {
            terminal.session == session && terminal.ordinal == 1
        })?;
        println!(
            "{}",
            render_session_format(
                format.as_deref().unwrap_or("#{session_name}:"),
                &session_summary(bmux, session)?,
                Some(&terminal),
            )
        );
    }
    Ok(())
}

fn attach_session(bmux: &Bmux, args: &[String]) -> Result<()> {
    let target = option_value(args, "-t");
    let session = bmux.resolve_session(target)?;
    bmux.activate(session)?;
    bmux.present()
}

fn list_sessions(bmux: &Bmux, args: &[String]) -> Result<()> {
    let format = option_value(args, "-F");
    let terminals = bmux.terminals()?;
    for session in bmux.sessions()? {
        let count = terminals
            .iter()
            .filter(|terminal| terminal.session == session.id)
            .count();
        if let Some(format) = format {
            println!(
                "{}",
                render_session_format(
                    format,
                    &session,
                    terminals.iter().find(|t| t.session == session.id)
                )
            );
        } else {
            println!(
                "{}: 1 windows (created by Brood){} ({} panes)",
                session.name,
                if session.active { " (attached)" } else { "" },
                count
            );
        }
    }
    Ok(())
}

fn has_session(bmux: &Bmux, args: &[String]) -> Result<()> {
    bmux.resolve_session(option_value(args, "-t"))?;
    Ok(())
}

fn kill_session(bmux: &Bmux, args: &[String]) -> Result<()> {
    let target = option_value(args, "-t").or_else(|| {
        args.iter()
            .find(|arg| !arg.starts_with('-'))
            .map(String::as_str)
    });
    let session = bmux.resolve_session(target)?;
    if args.iter().any(|arg| arg == "-a") {
        for candidate in bmux.sessions()? {
            if candidate.id != session {
                close_surface_session(&bmux.target, candidate.id)?;
            }
        }
        return Ok(());
    }
    ensure!(
        close_surface_session(&bmux.target, session)?,
        "can't find session: {session}"
    );
    Ok(())
}

fn kill_server(bmux: &Bmux) -> Result<()> {
    for session in bmux.sessions()? {
        close_surface_session(&bmux.target, session.id)?;
    }
    Ok(())
}

fn rename_session(bmux: &Bmux, args: &[String]) -> Result<()> {
    let target = option_value(args, "-t");
    let name = args.last().context("rename-session requires a new name")?;
    let session = bmux.resolve_session(target)?;
    ensure!(
        rename_surface_session(&bmux.target, session, name.clone())?,
        "can't rename session"
    );
    Ok(())
}

fn switch_client(bmux: &Bmux, args: &[String]) -> Result<()> {
    attach_session(bmux, args)
}

fn list_panes(bmux: &Bmux, args: &[String]) -> Result<()> {
    let format = option_value(args, "-F");
    let session = (!args.iter().any(|arg| arg == "-a"))
        .then(|| bmux.resolve_session(option_value(args, "-t")))
        .transpose()?;
    let mut terminals = bmux
        .terminals()?
        .into_iter()
        .filter(|terminal| session.is_none_or(|session| terminal.session == session))
        .collect::<Vec<_>>();
    terminals.sort_by_key(|terminal| terminal.ordinal);
    for terminal in terminals {
        if let Some(format) = format {
            println!(
                "{}",
                render_pane_format(format, &terminal, terminal.ordinal == 1)
            );
        } else {
            println!(
                "{}: [Brood terminal]{}",
                terminal.pane_index(),
                if terminal.ordinal == 1 {
                    " (active)"
                } else {
                    ""
                }
            );
        }
    }
    Ok(())
}

fn new_pane(bmux: &Bmux, args: &[String]) -> Result<()> {
    let target = option_value(args, "-t");
    let session = bmux.resolve_session(target)?;
    let detached = args.iter().any(|arg| arg == "-d");
    let print = args.iter().any(|arg| arg == "-P");
    let format = option_value(args, "-F");
    let command = positional_after_options(args, &["-t", "-c", "-F", "-l", "-p"])?;
    let terminal = bmux.new_terminal(session, &shell_join(&command))?;
    if !detached {
        bmux.focus(&terminal)?;
    }
    if print {
        println!(
            "{}",
            render_pane_format(format.unwrap_or("#{pane_id}"), &terminal, true)
        );
    }
    Ok(())
}

fn select_pane(bmux: &Bmux, args: &[String]) -> Result<()> {
    let terminal = bmux.resolve_terminal(option_value(args, "-t"))?;
    bmux.focus(&terminal)
}

fn kill_pane(bmux: &Bmux, args: &[String]) -> Result<()> {
    let terminal = bmux.resolve_terminal(option_value(args, "-t"))?;
    bmux.stop(&terminal)
}

fn send_keys(bmux: &Bmux, args: &[String]) -> Result<()> {
    let target = option_value(args, "-t");
    let literal = args.iter().any(|arg| arg == "-l");
    let hex = args.iter().any(|arg| arg == "-H");
    let keys = positional_after_options(args, &["-t", "-N"])?;
    ensure!(!keys.is_empty(), "send-keys requires keys");
    let bytes = encode_keys(&keys, literal, hex)?;
    let terminal = bmux.resolve_terminal(target)?;
    bmux.send(&terminal, &bytes)
}

fn list_windows(bmux: &Bmux, args: &[String]) -> Result<()> {
    let terminals = bmux.terminals()?;
    let sessions = if args.iter().any(|arg| arg == "-a") {
        bmux.sessions()?
    } else {
        vec![session_summary(
            bmux,
            bmux.resolve_session(option_value(args, "-t"))?,
        )?]
    };
    for summary in sessions {
        let panes = terminals
            .iter()
            .filter(|terminal| terminal.session == summary.id)
            .count();
        if let Some(format) = option_value(args, "-F") {
            println!("{}", render_window_format(format, &summary, panes));
        } else if args.iter().any(|arg| arg == "-a") {
            println!("{}:0: workspace* ({panes} panes) [Brood]", summary.name);
        } else {
            println!("0: workspace* ({panes} panes) [Brood]");
        }
    }
    Ok(())
}

fn select_window(bmux: &Bmux, args: &[String]) -> Result<()> {
    let target = option_value(args, "-t");
    let session = bmux.resolve_session(target)?;
    bmux.activate(session)?;
    bmux.present()
}

fn display_message(bmux: &Bmux, args: &[String]) -> Result<()> {
    let print = args.iter().any(|arg| arg == "-p");
    let positional = positional_after_options(args, &["-t", "-c"])?;
    let format = positional.first().map_or("#{session_name}", String::as_str);
    let session = bmux.resolve_session(option_value(args, "-t"))?;
    let summary = session_summary(bmux, session)?;
    let terminal = bmux.resolve_terminal(option_value(args, "-t")).ok();
    let output = render_session_format(format, &summary, terminal.as_ref());
    if print || !output.is_empty() {
        println!("{output}");
    }
    Ok(())
}

fn info(bmux: &Bmux) -> Result<()> {
    println!("server: Brood ({})", bmux.target.destination);
    println!("sessions: {}", bmux.sessions()?.len());
    println!("panes: {}", bmux.terminals()?.len());
    println!("windows: one Brood workspace window per session");
    Ok(())
}

fn wait_for_terminal<F>(bmux: &Bmux, predicate: F) -> Result<Terminal>
where
    F: Fn(&Terminal) -> bool,
{
    let deadline = Instant::now() + TERMINAL_WAIT;
    loop {
        if let Some(terminal) = bmux.terminals()?.into_iter().find(&predicate) {
            return Ok(terminal);
        }
        ensure!(
            Instant::now() < deadline,
            "timed out waiting for Brood terminal actor"
        );
        thread::sleep(TERMINAL_POLL);
    }
}

fn session_summary(bmux: &Bmux, id: u64) -> Result<brood_cli::control::SurfaceSessionSummary> {
    bmux.sessions()?
        .into_iter()
        .find(|session| session.id == id)
        .context("session disappeared")
}

fn render_session_format(
    format: &str,
    session: &brood_cli::control::SurfaceSessionSummary,
    terminal: Option<&Terminal>,
) -> String {
    let mut output = format.to_string();
    for (key, value) in [
        ("session_name", session.name.clone()),
        ("session_id", format!("${}", session.id)),
        ("session_attached", u8::from(session.active).to_string()),
        ("session_windows", "1".to_string()),
        ("window_index", "0".to_string()),
        ("window_name", "workspace".to_string()),
    ] {
        output = output.replace(&format!("#{{{key}}}"), &value);
    }
    terminal.map_or_else(
        || output.clone(),
        |terminal| render_pane_format(&output, terminal, terminal.ordinal == 1),
    )
}

fn render_window_format(
    format: &str,
    session: &brood_cli::control::SurfaceSessionSummary,
    panes: usize,
) -> String {
    render_session_format(format, session, None)
        .replace("#{window_id}", &format!("@{}", session.id))
        .replace("#{window_panes}", &panes.to_string())
        .replace("#{window_active}", &u8::from(session.active).to_string())
}

fn render_pane_format(format: &str, terminal: &Terminal, active: bool) -> String {
    let mut output = format.to_string();
    for (key, value) in [
        ("pane_id", format!("%{}", terminal.pane_id())),
        ("pane_index", terminal.pane_index().to_string()),
        ("pane_active", u8::from(active).to_string()),
        ("pane_current_command", "sh".to_string()),
        ("pane_title", terminal.actor.clone()),
        ("pane_dead", "0".to_string()),
    ] {
        output = output.replace(&format!("#{{{key}}}"), &value);
    }
    output
}

fn encode_keys(keys: &[String], literal: bool, hex: bool) -> Result<Vec<u8>> {
    if literal {
        return Ok(keys.join(" ").into_bytes());
    }
    if hex {
        return keys
            .iter()
            .map(|key| {
                u8::from_str_radix(key, 16).with_context(|| format!("invalid hex byte {key}"))
            })
            .collect();
    }
    let mut bytes = Vec::new();
    for key in keys {
        if let Some(encoded) = named_key(key) {
            bytes.extend_from_slice(encoded);
        } else if let Some(character) = key.strip_prefix("C-") {
            let byte = character
                .as_bytes()
                .first()
                .copied()
                .filter(|_| character.len() == 1)
                .context("invalid control key")?;
            bytes.push(byte.to_ascii_uppercase() & 0x1f);
        } else {
            bytes.extend_from_slice(key.as_bytes());
        }
    }
    Ok(bytes)
}

fn named_key(key: &str) -> Option<&'static [u8]> {
    match key {
        "Enter" | "C-m" => Some(b"\r"),
        "Escape" | "Esc" | "C-[" => Some(b"\x1b"),
        "Tab" | "C-i" => Some(b"\t"),
        "Space" => Some(b" "),
        "BSpace" | "C-h" => Some(b"\x7f"),
        "Up" => Some(b"\x1b[A"),
        "Down" => Some(b"\x1b[B"),
        "Right" => Some(b"\x1b[C"),
        "Left" => Some(b"\x1b[D"),
        "Home" => Some(b"\x1b[H"),
        "End" => Some(b"\x1b[F"),
        "DC" | "Delete" => Some(b"\x1b[3~"),
        "IC" | "Insert" => Some(b"\x1b[2~"),
        "PageUp" | "PPage" => Some(b"\x1b[5~"),
        "PageDown" | "NPage" => Some(b"\x1b[6~"),
        "F1" => Some(b"\x1bOP"),
        "F2" => Some(b"\x1bOQ"),
        "F3" => Some(b"\x1bOR"),
        "F4" => Some(b"\x1bOS"),
        "F5" => Some(b"\x1b[15~"),
        "F6" => Some(b"\x1b[17~"),
        "F7" => Some(b"\x1b[18~"),
        "F8" => Some(b"\x1b[19~"),
        "F9" => Some(b"\x1b[20~"),
        "F10" => Some(b"\x1b[21~"),
        "F11" => Some(b"\x1b[23~"),
        "F12" => Some(b"\x1b[24~"),
        _ => None,
    }
}

fn positional_after_options(args: &[String], valued: &[&str]) -> Result<Vec<String>> {
    let mut output = Vec::new();
    let mut cursor = 0;
    while cursor < args.len() {
        let arg = args
            .get(cursor)
            .context("option cursor exceeded arguments")?;
        if valued.contains(&arg.as_str()) {
            cursor += 2;
            ensure!(cursor <= args.len(), "{arg} requires a value");
            continue;
        }
        if arg.starts_with('-') {
            cursor += 1;
            continue;
        }
        output.extend_from_slice(
            args.get(cursor..)
                .context("argument cursor exceeded arguments")?,
        );
        break;
    }
    Ok(output)
}

fn option_value<'a>(args: &'a [String], option: &str) -> Option<&'a str> {
    args.windows(2).find_map(|pair| match pair {
        [key, value] if key == option => Some(value.as_str()),
        _ => None,
    })
}

fn value<'a>(args: &'a [String], cursor: usize, option: &str) -> Result<&'a str> {
    args.get(cursor)
        .map(String::as_str)
        .with_context(|| format!("{option} requires a value"))
}

fn parse_key_u64(output: &str, key: &str) -> Result<u64> {
    output
        .split_whitespace()
        .find_map(|field| field.strip_prefix(&format!("{key}=")))
        .context("Brood response omitted session id")?
        .parse()
        .context("decode Brood session id")
}

fn shell_join(argv: &[String]) -> String {
    argv.iter()
        .map(|arg| {
            if arg
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_+-./:=,@%".contains(&byte))
            {
                arg.clone()
            } else {
                format!("'{}'", arg.replace('\'', "'\\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn bmux_destination(name: &str) -> String {
    let mut encoded = String::new();
    for byte in name.bytes() {
        if byte.is_ascii_alphanumeric() || byte == b'_' {
            encoded.push(char::from(byte));
        } else {
            encoded.push('_');
            encoded.push(char::from_digit(u32::from(byte >> 4), 16).unwrap_or('0'));
            encoded.push(char::from_digit(u32::from(byte & 0x0f), 16).unwrap_or('0'));
        }
    }
    if encoded.is_empty() || encoded.as_bytes().first().is_some_and(u8::is_ascii_digit) {
        encoded.insert(0, 's');
    }
    format!("org.brood.Session.bmux.{encoded}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pane_ids_are_stable_across_sessions() {
        let first = Terminal {
            session: 1,
            ordinal: 2,
            actor: String::new(),
        };
        let second = Terminal {
            session: 2,
            ordinal: 2,
            actor: String::new(),
        };
        assert_eq!(first.pane_index(), 1);
        assert_ne!(first.pane_id(), second.pane_id());
    }

    #[test]
    #[allow(
        clippy::panic_in_result_fn,
        reason = "test assertions report all encoded byte mismatches"
    )]
    fn send_keys_covers_text_control_navigation_and_literal_modes() -> Result<()> {
        assert_eq!(
            encode_keys(&["C-c".to_string(), "Enter".to_string()], false, false)?,
            b"\x03\r"
        );
        assert_eq!(
            encode_keys(&["hello".to_string(), "world".to_string()], true, false)?,
            b"hello world"
        );
        assert_eq!(
            encode_keys(&["1b".to_string(), "5b".to_string()], false, true)?,
            b"\x1b["
        );
        Ok(())
    }

    #[test]
    fn format_expansion_uses_tmux_identifiers() {
        let terminal = Terminal {
            session: 7,
            ordinal: 3,
            actor: "terminal-session-3".to_string(),
        };
        assert_eq!(
            render_pane_format("#{pane_id}:#{pane_index}:#{pane_title}", &terminal, false),
            "%30064771075:2:terminal-session-3"
        );
    }

    #[test]
    fn formatted_session_print_includes_the_authoritative_initial_pane() {
        let session = brood_cli::control::SurfaceSessionSummary {
            id: 7,
            name: "work".to_string(),
            active: true,
            windows: 1,
        };
        let terminal = Terminal {
            session: 7,
            ordinal: 1,
            actor: "terminal-session".to_string(),
        };

        assert_eq!(
            render_session_format(
                "#{session_id}:#{session_name}:#{pane_id}",
                &session,
                Some(&terminal),
            ),
            "$7:work:%30064771073"
        );
    }

    #[test]
    fn socket_names_become_valid_isolated_dbus_destinations() {
        assert_eq!(
            bmux_destination("test-1"),
            "org.brood.Session.bmux.test_2d1"
        );
        assert_eq!(bmux_destination("2"), "org.brood.Session.bmux.s2");
    }
}
