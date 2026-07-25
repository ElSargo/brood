wit_bindgen::generate!({
    path: "../../crates/brood-wasm/wit",
    world: "brood-plugin",
});

use exports::brood::plugin::actor::{
    Command, Event, Guest, WorkerEvent, WorkerFinished, WorkerRequest,
};
use brood::plugin::host;

const WORKER: u64 = 1;
const REPORT: &str = ".brood/runtime-io-transcript.md";

struct RuntimeIoProof;

impl Guest for RuntimeIoProof {
    fn dispatch(event: Event) -> Vec<Command> {
        match event {
            Event::Mailbox(message) => vec![Command::SpawnWorker(WorkerRequest {
                worker: WORKER,
                payload: message.body,
            })],
            Event::Worker(WorkerEvent::Finished(WorkerFinished { worker, .. }))
                if worker == WORKER =>
            {
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    async fn worker_run(_worker: u64, payload: Vec<u8>) -> Result<Vec<u8>, String> {
        let address =
            String::from_utf8(payload).map_err(|error| format!("socket address: {error}"))?;
        let mut report = String::from("# Runtime I/O transcript\n\n");

        let pipeline = host::subprocess_spawn("tr", &["a-z".into(), "A-Z".into()])
            .map_err(debug("spawn pipeline"))?;
        write_all(pipeline.stdin, b"alpha beta\n").await?;
        host::stream_close_write(pipeline.stdin).map_err(debug("close transform stdin"))?;
        let pipeline_output = read_to_eof(pipeline.stdout).await?;
        let pipeline_status = wait_child(pipeline.child).await?;
        report.push_str(&format!(
            "## Subprocess pipe streams\n\n- Resolved command: `{}`\n- Stdin: `alpha beta\\n`\n- Transform: `tr a-z A-Z`\n- Stdout: `{}`\n- Exit: `{}`\n\n",
            pipeline.resolved_command,
            display(&pipeline_output),
            child_status_text(pipeline_status),
        ));
        let nonzero = host::subprocess_spawn("sh", &["-c".into(), "exit 7".into()])
            .map_err(debug("spawn nonzero child"))?;
        let nonzero_status = wait_child(nonzero.child).await?;
        report.push_str(&format!(
            "- Nonzero child exit: `{}`\n\n",
            child_status_text(nonzero_status)
        ));

        let pty = host::pty_spawn(
            "sh",
            &[
                "-c".into(),
                "test -t 0 && printf 'stdin-is-tty '; test -t 1 && printf 'stdout-is-tty terminal-prompt> '; IFS= read -r answer; printf 'terminal-reply=%s\\n' \"$answer\"".into(),
            ],
            24,
            80,
        )
        .map_err(debug("spawn PTY"))?;
        write_all(pty.terminal, b"typed-through-pty\n").await?;
        host::stream_close_write(pty.terminal).map_err(debug("close PTY input"))?;
        let terminal_output = read_to_eof(pty.terminal).await?;
        let terminal_status = wait_child(pty.child).await?;
        report.push_str(&format!(
            "## Real PTY interaction\n\n- Resolved command: `{}`\n- Sent: `typed-through-pty`\n- Terminal transcript: `{}`\n- Exit: `{}`\n\n",
            pty.resolved_command,
            display(&terminal_output),
            child_status_text(terminal_status),
        ));

        let socket = host::worker_loopback_connect(address.clone())
            .await
            .map_err(debug("connect loopback peer"))?;
        write_all(socket, b"hello-from-plugin\n").await?;
        host::stream_close_write(socket).map_err(debug("close socket write"))?;
        let peer_output = read_to_eof(socket).await?;
        report.push_str(&format!(
            "## Loopback socket exchange\n\n- Peer: `{address}`\n- Sent: `hello-from-plugin`\n- Received: `{}`\n\n",
            display(&peer_output),
        ));

        let later = host::timer_create(80).map_err(debug("create later timer"))?;
        let sooner = host::timer_create(20).map_err(debug("create sooner timer"))?;
        let first = wait_timer_pair(sooner, later).await?;
        let second = if first == "20ms" {
            wait_timer(later).await?;
            "80ms"
        } else {
            wait_timer(sooner).await?;
            "20ms"
        };
        report.push_str(&format!(
            "## Timer ordering\n\n1. `{first}` timer expired\n2. `{second}` timer expired\n"
        ));

        host::worker_write_file(REPORT.into(), 0, report.as_bytes().to_vec())
            .await
            .map_err(debug("write transcript"))?;
        Ok(report.into_bytes())
    }
}

async fn write_all(stream: u64, mut bytes: &[u8]) -> Result<(), String> {
    let mut generation = 0;
    while !bytes.is_empty() {
        match host::stream_write(stream, bytes) {
            Ok(written) => bytes = &bytes[written as usize..],
            Err(host::RuntimeIoError::WouldBlock) => {
                generation = wait_notification(
                    host::stream_notification(stream).map_err(debug("stream notification"))?,
                    generation,
                )
                .await?;
            }
            Err(error) => return Err(format!("stream write: {error:?}")),
        }
    }
    Ok(())
}

async fn read_to_eof(stream: u64) -> Result<Vec<u8>, String> {
    let notification = host::stream_notification(stream).map_err(debug("stream notification"))?;
    let mut generation = 0;
    let mut output = Vec::new();
    loop {
        match host::stream_read(stream, 4096).map_err(debug("stream read"))? {
            host::StreamReadResult::Data(bytes) => output.extend(bytes),
            host::StreamReadResult::Eof => return Ok(output),
            host::StreamReadResult::WouldBlock => {
                generation = wait_notification(notification, generation).await?;
            }
        }
    }
}

async fn wait_child(child: u64) -> Result<(host::ChildState, Option<i32>), String> {
    let notification = host::child_notification(child).map_err(debug("child notification"))?;
    let mut generation = 0;
    loop {
        let status = host::child_status(child).map_err(debug("child status"))?;
        if status.0 != host::ChildState::Running {
            return Ok(status);
        }
        generation = wait_notification(notification, generation).await?;
    }
}

async fn wait_timer(timer: u64) -> Result<(), String> {
    let notification = host::timer_notification(timer).map_err(debug("timer notification"))?;
    let mut generation = 0;
    while host::timer_status(timer).map_err(debug("timer status"))? == host::TimerState::Pending {
        generation = wait_notification(notification, generation).await?;
    }
    Ok(())
}

async fn wait_timer_pair(sooner: u64, later: u64) -> Result<&'static str, String> {
    let sooner_notification =
        host::timer_notification(sooner).map_err(debug("sooner notification"))?;
    let later_notification =
        host::timer_notification(later).map_err(debug("later notification"))?;
    let sooner_pollable =
        host::notification_pollable(sooner_notification, 0).map_err(debug("sooner pollable"))?;
    let later_pollable =
        host::notification_pollable(later_notification, 0).map_err(debug("later pollable"))?;
    let ready = host::worker_wait_any(
        vec![sooner_pollable, later_pollable],
        None,
        host::WaitMode::Fair,
    )
    .await
    .map_err(debug("wait timer pair"))?;
    let _ = host::pollable_drop(sooner_pollable);
    let _ = host::pollable_drop(later_pollable);
    Ok(if ready.index == 0 { "20ms" } else { "80ms" })
}

async fn wait_notification(notification: u64, generation: u64) -> Result<u64, String> {
    let pollable =
        host::notification_pollable(notification, generation).map_err(debug("create pollable"))?;
    let ready = host::worker_wait_any(vec![pollable], None, host::WaitMode::Fair)
        .await
        .map_err(debug("wait notification"));
    let _ = host::pollable_drop(pollable);
    ready.map(|ready| ready.generation)
}

fn display(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .replace('\r', "\\r")
        .replace('\n', "\\n")
}

fn child_status_text(status: (host::ChildState, Option<i32>)) -> String {
    match status {
        (host::ChildState::Exited, Some(code)) => code.to_string(),
        (host::ChildState::Signalled, _) => "signalled".to_string(),
        (host::ChildState::Running, _) | (host::ChildState::Exited, None) => "unknown".to_string(),
    }
}

fn debug<T: core::fmt::Debug>(context: &'static str) -> impl FnOnce(T) -> String {
    move |error| format!("{context}: {error:?}")
}

export!(RuntimeIoProof);
