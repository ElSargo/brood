use brood_plugin_sdk::{
    actor::spawn_worker,
    bindings,
    brood::plugin::host,
    exports::brood::plugin::actor::{Command, Event, Guest},
    runtime::FilesystemWatch,
};

const WORKER: u64 = 1;
const REPORT: &str = ".brood/component-workspace-watch.md";

struct ComponentWorkspaceWatch;

impl Guest for ComponentWorkspaceWatch {
    fn dispatch(event: Event) -> Vec<Command> {
        match event {
            Event::Mailbox(message) if message.body == b"start" => {
                vec![spawn_worker(WORKER, Vec::new())]
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
                if event.path == REPORT.as_bytes() {
                    continue;
                }
                changed = true;
                report.push_str(&format!(
                    "| {} | {} | `{}` |\n",
                    batch.generation,
                    event_kind(event.kind),
                    display_path(&event.path)
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

const fn event_kind(kind: host::FilesystemEventKind) -> &'static str {
    match kind {
        host::FilesystemEventKind::Created => "created",
        host::FilesystemEventKind::Modified => "modified",
        host::FilesystemEventKind::Removed => "removed",
        host::FilesystemEventKind::Changed => "changed",
        host::FilesystemEventKind::Rescan => "rescan",
    }
}

/// Render opaque native path bytes without conflating invalid UTF-8 with the
/// replacement character. Backslash is escaped too, so every `\xNN` sequence
/// in the report denotes exactly one original byte.
fn display_path(path: &[u8]) -> String {
    use std::fmt::Write;

    let mut display = String::with_capacity(path.len());
    for &byte in path {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_' | b'/') {
            display.push(char::from(byte));
        } else {
            write!(&mut display, "\\x{byte:02X}").expect("writing to String cannot fail");
        }
    }
    display
}

bindings::export!(ComponentWorkspaceWatch with_types_in bindings);
