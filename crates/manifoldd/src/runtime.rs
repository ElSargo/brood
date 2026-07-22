//! Product runtime ownership for a Manifold session.
//!
//! Compositor protocol wiring lives in `compositor`; this module owns process
//! threads, startup policy, the calloop drive loop, and ordered shutdown.

use std::{env, path::PathBuf, thread};

use anyhow::{Context, Result, anyhow, bail};
use manifold_wasm::{TestHarnessOutcomeKind, TestHarnessRuntime};
use smithay::reexports::{calloop::EventLoop, wayland_server::Display};
use tracing::{info, warn};

use crate::{
    MANIFOLDD_DBUS_ADDRESS_ENV, MANIFOLDD_DBUS_DESTINATION_ENV, RunArgs, SessionArgs,
    browser::BrowserLaunchOptions, config, control, presentation, session_runner, state::Manifold,
};

pub fn run(args: RunArgs) -> Result<()> {
    if session_runner::run_if_configured(&args)? {
        return Ok(());
    }
    serve_session(args.session)
}

pub fn serve_session(args: SessionArgs) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .thread_name("manifold-tokio")
        .enable_all()
        .build()
        .context("start Tokio runtime")?;
    let runtime_handle = runtime.handle().clone();
    let (result_tx, result_rx) = std::sync::mpsc::channel();
    let calloop_thread = thread::Builder::new()
        .name("manifold-calloop".to_string())
        .spawn(move || {
            result_tx
                .send(serve_session_on_calloop_thread(&args, runtime_handle))
                .ok();
        })
        .context("spawn calloop compositor thread")?;

    runtime.block_on(async move {
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
    })
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
    runtime: tokio::runtime::Handle,
) -> Result<()> {
    let mut event_loop: EventLoop<'static, Manifold> = EventLoop::try_new()?;
    let display: Display<Manifold> = Display::new()?;
    let mut state = Manifold::new(&mut event_loop, display);
    configure_session(&mut event_loop, &mut state, args, runtime)?;

    info!("manifoldd compositor running");
    let event_loop_result = event_loop.run(None, &mut state, service_runtime_tick);

    // Shutdown is unconditional: a calloop error must not strand plugin Stores,
    // workers, completion tasks, or legacy actor tasks.
    let harness_shutdown = state.shutdown_test_harness();
    let hosted_shutdown = state.stop_all_hosted_io_actors();
    let legacy_shutdown = state.stop_legacy_wasm_plugin_actors();
    event_loop_result?;
    harness_shutdown?;
    hosted_shutdown?;
    legacy_shutdown?;
    report_harness_completion(&state)
}

fn configure_session(
    event_loop: &mut EventLoop<'static, Manifold>,
    state: &mut Manifold,
    args: &SessionArgs,
    runtime: tokio::runtime::Handle,
) -> Result<()> {
    state.set_async_runtime(runtime.clone());
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
    control::listen(event_loop, control_target.clone(), runtime)?;
    configure_control_environment(state, &control_target);
    let init_actor = if args.no_init {
        None
    } else if let Some(path) = args.init_actor.clone() {
        Some(path)
    } else {
        manifold_cli::realm_init_actor_path(&config_path, manifold_cli::RealmKind::Session)?
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
    if let Some((provider, path)) = cli_policy_arg(args.wasm_key_policy.as_deref())? {
        state.replace_wasm_global_key_policy(&path, &provider)?;
        info!(provider, path = %path.display(), "loaded CLI WASM key policy");
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
    attach_initial_presentation(event_loop, state, args);

    if legacy_startup {
        let filter = if args.no_initial_surface_session {
            config::StartupAppFilter::PresentationOnly
        } else {
            config::StartupAppFilter::All
        };
        config::load_startup_apps(state, &config_path, filter)?;
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

fn configure_control_environment(state: &mut Manifold, target: &control::ControlTarget) {
    let address = target
        .address
        .clone()
        .or_else(|| env::var("DBUS_SESSION_BUS_ADDRESS").ok());
    state.manifold_session = Some(target.session_marker());
    state.manifold_dbus_destination = Some(target.destination.clone());
    state.manifold_dbus_address.clone_from(&address);
    state.host_dbus_address = address;
}

fn print_session_environment(state: &Manifold, target: &control::ControlTarget) {
    println!("WAYLAND_DISPLAY={}", state.socket_name.to_string_lossy());
    println!("{MANIFOLDD_DBUS_DESTINATION_ENV}={}", target.destination);
    if let Some(address) = state.manifold_dbus_address.as_deref() {
        println!("{MANIFOLDD_DBUS_ADDRESS_ENV}={address}");
    }
}

fn attach_initial_presentation(
    event_loop: &EventLoop<'static, Manifold>,
    state: &mut Manifold,
    args: &SessionArgs,
) {
    let result = if args.offscreen_presentation {
        presentation::attach_offscreen(&event_loop.handle(), state)
    } else if args.headless {
        return;
    } else {
        presentation::attach(&event_loop.handle(), state)
    };
    if let Err(err) = result {
        warn!(
            ?err,
            "failed to attach initial presentation; continuing detached"
        );
        state.record_presentation_attach_failure(&err.to_string());
    }
}

fn service_runtime_tick(state: &mut Manifold) {
    state.service_test_harness();
    state.service_realm_init_actor();
    if state.attach_requested && !state.presentation_started && !state.attach_schedule_pending {
        state.attach_schedule_pending = true;
        state.schedule_attach();
    }
}

fn report_harness_completion(state: &Manifold) -> Result<()> {
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

pub fn cli_policy_arg(values: Option<&[String]>) -> Result<Option<(String, PathBuf)>> {
    let Some(values) = values else {
        return Ok(None);
    };
    let [provider, path] = values else {
        bail!("WASM policy arguments require NAME and PATH");
    };
    Ok(Some((provider.clone(), PathBuf::from(path))))
}
