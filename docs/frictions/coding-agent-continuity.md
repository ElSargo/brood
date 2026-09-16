---
status: open
type: friction
authority: design-input
---

# Coding agent continuity boundaries

The core reliability request uses Codex as a reference. The implementation
supports durable model state and provider configuration, but several boundaries
need separate design decisions before claiming complete product parity.

- REPL values remain in a private process, while conversation state is durable.
  A kernel crash, explicit reset, or idle expiry clears values that a checkpoint
  may describe. Replaying arbitrary code to reconstruct them can repeat side
  effects. Should the UI expose kernel lifetime more prominently, and should
  explicit user-managed artifacts become the supported durable REPL mechanism?
- Providers currently need the Responses wire API. Opaque reasoning and remote
  compaction state bind conversations to their provider endpoint. Switching
  endpoint requires a new conversation. OpenCode Go makes the cost concrete:
  most of its catalog is served through chat-completions or Anthropic messages,
  so only the Responses subset is reachable. What portable history
  representation and capability negotiation should govern future
  cross-provider continuation?
- Shell sessions use pipes. Node and shell code execute with the enclosing
  session's authority; workspace checks on patch paths do not provide an OS
  sandbox. PTY interaction and approval/sandbox policy need explicit product
  targets rather than assumptions based on the Codex comparison.
- Brood owns a copied ChatGPT login and serializes its own refreshes, but Codex
  and Brood can independently rotate credentials originating from the same
  login. The live test encountered a rejected old Brood refresh token. Should
  Brood obtain an independent login instead of importing credentials, or should
  an explicit credential broker own refresh across applications?
- Automatic provider selection can resolve to a provider whose catalog does not
  serve the runtime default model (`gpt-5.6-sol`). A Go-only environment needs
  an explicit model choice in addition to automatic provider resolution. Should
  defaults be provider-scoped, or should provider resolution carry a model
  mapping?

The implementation evidence and verification limits are in
[the progress note](../progress/coding-agent.md). This note records open questions
and does not change the accepted target design.
