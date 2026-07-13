use std::str;

const BUFFER_BYTES: usize = 64 * 1024;

#[link(wasm_import_module = "manifold")]
unsafe extern "C" {
    fn sleep_ms(millis: i32);
    fn log(ptr: i32, len: i32) -> i32;
    fn status_json(ptr: i32, cap: i32) -> i32;
    fn control_json(req_ptr: i32, req_len: i32, resp_ptr: i32, resp_cap: i32) -> i32;
    fn wait_for_signal(resp_ptr: i32, resp_cap: i32) -> i32;
}

#[unsafe(no_mangle)]
pub extern "C" fn run() -> i32 {
    host_log("lifecycle proof: starting real animated Wayland client");
    let Ok(spawned) = host_control_json(r#"["spawn","weston-simple-egl","-f"]"#) else {
        host_log("lifecycle proof: graphical client spawn failed");
        return 1;
    };
    host_log(&format!(
        "lifecycle proof: EGL child handle {}",
        spawned.trim()
    ));
    let Ok(terminal) = host_control_json(r#"["spawn","weston-terminal"]"#) else {
        host_log("lifecycle proof: terminal client spawn failed");
        return 1;
    };
    host_log(&format!(
        "lifecycle proof: terminal child handle {}",
        terminal.trim()
    ));

    let mut ready = false;
    for _ in 0..200 {
        match host_status_json() {
            Ok(status) if status.contains(r#""windows":3"#) => {
                ready = true;
                break;
            }
            Ok(_) => unsafe { sleep_ms(25) },
            Err(()) => return 1,
        }
    }
    if !ready {
        host_log("lifecycle proof: three visible outer panes never became ready");
        return 1;
    }

    host_log("lifecycle proof: graphical workload ready; waiting for capture-and-finish");
    let Ok(signal) = host_wait_for_signal() else {
        host_log("lifecycle proof: signal wait ended without a named signal");
        return 1;
    };
    if signal != "capture-and-finish" {
        host_log("lifecycle proof: received the wrong signal");
        return 1;
    }

    if host_status_json().is_err() {
        host_log("lifecycle proof: final status request failed");
        return 1;
    }
    host_log("lifecycle proof: observed release signal and returning pass");
    0
}

fn host_log(message: &str) {
    unsafe {
        let _result = log(
            i32::try_from(message.as_ptr() as usize).unwrap_or(i32::MAX),
            i32::try_from(message.len()).unwrap_or(i32::MAX),
        );
    }
}

fn host_status_json() -> Result<String, ()> {
    let mut buffer = vec![0_u8; BUFFER_BYTES];
    let written = unsafe {
        status_json(
            i32::try_from(buffer.as_mut_ptr() as usize).unwrap_or(i32::MAX),
            i32::try_from(buffer.len()).unwrap_or(i32::MAX),
        )
    };
    decode_host_bytes(buffer, written)
}

fn host_control_json(request: &str) -> Result<String, ()> {
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

fn host_wait_for_signal() -> Result<String, ()> {
    let mut buffer = vec![0_u8; BUFFER_BYTES];
    let written = unsafe {
        wait_for_signal(
            i32::try_from(buffer.as_mut_ptr() as usize).unwrap_or(i32::MAX),
            i32::try_from(buffer.len()).unwrap_or(i32::MAX),
        )
    };
    decode_host_bytes(buffer, written)
}

fn decode_host_bytes(mut buffer: Vec<u8>, written: i32) -> Result<String, ()> {
    if written < 0 {
        return Err(());
    }
    let written = usize::try_from(written).map_err(|_| ())?;
    buffer.truncate(written);
    str::from_utf8(&buffer)
        .map(ToString::to_string)
        .map_err(|_| ())
}
