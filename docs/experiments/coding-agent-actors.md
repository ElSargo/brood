---
status: experiment
type: experiment
authority: product
last-reviewed: 2026-07-24
---

# Coding Agent Actors

This experiment treats a coding agent as an ordinary session actor and its UI
as an ordinary presentation actor. It is both a useful product workload and a
stress test of the actor, mailbox, capability, worker, and canvas contracts.

## Target Experience

A user can create several coding agents in one surface session, submit prompts,
choose a model and reasoning effort per agent, watch turns progress, cancel
work, and switch between agents without coupling their lifecycles. Closing or
replacing the UI does not stop the agents. An agent failure does not stop its
peers or the session root.

Each UI is a compact canvas application with agent tabs, status and
configuration, a transcript, and a prompt editor. `Ctrl+Alt+A` creates and
focuses another independently addressed UI actor without replacing or
restarting the agents. The architecture must also support editor panes, diff
viewers, LSP bridges, automation, and other agents as peers. Those clients
should use the same actor address and mailbox protocol rather than a privileged
coding-agent host API.

## Ownership

```text
surface-session root actor
  owns agent registry and actor lifecycle
          |
          +-- coding-agent actor A
          |     owns model, reasoning, turn state, history, and subscriptions
          |     +-- private provider/tool worker
          |
          +-- coding-agent actor B
                owns independent state, failure, and worker

presentation root actor
  owns presentation-local lifecycle
          |
          +-- coding-agent UI actor A
          |     owns prompt editing, selected tab, layout, and canvas output
          |     subscribes to registry and agent actors by mailbox
          |
          +-- coding-agent UI actor B
                owns independent presentation state and subscriptions
```

The public unit is the agent actor. Provider calls, stream pumps, and tool
execution are private workers because they need concurrency but not public
identity. A second agent is a second actor, not another task hidden in one
singleton agent process.

## Message Flow

The root publishes registry snapshots after a presentation actor subscribes or
the topology changes. The UI subscribes to each visible agent and receives an
initial state snapshot followed by updates.

```text
UI -> root:       registry-subscribe
root -> UI:       registry
UI -> agent:      subscribe
agent -> UI:      agent-state
UI -> agent:      configure / prompt / cancel
agent -> UI:      agent-state / agent-log
UI -> root:       create-agent
root -> UI:       registry
```

These are application messages over the ordinary bounded actor transport.
Presentation-to-session and session-to-presentation routing authenticate the
origin at the connection boundary. Message bodies do not grant authority.

Subscriptions are leases on a presentation connection, not ownership of the
agent. The target protocol needs explicit unsubscribe and connection-loss
cleanup, bounded replay or snapshot replacement, causal turn identifiers, and
backpressure behavior for slow observers. Multiple presentation clients may
subscribe to one session agent without becoming a single shared UI actor.

## Turns, Tools, And Providers

Submitting a prompt creates a turn with a stable identifier. The actor remains
responsive while a private worker performs provider I/O and tool work. Worker
notifications wake the actor, which incorporates output into durable actor
state and publishes bounded updates. Cancellation invalidates the turn
generation, closes its resources, and ignores stale completions.

The provider adapter is replaceable. A native model SDK, a local model service,
or a Codex process adapter should all project the same events into the agent
actor. Provider credentials and outbound network access belong to declared
capabilities and session configuration; they are not embedded in the UI or
mailbox protocol.

Tools should become actors when they need independent identity, lifecycle,
state, subscriptions, or reuse by several agents. A one-turn subprocess or
stream remains a private resource of the agent worker. Editors, diff tools,
LSPs, and other agents can send requests or subscribe without the host learning
coding-specific message types.

## Capability Direction

The session root starts each agent with a package-declared capability profile
resolved against the enclosing session grant. A coding profile may include the
workspace, approved subprocesses, PTY access, model credentials, and network
destinations. The root selects a profile; it cannot synthesize authority in an
actor-start message.

Presentation actors receive canvas, text, input, and addressed mailbox
authority, but no implicit workspace or model credentials. A backend agent must
explicitly opt into messaging an attached presentation. The runtime routes only
to the connection and actor address covered by that authority.

## Stress-Test Questions

The workload should keep pressure on:

- bidirectional cross-placement mailboxes and disconnect cleanup;
- many independently failing and replaceable agent actors;
- capability-profile inheritance for dynamically started children;
- long-running workers while actor dispatch remains responsive;
- transcript redraw cost and native text-resource reuse across scheduler yields;
- stream backpressure and bounded transcript snapshots;
- routing among agents, editors, diff tools, LSPs, and presentation clients;
- durable turn state and UI replacement or reconnection;
- virtual-keyboard, IME, pointer, focus, and resize input in canvas UIs.

## Verification Workload

The minimum proof starts the real session server and real presentation client in
an isolated nested compositor. It creates the session and UI roots, launches
and focuses a second UI with the global shortcut, verifies bidirectional
subscription traffic from that UI, creates two independently addressed agents,
switches between them, types a prompt with real input events, and waits for the
selected agent to modify a temporary workspace through its private worker. Idle
and completed screenshots must differ, and structured logs must correlate the
actor route, worker result, draw packet sequence, and workspace effect.
The workload then sends another configuration command through the same UI and
fails if the actor closed its mailbox or stopped producing scheduler progress.

Further proofs should run turns concurrently, switch the selected UI agent
during output, cancel one without affecting another, replace the UI actor,
disconnect and reconnect the client, and attach a non-visual actor subscriber.

Related:

- [[../concepts/plugin]]
- [[../systems/plugin-runtime]]
- [[../systems/plugin-io-and-resources]]
- [[../systems/plugin-ui]]
- [[../systems/plugin-graphics]]
