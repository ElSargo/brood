use std::{env, fs, path::PathBuf, time::Duration};

use anyhow::{Context, Result, ensure};
use brood_wasm::{ComputerUseJsConfig, ComputerUseJsIsolate};
use serde_json::Value;

fn main() -> Result<()> {
    let module_path = env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .context("usage: computer_use_js_probe <runtime.wasm>")?;
    let module = fs::read(&module_path)
        .with_context(|| format!("read QuickJS module at {}", module_path.display()))?;

    let mut isolate = ComputerUseJsIsolate::load(&module, ComputerUseJsConfig::default())?;
    let first = isolate.execute(
        "globalThis.counter = 40; globalThis.add = value => counter + value; console.log('seed', counter); counter",
    )?;
    ensure!(
        first.plan.exception.is_none(),
        "first call raised an exception"
    );
    ensure!(first.plan.value_json.as_deref() == Some("40"));
    ensure!(first.plan.console == ["seed 40"]);

    let second = isolate.execute(
        "counter = add(2); console.log('persisted', counter); computer.perform([computer.click(10, 20), computer.type('42'), computer.keypress(['ENTER'])]); counter",
    )?;
    ensure!(
        second.plan.exception.is_none(),
        "second call raised an exception"
    );
    ensure!(second.plan.value_json.as_deref() == Some("42"));
    ensure!(second.plan.console == ["persisted 42"]);
    ensure!(second.plan.calls.len() == 1);

    let authority = isolate.execute(
        "({ process: typeof process, require: typeof require, fetch: typeof fetch, WebSocket: typeof WebSocket, Deno: typeof Deno })",
    )?;
    let authority: Value = serde_json::from_str(
        authority
            .plan
            .value_json
            .as_deref()
            .context("authority probe returned no value")?,
    )?;
    for name in ["process", "require", "fetch", "WebSocket", "Deno"] {
        ensure!(
            authority
                .get(name)
                .is_some_and(|value| value == "undefined"),
            "unexpected {name} authority"
        );
    }

    let bounded_config = ComputerUseJsConfig {
        fuel_per_call: 50_000,
        wall_time: Duration::from_millis(50),
        ..ComputerUseJsConfig::default()
    };
    let mut bounded = ComputerUseJsIsolate::load(&module, bounded_config)?;
    let exhaustion = bounded.execute("for (;;) {}");
    ensure!(
        exhaustion.is_err(),
        "infinite script did not exhaust a limit"
    );

    println!(
        "runtime={} persistent_value=42 host_calls={} memory_bytes={} fuel={} ambient_authority=denied infinite_loop=bounded",
        module_path.display(),
        second.metrics.host_calls,
        second.metrics.memory_bytes,
        second.metrics.fuel_consumed,
    );
    Ok(())
}
