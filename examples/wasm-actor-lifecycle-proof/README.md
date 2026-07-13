# WASM actor lifecycle proof

This plugin declares two same-module actors. `layout` explicitly subscribes to
pane mapping and layout snapshots and visibly arranges two real clients.
`poison` deliberately declares no subscriptions; receiving a broadcast event
would make it spawn an unwanted client. The `replacement` feature changes the
layout from columns to rows while preserving the two public actor names, making
hot replacement visible in compositor screenshots rather than a success flag.
The `broken` feature gives the second actor an invalid ABI signature; the
nested proof inserts that artifact between the two valid generations and shows
that rejecting it leaves the first generation visible and owned.
