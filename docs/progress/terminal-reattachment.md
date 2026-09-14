---
status: progress
type: implementation
authority: implementation
---

# Terminal presentation reattachment

The affected live home-directory session had three running session actors
(`terminal-session`, `terminal-session-2`, and `terminal-session-3`) but only
the default presentation. The daemon reported four PTYs across its two
sessions. No session or shell was restarted during diagnosis.

The session init actor sent a one-shot `terminal-started` reply and then
discarded the terminal identity from its pending list. Replacement client init
could only invent a default terminal UI. This lost dynamic panes even though
their session actors, terminal buffers, and PTYs remained alive.

The plugin SDK now provides `inventory::ActorInventory`, `subscribe`, and
`Snapshot::changes`. A supervisor forwards the selected children's lifecycle
events and authenticated subscription messages. The inventory retains ready
children, publishes complete bounded snapshots on changes, and replies again
when the same logical subscriber reconnects. Empty snapshots remove stale
presentations. Client policy compares snapshots with both live children and
starts in flight before choosing its own presentation component and scope.
This mechanism uses ordinary actor messages and contains no terminal policy.

Session init uses the helper for boot and shortcut-created terminals. Client
init subscribes from its session catalogue and reconciles each backend/session
independently. It removes UI ownership before issuing inventory-driven stops,
so stop acknowledgements do not destroy session resources. UI failure no
longer sends a shell-stop request; explicit pane close still does.

Real-workload investigation also found an obsolete daemon routing check that
rejected all actor replies from inactive sessions. Replies now retain their
session realm and reach the attached client, whose recipient association checks
still apply. Selecting which session is visible does not select which plugin
inventories can be synchronized.

The workload then exposed a layout race: panes declared before the shell's
first rest delegation, but rendered afterward, remained owned by the shell
instead of the workspace. Inactive-session panes had the same problem. The
host now publishes `owned_panes` independently of `visible_panes`, and the
shell delegates the complete non-chrome ownership set. First-frame timing no
longer determines whether a restored pane belongs to the workspace.

## Verification

Focused SDK and plugin tests cover subscription before readiness, replacement
after readiness, dynamic creation, empty inventories, failed starts, explicit
removal, duplicate reconciliation, and identical session IDs on distinct
backends. The SDK's 14 tests, session init's 9 tests, session switcher's 44 tests,
layout serialization check, and two client association checks pass. SDK Clippy
passes. The Nix-backed full release build also passes and refreshes the normal
host executable and guest package artifacts.

The full host Clippy check is blocked by nine pre-existing findings in
`clients/brood-client/src/complaint_capture.rs`.

The built-in `terminal-reconnect` proof drives real shortcut creation and pane
closure under the isolated compositor, preserves shell variables and PIDs,
replaces the presentation three times, and creates a terminal while detached.
It retains screenshots, client logs, daemon metrics, and structured shell
readbacks. Parser and workload details live in `crates/brood-cli/src/cli.rs`
and `tools/test-terminal-reconnect.py`.

The release workload passed with evidence in
`/tmp/brood-terminal-reconnect-proof-9/terminal-reconnect-report.json`:

- Six real terminals across two sessions and three complete client replacements.
- Twenty-four keyboard-driven shell readbacks; retained shells keep their PIDs
  and variables, including those in the inactive session.
- The explicitly closed terminal stays absent. A terminal created while
  detached appears on the next client, with six PTYs remaining overall.
- Every terminal UI produces committed frames in every client generation;
  per-actor counts are in `terminal-render-readback.json`. The daemon records
  zero input drops.
- Captures show all three terminals after the second replacement, and the two
  remaining terminals in that session after explicit closure.

Earlier attempts exposed verification constraints worth retaining. Cold debug
WASM startup exceeded the old 20-second harness deadline, and one debug render
of the session switcher trapped on its existing facet deadline. The successful
workload uses the release host without changing runtime budgets. Long synthetic
typing also stalled the debug input queue; the lifecycle proof now types a
short script-source command through real keyboard input and checks the actual
shell's variable and PID. It does not establish high-volume input reliability.
Startup readiness accepts either ordinary or synchronized layout commits, then
each shell probe waits for its own pane's focus to commit.

Session init generation replacement remains separate: this change preserves
resources across presentation replacement, not session-owner replacement.
Existing processes were not restarted to load the new artifacts.

## Follow-up: creation after an empty attachment

The first reconnect workload created its sessions before the client attached.
It missed startup of a session while the client was already attached. The
reported regression left 14 terminal processes running in the home-directory
session, but none had a presentation subscriber.

The empty-start reproduction in `/tmp/brood-terminal-create-proof-2` shows the
inventory subscription arriving about 54 ms before session init's asynchronous
load completed. Routing fell through to the legacy actor registry and rejected
it as an unknown recipient. The session then started its default terminal, and
later Ctrl+Alt+T requests created more terminals without notifying any client.

The host now accepts bounded mailbox messages for admitted hosted actors that
are still starting, retaining authenticated senders and binary bodies in FIFO
order. Successful startup drains the messages. Failure, cancellation, and a
superseding start discard messages belonging to that candidate. The same path
handles plugin configuration messages and ordinary cross-realm messages; a
plugin does not need a startup delay or subscription retry timer.

The permanent terminal workload now starts with an empty catalogue and creates
both sessions after attachment. It checks each default terminal UI before
continuing with shortcut creation and the complete reconnect workload.

The expanded release workload passes in `/tmp/brood-terminal-create-proof-3`:
both newly created sessions display their default terminal before any shortcut;
all six terminals survive three reconnects with 24 shell readbacks and committed
frames from every UI. Explicit close and detached creation still pass, and the
daemon records zero input drops. The two focused startup-mailbox unit tests
also pass, covering sender/binary preservation, FIFO order, bounded admission,
and reuse after the earlier candidate's messages are removed. The updated
release executable is built; the user's existing daemon and shells have not
been restarted.

After explicit approval, the live daemon was restarted on 2026-09-15 to load
the host fix, closing the 14 old shells. A fresh `sargo` session in `/home/sargo`
was created with its default terminal. Server-only Clippy and all 37 host unit
tests pass; full-feature Clippy remains blocked by the previously recorded
client findings. Live restart status is retained in
`/tmp/brood-terminal-startup-live-restored.json`.
