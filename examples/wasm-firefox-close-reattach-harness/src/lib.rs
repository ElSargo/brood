use std::str;

const BUFFER_BYTES: usize = 64 * 1024;
const WAIT_ATTEMPTS: usize = 300;
const WAIT_MS: i32 = 100;

#[link(wasm_import_module = "manifold")]
unsafe extern "C" {
    fn sleep_ms(millis: i32);
    fn log(ptr: i32, len: i32) -> i32;
    fn status_json(ptr: i32, cap: i32) -> i32;
    fn control_json(req_ptr: i32, req_len: i32, resp_ptr: i32, resp_cap: i32) -> i32;
}

#[unsafe(no_mangle)]
pub extern "C" fn run() -> i32 {
    match run_test() {
        Ok(()) => {
            host_log("firefox close/reattach regression passed");
            0
        }
        Err(message) => {
            host_log(&message);
            1
        }
    }
}

fn run_test() -> Result<(), String> {
    host_log("firefox close/reattach regression starting");
    wait_inner(
        |status| string_field(status, "presentation") == Some("Attached"),
        "inner attached",
    )?;
    wait_outer(
        |status| number_field(status, "windows").unwrap_or(0) >= 1,
        "outer hosts inner presentation",
    )?;

    nested_control(r#"["nested-inner","spawn","weston-simple-shm"]"#)?;
    wait_inner(
        |status| number_field(status, "windows").unwrap_or(0) >= 1,
        "sentinel client mapped",
    )?;

    nested_control(r#"["nested-inner","spawn","firefox"]"#)?;
    wait_inner(
        |status| number_field(status, "windows").unwrap_or(0) >= 2,
        "firefox mapped next to sentinel",
    )?;

    nested_control(r#"["layout-action","toggle-managed-fullscreen"]"#)?;
    wait_outer(
        |status| bool_field(status, "managed_fullscreen").unwrap_or(false),
        "presentation surface fullscreened in outer workspace",
    )?;

    nested_control(r#"["nested-inner","layout-action","close-active-pane"]"#)?;
    wait_inner(
        |status| {
            number_field(status, "windows").unwrap_or(usize::MAX) == 1
                && number_field(status, "focused_windows").unwrap_or(0) == 1
                && number_field(status, "wayland_dispatch_errors").unwrap_or(0) == 0
                && number_field(status, "xdg_configure_errors").unwrap_or(0) == 0
        },
        "firefox closed and sentinel stayed focused",
    )?;

    host_log("settling after firefox close");
    unsafe { sleep_ms(1000) };

    nested_control(r#"["layout-action","close-active-pane"]"#)?;
    wait_inner(
        |status| {
            string_field(status, "presentation") == Some("Detached")
                && number_field(status, "windows").unwrap_or(0) == 1
        },
        "presentation surface detached after window close",
    )?;

    host_log("launching firefox while presentation surface is detached");
    nested_control(r#"["nested-inner","spawn","firefox"]"#)?;
    wait_inner(
        |status| {
            string_field(status, "presentation") == Some("Detached")
                && number_field(status, "windows").unwrap_or(0) >= 2
                && number_field(status, "wayland_dispatch_errors").unwrap_or(0) == 0
                && number_field(status, "xdg_configure_errors").unwrap_or(0) == 0
        },
        "firefox mapped while presentation surface stayed detached",
    )?;

    nested_control(r#"["nested-inner","layout-action","close-active-pane"]"#)?;
    wait_inner(
        |status| {
            string_field(status, "presentation") == Some("Detached")
                && number_field(status, "windows").unwrap_or(usize::MAX) == 1
                && number_field(status, "focused_windows").unwrap_or(0) == 1
                && number_field(status, "wayland_dispatch_errors").unwrap_or(0) == 0
                && number_field(status, "xdg_configure_errors").unwrap_or(0) == 0
        },
        "detached firefox closed and sentinel stayed focused",
    )?;

    host_log("settling before presentation reattach");
    unsafe { sleep_ms(500) };

    host_log("requesting presentation reattach");
    nested_control(r#"["nested-inner","attach"]"#)?;
    unsafe { sleep_ms(1000) };
    wait_inner(
        |status| {
            string_field(status, "presentation") == Some("Attached")
                && number_field(status, "windows").unwrap_or(0) == 1
                && number_field(status, "focused_windows").unwrap_or(0) == 1
        },
        "presentation reattached with sentinel still focused",
    )?;
    wait_outer(
        |status| number_field(status, "windows").unwrap_or(0) >= 1,
        "outer hosts reattached presentation",
    )?;

    Ok(())
}

fn wait_outer(
    mut predicate: impl FnMut(&str) -> bool,
    description: &'static str,
) -> Result<String, String> {
    wait_for(
        description,
        || host_status_json(),
        |status| predicate(status),
    )
}

fn wait_inner(
    mut predicate: impl FnMut(&str) -> bool,
    description: &'static str,
) -> Result<String, String> {
    wait_for(
        description,
        || nested_control(r#"["nested-inner","status-json"]"#),
        |status| predicate(status),
    )
}

fn wait_for(
    description: &'static str,
    mut sample: impl FnMut() -> Result<String, String>,
    mut predicate: impl FnMut(&str) -> bool,
) -> Result<String, String> {
    let mut last = String::new();
    for _ in 0..WAIT_ATTEMPTS {
        match sample() {
            Ok(status) => {
                if predicate(&status) {
                    host_log(&format!("ok: {description}"));
                    return Ok(status);
                }
                last = status;
            }
            Err(err) => last = err,
        }
        unsafe { sleep_ms(WAIT_MS) };
    }
    Err(format!("timed out waiting for {description}; last={last}"))
}

fn nested_control(request: &str) -> Result<String, String> {
    let response = host_control_json(request)?;
    if response.starts_with("OK ") || response.starts_with('{') {
        Ok(response)
    } else {
        Err(format!(
            "control request failed: request={request} response={response}"
        ))
    }
}

fn host_log(message: &str) {
    unsafe {
        let _ = log(
            i32::try_from(message.as_ptr() as usize).unwrap_or(i32::MAX),
            i32::try_from(message.len()).unwrap_or(i32::MAX),
        );
    }
}

fn host_status_json() -> Result<String, String> {
    let mut buffer = vec![0_u8; BUFFER_BYTES];
    let written = unsafe {
        status_json(
            i32::try_from(buffer.as_mut_ptr() as usize).unwrap_or(i32::MAX),
            i32::try_from(buffer.len()).unwrap_or(i32::MAX),
        )
    };
    decode_host_bytes(buffer, written)
}

fn host_control_json(request: &str) -> Result<String, String> {
    let mut buffer = vec![0_u8; BUFFER_BYTES];
    let written = unsafe {
        control_json(
            i32::try_from(request.as_ptr() as usize).unwrap_or(i32::MAX),
            i32::try_from(request.len()).unwrap_or(i32::MAX),
            i32::try_from(buffer.as_mut_ptr() as usize).unwrap_or(i32::MAX),
            i32::try_from(buffer.len()).unwrap_or(i32::MAX),
        )
    };
    decode_host_bytes(buffer, written)
}

fn decode_host_bytes(mut buffer: Vec<u8>, written: i32) -> Result<String, String> {
    if written < 0 {
        return Err("host import returned failure".to_string());
    }
    let written = usize::try_from(written).map_err(|err| err.to_string())?;
    buffer.truncate(written);
    str::from_utf8(&buffer)
        .map(ToString::to_string)
        .map_err(|err| err.to_string())
}

fn number_field(status: &str, key: &str) -> Option<usize> {
    let value = raw_json_value(status, key)?;
    let end = value
        .bytes()
        .position(|byte| !byte.is_ascii_digit())
        .unwrap_or(value.len());
    value.get(..end)?.parse().ok()
}

fn bool_field(status: &str, key: &str) -> Option<bool> {
    let value = raw_json_value(status, key)?;
    if value.starts_with("true") {
        Some(true)
    } else if value.starts_with("false") {
        Some(false)
    } else {
        None
    }
}

fn string_field<'a>(status: &'a str, key: &str) -> Option<&'a str> {
    let value = raw_json_value(status, key)?;
    let value = value.strip_prefix('"')?;
    let end = value.find('"')?;
    value.get(..end)
}

fn raw_json_value<'a>(status: &'a str, key: &str) -> Option<&'a str> {
    let pattern = format!("\"{key}\":");
    let offset = status.find(&pattern)? + pattern.len();
    status.get(offset..).map(str::trim_start)
}
