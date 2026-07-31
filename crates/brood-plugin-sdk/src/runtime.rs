//! Cancellation-safe helpers for worker-side runtime I/O.
//!
//! The dispatcher remains nonblocking: every wait in this module is valid only
//! from `Guest::worker_run`.

use crate::brood::plugin::host;

const READ_CHUNK_BYTES: u32 = 16 * 1024;

#[derive(Clone, Debug)]
pub struct ProcessOutput {
    pub state: host::ChildState,
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Generation-owned filesystem subscription using the ordinary worker wait
/// path. Dropping the wrapper drops the typed host resource.
pub struct FilesystemWatch {
    resource: host::FilesystemWatch,
    generation: u64,
}

impl FilesystemWatch {
    pub fn new(path: &str, recursive: bool) -> Result<Self, String> {
        let resource = host::filesystem_watch_create(path, recursive)
            .map_err(|error| format!("create filesystem watch: {error:?}"))?;
        Ok(Self {
            resource,
            generation: 0,
        })
    }

    pub fn recursive(path: &str) -> Result<Self, String> {
        Self::new(path, true)
    }

    pub async fn next(&mut self) -> Result<host::FilesystemWatchBatch, String> {
        let notification = self
            .resource
            .notification()
            .map_err(|error| format!("get watch notification: {error:?}"))?;
        wait_notification(notification, self.generation).await?;
        let batch = self
            .resource
            .read()
            .map_err(|error| format!("read filesystem watch: {error:?}"))?;
        self.generation = batch.generation;
        Ok(batch)
    }
}

/// Wait for a notification generation and always retire the temporary pollable.
pub async fn wait_notification(notification: u64, generation: u64) -> Result<u64, String> {
    let pollable = host::notification_pollable(notification, generation)
        .map_err(|error| format!("create pollable: {error:?}"))?;
    let ready = host::worker_wait_any(vec![pollable], None, host::WaitMode::Fair)
        .await
        .map_err(|error| format!("wait for notification: {error:?}"));
    let _drop_result = host::pollable_drop(pollable);
    ready.map(|ready| ready.generation)
}

/// Wait until a timer handle created by the caller expires.
pub async fn wait_timer(timer: u64) -> Result<(), String> {
    let notification = host::timer_notification(timer)
        .map_err(|error| format!("timer notification: {error:?}"))?;
    let mut generation = 0;
    while host::timer_status(timer).map_err(|error| format!("timer status: {error:?}"))?
        == host::TimerState::Pending
    {
        generation = wait_notification(notification, generation).await?;
    }
    Ok(())
}

/// Sleep without leaking the generation-owned timer resource.
pub async fn sleep(duration_ms: u64) -> Result<(), String> {
    let timer =
        host::timer_create(duration_ms).map_err(|error| format!("create timer: {error:?}"))?;
    let waited = wait_timer(timer).await;
    let dropped = host::timer_drop(timer)
        .await
        .map_err(|error| format!("drop timer: {error:?}"));
    waited.and(dropped)
}

/// Wait for a child to reach a terminal state.
pub async fn wait_child(child: u64) -> Result<(host::ChildState, Option<i32>), String> {
    let notification = host::child_notification(child)
        .map_err(|error| format!("child notification: {error:?}"))?;
    let mut generation = 0;
    loop {
        let status =
            host::child_status(child).map_err(|error| format!("child status: {error:?}"))?;
        if status.0 != host::ChildState::Running {
            return Ok(status);
        }
        generation = wait_notification(notification, generation).await?;
    }
}

/// Write a complete buffer, waiting only when the bounded host stream is full.
pub async fn write_all(stream: u64, mut bytes: &[u8]) -> Result<(), String> {
    let notification = host::stream_notification(stream)
        .map_err(|error| format!("stream notification: {error:?}"))?;
    let mut generation = 0;
    while !bytes.is_empty() {
        match host::stream_write(stream, bytes) {
            Ok(written) if written > 0 => {
                let written = usize::try_from(written).map_err(|_conversion_error| {
                    "stream write count does not fit usize".to_string()
                })?;
                bytes = bytes
                    .get(written..)
                    .ok_or_else(|| "host reported an oversized stream write".to_string())?;
            }
            Ok(_) | Err(host::RuntimeIoError::WouldBlock) => {
                generation = wait_notification(notification, generation).await?;
            }
            Err(error) => return Err(format!("write stream: {error:?}")),
        }
    }
    Ok(())
}

/// Spawn a subprocess, close its input, drain both output streams concurrently,
/// and retire every generation-owned handle.
pub async fn run_process(
    command: &str,
    arguments: &[String],
    max_stdout: usize,
    max_stderr: usize,
) -> Result<ProcessOutput, String> {
    let process = host::subprocess_spawn(command, arguments)
        .map_err(|error| format!("spawn {command}: {error:?}"))?;
    let _close_result = host::stream_close_write(process.stdin);
    let output = drain_process(&process, max_stdout, max_stderr).await;
    let _stdin_drop_result = host::stream_drop(process.stdin).await;
    let _stdout_drop_result = host::stream_drop(process.stdout).await;
    let _stderr_drop_result = host::stream_drop(process.stderr).await;
    let _child_drop_result = host::child_drop(process.child).await;
    output
}

async fn drain_process(
    process: &host::ProcessIo,
    max_stdout: usize,
    max_stderr: usize,
) -> Result<ProcessOutput, String> {
    let stdout_notification = host::stream_notification(process.stdout)
        .map_err(|error| format!("stdout notification: {error:?}"))?;
    let stderr_notification = host::stream_notification(process.stderr)
        .map_err(|error| format!("stderr notification: {error:?}"))?;
    let child_notification = host::child_notification(process.child)
        .map_err(|error| format!("child notification: {error:?}"))?;
    let mut generations = [0_u64; 3];
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut stdout_eof = false;
    let mut stderr_eof = false;

    loop {
        stdout_eof |= drain_stream(process.stdout, &mut stdout, max_stdout)?;
        stderr_eof |= drain_stream(process.stderr, &mut stderr, max_stderr)?;
        let status = host::child_status(process.child)
            .map_err(|error| format!("child status: {error:?}"))?;
        if status.0 != host::ChildState::Running && stdout_eof && stderr_eof {
            return Ok(ProcessOutput {
                state: status.0,
                code: status.1,
                stdout,
                stderr,
            });
        }

        let notifications = [stdout_notification, stderr_notification, child_notification];
        let mut pollables = Vec::with_capacity(3);
        for (notification, generation) in notifications.into_iter().zip(generations) {
            match host::notification_pollable(notification, generation) {
                Ok(pollable) => pollables.push(pollable),
                Err(error) => {
                    drop_pollables(&pollables);
                    return Err(format!("create process pollable: {error:?}"));
                }
            }
        }
        let ready = host::worker_wait_any(pollables.clone(), None, host::WaitMode::Fair).await;
        drop_pollables(&pollables);
        match ready {
            Ok(ready) => {
                let index = usize::try_from(ready.index).map_err(|_conversion_error| {
                    "ready pollable index does not fit usize".to_string()
                })?;
                let generation = generations
                    .get_mut(index)
                    .ok_or_else(|| "host returned an invalid ready pollable index".to_string())?;
                *generation = ready.generation;
            }
            Err(host::WaitError::Cancelled) => {
                return Err(format!("{} cancelled", process.resolved_command));
            }
            Err(error) => return Err(format!("wait for process output: {error:?}")),
        }
    }
}

fn drain_stream(stream: u64, output: &mut Vec<u8>, limit: usize) -> Result<bool, String> {
    loop {
        match host::stream_read(stream, READ_CHUNK_BYTES)
            .map_err(|error| format!("read process stream: {error:?}"))?
        {
            host::StreamReadResult::Data(bytes) => {
                let remaining = limit.saturating_sub(output.len());
                let bounded = bytes
                    .get(..bytes.len().min(remaining))
                    .ok_or_else(|| "failed to bound process output".to_string())?;
                output.extend_from_slice(bounded);
            }
            host::StreamReadResult::WouldBlock => return Ok(false),
            host::StreamReadResult::Eof => return Ok(true),
        }
    }
}

fn drop_pollables(pollables: &[u64]) {
    for pollable in pollables {
        let _drop_result = host::pollable_drop(*pollable);
    }
}
