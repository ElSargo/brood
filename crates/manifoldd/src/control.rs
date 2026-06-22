use std::{
    fs,
    io::{ErrorKind, Read, Write},
    os::unix::net::{UnixListener, UnixStream},
    path::Path,
    process::{Command as ProcessCommand, Stdio},
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use smithay::{
    backend::input::{Axis, AxisSource, ButtonState, KeyState},
    input::pointer::AxisFrame,
    reexports::calloop::{EventLoop, Interest, Mode, PostAction, generic::Generic},
    utils::Point,
};
use tracing::{info, warn};

use crate::{
    CtlArgs, SwitchArgs, browser,
    command::{SurfaceCommand, WorkspaceCommand},
    debug_dump::{self, DumpOptions},
    layout::LayoutAction,
    state::{
        FocusDirection, Manifold, PresentationStatus, SurfaceSessionId, SurfaceSessionSummary,
    },
};

const MAX_CONTROL_COMMAND_BYTES: usize = 64 * 1024;
const CONTROL_STREAM_TIMEOUT: Duration = Duration::from_millis(250);

pub fn listen(event_loop: &mut EventLoop<'static, Manifold>, path: &Path) -> Result<()> {
    if path.exists() {
        fs::remove_file(path).with_context(|| format!("remove stale socket {}", path.display()))?;
    }

    let listener = UnixListener::bind(path).with_context(|| format!("bind {}", path.display()))?;
    listener
        .set_nonblocking(true)
        .context("set control socket nonblocking")?;

    event_loop
        .handle()
        .insert_source(
            Generic::new(listener, Interest::READ, Mode::Level),
            |_, listener, state| {
                loop {
                    let accepted = listener.accept();
                    let Ok((mut stream, _addr)) = accepted else {
                        break;
                    };
                    if let Err(err) = stream.set_read_timeout(Some(CONTROL_STREAM_TIMEOUT)) {
                        warn!(?err, "failed to set control stream read timeout");
                    }
                    if let Err(err) = stream.set_write_timeout(Some(CONTROL_STREAM_TIMEOUT)) {
                        warn!(?err, "failed to set control stream write timeout");
                    }
                    if let Err(err) = handle_stream(&mut stream, state) {
                        warn!(?err, "control command failed");
                        let _ = writeln!(stream, "ERR {err}");
                    }
                }
                Ok(PostAction::Continue)
            },
        )
        .context("insert control listener")?;

    info!(path = %path.display(), "control socket listening");
    Ok(())
}

pub fn send(args: CtlArgs) -> Result<()> {
    let response = request_socket_command(&args.control_socket, &args.command, args.json)?;
    print!("{response}");
    Ok(())
}

pub fn switch(args: SwitchArgs) -> Result<()> {
    let list = request_surface_session_list(&args.control_socket)?;
    ensure!(!list.sessions.is_empty(), "no open sessions");
    let initial_query = joined_name(&args.query);

    let outcome = match args.session_id {
        Some(id) => {
            ensure!(
                list.sessions.iter().any(|session| session.id == id),
                "session {id} is not open"
            );
            SessionSwitchOutcome::Existing(id)
        }
        None => run_skim_session_picker(&args.skim, &list.sessions, initial_query.as_deref())?,
    };

    match outcome {
        SessionSwitchOutcome::Existing(selected) => {
            let response = request_socket_command(
                &args.control_socket,
                &[
                    "surface-session".to_string(),
                    "switch".to_string(),
                    selected.to_string(),
                ],
                false,
            )?;
            ensure_ok_response(&response)?;
        }
        SessionSwitchOutcome::Create(name) => {
            let response = request_socket_command(
                &args.control_socket,
                &["surface-session".to_string(), "new".to_string(), name],
                false,
            )?;
            ensure_ok_response(&response)?;
        }
    }

    if list.presentation == "Detached" {
        let response =
            request_socket_command(&args.control_socket, &["attach".to_string()], false)?;
        ensure_ok_response(&response)?;
    }

    Ok(())
}

pub fn request_socket_command(
    control_socket: &Path,
    command: &[String],
    json: bool,
) -> Result<String> {
    let mut stream = UnixStream::connect(control_socket)
        .with_context(|| format!("connect {}", control_socket.display()))?;
    let encoded = if json {
        command.join(" ")
    } else {
        serde_json::to_string(command).context("encode control command")?
    };
    if json {
        let _: Vec<String> =
            serde_json::from_str(&encoded).context("validate JSON control argv")?;
    }
    writeln!(stream, "{encoded}")?;

    let mut response = String::new();
    stream.read_to_string(&mut response)?;
    Ok(response)
}

pub fn execute_json_command(input: &str, state: &mut Manifold) -> Result<String> {
    let tokens = parse_command_tokens(input.trim())?;
    execute_tokens(&tokens, state)
}

pub fn status_json_string(state: &mut Manifold) -> Result<String> {
    let snapshot = StatusSnapshot::from_state(state);
    serde_json::to_string(&snapshot.to_json()).context("serialize status snapshot")
}

fn handle_stream(stream: &mut UnixStream, state: &mut Manifold) -> Result<()> {
    let input = read_command_line(stream)?;
    let response = execute_json_command(&input, state)?;
    write!(stream, "{response}")?;

    if let Err(err) = state.display_handle.flush_clients() {
        warn!(?err, "flush clients after control command failed");
    }

    Ok(())
}

fn execute_tokens(tokens: &[String], state: &mut Manifold) -> Result<String> {
    let Some((command, args)) = tokens.split_first() else {
        bail!("empty control command");
    };

    let response = match command.as_str() {
        "attach" if args.is_empty() => {
            state.request_attach();
            "OK attach requested\n".to_string()
        }
        "detach" if args.is_empty() => {
            state.request_detach();
            "OK detach requested\n".to_string()
        }
        "close-frontend" | "frontend-close" if args.is_empty() => {
            state.request_detach();
            "OK frontend close requested\n".to_string()
        }
        "status" if args.is_empty() => {
            let snapshot = StatusSnapshot::from_state(state);
            format!(
                "OK wayland_display={} wayland_client_insert_errors={} wayland_dispatch_errors={} xdg_configure_errors={} presentation={} presentation_attach_failures={} presentation_runtime_failures={} presentation_last_error={} windows={} total_windows={} active_surface_session={} surface_sessions={} focused_windows={} focused_window_index={} focused_surface_id={} input_mode={} managed_fullscreen={} requested_fullscreen={} focused_fullscreen={} layout_regions={} layout_placeholders={} layout_active_kind={} layout_active_rect={} browsers={} frames={} client_frame_callbacks={} commits={} pending_commits={} commit_frame_count={} last_commit_batch_commits={} max_commit_batch_commits={} last_commit_to_frame_us={} max_commit_to_frame_us={} last_commit_to_submit_us={} max_commit_to_submit_us={} client_frame_callback_commit_count={} last_client_frame_callback_batch_callbacks={} max_client_frame_callback_batch_callbacks={} last_client_frame_callback_to_commit_us={} max_client_frame_callback_to_commit_us={} client_frame_deferrals={} client_deferred_frames={} last_frame_us={} max_frame_us={} last_frame_interval_us={} max_frame_interval_us={} input_frame_deferrals={} input_deferred_frames={} input_events={} keyboard_events={} pointer_motion_events={} pointer_button_events={} pointer_axis_events={} pointer_target_hits={} pointer_target_misses={} focus_clears={} focus_repairs={} window_lifecycle_refreshes={} window_lifecycle_focus_clears={} window_lifecycle_focus_repairs={} pending_input_events={} input_frame_count={} last_input_batch_events={} max_input_batch_events={} last_input_to_frame_us={} max_input_to_frame_us={} last_input_to_submit_us={} max_input_to_submit_us={} input_commit_frame_count={} input_without_commit_frame_count={} last_input_to_commit_us={} max_input_to_commit_us={} last_input_commit_to_submit_us={} max_input_commit_to_submit_us={} attach_count={} detach_count={} last_attach_latency_us={} max_attach_latency_us={} last_attach_to_frame_us={} max_attach_to_frame_us={} last_detach_latency_us={} max_detach_latency_us={} shm_formats={} egl_wl_bind={} dmabuf={} dmabuf_formats={} dmabuf_disabled={} dmabuf_attempts={} dmabuf_accepted={} dmabuf_rejected={} dmabuf_last_import={} dmabuf_last_error={} dmabuf_explicit_sync_release_points={} dmabuf_explicit_sync_blockers={} dmabuf_implicit_blockers={} dmabuf_blocker_failures={} dmabuf_last_blocker_error={} render_node={} explicit_sync={} explicit_sync_disabled={}",
                snapshot.wayland_display,
                snapshot.wayland_client_insert_errors,
                snapshot.wayland_dispatch_errors,
                snapshot.xdg_configure_errors,
                snapshot.presentation,
                snapshot.presentation_attach_failures,
                snapshot.presentation_runtime_failures,
                snapshot.presentation_last_error,
                snapshot.windows,
                snapshot.total_windows,
                snapshot.active_surface_session,
                snapshot.surface_sessions,
                snapshot.focused_windows,
                snapshot.focused_window_index_text,
                snapshot.focused_surface_id,
                snapshot.input_mode,
                snapshot.managed_fullscreen,
                snapshot.requested_fullscreen,
                snapshot.focused_fullscreen,
                snapshot.layout_regions,
                snapshot.layout_placeholders,
                snapshot.layout_active_kind,
                snapshot.layout_active_rect,
                snapshot.browsers,
                snapshot.frames,
                snapshot.client_frame_callbacks,
                snapshot.commits,
                snapshot.pending_commits,
                snapshot.commit_frame_count,
                snapshot.last_commit_batch_commits,
                snapshot.max_commit_batch_commits,
                snapshot.last_commit_to_frame_us_text,
                snapshot.max_commit_to_frame_us,
                snapshot.last_commit_to_submit_us_text,
                snapshot.max_commit_to_submit_us,
                snapshot.client_frame_callback_commit_count,
                snapshot.last_client_frame_callback_batch_callbacks,
                snapshot.max_client_frame_callback_batch_callbacks,
                snapshot.last_client_frame_callback_to_commit_us_text,
                snapshot.max_client_frame_callback_to_commit_us,
                snapshot.client_frame_deferrals,
                snapshot.client_deferred_frames,
                snapshot.last_frame_us,
                snapshot.max_frame_us,
                snapshot.last_frame_interval_us,
                snapshot.max_frame_interval_us,
                snapshot.input_frame_deferrals,
                snapshot.input_deferred_frames,
                snapshot.input_events,
                snapshot.keyboard_events,
                snapshot.pointer_motion_events,
                snapshot.pointer_button_events,
                snapshot.pointer_axis_events,
                snapshot.pointer_target_hits,
                snapshot.pointer_target_misses,
                snapshot.focus_clears,
                snapshot.focus_repairs,
                snapshot.window_lifecycle_refreshes,
                snapshot.window_lifecycle_focus_clears,
                snapshot.window_lifecycle_focus_repairs,
                snapshot.pending_input_events,
                snapshot.input_frame_count,
                snapshot.last_input_batch_events,
                snapshot.max_input_batch_events,
                snapshot.last_input_to_frame_us_text,
                snapshot.max_input_to_frame_us,
                snapshot.last_input_to_submit_us_text,
                snapshot.max_input_to_submit_us,
                snapshot.input_commit_frame_count,
                snapshot.input_without_commit_frame_count,
                snapshot.last_input_to_commit_us_text,
                snapshot.max_input_to_commit_us,
                snapshot.last_input_commit_to_submit_us_text,
                snapshot.max_input_commit_to_submit_us,
                snapshot.attach_count,
                snapshot.detach_count,
                snapshot.last_attach_latency_us_text,
                snapshot.max_attach_latency_us,
                snapshot.last_attach_to_frame_us_text,
                snapshot.max_attach_to_frame_us,
                snapshot.last_detach_latency_us_text,
                snapshot.max_detach_latency_us,
                snapshot.shm_formats,
                snapshot.egl_wl_bind,
                snapshot.dmabuf,
                snapshot.dmabuf_formats,
                snapshot.dmabuf_disabled,
                snapshot.dmabuf_attempts,
                snapshot.dmabuf_accepted,
                snapshot.dmabuf_rejected,
                snapshot.dmabuf_last_import,
                snapshot.dmabuf_last_error,
                snapshot.dmabuf_explicit_sync_release_points,
                snapshot.dmabuf_explicit_sync_blockers,
                snapshot.dmabuf_implicit_blockers,
                snapshot.dmabuf_blocker_failures,
                snapshot.dmabuf_last_blocker_error,
                snapshot.render_node,
                snapshot.explicit_sync,
                snapshot.explicit_sync_disabled
            ) + "\n"
        }
        "status-json" if args.is_empty() => {
            format!("{}\n", status_json_string(state)?)
        }
        "quit" if args.is_empty() => {
            let closed_browsers = state.browsers.close_all().len();
            state.loop_signal.stop();
            format!("OK quit requested browsers_closed={closed_browsers}") + "\n"
        }
        "spawn" => {
            if args.is_empty() {
                bail!("spawn requires a command");
            }
            state.spawn_hosted_app_argv(args)?;
            format!("OK spawned {}\n", args.join(" "))
        }
        "focus" => {
            let direction = match args {
                [arg] if arg == "next" => FocusDirection::Next,
                [arg] if arg == "prev" || arg == "previous" => FocusDirection::Previous,
                _ => bail!("focus requires next or prev"),
            };
            let changed = state.cycle_focus(direction);
            format!("OK focus changed={changed}\n")
        }
        "layout-action" | "layout" => {
            let [action] = args else {
                bail!("layout-action requires exactly one action name");
            };
            let action = action
                .parse::<LayoutAction>()
                .with_context(|| format!("parse layout action {action:?}"))?;
            let changed = state.apply_layout_action(action);
            format!("OK layout-action action={action} changed={changed}\n")
        }
        "surface-session" => match args {
            [subcommand] if subcommand == "list-json" => {
                let response = SurfaceSessionList::from_state(state);
                format!("{}\n", serde_json::to_string(&response)?)
            }
            [subcommand] if subcommand == "next" => {
                let changed =
                    WorkspaceCommand::Surface(SurfaceCommand::SwitchActiveSessionNext).apply(state);
                format!(
                    "OK surface-session active={} changed={changed}\n",
                    state.active_surface_session().raw()
                )
            }
            [subcommand, name @ ..] if subcommand == "new" => {
                let name = joined_name(name);
                if let Some(name) = name {
                    state.create_and_attach_named_surface_session(Some(name));
                } else {
                    WorkspaceCommand::Surface(SurfaceCommand::CreateAndAttachSession).apply(state);
                }
                format!(
                    "OK surface-session active={} created=true\n",
                    state.active_surface_session().raw()
                )
            }
            [subcommand, id] if subcommand == "switch" => {
                let id = parse_u64(id, "session id")?;
                let changed = state.switch_to_surface_session(SurfaceSessionId::from_raw(id));
                ensure!(
                    changed || state.active_surface_session().raw() == id,
                    "unknown surface session {id}"
                );
                format!(
                    "OK surface-session active={} changed={changed}\n",
                    state.active_surface_session().raw()
                )
            }
            _ => bail!("usage: surface-session list-json|next|new|switch <id>"),
        },
        "browser" => browser::handle_control(args, state)?,
        "harness" => match args {
            [subcommand, name] if subcommand == "signal" => {
                state.signal_test_harness(name.clone())?;
                format!("OK harness signal {}\n", name)
            }
            _ => bail!("usage: harness signal <name>"),
        },
        "nested-inner" => {
            let socket = state
                .nested_inner_control_socket
                .clone()
                .context("nested inner control socket not configured")?;
            if args.is_empty() {
                bail!("usage: nested-inner <command...>");
            }
            request_socket_command(&socket, args, false)?
        }
        "debug" => match args {
            [subcommand, dir] if subcommand == "dump" => {
                let manifest =
                    debug_dump::dump_bundle(state, Path::new(dir), DumpOptions::parse(None)?)?;
                format!("OK debug dump {}\n", manifest)
            }
            [subcommand, dir, selectors] if subcommand == "dump" => {
                let manifest = debug_dump::dump_bundle(
                    state,
                    Path::new(dir),
                    DumpOptions::parse(Some(selectors))?,
                )?;
                format!("OK debug dump {}\n", manifest)
            }
            _ => bail!("usage: debug dump <dir> [state,composed,surfaces]"),
        },
        "inject" => {
            handle_inject(args, state)?;
            format!("OK injected {}\n", args.join(" "))
        }
        _ => bail!("unknown command: {}", tokens.join(" ")),
    };

    Ok(response)
}

#[derive(serde::Serialize)]
struct StatusSnapshot {
    wayland_display: String,
    wayland_client_insert_errors: u64,
    wayland_dispatch_errors: u64,
    xdg_configure_errors: u64,
    presentation: &'static str,
    presentation_attach_failures: u64,
    presentation_runtime_failures: u64,
    presentation_last_error: String,
    windows: usize,
    total_windows: usize,
    active_surface_session: u64,
    surface_sessions: usize,
    focused_windows: usize,
    focused_window_index: Option<usize>,
    #[serde(skip)]
    focused_window_index_text: String,
    focused_surface_id: String,
    input_mode: &'static str,
    managed_fullscreen: bool,
    requested_fullscreen: usize,
    focused_fullscreen: bool,
    layout_regions: usize,
    layout_placeholders: usize,
    layout_active_kind: &'static str,
    layout_active_rect: String,
    browsers: usize,
    frames: u64,
    client_frame_callbacks: u64,
    commits: u64,
    pending_commits: u64,
    commit_frame_count: u64,
    last_commit_batch_commits: u64,
    max_commit_batch_commits: u64,
    last_commit_to_frame_us: Option<u128>,
    #[serde(skip)]
    last_commit_to_frame_us_text: String,
    max_commit_to_frame_us: u128,
    last_commit_to_submit_us: Option<u128>,
    #[serde(skip)]
    last_commit_to_submit_us_text: String,
    max_commit_to_submit_us: u128,
    client_frame_callback_commit_count: u64,
    last_client_frame_callback_batch_callbacks: u64,
    max_client_frame_callback_batch_callbacks: u64,
    last_client_frame_callback_to_commit_us: Option<u128>,
    #[serde(skip)]
    last_client_frame_callback_to_commit_us_text: String,
    max_client_frame_callback_to_commit_us: u128,
    client_frame_deferrals: u64,
    client_deferred_frames: u64,
    last_frame_us: u128,
    max_frame_us: u128,
    last_frame_interval_us: u128,
    max_frame_interval_us: u128,
    input_frame_deferrals: u64,
    input_deferred_frames: u64,
    input_events: u64,
    keyboard_events: u64,
    pointer_motion_events: u64,
    pointer_button_events: u64,
    pointer_axis_events: u64,
    pointer_target_hits: u64,
    pointer_target_misses: u64,
    focus_clears: u64,
    focus_repairs: u64,
    window_lifecycle_refreshes: u64,
    window_lifecycle_focus_clears: u64,
    window_lifecycle_focus_repairs: u64,
    pending_input_events: u64,
    input_frame_count: u64,
    last_input_batch_events: u64,
    max_input_batch_events: u64,
    last_input_to_frame_us: Option<u128>,
    #[serde(skip)]
    last_input_to_frame_us_text: String,
    max_input_to_frame_us: u128,
    last_input_to_submit_us: Option<u128>,
    #[serde(skip)]
    last_input_to_submit_us_text: String,
    max_input_to_submit_us: u128,
    input_commit_frame_count: u64,
    input_without_commit_frame_count: u64,
    last_input_to_commit_us: Option<u128>,
    #[serde(skip)]
    last_input_to_commit_us_text: String,
    max_input_to_commit_us: u128,
    last_input_commit_to_submit_us: Option<u128>,
    #[serde(skip)]
    last_input_commit_to_submit_us_text: String,
    max_input_commit_to_submit_us: u128,
    attach_count: u64,
    detach_count: u64,
    last_attach_latency_us: Option<u128>,
    #[serde(skip)]
    last_attach_latency_us_text: String,
    max_attach_latency_us: u128,
    last_attach_to_frame_us: Option<u128>,
    #[serde(skip)]
    last_attach_to_frame_us_text: String,
    max_attach_to_frame_us: u128,
    last_detach_latency_us: Option<u128>,
    #[serde(skip)]
    last_detach_latency_us_text: String,
    max_detach_latency_us: u128,
    shm_formats: usize,
    egl_wl_bind: String,
    dmabuf: String,
    dmabuf_formats: usize,
    dmabuf_disabled: &'static str,
    dmabuf_attempts: u64,
    dmabuf_accepted: u64,
    dmabuf_rejected: u64,
    dmabuf_last_import: String,
    dmabuf_last_error: String,
    dmabuf_explicit_sync_release_points: u64,
    dmabuf_explicit_sync_blockers: u64,
    dmabuf_implicit_blockers: u64,
    dmabuf_blocker_failures: u64,
    dmabuf_last_blocker_error: String,
    render_node: String,
    explicit_sync: String,
    explicit_sync_disabled: &'static str,
}

impl StatusSnapshot {
    fn from_state(state: &mut Manifold) -> Self {
        state.refresh_window_state();

        let last_input_to_frame_us = state.stats.last_input_to_frame_micros;
        let last_commit_to_frame_us = state.stats.last_commit_to_frame_micros;
        let last_input_to_submit_us = state.stats.last_input_to_submit_micros;
        let last_commit_to_submit_us = state.stats.last_commit_to_submit_micros;
        let last_input_to_commit_us = state.stats.last_input_to_commit_micros;
        let last_input_commit_to_submit_us = state.stats.last_input_commit_to_submit_micros;
        let last_client_frame_callback_to_commit_us =
            state.stats.last_client_frame_callback_to_commit_micros;
        let last_attach_latency_us = state.stats.last_attach_latency_micros;
        let last_attach_to_frame_us = state.stats.last_attach_to_frame_micros;
        let last_detach_latency_us = state.stats.last_detach_latency_micros;
        let presentation = match state.presentation {
            crate::state::PresentationStatus::Attached => "Attached",
            crate::state::PresentationStatus::Detached => "Detached",
        };
        let dmabuf = state
            .dmabuf
            .protocol_version
            .map_or_else(|| "none".to_string(), |version| format!("v{version}"));
        let explicit_sync = state
            .dmabuf
            .explicit_sync_protocol_version
            .map_or_else(|| "none".to_string(), |version| format!("v{version}"));
        let layout_active_rect = state.active_layout_rect().map_or_else(
            || "none".to_string(),
            |rect| format!("{},{},{},{}", rect.x, rect.y, rect.w, rect.h),
        );

        Self {
            wayland_display: state.socket_name.to_string_lossy().into_owned(),
            wayland_client_insert_errors: state.stats.wayland_client_insert_errors,
            wayland_dispatch_errors: state.stats.wayland_dispatch_errors,
            xdg_configure_errors: state.stats.xdg_configure_errors,
            presentation,
            presentation_attach_failures: state.presentation_attach_failures,
            presentation_runtime_failures: state.presentation_runtime_failures,
            presentation_last_error: state
                .presentation_last_error
                .as_deref()
                .unwrap_or("none")
                .to_string(),
            windows: state.active_surface_session_window_count(),
            total_windows: state.space.elements().count(),
            active_surface_session: state.active_surface_session().raw(),
            surface_sessions: state.surface_session_count(),
            focused_windows: state.focused_window_count(),
            focused_window_index: state.focused_window_index(),
            focused_window_index_text: state
                .focused_window_index()
                .map_or_else(|| "none".to_string(), |index| index.to_string()),
            focused_surface_id: state
                .focused_surface_id()
                .unwrap_or_else(|| "none".to_string()),
            input_mode: match state.input_mode {
                crate::state::InputMode::Passthrough => "passthrough",
                crate::state::InputMode::Normal => "normal",
            },
            managed_fullscreen: state.managed_fullscreen(),
            requested_fullscreen: state.requested_fullscreen_count(),
            focused_fullscreen: state.focused_window_fullscreen(),
            layout_regions: state.layout.region_count(),
            layout_placeholders: state.layout.placeholder_count(),
            layout_active_kind: match state.layout.active_kind() {
                Some(crate::layout::LayoutLeafKind::Client) => "client",
                Some(crate::layout::LayoutLeafKind::Placeholder) => "placeholder",
                None => "none",
            },
            layout_active_rect,
            browsers: state.browsers.len(),
            frames: state.stats.frames_rendered,
            client_frame_callbacks: state.stats.client_frame_callbacks,
            commits: state.stats.commits,
            pending_commits: state.stats.pending_commits,
            commit_frame_count: state.stats.commit_frame_count,
            last_commit_batch_commits: state.stats.last_commit_batch_commits,
            max_commit_batch_commits: state.stats.max_commit_batch_commits,
            last_commit_to_frame_us,
            last_commit_to_frame_us_text: last_commit_to_frame_us
                .map_or_else(|| "none".to_string(), |micros| micros.to_string()),
            max_commit_to_frame_us: state.stats.max_commit_to_frame_micros,
            last_commit_to_submit_us,
            last_commit_to_submit_us_text: last_commit_to_submit_us
                .map_or_else(|| "none".to_string(), |micros| micros.to_string()),
            max_commit_to_submit_us: state.stats.max_commit_to_submit_micros,
            client_frame_callback_commit_count: state.stats.client_frame_callback_commit_count,
            last_client_frame_callback_batch_callbacks: state
                .stats
                .last_client_frame_callback_batch_callbacks,
            max_client_frame_callback_batch_callbacks: state
                .stats
                .max_client_frame_callback_batch_callbacks,
            last_client_frame_callback_to_commit_us,
            last_client_frame_callback_to_commit_us_text: last_client_frame_callback_to_commit_us
                .map_or_else(|| "none".to_string(), |micros| micros.to_string()),
            max_client_frame_callback_to_commit_us: state
                .stats
                .max_client_frame_callback_to_commit_micros,
            client_frame_deferrals: state.stats.client_frame_deferrals,
            client_deferred_frames: state.stats.client_deferred_frames,
            last_frame_us: state.stats.last_frame_micros,
            max_frame_us: state.stats.max_frame_micros,
            last_frame_interval_us: state.stats.last_frame_interval_micros,
            max_frame_interval_us: state.stats.max_frame_interval_micros,
            input_frame_deferrals: state.stats.input_frame_deferrals,
            input_deferred_frames: state.stats.input_deferred_frames,
            input_events: state.stats.input_events,
            keyboard_events: state.stats.keyboard_events,
            pointer_motion_events: state.stats.pointer_motion_events,
            pointer_button_events: state.stats.pointer_button_events,
            pointer_axis_events: state.stats.pointer_axis_events,
            pointer_target_hits: state.stats.pointer_target_hits,
            pointer_target_misses: state.stats.pointer_target_misses,
            focus_clears: state.stats.focus_clears,
            focus_repairs: state.stats.focus_repairs,
            window_lifecycle_refreshes: state.stats.window_lifecycle_refreshes,
            window_lifecycle_focus_clears: state.stats.window_lifecycle_focus_clears,
            window_lifecycle_focus_repairs: state.stats.window_lifecycle_focus_repairs,
            pending_input_events: state.stats.pending_input_events,
            input_frame_count: state.stats.input_frame_count,
            last_input_batch_events: state.stats.last_input_batch_events,
            max_input_batch_events: state.stats.max_input_batch_events,
            last_input_to_frame_us,
            last_input_to_frame_us_text: last_input_to_frame_us
                .map_or_else(|| "none".to_string(), |micros| micros.to_string()),
            max_input_to_frame_us: state.stats.max_input_to_frame_micros,
            last_input_to_submit_us,
            last_input_to_submit_us_text: last_input_to_submit_us
                .map_or_else(|| "none".to_string(), |micros| micros.to_string()),
            max_input_to_submit_us: state.stats.max_input_to_submit_micros,
            input_commit_frame_count: state.stats.input_commit_frame_count,
            input_without_commit_frame_count: state.stats.input_without_commit_frame_count,
            last_input_to_commit_us,
            last_input_to_commit_us_text: last_input_to_commit_us
                .map_or_else(|| "none".to_string(), |micros| micros.to_string()),
            max_input_to_commit_us: state.stats.max_input_to_commit_micros,
            last_input_commit_to_submit_us,
            last_input_commit_to_submit_us_text: last_input_commit_to_submit_us
                .map_or_else(|| "none".to_string(), |micros| micros.to_string()),
            max_input_commit_to_submit_us: state.stats.max_input_commit_to_submit_micros,
            attach_count: state.stats.attach_count,
            detach_count: state.stats.detach_count,
            last_attach_latency_us,
            last_attach_latency_us_text: last_attach_latency_us
                .map_or_else(|| "none".to_string(), |micros| micros.to_string()),
            max_attach_latency_us: state.stats.max_attach_latency_micros,
            last_attach_to_frame_us,
            last_attach_to_frame_us_text: last_attach_to_frame_us
                .map_or_else(|| "none".to_string(), |micros| micros.to_string()),
            max_attach_to_frame_us: state.stats.max_attach_to_frame_micros,
            last_detach_latency_us,
            last_detach_latency_us_text: last_detach_latency_us
                .map_or_else(|| "none".to_string(), |micros| micros.to_string()),
            max_detach_latency_us: state.stats.max_detach_latency_micros,
            shm_formats: state.dmabuf.shm_formats,
            egl_wl_bind: state.dmabuf.egl_wl_bind_status.clone(),
            dmabuf,
            dmabuf_formats: state.dmabuf.advertised_formats,
            dmabuf_disabled: state.dmabuf.disabled_reason.unwrap_or("none"),
            dmabuf_attempts: state.dmabuf.stats.attempts,
            dmabuf_accepted: state.dmabuf.stats.accepted,
            dmabuf_rejected: state.dmabuf.stats.rejected,
            dmabuf_last_import: state
                .dmabuf
                .stats
                .last_import
                .as_deref()
                .unwrap_or("none")
                .to_string(),
            dmabuf_last_error: state
                .dmabuf
                .stats
                .last_error
                .as_deref()
                .unwrap_or("none")
                .to_string(),
            dmabuf_explicit_sync_release_points: state.dmabuf.stats.explicit_sync_release_points,
            dmabuf_explicit_sync_blockers: state.dmabuf.stats.explicit_sync_blockers,
            dmabuf_implicit_blockers: state.dmabuf.stats.implicit_dmabuf_blockers,
            dmabuf_blocker_failures: state.dmabuf.stats.blocker_install_failures,
            dmabuf_last_blocker_error: state
                .dmabuf
                .stats
                .last_blocker_error
                .as_deref()
                .unwrap_or("none")
                .to_string(),
            render_node: state
                .dmabuf
                .render_node
                .as_deref()
                .unwrap_or("none")
                .to_string(),
            explicit_sync,
            explicit_sync_disabled: state.dmabuf.explicit_sync_disabled_reason,
        }
    }

    fn to_json(&self) -> serde_json::Value {
        serde_json::to_value(self).expect("serialize status snapshot")
    }
}

fn handle_inject(args: &[String], state: &mut Manifold) -> Result<()> {
    let Some((kind, rest)) = args.split_first() else {
        bail!("inject requires a kind: motion, button, key, or axis");
    };
    let time = monotonic_time_msec(state);

    match kind.as_str() {
        "motion" if rest.len() == 2 => {
            let x = parse_f64(&rest[0], "x")?;
            let y = parse_f64(&rest[1], "y")?;
            state.forward_pointer_motion(Point::from((x, y)), time);
        }
        "center-motion" if rest.is_empty() => {
            let Some(pos) = state.first_window_center() else {
                bail!("no mapped window to target");
            };
            state.forward_pointer_motion(pos, time);
        }
        "button" if rest.len() == 2 => {
            let button = parse_u32(&rest[0], "button")?;
            let button_state = parse_button_state(&rest[1])?;
            state.forward_pointer_button(button, button_state, time);
        }
        "key" if rest.len() == 2 => {
            let key = parse_u32(&rest[0], "key")?;
            let key_state = parse_key_state(&rest[1])?;
            state.forward_keyboard_key(key.into(), key_state, time);
        }
        "axis" if rest.len() == 2 => {
            let horizontal = parse_f64(&rest[0], "horizontal")?;
            let vertical = parse_f64(&rest[1], "vertical")?;
            let mut frame = AxisFrame::new(time).source(AxisSource::Wheel);
            if horizontal != 0.0 {
                frame = frame.value(Axis::Horizontal, horizontal);
            }
            if vertical != 0.0 {
                frame = frame.value(Axis::Vertical, vertical);
            }
            state.forward_pointer_axis(frame);
        }
        "wheel" if rest.len() == 2 => {
            let horizontal = parse_i32(&rest[0], "horizontal_v120")?;
            let vertical = parse_i32(&rest[1], "vertical_v120")?;
            let mut frame = AxisFrame::new(time).source(AxisSource::Wheel);
            if horizontal != 0 {
                frame = frame
                    .value(Axis::Horizontal, f64::from(horizontal) * 15.0 / 120.0)
                    .v120(Axis::Horizontal, horizontal);
            }
            if vertical != 0 {
                frame = frame
                    .value(Axis::Vertical, f64::from(vertical) * 15.0 / 120.0)
                    .v120(Axis::Vertical, vertical);
            }
            state.forward_pointer_axis(frame);
        }
        _ => bail!(
            "usage: inject motion <x> <y> | center-motion | button <code> <pressed|released> | key <code> <pressed|released> | axis <horizontal> <vertical> | wheel <horizontal_v120> <vertical_v120>"
        ),
    }

    Ok(())
}

fn monotonic_time_msec(state: &Manifold) -> u32 {
    let millis = state.start_time.elapsed().as_millis();
    u32::try_from(millis.min(u128::from(u32::MAX))).expect("millis clamped to u32 range")
}

fn parse_f64(value: &str, name: &str) -> Result<f64> {
    value
        .parse()
        .with_context(|| format!("{name} must be a number"))
}

fn parse_u32(value: &str, name: &str) -> Result<u32> {
    value
        .parse()
        .with_context(|| format!("{name} must be an unsigned integer"))
}

fn parse_u64(value: &str, name: &str) -> Result<u64> {
    value
        .parse()
        .with_context(|| format!("{name} must be an unsigned integer"))
}

fn parse_i32(value: &str, name: &str) -> Result<i32> {
    value
        .parse()
        .with_context(|| format!("{name} must be an integer"))
}

fn parse_button_state(value: &str) -> Result<ButtonState> {
    match value {
        "pressed" | "press" | "down" => Ok(ButtonState::Pressed),
        "released" | "release" | "up" => Ok(ButtonState::Released),
        _ => bail!("button state must be pressed or released"),
    }
}

fn parse_key_state(value: &str) -> Result<KeyState> {
    match value {
        "pressed" | "press" | "down" => Ok(KeyState::Pressed),
        "released" | "release" | "up" => Ok(KeyState::Released),
        _ => bail!("key state must be pressed or released"),
    }
}

fn read_command_line(stream: &mut UnixStream) -> Result<String> {
    let mut bytes = Vec::new();
    let mut byte = [0_u8; 1];
    loop {
        let read = match stream.read(&mut byte) {
            Ok(read) => read,
            Err(err) if matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                bail!("timed out reading control command");
            }
            Err(err) => return Err(err.into()),
        };
        if read == 0 || byte[0] == b'\n' {
            break;
        }
        if bytes.len() >= MAX_CONTROL_COMMAND_BYTES {
            bail!("control command too long");
        }
        bytes.push(byte[0]);
    }

    String::from_utf8(bytes).context("control command must be utf-8")
}

fn parse_command_tokens(input: &str) -> Result<Vec<String>> {
    if input.starts_with('[') {
        let tokens: Vec<String> =
            serde_json::from_str(input).context("decode control command argv")?;
        if let [single] = tokens.as_slice() {
            return Ok(single.split_whitespace().map(ToString::to_string).collect());
        }
        return Ok(tokens);
    }

    Ok(input.split_whitespace().map(ToString::to_string).collect())
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
struct SurfaceSessionList {
    presentation: String,
    active: u64,
    sessions: Vec<SurfaceSessionSummary>,
}

impl SurfaceSessionList {
    fn from_state(state: &mut Manifold) -> Self {
        state.refresh_window_state();
        Self {
            presentation: match state.presentation {
                PresentationStatus::Attached => "Attached".to_string(),
                PresentationStatus::Detached => "Detached".to_string(),
            },
            active: state.active_surface_session().raw(),
            sessions: state.surface_session_summaries(),
        }
    }
}

fn request_surface_session_list(control_socket: &Path) -> Result<SurfaceSessionList> {
    let response = request_socket_command(
        control_socket,
        &["surface-session".to_string(), "list-json".to_string()],
        false,
    )?;
    serde_json::from_str(response.trim()).context("decode surface-session list")
}

enum SessionSwitchOutcome {
    Existing(u64),
    Create(String),
}

fn run_skim_session_picker(
    skim: &std::ffi::OsStr,
    sessions: &[SurfaceSessionSummary],
    initial_query: Option<&str>,
) -> Result<SessionSwitchOutcome> {
    if let Some(outcome) = resolve_initial_query(sessions, initial_query) {
        return Ok(outcome);
    }

    let mut child = ProcessCommand::new(skim)
        .arg("--prompt")
        .arg("manifold session> ")
        .arg("--print-query")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .args(initial_query_args(initial_query))
        .spawn()
        .with_context(|| format!("start skim command {}", skim.to_string_lossy()))?;

    {
        let stdin = child.stdin.as_mut().context("open skim stdin")?;
        for session in sessions {
            writeln!(stdin, "{}", format_session_picker_row(session))?;
        }
    }

    let output = child.wait_with_output().context("wait for skim")?;
    let selected = String::from_utf8(output.stdout).context("skim output must be utf-8")?;
    let outcome = parse_skim_session_output(selected.trim_end())?;
    ensure!(
        output.status.success() || matches!(outcome, SessionSwitchOutcome::Create(_)),
        "skim exited with status {}",
        output.status
    );
    Ok(outcome)
}

fn initial_query_args(initial_query: Option<&str>) -> Vec<String> {
    let Some(initial_query) = initial_query else {
        return Vec::new();
    };
    vec!["--query".to_string(), initial_query.to_string()]
}

fn resolve_initial_query(
    sessions: &[SurfaceSessionSummary],
    initial_query: Option<&str>,
) -> Option<SessionSwitchOutcome> {
    let query = normalize_session_name(initial_query?)?;
    let query_lower = query.to_lowercase();
    let matches = sessions
        .iter()
        .filter(|session| session.name.to_lowercase().contains(&query_lower))
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [] => Some(SessionSwitchOutcome::Create(query)),
        [session] => Some(SessionSwitchOutcome::Existing(session.id)),
        _ => None,
    }
}

fn format_session_picker_row(session: &SurfaceSessionSummary) -> String {
    let marker = if session.active { "*" } else { " " };
    let windows = match session.windows {
        1 => "1 window".to_string(),
        count => format!("{count} windows"),
    };
    format!(
        "{}\t{} {}  {}",
        session.id,
        marker,
        sanitize_session_picker_text(&session.name),
        windows
    )
}

fn parse_skim_session_output(output: &str) -> Result<SessionSwitchOutcome> {
    let mut lines = output.lines();
    let query = lines.next().unwrap_or_default().trim();
    let selected = lines.next().unwrap_or_default().trim();
    if selected.is_empty() {
        let name = normalize_session_name(query).context("no session selected")?;
        return Ok(SessionSwitchOutcome::Create(name));
    }
    parse_selected_session_id(selected).map(SessionSwitchOutcome::Existing)
}

fn parse_selected_session_id(selected: &str) -> Result<u64> {
    let Some(id) = selected.split('\t').next().filter(|id| !id.is_empty()) else {
        bail!("no session selected");
    };
    parse_u64(id, "selected session id")
}

fn joined_name(parts: &[String]) -> Option<String> {
    normalize_session_name(&parts.join(" "))
}

fn normalize_session_name(name: &str) -> Option<String> {
    let name = name.split_whitespace().collect::<Vec<_>>().join(" ");
    (!name.is_empty()).then_some(name)
}

fn sanitize_session_picker_text(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn ensure_ok_response(response: &str) -> Result<()> {
    ensure!(
        response.starts_with("OK "),
        "control command failed: {}",
        response.trim()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        SessionSwitchOutcome, format_session_picker_row, parse_skim_session_output,
        resolve_initial_query,
    };
    use crate::state::SurfaceSessionSummary;

    fn session(id: u64, name: &str) -> SurfaceSessionSummary {
        SurfaceSessionSummary {
            id,
            name: name.to_string(),
            active: false,
            windows: 0,
        }
    }

    #[test]
    fn picker_row_starts_with_machine_readable_session_id() {
        let row = format_session_picker_row(&SurfaceSessionSummary {
            id: 7,
            name: "notes".to_string(),
            active: true,
            windows: 2,
        });

        let output = format!("ignored query\n{row}");
        let outcome = parse_skim_session_output(&output).unwrap();
        assert!(matches!(outcome, SessionSwitchOutcome::Existing(7)));
        assert!(row.contains("* notes"));
        assert!(row.contains("2 windows"));
    }

    #[test]
    fn query_without_selection_creates_named_session() {
        let outcome = parse_skim_session_output("research notes").unwrap();

        assert!(matches!(
            outcome,
            SessionSwitchOutcome::Create(name) if name == "research notes"
        ));
    }

    #[test]
    fn initial_query_selects_single_name_match() {
        let sessions = vec![session(1, "session 1"), session(2, "research notes")];
        let outcome = resolve_initial_query(&sessions, Some("research")).unwrap();

        assert!(matches!(outcome, SessionSwitchOutcome::Existing(2)));
    }

    #[test]
    fn initial_query_creates_when_name_matches_nothing() {
        let sessions = vec![session(1, "session 1")];
        let outcome = resolve_initial_query(&sessions, Some("research notes")).unwrap();

        assert!(matches!(
            outcome,
            SessionSwitchOutcome::Create(name) if name == "research notes"
        ));
    }

    #[test]
    fn empty_query_without_selection_is_cancelled_selection() {
        assert!(parse_skim_session_output("").is_err());
    }
}
