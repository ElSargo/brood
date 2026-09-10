---
status: target
type: system
authority: product
last-reviewed: 2026-09-10
---

# Themes As Pure Rendering Inputs

Brood supports Helix themes as a first-class visual configuration. The host
resolves a theme into named role styles and supplies an immutable snapshot to
plugin pure rendering. Each plugin maps its own semantic coloring onto those
roles. Changing the active theme redraws existing application views without
restarting plugins or requiring actors to publish new application state.

The shared vocabulary is Helix's semantic scopes, such as `ui.text` and
`ui.background`. Raw palette names are theme implementation details, not the
plugin contract. Brood does not insert a second global vocabulary of surface,
accent, or widget tokens between Helix roles and plugin code.

## Ownership

The host owns theme discovery, selection, loading, inheritance, palette-reference
resolution, scope fallback, validation, and revision lifetime. Themes from the
Helix collection and user-provided Helix theme files use the same resolution
path. Bundled themes retain their required attribution and licenses. Using a
theme does not require a running Helix instance.

Plugins own the mapping from their application semantics to theme roles and
the conversion of those styles into drawing commands. SDK helpers may share
mapping and style-application code without making widgets part of the host ABI.
Host-rendered chrome uses the same active role snapshot and owns its own mapping.

For example, a plugin can map body text to `ui.text.fg`, its background to
`ui.background.bg`, and an editing selection to `ui.selection`. These are
plugin mapping examples, not a requirement that every selected widget use an
editing-selection role. Roles carry styles rather than a single accent color.

## Theme Resolution

The input format and role vocabulary follow the
[Helix theme format](https://docs.helix-editor.com/themes.html), including named
scopes, foreground/background styles, palette references, and inheritance.
Brood resolves these before invoking plugin rendering. Plugins do not parse
theme files, look up palette names, or perform filesystem reads while rendering.

The resolved snapshot preserves foreground and background separately, along
with supported modifiers and underline attributes. An omitted style field
remains distinguishable from an explicit value so plugins can compose a role
over their base style. Missing roles use deterministic scope fallback; essential
text and background defaults must remain usable when a theme omits them.
Terminal-relative colors and default-color references must resolve against
explicit host defaults, never ambient terminal state.

The implementation must specify and verify inheritance, scope lookup, omitted
fields, default colors, and unsupported style behavior against its supported
Helix compatibility version. Unsupported attributes must have a documented,
deterministic treatment. Parsing a blinking modifier does not introduce an
animation scheduler under this target.

Theme loading is transactional. Invalid input, unresolved references, missing
parents, or inheritance cycles produce actionable diagnostics and leave the
last valid theme active. A valid built-in theme is available at startup.

## Render Contract And Live Changes

Conceptually:

```text
render(application revision, pane targets, target time, configuration, theme)
    -> draw outputs + interaction snapshot + next-frame demand

theme = immutable { revision, resolved role styles }
```

The role snapshot is an explicit input under [[plugin-pure-renderer]]. Its exact
storage and ABI representation are implementation choices; it must provide
bounded, deterministic role access with no mutable global theme lookup.
One invocation observes one complete theme revision across its requested panes.

Selecting or successfully reloading a theme publishes a new revision and
creates render demand even when application state and geometry are unchanged.
Theme changes coalesce through normal host scheduling. Hidden panes need not
render immediately, but their output must use the active theme before exposure.

The theme revision participates in invocation validity, retained-output reuse,
and any cache whose contents depend on theme styles. Superseded theme work
cannot newly commit after a theme switch. A no-change response cannot reuse
output from a different theme revision. Unrelated caches may remain reusable.

The previous committed frame may remain visible while replacement output is
prepared. New output and its interaction snapshot commit together under the
existing presentation contract. Input continues to refer to the displayed
frame. This target does not add a global barrier that waits for every plugin
to redraw before any pane can show the new theme.

Plugin semantic color categories should remain available in application views
until rendering maps them to styles. Application-supplied literal colors, such
as image pixels or terminal truecolor content, retain their meaning; theme
switching does not recolor arbitrary content. Terminal indexed-color policy is
separate from the Helix role vocabulary and must not assume palette names are
ANSI slots.

## Scope And Acceptance

Theme changes are immediate substitutions when the new output becomes ready.
Animation, interpolation, transition timing, and motion policy are separate
design work. This target does not change plugin layout ownership or introduce
a host widget styling engine.

Acceptance must demonstrate:

- Real bundled plugins and host chrome apply their semantic mappings across
  multiple Helix light and dark themes, including an inherited theme.
- A live switch updates an idle plugin without a new actor publication or
  restart, and works while another actor is busy.
- Rapid switches, in-flight renders, retained output, theme-dependent caches,
  and hidden-pane exposure cannot introduce superseded theme output.
- Invalid reloads preserve the last valid theme; missing scopes, partial
  styles, and supported modifiers have verified deterministic behavior.
- Nested offscreen workloads exercise text, selections, focus, input, and
  scrolling through the built-in test CLI under Nix. Screenshots verify role
  application and light/dark readability; client and broodd metrics record
  theme revision, render demand, stale work, and commit timing.

Related:

- [[plugin-pure-renderer]]
- [[plugin-ui]]
- [[render-scheduling]]
- [[terminal-plugin]]
