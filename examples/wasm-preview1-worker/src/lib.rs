use std::{
    alloc::{Layout, alloc, dealloc},
    io::Read,
    ptr,
    sync::atomic::{AtomicU32, AtomicU64, Ordering},
};

const STACK_ALIGNMENT: usize = 16;

// Like ordinary process threads, actor and worker instances use atomics for
// values which are shared through their imported linear memory.
static RESULT: AtomicU32 = AtomicU32::new(0);
static READERS_ENTERED: AtomicU32 = AtomicU32::new(0);
static SPINNERS_ENTERED: AtomicU32 = AtomicU32::new(0);
static COMPAT_FLAGS: AtomicU32 = AtomicU32::new(0);
static COMPAT_WORKER: AtomicU64 = AtomicU64::new(0);
static COMPAT_COMPLETION_BYTE: AtomicU32 = AtomicU32::new(0);
static INPUT_NOTIFICATION: AtomicU64 = AtomicU64::new(0);
static ALTERNATE_NOTIFICATION: AtomicU64 = AtomicU64::new(0);
static RESULT_NOTIFICATION: AtomicU64 = AtomicU64::new(0);
static INPUT_GENERATION: AtomicU64 = AtomicU64::new(0);
static ALTERNATE_GENERATION: AtomicU64 = AtomicU64::new(0);
static RESULT_GENERATION: AtomicU64 = AtomicU64::new(0);
static ALTERNATE_VALUE: AtomicU32 = AtomicU32::new(0);
static ALTERNATE_CLAIMED: AtomicU32 = AtomicU32::new(0);
static COORDINATED_RESULTS: AtomicU32 = AtomicU32::new(0);
static QUEUE_HEAD: AtomicU32 = AtomicU32::new(0);
static QUEUE_TAIL: AtomicU32 = AtomicU32::new(0);
static QUEUE: [AtomicU32; 8] = [const { AtomicU32::new(0) }; 8];
static RUBE_TRANSITION: AtomicU64 = AtomicU64::new(0);
static RUBE_INPUT: AtomicU64 = AtomicU64::new(0);
static RUBE_GENERATION: AtomicU64 = AtomicU64::new(0);
static RUBE_CLAIMED: AtomicU64 = AtomicU64::new(0);
static RUBE_OUTPUT: AtomicU64 = AtomicU64::new(0);
static RUBE_COMPLETED: AtomicU32 = AtomicU32::new(0);
static RUBE_ERROR: AtomicU32 = AtomicU32::new(0);

const RUBE_MESSAGE_MAGIC: &[u8; 8] = b"RUBEIO01";
const RUBE_WORKER_ARGUMENT: u32 = u32::MAX - 1;

const EVENT_MAILBOX: u32 = 1;
const EVENT_COMPLETION_READ: u32 = 2;
const EVENT_WORKER_FINISHED: u32 = 7;
const EVENT_NOTIFICATION: u32 = 10;
const FLAG_MAILBOX: u32 = 1 << 0;
const FLAG_COMPLETION: u32 = 1 << 1;
const FLAG_WORKER: u32 = 1 << 2;
const FLAG_NOTIFICATION: u32 = 1 << 3;
const FLAG_SELECT: u32 = 1 << 4;
const FLAG_ERROR: u32 = 1 << 31;

#[link(wasm_import_module = "manifold_preview1")]
unsafe extern "C" {
    #[link_name = "event_kind"]
    safe fn host_event_kind(value: *mut u32) -> u32;
    #[link_name = "event_sender_len"]
    safe fn host_event_sender_len(value: *mut u32) -> u32;
    #[link_name = "event_payload_len"]
    safe fn host_event_payload_len(value: *mut u32) -> u32;
    #[link_name = "event_value"]
    safe fn host_event_value(value: *mut u64) -> u32;
    #[link_name = "event_operation"]
    safe fn host_event_operation(value: *mut u64) -> u32;
    #[link_name = "event_token"]
    safe fn host_event_token(value: *mut u64) -> u32;
    #[link_name = "event_read"]
    unsafe fn host_event_read(
        sender: *mut u8,
        sender_len: u32,
        payload: *mut u8,
        payload_len: u32,
    ) -> u32;
    #[link_name = "completion_submit_read"]
    unsafe fn host_completion_submit_read(
        path: *const u8,
        path_len: u32,
        offset: u64,
        max_bytes: u32,
        operation: *mut u64,
    ) -> u32;
    #[link_name = "worker_spawn"]
    safe fn host_worker_spawn(argument: u32, worker: *mut u64) -> u32;
    #[link_name = "worker_spawn_with_stdin"]
    safe fn host_worker_spawn_with_stdin(argument: u32, worker: *mut u64) -> u32;
    #[link_name = "worker_input"]
    unsafe fn host_worker_input(worker: u64, bytes: *const u8, bytes_len: u32) -> u32;
    #[link_name = "notification_create"]
    safe fn host_notification_create(notification: *mut u64) -> u32;
    #[link_name = "notification_drop"]
    safe fn host_notification_drop(notification: u64) -> u32;
    #[link_name = "notification_signal"]
    safe fn host_notification_signal(notification: u64, generation: u64, latest: *mut u64) -> u32;
    #[link_name = "notification_watch"]
    safe fn host_notification_watch(
        notification: u64,
        observed_generation: u64,
        token: u64,
        queued: *mut u32,
    ) -> u32;
    #[link_name = "notification_pollable"]
    safe fn host_notification_pollable(
        notification: u64,
        observed_generation: u64,
        pollable: *mut u64,
    ) -> u32;
    #[link_name = "pollable_drop"]
    safe fn host_pollable_drop(pollable: u64) -> u32;
    #[link_name = "pollable_generation"]
    safe fn host_pollable_generation(pollable: u64, generation: *mut u64) -> u32;
    #[link_name = "worker_wait_any"]
    unsafe fn host_worker_wait_any(
        pollables: *const u64,
        pollables_len: u32,
        timeout_ms: u64,
        mode: u32,
        ready: *mut u64,
    ) -> u32;
}

#[unsafe(no_mangle)]
pub extern "C" fn manifold_preview1_compat_v1() -> u32 {
    0x4d50_3101
}

#[unsafe(no_mangle)]
pub extern "C" fn manifold_actor_dispatch_v1(_kind: u32) -> u32 {
    let Some(kind) = host_u32(host_event_kind) else {
        COMPAT_FLAGS.fetch_or(FLAG_ERROR, Ordering::Release);
        return 0;
    };
    match kind {
        EVENT_MAILBOX => dispatch_mailbox(),
        EVENT_COMPLETION_READ => {
            let Some((_sender, payload)) = read_event() else {
                COMPAT_FLAGS.fetch_or(FLAG_ERROR, Ordering::Release);
                return 0;
            };
            if let Some(byte) = payload.first() {
                COMPAT_COMPLETION_BYTE.store(u32::from(*byte), Ordering::Release);
            }
            COMPAT_FLAGS.fetch_or(FLAG_COMPLETION, Ordering::Release);
        }
        EVENT_WORKER_FINISHED => {
            if let Some(value) = host_u64(host_event_value) {
                RESULT.store(value as u32, Ordering::Release);
                COMPAT_FLAGS.fetch_or(FLAG_WORKER, Ordering::Release);
            }
        }
        EVENT_NOTIFICATION => dispatch_notification(),
        _ => {}
    }
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn manifold_worker_run_v1(argument: u32) -> u32 {
    if argument == u32::MAX {
        worker_read(argument)
    } else if argument == RUBE_WORKER_ARGUMENT {
        worker_rube()
    } else {
        worker_select(argument)
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn actor_compat_flags(_unused: u32) -> u32 {
    COMPAT_FLAGS.load(Ordering::Acquire)
}

#[unsafe(no_mangle)]
pub extern "C" fn actor_compat_completion_byte(_unused: u32) -> u32 {
    COMPAT_COMPLETION_BYTE.load(Ordering::Acquire)
}

#[unsafe(no_mangle)]
pub extern "C" fn actor_coordination_generation(_unused: u32) -> u32 {
    RESULT_GENERATION.load(Ordering::Acquire) as u32
}

#[unsafe(no_mangle)]
pub extern "C" fn actor_coordinated_results(_unused: u32) -> u32 {
    COORDINATED_RESULTS.load(Ordering::Acquire)
}

#[unsafe(no_mangle)]
pub extern "C" fn actor_rube_output_low(_unused: u32) -> u32 {
    RUBE_OUTPUT.load(Ordering::Acquire) as u32
}

#[unsafe(no_mangle)]
pub extern "C" fn actor_rube_output_high(_unused: u32) -> u32 {
    (RUBE_OUTPUT.load(Ordering::Acquire) >> 32) as u32
}

#[unsafe(no_mangle)]
pub extern "C" fn actor_rube_completed(_unused: u32) -> u32 {
    RUBE_COMPLETED.load(Ordering::Acquire)
}

#[unsafe(no_mangle)]
pub extern "C" fn actor_rube_error(_unused: u32) -> u32 {
    RUBE_ERROR.load(Ordering::Acquire)
}

fn dispatch_mailbox() {
    let Some((_sender, payload)) = read_event() else {
        COMPAT_FLAGS.fetch_or(FLAG_ERROR, Ordering::Release);
        return;
    };
    COMPAT_FLAGS.fetch_or(FLAG_MAILBOX, Ordering::Release);
    if payload == b"spin-dispatch" {
        loop {
            SPINNERS_ENTERED.fetch_add(1, Ordering::Relaxed);
        }
    } else if payload.starts_with(RUBE_MESSAGE_MAGIC) {
        dispatch_rube_message(&payload);
    } else if payload == b"start-notify" {
        start_notification_workers();
    } else if let Some(value) = payload
        .strip_prefix(b"queue:")
        .and_then(|bytes| bytes.first())
    {
        if queue_push(u32::from(*value)).is_none()
            || signal_generation(
                &INPUT_GENERATION,
                INPUT_NOTIFICATION.load(Ordering::Acquire),
            )
            .is_none()
        {
            COMPAT_FLAGS.fetch_or(FLAG_ERROR, Ordering::Release);
        }
    } else if let Some(value) = payload
        .strip_prefix(b"alternate:")
        .and_then(|bytes| bytes.first())
    {
        ALTERNATE_VALUE.store(u32::from(*value), Ordering::Release);
        if signal_generation(
            &ALTERNATE_GENERATION,
            ALTERNATE_NOTIFICATION.load(Ordering::Acquire),
        )
        .is_none()
        {
            COMPAT_FLAGS.fetch_or(FLAG_ERROR, Ordering::Release);
        }
    } else if payload == b"start-stdin" {
        let path = b"completion.txt";
        let mut operation = 0_u64;
        // SAFETY: all pointers refer to live buffers for the duration of the host call.
        let completion_status = unsafe {
            host_completion_submit_read(path.as_ptr(), path.len() as u32, 0, 64, &raw mut operation)
        };
        let mut worker = 0_u64;
        let worker_status = host_worker_spawn_with_stdin(u32::MAX, &raw mut worker);
        if completion_status == 0 && worker_status == 0 {
            COMPAT_WORKER.store(worker, Ordering::Release);
        } else {
            COMPAT_FLAGS.fetch_or(FLAG_ERROR, Ordering::Release);
        }
    } else if let Some(bytes) = payload.strip_prefix(b"feed:") {
        let worker = COMPAT_WORKER.load(Ordering::Acquire);
        // SAFETY: `bytes` remains live and readable for the synchronous host call.
        if unsafe { host_worker_input(worker, bytes.as_ptr(), bytes.len() as u32) } != 0 {
            COMPAT_FLAGS.fetch_or(FLAG_ERROR, Ordering::Release);
        }
    }
}

fn dispatch_rube_message(bytes: &[u8]) {
    let Some((transition, input, generation, claimed)) = parse_rube_message(bytes) else {
        RUBE_ERROR.store(1, Ordering::Release);
        return;
    };
    RUBE_TRANSITION.store(transition, Ordering::Release);
    RUBE_INPUT.store(input, Ordering::Release);
    RUBE_GENERATION.store(generation, Ordering::Release);
    RUBE_CLAIMED.store(claimed, Ordering::Release);
    let mut worker = 0_u64;
    if host_worker_spawn(RUBE_WORKER_ARGUMENT, &raw mut worker) != 0 {
        RUBE_ERROR.store(2, Ordering::Release);
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

fn worker_rube() -> u32 {
    let output = rube_mix(
        RUBE_INPUT.load(Ordering::Acquire),
        RUBE_TRANSITION.load(Ordering::Acquire),
        RUBE_GENERATION.load(Ordering::Acquire),
    );
    if output != RUBE_CLAIMED.load(Ordering::Acquire) {
        RUBE_ERROR.store(3, Ordering::Release);
    }
    RUBE_OUTPUT.store(output, Ordering::Release);
    RUBE_COMPLETED.fetch_add(1, Ordering::AcqRel);
    output as u32
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

fn start_notification_workers() {
    for slot in [
        &INPUT_NOTIFICATION,
        &ALTERNATE_NOTIFICATION,
        &RESULT_NOTIFICATION,
    ] {
        let notification = slot.swap(0, Ordering::AcqRel);
        if notification != 0 && host_notification_drop(notification) != 0 {
            COMPAT_FLAGS.fetch_or(FLAG_ERROR, Ordering::Release);
            return;
        }
    }
    INPUT_GENERATION.store(0, Ordering::Release);
    ALTERNATE_GENERATION.store(0, Ordering::Release);
    RESULT_GENERATION.store(0, Ordering::Release);

    let Some(input) = Signal::create() else {
        COMPAT_FLAGS.fetch_or(FLAG_ERROR, Ordering::Release);
        return;
    };
    let Some(alternate) = Signal::create() else {
        COMPAT_FLAGS.fetch_or(FLAG_ERROR, Ordering::Release);
        return;
    };
    let Some(result) = Signal::create() else {
        COMPAT_FLAGS.fetch_or(FLAG_ERROR, Ordering::Release);
        return;
    };
    INPUT_NOTIFICATION.store(input.raw(), Ordering::Release);
    ALTERNATE_NOTIFICATION.store(alternate.raw(), Ordering::Release);
    RESULT_NOTIFICATION.store(result.raw(), Ordering::Release);
    ALTERNATE_CLAIMED.store(0, Ordering::Release);
    let mut queued = 0_u32;
    if host_notification_watch(result.raw(), 0, 0xfeed, &raw mut queued) != 0 {
        COMPAT_FLAGS.fetch_or(FLAG_ERROR, Ordering::Release);
        return;
    }
    let mut first = 0_u64;
    let mut second = 0_u64;
    if host_worker_spawn(1, &raw mut first) != 0 || host_worker_spawn(2, &raw mut second) != 0 {
        COMPAT_FLAGS.fetch_or(FLAG_ERROR, Ordering::Release);
    }
}

fn dispatch_notification() {
    let Some(notification) = host_u64(host_event_operation) else {
        COMPAT_FLAGS.fetch_or(FLAG_ERROR, Ordering::Release);
        return;
    };
    let Some(generation) = host_u64(host_event_value) else {
        COMPAT_FLAGS.fetch_or(FLAG_ERROR, Ordering::Release);
        return;
    };
    let Some(token) = host_u64(host_event_token) else {
        COMPAT_FLAGS.fetch_or(FLAG_ERROR, Ordering::Release);
        return;
    };
    if notification != RESULT_NOTIFICATION.load(Ordering::Acquire)
        || generation != RESULT_GENERATION.load(Ordering::Acquire)
        || generation != 3
        || token != 0xfeed
    {
        COMPAT_FLAGS.fetch_or(FLAG_ERROR, Ordering::Release);
        return;
    }
    COMPAT_FLAGS.fetch_or(FLAG_NOTIFICATION, Ordering::Release);
}

fn worker_select(worker_number: u32) -> u32 {
    let input = Signal::from_raw(INPUT_NOTIFICATION.load(Ordering::Acquire));
    let alternate = Signal::from_raw(ALTERNATE_NOTIFICATION.load(Ordering::Acquire));
    let mut input_observed = INPUT_GENERATION.load(Ordering::Acquire);
    let mut alternate_observed = ALTERNATE_GENERATION.load(Ordering::Acquire);
    let value = loop {
        let Some(input_pollable) = input.pollable(input_observed) else {
            return mark_coordination_error();
        };
        let Some(alternate_pollable) = alternate.pollable(alternate_observed) else {
            return mark_coordination_error();
        };
        let pollables = [input_pollable, alternate_pollable];
        let Some((ready, generation)) = select(&pollables, 2_000, SelectMode::Fair) else {
            return mark_coordination_error();
        };
        if pollables
            .iter()
            .copied()
            .any(|pollable| host_pollable_drop(pollable) != 0)
        {
            return mark_coordination_error();
        }
        if ready == input_pollable {
            input_observed = generation;
            if let Some(value) = queue_pop() {
                break value;
            }
        } else if ready == alternate_pollable {
            alternate_observed = generation;
            if ALTERNATE_CLAIMED
                .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                break ALTERNATE_VALUE.load(Ordering::Acquire);
            }
        } else {
            return mark_coordination_error();
        }
    };
    COORDINATED_RESULTS.fetch_add(1, Ordering::AcqRel);
    COMPAT_FLAGS.fetch_or(FLAG_SELECT, Ordering::Release);
    if worker_number == 1 {
        // Publish one shared snapshot, then deliberately announce the same
        // burst out of order. The host must deliver one event at generation 3.
        RESULT.store(value, Ordering::Relaxed);
        RESULT_GENERATION.store(3, Ordering::Release);
        let result = RESULT_NOTIFICATION.load(Ordering::Acquire);
        let mut latest = 0_u64;
        if host_notification_signal(result, 1, &raw mut latest) != 0
            || host_notification_signal(result, 3, &raw mut latest) != 0
            || host_notification_signal(result, 2, &raw mut latest) != 0
            || latest != 3
        {
            return mark_coordination_error();
        }
    }
    value
}

fn mark_coordination_error() -> u32 {
    COMPAT_FLAGS.fetch_or(FLAG_ERROR, Ordering::Release);
    0
}

#[derive(Clone, Copy)]
struct Signal(u64);

impl Signal {
    fn create() -> Option<Self> {
        let mut notification = 0_u64;
        (host_notification_create(&raw mut notification) == 0).then_some(Self(notification))
    }

    const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    const fn raw(self) -> u64 {
        self.0
    }

    fn pollable(self, observed_generation: u64) -> Option<u64> {
        let mut pollable = 0_u64;
        (host_notification_pollable(self.0, observed_generation, &raw mut pollable) == 0)
            .then_some(pollable)
    }
}

#[derive(Clone, Copy)]
enum SelectMode {
    Fair = 1,
}

fn select(pollables: &[u64], timeout_ms: u64, mode: SelectMode) -> Option<(u64, u64)> {
    let mut ready = 0_u64;
    // SAFETY: the pollable slice remains live for the duration of the host call.
    let status = unsafe {
        host_worker_wait_any(
            pollables.as_ptr(),
            pollables.len() as u32,
            timeout_ms,
            mode as u32,
            &raw mut ready,
        )
    };
    if status != 0 {
        return None;
    }
    let mut generation = 0_u64;
    (host_pollable_generation(ready, &raw mut generation) == 0 && generation != 0)
        .then_some((ready, generation))
}

fn signal_generation(generation: &AtomicU64, notification: u64) -> Option<u64> {
    let published = generation.fetch_add(1, Ordering::Release).checked_add(1)?;
    let mut latest = 0_u64;
    (host_notification_signal(notification, published, &raw mut latest) == 0).then_some(latest)
}

fn queue_push(value: u32) -> Option<()> {
    let head = QUEUE_HEAD.load(Ordering::Acquire);
    let tail = QUEUE_TAIL.load(Ordering::Relaxed);
    if tail.wrapping_sub(head) >= QUEUE.len() as u32 {
        return None;
    }
    QUEUE
        .get((tail as usize) % QUEUE.len())?
        .store(value, Ordering::Relaxed);
    QUEUE_TAIL.store(tail.wrapping_add(1), Ordering::Release);
    Some(())
}

fn queue_pop() -> Option<u32> {
    loop {
        let head = QUEUE_HEAD.load(Ordering::Relaxed);
        if head == QUEUE_TAIL.load(Ordering::Acquire) {
            return None;
        }
        if QUEUE_HEAD
            .compare_exchange_weak(
                head,
                head.wrapping_add(1),
                Ordering::AcqRel,
                Ordering::Relaxed,
            )
            .is_ok()
        {
            return QUEUE
                .get((head as usize) % QUEUE.len())
                .map(|slot| slot.load(Ordering::Acquire));
        }
    }
}

fn read_event() -> Option<(Vec<u8>, Vec<u8>)> {
    let sender_len = host_u32(host_event_sender_len)?;
    let payload_len = host_u32(host_event_payload_len)?;
    let mut sender = vec![0_u8; sender_len as usize];
    let mut payload = vec![0_u8; payload_len as usize];
    // SAFETY: vectors expose writable regions of exactly the lengths supplied.
    let status = unsafe {
        host_event_read(
            sender.as_mut_ptr(),
            sender_len,
            payload.as_mut_ptr(),
            payload_len,
        )
    };
    (status == 0).then_some((sender, payload))
}

fn host_u32(function: extern "C" fn(*mut u32) -> u32) -> Option<u32> {
    let mut value = 0_u32;
    (function(&raw mut value) == 0).then_some(value)
}

fn host_u64(function: extern "C" fn(*mut u64) -> u32) -> Option<u64> {
    let mut value = 0_u64;
    (function(&raw mut value) == 0).then_some(value)
}

#[unsafe(no_mangle)]
pub extern "C" fn actor_ping(value: u32) -> u32 {
    value.wrapping_add(1)
}

#[unsafe(no_mangle)]
pub extern "C" fn actor_result(_unused: u32) -> u32 {
    RESULT.load(Ordering::Acquire)
}

#[unsafe(no_mangle)]
pub extern "C" fn actor_readers_entered(_unused: u32) -> u32 {
    READERS_ENTERED.load(Ordering::Acquire)
}

#[unsafe(no_mangle)]
pub extern "C" fn actor_spinners_entered(_unused: u32) -> u32 {
    SPINNERS_ENTERED.load(Ordering::Acquire)
}

#[unsafe(no_mangle)]
pub extern "C" fn worker_read(_unused: u32) -> u32 {
    let mut byte = [0_u8; 1];
    READERS_ENTERED.fetch_add(1, Ordering::AcqRel);
    std::io::stdin()
        .read_exact(&mut byte)
        .expect("ordinary blocking-looking stdin read");
    RESULT.store(u32::from(byte[0]), Ordering::Release);
    u32::from(byte[0])
}

#[unsafe(no_mangle)]
pub extern "C" fn worker_file_roundtrip(_unused: u32) -> u32 {
    let bytes =
        std::fs::read("worker-input.txt").expect("ordinary WASI filesystem open/read in worker");
    std::fs::write("worker-output.txt", &bytes)
        .expect("ordinary WASI filesystem open/write in worker");
    u32::try_from(bytes.len()).expect("worker input length fits in u32")
}

#[unsafe(no_mangle)]
pub extern "C" fn worker_spin(_unused: u32) -> u32 {
    SPINNERS_ENTERED.fetch_add(1, Ordering::AcqRel);
    loop {
        // This atomic side effect keeps the loop observable. Wasmtime epoch
        // checks turn it into a cooperatively yielding stackless task.
        SPINNERS_ENTERED.fetch_add(1, Ordering::Relaxed);
    }
}

/// Reserve a stack followed by an aligned TLS region with the guest allocator.
///
/// Low 32 bits of the result contain the stack top and high 32 bits contain
/// the TLS base. Zero reports allocation failure.
#[unsafe(no_mangle)]
pub extern "C" fn manifold_worker_reserve_v1(
    stack_bytes: u32,
    tls_bytes: u32,
    tls_align: u32,
) -> u64 {
    let Some(layout) = worker_layout(stack_bytes, tls_bytes, tls_align) else {
        return 0;
    };
    // SAFETY: `layout` was validated by `Layout::from_size_align`. The pointer
    // is either null or owns a unique allocation until the matching release.
    let base = unsafe { alloc(layout) };
    if base.is_null() {
        return 0;
    }
    let base = base as usize;
    let Some(stack_top) = base.checked_add(stack_bytes as usize) else {
        return 0;
    };
    let Some(tls_base) = align_up(stack_top, tls_align as usize) else {
        return 0;
    };
    let Ok(stack_top) = u32::try_from(stack_top) else {
        return 0;
    };
    let Ok(tls_base) = u32::try_from(tls_base) else {
        return 0;
    };
    (u64::from(tls_base) << 32) | u64::from(stack_top)
}

/// Release a slab previously returned by [`manifold_worker_reserve_v1`].
///
/// Returns zero on success and one if the host supplied inconsistent metadata.
#[unsafe(no_mangle)]
pub extern "C" fn manifold_worker_release_v1(
    encoded: u64,
    stack_bytes: u32,
    tls_bytes: u32,
    tls_align: u32,
) -> u32 {
    let Some(layout) = worker_layout(stack_bytes, tls_bytes, tls_align) else {
        return 1;
    };
    let stack_top = (encoded & u64::from(u32::MAX)) as usize;
    let tls_base = (encoded >> 32) as usize;
    let Some(base) = stack_top.checked_sub(stack_bytes as usize) else {
        return 1;
    };
    if align_up(stack_top, tls_align as usize) != Some(tls_base) || base == 0 {
        return 1;
    }
    // SAFETY: the host only calls this once, after the worker instance using
    // the slab has been dropped, with the exact metadata used for allocation.
    unsafe { dealloc(ptr::without_provenance_mut(base), layout) };
    0
}

fn worker_layout(stack_bytes: u32, tls_bytes: u32, tls_align: u32) -> Option<Layout> {
    let tls_align = tls_align as usize;
    if stack_bytes == 0 || tls_align == 0 || !tls_align.is_power_of_two() {
        return None;
    }
    let alignment = STACK_ALIGNMENT.max(tls_align);
    let padding = tls_align.checked_sub(1)?;
    let size = (stack_bytes as usize)
        .checked_add(padding)?
        .checked_add(tls_bytes as usize)?;
    Layout::from_size_align(size, alignment).ok()
}

fn align_up(value: usize, alignment: usize) -> Option<usize> {
    value
        .checked_add(alignment.checked_sub(1)?)
        .map(|value| value & !(alignment - 1))
}
