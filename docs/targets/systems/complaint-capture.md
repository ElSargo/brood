---
status: target
type: system
authority: product
last-reviewed: 2026-09-10
---

# Complaint Capture

Brood lets the user quickly record a complaint about Brood from inside the
app, preserve evidence of the app at that moment, and start an agent to
investigate and flesh out the complaint. Capture should take only an invocation,
a short note, and submission. Reproduction steps, a title, and a diagnosis are
not prerequisites.

## Capture flow

A presentation-global action opens a compact complaint editor with its text
field focused. It is discoverable in the action catalogue and has a remappable
shortcut. Invocation latches the originating presentation, active session,
focused pane, and latest presented frame before showing the editor. It also
requests bounded diagnostic snapshots asynchronously. Opening the editor must
not wait for the backend or an agent.

The editor offers Save and investigate as its primary action and Save only as
an alternative. Submission durably saves the user's words and starts the
investigation when requested. It returns focus to the originating pane if that
pane still exists. The user can continue working while the agent investigates.
Closing the editor retains its draft; reopening resumes that draft and its
original capture. Starting a fresh complaint explicitly takes a new capture.

A complaints view lists saved notes and investigation status and opens the
bundle, report, or associated agent conversation. Complaints remain available
across session switches and client restarts. The transient completion inbox
may announce a finished investigation but is not the complaint store.

## Files and ownership

Use one directory per complaint under a configurable durable complaints root.
A good candidate is ~/.local/share/brood/complaints
The default root belongs to the user's Brood data directory on the presentation
machine, so complaints about Brood do not depend on an active project or get
scattered among session working directories. The UI exposes its location.
Users can select a project directory when that is their preferred workflow.

Each bundle has a stable unique identity and ordinary inspectable files:

- `complaint.md`: the user's original words;
- `manifest.json`: schema version, identity, timestamps, originating client and
  session identities, capture results, and investigation identity and status;
- `evidence/`: the original presentation image and structured diagnostic files;
- `investigation.md`: the agent's evolving elaboration and findings.

Saving the note is independent of diagnostic and agent success. The UI reports
Saved only after durable storage succeeds. A failed save retains the draft and
shows a retryable error. Evidence collectors record unavailable, failed, or
truncated results explicitly; a partially captured complaint remains useful.
Retries use the complaint identity to avoid duplicate notes or agent launches.
Agent findings never overwrite the original note or original evidence.

## Evidence at the time of complaint

The presentation image must represent the composed Brood viewport, including
client-local plugin panes, chrome, and layout. Capturing a server automation
viewport alone cannot satisfy this contract. Capture is passive: it does not
resize applications, acquire an input-control lease, or change focus before
recording the originating state.

Collect bounded, versioned diagnostics where available:

- Brood build/revision and relevant platform and renderer information;
- presentation dimensions and scale, layout, visible pane identities, focus,
  and plugin identities and generations;
- originating backend/session identity and relevant managed-window state;
- recent bounded logs, errors, and existing rendering/input performance metrics.

Every source records its own collection time and available frame, revision, or
sequence identity. The manifest distinguishes the latched presentation state
from later backend observations. It must not imply an atomic snapshot across
processes. A stalled backend produces an explicit timeout while the client
retains the note and whatever evidence it could capture.

Capture only the selected Brood presentation and authorized session context.
Do not dump credentials, entire process environments, unrelated sessions, or
unbounded memory and logs. The editor shows what will be attached and allows
the user to omit the screenshot or other evidence before agent submission.
Local persistence and sharing with the configured agent are distinct states.

## Investigation agent

Save and investigate creates an independently addressed agent associated with
the complaint, using the configured agent/provider defaults. Its task is to
turn the rough note into an actionable account: observed behavior, expected
behavior where supported by the note or design, reproduction clues, affected
components, evidence references, hypotheses, and unresolved questions.

The agent first inspects the saved evidence, then uses scoped read-only live
introspection and available source/log access to fill gaps. Later observations
are recorded separately with timestamps; the live app may already have changed.
Claims distinguish user reports, direct observations, and hypotheses. Missing
evidence remains missing rather than becoming an invented reproduction or
confident diagnosis.

Investigation does not itself authorize code changes, filing an external issue,
or driving the user's active panes to reproduce the problem. Interactive
reproduction belongs in a separately requested isolated test session. The
agent can write its report and retain its conversation without interrupting
the user's current task.

The agent uses the existing session-owned agent lifecycle. Closing the complaint
editor or presentation does not cancel an accepted investigation. A complaint
captured without an available agent session still saves successfully and shows
investigation as unavailable until one is selected. The UI supports opening,
cancelling, and retrying investigations and distinguishes queued, running,
complete, failed, cancelled, and interrupted states.

For a remote backend, evidence is transferred as bounded content through an
authorized route; client filesystem paths are not backend paths. A running
agent retains its report on the backend until the complaint store acknowledges
receipt, allowing reconnect to complete the bundle. Retrying submission or
report delivery must not create a second investigation or lose completed work.

## Acceptance

Use the Nix-backed offscreen behavior-test path with a real presentation client.
Invoke capture through real keyboard input while a terminal and client-local
plugin pane are visible. Verify the stored screenshot contains the pre-editor
composition, type and save a complaint, and inspect its actual bundle files.
Verify focus restoration, review through the complaints view, and persistence
after reconnect and restart.

Exercise unavailable diagnostics, storage failure, no active session, agent
failure, repeated submission, and disconnect during investigation. A real
agent workload must read the captured artifacts and produce a separately saved
report with evidence references while the user continues working. Verify that
the original note and evidence remain unchanged and that later live
observations are identified as such. Retain screenshots and relevant client
and backend metrics as verification evidence.

Related:

- [[input-shortcuts]]
- [[presentation-client]]
- [[computer-use]]
- [[inbox-plugin]]
- [[../../experiments/coding-agent-actors]]
