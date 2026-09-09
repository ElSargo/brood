use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use brood_wasm::snapshot_store::{
    PreparedChunk, PreparedChunkedSnapshot, PreparedCopiedSnapshot, PublicationMeasurement,
    SnapshotLease, SnapshotStore, SnapshotStoreLimits,
};

const REVISIONS: u64 = 500;

struct Sample {
    publication: PublicationMeasurement,
    acquisition: Duration,
    transfer: Duration,
    reclamation: Duration,
}

fn main() -> Result<()> {
    for (name, rows, width) in [
        ("session-list", 32, 128),
        ("terminal-rows", 256, 256),
        ("large-terminal", 1_000, 1_024),
    ] {
        compare(name, rows, width)?;
    }
    Ok(())
}

fn compare(name: &str, rows: usize, width: usize) -> Result<()> {
    let limits = SnapshotStoreLimits {
        max_live_bytes: 8 * 1024 * 1024,
        max_live_allocations: 4_096,
        max_live_roots: 8,
        max_snapshot_bytes: 2 * 1024 * 1024,
        max_chunks_per_snapshot: 1_024,
    };
    let copied = SnapshotStore::new(1, limits);
    let chunked = SnapshotStore::new(2, limits);
    let mut source = vec![vec![0_u8; width]; rows];
    let mut copied_samples = Vec::new();
    let mut chunked_samples = Vec::new();
    let mut copied_pins = VecDeque::new();
    let mut chunked_pins = VecDeque::new();

    for revision in 1..=REVISIONS {
        let changed = usize::try_from(revision)? % rows;
        // Common application work is excluded from both strategies. Both
        // construct a render view of precisely the same changing row state.
        source
            .get_mut(changed)
            .context("changed row missing")?
            .fill(u8::try_from(revision % 251)?);
        let publication = copied.publish_copied(PreparedCopiedSnapshot::build(revision, || {
            let mut bytes = Vec::with_capacity(rows * width);
            for row in &source {
                bytes.extend_from_slice(row);
            }
            bytes
        }))?;
        let (observation, copied_bytes) = sample(&copied, publication, &mut copied_pins)?;
        copied_samples.push(observation);

        let previous = chunked.acquire_latest()?;
        let changed_row = source.get(changed).context("changed row missing")?;
        let publication =
            chunked.publish_chunks(PreparedChunkedSnapshot::build(revision, || {
                previous.as_ref().map_or_else(
                    || source.iter().cloned().map(PreparedChunk::New).collect(),
                    |previous| {
                        previous
                            .immutable_chunks()
                            .into_iter()
                            .enumerate()
                            .map(|(index, chunk)| {
                                if index == changed {
                                    PreparedChunk::New(changed_row.clone())
                                } else {
                                    PreparedChunk::Shared(chunk)
                                }
                            })
                            .collect()
                    },
                )
            }))?;
        drop(previous);
        let (observation, chunked_bytes) = sample(&chunked, publication, &mut chunked_pins)?;
        chunked_samples.push(observation);
        ensure!(
            copied_bytes == chunked_bytes,
            "strategies produced different state at {revision}"
        );
    }

    report(name, "copied", &copied_samples);
    report(name, "immutable-chunks", &chunked_samples);
    copied_pins.clear();
    chunked_pins.clear();
    for store in [&copied, &chunked] {
        store.invalidate();
        let start = Instant::now();
        store.reclaim_retired();
        let retained = store.retention();
        ensure!(
            retained.live_bytes == 0 && retained.live_roots == 0 && retained.live_allocations == 0,
            "teardown retained allocations: {retained:?}"
        );
        println!(
            "shape={name} final_reclamation_ns={}",
            start.elapsed().as_nanos()
        );
    }
    Ok(())
}

fn sample(
    store: &SnapshotStore,
    publication: PublicationMeasurement,
    pins: &mut VecDeque<SnapshotLease>,
) -> Result<(Sample, Vec<u8>)> {
    let start = Instant::now();
    let lease = store.acquire_latest()?.context("published root missing")?;
    let acquisition = start.elapsed();
    let transfer = lease.copy_for_guest()?;
    pins.push_back(lease);
    // A committed frame plus two delayed readers retain historical revisions.
    if pins.len() > 3 {
        drop(pins.pop_front());
    }
    let start = Instant::now();
    store.reclaim_retired();
    let reclamation = start.elapsed();
    Ok((
        Sample {
            publication,
            acquisition,
            transfer: transfer.transfer_time,
            reclamation,
        },
        transfer.bytes,
    ))
}

fn report(shape: &str, strategy: &str, samples: &[Sample]) {
    let allocated: usize = samples
        .iter()
        .map(|s| s.publication.newly_allocated_bytes)
        .sum();
    let peak = samples
        .iter()
        .map(|s| s.publication.live_bytes_after_publication)
        .max()
        .unwrap_or(0);
    println!(
        "shape={shape} strategy={strategy} revisions={} allocated_payload_bytes={allocated} peak_payload_bytes={peak}",
        samples.len()
    );
    for (stage, mut values) in [
        (
            "construction",
            samples
                .iter()
                .map(|s| s.publication.preparation_time)
                .collect::<Vec<_>>(),
        ),
        (
            "assembly",
            samples
                .iter()
                .map(|s| s.publication.assembly_time)
                .collect(),
        ),
        (
            "exchange",
            samples
                .iter()
                .map(|s| s.publication.root_exchange_time)
                .collect(),
        ),
        (
            "acquisition",
            samples.iter().map(|s| s.acquisition).collect(),
        ),
        (
            "transfer_buffer",
            samples.iter().map(|s| s.transfer).collect(),
        ),
        (
            "reclamation",
            samples.iter().map(|s| s.reclamation).collect(),
        ),
    ] {
        values.sort_unstable();
        let quantile = |percent: usize| {
            values
                .get((values.len() * percent).div_ceil(100).saturating_sub(1))
                .copied()
                .unwrap_or_default()
                .as_nanos()
        };
        println!(
            "shape={shape} strategy={strategy} stage={stage} p50_ns={} p95_ns={} p99_ns={} max_ns={}",
            quantile(50),
            quantile(95),
            quantile(99),
            quantile(100)
        );
    }
}
