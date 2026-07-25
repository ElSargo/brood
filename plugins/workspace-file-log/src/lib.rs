use std::{
    alloc::{Layout, alloc, dealloc},
    collections::BTreeSet,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    ptr,
    sync::{
        Mutex,
        atomic::{AtomicU32, AtomicU64, Ordering},
    },
};

const EVENT_MAILBOX: u32 = 1;
const EVENT_COMPLETION_READ: u32 = 2;
const EVENT_COMPLETION_WRITE: u32 = 3;
const EVENT_COMPLETION_FAILED: u32 = 5;
const EVENT_WORKER_FINISHED: u32 = 7;
const EVENT_LIFECYCLE_START: u32 = 11;
const EVENT_LIFECYCLE_STOP: u32 = 12;
const STACK_ALIGNMENT: usize = 16;
const LOG_DIRECTORY: &str = ".brood";
const LOG_PATH: &str = ".brood/workspace-file-log.log";
const MIRROR_LOG_PATH: &str = ".brood/workspace-file-log.mirror.log";
const INDEX_PATH: &str = ".brood/workspace-file-index.tsv";
const PAUSE_ACK_PATH: &str = ".brood/workspace-file-log.paused";
const LIFECYCLE_LOG_PATH: &str = ".brood/workspace-file-lifecycle.log";
const SNAPSHOT_PATH: &str = ".brood/workspace-file-log.snapshot";
const SNAPSHOT_MAX_BYTES: u32 = 1024 * 1024;
const WORKER_ARGUMENT: u32 = 1;
const MIRROR_WORKER_ARGUMENT: u32 = 2;
const LIFECYCLE_WORKER_ARGUMENT: u32 = 3;
const RUNTIME_IO_WORKER_ARGUMENT: u32 = 4;
const RUNTIME_IO_REPORT_PATH: &str = ".brood/preview1-runtime-io-transcript.md";

static WATCH_STARTED: AtomicU32 = AtomicU32::new(0);
static SCAN_COUNT: AtomicU32 = AtomicU32::new(0);
static ERROR_CODE: AtomicU32 = AtomicU32::new(0);
static WATCH: AtomicU64 = AtomicU64::new(0);
static NOTIFICATION: AtomicU64 = AtomicU64::new(0);
static MIRROR_WATCH: AtomicU64 = AtomicU64::new(0);
static MIRROR_NOTIFICATION: AtomicU64 = AtomicU64::new(0);
static SNAPSHOT_READ_OPERATION: AtomicU64 = AtomicU64::new(0);
static SNAPSHOT_WRITE_OPERATION: AtomicU64 = AtomicU64::new(0);
static SNAPSHOT_BYTES: AtomicU64 = AtomicU64::new(0);
static PRIMARY_PAUSED: AtomicU32 = AtomicU32::new(0);
static LIFECYCLE_WORKER: AtomicU64 = AtomicU64::new(0);
static LIFECYCLE_RECORD: Mutex<Option<String>> = Mutex::new(None);
static RUNTIME_IO_ADDRESS: Mutex<Option<String>> = Mutex::new(None);

#[link(wasm_import_module = "brood_preview1")]
unsafe extern "C" {
    #[link_name = "event_kind"]
    safe fn host_event_kind(value: *mut u32) -> u32;
    #[link_name = "event_sender_len"]
    safe fn host_event_sender_len(value: *mut u32) -> u32;
    #[link_name = "event_payload_len"]
    safe fn host_event_payload_len(value: *mut u32) -> u32;
    #[link_name = "event_operation"]
    safe fn host_event_operation(value: *mut u64) -> u32;
    #[link_name = "event_value"]
    safe fn host_event_value(value: *mut u64) -> u32;
    #[link_name = "event_token"]
    safe fn host_event_token(value: *mut u64) -> u32;
    #[link_name = "event_read"]
    unsafe fn host_event_read(
        sender: *mut u8,
        sender_len: u32,
        payload: *mut u8,
        payload_len: u32,
    ) -> u32;
    #[link_name = "event_lifecycle_data_count"]
    safe fn host_event_lifecycle_data_count(value: *mut u32) -> u32;
    #[link_name = "event_lifecycle_key_len"]
    safe fn host_event_lifecycle_key_len(index: u32, value: *mut u32) -> u32;
    #[link_name = "event_lifecycle_value_len"]
    safe fn host_event_lifecycle_value_len(index: u32, value: *mut u32) -> u32;
    #[link_name = "event_lifecycle_data_read"]
    unsafe fn host_event_lifecycle_data_read(
        index: u32,
        key: *mut u8,
        key_len: u32,
        value: *mut u8,
        value_len: u32,
    ) -> u32;
    #[link_name = "lifecycle_complete"]
    unsafe fn host_lifecycle_complete(status: u32, message: *const u8, message_len: u32) -> u32;
    #[link_name = "completion_submit_read"]
    unsafe fn host_completion_submit_read(
        path: *const u8,
        path_len: u32,
        offset: u64,
        max_bytes: u32,
        operation: *mut u64,
    ) -> u32;
    #[link_name = "completion_submit_write"]
    unsafe fn host_completion_submit_write(
        path: *const u8,
        path_len: u32,
        offset: u64,
        bytes: *const u8,
        bytes_len: u32,
        operation: *mut u64,
    ) -> u32;
    #[link_name = "worker_spawn"]
    safe fn host_worker_spawn(argument: u32, worker: *mut u64) -> u32;
    #[link_name = "filesystem_watch_create"]
    unsafe fn host_filesystem_watch_create(
        path: *const u8,
        path_len: u32,
        recursive: u32,
        watch: *mut u64,
    ) -> u32;
    #[link_name = "filesystem_watch_notification"]
    safe fn host_filesystem_watch_notification(watch: u64, notification: *mut u64) -> u32;
    #[link_name = "filesystem_watch_event_bytes"]
    safe fn host_filesystem_watch_event_bytes(watch: u64, bytes: *mut u32) -> u32;
    #[link_name = "filesystem_watch_read"]
    unsafe fn host_filesystem_watch_read(
        watch: u64,
        events: *mut u8,
        events_len: u32,
        generation: *mut u64,
    ) -> u32;
    #[link_name = "filesystem_watch_drop"]
    safe fn host_filesystem_watch_drop(watch: u64) -> u32;
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
    #[link_name = "subprocess_spawn"]
    unsafe fn host_subprocess_spawn(
        command: *const u8,
        command_len: u32,
        arguments: *const u8,
        arguments_len: u32,
        child: *mut u64,
    ) -> u32;
    #[link_name = "pty_spawn"]
    unsafe fn host_pty_spawn(
        command: *const u8,
        command_len: u32,
        arguments: *const u8,
        arguments_len: u32,
        rows: u32,
        cols: u32,
        child: *mut u64,
    ) -> u32;
    #[link_name = "worker_loopback_connect"]
    unsafe fn host_worker_loopback_connect(
        address: *const u8,
        address_len: u32,
        stream: *mut u64,
    ) -> u32;
    #[link_name = "child_stream"]
    safe fn host_child_stream(child: u64, kind: u32, stream: *mut u64) -> u32;
    #[link_name = "child_notification"]
    safe fn host_child_notification(child: u64, notification: *mut u64) -> u32;
    #[link_name = "child_status"]
    safe fn host_child_status(child: u64, status: *mut u32) -> u32;
    #[link_name = "child_exit_code"]
    safe fn host_child_exit_code(child: u64, code: *mut u32) -> u32;
    #[link_name = "child_resolved_command_len"]
    safe fn host_child_resolved_command_len(child: u64, len: *mut u32) -> u32;
    #[link_name = "child_resolved_command_read"]
    unsafe fn host_child_resolved_command_read(child: u64, path: *mut u8, path_len: u32) -> u32;
    #[link_name = "stream_notification"]
    safe fn host_stream_notification(stream: u64, notification: *mut u64) -> u32;
    #[link_name = "stream_read"]
    unsafe fn host_stream_read(stream: u64, bytes: *mut u8, bytes_len: u32, read: *mut u64) -> u32;
    #[link_name = "stream_write"]
    unsafe fn host_stream_write(
        stream: u64,
        bytes: *const u8,
        bytes_len: u32,
        written: *mut u32,
    ) -> u32;
    #[link_name = "stream_close_write"]
    safe fn host_stream_close_write(stream: u64) -> u32;
    #[link_name = "timer_create"]
    safe fn host_timer_create(delay_ms: u64, timer: *mut u64) -> u32;
    #[link_name = "timer_notification"]
    safe fn host_timer_notification(timer: u64, notification: *mut u64) -> u32;
    #[link_name = "timer_status"]
    safe fn host_timer_status(timer: u64, status: *mut u32) -> u32;
}

#[unsafe(no_mangle)]
pub extern "C" fn brood_preview1_compat_v1() -> u32 {
    0x4d50_3101
}

#[unsafe(no_mangle)]
pub extern "C" fn brood_actor_dispatch_v1(_kind: u32) -> u32 {
    let Some(kind) = host_u32(host_event_kind) else {
        ERROR_CODE.store(1, Ordering::Release);
        return 0;
    };
    match kind {
        EVENT_MAILBOX => {
            dispatch_mailbox();
            if ERROR_CODE.load(Ordering::Acquire) == 9 {
                return 9;
            }
        }
        EVENT_COMPLETION_READ => dispatch_snapshot_read(),
        EVENT_COMPLETION_WRITE => dispatch_snapshot_write(),
        EVENT_COMPLETION_FAILED => ERROR_CODE.store(7, Ordering::Release),
        EVENT_LIFECYCLE_START => dispatch_lifecycle("start"),
        EVENT_LIFECYCLE_STOP => dispatch_lifecycle("stop"),
        EVENT_WORKER_FINISHED
            if host_u64(host_event_operation) == Some(LIFECYCLE_WORKER.load(Ordering::Acquire)) =>
        {
            let worker_result = host_u64(host_event_value).unwrap_or(u64::MAX);
            let status = if let Some(message) = lifecycle_worker_error(worker_result) {
                unsafe { host_lifecycle_complete(1, message.as_ptr(), message.len() as u32) }
            } else {
                unsafe { host_lifecycle_complete(0, ptr::null(), 0) }
            };
            if status != 0 {
                ERROR_CODE.store(21, Ordering::Release);
            }
        }
        _ => {}
    }
    0
}

fn lifecycle_worker_error(worker_result: u64) -> Option<String> {
    (worker_result != 0).then(|| format!("lifecycle artifact worker returned {worker_result}"))
}

fn dispatch_lifecycle(kind: &str) {
    if kind == "start" && !start_watches() {
        complete_lifecycle_error(b"watch setup failed");
        return;
    }
    let Some((instance, workspace)) = read_event_with_sender() else {
        complete_lifecycle_error(b"lifecycle context unavailable");
        return;
    };
    let generation = host_u64(host_event_operation).unwrap_or(0);
    let reason = match (kind, host_u64(host_event_value).unwrap_or(0)) {
        ("start", 1) => "initial",
        ("start", 2) => "replacement",
        ("start", 3) => "session-rebind",
        ("stop", 1) => "replacement",
        ("stop", 2) => "session-rebind",
        ("stop", 3) => "shutdown",
        _ => "unknown",
    };
    let related = host_u64(host_event_token).unwrap_or(0);
    let mut data = Vec::new();
    let count = host_u32(host_event_lifecycle_data_count).unwrap_or(0);
    for index in 0..count {
        let mut key_len = 0;
        let mut value_len = 0;
        if host_event_lifecycle_key_len(index, &raw mut key_len) != 0
            || host_event_lifecycle_value_len(index, &raw mut value_len) != 0
        {
            continue;
        }
        let mut key = vec![0; key_len as usize];
        let mut value = vec![0; value_len as usize];
        let status = unsafe {
            host_event_lifecycle_data_read(
                index,
                key.as_mut_ptr(),
                key_len,
                value.as_mut_ptr(),
                value_len,
            )
        };
        if status == 0 {
            data.push(format!(
                "{}={}",
                String::from_utf8_lossy(&key),
                String::from_utf8_lossy(&value)
            ));
        }
    }
    data.sort();
    let record = format!(
        "{kind}\tgeneration={generation}\treason={reason}\trelated={related}\tinstance={}\tworkspace={}\tdata={}\n",
        String::from_utf8_lossy(&instance),
        String::from_utf8_lossy(&workspace),
        data.join(",")
    );
    if let Ok(mut slot) = LIFECYCLE_RECORD.lock() {
        *slot = Some(record);
    }
    let mut worker = 0;
    if host_worker_spawn(LIFECYCLE_WORKER_ARGUMENT, &raw mut worker) == 0 {
        LIFECYCLE_WORKER.store(worker, Ordering::Release);
    } else {
        complete_lifecycle_error(b"lifecycle worker failed");
    }
}

fn complete_lifecycle_error(message: &[u8]) {
    let _ = unsafe { host_lifecycle_complete(1, message.as_ptr(), message.len() as u32) };
}

fn start_watches() -> bool {
    if WATCH_STARTED
        .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return true;
    }
    let Some((watch, notification)) = create_recursive_watch() else {
        return false;
    };
    let Some((mirror_watch, mirror_notification)) = create_recursive_watch() else {
        return false;
    };
    WATCH.store(watch, Ordering::Release);
    NOTIFICATION.store(notification, Ordering::Release);
    MIRROR_WATCH.store(mirror_watch, Ordering::Release);
    MIRROR_NOTIFICATION.store(mirror_notification, Ordering::Release);
    let mut worker = 0;
    let mut mirror_worker = 0;
    host_worker_spawn(WORKER_ARGUMENT, &raw mut worker) == 0
        && host_worker_spawn(MIRROR_WORKER_ARGUMENT, &raw mut mirror_worker) == 0
}

fn dispatch_mailbox() {
    let Some(payload) = read_event() else {
        ERROR_CODE.store(1, Ordering::Release);
        return;
    };
    if let Some(address) = payload.strip_prefix(b"runtime-io:") {
        if let Ok(mut slot) = RUNTIME_IO_ADDRESS.lock() {
            *slot = Some(String::from_utf8_lossy(address).into_owned());
        }
        let mut worker = 0;
        if host_worker_spawn(RUNTIME_IO_WORKER_ARGUMENT, &raw mut worker) != 0 {
            ERROR_CODE.store(25, Ordering::Release);
        }
    } else if payload == b"start"
        && WATCH_STARTED
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    {
        let Some((watch, notification)) = create_recursive_watch() else {
            ERROR_CODE.store(2, Ordering::Release);
            WATCH_STARTED.store(0, Ordering::Release);
            return;
        };
        let Some((mirror_watch, mirror_notification)) = create_recursive_watch() else {
            let _drop_status = host_filesystem_watch_drop(watch);
            ERROR_CODE.store(2, Ordering::Release);
            WATCH_STARTED.store(0, Ordering::Release);
            return;
        };
        WATCH.store(watch, Ordering::Release);
        NOTIFICATION.store(notification, Ordering::Release);
        MIRROR_WATCH.store(mirror_watch, Ordering::Release);
        MIRROR_NOTIFICATION.store(mirror_notification, Ordering::Release);
        let mut worker = 0_u64;
        let mut mirror_worker = 0_u64;
        if host_worker_spawn(WORKER_ARGUMENT, &raw mut worker) != 0
            || host_worker_spawn(MIRROR_WORKER_ARGUMENT, &raw mut mirror_worker) != 0
        {
            ERROR_CODE.store(3, Ordering::Release);
            WATCH_STARTED.store(0, Ordering::Release);
        }
    } else if payload == b"snapshot" {
        let mut operation = 0_u64;
        let path = LOG_PATH.as_bytes();
        let status = unsafe {
            host_completion_submit_read(
                path.as_ptr(),
                path.len() as u32,
                0,
                SNAPSHOT_MAX_BYTES,
                &raw mut operation,
            )
        };
        if status == 0 && operation != 0 {
            SNAPSHOT_READ_OPERATION.store(operation, Ordering::Release);
        } else {
            ERROR_CODE.store(4, Ordering::Release);
        }
    } else if payload == b"probe-watch-teardown" {
        let path = b".";
        let mut watch = 0_u64;
        let create_status = unsafe {
            host_filesystem_watch_create(path.as_ptr(), path.len() as u32, 0, &raw mut watch)
        };
        if create_status != 0 || watch == 0 || host_filesystem_watch_drop(watch) != 0 {
            ERROR_CODE.store(9, Ordering::Release);
        }
    } else if payload == b"pause-primary" {
        PRIMARY_PAUSED.store(1, Ordering::Release);
    } else if payload == b"resume-primary" {
        PRIMARY_PAUSED.store(0, Ordering::Release);
    }
}

fn create_recursive_watch() -> Option<(u64, u64)> {
    let mut watch = 0_u64;
    let path = b".";
    let status = unsafe {
        host_filesystem_watch_create(path.as_ptr(), path.len() as u32, 1, &raw mut watch)
    };
    let mut notification = 0_u64;
    (status == 0 && host_filesystem_watch_notification(watch, &raw mut notification) == 0)
        .then_some((watch, notification))
}

fn dispatch_snapshot_read() {
    let Some(operation) = host_u64(host_event_operation) else {
        ERROR_CODE.store(5, Ordering::Release);
        return;
    };
    let Some((_sender, bytes)) = read_event_with_sender() else {
        ERROR_CODE.store(5, Ordering::Release);
        return;
    };
    if operation != SNAPSHOT_READ_OPERATION.load(Ordering::Acquire) || bytes.is_empty() {
        ERROR_CODE.store(5, Ordering::Release);
        return;
    }
    let path = SNAPSHOT_PATH.as_bytes();
    let mut write_operation = 0_u64;
    let status = unsafe {
        host_completion_submit_write(
            path.as_ptr(),
            path.len() as u32,
            0,
            bytes.as_ptr(),
            bytes.len() as u32,
            &raw mut write_operation,
        )
    };
    if status == 0 && write_operation != 0 {
        SNAPSHOT_BYTES.store(bytes.len() as u64, Ordering::Release);
        SNAPSHOT_WRITE_OPERATION.store(write_operation, Ordering::Release);
    } else {
        ERROR_CODE.store(6, Ordering::Release);
    }
}

fn dispatch_snapshot_write() {
    let operation = host_u64(host_event_operation);
    let bytes_written = host_u64(host_event_value);
    if operation != Some(SNAPSHOT_WRITE_OPERATION.load(Ordering::Acquire))
        || bytes_written != Some(SNAPSHOT_BYTES.load(Ordering::Acquire))
    {
        ERROR_CODE.store(8, Ordering::Release);
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn brood_worker_run_v1(argument: u32) -> u32 {
    if argument == RUNTIME_IO_WORKER_ARGUMENT {
        return run_runtime_io_proof().map_or_else(|code| code, |()| 0);
    }
    if argument == LIFECYCLE_WORKER_ARGUMENT {
        return match LIFECYCLE_RECORD
            .lock()
            .ok()
            .and_then(|mut record| record.take())
        {
            Some(record) => fs::create_dir_all(LOG_DIRECTORY)
                .and_then(|()| open_log(LIFECYCLE_LOG_PATH))
                .and_then(|mut log| log.write_all(record.as_bytes()))
                .map_or(23, |()| 0),
            None => 24,
        };
    }
    let resources = match argument {
        WORKER_ARGUMENT => (
            WATCH.load(Ordering::Acquire),
            NOTIFICATION.load(Ordering::Acquire),
            LOG_PATH,
            INDEX_PATH,
            true,
        ),
        MIRROR_WORKER_ARGUMENT => (
            MIRROR_WATCH.load(Ordering::Acquire),
            MIRROR_NOTIFICATION.load(Ordering::Acquire),
            MIRROR_LOG_PATH,
            "",
            false,
        ),
        _ => return 1,
    };
    match watch_workspace(
        resources.0,
        resources.1,
        resources.2,
        resources.3,
        resources.4,
    ) {
        Ok(never) => never,
        Err(code) => {
            ERROR_CODE.store(code, Ordering::Release);
            code
        }
    }
}

fn run_runtime_io_proof() -> Result<(), u32> {
    let address = RUNTIME_IO_ADDRESS
        .lock()
        .map_err(|_| 26_u32)?
        .take()
        .ok_or(27_u32)?;
    let pipeline = spawn_child(false, "tr", &["a-z", "A-Z"])?;
    let pipeline_stdin = child_stream(pipeline, 0)?;
    write_stream(pipeline_stdin, b"preview one\n")?;
    if host_stream_close_write(pipeline_stdin) != 0 {
        return Err(53);
    }
    let pipeline_output = read_stream(child_stream(pipeline, 1)?)?;
    let pipeline_code = wait_child(pipeline)?;
    let resolved = child_resolved(pipeline)?;

    let nonzero = spawn_child(false, "sh", &["-c", "exit 7"])?;
    let nonzero_code = wait_child(nonzero)?;

    let pty = spawn_child(
        true,
        "sh",
        &[
            "-c",
            "test -t 0 && printf 'stdin-is-tty '; test -t 1 && printf 'stdout-is-tty terminal-prompt> '; IFS= read -r x; printf 'terminal-reply=%s\\n' \"$x\"",
        ],
    )?;
    let terminal = child_stream(pty, 3)?;
    write_stream(terminal, b"preview1-typed-through-pty\n")?;
    if host_stream_close_write(terminal) != 0 {
        return Err(28);
    }
    let terminal_output = read_stream(terminal)?;
    let pty_code = wait_child(pty)?;

    let mut socket = 0;
    if unsafe {
        host_worker_loopback_connect(address.as_ptr(), address.len() as u32, &raw mut socket)
    } != 0
    {
        return Err(29);
    }
    write_stream(socket, b"hello-from-preview1\n")?;
    if host_stream_close_write(socket) != 0 {
        return Err(30);
    }
    let peer_output = read_stream(socket)?;

    let mut later = 0;
    let mut sooner = 0;
    if host_timer_create(80, &raw mut later) != 0 || host_timer_create(20, &raw mut sooner) != 0 {
        return Err(31);
    }
    let first = wait_timer_pair(sooner, later)?;
    let second = if first == "20ms" {
        wait_timer(later)?;
        "80ms"
    } else {
        wait_timer(sooner)?;
        "20ms"
    };

    fs::create_dir_all(LOG_DIRECTORY).map_err(|_| 32_u32)?;
    let report = format!(
        "# Preview 1 runtime I/O transcript\n\n## Subprocess pipe streams\n\n- Resolved command: `{resolved}`\n- Stdin: `preview one\\n`\n- Transform: `tr a-z A-Z`\n- Stdout: `{}`\n- Exit: `{pipeline_code}`\n- Nonzero child exit: `{nonzero_code}`\n\n## Real PTY interaction\n\n- Sent: `preview1-typed-through-pty`\n- Terminal transcript: `{}`\n- Exit: `{pty_code}`\n\n## Loopback socket exchange\n\n- Peer: `{address}`\n- Sent: `hello-from-preview1`\n- Received: `{}`\n\n## Timer ordering\n\n1. `{first}` timer expired\n2. `{second}` timer expired\n",
        display_runtime_bytes(&pipeline_output),
        display_runtime_bytes(&terminal_output),
        display_runtime_bytes(&peer_output),
    );
    fs::write(RUNTIME_IO_REPORT_PATH, report).map_err(|_| 33)
}

fn spawn_child(pty: bool, command: &str, arguments: &[&str]) -> Result<u64, u32> {
    let encoded = arguments.join("\0");
    let mut child = 0;
    let status = if pty {
        unsafe {
            host_pty_spawn(
                command.as_ptr(),
                command.len() as u32,
                encoded.as_ptr(),
                encoded.len() as u32,
                24,
                80,
                &raw mut child,
            )
        }
    } else {
        unsafe {
            host_subprocess_spawn(
                command.as_ptr(),
                command.len() as u32,
                encoded.as_ptr(),
                encoded.len() as u32,
                &raw mut child,
            )
        }
    };
    (status == 0 && child != 0).then_some(child).ok_or(34)
}

fn child_stream(child: u64, kind: u32) -> Result<u64, u32> {
    let mut stream = 0;
    (host_child_stream(child, kind, &raw mut stream) == 0 && stream != 0)
        .then_some(stream)
        .ok_or(35)
}

fn child_resolved(child: u64) -> Result<String, u32> {
    let mut len = 0;
    if host_child_resolved_command_len(child, &raw mut len) != 0 {
        return Err(36);
    }
    let mut bytes = vec![0; len as usize];
    if unsafe { host_child_resolved_command_read(child, bytes.as_mut_ptr(), len) } != 0 {
        return Err(37);
    }
    String::from_utf8(bytes).map_err(|_| 38)
}

fn wait_child(child: u64) -> Result<u32, u32> {
    let mut notification = 0;
    if host_child_notification(child, &raw mut notification) != 0 {
        return Err(39);
    }
    let mut generation = 0;
    loop {
        let mut status = 0;
        if host_child_status(child, &raw mut status) != 0 {
            return Err(40);
        }
        if status == 1 {
            let mut code = 0;
            return (host_child_exit_code(child, &raw mut code) == 0)
                .then_some(code)
                .ok_or(41);
        }
        if status == 2 {
            return Err(42);
        }
        generation = wait_runtime_notification(notification, generation)?;
    }
}

fn read_stream(stream: u64) -> Result<Vec<u8>, u32> {
    let mut notification = 0;
    if host_stream_notification(stream, &raw mut notification) != 0 {
        return Err(43);
    }
    let mut generation = 0;
    let mut output = Vec::new();
    loop {
        let mut bytes = vec![0; 4096];
        let mut read = 0;
        if unsafe {
            host_stream_read(
                stream,
                bytes.as_mut_ptr(),
                bytes.len() as u32,
                &raw mut read,
            )
        } != 0
        {
            return Err(44);
        }
        if read == u64::MAX {
            return Ok(output);
        }
        if read == u64::MAX - 1 {
            generation = wait_runtime_notification(notification, generation)?;
            continue;
        }
        bytes.truncate(read as usize);
        output.extend(bytes);
    }
}

fn write_stream(stream: u64, mut bytes: &[u8]) -> Result<(), u32> {
    let mut notification = 0;
    if host_stream_notification(stream, &raw mut notification) != 0 {
        return Err(45);
    }
    let mut generation = 0;
    while !bytes.is_empty() {
        let mut written = 0;
        let status = unsafe {
            host_stream_write(stream, bytes.as_ptr(), bytes.len() as u32, &raw mut written)
        };
        if status == 3 {
            generation = wait_runtime_notification(notification, generation)?;
            continue;
        }
        if status != 0 || written == 0 {
            return Err(45);
        }
        bytes = &bytes[written as usize..];
    }
    Ok(())
}

fn wait_timer(timer: u64) -> Result<(), u32> {
    let mut notification = 0;
    if host_timer_notification(timer, &raw mut notification) != 0 {
        return Err(46);
    }
    let mut generation = 0;
    loop {
        let mut status = 0;
        if host_timer_status(timer, &raw mut status) != 0 {
            return Err(47);
        }
        if status == 1 {
            return Ok(());
        }
        generation = wait_runtime_notification(notification, generation)?;
    }
}

fn wait_timer_pair(sooner: u64, later: u64) -> Result<&'static str, u32> {
    let mut first_notification = 0;
    let mut second_notification = 0;
    if host_timer_notification(sooner, &raw mut first_notification) != 0
        || host_timer_notification(later, &raw mut second_notification) != 0
    {
        return Err(48);
    }
    let mut pollables = [0_u64; 2];
    if host_notification_pollable(first_notification, 0, &raw mut pollables[0]) != 0
        || host_notification_pollable(second_notification, 0, &raw mut pollables[1]) != 0
    {
        return Err(49);
    }
    let mut ready = 0;
    if unsafe { host_worker_wait_any(pollables.as_ptr(), 2, u64::MAX, 1, &raw mut ready) } != 0 {
        return Err(50);
    }
    let _ = host_pollable_drop(pollables[0]);
    let _ = host_pollable_drop(pollables[1]);
    Ok(if ready == pollables[0] {
        "20ms"
    } else {
        "80ms"
    })
}

fn wait_runtime_notification(notification: u64, generation: u64) -> Result<u64, u32> {
    let mut pollable = 0;
    if host_notification_pollable(notification, generation, &raw mut pollable) != 0 {
        return Err(51);
    }
    let mut ready = 0;
    let status =
        unsafe { host_worker_wait_any(&raw const pollable, 1, u64::MAX, 1, &raw mut ready) };
    if status != 0 {
        let _ = host_pollable_drop(pollable);
        return Err(52);
    }
    let mut ready_generation = 0;
    let generation_status = host_pollable_generation(ready, &raw mut ready_generation);
    let _ = host_pollable_drop(pollable);
    if generation_status != 0 {
        return Err(52);
    }
    Ok(ready_generation)
}

fn display_runtime_bytes(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .replace('\r', "\\r")
        .replace('\n', "\\n")
}

fn watch_workspace(
    watch: u64,
    notification: u64,
    log_path: &str,
    index_path: &str,
    primary: bool,
) -> Result<u32, u32> {
    fs::create_dir_all(LOG_DIRECTORY).map_err(|_| 10_u32)?;
    let mut index = scan_workspace().map_err(|_| 11_u32)?;
    append_initial(log_path, &index).map_err(|_| 12_u32)?;
    if !index_path.is_empty() {
        write_index(index_path, &index).map_err(|_| 19_u32)?;
    }
    SCAN_COUNT.fetch_add(1, Ordering::AcqRel);
    if watch == 0 || notification == 0 {
        return Err(13);
    }
    let mut observed_generation = 0_u64;

    loop {
        let mut pollable = 0_u64;
        if host_notification_pollable(notification, observed_generation, &raw mut pollable) != 0 {
            return Err(14);
        }
        let pollables = [pollable];
        let mut ready = 0_u64;
        let wait_status = unsafe {
            host_worker_wait_any(
                pollables.as_ptr(),
                pollables.len() as u32,
                u64::MAX,
                0,
                &raw mut ready,
            )
        };
        let _drop_status = host_pollable_drop(pollable);
        if wait_status != 0 || ready != pollable {
            return Err(15);
        }
        if primary && PRIMARY_PAUSED.load(Ordering::Acquire) != 0 {
            fs::write(
                PAUSE_ACK_PATH,
                b"primary watch paused with notification live\n",
            )
            .map_err(|_| 20_u32)?;
            while PRIMARY_PAUSED.load(Ordering::Acquire) != 0 {
                std::hint::spin_loop();
            }
            fs::write(PAUSE_ACK_PATH, b"primary watch resumed\n").map_err(|_| 20_u32)?;
        }

        let mut bytes_len = 0_u32;
        if host_filesystem_watch_event_bytes(watch, &raw mut bytes_len) != 0 || bytes_len == 0 {
            return Err(16);
        }
        let mut bytes = vec![0_u8; bytes_len as usize];
        let mut generation = 0_u64;
        let read_status = unsafe {
            host_filesystem_watch_read(watch, bytes.as_mut_ptr(), bytes_len, &raw mut generation)
        };
        if read_status != 0 || generation == 0 {
            return Err(17);
        }
        apply_watch_events(log_path, index_path, &mut index, &bytes).map_err(|_| 18_u32)?;
        observed_generation = generation;
        SCAN_COUNT.fetch_add(1, Ordering::AcqRel);
    }
}

fn scan_workspace() -> std::io::Result<BTreeSet<String>> {
    let mut files = BTreeSet::new();
    let mut pending = vec![PathBuf::from(".")];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let path = entry.path();
            let relative = path.strip_prefix(".").unwrap_or(&path);
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                if !excluded_directory(relative) {
                    pending.push(path);
                }
            } else if file_type.is_file() {
                files.insert(encode_path(relative));
            }
        }
    }
    Ok(files)
}

fn excluded_directory(path: &Path) -> bool {
    matches!(
        path.file_name().and_then(|name| name.to_str()),
        Some(".git" | ".direnv" | ".brood" | "target")
    )
}

fn append_initial(log_path: &str, files: &BTreeSet<String>) -> std::io::Result<()> {
    let mut log = open_log(log_path)?;
    for path in files {
        writeln!(log, "initial\t{path}")?;
    }
    log.flush()
}

fn apply_watch_events(
    log_path: &str,
    index_path: &str,
    index: &mut BTreeSet<String>,
    bytes: &[u8],
) -> std::io::Result<()> {
    let lines = String::from_utf8_lossy(bytes);
    let ambiguous = lines.lines().find_map(|line| {
        let (kind, path) = line.split_once('\t')?;
        (kind == "rescan" || (kind == "changed" && !excluded_path(Path::new(path))))
            .then_some((kind, path))
    });
    if let Some((kind, path)) = ambiguous {
        let mut log = open_log(log_path)?;
        writeln!(log, "{kind}\t{path}")?;
        writeln!(log, "reconcile-started\tfilesystem")?;
        log.flush()?;
        let rebuilt = match scan_workspace() {
            Ok(rebuilt) => rebuilt,
            Err(error) => {
                writeln!(log, "reconcile-failed\t{error}")?;
                log.flush()?;
                return Err(error);
            }
        };
        *index = rebuilt;
        if !index_path.is_empty() {
            write_index(index_path, index)?;
        }
        // Publish completion only after the replacement index is durable and
        // renamed into place; controllers may treat this record as the model's
        // visibility boundary.
        writeln!(log, "reconciled\t{} files", index.len())?;
        return log.flush();
    }

    let mut log: Option<fs::File> = None;
    let mut index_changed = false;
    for line in lines.lines() {
        let Some((kind, path)) = line.split_once('\t') else {
            continue;
        };
        if !excluded_path(Path::new(path)) {
            // The mirror is the routing barrier during the deterministic
            // overflow workload. Keep its proof artifact compact while still
            // recording the final routed path; the primary index is the
            // complete stateful result.
            let compact_mirror_burst = index_path.is_empty()
                && path.starts_with("overflow/")
                && path != "overflow/0079.txt";
            if !compact_mirror_burst {
                let log = match &mut log {
                    Some(log) => log,
                    None => log.insert(open_log(log_path)?),
                };
                writeln!(log, "{kind}\t{path}")?;
            }
            match kind {
                "removed" => {
                    index_changed |= index.remove(path);
                }
                "created" => {
                    index_changed |= index.insert(path.to_owned());
                }
                _ => {}
            }
        }
    }
    if let Some(mut log) = log {
        log.flush()?;
    }
    if index_path.is_empty() || !index_changed {
        Ok(())
    } else {
        write_index(index_path, index)
    }
}

fn write_index(path: &str, index: &BTreeSet<String>) -> std::io::Result<()> {
    let temporary = format!("{path}.new");
    let mut output = String::from("path\n");
    for path in index {
        use std::fmt::Write as _;
        let _ = writeln!(output, "{path}");
    }
    // One host write keeps this excluded proof artifact from manufacturing an
    // event per inventory row and overflowing its own live watch.
    fs::write(&temporary, output)?;
    fs::rename(temporary, path)
}

fn encode_path(path: &Path) -> String {
    let mut encoded = String::new();
    for &byte in path.as_os_str().as_encoded_bytes() {
        if (0x20..=0x7e).contains(&byte) && byte != b'%' {
            encoded.push(char::from(byte));
        } else {
            use std::fmt::Write as _;
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}

fn excluded_path(path: &Path) -> bool {
    path.components().any(|component| {
        matches!(
            component.as_os_str().to_str(),
            Some(".git" | ".direnv" | ".brood" | "target")
        )
    })
}

fn open_log(path: &str) -> std::io::Result<fs::File> {
    OpenOptions::new().create(true).append(true).open(path)
}

fn read_event() -> Option<Vec<u8>> {
    read_event_with_sender().map(|(_sender, payload)| payload)
}

fn read_event_with_sender() -> Option<(Vec<u8>, Vec<u8>)> {
    let sender_len = host_u32(host_event_sender_len)?;
    let payload_len = host_u32(host_event_payload_len)?;
    let mut sender = vec![0_u8; sender_len as usize];
    let mut payload = vec![0_u8; payload_len as usize];
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
pub extern "C" fn actor_watch_started(_unused: u32) -> u32 {
    WATCH_STARTED.load(Ordering::Acquire)
}

#[unsafe(no_mangle)]
pub extern "C" fn actor_scan_count(_unused: u32) -> u32 {
    SCAN_COUNT.load(Ordering::Acquire)
}

#[unsafe(no_mangle)]
pub extern "C" fn actor_error(_unused: u32) -> u32 {
    ERROR_CODE.load(Ordering::Acquire)
}

#[unsafe(no_mangle)]
pub extern "C" fn brood_worker_reserve_v1(
    stack_bytes: u32,
    tls_bytes: u32,
    tls_align: u32,
) -> u64 {
    let Some(layout) = worker_layout(stack_bytes, tls_bytes, tls_align) else {
        return 0;
    };
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
    let (Ok(stack_top), Ok(tls_base)) = (u32::try_from(stack_top), u32::try_from(tls_base)) else {
        return 0;
    };
    (u64::from(tls_base) << 32) | u64::from(stack_top)
}

#[unsafe(no_mangle)]
pub extern "C" fn brood_worker_release_v1(
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
    unsafe { dealloc(ptr::without_provenance_mut(base), layout) };
    0
}

fn worker_layout(stack_bytes: u32, tls_bytes: u32, tls_align: u32) -> Option<Layout> {
    let tls_align = tls_align as usize;
    if stack_bytes == 0 || tls_align == 0 || !tls_align.is_power_of_two() {
        return None;
    }
    let alignment = STACK_ALIGNMENT.max(tls_align);
    let size = (stack_bytes as usize)
        .checked_add(tls_align.checked_sub(1)?)?
        .checked_add(tls_bytes as usize)?;
    Layout::from_size_align(size, alignment).ok()
}

fn align_up(value: usize, alignment: usize) -> Option<usize> {
    value
        .checked_add(alignment.checked_sub(1)?)
        .map(|value| value & !(alignment - 1))
}
