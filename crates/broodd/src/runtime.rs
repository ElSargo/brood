//! Product runtime ownership for a Brood session.
//!
//! Compositor protocol wiring lives in `compositor`; this module owns process
//! threads, startup policy, the calloop drive loop, and ordered shutdown.

use std::{env, thread};

use anyhow::{Context, Result, anyhow, bail};
use brood_wasm::{TestHarnessOutcomeKind, TestHarnessRuntime};
use smithay::reexports::{calloop::EventLoop, wayland_server::Display};
use tracing::info;

use crate::{
    BROODD_DBUS_ADDRESS_ENV, BROODD_DBUS_DESTINATION_ENV, RunArgs, SessionArgs,
    browser::BrowserLaunchOptions, config, control, frame_export, session_runner, state::Brood,
};

pub fn run(args: RunArgs) -> Result<()> {
    if session_runner::run_if_configured(&args)? {
        return Ok(());
    }
    serve_session(args.session)
}

pub fn serve_session(args: SessionArgs) -> Result<()> {
    let worker_partition =
        crate::scheduling_domain::backend_worker_partition(available_parallelism());
    let general_runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(worker_partition.general_io)
        .max_blocking_threads(32)
        .thread_name("brood-io")
        .enable_all()
        .build()
        .context("start backend I/O runtime")?;
    let actor_runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(worker_partition.actors)
        .max_blocking_threads(8)
        .thread_name("brood-backend-actor")
        .enable_all()
        .build()
        .context("start backend actor runtime")?;
    let general_handle = general_runtime.handle().clone();
    let actor_handle = actor_runtime.handle().clone();
    let (result_tx, result_rx) = std::sync::mpsc::channel();
    let calloop_thread = thread::Builder::new()
        .name("brood-calloop".to_string())
        .spawn(move || {
            result_tx
                .send(serve_session_on_calloop_thread(
                    &args,
                    general_handle,
                    actor_handle,
                ))
                .ok();
        })
        .context("spawn calloop compositor thread")?;

    let result = general_runtime.block_on(async move {
        tokio::task::spawn_blocking(move || {
            let result = result_rx
                .recv()
                .context("calloop compositor thread exited without result")?;
            calloop_thread
                .join()
                .map_err(|payload| compositor_thread_panic(payload.as_ref()))?;
            result
        })
        .await
        .context("join calloop compositor thread")?
    });
    // The actor runtime remains alive until calloop has synchronously joined
    // every generation-owned actor task.
    drop(actor_runtime);
    result
}

fn available_parallelism() -> usize {
    thread::available_parallelism().map_or(2, std::num::NonZero::get)
}

fn compositor_thread_panic(payload: &(dyn std::any::Any + Send)) -> anyhow::Error {
    if let Some(message) = payload.downcast_ref::<&str>() {
        anyhow!("calloop compositor thread panicked: {message}")
    } else if let Some(message) = payload.downcast_ref::<String>() {
        anyhow!("calloop compositor thread panicked: {message}")
    } else {
        anyhow!("calloop compositor thread panicked")
    }
}

#[allow(
    clippy::significant_drop_tightening,
    reason = "the event loop and compositor state intentionally live through ordered actor shutdown and harness reporting"
)]
fn serve_session_on_calloop_thread(
    args: &SessionArgs,
    general_runtime: tokio::runtime::Handle,
    actor_runtime: tokio::runtime::Handle,
) -> Result<()> {
    let mut event_loop: EventLoop<'static, Brood> = EventLoop::try_new()?;
    let display: Display<Brood> = Display::new()?;
    let mut state = Brood::new(&mut event_loop, display);
    configure_session(
        &mut event_loop,
        &mut state,
        args,
        general_runtime,
        actor_runtime,
    )?;

    info!("broodd compositor running");
    state.begin_event_loop();
    let event_loop_result = event_loop.run(None, &mut state, service_runtime_tick);
    state.end_event_loop();

    // Shutdown is unconditional: a calloop error must not strand plugin Stores,
    // workers, completion tasks, or legacy actor tasks.
    let harness_shutdown = state.shutdown_test_harness();
    let lifecycle_shutdown = state.finish_backend_lifecycle_tasks();
    let hosted_shutdown = state.stop_all_hosted_io_actors();
    let legacy_shutdown = state.stop_legacy_wasm_plugin_actors();
    event_loop_result?;
    harness_shutdown?;
    lifecycle_shutdown?;
    hosted_shutdown?;
    legacy_shutdown?;
    report_harness_completion(&state)
}

fn configure_session(
    event_loop: &mut EventLoop<'static, Brood>,
    state: &mut Brood,
    args: &SessionArgs,
    general_runtime: tokio::runtime::Handle,
    actor_runtime: tokio::runtime::Handle,
) -> Result<()> {
    state.set_runtime_domains(general_runtime.clone(), actor_runtime);
    frame_export::initialize(state).context("initialize headless frame exporter")?;
    state.test_surface_markers = args.test_surface_markers;
    let config_path = args
        .config
        .clone()
        .unwrap_or_else(config::global_config_path);
    if args.no_initial_surface_session {
        state.clear_initial_surface_session();
    }
    config::load_runtime_settings(state, &config_path)?;
    let control_target =
        control::ControlTarget::new(args.dbus_name.clone(), args.dbus_address.clone());
    let native_tool_socket = control::listen(event_loop, control_target.clone(), general_runtime)?;
    configure_control_environment(state, &control_target, &native_tool_socket);
    let backend_init_actor = if args.no_init {
        None
    } else {
        brood_cli::realm_init_actor_path(&config_path, brood_cli::RealmKind::Backend)?
    };
    if let Some(path) = backend_init_actor.as_deref() {
        state
            .configure_backend_init_actor(path)
            .with_context(|| format!("configure backend init actor {}", path.display()))?;
        info!(path = %path.display(), "configured blessed backend init actor");
    }
    let init_actor = if args.no_init {
        None
    } else if let Some(path) = args.init_actor.clone() {
        Some(path)
    } else {
        brood_cli::realm_init_actor_path(&config_path, brood_cli::RealmKind::Session)?
    };
    if let Some(path) = init_actor.as_deref() {
        state
            .configure_session_init_actor(path)
            .with_context(|| format!("configure session init actor {}", path.display()))?;
        info!(path = %path.display(), "configured blessed session init actor");
    }
    let legacy_startup = init_actor.is_none() && !args.no_default_modules;
    if legacy_startup {
        config::load_startup_modules(state, &config_path)?;
    }
    if let Some(path) = args.wasm_plugin.as_deref() {
        state.replace_wasm_plugin(path)?;
        info!(path = %path.display(), "loaded CLI WASM plugin");
    }
    if let Some(path) = args.test_harness.as_deref() {
        state.install_test_harness(TestHarnessRuntime::start(path)?)?;
        info!(path = %path.display(), "loaded CLI test harness");
    }
    state.configure_nested_test_target(
        args.nested_inner_dbus_name.clone(),
        args.nested_inner_dbus_address.clone(),
    )?;

    print_session_environment(state, &control_target);
    if legacy_startup && !args.no_initial_surface_session {
        config::load_startup_apps(state, &config_path)?;
    }
    if let Some(command) = args.command.as_ref() {
        state.spawn_hosted_app(command)?;
    }
    if let Some(url) = args.browser.as_ref() {
        state.spawn_browser_pane(BrowserLaunchOptions {
            url: url.clone(),
            ..BrowserLaunchOptions::default()
        })?;
    }
    Ok(())
}

fn configure_control_environment(
    state: &mut Brood,
    target: &control::ControlTarget,
    native_tool_socket: &std::path::Path,
) {
    let address = target
        .address
        .clone()
        .or_else(|| env::var("DBUS_SESSION_BUS_ADDRESS").ok());
    state.brood_session = Some(target.session_marker());
    state.brood_dbus_destination = Some(target.destination.clone());
    state.brood_dbus_address.clone_from(&address);
    state.brood_native_tool_socket = Some(native_tool_socket.as_os_str().to_os_string());
    state.host_dbus_address = address;
}

fn print_session_environment(state: &Brood, target: &control::ControlTarget) {
    println!("WAYLAND_DISPLAY={}", state.socket_name.to_string_lossy());
    println!("{BROODD_DBUS_DESTINATION_ENV}={}", target.destination);
    if let Some(address) = state.brood_dbus_address.as_deref() {
        println!("{BROODD_DBUS_ADDRESS_ENV}={address}");
    }
}

fn service_runtime_tick(state: &mut Brood) {
    let _profile_span = brood_profiler::profile_span!("server", "runtime_tick");
    let started = std::time::Instant::now();
    let driver_before_micros = state.stats.smithay_driver_total_micros;
    state.service_test_harness();
    state.service_realm_init_actor();
    state
        .stats
        .record_smithay_callback(started.elapsed(), driver_before_micros);
}

fn report_harness_completion(state: &Brood) -> Result<()> {
    if let Some(cleanup) = state.test_harness_cleanup() {
        println!("TEST_HARNESS_CLEANUP {cleanup:?}");
    }
    let Some(completion) = state.test_harness_completion() else {
        return Ok(());
    };
    println!(
        "TEST_HARNESS_RESULT kind={:?} exit_code={} detail={}",
        completion.kind, completion.exit_code, completion.detail
    );
    if completion.kind != TestHarnessOutcomeKind::Passed {
        bail!(
            "test harness exited with {:?} code {}: {}",
            completion.kind,
            completion.exit_code,
            completion.detail
        );
    }
    Ok(())
}
