wit_bindgen::generate!({
    path: "../../crates/manifold-wasm/wit",
    world: "manifold-plugin",
});

use std::sync::atomic::{AtomicU64, Ordering};

use exports::manifold::plugin::actor::{Command, Event, Guest, WorkerRequest};
use manifold::plugin::host;

static INPUT_NOTIFICATION: AtomicU64 = AtomicU64::new(0);
static RESULT_NOTIFICATION: AtomicU64 = AtomicU64::new(0);
static CANCEL_NOTIFICATION: AtomicU64 = AtomicU64::new(0);
static INPUT_GENERATION: AtomicU64 = AtomicU64::new(0);
static RESULT_GENERATION: AtomicU64 = AtomicU64::new(0);
static SHARED_VALUE: AtomicU64 = AtomicU64::new(0);

const RUBE_MESSAGE_MAGIC: &[u8; 8] = b"RUBEIO01";
const RUBE_ACK_MAGIC: &[u8; 8] = b"RUBEACK1";
const RUBE_WORKER_BASE: u64 = 1_u64 << 40;

struct ExampleActor;

impl Guest for ExampleActor {
    fn dispatch(event: Event) -> Vec<Command> {
        if matches!(
            &event,
            Event::Mailbox(message) if message.body == b"spin-dispatch"
        ) {
            // The host's epoch scheduler must yield this otherwise
            // non-cooperative dispatcher so its wall-clock deadline can fire.
            loop {
                core::hint::spin_loop();
            }
        }
        match event {
            Event::Mailbox(message) if message.body.starts_with(RUBE_MESSAGE_MAGIC) => {
                let Some((transition, input, generation, claimed)) =
                    parse_rube_message(&message.body)
                else {
                    return Vec::new();
                };
                let actual = rube_mix(input, transition, generation);
                let mut payload = RUBE_ACK_MAGIC.to_vec();
                payload.extend_from_slice(&actual.to_le_bytes());
                // A malformed claimed checksum remains externally visible as
                // the independently computed value, so the host oracle fails
                // the exact transition instead of silently accepting it.
                if claimed != actual {
                    payload.extend_from_slice(b"bad-claim");
                }
                vec![Command::SpawnWorker(WorkerRequest {
                    worker: RUBE_WORKER_BASE.saturating_add(transition),
                    payload,
                })]
            }
            Event::Mailbox(message) if message.body == b"spawn-worker" => {
                vec![Command::SpawnWorker(WorkerRequest {
                    worker: 1,
                    payload: b"input.txt".to_vec(),
                })]
            }
            Event::Mailbox(message) if message.body == b"completion-read" => {
                // Submission is nonblocking. The typed result returns later as
                // Event::Completion on the actor-private completion lane.
                let _operation = host::submit_read("completion.txt", 0, 1024);
                Vec::new()
            }
            Event::Mailbox(message) if message.body == b"spawn-duplicate-workers" => vec![
                Command::SpawnWorker(WorkerRequest {
                    worker: 700,
                    payload: b"duplicate.pipe".to_vec(),
                }),
                Command::SpawnWorker(WorkerRequest {
                    worker: 700,
                    payload: b"duplicate.pipe".to_vec(),
                }),
            ],
            Event::Completion(exports::manifold::plugin::actor::Completion::Read(completion)) => {
                vec![Command::SpawnWorker(WorkerRequest {
                    worker: 99,
                    payload: completion.bytes,
                })]
            }
            Event::Mailbox(message) if message.body == b"notification-start" => {
                let Ok(input) = host::notification_create() else {
                    return Vec::new();
                };
                let Ok(result) = host::notification_create() else {
                    return Vec::new();
                };
                INPUT_NOTIFICATION.store(input, Ordering::Release);
                RESULT_NOTIFICATION.store(result, Ordering::Release);
                if host::notification_watch_actor(result, 0, 0xcafe).is_err() {
                    return Vec::new();
                }
                vec![Command::SpawnWorker(WorkerRequest {
                    worker: 800,
                    payload: b"__notification_wait__".to_vec(),
                })]
            }
            Event::Mailbox(message) if message.body == b"notification-signal" => {
                SHARED_VALUE.store(0x5a, Ordering::Relaxed);
                let generation = INPUT_GENERATION.fetch_add(1, Ordering::Release) + 1;
                let _latest = host::notification_signal(
                    INPUT_NOTIFICATION.load(Ordering::Acquire),
                    generation,
                );
                Vec::new()
            }
            Event::Mailbox(message) if message.body == b"notification-cancel-start" => {
                let Ok(notification) = host::notification_create() else {
                    return Vec::new();
                };
                CANCEL_NOTIFICATION.store(notification, Ordering::Release);
                vec![Command::SpawnWorker(WorkerRequest {
                    worker: 802,
                    payload: b"__notification_cancel_wait__".to_vec(),
                })]
            }
            Event::Notification(notification)
                if notification.notification == RESULT_NOTIFICATION.load(Ordering::Acquire)
                    && notification.generation == RESULT_GENERATION.load(Ordering::Acquire)
                    && notification.generation == 3
                    && notification.token == 0xcafe =>
            {
                vec![Command::SpawnWorker(WorkerRequest {
                    worker: 801,
                    payload: b"notification-ok".to_vec(),
                })]
            }
            _ => Vec::new(),
        }
    }

    async fn worker_run(worker: u64, payload: Vec<u8>) -> Result<Vec<u8>, String> {
        if worker >= RUBE_WORKER_BASE && payload.starts_with(RUBE_ACK_MAGIC) {
            return Ok(payload[RUBE_ACK_MAGIC.len()..].to_vec());
        }
        if worker == 99 {
            return Ok(payload);
        }
        if worker == 801 {
            return Ok(payload);
        }
        if payload == b"__notification_wait__" {
            let input = INPUT_NOTIFICATION.load(Ordering::Acquire);
            let pollable = host::notification_pollable(
                input,
                INPUT_GENERATION.load(Ordering::Acquire),
            )
            .map_err(|error| format!("create notification pollable: {error:?}"))?;
            let ready = host::worker_wait_any(
                vec![pollable],
                Some(1_000),
                host::WaitMode::Fair,
            )
            .await
            .map_err(|error| format!("wait for notification: {error:?}"))?;
            host::pollable_drop(pollable)
                .map_err(|error| format!("drop notification pollable: {error:?}"))?;
            if ready.pollable != pollable || ready.generation == 0 {
                return Err("notification wait returned an invalid ready item".to_string());
            }
            let value = SHARED_VALUE.load(Ordering::Acquire);
            RESULT_GENERATION.store(3, Ordering::Release);
            let result = RESULT_NOTIFICATION.load(Ordering::Acquire);
            let latest = host::notification_signal(result, 1)
                .and_then(|_| host::notification_signal(result, 3))
                .and_then(|_| host::notification_signal(result, 2))
                .map_err(|error| format!("signal result notification: {error:?}"))?;
            if latest != 3 {
                return Err("result notification did not preserve max generation".to_string());
            }
            return Ok(vec![u8::try_from(value).unwrap_or_default()]);
        }
        if payload == b"__notification_cancel_wait__" {
            let pollable = host::notification_pollable(
                CANCEL_NOTIFICATION.load(Ordering::Acquire),
                0,
            )
            .map_err(|error| format!("create cancellation pollable: {error:?}"))?;
            let result = host::worker_wait_any(
                vec![pollable],
                None,
                host::WaitMode::Biased,
            )
            .await;
            let _dropped = host::pollable_drop(pollable);
            return Err(format!("cancelled notification wait resumed: {result:?}"));
        }
        if payload == b"__spin__" {
            // Prove this call entered the CPU-running branch before the host
            // checks actor progress. The completion becomes externally
            // observable while this worker remains in the loop.
            host::submit_write("spin-entered", 0, b"yes")
                .map_err(|error| format!("spin checkpoint failed: {error:?}"))?;
            loop {
                // Component workers are stackless coroutines. A native async
                // import releases the shared Store while preserving this
                // worker task, allowing the owning actor to dispatch.
                if host::worker_yield().await {
                    return Err("worker cancellation requested".to_string());
                }
            }
        }
        let path = String::from_utf8(payload).map_err(|error| error.to_string())?;
        let cancellation_probe = path == "cancel.pipe";
        // This awaits a native async Component Model import. Only this worker
        // task suspends; the actor dispatcher remains callable.
        let bytes = WorkerFile::open(path).read_to_end().await?;
        if cancellation_probe {
            // The host workload unblocks the FIFO only after observing the
            // terminal cancellation event. Reaching this submission would
            // prove that the guest task survived that event.
            let _ = host::submit_write("cancelled-worker-survived", 0, b"bad");
        }
        Ok(bytes)
    }
}

fn parse_rube_message(bytes: &[u8]) -> Option<(u64, u64, u64, u64)> {
    if bytes.len() != 40 || !bytes.starts_with(RUBE_MESSAGE_MAGIC) {
        return None;
    }
    Some((
        u64::from_le_bytes(bytes.get(8..16)?.try_into().ok()?),
        u64::from_le_bytes(bytes.get(16..24)?.try_into().ok()?),
        u64::from_le_bytes(bytes.get(24..32)?.try_into().ok()?),
        u64::from_le_bytes(bytes.get(32..40)?.try_into().ok()?),
    ))
}

fn rube_mix(input: u64, transition: u64, generation: u64) -> u64 {
    let mut value = input
        ^ transition.wrapping_mul(0x9e37_79b9_7f4a_7c15)
        ^ generation.rotate_left(17);
    value ^= value >> 30;
    value = value.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value ^= value >> 27;
    value = value.wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

/// Small SDK-side worker file wrapper over the native async host ABI.
struct WorkerFile {
    path: String,
    offset: u64,
}

impl WorkerFile {
    fn open(path: String) -> Self {
        Self { path, offset: 0 }
    }

    async fn read_to_end(mut self) -> Result<Vec<u8>, String> {
        let bytes = host::worker_read_file(self.path, self.offset, 1024)
            .await
            .map_err(|error| format!("worker read failed: {error:?}"))?;
        self.offset = self
            .offset
            .saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
        Ok(bytes)
    }
}

export!(ExampleActor);
