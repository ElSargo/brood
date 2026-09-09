//! Restricted linker support for a facet instance of a trusted component.
//!
//! Every imported function is linked to a trap, including async functions and
//! functions nested in imported instances. Wasmtime dynamically lifts an
//! import's arguments before these handlers run. Callers that admit untrusted
//! components must account for that host allocation separately; this helper is
//! intended for trusted plugin artifacts.

use anyhow::{Result, bail};
use wasmtime::{
    Engine,
    component::{
        Component, Linker, LinkerInstance, ResourceType,
        types::{ComponentInstance, ComponentItem},
    },
};

/// Defines all function imports of `component` as traps.
///
/// Imported resource types are supplied as inert host resources so components
/// whose function signatures mention resources can instantiate. Imported core
/// modules and nested components are rejected.
pub fn link_denied_imports<T: Send + 'static>(
    linker: &mut Linker<T>,
    component: &Component,
) -> Result<()> {
    link_imports_with(linker, component, |_linker, _name, _path, _async| Ok(false))
}

/// Links imports recursively, allowing `hook` to define a narrow permitted
/// function before the default denial stub is installed.
pub fn link_imports_with<T: Send + 'static>(
    linker: &mut Linker<T>,
    component: &Component,
    mut hook: impl FnMut(&mut LinkerInstance<'_, T>, &str, &str, bool) -> Result<bool>,
) -> Result<()> {
    let engine = component.engine();
    for (name, import) in component.component_type().imports(engine) {
        match import.ty {
            ComponentItem::ComponentInstance(instance) => {
                let mut child = linker.instance(name)?;
                define_instance(engine, &mut child, &instance, name, &mut hook)?;
            }
            ComponentItem::ComponentFunc(function) => {
                let mut root = linker.root();
                if !hook(&mut root, name, name, function.async_())? {
                    define_function(&mut root, name, name, function.async_())?;
                }
            }
            ComponentItem::Resource(_) => {
                linker
                    .root()
                    .resource(name, ResourceType::host::<()>(), |_store, _rep| Ok(()))?;
            }
            ComponentItem::Component(_) | ComponentItem::Module(_) => {
                bail!(
                    "unsupported facet import `{name}`: components and core modules cannot be denied lazily"
                );
            }
            _ => {}
        }
    }
    Ok(())
}

fn define_instance<T: 'static>(
    engine: &Engine,
    linker: &mut LinkerInstance<'_, T>,
    instance: &ComponentInstance,
    prefix: &str,
    hook: &mut impl FnMut(&mut LinkerInstance<'_, T>, &str, &str, bool) -> Result<bool>,
) -> Result<()> {
    for (name, export) in instance.exports(engine) {
        let path = format!("{prefix}.{name}");
        match export.ty {
            ComponentItem::ComponentFunc(function) => {
                if !hook(linker, name, &path, function.async_())? {
                    define_function(linker, name, &path, function.async_())?;
                }
            }
            ComponentItem::Resource(_) => {
                linker.resource(name, ResourceType::host::<()>(), |_store, _rep| Ok(()))?;
            }
            ComponentItem::ComponentInstance(child_type) => {
                let mut child = linker.instance(name)?;
                define_instance(engine, &mut child, &child_type, &path, hook)?;
            }
            ComponentItem::Component(_) | ComponentItem::Module(_) => {
                bail!(
                    "unsupported facet import `{path}`: components and core modules cannot be denied lazily"
                );
            }
            _ => {}
        }
    }
    Ok(())
}

fn define_function<T: 'static>(
    linker: &mut LinkerInstance<'_, T>,
    name: &str,
    path: &str,
    asynchronous: bool,
) -> Result<()> {
    let denied = path.to_owned();
    if asynchronous {
        linker.func_new_concurrent(name, move |_access, _ty, _params, _results| {
            let denied = denied.clone();
            Box::pin(async move {
                Err(wasmtime::Error::msg(format!(
                    "denied facet import: {denied}"
                )))
            })
        })?;
    } else {
        linker.func_new(name, move |_store, _ty, _params, _results| {
            Err(wasmtime::Error::msg(format!(
                "denied facet import: {denied}"
            )))
        })?;
    }
    Ok(())
}
