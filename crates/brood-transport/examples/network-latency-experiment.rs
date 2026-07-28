//! Measure small-message RTT over QUIC streams, QUIC datagrams, and isolated UDP.
//!
//! This is an experiment, not a product protocol. The server prints the
//! ephemeral certificate and ports needed by the client so the same binary can
//! be copied to a remote host without installing persistent credentials.

#![allow(
    clippy::cast_precision_loss,
    clippy::indexing_slicing,
    reason = "the experiment operates on fixed-size packets and reports f64 statistics"
)]

use std::{
    env,
    io::{self, Write as _},
    net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail, ensure};
use quinn::{
    AckFrequencyConfig, Connection, Endpoint, RecvStream, SendStream, VarInt,
    crypto::rustls::{QuicClientConfig, QuicServerConfig},
};
use rcgen::{CertifiedKey, generate_simple_self_signed};
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};
use tokio::time::{sleep, timeout};

const PACKET_BYTES: usize = 128;
const SEQUENCE_START: usize = 1;
const SEQUENCE_END: usize = SEQUENCE_START + size_of::<u64>();
const ECHO_TIMEOUT: Duration = Duration::from_secs(1);
const BULK_CHUNK_BYTES: usize = 64 * 1024;

#[tokio::main(worker_threads = 2)]
async fn main() -> Result<()> {
    let arguments = env::args().collect::<Vec<_>>();
    match arguments.get(1).map(String::as_str) {
        Some("server") => run_server().await,
        Some("client") => run_client(&arguments).await,
        _ => bail!(
            "usage: network-latency-experiment server | \
             client HOST QUIC_PORT UDP_PORT CERT_HEX \
             [SAMPLES] [INTERVAL_MS] [TRIAL] [BULK_MIBPS]"
        ),
    }
}

async fn run_server() -> Result<()> {
    let CertifiedKey { cert, signing_key } =
        generate_simple_self_signed(vec!["localhost".to_string()])
            .context("generate ephemeral experiment certificate")?;
    let endpoint = server_endpoint(cert.der().clone(), signing_key.serialize_der())?;
    let quic_port = endpoint
        .local_addr()
        .context("read QUIC listener address")?
        .port();
    let udp = UdpSocket::bind(SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)))
        .context("bind raw UDP echo listener")?;
    let udp_port = udp
        .local_addr()
        .context("read UDP listener address")?
        .port();
    let stopping = Arc::new(AtomicBool::new(false));
    let udp_stopping = Arc::clone(&stopping);
    let udp_thread = thread::Builder::new()
        .name("latency-raw-udp-server".to_string())
        .spawn(move || raw_udp_echo_server(&udp, &udp_stopping))
        .context("spawn raw UDP echo thread")?;

    println!(
        "READY quic_port={quic_port} udp_port={udp_port} cert={}",
        encode_hex(cert.der().as_ref())
    );
    io::stdout().flush().context("flush server readiness")?;

    while let Some(incoming) = endpoint.accept().await {
        tokio::spawn(async move {
            match incoming.await {
                Ok(connection) => serve_quic_connection(connection).await,
                Err(error) => eprintln!("could not accept experiment connection: {error}"),
            }
        });
    }

    stopping.store(true, Ordering::Relaxed);
    udp_thread
        .join()
        .map_err(|_panic| anyhow::anyhow!("raw UDP server thread panicked"))??;
    Ok(())
}

async fn serve_quic_connection(connection: Connection) {
    let stream_connection = connection.clone();
    let stream_task = tokio::spawn(async move {
        let result = async {
            let (send, receive) = stream_connection
                .accept_bi()
                .await
                .context("accept latency echo stream")?;
            stream_echo(send, receive).await
        }
        .await;
        if let Err(error) = result {
            eprintln!("stream echo ended: {error:#}");
        }
    });

    let datagram_connection = connection.clone();
    let datagram_task = tokio::spawn(async move {
        loop {
            let Ok(packet) = datagram_connection.read_datagram().await else {
                break;
            };
            if datagram_connection.send_datagram(packet).is_err() {
                break;
            }
        }
    });

    let bulk_connection = connection.clone();
    let bulk_task = tokio::spawn(async move {
        loop {
            let Ok(mut receive) = bulk_connection.accept_uni().await else {
                break;
            };
            tokio::spawn(async move {
                let _bytes = receive.read_to_end(usize::MAX).await;
            });
        }
    });

    let _closed = connection.closed().await;
    stream_task.abort();
    datagram_task.abort();
    bulk_task.abort();
}

async fn stream_echo(mut send: SendStream, mut receive: RecvStream) -> Result<()> {
    let mut packet = [0_u8; PACKET_BYTES];
    loop {
        match receive.read_exact(&mut packet).await {
            Ok(()) => send
                .write_all(&packet)
                .await
                .context("echo latency stream packet")?,
            Err(quinn::ReadExactError::FinishedEarly(_)) => return Ok(()),
            Err(error) => return Err(error).context("read latency stream packet"),
        }
    }
}

fn raw_udp_echo_server(socket: &UdpSocket, stopping: &AtomicBool) -> Result<()> {
    socket
        .set_read_timeout(Some(Duration::from_millis(200)))
        .context("set raw UDP server timeout")?;
    let mut packet = [0_u8; 2_048];
    while !stopping.load(Ordering::Relaxed) {
        match socket.recv_from(&mut packet) {
            Ok((length, peer)) => {
                socket
                    .send_to(&packet[..length], peer)
                    .context("echo raw UDP packet")?;
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) => {}
            Err(error) => return Err(error).context("receive raw UDP packet"),
        }
    }
    Ok(())
}

async fn run_client(arguments: &[String]) -> Result<()> {
    ensure!(
        arguments.len() >= 6,
        "client requires HOST QUIC_PORT UDP_PORT CERT_HEX"
    );
    let host = arguments[2]
        .parse::<IpAddr>()
        .context("parse experiment host IP")?;
    let quic_port = arguments[3].parse::<u16>().context("parse QUIC port")?;
    let udp_port = arguments[4].parse::<u16>().context("parse UDP port")?;
    let certificate = decode_hex(&arguments[5]).context("decode server certificate")?;
    let samples = optional_number(arguments, 6, 400_usize)?;
    let interval_ms = optional_number(arguments, 7, 10_u64)?;
    let trial = arguments.get(8).map_or("1", String::as_str);
    let bulk_mibps = optional_number(arguments, 9, 8.0_f64)?;
    ensure!(
        bulk_mibps >= 0.0 && bulk_mibps.is_finite(),
        "bulk rate must be a finite non-negative MiB/s value"
    );
    let interval = Duration::from_millis(interval_ms);

    let endpoint = client_endpoint(CertificateDer::from(certificate))?;
    let connection = endpoint
        .connect(SocketAddr::new(host, quic_port), "localhost")
        .context("start experiment QUIC connection")?
        .await
        .context("connect experiment QUIC endpoint")?;
    ensure!(
        connection.max_datagram_size().is_some(),
        "peer did not enable QUIC DATAGRAM"
    );
    let (mut stream_send, mut stream_receive) = connection
        .open_bi()
        .await
        .context("open latency echo stream")?;

    let raw_peer = SocketAddr::new(host, udp_port);
    let (stream_idle, datagram_idle, raw_idle) = measure_all(
        &mut stream_send,
        &mut stream_receive,
        &connection,
        raw_peer,
        samples,
        interval,
    )
    .await?;
    report(trial, "quic-stream", "idle", &stream_idle);
    report(trial, "quic-datagram", "idle", &datagram_idle);
    report(trial, "raw-udp-thread", "idle", &raw_idle);

    let bulk_bytes = Arc::new(AtomicU64::new(0));
    let bulk_task =
        start_bulk_sender(connection.clone(), Arc::clone(&bulk_bytes), bulk_mibps).await?;
    sleep(Duration::from_millis(250)).await;
    let bulk_started = Instant::now();

    let (stream_bulk, datagram_bulk, raw_bulk) = measure_all(
        &mut stream_send,
        &mut stream_receive,
        &connection,
        raw_peer,
        samples,
        interval,
    )
    .await?;
    report(trial, "quic-stream", "bulk", &stream_bulk);
    report(trial, "quic-datagram", "bulk", &datagram_bulk);
    report(trial, "raw-udp-thread", "bulk", &raw_bulk);

    let bulk_elapsed = bulk_started.elapsed();
    bulk_task.abort();
    let sent = bulk_bytes.load(Ordering::Relaxed);
    let mib_per_second = sent as f64 / bulk_elapsed.as_secs_f64() / (1024.0 * 1024.0);
    println!(
        "BULK trial={trial} target_mib_per_second={bulk_mibps:.3} bytes={sent} \
         elapsed_ms={} mib_per_second={mib_per_second:.3}",
        bulk_elapsed.as_millis()
    );
    connection.close(VarInt::from_u32(0), b"experiment complete");
    endpoint.wait_idle().await;
    Ok(())
}

async fn measure_all(
    stream_send: &mut SendStream,
    stream_receive: &mut RecvStream,
    connection: &Connection,
    raw_peer: SocketAddr,
    samples: usize,
    interval: Duration,
) -> Result<(Measurements, Measurements, Measurements)> {
    let raw_task =
        tokio::task::spawn_blocking(move || measure_raw_udp(raw_peer, samples, interval));
    let (stream, datagram, raw) = tokio::join!(
        measure_stream(stream_send, stream_receive, samples, interval),
        measure_datagram(connection, samples, interval),
        raw_task
    );
    Ok((
        stream?,
        datagram?,
        raw.context("raw UDP experiment thread panicked")??,
    ))
}

async fn measure_stream(
    send: &mut SendStream,
    receive: &mut RecvStream,
    samples: usize,
    interval: Duration,
) -> Result<Measurements> {
    let mut measurements = Measurements::default();
    let mut response = [0_u8; PACKET_BYTES];
    for sequence in 0..samples {
        let packet = packet(1, sequence as u64);
        let started = Instant::now();
        let echoed = timeout(ECHO_TIMEOUT, async {
            send.write_all(&packet)
                .await
                .context("send QUIC stream probe")?;
            receive
                .read_exact(&mut response)
                .await
                .context("receive QUIC stream probe")
        })
        .await;
        if let Ok(result) = echoed {
            result?;
        } else {
            measurements.lost = measurements
                .lost
                .saturating_add(samples.saturating_sub(sequence));
            break;
        }
        ensure!(
            packet_sequence(&response) == sequence as u64,
            "QUIC stream echo sequence mismatch"
        );
        measurements.push(started.elapsed());
        sleep(interval).await;
    }
    Ok(measurements)
}

async fn measure_datagram(
    connection: &Connection,
    samples: usize,
    interval: Duration,
) -> Result<Measurements> {
    let mut measurements = Measurements::default();
    let mut consecutive_losses = 0_usize;
    for sequence in 0..samples {
        let expected = sequence as u64;
        let started = Instant::now();
        connection
            .send_datagram(packet(2, expected).to_vec().into())
            .context("send QUIC datagram probe")?;
        let deadline = Instant::now() + ECHO_TIMEOUT;
        let received = loop {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                break false;
            };
            let response = match timeout(remaining, connection.read_datagram()).await {
                Ok(Ok(response)) => response,
                Ok(Err(error)) => return Err(error).context("receive QUIC datagram probe"),
                Err(_) => break false,
            };
            if response.first() == Some(&2) && packet_sequence(&response) == expected {
                break true;
            }
        };
        if received {
            measurements.push(started.elapsed());
            consecutive_losses = 0;
        } else {
            measurements.lost = measurements.lost.saturating_add(1);
            consecutive_losses = consecutive_losses.saturating_add(1);
            if consecutive_losses >= 3 {
                measurements.lost = measurements
                    .lost
                    .saturating_add(samples.saturating_sub(sequence + 1));
                break;
            }
        }
        sleep(interval).await;
    }
    Ok(measurements)
}

fn measure_raw_udp(peer: SocketAddr, samples: usize, interval: Duration) -> Result<Measurements> {
    let socket = UdpSocket::bind(SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)))
        .context("bind raw UDP client")?;
    socket.connect(peer).context("connect raw UDP client")?;
    socket
        .set_read_timeout(Some(ECHO_TIMEOUT))
        .context("set raw UDP client timeout")?;
    let mut measurements = Measurements::default();
    let mut response = [0_u8; 2_048];
    let mut consecutive_losses = 0_usize;
    for sequence in 0..samples {
        let expected = sequence as u64;
        let probe = packet(3, expected);
        let started = Instant::now();
        socket.send(&probe).context("send raw UDP probe")?;
        let received = loop {
            match socket.recv(&mut response) {
                Ok(length)
                    if response.first() == Some(&3)
                        && length >= SEQUENCE_END
                        && packet_sequence(&response[..length]) == expected =>
                {
                    break true;
                }
                Ok(_) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    break false;
                }
                Err(error) => return Err(error).context("receive raw UDP probe"),
            }
        };
        if received {
            measurements.push(started.elapsed());
            consecutive_losses = 0;
        } else {
            measurements.lost = measurements.lost.saturating_add(1);
            consecutive_losses = consecutive_losses.saturating_add(1);
            if consecutive_losses >= 3 {
                measurements.lost = measurements
                    .lost
                    .saturating_add(samples.saturating_sub(sequence + 1));
                break;
            }
        }
        thread::sleep(interval);
    }
    Ok(measurements)
}

async fn start_bulk_sender(
    connection: Connection,
    bytes_sent: Arc<AtomicU64>,
    mib_per_second: f64,
) -> Result<tokio::task::JoinHandle<()>> {
    let mut send = connection
        .open_uni()
        .await
        .context("open experiment bulk stream")?;
    send.set_priority(-20)
        .context("set experiment bulk stream priority")?;
    Ok(tokio::spawn(async move {
        let chunk = vec![0x5a; BULK_CHUNK_BYTES];
        let interval = if mib_per_second == 0.0 {
            Duration::ZERO
        } else {
            Duration::from_secs_f64(BULK_CHUNK_BYTES as f64 / (mib_per_second * 1024.0 * 1024.0))
        };
        while send.write_all(&chunk).await.is_ok() {
            bytes_sent.fetch_add(BULK_CHUNK_BYTES as u64, Ordering::Relaxed);
            if !interval.is_zero() {
                sleep(interval).await;
            }
        }
    }))
}

#[derive(Default)]
struct Measurements {
    microseconds: Vec<u64>,
    lost: usize,
}

impl Measurements {
    fn push(&mut self, elapsed: Duration) {
        self.microseconds
            .push(u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX));
    }
}

fn report(trial: &str, carrier: &str, load: &str, measurements: &Measurements) {
    let mut sorted = measurements.microseconds.clone();
    sorted.sort_unstable();
    let count = sorted.len();
    let sum = sorted.iter().map(|value| *value as f64).sum::<f64>();
    let mean = if count == 0 { 0.0 } else { sum / count as f64 };
    let variance = if count == 0 {
        0.0
    } else {
        sorted
            .iter()
            .map(|value| {
                let difference = *value as f64 - mean;
                difference * difference
            })
            .sum::<f64>()
            / count as f64
    };
    println!(
        "RESULT trial={trial} carrier={carrier} load={load} samples={count} lost={} \
         mean_us={mean:.3} p50_us={} p95_us={} p99_us={} max_us={} stddev_us={:.3}",
        measurements.lost,
        percentile(&sorted, 50),
        percentile(&sorted, 95),
        percentile(&sorted, 99),
        sorted.last().copied().unwrap_or(0),
        variance.sqrt()
    );
}

fn percentile(sorted: &[u64], percentile: usize) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = percentile.saturating_mul(sorted.len()).saturating_add(99) / 100;
    sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
}

fn packet(marker: u8, sequence: u64) -> [u8; PACKET_BYTES] {
    let mut packet = [0xa5; PACKET_BYTES];
    packet[0] = marker;
    packet[SEQUENCE_START..SEQUENCE_END].copy_from_slice(&sequence.to_be_bytes());
    packet
}

fn packet_sequence(packet: &[u8]) -> u64 {
    if packet.len() < SEQUENCE_END {
        return u64::MAX;
    }
    let mut encoded = [0_u8; size_of::<u64>()];
    encoded.copy_from_slice(&packet[SEQUENCE_START..SEQUENCE_END]);
    u64::from_be_bytes(encoded)
}

fn optional_number<T>(arguments: &[String], index: usize, fallback: T) -> Result<T>
where
    T: std::str::FromStr,
    T::Err: std::error::Error + Send + Sync + 'static,
{
    arguments
        .get(index)
        .map_or(Ok(fallback), |value| value.parse::<T>().map_err(Into::into))
}

fn server_endpoint(certificate: CertificateDer<'static>, private_key: Vec<u8>) -> Result<Endpoint> {
    let tls = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(
            vec![certificate],
            PrivatePkcs8KeyDer::from(private_key).into(),
        )
        .context("configure experiment QUIC server identity")?;
    let mut config = quinn::ServerConfig::with_crypto(Arc::new(
        QuicServerConfig::try_from(tls).context("configure experiment QUIC server TLS")?,
    ));
    config.transport_config(Arc::new(transport_config()));
    Endpoint::server(config, SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)))
        .context("bind experiment QUIC server")
}

fn client_endpoint(certificate: CertificateDer<'static>) -> Result<Endpoint> {
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(certificate)
        .context("trust ephemeral experiment certificate")?;
    let tls = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let mut config = quinn::ClientConfig::new(Arc::new(
        QuicClientConfig::try_from(tls).context("configure experiment QUIC client TLS")?,
    ));
    config.transport_config(Arc::new(transport_config()));
    let mut endpoint = Endpoint::client(SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)))
        .context("bind experiment QUIC client")?;
    endpoint.set_default_client_config(config);
    Ok(endpoint)
}

fn transport_config() -> quinn::TransportConfig {
    let mut config = quinn::TransportConfig::default();
    config.keep_alive_interval(Some(Duration::from_secs(10)));
    config.datagram_receive_buffer_size(Some(4 * 1024 * 1024));
    config.datagram_send_buffer_size(4 * 1024 * 1024);
    let mut acknowledgements = AckFrequencyConfig::default();
    acknowledgements
        .ack_eliciting_threshold(VarInt::from_u32(0))
        .max_ack_delay(Some(Duration::from_millis(2)));
    config.ack_frequency_config(Some(acknowledgements));
    config
}

fn encode_hex(bytes: &[u8]) -> String {
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _result = write!(encoded, "{byte:02x}");
    }
    encoded
}

fn decode_hex(encoded: &str) -> Result<Vec<u8>> {
    ensure!(
        encoded.len().is_multiple_of(2),
        "certificate hex has odd length"
    );
    encoded
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let digits = std::str::from_utf8(pair).context("certificate hex is not UTF-8")?;
            u8::from_str_radix(digits, 16).context("certificate contains non-hex digits")
        })
        .collect()
}
