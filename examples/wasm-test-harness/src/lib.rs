use std::str;

const STATUS_BUFFER_BYTES: usize = 16 * 1024;

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
    host_log("test harness starting");

    for _ in 0..20 {
        let Ok(status) = host_status_json() else {
            host_log("failed to read status-json");
            return 1;
        };
        if status.contains("\"presentation\":\"Attached\"") {
            host_log("presentation attached");
            return 0;
        }
        unsafe { sleep_ms(50) };
    }

    let _ = host_control_json(r#"["status"]"#);
    let _ = host_wait_for_signal();
    host_log("presentation never attached; pausing");
    2
}

fn host_log(message: &str) {
    unsafe {
        let _ = log(
            i32::try_from(message.as_ptr() as usize).unwrap_or(i32::MAX),
            i32::try_from(message.len()).unwrap_or(i32::MAX),
        );
    }
}

fn host_status_json() -> Result<String, ()> {
    let mut buffer = vec![0_u8; STATUS_BUFFER_BYTES];
    let written = unsafe {
        status_json(
            i32::try_from(buffer.as_mut_ptr() as usize).unwrap_or(i32::MAX),
            i32::try_from(buffer.len()).unwrap_or(i32::MAX),
        )
    };
    decode_host_bytes(buffer, written)
}

fn host_control_json(request: &str) -> Result<String, ()> {
    let mut buffer = vec![0_u8; STATUS_BUFFER_BYTES];
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
    let mut buffer = vec![0_u8; STATUS_BUFFER_BYTES];
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
