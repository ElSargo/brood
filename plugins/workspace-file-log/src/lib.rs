use std::{
    alloc::{Layout, alloc, dealloc},
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    ptr,
    sync::atomic::{AtomicU32, AtomicU64, Ordering},
    time::UNIX_EPOCH,
};

const EVENT_MAILBOX: u32 = 1;
const STACK_ALIGNMENT: usize = 16;
const LOG_DIRECTORY: &str = ".manifold";
const LOG_PATH: &str = ".manifold/workspace-file-log.log";
const WORKER_ARGUMENT: u32 = 1;

static WATCH_STARTED: AtomicU32 = AtomicU32::new(0);
static SCAN_COUNT: AtomicU32 = AtomicU32::new(0);
static ERROR_CODE: AtomicU32 = AtomicU32::new(0);
static WATCH: AtomicU64 = AtomicU64::new(0);
static NOTIFICATION: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, PartialEq, Eq)]
struct FileStamp {
    len: u64,
    modified_nanos: u128,
}

#[link(wasm_import_module = "manifold_preview1")]
unsafe extern "C" {
    #[link_name = "event_kind"]
    safe fn host_event_kind(value: *mut u32) -> u32;
    #[link_name = "event_sender_len"]
    safe fn host_event_sender_len(value: *mut u32) -> u32;
    #[link_name = "event_payload_len"]
    safe fn host_event_payload_len(value: *mut u32) -> u32;
    #[link_name = "event_read"]
    unsafe fn host_event_read(
        sender: *mut u8,
        sender_len: u32,
        payload: *mut u8,
        payload_len: u32,
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
    #[link_name = "notification_pollable"]
    safe fn host_notification_pollable(
        notification: u64,
        observed_generation: u64,
        pollable: *mut u64,
    ) -> u32;
    #[link_name = "pollable_drop"]
    safe fn host_pollable_drop(pollable: u64) -> u32;
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
    if host_u32(host_event_kind) != Some(EVENT_MAILBOX) {
        return 0;
    }
    let Some(payload) = read_event() else {
        ERROR_CODE.store(1, Ordering::Release);
        return 0;
    };
    if payload == b"start"
        && WATCH_STARTED
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    {
        let mut watch = 0_u64;
        let path = b".";
        let watch_status = unsafe {
            host_filesystem_watch_create(path.as_ptr(), path.len() as u32, 1, &raw mut watch)
        };
        let mut notification = 0_u64;
        if watch_status != 0
            || host_filesystem_watch_notification(watch, &raw mut notification) != 0
        {
            ERROR_CODE.store(2, Ordering::Release);
            WATCH_STARTED.store(0, Ordering::Release);
            return 0;
        }
        WATCH.store(watch, Ordering::Release);
        NOTIFICATION.store(notification, Ordering::Release);
        let mut worker = 0_u64;
        if host_worker_spawn(WORKER_ARGUMENT, &raw mut worker) != 0 {
            ERROR_CODE.store(3, Ordering::Release);
            WATCH_STARTED.store(0, Ordering::Release);
        }
    }
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn manifold_worker_run_v1(argument: u32) -> u32 {
    if argument != WORKER_ARGUMENT {
        return 1;
    }
    match watch_workspace() {
        Ok(never) => never,
        Err(code) => {
            ERROR_CODE.store(code, Ordering::Release);
            code
        }
    }
}

fn watch_workspace() -> Result<u32, u32> {
    fs::create_dir_all(LOG_DIRECTORY).map_err(|_| 10_u32)?;
    let previous = scan_workspace().map_err(|_| 11_u32)?;
    append_initial(&previous).map_err(|_| 12_u32)?;
    SCAN_COUNT.fetch_add(1, Ordering::AcqRel);
    let watch = WATCH.load(Ordering::Acquire);
    let notification = NOTIFICATION.load(Ordering::Acquire);
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

        let mut bytes_len = 0_u32;
        if host_filesystem_watch_event_bytes(watch, &raw mut bytes_len) != 0 || bytes_len == 0 {
            return Err(16);
        }
        let mut bytes = vec![0_u8; bytes_len as usize];
        let mut generation = 0_u64;
        let read_status = unsafe {
            host_filesystem_watch_read(
                watch,
                bytes.as_mut_ptr(),
                bytes_len,
                &raw mut generation,
            )
        };
        if read_status != 0 || generation == 0 {
            return Err(17);
        }
        append_watch_events(&bytes).map_err(|_| 18_u32)?;
        observed_generation = generation;
        SCAN_COUNT.fetch_add(1, Ordering::AcqRel);
    }
}

fn scan_workspace() -> std::io::Result<BTreeMap<String, FileStamp>> {
    let mut files = BTreeMap::new();
    let mut pending = vec![PathBuf::from(".")];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let path = entry.path();
            let relative = path.strip_prefix(".").unwrap_or(&path);
            let metadata = entry.metadata()?;
            if metadata.is_dir() {
                if !excluded_directory(relative) {
                    pending.push(path);
                }
            } else if metadata.is_file() {
                let modified_nanos = metadata
                    .modified()
                    .ok()
                    .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                    .map_or(0, |duration| duration.as_nanos());
                files.insert(
                    relative.to_string_lossy().into_owned(),
                    FileStamp {
                        len: metadata.len(),
                        modified_nanos,
                    },
                );
            }
        }
    }
    Ok(files)
}

fn excluded_directory(path: &Path) -> bool {
    matches!(
        path.file_name().and_then(|name| name.to_str()),
        Some(".git" | ".direnv" | ".manifold" | "target")
    )
}

fn append_initial(files: &BTreeMap<String, FileStamp>) -> std::io::Result<()> {
    let mut log = open_log()?;
    for path in files.keys() {
        writeln!(log, "initial\t{path}")?;
    }
    log.flush()
}

fn append_watch_events(bytes: &[u8]) -> std::io::Result<()> {
    let mut log = open_log()?;
    for line in String::from_utf8_lossy(bytes).lines() {
        let Some((kind, path)) = line.split_once('\t') else {
            continue;
        };
        if !excluded_path(Path::new(path)) {
            writeln!(log, "{kind}\t{path}")?;
        }
    }
    log.flush()
}

fn excluded_path(path: &Path) -> bool {
    path.components().any(|component| {
        matches!(
            component.as_os_str().to_str(),
            Some(".git" | ".direnv" | ".manifold" | "target")
        )
    })
}

fn open_log() -> std::io::Result<fs::File> {
    OpenOptions::new().create(true).append(true).open(LOG_PATH)
}

fn read_event() -> Option<Vec<u8>> {
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
    (status == 0).then_some(payload)
}

fn host_u32(function: extern "C" fn(*mut u32) -> u32) -> Option<u32> {
    let mut value = 0_u32;
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
pub extern "C" fn manifold_worker_reserve_v1(
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
