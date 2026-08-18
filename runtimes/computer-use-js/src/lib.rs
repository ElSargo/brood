use std::{cell::RefCell, mem, slice, str};

use rquickjs::{Context, Runtime};
use serde_json::json;

const BOOTSTRAP: &str = r#"
(() => {
  const state = { calls: [], console: [], windows: [], activeWindow: null };
  const push = (kind, fields = {}) => {
    state.calls.push(Object.assign({ kind }, fields));
    return state.calls.length - 1;
  };
  const action = (kind, fields = {}) => ({ kind, ...fields });
  const stringify = (value) => {
    if (typeof value === "string") return value;
    try { return JSON.stringify(value); } catch (_) { return String(value); }
  };
  const clone = (value) => JSON.parse(JSON.stringify(value));

  globalThis.console = Object.freeze({
    log: (...values) => state.console.push(values.map(stringify).join(" ")),
    info: (...values) => state.console.push(values.map(stringify).join(" ")),
    warn: (...values) => state.console.push(values.map(stringify).join(" ")),
    error: (...values) => state.console.push(values.map(stringify).join(" ")),
  });
  globalThis.computer = Object.freeze({
    screenshot: () => ({ __broodScreenshot: push("screenshot") }),
    display: (image) => push("display", { image }),
    perform: (actions) => push("perform", { actions }),
    windows: () => clone(state.windows),
    activeWindow: () => clone(state.activeWindow),
    click: (x, y, button = "left") => action("click", { x, y, button }),
    doubleClick: (x, y, button = "left", intervalMs = 100) =>
      action("double_click", { x, y, button, interval_ms: intervalMs }),
    move: (x, y) => action("move", { x, y }),
    drag: (path, button = "left", durationMs = 250) =>
      action("drag", { path, button, duration_ms: durationMs }),
    scroll: (x, y, deltaX, deltaY) => action("scroll", { x, y, delta_x: deltaX, delta_y: deltaY }),
    type: (text) => action("type", { text }),
    keypress: (keys) => action("keypress", { keys }),
    wait: (durationMs) => action("wait", { duration_ms: durationMs }),
  });
  globalThis.display = (image) => computer.display(image);
  globalThis.askUser = (prompt) => push("ask_user", { prompt: String(prompt) });
  Object.defineProperty(globalThis, "__broodSetContext", {
    configurable: false,
    writable: false,
    value: (windows, activeWindow) => {
      state.windows = clone(windows);
      state.activeWindow = clone(activeWindow);
    },
  });
  Object.defineProperty(globalThis, "__broodExec", {
    configurable: false,
    writable: false,
    value: (source) => {
    state.calls = [];
    state.console = [];
    try {
      const value = (0, eval)(source);
      let valueJson = null;
      if (value !== undefined) valueJson = JSON.stringify(value);
      return JSON.stringify({ calls: state.calls, console: state.console, value_json: valueJson, exception: null });
    } catch (error) {
      return JSON.stringify({
        calls: state.calls,
        console: state.console,
        value_json: null,
        exception: String(error && error.stack ? error.stack : error),
      });
    }
    },
  });
})();
"#;

struct JsState {
    _runtime: Runtime,
    context: Context,
}

thread_local! {
    static STATE: RefCell<Option<JsState>> = const { RefCell::new(None) };
}

fn initialize() -> Result<JsState, String> {
    let runtime = Runtime::new().map_err(|error| error.to_string())?;
    let context = Context::full(&runtime).map_err(|error| error.to_string())?;
    context
        .with(|context| context.eval::<(), _>(BOOTSTRAP))
        .map_err(|error| error.to_string())?;
    Ok(JsState {
        _runtime: runtime,
        context,
    })
}

fn execute(source: &str) -> String {
    STATE.with_borrow_mut(|slot| {
        if slot.is_none() {
            match initialize() {
                Ok(state) => *slot = Some(state),
                Err(error) => {
                    return json!({
                        "calls": [],
                        "console": [],
                        "value_json": null,
                        "exception": format!("initialize QuickJS: {error}"),
                    })
                    .to_string();
                }
            }
        }
        let Some(state) = slot.as_ref() else {
            return json!({
                "calls": [],
                "console": [],
                "value_json": null,
                "exception": "QuickJS state unavailable",
            })
            .to_string();
        };
        state.context.with(|context| {
            let globals = context.globals();
            if let Err(error) = globals.set("__broodSource", source) {
                return json!({
                    "calls": [],
                    "console": [],
                    "value_json": null,
                    "exception": format!("set script source: {error}"),
                })
                .to_string();
            }
            context
                .eval::<String, _>("__broodExec(__broodSource)")
                .unwrap_or_else(|error| {
                    json!({
                        "calls": [],
                        "console": [],
                        "value_json": null,
                        "exception": format!("evaluate script: {error}"),
                    })
                    .to_string()
                })
        })
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn cu_alloc(len: u32) -> u32 {
    let Ok(len) = usize::try_from(len) else {
        return 0;
    };
    let mut bytes = Vec::<u8>::with_capacity(len);
    let pointer = bytes.as_mut_ptr();
    mem::forget(bytes);
    u32::try_from(pointer as usize).unwrap_or(0)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn cu_free(pointer: u32, len: u32) {
    if pointer == 0 {
        return;
    }
    let Ok(len) = usize::try_from(len) else {
        return;
    };
    // SAFETY: The host returns only allocations created by `cu_alloc`, once.
    drop(unsafe { Vec::from_raw_parts(pointer as *mut u8, len, len) });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn cu_exec(pointer: u32, len: u32) -> u64 {
    let response = usize::try_from(len)
        .ok()
        .and_then(|len| {
            // SAFETY: The host writes exactly `len` initialized bytes into a
            // live allocation returned by `cu_alloc` before calling `cu_exec`.
            let bytes = unsafe { slice::from_raw_parts(pointer as *const u8, len) };
            str::from_utf8(bytes).ok().map(execute)
        })
        .unwrap_or_else(|| {
            json!({
                "calls": [],
                "console": [],
                "value_json": null,
                "exception": "script source is not valid UTF-8",
            })
            .to_string()
        });
    let mut bytes = response.into_bytes();
    let response_len = u32::try_from(bytes.len()).unwrap_or(0);
    let response_pointer = u32::try_from(bytes.as_mut_ptr() as usize).unwrap_or(0);
    mem::forget(bytes);
    (u64::from(response_len) << 32) | u64::from(response_pointer)
}
