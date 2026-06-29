#![no_std]

use core::panic::PanicInfo;

const RESPONSE_CAP: usize = 2048;

static mut RESPONSE: [u8; RESPONSE_CAP] = [0; RESPONSE_CAP];

#[link(wasm_import_module = "manifold")]
unsafe extern "C" {
    fn visible_pane_id(index: i32) -> i64;
    fn set_map_response(ptr: *const u8, len: usize) -> i32;
}

#[unsafe(no_mangle)]
pub extern "C" fn on_map_v1(pending_pane: i64, visible_count: i32) -> i32 {
    if pending_pane <= 0 || visible_count <= 0 {
        return 0;
    }

    let mut writer = ResponseWriter::new();
    writer.push_str(r#"{"type":"resolved-layout","focus":"#);
    writer.push_i64(pending_pane);
    writer.push_str(r#","layout":{"items":["#);

    if visible_count == 1 {
        writer.push_pane_item(pending_pane, 0, -1, -1, 2, 2);
    } else {
        let first = unsafe { visible_pane_id(0) };
        if first <= 0 {
            return 0;
        }

        writer.push_pane_item(first, 0, -1, -1, 1, 2);
        let mut index = 1;
        while index < visible_count {
            let pane = unsafe { visible_pane_id(index) };
            if pane <= 0 {
                return 0;
            }
            writer.push_byte(b',');
            writer.push_pane_item(pane, i64::from(index), 0, -1, 1, 2);
            index += 1;
        }
    }

    writer.push_str(r#"]}}"#);

    if writer.overflowed {
        return 0;
    }

    unsafe { set_map_response((&raw const RESPONSE).cast::<u8>(), writer.len) }
}

struct ResponseWriter {
    len: usize,
    overflowed: bool,
}

impl ResponseWriter {
    const fn new() -> Self {
        Self {
            len: 0,
            overflowed: false,
        }
    }

    fn push_pane_item(
        &mut self,
        pane: i64,
        z: i64,
        offset_u: i64,
        offset_v: i64,
        size_u: i64,
        size_v: i64,
    ) {
        self.push_str(r#"{"id":"#);
        self.push_i64(pane);
        self.push_str(r#","kind":{"type":"pane","pane":"#);
        self.push_i64(pane);
        self.push_str(r#"},"z":"#);
        self.push_i64(z);
        self.push_str(r#","transform":{"offset":{"u":"#);
        self.push_i64(offset_u);
        self.push_str(r#","v":"#);
        self.push_i64(offset_v);
        self.push_str(r#"},"size":{"u":"#);
        self.push_i64(size_u);
        self.push_str(r#","v":"#);
        self.push_i64(size_v);
        self.push_str(r#"}},"input":"pane"}"#);
    }

    fn push_str(&mut self, value: &str) {
        for byte in value.as_bytes() {
            self.push_byte(*byte);
        }
    }

    fn push_i64(&mut self, value: i64) {
        if value < 0 {
            self.push_byte(b'-');
            self.push_u64(value.unsigned_abs());
        } else {
            self.push_u64(value as u64);
        }
    }

    fn push_u64(&mut self, mut value: u64) {
        let mut digits = [0_u8; 20];
        let mut len = 0;
        loop {
            digits[len] = b'0' + (value % 10) as u8;
            len += 1;
            value /= 10;
            if value == 0 {
                break;
            }
        }
        while len > 0 {
            len -= 1;
            self.push_byte(digits[len]);
        }
    }

    fn push_byte(&mut self, byte: u8) {
        if self.len >= RESPONSE_CAP {
            self.overflowed = true;
            return;
        }
        unsafe {
            RESPONSE[self.len] = byte;
        }
        self.len += 1;
    }
}

#[panic_handler]
fn panic(_info: &PanicInfo<'_>) -> ! {
    loop {}
}
