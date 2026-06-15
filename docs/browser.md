# Browser Pane

The browser roadmap has two useful shapes: a real Chromium/Chrome managed app for testing workspace behavior, and a future minimal first-party browser pane client for low-overhead page display.

## Direction

The future browser pane client should own Chromium or CEF runtime details, while the Manifold session process owns pane lifecycle, focus, commands, permissions, and surface placement.

This keeps browser runtime details out of the core session binary and lets the browser crash, restart, or evolve independently.

For workspace behavior testing, Manifold should prefer real Chrome/Chromium. It exercises realistic app startup, input, surface, tab/window, DevTools, and lifecycle behavior better than a minimal pane browser would.

## Responsibilities

Browser pane client:

- Chromium/CEF lifecycle
- navigation and page state
- profile, cookies, cache, and permissions integration
- browser-specific input conversion
- JS bridge
- surface production
- title, loading state, URL, and metadata

Session process:

- starts and supervises browser pane clients
- routes input and browser commands
- records pane identity and placement
- owns workspace permissions and session lifecycle

Presentation endpoint:

- imports and composites browser surfaces
- handles clipping, transforms, animations, and damage

## Rendering

Preferred path:

- offscreen Chromium rendering
- shared GPU textures on supported Linux/WSL stacks
- CPU BGRA fallback for correctness

Native child windows are explicitly not the desired model.

## Current Prototype

`manifoldd` can launch Chromium as a hosted Wayland managed app through the local control socket:

```sh
manifoldd run --browser https://example.com
manifoldd ctl browser open https://example.com
manifoldd ctl browser open --extension /path/to/unpacked-extension --env FOO=bar --platform auto
manifoldd ctl browser open-config /path/to/browser-pane.json
manifoldd ctl browser list
manifoldd ctl browser list-json
manifoldd ctl browser inspect 0
manifoldd ctl browser inspect-json 0
manifoldd ctl browser tabs 0
manifoldd ctl browser new 0 https://example.org
manifoldd ctl browser activate 0 <target-id>
manifoldd ctl browser close-target 0 <target-id>
manifoldd ctl browser devtools 0 GET /json/version
manifoldd ctl browser close 0
manifoldd ctl browser close-all
```

Each `browser open` call uses an isolated profile directory by default, assigns its own local DevTools port, and defaults to Wayland/Ozone when a nested `WAYLAND_DISPLAY` is available. `--platform wayland` forces Wayland, `--platform x11` removes `WAYLAND_DISPLAY`, and `--platform auto` is the default. `--ui` opens a normal Chromium window; the default is `--app=<url>` for a minimal browser surface. Use `MANIFOLD_CHROMIUM` or `--chromium <path>` when Chromium is not on `PATH`.

If no extensions are configured, Manifold passes `--disable-extensions` for a lighter process. If local unpacked extensions are configured, their canonical paths are passed to both `--disable-extensions-except` and `--load-extension` so Chromium loads only the requested local extensions.

New browser toplevels are placed by Manifold's temporary pane allocator instead of all being mapped at the origin. The current allocator is a deterministic cascade bounded by the presentation size; it is intentionally small and will be replaced as the workspace/session model and presentation-surface behavior mature.

The `browser tabs`, `browser new`, `browser activate`, `browser close-target`, and `browser devtools` commands bridge to Chromium's local DevTools HTTP endpoint through Manifold's Unix control socket. This keeps browser control local and per-instance while avoiding a global remote debugging port.

`browser list-json` and `browser inspect-json` return machine-readable browser state with exact Chromium argv and redacted environment keys. `browser close`, `browser close-all`, and session `quit` terminate browser processes tracked by Manifold, so managed Chromium panes are not left behind when the session exits.

For exact local-control requests, `manifoldd ctl --json '["browser","list"]'` sends the JSON argv array directly to the socket. Browser launch config files use the same option names as `browser open`:

```json
{
  "url": "https://example.com",
  "chromium": "/usr/bin/chromium",
  "profile-dir": "/tmp/manifold-example-profile",
  "extensions": ["/home/me/ext"],
  "env": { "FOO": "bar" },
  "unset-env": ["DISPLAY"],
  "extra-args": ["--disable-gpu"],
  "app-mode": true,
  "platform": "auto"
}
```
