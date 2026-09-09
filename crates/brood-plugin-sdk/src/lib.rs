//! Guest-side SDK for Brood Component Model plugins.
//!
//! The SDK is deliberately below plugin policy. It owns the generated ABI
//! bindings and reusable bounded mechanisms, while plugins continue to own
//! their actor topology, protocols, state machines, and presentation design.

pub mod actor;
pub mod graphics;
pub mod input;
pub mod layout;
pub mod packages;
pub mod runtime;
pub mod text;

#[cfg(feature = "facets")]
pub mod publication;

/// Shared versioned wire protocol for optional facet exports. Encoding writes
/// into caller-admitted storage; decoding requires explicit allocation limits.
#[cfg(feature = "facets")]
pub use brood_protocol::{facet_codec, facets};

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
    #[cfg(not(feature = "facets"))]
    wit_bindgen::generate!({
        path: "../brood-wasm/wit",
        world: "brood-plugin",
        pub_export_macro: true,
    });

    #[cfg(feature = "facets")]
    wit_bindgen::generate!({
        path: "../brood-wasm/wit",
        world: "brood-visual-plugin",
        pub_export_macro: true,
    });
}

pub use bindings::{brood, exports};

/// Guest implementation of the two optional pure facet operations.
#[cfg(feature = "facets")]
pub trait Facets {
    fn render(input: Vec<u8>) -> Vec<u8>;
    fn interpret(input: Vec<u8>) -> Vec<u8>;
}

/// Connects a [`Facets`] implementation to the generated visual-plugin world
/// and exports the component. The type must also implement the ordinary actor
/// guest interface.
#[cfg(feature = "facets")]
#[macro_export]
macro_rules! export_visual_plugin {
    ($plugin:ident) => {
        impl $crate::exports::brood::plugin::facets::Guest for $plugin {
            fn render(input: ::std::vec::Vec<u8>) -> ::std::vec::Vec<u8> {
                <$plugin as $crate::Facets>::render(input)
            }

            fn interpret(input: ::std::vec::Vec<u8>) -> ::std::vec::Vec<u8> {
                <$plugin as $crate::Facets>::interpret(input)
            }
        }

        $crate::bindings::export!($plugin with_types_in $crate::bindings);
    };
}
