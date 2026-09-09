use std::{sync::mpsc, thread, time::Duration};

use brood_wasm::facet_probe::{FacetProbeConfig, FacetProbeInput, run_facet_probe};

fn wasm(source: &str) -> Vec<u8> {
    source.as_bytes().to_vec()
}

fn input() -> Vec<FacetProbeInput> {
    vec![FacetProbeInput {
        label: "probe".to_owned(),
        bytes: b"immutable input".to_vec(),
    }]
}

const fn config() -> FacetProbeConfig {
    FacetProbeConfig {
        fuel_per_call: 100_000,
        epoch_tick: Duration::from_millis(1),
        epoch_deadline_ticks: 100,
    }
}

fn clean_fixture() -> Vec<u8> {
    wasm(
        r#"(module
            (memory (export "memory") 32)
            (func (export "facet-render") (param i32 i32 i32) (result i64)
                (i64.const 0)))"#,
    )
}

fn rejects_promptly(expected: &str, fixture: Vec<u8>) -> anyhow::Result<()> {
    let (sender, receiver) = mpsc::sync_channel(1);
    let invocation = thread::spawn(move || {
        let _delivery = sender.send(run_facet_probe(&fixture, input(), config()));
    });
    let result = receiver
        .recv_timeout(Duration::from_secs(2))
        .map_err(|error| anyhow::anyhow!("facet probe did not return promptly: {error}"))?;
    invocation
        .join()
        .map_err(|_panic| anyhow::anyhow!("probe thread panicked"))?;
    let error = result
        .err()
        .ok_or_else(|| anyhow::anyhow!("invalid facet was accepted"))?;
    anyhow::ensure!(
        format!("{error:#}").contains(expected),
        "wrong failure stage: {error:#}"
    );
    run_facet_probe(&clean_fixture(), input(), config())?;
    Ok(())
}

#[test]
fn denied_import_is_rejected_and_next_invocation_is_clean() -> anyhow::Result<()> {
    rejects_promptly(
        "denied imports",
        wasm(
            r#"(module
            (import "denied" "host-call" (func))
            (memory (export "memory") 32)
            (func (export "facet-render") (param i32 i32 i32) (result i64)
                (i64.const 0)))"#,
        ),
    )
}

#[test]
fn oversized_initial_memory_is_rejected_and_next_invocation_is_clean() -> anyhow::Result<()> {
    rejects_promptly(
        "instantiate isolated facet guest",
        wasm(
            r#"(module
            (memory (export "memory") 100)
            (func (export "facet-render") (param i32 i32 i32) (result i64)
                (i64.const 0)))"#,
        ),
    )
}

#[test]
fn runaway_start_is_bounded() -> anyhow::Result<()> {
    rejects_promptly(
        "instantiate isolated facet guest",
        wasm(
            r#"(module
            (memory (export "memory") 32)
            (func $spin (loop (br 0)))
            (start $spin)
            (func (export "facet-render") (param i32 i32 i32) (result i64)
                (i64.const 0)))"#,
        ),
    )
}

#[test]
fn runaway_render_is_bounded_and_next_invocation_is_clean() -> anyhow::Result<()> {
    rejects_promptly(
        "execute facet for",
        wasm(
            r#"(module
            (memory (export "memory") 32)
            (func (export "facet-render") (param i32 i32 i32) (result i64)
                (loop (br 0)) (i64.const 0)))"#,
        ),
    )
}

#[test]
fn forged_huge_output_length_is_rejected() -> anyhow::Result<()> {
    rejects_promptly(
        "output exceeds the probe output limit",
        wasm(
            r#"(module
            (memory (export "memory") 32)
            (func (export "facet-render") (param i32 i32 i32) (result i64)
                (i64.const -4294967296)))"#,
        ),
    )
}

#[test]
fn repeated_identical_input_is_deterministic() -> anyhow::Result<()> {
    let report = run_facet_probe(&clean_fixture(), input(), config())?;
    anyhow::ensure!(report.timings.len() == 1, "missing invocation timing");
    anyhow::ensure!(
        report
            .timings
            .first()
            .is_some_and(|timing| timing.output_bytes == 0),
        "unexpected output size"
    );
    Ok(())
}

#[test]
fn hidden_mutation_is_rejected_and_next_invocation_is_clean() -> anyhow::Result<()> {
    rejects_promptly(
        "changed on discard/repeat",
        wasm(
            r#"(module
            (memory (export "memory") 32)
            (global $counter (mut i32) (i32.const 0))
            (func (export "facet-render") (param i32 i32 i32) (result i64)
                (global.get $counter)
                (i64.extend_i32_u)
                (global.get $counter)
                (i32.const 1)
                (i32.add)
                (global.set $counter)))"#,
        ),
    )
}
