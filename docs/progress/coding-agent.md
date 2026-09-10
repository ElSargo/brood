---
status: current
type: progress
authority: implementation
---

# Coding Agent

## 2026-09-10 core agent reliability

The requested implementation prioritizes durable conversation state, compaction,
a persistent code REPL, provider configuration, tools, cancellation, and recovery.
The [runtime guide](../../agents/brood-agent/README.md) describes the operator
workflow and points to the CLI/configuration parsers. No target document changed.

The independent Node runner now saves complete Responses items and tool results
atomically, uses private kernel locks, resumes saved conversations, and journals
calls before side effects. Recovery marks uncertain outcomes for inspection
instead of automatically replaying actions. The actor retains the conversation
ID before a turn finishes and ignores events from obsolete workers. Submission
while running or cancelling no longer discards the draft.

Provider profiles support custom Responses endpoints and environment-sourced
credentials, model/reasoning selection, context budgets, and capability settings.
The transport handles streaming boundaries, incomplete streams, request/idle
timeouts, bounded retry, and serialized OAuth refresh. Live verification rejected
the previous default `gpt-5.4` for the current ChatGPT login; `gpt-5.6-sol` passed
and is now the default. The current Codex login was explicitly imported into the
Brood-owned auth file after its previous refresh token was rejected.

Compaction runs automatically, on request, and following context-limit errors.
It prefers the provider compact endpoint and supports a summary fallback when
that endpoint is unavailable. Complete recent tool batches and the latest user
request are retained. Invalid results leave the original checkpoint intact.
A live small-context attempt produced a larger summary: small/nonshrinking
summaries now leave the conversation unchanged, and recent-tail retention uses
a token budget rather than an item count.

The persistent Node REPL keeps lexical bindings and top-level await across calls,
turns, and compaction; errors preserve bindings, while cancellation/timeout resets
the kernel. Nested agent tools and computer screenshots are supported. Shell
sessions yield, accept stdin, retain bounded output/artifacts, and terminate
process groups. Real Escape testing revealed that Brood kills workers before
JavaScript cleanup can run; each command now has a supervisor that detects the
runner disconnect and kills remaining descendants. `apply_patch` validates full
patches before writing, rejects ambiguous/outside paths, and rolls back failure.

The real attachment workload also exposed a renderer fuel exhaustion: two RGBA
previews serialized as JSON integer arrays caused the pane to stop rendering.
The view format now carries JSON metadata and raw bounded pixels; both the
round-trip check and actual two-attachment pane test pass.

Verification evidence is in [results.json](coding-agent-evidence/results.json):

- 35 Node tests pass inside the sandboxed Nix `agentCheck` derivation, including
  real HTTP fixtures, subprocesses, killed-lock-owner contention, tool failures,
  compaction, and interrupted-call recovery. This check is part of `lintCheck`.
- Plugin Rust tests pass with both renderers: 21 default and 17 legacy tests.
  Cargo formatting and ShellCheck on the changed harness pass. Repository-wide
  ShellCheck still finds an existing missing shell directive in
  `tools/test-themes-live.sh`; strict plugin Clippy still finds an existing
  duplicate branch and two oversized helper signatures.
- `brood test coding-agent` runs offscreen, substitutes only the model service,
  and uses real typing, image paste/drop, actor workers, Node tools, and files.
  Eight provider requests include one compaction. Broodd records four completed workers,
  one cancelled worker, and zero failed workers. Both attachment colors are
  verified, the REPL binding survives compaction, and the delayed descendant
  write remains absent after Escape. A new turn verifies the interrupted
  checkpoint without replaying its command. Client/server logs are retained in
  `/tmp/brood-agent-reliability-cancellation`.
- The live ChatGPT provider edited and checked a real workspace file, compacted
  13,867 estimated input tokens to 1,940 via the summary fallback, and verified
  the same REPL variable in the next turn (`LIVE_AGENT_OK`). Sanitized usage and
  compaction events are retained in the evidence JSON; credentials and full
  conversation state are excluded.

Screenshots: [attachments](coding-agent-evidence/attachments.png),
[after compaction](coding-agent-evidence/compaction-resumed.png), and
[after cancellation recovery](coding-agent-evidence/cancellation-recovered.png).
Known design questions are recorded separately in
[agent continuity friction](../frictions/coding-agent-continuity.md).

## 2026-08-31 prompt history recall

OpenCode's current prompt editors retain a bounded, adjacent-deduplicated
history and navigate it with editor-boundary Up/Down actions. Its app history
reducer also saves the current prompt before entering history and restores that
saved value after the newest entry. Primary references:

- [OpenCode TUI prompt history](https://github.com/anomalyco/opencode/blob/10765ff2a9da8c3b88e4de873aa383a49c318912/packages/tui/src/prompt/history.tsx)
- [OpenCode app prompt history reducer](https://github.com/anomalyco/opencode/blob/10765ff2a9da8c3b88e4de873aa383a49c318912/packages/app/src/components/prompt-input/history.ts)
- [OpenCode prompt submission](https://github.com/anomalyco/opencode/blob/10765ff2a9da8c3b88e4de873aa383a49c318912/packages/app/src/components/prompt-input/submit.ts)

Brood's coding-agent prompt editor now keeps its last 100 submitted text
prompts, skips adjacent duplicates, and recalls them without displacing normal
multiline cursor movement. Up enters history only after the cursor reaches the
first logical line; Down advances only after it reaches the last. Moving past
the newest entry restores the exact unsent Unicode draft and its end cursor.
Editing a recalled entry deliberately exits history navigation, making the edit
a new draft rather than mutating retained history.

Focused reducer tests cover ordered recall, draft restoration, multiline
boundary precedence, capacity, and adjacent deduplication.
