use anyhow::{Context, Result, ensure};
use wasmtime::{
    Config, Engine, Store,
    component::{Component, Linker},
};

#[test]
fn denied_import_traps_during_component_constructor() -> Result<()> {
    let mut config = Config::new();
    config
        .wasm_component_model(true)
        .consume_fuel(true)
        .epoch_interruption(true);
    let engine =
        Engine::new(&config).map_err(|error| anyhow::anyhow!("constructor engine: {error}"))?;
    let component = Component::new(
        &engine,
        r#"
        (component
          (type $f (func))
          (import "denied" (func $denied (type $f)))
          (core func $lowered (canon lower (func $denied)))
          (core module $module
            (import "" "denied" (func $denied))
            (start $denied))
          (core instance $imports
            (export "denied" (func $lowered)))
          (core instance $instance
            (instantiate $module (with "" (instance $imports)))))
    "#,
    )
    .map_err(|error| anyhow::anyhow!("compile constructor component: {error}"))?;
    let mut linker = Linker::<()>::new(&engine);
    linker
        .define_unknown_imports_as_traps(&component)
        .map_err(|error| anyhow::anyhow!("define constructor trap: {error}"))?;
    let mut store = Store::new(&engine, ());
    store
        .set_fuel(100_000)
        .map_err(|error| anyhow::anyhow!("set constructor fuel: {error}"))?;
    store.set_epoch_deadline(1);
    store.epoch_deadline_trap();
    let error = linker
        .instantiate(&mut store, &component)
        .err()
        .context("constructor unexpectedly succeeded")?;
    ensure!(
        error.to_string().contains("unknown import"),
        "wrong constructor trap: {error}"
    );
    Ok(())
}
