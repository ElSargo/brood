# Workspace layouts and focus — September 10, 2026

The request adds a selectable master stack+ layout and Ctrl+J/K workspace
focus cycling, and reports intermittent divider failure between two terminals.
The user confirmed that the stack limit is five windows excluding the master,
and that the resize failure occurred while dragging the divider.

The grid remains the initial layout. Master stack+ preserves insertion order,
keeps backend groups separate, raises the focused stack window, and caps the
visible fan at five stack windows. Wheel input moves through overflow in window
steps; keyboard focus reveals omitted windows. Ctrl+Alt+L cycles layouts.
These product choices are recorded in the layout target.

## Defects and evidence

The old focus policy preferred managed panes and exempted first-party terminals
from Ctrl+J/K. Focus now goes through the workspace actor's full pane inventory.
The first real-input proof found another defect: an already-applied ancestor
focus request remained in the durable lease tree, overriding subsequent child
requests. The existing host was confirmed to consume Ctrl+J and deliver it to
the workspace while typing continued to reach the same terminal. Both ordinary
layout commits and synchronized promotion now consume the tree's focus
requests. Incomplete compositions retain their pending requests, and competing
requests still use the existing outermost-first rule.

Overflow testing found two related focus handoff defects. Deferred host focus
must be published to the lease holder so it can reveal an omitted pane. Also,
host-synchronized promotion had compared current focus with the actor's requested
focus and treated any difference as newer user input. That discarded the first
focus action which revealed a hidden window. Transactions now retain actual
focus at admission separately from requested focus, preserving the actor request
unless focus really changes during the transaction.

Divider zones and the two-pane split orientation previously depended on the
paint wait set. A temporarily ineligible pane could remove a divider. Layout
geometry now stays available while synchronized requests wait only for eligible
panes. The regression test changes eligibility from two panes, to one, to none,
and back, checking the same divider and a real actor resize request each time.

The original package passed ordinary two-terminal narrow/wide drags with 20
intermediate positions and exact final widths (`/tmp/brood-layout-baseline2`).
This evidence does not prove that paint eligibility caused the user's incident.
A later six-terminal stress run (`/tmp/brood-layout-e2e8`) reproduced a resize
failure: `layout_terminal_text` panicked on a failed text allocation, retiring
the terminal and cancelling pointer capture after two visible divider positions.
The allocation handles in the trace had reached the glyph worker's bounded
queue depth while six terminals rendered simultaneously.

The host now distinguishes temporary glyph-queue pressure from hard limits and
a disconnected worker. A resource lease records that temporary rejection. The
terminal returns without trapping, and the frame coordinator discards the
partial candidate and retries on a scheduled render opportunity. Its last
accepted frame remains visible; partial resources retire on the existing
reclaimer. Queue size and resource budgets remain unchanged. Permanent budget
violations are not converted into endless retries.

The client previously forwarded wheel events directly to panes. Axis zones now
use the same bounded, lease-authorized pointer-policy route as buttons/motion.
Scroll deltas reach the policy and canonical actor message. Pane grabs retain
wheel ownership; events outside an axis zone continue to the pane.

## Verification

The focused graphical workload is `tools/test-workspace-layouts.sh`, using the
existing private offscreen Sway/session harness. It types into real terminal
PTYs, records their actual input recipients, exercises continuous divider drags,
clicks exposed stack strips, scrolls overflow, switches back to the grid, and
closes a terminal. Client/backend logs, geometry metrics, and screenshots remain
under its output directory.

Nix-backed checks passed: the package build and packaged key-policy smoke
check; 11 workspace-plugin tests; 7 layout/keybinding tests; 9 pointer-policy
and wire tests; 11 resource-lease/runtime tests; 26 terminal tests; 4 facet-frame
coordinator tests; and 8 native focus tests. Clippy with warnings denied passed
for the layout/keybinding/WASM crates and both changed WASM plugins. The client
Clippy check remains blocked by nine pre-existing diagnostics, all in
`clients/brood-client/src/complaint_capture.rs`; it reports none in the changed
client code.

The complete real-terminal workload passed in `/tmp/brood-layout-e2e11` against
`/nix/store/p98875wz58jm0rf4hx1iigv04y114l3k-brood`. All four continuous drags
reached exact target widths, with 19–20 distinct visible positions and no
capture cancellations. Six- and seven-window focus cycles reached every real
PTY exactly once. Opening the seventh window focused it immediately. Wheel
scroll retained the master and exactly five stack windows; exposed-edge clicks,
returning to the grid, and closing a terminal also passed. Screenshots were
visually inspected. The final run had no allocation pressure; the prior run
`/tmp/brood-layout-e2e10` exercised five deferred frames without losing an actor
and passed all four drags before exposing the now-fixed overflow focus defect.

Compact [metrics](workspace-layout-evidence/results.json) and screenshots are
preserved here: [five-window stack](workspace-layout-evidence/master-stack-five.png),
[middle window focused](workspace-layout-evidence/master-stack-middle-focused.png),
[scrolled overflow](workspace-layout-evidence/master-stack-overflow-scrolled.png),
and [grid restored](workspace-layout-evidence/grid-restored.png).

Initial proof setup needed an explicit click to establish focus. The terminal
latency-correlation pending counter is not an input-delivery drain barrier;
the proof instead waits for actual backend input delivery to stop changing
before attributing the next command. These setup failures do not count as
passing focus or layout coverage.
Another run (`/tmp/brood-layout-e2e9`) sent six virtual Ctrl+J chords but the
client received only five. The input helper now gives its virtual device time
to attach and remain alive after sending; the workload verifies each Ctrl+J/K
arrives before testing the resulting PTY recipient.

One isolated harness startup (`/tmp/brood-layout-e2e6`) never established the
root/rest composition and timed out before the workload. The same package
started successfully on subsequent runs. That startup failure is separate from
the divider failure reproduced after terminals were already running.
