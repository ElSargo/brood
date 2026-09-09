use std::{
    fs,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use brood_wasm::{
    facet_executor::FacetExecutor,
    facet_runtime::{FacetOperation, FacetRuntimeConfig},
    snapshot_budget::{BudgetedBytes, ByteBudget, ByteBudgetLimits},
};

const fn config() -> FacetRuntimeConfig {
    FacetRuntimeConfig {
        fuel: 100_000,
        epoch_tick_interval: Duration::from_millis(1),
        epoch_deadline_ticks: 20,
        max_memory_bytes: 64 * 1024,
        max_input_bytes: 4096,
        max_output_bytes: 4096,
    }
}

fn budget(generation: u64, bytes: usize, allocations: usize) -> ByteBudget {
    ByteBudget::new(
        generation,
        ByteBudgetLimits {
            max_live_bytes: bytes,
            max_live_allocations: allocations,
            max_buffer_bytes: bytes,
        },
    )
}

fn write_component(text: &str) -> Result<(tempfile::TempDir, PathBuf)> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("facet.component.wat");
    fs::write(&path, text)?;
    Ok((directory, path))
}

fn component(render_body: &str) -> String {
    format!(
        r#"(component
      (core module $m
        (memory (export "memory") 1)
        (func (export "render") (param $ptr i32) (param $len i32) (result i32) {render_body})
        (func (export "interpret") (param $ptr i32) (param $len i32) (result i32)
          i32.const 0 local.get $ptr i32.store i32.const 4 local.get $len i32.store i32.const 0)
        (func (export "realloc") (param i32 i32 i32 i32) (result i32) i32.const 4096))
      (core instance $i (instantiate $m))
      (alias core export $i "memory" (core memory $mem))
      (alias core export $i "render" (core func $render))
      (alias core export $i "interpret" (core func $interpret))
      (alias core export $i "realloc" (core func $realloc))
      (type $f (func (param "input" (list u8)) (result (list u8))))
      (func $render-f (type $f) (canon lift (core func $render) (memory $mem) (realloc $realloc)))
      (func $interpret-f (type $f) (canon lift (core func $interpret) (memory $mem) (realloc $realloc)))
      (instance $facets (export "render" (func $render-f)) (export "interpret" (func $interpret-f)))
      (export "brood:plugin/facets@0.1.0" (instance $facets)))"#
    )
}

async fn wait_until(mut predicate: impl FnMut() -> bool) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(2);
    while !predicate() {
        ensure!(Instant::now() < deadline, "condition timed out");
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    Ok(())
}

#[tokio::test]
async fn context_slot_lives_with_worker_and_missing_export_releases_it() -> Result<()> {
    let (_dir, echo) = write_component(&component(
        "i32.const 0 local.get $ptr i32.store i32.const 4 local.get $len i32.store i32.const 0",
    ))?;
    let (_missing_dir, missing) = write_component("(component)")?;
    let executor = FacetExecutor::new(1, 1)?;
    let worker = executor
        .load(echo, config())
        .await?
        .context("facet missing")?;
    ensure!(executor.active_contexts() == 1);
    ensure!(executor.load(missing.clone(), config()).await.is_err());
    drop(worker);
    wait_until(|| executor.active_contexts() == 0).await?;
    ensure!(executor.load(missing, config()).await?.is_none());
    wait_until(|| executor.active_contexts() == 0).await?;
    Ok(())
}

#[tokio::test]
async fn trap_stops_worker_and_fresh_load_recovers() -> Result<()> {
    let (_dir, path) = write_component(&component("unreachable"))?;
    let executor = FacetExecutor::new(1, 2)?;
    let worker = executor
        .load(path.clone(), config())
        .await?
        .context("facet missing")?;
    let input_budget = budget(1, 16, 1);
    let input = BudgetedBytes::copy_from_slice(&input_budget, b"x")?;
    let result = worker
        .try_invoke(FacetOperation::Render, input, budget(2, 64 * 1024, 1))?
        .await?;
    ensure!(result.is_err());
    wait_until(|| executor.active_contexts() == 0).await?;
    ensure!(
        worker
            .try_invoke(
                FacetOperation::Interpret,
                BudgetedBytes::copy_from_slice(&input_budget, b"x")?,
                budget(3, 64 * 1024, 1)
            )
            .is_err()
    );
    drop(worker);

    let fresh = executor
        .load(path, config())
        .await?
        .context("facet missing")?;
    let input = BudgetedBytes::copy_from_slice(&input_budget, b"ok")?;
    let fresh_outputs = budget(4, 64 * 1024, 1);
    let output = fresh
        .try_invoke(FacetOperation::Interpret, input, fresh_outputs.clone())?
        .await??;
    ensure!(output.bytes()? == b"ok");
    Ok(())
}

#[tokio::test]
async fn queue_and_output_reservations_follow_owned_values() -> Result<()> {
    let (_dir, path) = write_component(&component("(loop $forever (br $forever)) unreachable"))?;
    let executor = FacetExecutor::new(1, 2)?;
    let worker = executor
        .load(path, config())
        .await?
        .context("facet missing")?;
    let inputs = budget(5, 16, 2);
    let first = worker.try_invoke(
        FacetOperation::Render,
        BudgetedBytes::copy_from_slice(&inputs, b"a")?,
        budget(6, 64 * 1024, 1),
    )?;
    let second = worker.try_invoke(
        FacetOperation::Interpret,
        BudgetedBytes::copy_from_slice(&inputs, b"queued")?,
        budget(7, 64 * 1024, 1),
    )?;
    ensure!(inputs.retention().live_bytes == 7 && inputs.retention().live_allocations == 2);
    ensure!(first.await?.is_err());
    ensure!(
        second.await.is_err(),
        "queued call unexpectedly completed after trap"
    );
    wait_until(|| inputs.retention().live_allocations == 0).await?;

    let (_echo_dir, echo) = write_component(&component(
        "i32.const 0 local.get $ptr i32.store i32.const 4 local.get $len i32.store i32.const 0",
    ))?;
    drop(worker);
    wait_until(|| executor.active_contexts() == 0).await?;
    let worker = executor
        .load(echo, config())
        .await?
        .context("facet missing")?;
    let outputs = budget(8, 64 * 1024, 1);
    let echo_inputs = budget(9, 8, 1);
    let input = BudgetedBytes::copy_from_slice(&echo_inputs, b"data")?;
    let output = worker
        .try_invoke(FacetOperation::Render, input, outputs.clone())?
        .await??;
    ensure!(outputs.retention().live_bytes == 64 * 1024);
    ensure!(output.bytes()? == b"data");
    drop(output);
    ensure!(outputs.retention().live_bytes == 0);
    Ok(())
}

#[tokio::test]
async fn bounded_guest_does_not_block_independent_peer() -> Result<()> {
    let (_dir, path) = write_component(&component("(loop $forever (br $forever)) unreachable"))?;
    let executor = FacetExecutor::new(1, 1)?;
    let worker = executor
        .load(path, config())
        .await?
        .context("facet missing")?;
    let progressed = Arc::new(AtomicBool::new(false));
    let peer_progressed = Arc::clone(&progressed);
    let peer = thread::spawn(move || {
        for _ in 0..10_000 {
            std::hint::spin_loop();
        }
        peer_progressed.store(true, Ordering::Release);
    });
    let inputs = budget(10, 8, 1);
    let input = BudgetedBytes::copy_from_slice(&inputs, b"x")?;
    let result = worker
        .try_invoke(FacetOperation::Render, input, budget(11, 64 * 1024, 1))?
        .await?;
    ensure!(result.is_err());
    peer.join()
        .map_err(|_panic_payload| anyhow::anyhow!("peer panicked"))?;
    ensure!(progressed.load(Ordering::Acquire));
    Ok(())
}
