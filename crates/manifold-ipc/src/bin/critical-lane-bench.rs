use std::{
    hint::spin_loop,
    io::{Read, Write},
    os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, RawFd},
    os::unix::net::UnixStream,
    process::{Child, Command},
    sync::mpsc,
    thread,
};

use anyhow::{Context, Result, bail, ensure};
use clap::{Parser, ValueEnum};
use manifold_ipc::{CriticalConsumer, CriticalLaneFile, CriticalProducer, CriticalRecord};

const GENERATION: u64 = 1;
const OPCODE_PING: u32 = 1;
const OPCODE_PONG: u32 = 2;

#[derive(Debug, Parser)]
#[command(about = "Compare Manifold critical-lane latency with Rust mpsc and Unix sockets")]
struct Args {
    /// Measured ping/pong operations per transport.
    #[arg(long, default_value_t = 200_000)]
    iterations: usize,
    /// Unmeasured operations used to reach steady state.
    #[arg(long, default_value_t = 20_000)]
    warmup: usize,
    #[arg(long, hide = true)]
    worker: Option<WorkerKind>,
    #[arg(long, hide = true)]
    request_fd: Option<RawFd>,
    #[arg(long, hide = true)]
    response_fd: Option<RawFd>,
    #[arg(long, hide = true)]
    operations: Option<usize>,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum WorkerKind {
    SharedMemory,
    UnixSocket,
}

#[derive(Debug)]
struct Measurement {
    name: &'static str,
    one_way_ns: Vec<u64>,
    round_trip_ns: Vec<u64>,
}

#[derive(Debug)]
struct Summary {
    name: &'static str,
    one_way_p50: u64,
    one_way_p95: u64,
    one_way_p99: u64,
    one_way_mean: u64,
    round_trip_p50: u64,
    round_trip_p95: u64,
    round_trip_p99: u64,
    round_trip_mean: u64,
    operations_per_second: u128,
}

fn main() -> Result<()> {
    let args = Args::parse();
    if let Some(worker) = args.worker {
        return run_worker(worker, &args);
    }
    ensure!(args.iterations > 0, "iterations must be nonzero");
    let operations = args
        .warmup
        .checked_add(args.iterations)
        .context("operation count overflow")?;

    eprintln!(
        "critical-lane benchmark: iterations={} warmup={} record_bytes={} capacity={}",
        args.iterations,
        args.warmup,
        std::mem::size_of::<CriticalRecord>(),
        manifold_ipc::CRITICAL_CAPACITY
    );

    let measurements = [
        benchmark_channel(args.warmup, args.iterations, operations)?,
        benchmark_unix_socket(args.warmup, args.iterations, operations)?,
        benchmark_shared_memory(args.warmup, args.iterations, operations)?,
    ];

    println!(
        "{:<20} {:>10} {:>10} {:>10} {:>10} | {:>10} {:>10} {:>10} {:>10} | {:>12}",
        "transport",
        "one-p50",
        "one-p95",
        "one-p99",
        "one-mean",
        "rtt-p50",
        "rtt-p95",
        "rtt-p99",
        "rtt-mean",
        "ping-pong/s"
    );
    println!("{}", "-".repeat(143));
    for measurement in measurements {
        let summary = summarize(measurement)?;
        println!(
            "{:<20} {:>10} {:>10} {:>10} {:>10} | {:>10} {:>10} {:>10} {:>10} | {:>12}",
            summary.name,
            summary.one_way_p50,
            summary.one_way_p95,
            summary.one_way_p99,
            summary.one_way_mean,
            summary.round_trip_p50,
            summary.round_trip_p95,
            summary.round_trip_p99,
            summary.round_trip_mean,
            summary.operations_per_second
        );
    }
    println!("latencies are nanoseconds; one-way uses the device-wide CLOCK_MONOTONIC domain");
    Ok(())
}

fn benchmark_channel(warmup: usize, iterations: usize, operations: usize) -> Result<Measurement> {
    let (request_tx, request_rx) = mpsc::channel::<CriticalRecord>();
    let (response_tx, response_rx) = mpsc::channel::<CriticalRecord>();
    let worker = thread::Builder::new()
        .name("critical-channel-bench".to_string())
        .spawn(move || -> Result<()> {
            for expected in 0..operations {
                let request = request_rx.recv().context("channel request closed")?;
                let received = monotonic_ns()?;
                validate_request(&request, expected)?;
                response_tx
                    .send(response_record(&request, received))
                    .context("channel response closed")?;
            }
            Ok(())
        })
        .context("spawn channel benchmark worker")?;

    let mut measurement = Measurement::new("std-mpsc", iterations);
    for sequence in 0..operations {
        let sent = monotonic_ns()?;
        request_tx
            .send(request_record(sequence, sent)?)
            .context("send channel request")?;
        let response = response_rx.recv().context("receive channel response")?;
        let returned = monotonic_ns()?;
        record_sample(&mut measurement, warmup, sequence, &response, returned)?;
    }
    worker
        .join()
        .map_err(|_panic| anyhow::anyhow!("channel benchmark worker panicked"))??;
    Ok(measurement)
}

fn benchmark_shared_memory(
    warmup: usize,
    iterations: usize,
    operations: usize,
) -> Result<Measurement> {
    let requests = CriticalLaneFile::create(GENERATION)?;
    let responses = CriticalLaneFile::create(GENERATION)?;
    let mut child = spawn_worker(
        WorkerKind::SharedMemory,
        requests.as_fd().as_raw_fd(),
        Some(responses.as_fd().as_raw_fd()),
        operations,
    )?;
    let mut producer = CriticalProducer::map(requests.as_fd(), GENERATION)?;
    let mut consumer = CriticalConsumer::map(responses.as_fd(), GENERATION)?;
    let mut measurement = Measurement::new("shared-memory-spin", iterations);

    for sequence in 0..operations {
        let sent = monotonic_ns()?;
        let request = request_record(sequence, sent)?;
        send_spinning(&mut producer, request);
        let response = consumer.recv_spinning();
        let returned = monotonic_ns()?;
        record_sample(&mut measurement, warmup, sequence, &response, returned)?;
    }
    wait_worker(&mut child, "shared-memory")?;
    Ok(measurement)
}

fn benchmark_unix_socket(
    warmup: usize,
    iterations: usize,
    operations: usize,
) -> Result<Measurement> {
    let (mut parent, child_socket) = UnixStream::pair().context("create Unix socket pair")?;
    set_close_on_exec(child_socket.as_raw_fd(), false)?;
    let mut child = spawn_worker(
        WorkerKind::UnixSocket,
        child_socket.as_raw_fd(),
        None,
        operations,
    )?;
    set_close_on_exec(child_socket.as_raw_fd(), true)?;
    drop(child_socket);

    let mut measurement = Measurement::new("unix-stream", iterations);
    for sequence in 0..operations {
        let sent = monotonic_ns()?;
        write_record(&mut parent, &request_record(sequence, sent)?)?;
        let response = read_record(&mut parent)?;
        let returned = monotonic_ns()?;
        record_sample(&mut measurement, warmup, sequence, &response, returned)?;
    }
    wait_worker(&mut child, "Unix socket")?;
    Ok(measurement)
}

fn run_worker(worker: WorkerKind, args: &Args) -> Result<()> {
    let request_fd = args.request_fd.context("worker requires --request-fd")?;
    let operations = args.operations.context("worker requires --operations")?;
    match worker {
        WorkerKind::SharedMemory => {
            let response_fd = args
                .response_fd
                .context("shared worker requires --response-fd")?;
            // SAFETY: descriptors are inherited from the parent and remain open
            // for the entire worker invocation.
            let request_fd = unsafe { BorrowedFd::borrow_raw(request_fd) };
            // SAFETY: same inheritance contract as the request descriptor.
            let response_fd = unsafe { BorrowedFd::borrow_raw(response_fd) };
            let mut consumer = CriticalConsumer::map(request_fd, GENERATION)?;
            let mut producer = CriticalProducer::map(response_fd, GENERATION)?;
            for expected in 0..operations {
                let request = consumer.recv_spinning();
                let received = monotonic_ns()?;
                validate_request(&request, expected)?;
                send_spinning(&mut producer, response_record(&request, received));
            }
            Ok(())
        }
        WorkerKind::UnixSocket => {
            // SAFETY: the parent explicitly cleared `FD_CLOEXEC` on this end and
            // transferred ownership to the worker process.
            let mut stream = unsafe { UnixStream::from_raw_fd(request_fd) };
            for expected in 0..operations {
                let request = read_record(&mut stream)?;
                let received = monotonic_ns()?;
                validate_request(&request, expected)?;
                write_record(&mut stream, &response_record(&request, received))?;
            }
            Ok(())
        }
    }
}

fn spawn_worker(
    kind: WorkerKind,
    request_fd: RawFd,
    response_fd: Option<RawFd>,
    operations: usize,
) -> Result<Child> {
    let executable = std::env::current_exe().context("resolve benchmark executable")?;
    let mut command = Command::new(executable);
    command
        .arg("--worker")
        .arg(match kind {
            WorkerKind::SharedMemory => "shared-memory",
            WorkerKind::UnixSocket => "unix-socket",
        })
        .arg("--request-fd")
        .arg(request_fd.to_string())
        .arg("--operations")
        .arg(operations.to_string());
    if let Some(fd) = response_fd {
        command.arg("--response-fd").arg(fd.to_string());
    }
    command.spawn().context("spawn benchmark worker")
}

fn send_spinning(producer: &mut CriticalProducer, record: CriticalRecord) {
    loop {
        if producer.try_send(record).is_ok() {
            return;
        }
        spin_loop();
    }
}

fn request_record(sequence: usize, sent: u64) -> Result<CriticalRecord> {
    Ok(CriticalRecord {
        opcode: OPCODE_PING,
        connection_generation: GENERATION,
        sequence: u64::try_from(sequence).context("sequence exceeds u64")?,
        arguments: [sent, 0, 0, 0, 0],
        ..CriticalRecord::default()
    })
}

fn response_record(request: &CriticalRecord, received: u64) -> CriticalRecord {
    CriticalRecord {
        opcode: OPCODE_PONG,
        connection_generation: request.connection_generation,
        sequence: request.sequence,
        arguments: [
            request.arguments.first().copied().unwrap_or(0),
            received,
            0,
            0,
            0,
        ],
        ..CriticalRecord::default()
    }
}

fn validate_request(request: &CriticalRecord, expected: usize) -> Result<()> {
    ensure!(request.opcode == OPCODE_PING, "unexpected request opcode");
    ensure!(
        request.connection_generation == GENERATION,
        "unexpected request generation"
    );
    ensure!(
        request.sequence == u64::try_from(expected).context("expected sequence exceeds u64")?,
        "request sequence gap"
    );
    Ok(())
}

fn record_sample(
    measurement: &mut Measurement,
    warmup: usize,
    sequence: usize,
    response: &CriticalRecord,
    returned: u64,
) -> Result<()> {
    ensure!(response.opcode == OPCODE_PONG, "unexpected response opcode");
    ensure!(
        response.sequence == u64::try_from(sequence).context("response sequence exceeds u64")?,
        "response sequence gap"
    );
    let sent = response
        .arguments
        .first()
        .copied()
        .context("missing send timestamp")?;
    let received = response
        .arguments
        .get(1)
        .copied()
        .context("missing receive timestamp")?;
    ensure!(received >= sent, "non-monotonic one-way timestamps");
    ensure!(returned >= sent, "non-monotonic round-trip timestamps");
    if sequence >= warmup {
        measurement.one_way_ns.push(received - sent);
        measurement.round_trip_ns.push(returned - sent);
    }
    Ok(())
}

impl Measurement {
    fn new(name: &'static str, capacity: usize) -> Self {
        Self {
            name,
            one_way_ns: Vec::with_capacity(capacity),
            round_trip_ns: Vec::with_capacity(capacity),
        }
    }
}

fn summarize(mut measurement: Measurement) -> Result<Summary> {
    measurement.one_way_ns.sort_unstable();
    measurement.round_trip_ns.sort_unstable();
    ensure!(!measurement.one_way_ns.is_empty(), "no one-way samples");
    ensure!(
        !measurement.round_trip_ns.is_empty(),
        "no round-trip samples"
    );
    let round_trip_total = measurement
        .round_trip_ns
        .iter()
        .map(|value| u128::from(*value))
        .sum::<u128>();
    let count = u128::try_from(measurement.round_trip_ns.len()).context("sample count overflow")?;
    let operations_per_second = count
        .checked_mul(1_000_000_000)
        .context("throughput numerator overflow")?
        / round_trip_total.max(1);
    Ok(Summary {
        name: measurement.name,
        one_way_p50: percentile(&measurement.one_way_ns, 50)?,
        one_way_p95: percentile(&measurement.one_way_ns, 95)?,
        one_way_p99: percentile(&measurement.one_way_ns, 99)?,
        one_way_mean: integer_mean(&measurement.one_way_ns)?,
        round_trip_p50: percentile(&measurement.round_trip_ns, 50)?,
        round_trip_p95: percentile(&measurement.round_trip_ns, 95)?,
        round_trip_p99: percentile(&measurement.round_trip_ns, 99)?,
        round_trip_mean: integer_mean(&measurement.round_trip_ns)?,
        operations_per_second,
    })
}

fn percentile(sorted: &[u64], percentile: usize) -> Result<u64> {
    ensure!(!sorted.is_empty(), "cannot select from empty samples");
    let index = sorted
        .len()
        .saturating_sub(1)
        .checked_mul(percentile)
        .context("percentile index overflow")?
        / 100;
    sorted
        .get(index)
        .copied()
        .context("percentile index outside samples")
}

fn integer_mean(values: &[u64]) -> Result<u64> {
    ensure!(!values.is_empty(), "cannot average empty samples");
    let total = values.iter().map(|value| u128::from(*value)).sum::<u128>();
    let count = u128::try_from(values.len()).context("sample count overflow")?;
    u64::try_from(total / count).context("mean exceeds u64")
}

fn monotonic_ns() -> Result<u64> {
    let mut timestamp = std::mem::MaybeUninit::<libc::timespec>::uninit();
    // SAFETY: the pointer references writable storage for one `timespec`.
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, timestamp.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error()).context("read CLOCK_MONOTONIC");
    }
    // SAFETY: successful `clock_gettime` initialized the timestamp.
    let timestamp = unsafe { timestamp.assume_init() };
    let seconds = u64::try_from(timestamp.tv_sec).context("negative monotonic seconds")?;
    let nanoseconds = u64::try_from(timestamp.tv_nsec).context("negative monotonic nanoseconds")?;
    seconds
        .checked_mul(1_000_000_000)
        .and_then(|value| value.checked_add(nanoseconds))
        .context("monotonic timestamp overflow")
}

fn write_record(stream: &mut UnixStream, record: &CriticalRecord) -> Result<()> {
    // SAFETY: `CriticalRecord` is a fully initialized plain fixed-layout value;
    // the byte slice is read-only and exactly covers that value.
    let bytes = unsafe {
        std::slice::from_raw_parts(
            std::ptr::from_ref(record).cast::<u8>(),
            std::mem::size_of::<CriticalRecord>(),
        )
    };
    stream.write_all(bytes).context("write Unix socket record")
}

fn read_record(stream: &mut UnixStream) -> Result<CriticalRecord> {
    let mut record = CriticalRecord::default();
    // SAFETY: `record` is valid writable storage and the byte slice exactly
    // covers it. Every bit pattern is valid for the integer-only record.
    let bytes = unsafe {
        std::slice::from_raw_parts_mut(
            std::ptr::from_mut(&mut record).cast::<u8>(),
            std::mem::size_of::<CriticalRecord>(),
        )
    };
    stream
        .read_exact(bytes)
        .context("read Unix socket record")?;
    Ok(record)
}

fn set_close_on_exec(fd: RawFd, enabled: bool) -> Result<()> {
    // SAFETY: `fd` is a live socket descriptor and `F_GETFD` takes no pointer.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error()).context("read descriptor flags");
    }
    let updated = if enabled {
        flags | libc::FD_CLOEXEC
    } else {
        flags & !libc::FD_CLOEXEC
    };
    // SAFETY: `updated` contains descriptor flags returned by `F_GETFD`.
    if unsafe { libc::fcntl(fd, libc::F_SETFD, updated) } < 0 {
        return Err(std::io::Error::last_os_error()).context("set descriptor flags");
    }
    Ok(())
}

fn wait_worker(child: &mut Child, name: &str) -> Result<()> {
    let status = child
        .wait()
        .with_context(|| format!("wait for {name} worker"))?;
    if !status.success() {
        bail!("{name} worker exited with {status}");
    }
    Ok(())
}
