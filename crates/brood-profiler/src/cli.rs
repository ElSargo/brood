use std::{
    collections::{BTreeMap, HashMap},
    fs::{self, File},
    io::{BufRead, BufReader, BufWriter, Write},
    path::{Path, PathBuf},
};

use crate::{Event, Kind, Value};
use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde::Serialize;

#[derive(Parser)]
#[command(about = "Analyze and convert Brood systemic profiler captures")]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Summarize latency, frame cadence, CPU occupancy, and causal stages.
    Report {
        #[arg(value_name = "CAPTURE_DIR")]
        input: PathBuf,
        #[arg(long, value_name = "FILE")]
        json: Option<PathBuf>,
    },
    /// Convert a capture to Chrome/Perfetto trace-event JSON.
    Chrome {
        #[arg(value_name = "CAPTURE_DIR")]
        input: PathBuf,
        #[arg(short, long, value_name = "FILE")]
        output: PathBuf,
    },
}

#[derive(Serialize)]
struct Distribution {
    samples: usize,
    p999_adequate: bool,
    min_ns: u64,
    mean_ns: u64,
    p50_ns: u64,
    p95_ns: u64,
    p99_ns: u64,
    p999_ns: u64,
    max_ns: u64,
}

#[derive(Serialize)]
struct SpanSummary {
    wall: Distribution,
    cpu: Option<Distribution>,
    #[serde(rename = "non_cpu_wall")]
    non_cpu_wall: Option<Distribution>,
}

#[derive(Default)]
struct SpanSamples {
    wall: Vec<u64>,
    cpu: Vec<u64>,
    non_cpu_wall: Vec<u64>,
}

#[derive(Serialize)]
struct Report {
    schema: u8,
    events: usize,
    truncated_records: usize,
    processes: Vec<String>,
    recorder_dropped: u64,
    complete_flows: usize,
    incomplete_flows: usize,
    input_to_present: Option<Distribution>,
    presented_frame_intervals: Option<Distribution>,
    flow_stages: BTreeMap<String, Distribution>,
    counters: BTreeMap<String, Distribution>,
    spans: BTreeMap<String, SpanSummary>,
}

pub fn run_cli() -> Result<()> {
    let args = Args::parse_from(brood_launch::args());
    match args.command {
        Command::Report { input, json } => {
            let (events, truncated_records) = read_events(&input)?;
            let report = analyze(&events, truncated_records);
            let rendered = serde_json::to_string_pretty(&report)?;
            if let Some(path) = json {
                fs::write(&path, format!("{rendered}\n"))
                    .with_context(|| format!("write {}", path.display()))?;
            }
            println!("{rendered}");
        }
        Command::Chrome { input, output } => {
            let (events, _truncated_records) = read_events(&input)?;
            write_chrome(&events, &output)?;
        }
    }
    Ok(())
}

fn read_events(directory: &Path) -> Result<(Vec<Event>, usize)> {
    let mut paths = fs::read_dir(directory)
        .with_context(|| format!("read capture directory {}", directory.display()))?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "jsonl")
        })
        .collect::<Vec<_>>();
    paths.sort();
    let mut events = Vec::new();
    let mut truncated_records = 0;
    for path in paths {
        let reader =
            BufReader::new(File::open(&path).with_context(|| format!("open {}", path.display()))?);
        let mut lines = reader.lines().peekable();
        let mut line_number = 0;
        while let Some(line) = lines.next() {
            line_number += 1;
            let line = line.with_context(|| format!("read {}:{line_number}", path.display()))?;
            match serde_json::from_str(&line) {
                Ok(event) => events.push(event),
                Err(_error) if lines.peek().is_none() => {
                    truncated_records += 1;
                }
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("parse {}:{line_number}", path.display()));
                }
            }
        }
    }
    events.sort_by_key(|event: &Event| event.ts_ns);
    Ok((events, truncated_records))
}

fn distribution(mut samples: Vec<u64>) -> Distribution {
    samples.sort_unstable();
    let percentile = |parts: u64, scale: u64| {
        if samples.is_empty() {
            return 0;
        }
        let rank = u64::try_from(samples.len())
            .unwrap_or(u64::MAX)
            .saturating_mul(parts)
            .div_ceil(scale)
            .saturating_sub(1);
        samples
            .get(usize::try_from(rank).unwrap_or(usize::MAX))
            .copied()
            .unwrap_or_else(|| samples.last().copied().unwrap_or_default())
    };
    let total = samples.iter().fold(0_u128, |sum, sample| {
        sum.saturating_add(u128::from(*sample))
    });
    Distribution {
        samples: samples.len(),
        p999_adequate: samples.len() >= 1_000,
        min_ns: samples.first().copied().unwrap_or_default(),
        mean_ns: u64::try_from(total / u128::try_from(samples.len().max(1)).unwrap_or(1))
            .unwrap_or(u64::MAX),
        p50_ns: percentile(50, 100),
        p95_ns: percentile(95, 100),
        p99_ns: percentile(99, 100),
        p999_ns: percentile(999, 1_000),
        max_ns: samples.last().copied().unwrap_or_default(),
    }
}

fn analyze(events: &[Event], truncated_records: usize) -> Report {
    let mut processes = events
        .iter()
        .map(|event| format!("{}:{}", event.process, event.pid))
        .collect::<Vec<_>>();
    processes.sort();
    processes.dedup();
    let recorder_dropped = events
        .iter()
        .flat_map(|event| &event.fields)
        .filter_map(|(name, value)| (name.as_ref() == "recorder_dropped").then_some(value))
        .filter_map(|value| match value {
            Value::U64(value) => Some(*value),
            _ => None,
        })
        .sum();

    let mut spans: BTreeMap<String, SpanSamples> = BTreeMap::new();
    for event in events.iter().filter(|event| event.kind == Kind::Span) {
        let wall = event.duration_ns.unwrap_or_default();
        let entry = spans
            .entry(format!("{}/{}", event.category, event.name))
            .or_default();
        entry.wall.push(wall);
        if let Some(cpu) = event.cpu_duration_ns {
            let cpu = cpu.min(wall);
            entry.cpu.push(cpu);
            entry.non_cpu_wall.push(wall.saturating_sub(cpu));
        }
    }

    let mut flows: HashMap<u64, Vec<&Event>> = HashMap::new();
    for event in events
        .iter()
        .filter(|event| matches!(event.kind, Kind::FlowStart | Kind::FlowStep | Kind::FlowEnd))
    {
        if let Some(id) = event.id {
            flows.entry(id).or_default().push(event);
        }
    }
    let mut input_latencies = Vec::new();
    let mut complete_flows = 0;
    let mut incomplete_flows = 0;
    let mut stage_samples: BTreeMap<String, Vec<u64>> = BTreeMap::new();
    for flow in flows.values_mut() {
        flow.sort_by_key(|event| event.ts_ns);
        if flow
            .first()
            .is_some_and(|event| event.kind == Kind::FlowStart)
            && flow.last().is_some_and(|event| event.kind == Kind::FlowEnd)
        {
            complete_flows += 1;
        } else {
            incomplete_flows += 1;
        }
        if let (Some(first), Some(last)) = (flow.first(), flow.last())
            && first.kind == Kind::FlowStart
            && last.kind == Kind::FlowEnd
            && first.category == "input"
            && last.name == "presented"
        {
            input_latencies.push(last.ts_ns.saturating_sub(first.ts_ns));
        }
        for pair in flow.windows(2) {
            let [first, second] = pair else {
                continue;
            };
            stage_samples
                .entry(format!("{} -> {}", first.name, second.name))
                .or_default()
                .push(second.ts_ns.saturating_sub(first.ts_ns));
        }
    }

    let mut presentations: HashMap<u32, Vec<u64>> = HashMap::new();
    for event in events.iter().filter(|event| {
        event.kind == Kind::Instant && event.category == "frame" && event.name == "presented"
    }) {
        presentations
            .entry(event.pid)
            .or_default()
            .push(event.ts_ns);
    }
    let mut frame_intervals = Vec::new();
    for timestamps in presentations.values_mut() {
        timestamps.sort_unstable();
        timestamps.dedup();
        frame_intervals.extend(timestamps.windows(2).filter_map(|pair| {
            let [first, second] = pair else { return None };
            Some(second.saturating_sub(*first))
        }));
    }

    let mut counters: BTreeMap<String, Vec<u64>> = BTreeMap::new();
    for event in events.iter().filter(|event| event.kind == Kind::Counter) {
        let value = event
            .fields
            .iter()
            .find_map(|(name, value)| (name.as_ref() == "value").then_some(value));
        if let Some(Value::U64(value)) = value {
            counters
                .entry(format!("{}/{}", event.category, event.name))
                .or_default()
                .push(*value);
        }
    }

    Report {
        schema: 1,
        events: events.len(),
        truncated_records,
        processes,
        recorder_dropped,
        complete_flows,
        incomplete_flows,
        input_to_present: (!input_latencies.is_empty()).then(|| distribution(input_latencies)),
        presented_frame_intervals: (!frame_intervals.is_empty())
            .then(|| distribution(frame_intervals)),
        flow_stages: stage_samples
            .into_iter()
            .map(|(name, samples)| (name, distribution(samples)))
            .collect(),
        counters: counters
            .into_iter()
            .map(|(name, samples)| (name, distribution(samples)))
            .collect(),
        spans: spans
            .into_iter()
            .map(|(name, samples)| {
                (
                    name,
                    SpanSummary {
                        wall: distribution(samples.wall),
                        cpu: (!samples.cpu.is_empty()).then(|| distribution(samples.cpu)),
                        non_cpu_wall: (!samples.non_cpu_wall.is_empty())
                            .then(|| distribution(samples.non_cpu_wall)),
                    },
                )
            })
            .collect(),
    }
}

fn write_chrome(events: &[Event], path: &Path) -> Result<()> {
    let mut writer =
        BufWriter::new(File::create(path).with_context(|| format!("create {}", path.display()))?);
    writer.write_all(b"{\"traceEvents\":[")?;
    for (index, event) in events.iter().enumerate() {
        if index > 0 {
            writer.write_all(b",")?;
        }
        let phase = match event.kind {
            Kind::Span => "X",
            Kind::Instant | Kind::Counter | Kind::Metadata => "i",
            Kind::FlowStart => "s",
            Kind::FlowStep => "t",
            Kind::FlowEnd => "f",
        };
        let mut object = serde_json::json!({
            "name": event.name, "cat": event.category, "ph": phase,
            "ts": event.ts_ns / 1_000, "pid": event.pid, "tid": event.thread,
            "args": event.fields.iter().map(|(key, value)| (key.as_ref(), value)).collect::<BTreeMap<_, _>>(),
        });
        if let Some(fields) = object.as_object_mut() {
            if let Some(duration) = event.duration_ns {
                fields.insert("dur".to_string(), serde_json::json!(duration / 1_000));
            }
            if let Some(id) = event.id {
                fields.insert("id".to_string(), serde_json::json!(id));
            }
        }
        serde_json::to_writer(&mut writer, &object)?;
    }
    writer.write_all(b"]}\n")?;
    writer.flush()?;
    Ok(())
}
