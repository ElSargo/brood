# Brood agent runtime

The pane and headless CLI use the same Responses model loop, workspace tools,
conversation checkpoints, and provider configuration. Run the CLI inside the
session or workspace whose files the agent should use. Exact CLI parsing is in
[`src/cli.mjs`](src/cli.mjs); pane command parsing is in
[`src/agent.mjs`](src/agent.mjs).

## Configuration and login

Create a starting configuration with:

```sh
nix develop --command tools/brood-agent config init
```

Edit `$XDG_CONFIG_HOME/brood/agent.json` (normally
`~/.config/brood/agent.json`), or select another file with `BROOD_AGENT_CONFIG`.
The [example configuration](agent.example.json) includes a local service and a
gateway. Custom services must implement the Responses API. Credentials are
resolved from `env_key` or `env_http_headers`; custom providers never inherit
OpenAI credentials. Keep provider settings in the backend session's environment,
including when the presentation client is remote.

Settings resolve from built-in defaults, the user config, a selected profile,
and explicit turn overrides. Saved conversation choices carry forward unless
overridden. `BROOD_AGENT_PROFILE` and `BROOD_AGENT_PROVIDER` select initial
defaults. The schema and validation rules live in
[`src/config.mjs`](src/config.mjs). `config check` validates configuration and
profiles without contacting a provider or printing credentials.

The built-in `auto` provider uses an explicitly configured API key first
(`OPENAI_API_KEY`, then `OPENCODE_API_KEY`) and otherwise the Brood-owned
ChatGPT login. Initial use can import `~/.codex/auth.json` into Brood's private
auth file. Refresh is serialized across agents and a rejected access token gets
one refresh attempt. An expired or revoked saved login can be replaced
explicitly with `brood-agent auth import-codex`; this never writes Codex's file.
`BROOD_AGENT_AUTH` selects the Brood-owned credential file.

The built-in `opencode-go` provider selects OpenCode Go's Responses endpoint
(`https://opencode.ai/zen/go/v1/responses`) with `OPENCODE_API_KEY` and sends
the conversation ID as `x-opencode-session`. Because the runtime only speaks the
Responses wire API, select Go models the catalog lists for `/responses`, such as
`gpt-5.6-luna` or `grok-4.6`; Go models served through chat-completions or
messages endpoints are not reachable.

Provider credentials are resolved only when a model request is made. Agent
creation, `/help`, `/status`, `/provider`, `/model`, `/reasoning`, `/profile`,
`/new`, and `/resume` work with no provider authenticated; `/status` marks each
provider as ready or names the missing credential. A failed credential lookup
does not bind the conversation to an endpoint, so the provider can be changed
without starting a new conversation.

Use the pane's `/help` and `/status` commands to inspect available commands,
providers, profiles, the conversation ID, and context usage. Model and provider
names are not restricted to the toolbar's common model choices. A conversation
containing provider-specific reasoning or compaction state stays bound to its
endpoint; select the desired provider and start a new conversation to change
endpoints. Named profiles can also set a service's context budget and disable
unsupported reasoning or verbosity parameters.

## Continuity and recovery

Conversations live under `$XDG_STATE_HOME/brood/agent`, normally
`~/.local/state/brood/agent`, or `BROOD_AGENT_STATE_DIR`. Checkpoints are private,
atomically replaced, and locked against simultaneous writers with a kernel
lock (`flock`, supplied by Nix). A runner crash releases its lock automatically. They retain full
Responses items, including assistant phases, reasoning, tool calls and outputs,
the plan, usage, and compaction state. The pane's 24-turn text projection does
not limit model memory. The CLI's `sessions` command lists saved IDs without
printing conversation contents; `/resume ID` opens one in the current workspace.

Each model response is saved before tools execute, and each tool result is saved
after execution. Following interruption, a call without a saved result gets an
explicit uncertain-outcome marker. The agent must inspect its effects before
retrying. A saved call is never automatically executed during recovery. REPL
tool calls are covered by the enclosing cell's checkpoint boundary.

Compaction uses provider-reported usage and conservative local estimates. It
also runs on an explicit pane command or after a provider context-limit error.
The remote compact endpoint is preferred; providers without that endpoint can
use a model-generated summary. Summaries retain complete recent tool batches
within a token budget and the latest user request. Invalid or empty compaction
results cannot replace existing state. A summary that cannot reduce a small
conversation leaves it unchanged. Context estimates are not exact token counts;
configure the budget for the selected model.

## Tools and verification

The JavaScript REPL supports top-level `await`, imports, persistent `let` and
`const` bindings, `text(value)`, and `await tools.TOOL_NAME(arguments)`. Bindings
survive turns and compaction while the conversation's private REPL service is
alive. Normal evaluation errors preserve bindings; timeout, cancellation,
explicit reset, process exit, or 30 minutes idle clears them. Bindings are not
serialized or replayed after a process restart. The REPL has the same session
execution authority as shell commands.

Shell commands return a session ID when they outlive the initial wait. The
agent can collect incremental output, send input, close stdin, and terminate a
process group. Output is bounded; longer results are saved in private artifacts
with an explicit disk limit. Cancellation and turn teardown terminate owned
commands, including descendants. A separate command supervisor also cleans up
when Brood kills a runner before its signal handlers can run. Shell sessions use pipes, not a PTY.

`apply_patch` validates every file before writing, rejects ambiguous context
and paths outside the workspace, and rolls back committed files if a patch
fails. This path validation is not an OS sandbox: shell commands and Node code
still have the enclosing session's authority. Existing session-scoped computer
tools remain available when the native tool socket is present.

Run the regression suite and the real offscreen pane proof through Nix:

```sh
nix develop --command node --test agents/brood-agent/test/*.test.mjs
nix develop --command cargo run -- test coding-agent
```

The pane proof replaces only the model service with a local HTTP fixture. It
uses real typing, paste, drag-and-drop, actor workers, the runner, patch and shell
tools, a persistent REPL across compaction, and Escape cancellation followed by
checkpoint recovery. It retains screenshots, client
and server logs, provider evidence, and workspace effects. The Node suite also
runs in the Nix `agentCheck` package and the aggregate lint check.
