//! Guest-side SDK for Brood Component Model plugins.
//!
//! The SDK is deliberately below plugin policy. It owns the generated ABI
//! bindings and reusable bounded mechanisms, while plugins continue to own
//! their actor topology, protocols, state machines, and presentation design.

pub mod actor;
pub mod graphics;
pub mod input;
pub mod layout;
pub mod runtime;
pub mod text;
pub mod ui;

/// Canonical bindings for the `brood-plugin` world.
///
/// Plugin crates implement [`exports::brood::plugin::actor::Guest`] and invoke
/// [`export!`] with this module as the type root. Keeping generation here makes
/// host-contract changes compile in one place instead of producing subtly
/// different bindings in every plugin.
#[allow(
    clippy::mem_forget,
    clippy::same_length_and_capacity,
    reason = "wit-bindgen owns canonical ABI allocation and deallocation glue"
)]
pub mod bindings {
    wit_bindgen::generate!({
        path: "../brood-wasm/wit",
        world: "brood-plugin",
        pub_export_macro: true,
    });
}

pub use bindings::{brood, exports};
