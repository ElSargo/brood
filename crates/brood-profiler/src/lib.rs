//! Bounded, process-independent profiling for Brood's latency-critical paths.
//!
//! Recording is disabled unless `BROOD_PROFILE_DIR` is set. All processes use
//! Linux's shared monotonic clock, so their JSONL streams can be merged without
//! clock synchronization. Events are moved through a bounded channel to keep
//! file I/O off instrumented threads; saturation is counted rather than hidden.

use std::{
    borrow::Cow,
    fs::{File, create_dir_all},
    io::{BufWriter, Write},
    path::PathBuf,
    process,
    sync::{
        OnceLock,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, SyncSender},
    },
    thread,
    time::Duration,
};

use rustix::time::{ClockId, clock_gettime};
use serde::{Deserialize, Serialize};

const CHANNEL_CAPACITY: usize = 65_536;
const SCHEMA_VERSION: u8 = 1;
static RECORDER: OnceLock<Option<Recorder>> = OnceLock::new();
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(untagged)]
pub enum Value {
    U64(u64),
    I64(i64),
    Bool(bool),
    Text(Cow<'static, str>),
}

impl From<u64> for Value {
    fn from(value: u64) -> Self {
        Self::U64(value)
    }
}

impl From<usize> for Value {
    fn from(value: usize) -> Self {
        Self::U64(u64::try_from(value).unwrap_or(u64::MAX))
    }
}

impl From<bool> for Value {
    fn from(value: bool) -> Self {
        Self::Bool(value)
    }
}

impl From<&'static str> for Value {
    fn from(value: &'static str) -> Self {
        Self::Text(Cow::Borrowed(value))
    }
}

impl From<String> for Value {
    fn from(value: String) -> Self {
        Self::Text(Cow::Owned(value))
    }
}

pub type Fields = Vec<(Cow<'static, str>, Value)>;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Span,
    Instant,
    FlowStart,
    FlowStep,
    FlowEnd,
    Counter,
    Metadata,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Event {
    pub schema: u8,
    pub run: String,
    pub process: String,
    pub pid: u32,
    pub thread: String,
    pub ts_ns: u64,
    pub cpu_ns: u64,
    pub kind: Kind,
    pub category: Cow<'static, str>,
    pub name: Cow<'static, str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ns: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_duration_ns: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fields: Fields,
}

struct Recorder {
    sender: SyncSender<WriterMessage>,
    run: String,
    process: String,
    dropped: AtomicU64,
}

enum WriterMessage {
    Event(Event),
    Flush(mpsc::SyncSender<()>),
}

fn clock_ns(id: ClockId) -> u64 {
    let timestamp = clock_gettime(id);
    u64::try_from(timestamp.tv_sec)
        .unwrap_or_default()
        .saturating_mul(1_000_000_000)
        .saturating_add(u64::try_from(timestamp.tv_nsec).unwrap_or_default())
}

#[must_use]
pub fn monotonic_ns() -> u64 {
    clock_ns(ClockId::Monotonic)
}

#[must_use]
pub fn thread_cpu_ns() -> u64 {
    clock_ns(ClockId::ThreadCPUTime)
}

#[must_use]
pub fn next_id() -> u64 {
    u64::from(process::id())
        .wrapping_shl(32)
        .wrapping_add(NEXT_ID.fetch_add(1, Ordering::Relaxed) & u64::from(u32::MAX))
}

#[must_use]
pub fn enabled() -> bool {
    recorder().is_some()
}

fn recorder() -> Option<&'static Recorder> {
    RECORDER
        .get_or_init(|| {
            let directory = std::env::var_os("BROOD_PROFILE_DIR").map(PathBuf::from)?;
            let process_name = std::env::var("BROOD_PROFILE_PROCESS").unwrap_or_else(|_| {
                std::env::args()
                    .next()
                    .and_then(|path| {
                        PathBuf::from(path)
                            .file_name()
                            .map(|name| name.to_string_lossy().into_owned())
                    })
                    .unwrap_or_else(|| "brood".to_string())
            });
            let run = std::env::var("BROOD_PROFILE_RUN").unwrap_or_else(|_| "default".to_string());
            if let Err(error) = create_dir_all(&directory) {
                eprintln!(
                    "profiler disabled: cannot create {}: {error}",
                    directory.display()
                );
                return None;
            }
            let path = directory.join(format!(
                "{}-{}.jsonl",
                sanitize(&process_name),
                process::id()
            ));
            let file = match File::create(&path) {
                Ok(file) => file,
                Err(error) => {
                    eprintln!(
                        "profiler disabled: cannot create {}: {error}",
                        path.display()
                    );
                    return None;
                }
            };
            let (sender, receiver) = mpsc::sync_channel::<WriterMessage>(CHANNEL_CAPACITY);
            let writer_name = format!("profile-writer-{}", process::id());
            if let Err(error) = thread::Builder::new().name(writer_name).spawn(move || {
                let mut writer = BufWriter::with_capacity(1024 * 1024, file);
                loop {
                    match receiver.recv_timeout(Duration::from_millis(100)) {
                        Ok(WriterMessage::Event(event)) => {
                            if serde_json::to_writer(&mut writer, &event).is_err()
                                || writer.write_all(b"\n").is_err()
                            {
                                break;
                            }
                        }
                        Ok(WriterMessage::Flush(acknowledge)) => {
                            drop(writer.flush());
                            if acknowledge.try_send(()).is_err() {}
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => {
                            drop(writer.flush());
                        }
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                }
                drop(writer.flush());
            }) {
                eprintln!("profiler disabled: cannot start writer: {error}");
                return None;
            }
            Some(Recorder {
                sender,
                run,
                process: process_name,
                dropped: AtomicU64::new(0),
            })
        })
        .as_ref()
}

fn sanitize(name: &str) -> String {
    name.chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect()
}

#[allow(
    clippy::too_many_arguments,
    reason = "the recorder's fixed event columns remain explicit at its single serialization boundary"
)]
fn emit_at(
    timestamp_ns: u64,
    cpu_ns: u64,
    kind: Kind,
    category: Cow<'static, str>,
    name: Cow<'static, str>,
    id: Option<u64>,
    duration_ns: Option<u64>,
    cpu_duration_ns: Option<u64>,
    mut fields: Fields,
) {
    let Some(recorder) = recorder() else {
        return;
    };
    let dropped = recorder.dropped.swap(0, Ordering::Relaxed);
    if dropped > 0 {
        fields.push((Cow::Borrowed("recorder_dropped"), Value::U64(dropped)));
    }
    let event = Event {
        schema: SCHEMA_VERSION,
        run: recorder.run.clone(),
        process: recorder.process.clone(),
        pid: process::id(),
        thread: thread::current().name().unwrap_or("unnamed").to_string(),
        ts_ns: timestamp_ns,
        cpu_ns,
        kind,
        category,
        name,
        id,
        duration_ns,
        cpu_duration_ns,
        fields,
    };
    if recorder
        .sender
        .try_send(WriterMessage::Event(event))
        .is_err()
    {
        recorder.dropped.fetch_add(1, Ordering::Relaxed);
    }
}

/// Flush every event accepted before this call to stable kernel buffers.
///
/// Applications call this after their workers stop and before returning from
/// `main`; the bounded timeout prevents a failed writer from hanging shutdown.
pub fn flush() {
    let Some(recorder) = RECORDER.get().and_then(Option::as_ref) else {
        return;
    };
    let (sender, receiver) = mpsc::sync_channel(1);
    if recorder.sender.send(WriterMessage::Flush(sender)).is_err() {
        return;
    }
    match receiver.recv_timeout(Duration::from_secs(2)) {
        Ok(()) | Err(_) => {}
    }
}

pub fn instant(category: &'static str, name: &'static str, id: Option<u64>, fields: Fields) {
    emit_at(
        monotonic_ns(),
        thread_cpu_ns(),
        Kind::Instant,
        Cow::Borrowed(category),
        Cow::Borrowed(name),
        id,
        None,
        None,
        fields,
    );
}

pub fn flow(kind: Kind, category: &'static str, name: &'static str, id: u64, fields: Fields) {
    debug_assert!(matches!(
        kind,
        Kind::FlowStart | Kind::FlowStep | Kind::FlowEnd
    ));
    emit_at(
        monotonic_ns(),
        thread_cpu_ns(),
        kind,
        Cow::Borrowed(category),
        Cow::Borrowed(name),
        Some(id),
        None,
        None,
        fields,
    );
}

pub fn counter(category: &'static str, name: &'static str, value: u64, fields: Fields) {
    let mut fields = fields;
    fields.push((Cow::Borrowed("value"), Value::U64(value)));
    emit_at(
        monotonic_ns(),
        thread_cpu_ns(),
        Kind::Counter,
        Cow::Borrowed(category),
        Cow::Borrowed(name),
        None,
        None,
        None,
        fields,
    );
}

#[must_use]
pub struct Span {
    category: &'static str,
    name: &'static str,
    id: Option<u64>,
    started_ns: u64,
    started_cpu_ns: u64,
    measure_cpu: bool,
    fields: Fields,
    active: bool,
}

impl Span {
    pub fn new(
        category: &'static str,
        name: &'static str,
        id: Option<u64>,
        fields: Fields,
    ) -> Self {
        let active = recorder().is_some();
        Self {
            category,
            name,
            id,
            started_ns: if active { monotonic_ns() } else { 0 },
            started_cpu_ns: if active { thread_cpu_ns() } else { 0 },
            measure_cpu: true,
            fields,
            active,
        }
    }

    /// A wall-time span which may cross executor threads.
    pub fn asynchronous(
        category: &'static str,
        name: &'static str,
        id: Option<u64>,
        fields: Fields,
    ) -> Self {
        let active = recorder().is_some();
        Self {
            category,
            name,
            id,
            started_ns: if active { monotonic_ns() } else { 0 },
            started_cpu_ns: 0,
            measure_cpu: false,
            fields,
            active,
        }
    }
}

impl Drop for Span {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let ended_ns = monotonic_ns();
        let ended_cpu_ns = if self.measure_cpu { thread_cpu_ns() } else { 0 };
        emit_at(
            self.started_ns,
            self.started_cpu_ns,
            Kind::Span,
            Cow::Borrowed(self.category),
            Cow::Borrowed(self.name),
            self.id,
            Some(ended_ns.saturating_sub(self.started_ns)),
            self.measure_cpu
                .then(|| ended_cpu_ns.saturating_sub(self.started_cpu_ns)),
            std::mem::take(&mut self.fields),
        );
    }
}

#[macro_export]
macro_rules! profile_span {
    ($category:literal, $name:literal) => {
        $crate::Span::new($category, $name, None, Vec::new())
    };
    ($category:literal, $name:literal, id = $id:expr) => {
        $crate::Span::new($category, $name, Some($id), Vec::new())
    };
}

#[macro_export]
macro_rules! profile_instant {
    ($category:literal, $name:literal) => {
        $crate::instant($category, $name, None, Vec::new())
    };
    ($category:literal, $name:literal, id = $id:expr) => {
        $crate::instant($category, $name, Some($id), Vec::new())
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monotonic_and_cpu_clocks_advance() {
        let before = monotonic_ns();
        let cpu_before = thread_cpu_ns();
        std::hint::black_box((0..10_000_u64).sum::<u64>());
        assert!(monotonic_ns() >= before);
        assert!(thread_cpu_ns() >= cpu_before);
        let id = next_id();
        flow(Kind::FlowStart, "input", "key_received", id, Vec::new());
        {
            let _span = Span::new("test", "work", Some(id), Vec::new());
            std::hint::black_box((0..10_000_u64).sum::<u64>());
        }
        flow(Kind::FlowEnd, "input", "presented", id, Vec::new());
        instant("frame", "presented", Some(id), Vec::new());
        flush();
    }
}

pub mod cli;
