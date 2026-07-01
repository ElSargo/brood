---
status: canonical
type: concept
authority: product
last-reviewed: 2026-07-01
---

# Session

A session is an isolated collection of related panes, producer state, environment, cwd, agents, programs, and capabilities that can be presented and managed together.

The session is the primary local trust boundary. Programs and agents inside the same session may discover and communicate with each other according to session capabilities. Cross-session communication should require explicit workspace-level bridging or grants.

Current user-facing operations such as `enter`, `show`, `hide`, `list`, and `switch` are session-oriented. See [[../workflows/enter-session]].

Related:

- [[workspace]]
- [[pane]]
- [[session-runner]]
- [[../systems/runtime]]
