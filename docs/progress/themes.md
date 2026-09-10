# Helix theme implementation

The implementation follows `docs/targets/systems/themes.md`. The target has not
been edited.

## Operator workflow

Set the top-level `theme` string in the Brood configuration passed to the client.
The complete Helix 25.07.1 collection is bundled: 172 runtime themes plus
Helix's `default` and `base16_default` themes. Ctrl+Alt+H opens the theme switcher;
focus previews, Enter/click saves, and Escape restores the saved theme. Place additional `<name>.toml` files in the `themes/`
directory beside that configuration. User files take precedence over bundled
files. `brood_theme::ThemeLoader::names` provides sorted discovery from these
same roots.

The client watches the configuration and theme directory (creating a missing
configuration directory). Atomic saves, theme
selection changes, and edits to inherited themes reload without restarting
actors. Invalid loads log `THEME_REJECTED` and retain the last valid snapshot.
At startup, an invalid selection falls back to bundled Ashen. There is no Helix
process or runtime dependency. A byte-identical resolved configuration does not
create a new revision.

## Compatibility and rendering

The compatibility baseline is Helix 25.07.1's
[`helix-view/src/theme.rs`](https://github.com/helix-editor/helix/blob/25.07.1/helix-view/src/theme.rs).
Bundled theme files are verbatim from that release's `runtime/themes`; author
comments are preserved. `crates/brood-theme/themes/LICENSE` contains Helix's
MPL 2.0 license, and `LICENSE-ashen` contains Ashen's upstream MIT license.

- Inheritance replaces complete named scope styles and merges palette entries
  before resolving references. A child palette override affects inherited roles.
- Lookup takes the longest dot-delimited scope match. It does not fill omitted
  fields from broader scopes when an exact partial style exists. `RoleStyle::over`
  explicitly composes a role over a plugin-chosen base.
- Palette values are literal hexadecimal RGB or decimal ANSI indices, as in
  Helix; palette aliases are rejected. Hex colors consume the first six digits,
  including when a theme supplies an alpha suffix (Helix ignores that suffix).
- Named terminal colors and decimal indices use an explicit host ANSI table,
  xterm color cube, and grayscale ramp. The `default` reference resolves to the
  host's Ashen text/background defaults according to the field being resolved.
  These choices are independent of the terminal running Brood.
- Missing essential text/background values use explicit host defaults. Missing
  other fields remain absent in the snapshot. Unknown attributes, modifiers,
  malformed colors, missing parents, and inheritance cycles reject the whole
  candidate. Inheritance is limited to 32 levels; input/output bytes, scope
  lengths, and role count are bounded.
- Snapshots preserve all nine documented modifier spellings and underline
  color/style. `underlined` maps to a line underline. SDK role text supports bold,
  italic, dim, reversed, hidden, crossed-out, line, and double-line decorations.
  Curl, dashed, and dotted underlines render as a single line. Blinking modifiers
  are static. This introduces no animation timer.
- Plugin mappings use Helix roles directly. Color-only drawing sites deliberately
  use a role's foreground or background; the status bar exercises full role text
  styles. Plugins choose their own mappings and base composition. No palette
  names or additional global surface/accent vocabulary enter the facet contract.
- Terminal snapshot version 5 retains semantic default-foreground/background
  markers. Rendering resolves these through `ui.text`/`ui.background`. Explicit
  RGB, even RGB numerically equal to the old defaults, remains literal. Indexed
  ANSI colors retain the terminal session's existing palette policy. Neovim's
  explicit grid highlights and application image pixels remain application data.

## Revision and lifetime behavior

`RenderRequest.configuration` contains the encoded immutable `ThemeSnapshot`;
`RenderToken.host_configuration_revision` is its revision. Every pane in an
invocation observes the same snapshot. The configuration is admitted by the
existing facet byte budget; theme decoding also enforces its own bounds.

Selection invalidates speculative CPU and native receipts and schedules every
visual actor independently of application publication. Existing committed input
pairs remain authoritative until replacement output commits. Configuration
revision already participates in no-change admission. Native preparation also
checks the current theme revision before promotion. Text caches key resolved
colors and font modifiers; pure facets create their caches per invocation.

The client remembers which exact scenes were displayed at a switch. Those scenes
may remain visible while replacements prepare. A hidden scene from an older
revision is omitted from composition until current output is ready. No global
redraw barrier is introduced. Native dividers, placeholders, automation overlays, and the composition
background use the same snapshot, converting sRGB colors to the render target's
linear space.

`THEME_PUBLISHED` and `THEME_RENDER_DEMAND` include revisions and monotonic times.
Demand records identify in-flight and pending-native work invalidated by a
switch. Existing ordinary/synchronized pair-commit diagnostics now include
`theme_revision` and `time_ns`; they retain actor, candidate, application
revision, and pane fields.

## Verification

The built-in `brood test themes` lane uses the existing isolated offscreen
session-switcher harness. The command definition is in
`crates/brood-cli/src/cli.rs`; the workload and log assertions are in
`tools/test-themes-live.sh` and `tools/check-theme-proof.py`.

Verified under `nix develop` on 2026-09-10:

- Host build and Clippy for `brood-client`, `brood-theme`, `brood-graphics`, and
  the SDK with facets enabled pass. All bundled components used by the nested
  harness were built from this worktree with locked dependency graphs.
- The theme/graphics/SDK library tests pass (17 tests), the facet-frame tests
  pass (9 tests), and status-bar/terminal UI/terminal session tests pass
  (50 tests). Frame tests explicitly cover unchanged application revision,
  rapid configuration supersession, preserved displayed input, and rejection
  of cross-configuration no-change output.
- The final built-in offscreen theme workload passes. Its 20 publications and
  158 paired commits contain zero superseded commits and 13 in-flight
  invalidations. The initial status bar stays on application revision 1 and
  commits the light theme in 19.671 ms. With the session-switcher facet's
  completion deliberately held for 120 ms, its matching commit takes 134.365 ms;
  the status bar does not wait for it.
- A real terminal processes a continuous shell-output workload during dark/light
  switches. Its first final-theme commit takes 20.157 ms. Screenshots show the
  default text changing from light gray to dark text while explicit RGB
  `#b4b4b4` remains gray on both backgrounds, including when selected.
- Screenshots were inspected for dark/light readability, native dividers,
  picker selection/filtering/scrolling, hidden-picker re-exposure, and the user
  theme's bold/italic/double-underlined status bar. Invalid selection leaves
  the last valid revision intact. User-theme file edits are detected without
  changing the selected name.

Portable evidence is in [theme-evidence](theme-evidence/theme-results.json).
The paired terminal captures are
[dark](theme-evidence/theme-terminal-dark.png),
[light](theme-evidence/theme-terminal-light.png), and
[selected](theme-evidence/theme-terminal-selection.png).
The real broodd status/actor metrics are preserved alongside the client
revision/commit summary.

An initial run reused the managed demo-window setup and encountered existing
250 ms startup layout timeouts before the shell became visible. The theme lane
now starts from the existing empty-workspace harness, then creates its own
terminal after the idle-theme checks. A broad SDK test invocation also tried to
link its WASM export example as a native executable; SDK library tests were run
with `--lib`, while the actual WASM artifacts were verified by the nested build.

## 2026-09-10 complete collection and picker previews

Imported all 172 Helix 25.07.1 runtime themes, both root default themes, the
upstream attribution README, and the full theme license directory. The build
script embeds every `.toml` file. All 174 bundled names resolve and round-trip
through the theme snapshot codec, including inherited themes.

The default presentation starts `theme-switcher`. Ctrl+Alt+H requests the host's
current catalogue and opens the shared picker with focus notifications. The
saved name appears first. Focus sends a preview request; selection sends a
commit request; cancellation or an empty match set restores the saved theme.
Repeated open actions are ignored while a request is pending or queued. Stable
IDs and result values are validated against the catalogue, and replies for
finished picker IDs cannot reverse a selection.

One host worker serializes filesystem reloads and theme commands, preserving a
single monotonic display revision. The committed snapshot and temporary preview
are separate. External valid config edits update the cancellation destination
without replacing an active preview. Saving reparses the latest config with
`toml_edit`, preserves unrelated settings and comments, and atomically replaces
the file. Failed saves preserve the file, restore the committed snapshot, and
report an error through the status bar. The init supervisor clears previews if
the picker or theme actor fails/stops. The host currently reserves a pane for
every presentation child; the shell waits for the theme actor's ready message
and keeps its reserved pane outside layout and workspace delegation.

The default package manifest, local component build/assembly, Nix guest lock
inventory, manual shell assemblies, and bundled-plugin coverage audit include
the new component. No target documents were changed.

Focused tests passed for the theme loader, host preview/persistence state,
picker (both renderer variants), theme switcher, session shell, and shared key
policy. The bundled-plugin audit and changed shell-script syntax checks passed.
Client Clippy still reports nine existing diagnostics in `complaint_capture.rs`;
none remain in the modified theme code. An initial offscreen proof inherited
`RUST_LOG=warn`, suppressing the commit evidence the harness waits for. The
built-in theme lane now explicitly enables its required metrics logging.

The final offscreen `test themes` run passed, including keyboard and pointer
previews, filtered and empty results, queued promotion, Enter and click saves,
cancellation after saving, and recovery from an intentionally malformed config
at commit time. Screenshots were inspected in dark Ashen, light Acme, and
inherited GitHub Light High Contrast. The run recorded 69 theme publications,
225 native commits, 82 in-flight invalidations, and zero superseded commits.
Client metrics, broodd metrics, and screenshots are in
[theme-switcher-evidence](theme-switcher-evidence/summary.json). The expanded
workload now waits for a committed, focused picker frame before subsequent
input; a fixed startup delay previously dropped a test shortcut during a
layout transition. The standard local package was also rebuilt and assembled.
