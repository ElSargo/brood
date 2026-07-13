use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender, TryRecvError, TrySendError},
    },
    thread,
    time::Duration,
};

use anyhow::{Context, Result};
use wasmtime::{Caller, Config, Engine, Linker, Memory, Module, Store, TypedFunc};

use crate::control;

const MAX_HARNESS_COMMAND_BYTES: usize = 64 * 1024;
const HARNESS_WAIT_POLL: Duration = Duration::from_millis(25);
static NEXT_HARNESS_CHILD_ID: AtomicU64 = AtomicU64::new(1);

fn allocate_harness_child_id() -> Result<TestHarnessChildId> {
    NEXT_HARNESS_CHILD_ID
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            current.checked_add(1)
        })
        .map(TestHarnessChildId)
        .map_err(|_current| anyhow::anyhow!("test harness child identity space is exhausted"))
}

pub enum TestHarnessRequest {
    CommandJson {
        input: String,
        response: Sender<std::result::Result<String, String>>,
    },
    StatusJson {
        response: Sender<std::result::Result<String, String>>,
    },
}

pub enum TestHarnessSignal {
    Named(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TestHarnessOutcomeKind {
    Passed,
    Failed,
    Paused,
}

impl TestHarnessOutcomeKind {
    const fn from_exit_code(code: i32) -> Self {
        match code {
            0 => Self::Passed,
            2 => Self::Paused,
            _ => Self::Failed,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestHarnessCompletion {
    pub kind: TestHarnessOutcomeKind,
    pub exit_code: i32,
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TestHarnessChildId(u64);

#[derive(Debug, Clone, PartialEq, Eq)]
enum TestHarnessChildExit {
    Completed(TestHarnessCompletion),
    Failed(String),
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TestHarnessLifecycleEvent {
    Started(TestHarnessChildId),
    Exited {
        child: TestHarnessChildId,
        outcome: TestHarnessChildExit,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TestHarnessCleanupResult {
    Joined {
        child: TestHarnessChildId,
        cancellation_requested: bool,
    },
    AlreadyJoined {
        child: TestHarnessChildId,
    },
    Panicked {
        child: TestHarnessChildId,
        detail: String,
    },
}

impl TestHarnessCleanupResult {
    pub const fn is_success(&self) -> bool {
        matches!(self, Self::Joined { .. } | Self::AlreadyJoined { .. })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TestHarnessLifecyclePhase {
    Starting,
    Running,
    Exited,
}

struct TestHarnessLifecycle {
    child: TestHarnessChildId,
    phase: TestHarnessLifecyclePhase,
}

impl TestHarnessLifecycle {
    const fn new(child: TestHarnessChildId) -> Self {
        Self {
            child,
            phase: TestHarnessLifecyclePhase::Starting,
        }
    }

    fn apply(&mut self, event: TestHarnessLifecycleEvent) -> Result<Option<TestHarnessChildExit>> {
        match event {
            TestHarnessLifecycleEvent::Started(child) => {
                anyhow::ensure!(child == self.child, "test harness child identity changed");
                anyhow::ensure!(
                    self.phase == TestHarnessLifecyclePhase::Starting,
                    "test harness started outside its starting phase"
                );
                self.phase = TestHarnessLifecyclePhase::Running;
                Ok(None)
            }
            TestHarnessLifecycleEvent::Exited { child, outcome } => {
                anyhow::ensure!(child == self.child, "test harness child identity changed");
                anyhow::ensure!(
                    self.phase == TestHarnessLifecyclePhase::Running,
                    "test harness exited outside its running phase"
                );
                self.phase = TestHarnessLifecyclePhase::Exited;
                Ok(Some(outcome))
            }
        }
    }

    fn has_exited(&self) -> bool {
        self.phase == TestHarnessLifecyclePhase::Exited
    }

    fn channel_disconnected(&mut self) -> Option<TestHarnessChildExit> {
        if self.has_exited() {
            return None;
        }
        self.phase = TestHarnessLifecyclePhase::Exited;
        Some(TestHarnessChildExit::Failed(
            "test harness lifecycle channel disconnected before an exit event".to_string(),
        ))
    }
}

struct TestHarnessCancellation {
    requested: AtomicBool,
    engine: Engine,
    wake_lock: Mutex<()>,
    wake: Condvar,
}

impl TestHarnessCancellation {
    const fn new(engine: Engine) -> Self {
        Self {
            requested: AtomicBool::new(false),
            engine,
            wake_lock: Mutex::new(()),
            wake: Condvar::new(),
        }
    }

    fn request(&self) {
        self.requested.store(true, Ordering::Release);
        self.engine.increment_epoch();
        self.wake.notify_all();
    }

    fn ensure_running(&self) -> Result<()> {
        anyhow::ensure!(
            !self.requested.load(Ordering::Acquire),
            "test harness child was cancelled"
        );
        Ok(())
    }

    fn sleep(&self, duration: Duration) -> Result<()> {
        self.ensure_running()?;
        let wait_result = {
            let guard = self.wake_lock.lock().map_err(|_poison| {
                anyhow::anyhow!("test harness cancellation lock was poisoned")
            })?;
            self.wake
                .wait_timeout_while(guard, duration, |()| {
                    !self.requested.load(Ordering::Acquire)
                })
                .map_err(|_poison| anyhow::anyhow!("test harness cancellation lock was poisoned"))?
        };
        drop(wait_result);
        self.ensure_running()
    }
}

struct TestHarnessChildHandle {
    id: TestHarnessChildId,
    join: Option<thread::JoinHandle<()>>,
    cancellation: Arc<TestHarnessCancellation>,
}

impl TestHarnessChildHandle {
    fn cleanup(&mut self, request_cancellation: bool) -> TestHarnessCleanupResult {
        if request_cancellation {
            self.cancellation.request();
        }
        let Some(join) = self.join.take() else {
            return TestHarnessCleanupResult::AlreadyJoined { child: self.id };
        };
        match join.join() {
            Ok(()) => TestHarnessCleanupResult::Joined {
                child: self.id,
                cancellation_requested: request_cancellation,
            },
            Err(payload) => TestHarnessCleanupResult::Panicked {
                child: self.id,
                detail: panic_detail(payload.as_ref()),
            },
        }
    }
}

fn panic_detail(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "test harness child panicked".to_string()
    }
}

pub struct TestHarnessRuntime {
    requests: Receiver<TestHarnessRequest>,
    lifecycle_events: Receiver<TestHarnessLifecycleEvent>,
    signal_sender: SyncSender<TestHarnessSignal>,
    lifecycle: TestHarnessLifecycle,
    child: TestHarnessChildHandle,
    completion_delivered: bool,
}

impl TestHarnessRuntime {
    pub fn start(path: &Path) -> Result<Self> {
        let (requests_tx, requests_rx) = mpsc::channel();
        let (lifecycle_tx, lifecycle_rx) = mpsc::channel();
        let (signal_tx, signal_rx) = mpsc::sync_channel(64);
        let path = path.to_path_buf();
        let engine = harness_engine()?;
        let cancellation = Arc::new(TestHarnessCancellation::new(engine.clone()));
        let child_id = allocate_harness_child_id()?;
        let child_cancellation = Arc::clone(&cancellation);
        let join = thread::Builder::new()
            .name(format!("manifold-test-harness-{}", child_id.0))
            .spawn(move || {
                if lifecycle_tx
                    .send(TestHarnessLifecycleEvent::Started(child_id))
                    .is_err()
                {
                    return;
                }
                let outcome = match run_harness_child(
                    &path,
                    &engine,
                    requests_tx,
                    signal_rx,
                    Arc::clone(&child_cancellation),
                ) {
                    Ok(completion) => TestHarnessChildExit::Completed(completion),
                    Err(_err) if child_cancellation.requested.load(Ordering::Acquire) => {
                        TestHarnessChildExit::Cancelled
                    }
                    Err(err) => TestHarnessChildExit::Failed(err.to_string()),
                };
                if lifecycle_tx
                    .send(TestHarnessLifecycleEvent::Exited {
                        child: child_id,
                        outcome,
                    })
                    .is_err()
                {
                    tracing::debug!(child = child_id.0, "test harness lifecycle receiver closed");
                }
            })
            .context("spawn test harness child thread")?;

        Ok(Self {
            requests: requests_rx,
            lifecycle_events: lifecycle_rx,
            signal_sender: signal_tx,
            lifecycle: TestHarnessLifecycle::new(child_id),
            child: TestHarnessChildHandle {
                id: child_id,
                join: Some(join),
                cancellation,
            },
            completion_delivered: false,
        })
    }

    pub fn take_pending_requests(&self) -> Vec<TestHarnessRequest> {
        let mut requests = Vec::new();
        while let Ok(request) = self.requests.try_recv() {
            requests.push(request);
        }
        requests
    }

    pub fn try_take_completion(&mut self) -> Result<Option<TestHarnessCompletion>> {
        if self.completion_delivered {
            return Ok(None);
        }
        loop {
            match self.lifecycle_events.try_recv() {
                Ok(event) => {
                    if let Some(outcome) = self.lifecycle.apply(event)? {
                        return Ok(self.deliver_outcome(outcome));
                    }
                }
                Err(TryRecvError::Empty) => return Ok(None),
                Err(TryRecvError::Disconnected) => {
                    let Some(outcome) = self.lifecycle.channel_disconnected() else {
                        return Ok(None);
                    };
                    return Ok(self.deliver_outcome(outcome));
                }
            }
        }
    }

    fn deliver_outcome(&mut self, outcome: TestHarnessChildExit) -> Option<TestHarnessCompletion> {
        let completion = match outcome {
            TestHarnessChildExit::Completed(completion) => completion,
            TestHarnessChildExit::Failed(detail) => TestHarnessCompletion {
                kind: TestHarnessOutcomeKind::Failed,
                exit_code: 1,
                detail,
            },
            TestHarnessChildExit::Cancelled => return None,
        };
        self.completion_delivered = true;
        Some(completion)
    }

    pub fn send_signal(&self, name: String) -> Result<()> {
        match self.signal_sender.try_send(TestHarnessSignal::Named(name)) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => anyhow::bail!("test harness signal queue is full"),
            Err(TrySendError::Disconnected(_)) => {
                anyhow::bail!("test harness signal receiver is closed")
            }
        }
    }

    pub fn cleanup(&mut self) -> TestHarnessCleanupResult {
        let request_cancellation = !self.lifecycle.has_exited();
        self.child.cleanup(request_cancellation)
    }
}

impl Drop for TestHarnessRuntime {
    fn drop(&mut self) {
        if self.child.join.is_some() {
            let cleanup = self.cleanup();
            if !cleanup.is_success() {
                tracing::error!(?cleanup, "test harness fallback cleanup failed");
            }
        }
    }
}

#[derive(Default)]
struct TestHarnessStore {
    request_sender: Option<Sender<TestHarnessRequest>>,
    signal_receiver: Option<Receiver<TestHarnessSignal>>,
    cancellation: Option<Arc<TestHarnessCancellation>>,
}

fn harness_engine() -> Result<Engine> {
    let mut config = Config::new();
    config.epoch_interruption(true);
    Engine::new(&config).map_err(|err| anyhow::anyhow!("create test harness engine: {err}"))
}

fn run_harness_child(
    path: &PathBuf,
    engine: &Engine,
    requests_tx: Sender<TestHarnessRequest>,
    signal_rx: Receiver<TestHarnessSignal>,
    cancellation: Arc<TestHarnessCancellation>,
) -> Result<TestHarnessCompletion> {
    let module = Module::from_file(engine, path)
        .map_err(|err| anyhow::anyhow!("load {}: {err}", path.display()))?;
    let mut linker = Linker::new(engine);

    linker
        .func_wrap(
            "manifold",
            "sleep_ms",
            |caller: Caller<'_, TestHarnessStore>, millis: i32| {
                let millis = u64::try_from(millis.max(0)).unwrap_or(0);
                let Some(cancellation) = caller.data().cancellation.as_ref() else {
                    return;
                };
                if let Err(err) = cancellation.sleep(Duration::from_millis(millis)) {
                    tracing::debug!(?err, "cancelled test harness sleep");
                }
            },
        )
        .map_err(|err| anyhow::anyhow!("define manifold.sleep_ms import: {err}"))?;
    linker
        .func_wrap(
            "manifold",
            "log",
            |mut caller: Caller<'_, TestHarnessStore>, ptr: i32, len: i32| -> i32 {
                match read_guest_string(&mut caller, ptr, len) {
                    Ok(message) => {
                        tracing::info!(target: "manifoldd::test_harness", "{message}");
                        1
                    }
                    Err(err) => {
                        tracing::warn!(?err, "test harness passed invalid log message");
                        0
                    }
                }
            },
        )
        .map_err(|err| anyhow::anyhow!("define manifold.log import: {err}"))?;
    linker
        .func_wrap(
            "manifold",
            "wait_for_signal",
            |mut caller: Caller<'_, TestHarnessStore>, resp_ptr: i32, resp_cap: i32| -> i32 {
                write_guest_response(
                    &mut caller,
                    resp_ptr,
                    resp_cap,
                    |_request_sender, signal_receiver, cancellation| {
                        let signal = receive_until_cancelled(signal_receiver, cancellation)?;
                        Ok(match signal {
                            TestHarnessSignal::Named(name) => name,
                        })
                    },
                )
            },
        )
        .map_err(|err| anyhow::anyhow!("define manifold.wait_for_signal import: {err}"))?;
    linker
        .func_wrap(
            "manifold",
            "control_json",
            |mut caller: Caller<'_, TestHarnessStore>,
             req_ptr: i32,
             req_len: i32,
             resp_ptr: i32,
             resp_cap: i32|
             -> i32 {
                round_trip_guest_string(
                    &mut caller,
                    req_ptr,
                    req_len,
                    resp_ptr,
                    resp_cap,
                    |input, request_sender, _signal_receiver, cancellation| {
                        let (response_tx, response_rx) = mpsc::channel();
                        request_sender
                            .send(TestHarnessRequest::CommandJson {
                                input,
                                response: response_tx,
                            })
                            .map_err(|err| {
                                anyhow::anyhow!("send control command request: {err}")
                            })?;
                        receive_until_cancelled(&response_rx, cancellation)?
                            .map_err(anyhow::Error::msg)
                    },
                )
            },
        )
        .map_err(|err| anyhow::anyhow!("define manifold.control_json import: {err}"))?;
    linker
        .func_wrap(
            "manifold",
            "status_json",
            |mut caller: Caller<'_, TestHarnessStore>, resp_ptr: i32, resp_cap: i32| -> i32 {
                write_guest_response(
                    &mut caller,
                    resp_ptr,
                    resp_cap,
                    |request_sender, _signal_receiver, cancellation| {
                        let (response_tx, response_rx) = mpsc::channel();
                        request_sender
                            .send(TestHarnessRequest::StatusJson {
                                response: response_tx,
                            })
                            .map_err(|err| anyhow::anyhow!("send status request: {err}"))?;
                        receive_until_cancelled(&response_rx, cancellation)?
                            .map_err(anyhow::Error::msg)
                    },
                )
            },
        )
        .map_err(|err| anyhow::anyhow!("define manifold.status_json import: {err}"))?;

    let mut store = Store::new(
        engine,
        TestHarnessStore {
            request_sender: Some(requests_tx),
            signal_receiver: Some(signal_rx),
            cancellation: Some(cancellation),
        },
    );
    store.set_epoch_deadline(1);
    store.epoch_deadline_trap();
    store
        .data()
        .cancellation
        .as_ref()
        .context("missing test harness cancellation handle before instantiation")?
        .ensure_running()?;
    let instance = linker
        .instantiate(&mut store, &module)
        .map_err(|err| anyhow::anyhow!("instantiate test harness: {err}"))?;
    let run = instance
        .get_typed_func::<(), i32>(&mut store, "run")
        .map_err(|err| anyhow::anyhow!("find exported run(): {err}"))?;
    let exit_code = call_harness_run(&mut store, &run)?;
    Ok(TestHarnessCompletion {
        kind: TestHarnessOutcomeKind::from_exit_code(exit_code),
        exit_code,
        detail: format!("harness exited with code {exit_code}"),
    })
}

fn call_harness_run(store: &mut Store<TestHarnessStore>, run: &TypedFunc<(), i32>) -> Result<i32> {
    store
        .data()
        .cancellation
        .as_ref()
        .context("missing test harness cancellation handle before run")?
        .ensure_running()?;
    run.call(store, ())
        .map_err(|err| anyhow::anyhow!("call test harness run(): {err}"))
}

fn receive_until_cancelled<T>(
    receiver: &Receiver<T>,
    cancellation: &TestHarnessCancellation,
) -> Result<T> {
    loop {
        cancellation.ensure_running()?;
        match receiver.recv_timeout(HARNESS_WAIT_POLL) {
            Ok(value) => return Ok(value),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                anyhow::bail!("test harness host channel disconnected")
            }
        }
    }
}

fn round_trip_guest_string(
    caller: &mut Caller<'_, TestHarnessStore>,
    req_ptr: i32,
    req_len: i32,
    resp_ptr: i32,
    resp_cap: i32,
    f: impl FnOnce(
        String,
        &Sender<TestHarnessRequest>,
        &Receiver<TestHarnessSignal>,
        &TestHarnessCancellation,
    ) -> Result<String>,
) -> i32 {
    let input = match read_guest_string(caller, req_ptr, req_len) {
        Ok(input) => input,
        Err(err) => return log_failure(&err, "test harness passed invalid request"),
    };
    write_guest_response(
        caller,
        resp_ptr,
        resp_cap,
        |request_sender, signal_receiver, cancellation| {
            f(input, request_sender, signal_receiver, cancellation)
        },
    )
}

fn write_guest_response(
    caller: &mut Caller<'_, TestHarnessStore>,
    resp_ptr: i32,
    resp_cap: i32,
    f: impl FnOnce(
        &Sender<TestHarnessRequest>,
        &Receiver<TestHarnessSignal>,
        &TestHarnessCancellation,
    ) -> Result<String>,
) -> i32 {
    let Some(request_sender) = caller.data().request_sender.as_ref() else {
        return log_failure(
            &anyhow::anyhow!("missing request sender"),
            "test harness runtime unavailable",
        );
    };
    let Some(signal_receiver) = caller.data().signal_receiver.as_ref() else {
        return log_failure(
            &anyhow::anyhow!("missing signal receiver"),
            "test harness runtime unavailable",
        );
    };
    let Some(cancellation) = caller.data().cancellation.as_ref() else {
        return log_failure(
            &anyhow::anyhow!("missing cancellation handle"),
            "test harness runtime unavailable",
        );
    };
    let response = match f(request_sender, signal_receiver, cancellation) {
        Ok(response) => response,
        Err(err) => return log_failure(&err, "test harness host request failed"),
    };
    match write_guest_bytes(caller, resp_ptr, resp_cap, response.as_bytes()) {
        Ok(written) => written,
        Err(err) => log_failure(&err, "test harness response write failed"),
    }
}

fn log_failure(err: &anyhow::Error, message: &str) -> i32 {
    tracing::warn!(?err, "{message}");
    -1
}

fn read_guest_string(
    caller: &mut Caller<'_, TestHarnessStore>,
    ptr: i32,
    len: i32,
) -> Result<String> {
    anyhow::ensure!(ptr >= 0, "guest string pointer must be non-negative");
    anyhow::ensure!(len >= 0, "guest string length must be non-negative");
    let len = usize::try_from(len).context("convert guest string length")?;
    anyhow::ensure!(
        len <= MAX_HARNESS_COMMAND_BYTES,
        "guest request exceeds {MAX_HARNESS_COMMAND_BYTES} bytes"
    );
    let memory = guest_memory(caller)?;
    let ptr = usize::try_from(ptr).context("convert guest string pointer")?;
    let mut bytes = vec![0; len];
    memory
        .read(caller, ptr, &mut bytes)
        .context("read guest memory")?;
    String::from_utf8(bytes).context("guest string must be UTF-8")
}

fn write_guest_bytes(
    caller: &mut Caller<'_, TestHarnessStore>,
    ptr: i32,
    cap: i32,
    bytes: &[u8],
) -> Result<i32> {
    anyhow::ensure!(ptr >= 0, "guest buffer pointer must be non-negative");
    anyhow::ensure!(cap >= 0, "guest buffer capacity must be non-negative");
    let cap = usize::try_from(cap).context("convert guest buffer capacity")?;
    anyhow::ensure!(
        bytes.len() <= cap,
        "guest buffer too small: need {} bytes but buffer has {cap}",
        bytes.len()
    );
    let memory = guest_memory(caller)?;
    let ptr = usize::try_from(ptr).context("convert guest buffer pointer")?;
    memory
        .write(caller, ptr, bytes)
        .context("write guest memory")?;
    i32::try_from(bytes.len()).context("convert response length to i32")
}

fn guest_memory(caller: &mut Caller<'_, TestHarnessStore>) -> Result<Memory> {
    caller
        .get_export("memory")
        .and_then(wasmtime::Extern::into_memory)
        .context("guest must export memory")
}

pub fn service_request(state: &mut crate::state::Manifold, request: TestHarnessRequest) {
    match request {
        TestHarnessRequest::CommandJson { input, response } => {
            let result =
                control::execute_json_command(&input, state).map_err(|err| err.to_string());
            if response.send(result).is_err() {
                tracing::debug!("test harness dropped its command response receiver");
            }
        }
        TestHarnessRequest::StatusJson { response } => {
            let result = control::status_json_string(state).map_err(|err| err.to_string());
            if response.send(result).is_err() {
                tracing::debug!("test harness dropped its status response receiver");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        TestHarnessCancellation, TestHarnessChildExit, TestHarnessChildHandle, TestHarnessChildId,
        TestHarnessCleanupResult, TestHarnessCompletion, TestHarnessLifecycle,
        TestHarnessLifecycleEvent, TestHarnessOutcomeKind, TestHarnessRuntime, TestHarnessStore,
        call_harness_run, harness_engine, run_harness_child,
    };
    use anyhow::Context as _;
    use std::{
        fs,
        sync::{Arc, mpsc},
        thread,
        time::Duration,
    };
    use wasmtime::{Linker, Module, Store};

    #[test]
    fn lifecycle_requires_start_before_one_exact_exit() -> anyhow::Result<()> {
        let child = TestHarnessChildId(41);
        let mut lifecycle = TestHarnessLifecycle::new(child);
        let completion = TestHarnessCompletion {
            kind: TestHarnessOutcomeKind::Passed,
            exit_code: 0,
            detail: "finished".to_string(),
        };

        anyhow::ensure!(
            lifecycle
                .apply(TestHarnessLifecycleEvent::Exited {
                    child,
                    outcome: TestHarnessChildExit::Completed(completion.clone()),
                })
                .is_err()
        );
        anyhow::ensure!(lifecycle.apply(TestHarnessLifecycleEvent::Started(child))? == None);
        anyhow::ensure!(
            lifecycle.apply(TestHarnessLifecycleEvent::Exited {
                child,
                outcome: TestHarnessChildExit::Completed(completion.clone()),
            })? == Some(TestHarnessChildExit::Completed(completion))
        );
        anyhow::ensure!(
            lifecycle
                .apply(TestHarnessLifecycleEvent::Exited {
                    child,
                    outcome: TestHarnessChildExit::Cancelled,
                })
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn lifecycle_rejects_events_for_another_child() {
        let mut lifecycle = TestHarnessLifecycle::new(TestHarnessChildId(41));
        assert!(
            lifecycle
                .apply(TestHarnessLifecycleEvent::Started(TestHarnessChildId(42)))
                .is_err()
        );
    }

    #[test]
    fn pre_start_cancellation_prevents_guest_preparation() -> anyhow::Result<()> {
        let engine = harness_engine()?;
        let cancellation = Arc::new(TestHarnessCancellation::new(engine.clone()));
        cancellation.request();
        let artifact = std::env::temp_dir().join(format!(
            "manifold-harness-cancel-before-instantiate-{}.wasm",
            std::process::id()
        ));
        fs::write(
            &artifact,
            wat::parse_str(
                r#"(module
                    (func $must_not_start unreachable)
                    (start $must_not_start)
                    (func (export "run") (result i32) i32.const 0))"#,
            )?,
        )?;
        let (request_tx, _request_rx) = mpsc::channel();
        let (_signal_tx, signal_rx) = mpsc::sync_channel(1);
        let result = run_harness_child(&artifact, &engine, request_tx, signal_rx, cancellation);
        fs::remove_file(&artifact)?;
        let Err(error) = result else {
            anyhow::bail!("cancelled harness entered guest instantiation");
        };
        anyhow::ensure!(error.to_string().contains("cancelled"));
        Ok(())
    }

    #[test]
    fn cancellation_between_instantiation_and_run_prevents_guest_call() -> anyhow::Result<()> {
        let engine = harness_engine()?;
        let cancellation = Arc::new(TestHarnessCancellation::new(engine.clone()));
        let module = Module::new(
            &engine,
            r#"(module (func (export "run") (result i32) i32.const 77))"#,
        )?;
        let linker = Linker::new(&engine);
        let mut store = Store::new(
            &engine,
            TestHarnessStore {
                cancellation: Some(Arc::clone(&cancellation)),
                ..TestHarnessStore::default()
            },
        );
        store.set_epoch_deadline(1);
        store.epoch_deadline_trap();
        let instance = linker.instantiate(&mut store, &module)?;
        let run = instance.get_typed_func::<(), i32>(&mut store, "run")?;
        cancellation.request();

        let Err(error) = call_harness_run(&mut store, &run) else {
            anyhow::bail!("cancelled harness entered its run export");
        };
        anyhow::ensure!(error.to_string().contains("cancelled"));
        Ok(())
    }

    #[test]
    fn disconnected_lifecycle_channel_becomes_typed_failure() -> anyhow::Result<()> {
        let (_request_tx, request_rx) = mpsc::channel();
        let (lifecycle_tx, lifecycle_rx) = mpsc::channel();
        drop(lifecycle_tx);
        let (signal_tx, _signal_rx) = mpsc::sync_channel(1);
        let child = TestHarnessChildId(55);
        let cancellation = Arc::new(TestHarnessCancellation::new(harness_engine()?));
        let mut runtime = TestHarnessRuntime {
            requests: request_rx,
            lifecycle_events: lifecycle_rx,
            signal_sender: signal_tx,
            lifecycle: TestHarnessLifecycle::new(child),
            child: TestHarnessChildHandle {
                id: child,
                join: Some(thread::spawn(|| {})),
                cancellation,
            },
            completion_delivered: false,
        };

        let completion = runtime
            .try_take_completion()?
            .context("disconnected lifecycle must produce a completion")?;
        anyhow::ensure!(completion.kind == TestHarnessOutcomeKind::Failed);
        anyhow::ensure!(completion.detail.contains("disconnected"));
        anyhow::ensure!(runtime.try_take_completion()?.is_none());
        Ok(())
    }

    #[test]
    fn cleanup_wakes_and_joins_a_blocked_child() -> anyhow::Result<()> {
        let cancellation = Arc::new(TestHarnessCancellation::new(harness_engine()?));
        let child_cancellation = Arc::clone(&cancellation);
        let join = thread::spawn(move || {
            let Err(error) = child_cancellation.sleep(Duration::from_mins(1)) else {
                std::panic::resume_unwind(Box::new(
                    "cleanup did not cancel the blocked child".to_string(),
                ));
            };
            if !error.to_string().contains("cancelled") {
                std::panic::resume_unwind(Box::new(
                    "blocked child returned a non-cancellation error".to_string(),
                ));
            }
        });
        let mut child = TestHarnessChildHandle {
            id: TestHarnessChildId(7),
            join: Some(join),
            cancellation,
        };

        anyhow::ensure!(
            child.cleanup(true)
                == TestHarnessCleanupResult::Joined {
                    child: TestHarnessChildId(7),
                    cancellation_requested: true,
                }
        );
        anyhow::ensure!(
            child.cleanup(true)
                == TestHarnessCleanupResult::AlreadyJoined {
                    child: TestHarnessChildId(7),
                }
        );
        Ok(())
    }

    #[test]
    fn cleanup_reports_child_panic() -> anyhow::Result<()> {
        let cancellation = Arc::new(TestHarnessCancellation::new(harness_engine()?));
        let join = thread::spawn(|| {
            std::panic::resume_unwind(Box::new("deliberate harness panic".to_string()));
        });
        let mut child = TestHarnessChildHandle {
            id: TestHarnessChildId(9),
            join: Some(join),
            cancellation,
        };

        anyhow::ensure!(
            child.cleanup(false)
                == TestHarnessCleanupResult::Panicked {
                    child: TestHarnessChildId(9),
                    detail: "deliberate harness panic".to_string(),
                }
        );
        Ok(())
    }
}
