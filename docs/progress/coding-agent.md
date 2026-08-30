---
status: current
type: progress
authority: implementation
---

# Coding Agent

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
