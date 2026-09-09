use anyhow::{Context, Result, ensure};
use brood_wasm::facet_linker::{link_denied_imports, link_imports_with};
use wasmtime::{
    Config, Engine, Store,
    component::{Component, Linker},
};

fn engine() -> Result<Engine> {
    let mut config = Config::new();
    config
        .wasm_component_model(true)
        .wasm_component_model_async(true)
        .wasm_component_model_async_stackful(true)
        .wasm_component_model_async_stackful(true)
        .concurrency_support(true);
    Ok(Engine::new(&config)?)
}

#[tokio::test(flavor = "current_thread")]
async fn exact_path_hook_can_replace_one_import_only() -> Result<()> {
    let engine = engine()?;
    let component = Component::new(
        &engine,
        r#"
        (component
          (type $f (func))
          (type $host-type (instance
            (export "permitted" (func (type $f)))
            (export "forbidden" (func (type $f)))))
          (import "host" (instance $host (type $host-type)))
          (alias export $host "permitted" (func $permitted))
          (alias export $host "forbidden" (func $forbidden))
          (core func $lowered-permitted (canon lower (func $permitted)))
          (core func $lowered-forbidden (canon lower (func $forbidden)))
          (core module $caller
            (import "" "permitted" (func $permitted))
            (import "" "forbidden" (func $forbidden))
            (func (export "permitted") call $permitted)
            (func (export "forbidden") call $forbidden))
          (core instance $imports
            (export "permitted" (func $lowered-permitted))
            (export "forbidden" (func $lowered-forbidden)))
          (core instance $caller-instance (instantiate $caller (with "" (instance $imports))))
          (alias core export $caller-instance "permitted" (core func $call-permitted))
          (alias core export $caller-instance "forbidden" (core func $call-forbidden))
          (func $permitted-export (type $f) (canon lift (core func $call-permitted)))
          (func $forbidden-export (type $f) (canon lift (core func $call-forbidden)))
          (export "permitted" (func $permitted-export))
          (export "forbidden" (func $forbidden-export)))
    "#,
    )?;
    let mut linker = Linker::<()>::new(&engine);
    link_imports_with(
        &mut linker,
        &component,
        |instance, name, path, asynchronous| {
            ensure!(!asynchronous);
            if path == "host.permitted" {
                instance.func_wrap(name, |_store, (): ()| Ok(()))?;
                Ok(true)
            } else {
                Ok(false)
            }
        },
    )?;
    let mut store = Store::new(&engine, ());
    let instance = linker.instantiate_async(&mut store, &component).await?;
    instance
        .get_typed_func::<(), ()>(&mut store, "permitted")?
        .call_async(&mut store, ())
        .await?;
    let error = instance
        .get_typed_func::<(), ()>(&mut store, "forbidden")?
        .call_async(&mut store, ())
        .await
        .err()
        .context("forbidden import succeeded")?;
    ensure!(format!("{error:#}").contains("denied facet import: host.forbidden"));
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn recursively_links_mixed_sync_and_async_imports() -> Result<()> {
    let engine = engine()?;
    let component = Component::new(
        &engine,
        r#"
        (component
          (type $sync-type (func))
          (type $async-type (func async))
          (type $host-type (instance
            (export "sync" (func (type $sync-type)))
            (export "async" (func (type $async-type)))))
          (import "host" (instance $host (type $host-type)))
          (alias export $host "sync" (func $sync))
          (alias export $host "async" (func $async))
          (core func $lowered-sync (canon lower (func $sync)))
          (core func $lowered-async (canon lower (func $async)))
          (core module $caller
            (import "" "sync" (func $sync))
            (import "" "async" (func $async))
            (func (export "run") call $sync)
            (func (export "run-async") call $async))
          (core instance $imports
            (export "sync" (func $lowered-sync))
            (export "async" (func $lowered-async)))
          (core instance $caller-instance
            (instantiate $caller (with "" (instance $imports))))
          (alias core export $caller-instance "run" (core func $run))
          (alias core export $caller-instance "run-async" (core func $run-async))
          (func $call-sync (type $sync-type) (canon lift (core func $run)))
          (export "call-sync" (func $call-sync))
          (func $call-async (type $async-type) (canon lift (core func $run-async) async))
          (export "call-async" (func $call-async)))
    "#,
    )?;
    let mut linker = Linker::<()>::new(&engine);
    link_denied_imports(&mut linker, &component)?;
    // Each failure owns a fresh instance and uses the component task driver.
    for (export, denied) in [("call-sync", "host.sync"), ("call-async", "host.async")] {
        let mut store = Store::new(&engine, ());
        let instance = linker.instantiate_async(&mut store, &component).await?;
        let function = instance.get_typed_func::<(), ()>(&mut store, export)?;
        let outcome = store
            .run_concurrent(async move |accessor| {
                let call =
                    accessor.with(|mut access| function.start_call_concurrent(&mut access, ()))?;
                function.finish_call_concurrent(accessor, call).await
            })
            .await;
        let error = outcome
            .and_then(|inner| inner)
            .err()
            .context("denied import succeeded")?;
        ensure!(
            format!("{error:#}").contains(&format!("denied facet import: {denied}")),
            "wrong failure for {export}: {error:#}"
        );
    }

    Ok(())
}

#[test]
fn constructor_trap_does_not_affect_independent_instance() -> Result<()> {
    let engine = engine()?;
    let failing = Component::new(
        &engine,
        r#"
        (component
          (type $f (func))
          (import "denied" (func $denied (type $f)))
          (core func $lowered (canon lower (func $denied)))
          (core module $module
            (import "" "denied" (func $denied))
            (start $denied))
          (core instance $imports (export "denied" (func $lowered)))
          (core instance $instance
            (instantiate $module (with "" (instance $imports)))))
    "#,
    )?;
    let healthy = Component::new(&engine, "(component)")?;
    let mut failing_linker = Linker::<()>::new(&engine);
    link_denied_imports(&mut failing_linker, &failing)?;
    let runtime = tokio::runtime::Builder::new_current_thread().build()?;
    let mut failing_store = Store::new(&engine, ());
    let error = runtime
        .block_on(failing_linker.instantiate_async(&mut failing_store, &failing))
        .err()
        .context("constructor unexpectedly succeeded")?;
    ensure!(format!("{error:#}").contains("denied facet import: denied"));

    let healthy_linker = Linker::<()>::new(&engine);
    let mut healthy_store = Store::new(&engine, ());
    runtime.block_on(healthy_linker.instantiate_async(&mut healthy_store, &healthy))?;
    Ok(())
}
