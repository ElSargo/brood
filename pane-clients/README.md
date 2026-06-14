# Pane Clients

Pane clients are first-party or external processes that produce scene data or surfaces for panes in a Manifold session.

Likely first-party pane clients:

- browser pane client
- terminal pane client
- nested Wayland app host

Pane clients should communicate with the session process through explicit lifecycle, input, metadata, and surface channels.
