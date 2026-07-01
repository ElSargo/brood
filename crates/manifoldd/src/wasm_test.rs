#![allow(
    clippy::let_underscore_must_use,
    clippy::needless_pass_by_value,
    clippy::while_let_loop,
    reason = "tracked in issues.md: WASM test harness still manages child processes and messages procedurally"
)]
// TODO(lint-structure): model harness process lifecycle with explicit handles and events.

use std::{
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver, Sender, SyncSender, TryRecvError},
    thread,
    time::Duration,
};

use anyhow::{Context, Result};
use wasmtime::{Caller, Engine, Linker, Memory, Module, Store};

use crate::control;

const MAX_HARNESS_COMMAND_BYTES: usize = 64 * 1024;

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

#[derive(Debug)]
pub struct TestHarnessCompletion {
    pub kind: TestHarnessOutcomeKind,
    pub exit_code: i32,
    pub detail: String,
}

pub struct TestHarnessRuntime {
    requests: Receiver<TestHarnessRequest>,
    completion: Receiver<TestHarnessCompletion>,
    signal_sender: SyncSender<TestHarnessSignal>,
    completed: bool,
}

impl TestHarnessRuntime {
    pub fn start(path: &Path) -> Result<Self> {
        let (requests_tx, requests_rx) = mpsc::channel();
        let (completion_tx, completion_rx) = mpsc::channel();
        let (signal_tx, signal_rx) = mpsc::sync_channel(64);
        let path = path.to_path_buf();
        let thread_path = path.clone();
        thread::Builder::new()
            .name("manifold-test-harness".to_string())
            .spawn(move || {
                if let Err(err) =
                    run_harness_thread(&thread_path, requests_tx, signal_rx, completion_tx.clone())
                {
                    let _ = completion_tx.send(TestHarnessCompletion {
                        kind: TestHarnessOutcomeKind::Failed,
                        exit_code: 1,
                        detail: err.to_string(),
                    });
                }
            })
            .with_context(|| format!("spawn test harness thread for {}", path.display()))?;

        Ok(Self {
            requests: requests_rx,
            completion: completion_rx,
            signal_sender: signal_tx,
            completed: false,
        })
    }

    pub fn take_pending_requests(&self) -> Vec<TestHarnessRequest> {
        let mut requests = Vec::new();
        loop {
            match self.requests.try_recv() {
                Ok(request) => requests.push(request),
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            }
        }
        requests
    }

    pub fn try_take_completion(&mut self) -> Option<TestHarnessCompletion> {
        if self.completed {
            return None;
        }
        match self.completion.try_recv() {
            Ok(completion) => {
                self.completed = true;
                Some(completion)
            }
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => None,
        }
    }

    pub fn send_signal(&self, name: String) -> Result<()> {
        self.signal_sender
            .send(TestHarnessSignal::Named(name))
            .context("send test harness signal")
    }
}

#[derive(Default)]
struct TestHarnessStore {
    request_sender: Option<Sender<TestHarnessRequest>>,
    signal_receiver: Option<Receiver<TestHarnessSignal>>,
}

fn run_harness_thread(
    path: &PathBuf,
    requests_tx: Sender<TestHarnessRequest>,
    signal_rx: Receiver<TestHarnessSignal>,
    completion_tx: Sender<TestHarnessCompletion>,
) -> Result<()> {
    let engine = Engine::default();
    let module = Module::from_file(&engine, path)
        .map_err(|err| anyhow::anyhow!("load {}: {err}", path.display()))?;
    let mut linker = Linker::new(&engine);

    linker
        .func_wrap("manifold", "sleep_ms", |millis: i32| {
            let millis = u64::try_from(millis.max(0)).unwrap_or(0);
            thread::sleep(Duration::from_millis(millis));
        })
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
                    |_request_sender, signal_receiver| {
                        let signal = signal_receiver
                            .recv()
                            .map_err(|err| anyhow::anyhow!("receive harness signal: {err}"))?;
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
                    |input, request_sender, _signal_receiver| {
                        let (response_tx, response_rx) = mpsc::channel();
                        request_sender
                            .send(TestHarnessRequest::CommandJson {
                                input,
                                response: response_tx,
                            })
                            .map_err(|err| {
                                anyhow::anyhow!("send control command request: {err}")
                            })?;
                        response_rx
                            .recv()
                            .map_err(|err| {
                                anyhow::anyhow!("receive control command response: {err}")
                            })?
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
                    |request_sender, _signal_receiver| {
                        let (response_tx, response_rx) = mpsc::channel();
                        request_sender
                            .send(TestHarnessRequest::StatusJson {
                                response: response_tx,
                            })
                            .map_err(|err| anyhow::anyhow!("send status request: {err}"))?;
                        response_rx
                            .recv()
                            .map_err(|err| anyhow::anyhow!("receive status response: {err}"))?
                            .map_err(anyhow::Error::msg)
                    },
                )
            },
        )
        .map_err(|err| anyhow::anyhow!("define manifold.status_json import: {err}"))?;

    let mut store = Store::new(
        &engine,
        TestHarnessStore {
            request_sender: Some(requests_tx),
            signal_receiver: Some(signal_rx),
        },
    );
    let instance = linker
        .instantiate(&mut store, &module)
        .map_err(|err| anyhow::anyhow!("instantiate test harness: {err}"))?;
    let run = instance
        .get_typed_func::<(), i32>(&mut store, "run")
        .map_err(|err| anyhow::anyhow!("find exported run(): {err}"))?;
    let exit_code = run
        .call(&mut store, ())
        .map_err(|err| anyhow::anyhow!("call test harness run(): {err}"))?;
    let completion = TestHarnessCompletion {
        kind: TestHarnessOutcomeKind::from_exit_code(exit_code),
        exit_code,
        detail: format!("harness exited with code {exit_code}"),
    };
    completion_tx
        .send(completion)
        .context("send test harness completion")?;
    Ok(())
}

fn round_trip_guest_string(
    caller: &mut Caller<'_, TestHarnessStore>,
    req_ptr: i32,
    req_len: i32,
    resp_ptr: i32,
    resp_cap: i32,
    f: impl FnOnce(String, &Sender<TestHarnessRequest>, &Receiver<TestHarnessSignal>) -> Result<String>,
) -> i32 {
    let input = match read_guest_string(caller, req_ptr, req_len) {
        Ok(input) => input,
        Err(err) => return log_failure(err, "test harness passed invalid request"),
    };
    write_guest_response(
        caller,
        resp_ptr,
        resp_cap,
        |request_sender, signal_receiver| f(input, request_sender, signal_receiver),
    )
}

fn write_guest_response(
    caller: &mut Caller<'_, TestHarnessStore>,
    resp_ptr: i32,
    resp_cap: i32,
    f: impl FnOnce(&Sender<TestHarnessRequest>, &Receiver<TestHarnessSignal>) -> Result<String>,
) -> i32 {
    let Some(request_sender) = caller.data().request_sender.as_ref() else {
        return log_failure(
            anyhow::anyhow!("missing request sender"),
            "test harness runtime unavailable",
        );
    };
    let Some(signal_receiver) = caller.data().signal_receiver.as_ref() else {
        return log_failure(
            anyhow::anyhow!("missing signal receiver"),
            "test harness runtime unavailable",
        );
    };
    let response = match f(request_sender, signal_receiver) {
        Ok(response) => response,
        Err(err) => return log_failure(err, "test harness host request failed"),
    };
    match write_guest_bytes(caller, resp_ptr, resp_cap, response.as_bytes()) {
        Ok(written) => written,
        Err(err) => log_failure(err, "test harness response write failed"),
    }
}

fn log_failure(err: anyhow::Error, message: &str) -> i32 {
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
            let _ = response.send(result);
        }
        TestHarnessRequest::StatusJson { response } => {
            let result = control::status_json_string(state).map_err(|err| err.to_string());
            let _ = response.send(result);
        }
    }
}
