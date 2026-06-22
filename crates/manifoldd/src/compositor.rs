use std::{
    env,
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
#[cfg(feature = "shm-fallback")]
use smithay::wayland::shm::{ShmHandler, ShmState};
use smithay::{
    backend::{
        allocator::{Buffer, dmabuf::Dmabuf},
        renderer::{ImportDma, utils::on_commit_buffer_handler},
    },
    desktop::{PopupKind, Window},
    input::{Seat, SeatHandler, SeatState},
    reexports::{
        calloop::{EventLoop, Interest},
        wayland_server::{
            Client, Display, Resource,
            protocol::{wl_buffer, wl_output, wl_seat, wl_surface::WlSurface},
        },
    },
    utils::Serial,
    wayland::{
        buffer::BufferHandler,
        compositor::{
            BufferAssignment, CompositorClientState, CompositorHandler, CompositorState,
            SurfaceAttributes, add_blocker, add_pre_commit_hook, get_parent, is_sync_subsurface,
            with_states,
        },
        dmabuf::{DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier, get_dmabuf},
        drm_syncobj::{DrmSyncobjCachedState, DrmSyncobjHandler, DrmSyncobjState},
        output::OutputHandler,
        selection::{
            SelectionHandler,
            data_device::{
                ClientDndGrabHandler, DataDeviceHandler, DataDeviceState, ServerDndGrabHandler,
                set_data_device_focus,
            },
        },
        shell::xdg::{
            PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
            XdgToplevelSurfaceData,
        },
    },
};
use tracing::{info, warn};

use crate::{
    MANIFOLD_SESSION_ENV, NestedTestArgs, RunArgs, TestArgs, TestCommand,
    browser::BrowserLaunchOptions,
    config, control, presentation,
    state::{ClientState, Manifold, PresentationStatus},
    wasm_key::WasmKeyHandler,
    wasm_map::WasmMapHook,
    wasm_test::TestHarnessRuntime,
};

pub fn run(args: RunArgs) -> Result<()> {
    let mut event_loop: EventLoop<'static, Manifold> = EventLoop::try_new()?;
    let display: Display<Manifold> = Display::new()?;
    let mut state = Manifold::new(&mut event_loop, display);

    if !args.no_default_modules {
        config::load_startup_modules(&mut state, &args.config)?;
    }

    if let Some(path) = args.wasm_key_handler.as_deref() {
        state.wasm_key_handler = Some(WasmKeyHandler::load(path)?);
        info!(path = %path.display(), "loaded CLI WASM key handler");
    }

    if let Some(path) = args.wasm_map_hook.as_deref() {
        state.wasm_map_hook = Some(WasmMapHook::load(path)?);
        info!(path = %path.display(), "loaded CLI WASM map hook");
    }

    if let Some(path) = args.test_harness.as_deref() {
        state.test_harness = Some(TestHarnessRuntime::start(path)?);
        info!(path = %path.display(), "loaded CLI test harness");
    }

    state.nested_inner_control_socket = args.nested_inner_control_socket.clone();

    control::listen(&mut event_loop, &args.control_socket)?;
    state.manifold_session = Some(format!("control={}", args.control_socket.display()));

    println!("WAYLAND_DISPLAY={}", state.socket_name.to_string_lossy());
    println!("MANIFOLDD_CONTROL={}", args.control_socket.display());

    if !args.headless {
        if let Err(err) = presentation::attach(&event_loop.handle(), &mut state) {
            warn!(
                ?err,
                "failed to attach initial presentation; continuing detached"
            );
            state.record_presentation_attach_failure(&err.to_string());
        }
    }

    if let Some(command) = args.command {
        state.spawn_hosted_app(&command)?;
    }

    if let Some(url) = args.browser {
        let options = BrowserLaunchOptions {
            url,
            ..BrowserLaunchOptions::default()
        };
        state.spawn_browser_pane(options)?;
    }

    info!("manifoldd compositor running");
    event_loop.run(None, &mut state, |state| {
        state.service_test_harness();
        if state.attach_requested && !state.presentation_started {
            state.attach_requested = false;
            state.schedule_attach();
        }
    })?;

    if let Some(completion) = state.test_harness_completion.as_ref() {
        println!(
            "TEST_HARNESS_RESULT kind={:?} exit_code={} detail={}",
            completion.kind, completion.exit_code, completion.detail
        );
    }

    Ok(())
}

pub fn run_test(args: TestArgs) -> Result<()> {
    match args
        .command
        .unwrap_or(TestCommand::Nested(NestedTestArgs::default()))
    {
        TestCommand::Nested(args) => run_nested_test(args),
    }
}

fn run_nested_test(args: NestedTestArgs) -> Result<()> {
    let out_dir = match args.out_dir.as_ref() {
        Some(dir) => {
            fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
            dir.clone()
        }
        None => {
            let dir = env::temp_dir().join(format!("manifold-nested-test-{}", std::process::id()));
            fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
            dir
        }
    };

    let outer_sock = out_dir.join("outer.sock");
    let inner_sock = out_dir.join("inner.sock");
    let outer_log = out_dir.join("outer.log");
    let inner_log = out_dir.join("inner.log");
    let current_exe = env::current_exe().context("resolve current executable")?;
    let inherited_session = env::var_os(MANIFOLD_SESSION_ENV);

    match nested_test_topology(&args, inherited_session.as_deref()) {
        NestedTestTopology::OuterSupervisor => run_nested_test_with_outer(
            args,
            out_dir,
            current_exe,
            outer_sock,
            inner_sock,
            outer_log,
            inner_log,
        ),
        NestedTestTopology::CurrentSession => run_nested_test_in_current_session(
            args,
            out_dir,
            current_exe,
            inner_sock,
            inner_log,
            inherited_session,
        ),
    }
}

fn run_nested_test_with_outer(
    args: NestedTestArgs,
    out_dir: PathBuf,
    current_exe: PathBuf,
    outer_sock: PathBuf,
    inner_sock: PathBuf,
    outer_log: PathBuf,
    inner_log: PathBuf,
) -> Result<()> {
    let mut outer_child = spawn_outer(
        &current_exe,
        &outer_sock,
        &inner_sock,
        &outer_log,
        args.outer_headful,
        args.harness.as_deref(),
    )?;

    let cleanup = NestedCleanup {
        outer_sock: outer_sock.clone(),
        inner_sock: inner_sock.clone(),
    };

    wait_for_control_socket(&outer_sock, &outer_log, &mut outer_child, "outer")?;
    spawn_inner(
        &outer_sock,
        &current_exe,
        &inner_sock,
        &inner_log,
        args.inner_headless,
    )?;
    wait_for_control_socket(&inner_sock, &inner_log, &mut outer_child, "inner")?;

    if !args.inner_headless {
        wait_for_outer_window(&outer_sock, &outer_log, &mut outer_child)?;
    }

    if let Some(command) = args.inner_command.as_deref() {
        let inner_command = shell_split(command)?;
        control::request_socket_command(
            &inner_sock,
            &[vec!["spawn".to_string()], inner_command].concat(),
            false,
        )?;
    }

    println!("OK nested test ready");
    println!("out_dir={}", out_dir.display());
    println!("outer_control={}", outer_sock.display());
    println!("inner_control={}", inner_sock.display());
    println!("outer_log={}", outer_log.display());
    println!("inner_log={}", inner_log.display());
    println!("topology=outer-supervisor");
    println!("outer_headless={}", !args.outer_headful);
    println!("inner_headless={}", args.inner_headless);

    if args.keep_running {
        let status = outer_child.wait().context("wait for outer process")?;
        if !status.success() {
            bail!("outer process exited with {status}");
        }
        return Ok(());
    }

    if args.harness.is_some() {
        let status = outer_child.wait().context("wait for outer process")?;
        println!("outer_exit={status}");
        if !status.success() {
            bail!("outer process exited with {status}");
        }
        return Ok(());
    }

    cleanup.stop_all();
    let status = outer_child.wait().context("wait for outer process")?;
    if !status.success() {
        bail!("outer process exited with {status}");
    }
    Ok(())
}

fn run_nested_test_in_current_session(
    args: NestedTestArgs,
    out_dir: PathBuf,
    current_exe: PathBuf,
    subject_sock: PathBuf,
    subject_log: PathBuf,
    inherited_session: Option<std::ffi::OsString>,
) -> Result<()> {
    let mut subject_child = spawn_subject(
        &current_exe,
        &subject_sock,
        &subject_log,
        args.inner_headless,
        args.harness.as_deref(),
    )?;

    wait_for_control_socket(&subject_sock, &subject_log, &mut subject_child, "subject")?;

    if let Some(command) = args.inner_command.as_deref() {
        let subject_command = shell_split(command)?;
        control::request_socket_command(
            &subject_sock,
            &[vec!["spawn".to_string()], subject_command].concat(),
            false,
        )?;
    }

    println!("OK nested test ready");
    println!("out_dir={}", out_dir.display());
    println!("topology=current-session");
    println!("subject_control={}", subject_sock.display());
    println!("subject_log={}", subject_log.display());
    println!("inner_control={}", subject_sock.display());
    println!("inner_log={}", subject_log.display());
    println!("inner_headless={}", args.inner_headless);
    if let Some(session) = inherited_session {
        println!(
            "inherited_{}={}",
            MANIFOLD_SESSION_ENV,
            session.to_string_lossy()
        );
    }

    if args.keep_running {
        let status = subject_child.wait().context("wait for subject process")?;
        if !status.success() {
            bail!("subject process exited with {status}");
        }
        return Ok(());
    }

    if args.harness.is_some() {
        let status = subject_child.wait().context("wait for subject process")?;
        println!("subject_exit={status}");
        if !status.success() {
            bail!("subject process exited with {status}");
        }
        return Ok(());
    }

    let _ = control::request_socket_command(&subject_sock, &[String::from("quit")], false);
    let status = subject_child.wait().context("wait for subject process")?;
    if !status.success() {
        bail!("subject process exited with {status}");
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NestedTestTopology {
    OuterSupervisor,
    CurrentSession,
}

fn nested_test_topology(
    args: &NestedTestArgs,
    inherited_session: Option<&OsStr>,
) -> NestedTestTopology {
    let has_inherited_session = inherited_session
        .map(|session| !session.is_empty())
        .unwrap_or(false);
    if args.force_outer || !has_inherited_session {
        NestedTestTopology::OuterSupervisor
    } else {
        NestedTestTopology::CurrentSession
    }
}

fn spawn_outer(
    current_exe: &Path,
    outer_sock: &Path,
    inner_sock: &Path,
    outer_log: &Path,
    outer_headful: bool,
    harness: Option<&Path>,
) -> Result<Child> {
    let mut command = Command::new(current_exe);
    command.arg("run");
    command.arg("--control-socket").arg(outer_sock);
    command.arg("--nested-inner-control-socket").arg(inner_sock);
    if !outer_headful {
        command.arg("--headless");
    }
    if let Some(harness) = harness {
        command.arg("--test-harness").arg(harness);
    }
    command.stdout(Stdio::from(
        fs::File::create(outer_log).with_context(|| format!("create {}", outer_log.display()))?,
    ));
    command.stderr(Stdio::from(
        fs::OpenOptions::new()
            .append(true)
            .open(outer_log)
            .with_context(|| format!("open {}", outer_log.display()))?,
    ));
    command
        .spawn()
        .with_context(|| format!("spawn outer process {}", current_exe.display()))
}

fn spawn_subject(
    current_exe: &Path,
    subject_sock: &Path,
    subject_log: &Path,
    subject_headless: bool,
    harness: Option<&Path>,
) -> Result<Child> {
    let mut command = Command::new(current_exe);
    command.arg("run");
    command.arg("--control-socket").arg(subject_sock);
    if subject_headless {
        command.arg("--headless");
    }
    if let Some(harness) = harness {
        command.arg("--test-harness").arg(harness);
    }
    command.stdout(Stdio::from(
        fs::File::create(subject_log)
            .with_context(|| format!("create {}", subject_log.display()))?,
    ));
    command.stderr(Stdio::from(
        fs::OpenOptions::new()
            .append(true)
            .open(subject_log)
            .with_context(|| format!("open {}", subject_log.display()))?,
    ));
    command
        .spawn()
        .with_context(|| format!("spawn subject process {}", current_exe.display()))
}

fn spawn_inner(
    outer_sock: &Path,
    current_exe: &Path,
    inner_sock: &Path,
    inner_log: &Path,
    inner_headless: bool,
) -> Result<()> {
    let mut command = format!(
        "exec {} run --control-socket {}",
        shell_quote(&current_exe.display().to_string()),
        shell_quote(&inner_sock.display().to_string()),
    );
    if inner_headless {
        command.push_str(" --headless");
    }
    command.push_str(&format!(
        " >{} 2>&1",
        shell_quote(&inner_log.display().to_string())
    ));
    let inner_args = vec![
        "spawn".to_string(),
        "bash".to_string(),
        "-lc".to_string(),
        command,
    ];
    let response = control::request_socket_command(outer_sock, &inner_args, false)?;
    if !response.starts_with("OK ") {
        bail!("spawn inner failed: {}", response.trim());
    }
    Ok(())
}

fn wait_for_control_socket(
    control_socket: &Path,
    log_path: &Path,
    outer_child: &mut Child,
    label: &str,
) -> Result<()> {
    let start = Instant::now();
    loop {
        if let Some(status) = outer_child.try_wait().context("poll outer process")? {
            let log_tail = read_log_tail(log_path);
            bail!("{label} failed before control socket ready: {status}\n{log_tail}");
        }
        if let Ok(response) =
            control::request_socket_command(control_socket, &[String::from("status-json")], false)
        {
            if response.trim_start().starts_with('{') {
                return Ok(());
            }
        }
        if start.elapsed() > Duration::from_secs(15) {
            let log_tail = read_log_tail(log_path);
            bail!("timed out waiting for {label} control socket\n{log_tail}");
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn wait_for_outer_window(
    outer_sock: &Path,
    log_path: &Path,
    outer_child: &mut Child,
) -> Result<()> {
    let start = Instant::now();
    loop {
        if let Some(status) = outer_child.try_wait().context("poll outer process")? {
            let log_tail = read_log_tail(log_path);
            bail!("outer exited before inner presentation mapped: {status}\n{log_tail}");
        }
        if let Ok(response) =
            control::request_socket_command(outer_sock, &[String::from("status-json")], false)
        {
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&response) {
                if json
                    .get("windows")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0)
                    >= 1
                {
                    return Ok(());
                }
            }
        }
        if start.elapsed() > Duration::from_secs(15) {
            let log_tail = read_log_tail(log_path);
            bail!("timed out waiting for outer to host inner presentation\n{log_tail}");
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn shell_split(command: &str) -> Result<Vec<String>> {
    let command = command.trim();
    if command.is_empty() {
        bail!("inner command must not be empty");
    }
    Ok(command
        .split_whitespace()
        .map(ToString::to_string)
        .collect())
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn read_log_tail(path: &Path) -> String {
    match fs::read_to_string(path) {
        Ok(contents) => {
            let lines = contents.lines().rev().take(40).collect::<Vec<_>>();
            lines.into_iter().rev().collect::<Vec<_>>().join("\n")
        }
        Err(err) => format!("log unreadable: {err}"),
    }
}

struct NestedCleanup {
    outer_sock: PathBuf,
    inner_sock: PathBuf,
}

impl NestedCleanup {
    fn stop_all(&self) {
        let _ = control::request_socket_command(&self.inner_sock, &[String::from("quit")], false);
        let _ = control::request_socket_command(&self.outer_sock, &[String::from("quit")], false);
    }
}

impl CompositorHandler for Manifold {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        &client
            .get_data::<ClientState>()
            .expect("client state")
            .compositor_state
    }

    fn new_surface(&mut self, surface: &WlSurface) {
        add_pre_commit_hook::<Self, _>(surface, move |state, _dh, surface| {
            let mut acquire_point = None;
            let mut has_release_point = false;
            let maybe_dmabuf = with_states(surface, |surface_data| {
                let mut syncobj_guard = surface_data.cached_state.get::<DrmSyncobjCachedState>();
                let syncobj_state = syncobj_guard.pending();
                acquire_point.clone_from(&syncobj_state.acquire_point);
                has_release_point = syncobj_state.release_point.is_some();
                surface_data
                    .cached_state
                    .get::<SurfaceAttributes>()
                    .pending()
                    .buffer
                    .as_ref()
                    .and_then(|assignment| match assignment {
                        BufferAssignment::NewBuffer(buffer) => get_dmabuf(buffer).cloned().ok(),
                        BufferAssignment::Removed => None,
                    })
            });

            if let Some(dmabuf) = maybe_dmabuf {
                if has_release_point {
                    state.dmabuf.stats.explicit_sync_release_points += 1;
                }

                match dmabuf_blocker_plan(true, acquire_point.is_some()) {
                    DmabufBlockerPlan::ExplicitSync => {
                        let acquire_point =
                            acquire_point.expect("explicit sync plan has acquire point");
                        match acquire_point.generate_blocker() {
                            Ok((blocker, source)) => {
                                if let Some(client) = surface.client() {
                                    let result = state.loop_handle.insert_source(
                                        source,
                                        move |_, _, data| {
                                            let display_handle = data.display_handle.clone();
                                            data.client_compositor_state(&client)
                                                .blocker_cleared(data, &display_handle);
                                            Ok(())
                                        },
                                    );
                                    match result {
                                        Ok(_) => {
                                            state.dmabuf.stats.explicit_sync_blockers += 1;
                                            state.dmabuf.stats.last_blocker_error = None;
                                            add_blocker(surface, blocker);
                                        }
                                        Err(err) => {
                                            state.record_dmabuf_blocker_failure(&format!(
                                                "explicit_sync_insert_source:{err:?}"
                                            ));
                                        }
                                    }
                                } else {
                                    state.record_dmabuf_blocker_failure("explicit_sync_no_client");
                                }
                            }
                            Err(err) => {
                                state.record_dmabuf_blocker_failure(&format!(
                                    "explicit_sync_generate_blocker:{err:?}"
                                ));
                            }
                        }
                        return;
                    }
                    DmabufBlockerPlan::ImplicitDmabuf => {}
                    DmabufBlockerPlan::None => return,
                }

                match dmabuf.generate_blocker(Interest::READ) {
                    Ok((blocker, source)) => {
                        if let Some(client) = surface.client() {
                            let result =
                                state.loop_handle.insert_source(source, move |_, _, data| {
                                    let display_handle = data.display_handle.clone();
                                    data.client_compositor_state(&client)
                                        .blocker_cleared(data, &display_handle);
                                    Ok(())
                                });
                            match result {
                                Ok(_) => {
                                    state.dmabuf.stats.implicit_dmabuf_blockers += 1;
                                    state.dmabuf.stats.last_blocker_error = None;
                                    add_blocker(surface, blocker);
                                }
                                Err(err) => {
                                    state.record_dmabuf_blocker_failure(&format!(
                                        "implicit_dmabuf_insert_source:{err:?}"
                                    ));
                                }
                            }
                        } else {
                            state.record_dmabuf_blocker_failure("implicit_dmabuf_no_client");
                        }
                    }
                    Err(err) => {
                        state.record_dmabuf_blocker_failure(&format!(
                            "implicit_dmabuf_generate_blocker:{err:?}"
                        ));
                    }
                }
            }
        });
    }

    fn commit(&mut self, surface: &WlSurface) {
        self.stats
            .record_commit(self.presentation == PresentationStatus::Attached);
        on_commit_buffer_handler::<Self>(surface);
        if !is_sync_subsurface(surface) {
            let mut root = surface.clone();
            while let Some(parent) = get_parent(&root) {
                root = parent;
            }
            if let Some(window) = self
                .space
                .elements()
                .find(|window| Manifold::window_toplevel_surface(window).as_ref() == Some(&root))
            {
                window.on_commit();
            }
        }

        handle_xdg_commit(self, surface);
    }
}

impl BufferHandler for Manifold {
    fn buffer_destroyed(&mut self, _buffer: &wl_buffer::WlBuffer) {}
}

#[cfg(feature = "shm-fallback")]
impl ShmHandler for Manifold {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

impl DmabufHandler for Manifold {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf.state
    }

    fn dmabuf_imported(
        &mut self,
        _global: &DmabufGlobal,
        dmabuf: Dmabuf,
        notifier: ImportNotifier,
    ) {
        self.dmabuf.stats.attempts += 1;
        self.dmabuf.stats.last_import = Some(dmabuf_import_summary(&dmabuf));
        self.dmabuf.stats.last_error = None;

        let Some(importer) = self.dmabuf.importer.clone() else {
            self.dmabuf.stats.rejected += 1;
            self.dmabuf.stats.last_error = Some("no_presentation_importer".to_string());
            notifier.failed();
            return;
        };

        let mut runtime = importer.borrow_mut();
        let Some(importer) = runtime.backend.as_mut() else {
            self.dmabuf.stats.rejected += 1;
            self.dmabuf.stats.last_error = Some("no_presentation_importer".to_string());
            notifier.failed();
            return;
        };

        let result = importer.renderer().import_dmabuf(&dmabuf, None);

        match result {
            Ok(_) => {
                self.dmabuf.stats.accepted += 1;
                if let Err(err) = notifier.successful::<Manifold>() {
                    warn!(
                        ?err,
                        "client disconnected before dmabuf buffer creation completed"
                    );
                }
            }
            Err(err) => {
                self.dmabuf.stats.rejected += 1;
                self.dmabuf.stats.last_error = Some(crate::presentation::sanitize_status_value(
                    &format!("{err:?}"),
                ));
                warn!(
                    ?err,
                    "failed to import client dmabuf into presentation renderer"
                );
                notifier.failed();
            }
        }
    }
}

impl DrmSyncobjHandler for Manifold {
    fn drm_syncobj_state(&mut self) -> Option<&mut DrmSyncobjState> {
        self.dmabuf.explicit_sync_state.as_mut()
    }
}

fn dmabuf_import_summary(dmabuf: &Dmabuf) -> String {
    let size = dmabuf.size();
    let format = dmabuf.format();
    let strides = dmabuf
        .strides()
        .map(|stride| stride.to_string())
        .collect::<Vec<_>>()
        .join(".");
    let offsets = dmabuf
        .offsets()
        .map(|offset| offset.to_string())
        .collect::<Vec<_>>()
        .join(".");
    let node = dmabuf
        .node()
        .map(|node| format!("{node:?}"))
        .unwrap_or_else(|| "none".to_string());

    crate::presentation::sanitize_status_value(&format!(
        "{}x{}:{:?}:{:?}:planes{}:strides{}:offsets{}:yinvert{}:node{}",
        size.w,
        size.h,
        format.code,
        format.modifier,
        dmabuf.num_planes(),
        if strides.is_empty() { "none" } else { &strides },
        if offsets.is_empty() { "none" } else { &offsets },
        dmabuf.y_inverted(),
        node,
    ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DmabufBlockerPlan {
    None,
    ImplicitDmabuf,
    ExplicitSync,
}

fn dmabuf_blocker_plan(has_dmabuf: bool, has_explicit_acquire_point: bool) -> DmabufBlockerPlan {
    if !has_dmabuf {
        return DmabufBlockerPlan::None;
    }
    if has_explicit_acquire_point {
        return DmabufBlockerPlan::ExplicitSync;
    }
    DmabufBlockerPlan::ImplicitDmabuf
}

impl XdgShellHandler for Manifold {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        let window = Window::new_wayland_window(surface);
        let active_surface = Manifold::window_toplevel_surface(&window);
        let mut focus_surface = active_surface.clone();
        if let Some(surface) = active_surface.as_ref() {
            focus_surface = self.assign_new_toplevel_surface(surface);
        }
        let location = self.allocate_toplevel_location();
        self.space.map_element(window, location, true);
        self.record_window_mapped();
        self.focus_toplevel_surface(focus_surface);
    }

    fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        let _ = self.popups.track_popup(PopupKind::Xdg(surface));
    }

    fn fullscreen_request(
        &mut self,
        surface: ToplevelSurface,
        output: Option<wl_output::WlOutput>,
    ) {
        self.request_toplevel_fullscreen(surface.wl_surface().clone(), output);
    }

    fn unfullscreen_request(&mut self, surface: ToplevelSurface) {
        self.request_toplevel_unfullscreen(surface.wl_surface());
    }

    fn grab(&mut self, _surface: PopupSurface, _seat: wl_seat::WlSeat, _serial: Serial) {}

    fn reposition_request(
        &mut self,
        surface: PopupSurface,
        positioner: PositionerState,
        token: u32,
    ) {
        surface.with_pending_state(|state| {
            state.geometry = positioner.get_geometry();
            state.positioner = positioner;
        });
        surface.send_repositioned(token);
    }
}

fn handle_xdg_commit(state: &mut Manifold, surface: &WlSurface) {
    if let Some(window) = state
        .space
        .elements()
        .find(|window| Manifold::window_toplevel_surface(window).as_ref() == Some(surface))
        .cloned()
    {
        let initial_configure_sent = with_states(surface, |states| {
            let Some(data) = states.data_map.get::<XdgToplevelSurfaceData>() else {
                return None;
            };
            match data.lock() {
                Ok(data) => Some(data.initial_configure_sent),
                Err(err) => {
                    warn!(?err, "xdg toplevel data lock poisoned");
                    None
                }
            }
        });

        if initial_configure_sent == Some(false) {
            if let Some(toplevel) = window.toplevel() {
                toplevel.send_configure();
            }
        }
    }

    state.popups.commit(surface);
    if let Some(PopupKind::Xdg(popup)) = state.popups.find_popup(surface) {
        if !popup.is_initial_configure_sent() {
            if let Err(err) = popup.send_configure() {
                state.stats.xdg_configure_errors += 1;
                warn!(?err, "send initial popup configure failed");
            }
        }
    }
}

impl SeatHandler for Manifold {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }

    fn cursor_image(
        &mut self,
        _seat: &Seat<Self>,
        _image: smithay::input::pointer::CursorImageStatus,
    ) {
    }

    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&WlSurface>) {
        let client = focused.and_then(|surface| self.display_handle.get_client(surface.id()).ok());
        set_data_device_focus(&self.display_handle, seat, client);
    }
}

impl SelectionHandler for Manifold {
    type SelectionUserData = ();
}

impl DataDeviceHandler for Manifold {
    fn data_device_state(&self) -> &DataDeviceState {
        &self.data_device_state
    }
}

impl ClientDndGrabHandler for Manifold {}

impl ServerDndGrabHandler for Manifold {}

impl OutputHandler for Manifold {}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use super::{DmabufBlockerPlan, NestedTestTopology, dmabuf_blocker_plan, nested_test_topology};
    use crate::NestedTestArgs;

    #[test]
    fn dmabuf_without_explicit_acquire_uses_implicit_readiness_blocker() {
        assert_eq!(
            dmabuf_blocker_plan(true, false),
            DmabufBlockerPlan::ImplicitDmabuf,
        );
    }

    #[test]
    fn explicit_acquire_point_requires_explicit_sync_blocker() {
        assert_eq!(
            dmabuf_blocker_plan(true, true),
            DmabufBlockerPlan::ExplicitSync,
        );
    }

    #[test]
    fn no_dmabuf_needs_no_dmabuf_blocker() {
        assert_eq!(dmabuf_blocker_plan(false, true), DmabufBlockerPlan::None,);
        assert_eq!(dmabuf_blocker_plan(false, false), DmabufBlockerPlan::None,);
    }

    #[test]
    fn nested_test_uses_outer_without_session_marker() {
        let args = NestedTestArgs::default();
        assert_eq!(
            nested_test_topology(&args, None),
            NestedTestTopology::OuterSupervisor
        );
    }

    #[test]
    fn nested_test_uses_current_session_with_session_marker() {
        let args = NestedTestArgs::default();
        assert_eq!(
            nested_test_topology(&args, Some(OsStr::new("control=/tmp/manifoldd.sock"))),
            NestedTestTopology::CurrentSession
        );
    }

    #[test]
    fn nested_test_force_outer_overrides_session_marker() {
        let args = NestedTestArgs {
            force_outer: true,
            ..NestedTestArgs::default()
        };
        assert_eq!(
            nested_test_topology(&args, Some(OsStr::new("control=/tmp/manifoldd.sock"))),
            NestedTestTopology::OuterSupervisor
        );
    }
}

smithay::delegate_compositor!(Manifold);
smithay::delegate_xdg_shell!(Manifold);
#[cfg(feature = "shm-fallback")]
smithay::delegate_shm!(Manifold);
smithay::delegate_dmabuf!(Manifold);
smithay::delegate_drm_syncobj!(Manifold);
smithay::delegate_seat!(Manifold);
smithay::delegate_data_device!(Manifold);
smithay::delegate_output!(Manifold);
