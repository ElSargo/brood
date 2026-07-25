# Pane Clients

Pane clients are first-party or external processes that produce scene data or surfaces for one or more panes in a Brood session.

Likely first-party pane clients:

- browser pane client
- terminal pane client
- nested Wayland app host

Pane clients should communicate with the session process through explicit lifecycle, input, metadata, and surface channels.
