use serde::Serialize;

const ARENA_CAPACITY: usize = 128 * 1024;
const RESPONSE_CAPACITY: usize = 4096;

static mut ARENA: [u8; ARENA_CAPACITY] = [0; ARENA_CAPACITY];
static mut RESPONSE: [u8; RESPONSE_CAPACITY] = [0; RESPONSE_CAPACITY];
static mut DESCRIPTOR: [i32; 4] = [0; 4];

#[derive(Serialize)]
struct PolicyOutcome<'a> {
    decision: &'a str,
    messages: [ActorMessage<'a>; 1],
}

#[derive(Serialize)]
struct ActorMessage<'a> {
    actor: &'a str,
    payload: &'a [u8],
}

const HOST_LAYOUT_MESSAGE: &[u8] = br#"{
  "type":"set-resolved-layout",
  "layout":{"items":[
    {
      "id":101,
      "kind":{"type":"pane","pane":1},
      "z":0.0,
      "transform":{"offset":{"u":-1.0,"v":-1.0},"size":{"u":1.0,"v":2.0}},
      "input":"pane"
    },
    {
      "id":102,
      "kind":{"type":"pane","pane":2},
      "z":1.0,
      "transform":{"offset":{"u":0.0,"v":-1.0},"size":{"u":1.0,"v":2.0}},
      "input":"pane"
    }
  ]},
  "focus":2
}"#;

#[unsafe(no_mangle)]
pub extern "C" fn manifold_arena_alloc(capacity: i32, _align: i32) -> i32 {
    if capacity < 0 || usize::try_from(capacity).map_or(true, |value| value > ARENA_CAPACITY) {
        return 0;
    }
    i32::try_from((&raw mut ARENA).cast::<u8>().addr()).unwrap_or_default()
}

#[unsafe(no_mangle)]
pub extern "C" fn manifold_arena_free(_ptr: i32, _capacity: i32, _align: i32) {}

#[unsafe(no_mangle)]
pub extern "C" fn manifold_policy_provider_key_workspace(
    _query_ptr: i32,
    _query_len: i32,
    _state_ptr: i32,
    _state_len: i32,
) -> i32 {
    let outcome = PolicyOutcome {
        decision: "consume",
        messages: [ActorMessage {
            actor: "host",
            payload: HOST_LAYOUT_MESSAGE,
        }],
    };
    let Ok(bytes) = serde_json::to_vec(&outcome) else {
        return -1;
    };
    if bytes.len() > RESPONSE_CAPACITY {
        return -1;
    }

    let response_ptr = (&raw mut RESPONSE).cast::<u8>();
    unsafe {
        core::ptr::copy_nonoverlapping(bytes.as_ptr(), response_ptr, bytes.len());
    }
    let Ok(response_ptr_i32) = i32::try_from(response_ptr.addr()) else {
        return -1;
    };
    let Ok(response_len_i32) = i32::try_from(bytes.len()) else {
        return -1;
    };
    let descriptor_ptr = (&raw mut DESCRIPTOR).cast::<i32>();
    unsafe {
        descriptor_ptr.write(1);
        descriptor_ptr.add(1).write(response_ptr_i32);
        descriptor_ptr.add(2).write(response_len_i32);
        descriptor_ptr.add(3).write(0);
    }
    i32::try_from(descriptor_ptr.addr()).unwrap_or(-1)
}
