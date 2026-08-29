#![allow(
    clippy::expect_used,
    reason = "a failed setup or an unexpected result must fail this test"
)]

use std::{fs, path::Path};

use brood_wasm::{
    LayoutItemId, LayoutLeaseToken, LocalPoint, PointerPhase, PointerPolicyInvocation,
    PointerPolicyQuery, WasmPointerPolicy,
};
use tempfile::TempDir;

fn write_provider(body: &str, initial_pages: u32, tables: &str) -> (TempDir, std::path::PathBuf) {
    let directory = tempfile::tempdir().expect("create pointer-policy test directory");
    let path = directory.path().join("provider.wat");
    let module = format!(
        r#"(module
            (import "brood" "read_message" (func $read_message (result i32)))
            (memory (export "memory") {initial_pages} 128)
            {tables}
            (global $next (mut i32) (i32.const 1024))
            (func (export "brood_arena_alloc") (param $capacity i32) (param $align i32) (result i32)
                (local $result i32)
                global.get $next
                local.tee $result
                local.get $capacity
                i32.add
                global.set $next
                local.get $result)
            (func (export "brood_policy_provider_pointer_test")
                (param i32 i32 i32 i32) (result i32)
                {body}))"#
    );
    fs::write(&path, module).expect("write pointer-policy test module");
    (directory, path)
}

const fn query() -> PointerPolicyQuery {
    PointerPolicyQuery {
        interaction: None,
        event_sequence: 1,
        lease: LayoutLeaseToken {
            id: 1,
            generation: 1,
        },
        zone: LayoutItemId(1),
        phase: PointerPhase::ButtonPress,
        local_position: LocalPoint { u: 0.0, v: 0.0 },
        button: Some(0x110),
        modifiers: 0,
        captured: false,
    }
}

fn load(path: &Path) -> WasmPointerPolicy {
    WasmPointerPolicy::load(path, "test").expect("load bounded pointer policy")
}

#[test]
fn infinite_provider_exhausts_its_per_call_fuel() {
    let (_directory, path) = write_provider("(loop $forever (br $forever)) unreachable", 4, "");
    let error = load(&path)
        .handle_pointer(PointerPolicyInvocation::new(query(), None))
        .expect_err("infinite provider must exhaust fuel");
    let message = format!("{error:#}");
    assert!(
        message.contains("call WASM pointer policy"),
        "unexpected limit error: {message}"
    );
}

#[test]
fn trapping_provider_fails_the_synchronous_call() {
    let (_directory, path) = write_provider("unreachable", 4, "");
    let error = load(&path)
        .handle_pointer(PointerPolicyInvocation::new(query(), None))
        .expect_err("trapping provider must fail");
    assert!(format!("{error:#}").contains("call WASM pointer policy"));
}

#[test]
fn provider_initial_memory_cannot_exceed_the_store_limit() {
    let (_directory, path) = write_provider("i32.const 0", 65, "");
    let error = WasmPointerPolicy::load(&path, "test")
        .err()
        .expect("oversized provider memory must fail to instantiate");
    let message = format!("{error:#}");
    assert!(
        message.contains("memory") || message.contains("limit"),
        "unexpected memory-limit error: {message}"
    );
}

#[test]
fn aggregate_table_elements_cannot_exceed_the_store_limit() {
    let (_directory, path) = write_provider(
        "i32.const 0",
        4,
        "(table 3000 funcref) (table 3000 funcref)",
    );
    assert!(WasmPointerPolicy::load(&path, "test").is_err());
}

#[test]
fn one_oversized_table_is_refused_by_the_per_table_limit() {
    let (_directory, path) = write_provider("i32.const 0", 4, "(table 4097 funcref)");
    assert!(WasmPointerPolicy::load(&path, "test").is_err());
}
