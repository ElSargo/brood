wit_bindgen::generate!({
    path: "../../crates/manifold-wasm/wit",
    world: "manifold-plugin",
});

use exports::manifold::plugin::actor::{Command, Event, Guest, WorkerRequest};
use manifold::plugin::host;

const WORKER: u64 = 1;
const REPORT: &str = ".manifold/component-workspace-watch.md";

struct ComponentWorkspaceWatch;

impl Guest for ComponentWorkspaceWatch {
    fn dispatch(event: Event) -> Vec<Command> {
        match event {
            Event::Mailbox(message) if message.body == b"start" => {
                vec![Command::SpawnWorker(WorkerRequest {
                    worker: WORKER,
                    payload: Vec::new(),
                })]
            }
            _ => Vec::new(),
        }
    }

    async fn worker_run(_worker: u64, _payload: Vec<u8>) -> Result<Vec<u8>, String> {
        let mut watch = FilesystemWatch::recursive(".")?;
        let mut report = String::from(
            "# Component workspace watch\n\n\
             This file is maintained by a Component Model plugin watching its capability root.\n\n\
             | generation | change | relative path |\n\
             |---:|---|---|\n",
        );
        loop {
            let batch = watch.next().await?;
            let mut changed = false;
            for event in batch.events {
                if event.path == REPORT {
                    continue;
                }
                changed = true;
                report.push_str(&format!(
                    "| {} | {} | `{}` |\n",
                    batch.generation,
                    event_kind(event.kind),
                    event.path.replace('`', "\\`")
                ));
            }
            if changed {
                host::worker_write_file(REPORT.to_string(), 0, report.as_bytes().to_vec())
                    .await
                    .map_err(|error| format!("write observable watch report: {error:?}"))?;
            }
        }
    }
}

/// Guest SDK wrapper: resource ownership is RAII, while readiness reuses the
/// normal notification pollable and worker `wait-any` path.
struct FilesystemWatch {
    resource: host::FilesystemWatch,
    generation: u64,
}

impl FilesystemWatch {
    fn recursive(path: &str) -> Result<Self, String> {
        let resource = host::filesystem_watch_create(path, true)
            .map_err(|error| format!("create filesystem watch: {error:?}"))?;
        Ok(Self {
            resource,
            generation: 0,
        })
    }

    async fn next(&mut self) -> Result<host::FilesystemWatchBatch, String> {
        let notification = self
            .resource
            .notification()
            .map_err(|error| format!("get watch notification: {error:?}"))?;
        let pollable = host::notification_pollable(notification, self.generation)
            .map_err(|error| format!("create watch pollable: {error:?}"))?;
        let ready = host::worker_wait_any(vec![pollable], None, host::WaitMode::Fair).await;
        let _drop_result = host::pollable_drop(pollable);
        ready.map_err(|error| format!("wait for filesystem watch: {error:?}"))?;
        let batch = self
            .resource
            .read()
            .map_err(|error| format!("read filesystem watch: {error:?}"))?;
        self.generation = batch.generation;
        Ok(batch)
    }
}

const fn event_kind(kind: host::FilesystemEventKind) -> &'static str {
    match kind {
        host::FilesystemEventKind::Created => "created",
        host::FilesystemEventKind::Modified => "modified",
        host::FilesystemEventKind::Removed => "removed",
        host::FilesystemEventKind::Changed => "changed",
        host::FilesystemEventKind::Rescan => "rescan",
    }
}

export!(ComponentWorkspaceWatch);
