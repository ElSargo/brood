use std::{
    fs,
    io::{ErrorKind, Read, Write},
    os::unix::net::{UnixListener, UnixStream},
    path::Path,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use smithay::{
    backend::input::{Axis, AxisSource, ButtonState, KeyState},
    input::pointer::AxisFrame,
    reexports::calloop::{EventLoop, Interest, Mode, PostAction, generic::Generic},
    utils::Point,
};
use tracing::{info, warn};

use crate::{CtlArgs, browser, state::Manifold};

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
    let mut stream = UnixStream::connect(&args.control_socket)
        .with_context(|| format!("connect {}", args.control_socket.display()))?;
    let command = if args.json {
        args.command.join(" ")
    } else {
        serde_json::to_string(&args.command).context("encode control command")?
    };
    if args.json {
        let _: Vec<String> =
            serde_json::from_str(&command).context("validate JSON control argv")?;
    }
    writeln!(stream, "{command}")?;

    let mut response = String::new();
    stream.read_to_string(&mut response)?;
    print!("{response}");
    Ok(())
}

fn handle_stream(stream: &mut UnixStream, state: &mut Manifold) -> Result<()> {
    let input = read_command_line(stream)?;
    let tokens = parse_command_tokens(input.trim())?;
    let Some((command, args)) = tokens.split_first() else {
        bail!("empty control command");
    };

    match command.as_str() {
        "attach" if args.is_empty() => {
            state.request_attach();
            writeln!(stream, "OK attach requested")?;
        }
        "detach" if args.is_empty() => {
            state.request_detach();
            writeln!(stream, "OK detach requested")?;
        }
        "close-frontend" | "frontend-close" if args.is_empty() => {
            state.request_detach();
            writeln!(stream, "OK frontend close requested")?;
        }
        "status" if args.is_empty() => {
            let snapshot = StatusSnapshot::from_state(state);
            writeln!(
                stream,
                "OK wayland_display={} wayland_client_insert_errors={} wayland_dispatch_errors={} xdg_configure_errors={} presentation={} presentation_attach_failures={} presentation_runtime_failures={} presentation_last_error={} windows={} browsers={} frames={} client_frame_callbacks={} commits={} pending_commits={} commit_frame_count={} last_commit_batch_commits={} max_commit_batch_commits={} last_commit_to_frame_us={} max_commit_to_frame_us={} last_frame_us={} max_frame_us={} last_frame_interval_us={} max_frame_interval_us={} input_events={} keyboard_events={} pointer_motion_events={} pointer_button_events={} pointer_axis_events={} pointer_target_hits={} pointer_target_misses={} focus_clears={} focus_repairs={} window_lifecycle_refreshes={} window_lifecycle_focus_clears={} window_lifecycle_focus_repairs={} pending_input_events={} input_frame_count={} last_input_batch_events={} max_input_batch_events={} last_input_to_frame_us={} max_input_to_frame_us={} attach_count={} detach_count={} last_attach_latency_us={} max_attach_latency_us={} last_attach_to_frame_us={} max_attach_to_frame_us={} last_detach_latency_us={} max_detach_latency_us={} shm_formats={} egl_wl_bind={} dmabuf={} dmabuf_formats={} dmabuf_disabled={} dmabuf_attempts={} dmabuf_accepted={} dmabuf_rejected={} dmabuf_last_import={} dmabuf_last_error={} dmabuf_explicit_sync_release_points={} dmabuf_explicit_sync_blockers={} dmabuf_implicit_blockers={} dmabuf_blocker_failures={} dmabuf_last_blocker_error={} render_node={} explicit_sync={} explicit_sync_disabled={}",
                snapshot.wayland_display,
                snapshot.wayland_client_insert_errors,
                snapshot.wayland_dispatch_errors,
                snapshot.xdg_configure_errors,
                snapshot.presentation,
                snapshot.presentation_attach_failures,
                snapshot.presentation_runtime_failures,
                snapshot.presentation_last_error,
                snapshot.windows,
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
                snapshot.last_frame_us,
                snapshot.max_frame_us,
                snapshot.last_frame_interval_us,
                snapshot.max_frame_interval_us,
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
            )?;
        }
        "status-json" if args.is_empty() => {
            let snapshot = StatusSnapshot::from_state(state);
            writeln!(stream, "{}", snapshot.to_json())?;
        }
        "quit" if args.is_empty() => {
            let closed_browsers = state.browsers.close_all().len();
            state.loop_signal.stop();
            writeln!(
                stream,
                "OK quit requested browsers_closed={closed_browsers}"
            )?;
        }
        "spawn" => {
            if args.is_empty() {
                bail!("spawn requires a command");
            }
            state.spawn_hosted_app_argv(args)?;
            writeln!(stream, "OK spawned {}", args.join(" "))?;
        }
        "browser" => {
            write!(stream, "{}", browser::handle_control(args, state)?)?;
        }
        "inject" => {
            handle_inject(args, state)?;
            writeln!(stream, "OK injected {}", args.join(" "))?;
        }
        _ => bail!("unknown command: {}", tokens.join(" ")),
    }

    if let Err(err) = state.display_handle.flush_clients() {
        warn!(?err, "flush clients after control command failed");
    }

    Ok(())
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
    last_frame_us: u128,
    max_frame_us: u128,
    last_frame_interval_us: u128,
    max_frame_interval_us: u128,
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
            windows: state.space.elements().count(),
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
            last_frame_us: state.stats.last_frame_micros,
            max_frame_us: state.stats.max_frame_micros,
            last_frame_interval_us: state.stats.last_frame_interval_micros,
            max_frame_interval_us: state.stats.max_frame_interval_micros,
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
        _ => bail!(
            "usage: inject motion <x> <y> | center-motion | button <code> <pressed|released> | key <code> <pressed|released> | axis <horizontal> <vertical>"
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
