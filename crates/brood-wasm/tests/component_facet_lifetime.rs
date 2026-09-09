use anyhow::{Result, ensure};
use wasmtime::{
    Engine, Store,
    component::{Component, Linker},
};

fn component(result_length: u32) -> String {
    format!(
        r#"(component
        (core module $guest
            (memory (export "memory") 1)
            (data (i32.const 16) "\01\02\03")
            (func (export "render") (result i32)
                (i32.store (i32.const 0) (i32.const 16))
                (i32.store (i32.const 4) (i32.const {result_length}))
                (i32.const 0))
            (func (export "cleanup") (param i32)
                (memory.fill (i32.const 16) (i32.const 9) (i32.const 3))))
        (core instance $instance (instantiate $guest))
        (alias core export $instance "memory" (core memory $memory))
        (alias core export $instance "render" (core func $render))
        (alias core export $instance "cleanup" (core func $cleanup))
        (func (export "render") (result (list u8))
            (canon lift (core func $render) (memory $memory) (post-return $cleanup))))"#
    )
}

#[test]
fn owned_lifting_preserves_bytes_before_automatic_post_return_cleanup() -> Result<()> {
    let engine = Engine::default();
    let component = Component::new(&engine, component(3))
        .map_err(|error| anyhow::anyhow!("compile lifetime fixture: {error}"))?;
    let mut store = Store::new(&engine, ());
    let instance = Linker::new(&engine)
        .instantiate(&mut store, &component)
        .map_err(|error| anyhow::anyhow!("instantiate lifetime fixture: {error}"))?;
    let render = instance
        .get_typed_func::<(), (Vec<u8>,)>(&mut store, "render")
        .map_err(|error| anyhow::anyhow!("typecheck owned result: {error}"))?;
    let (first,) = render
        .call(&mut store, ())
        .map_err(|error| anyhow::anyhow!("first call: {error}"))?;
    ensure!(first == [1, 2, 3], "cleanup changed the owned result");
    let (second,) = render
        .call(&mut store, ())
        .map_err(|error| anyhow::anyhow!("second call: {error}"))?;
    ensure!(
        second == [9, 9, 9],
        "post-return did not actually overwrite guest memory"
    );
    ensure!(
        first == [1, 2, 3],
        "later guest mutation changed the first result"
    );
    Ok(())
}

#[test]
fn flat_byte_list_range_is_rejected_before_host_vector_allocation() -> Result<()> {
    let engine = Engine::default();
    let component = Component::new(&engine, component(u32::MAX))
        .map_err(|error| anyhow::anyhow!("compile bounds fixture: {error}"))?;
    let mut store = Store::new(&engine, ());
    let instance = Linker::new(&engine)
        .instantiate(&mut store, &component)
        .map_err(|error| anyhow::anyhow!("instantiate bounds fixture: {error}"))?;
    let render = instance
        .get_typed_func::<(), (Vec<u8>,)>(&mut store, "render")
        .map_err(|error| anyhow::anyhow!("typecheck owned result: {error}"))?;
    let error = render
        .call(&mut store, ())
        .err()
        .ok_or_else(|| anyhow::anyhow!("forged list length was accepted"))?;
    ensure!(
        format!("{error:#}").contains("out of bounds"),
        "wrong rejection stage: {error:#}"
    );
    Ok(())
}
