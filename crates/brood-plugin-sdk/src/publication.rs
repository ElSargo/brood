//! Publication helper available to opt-in visual plugins.

use crate::brood::plugin::render_publication;
pub use crate::brood::plugin::render_publication::PublicationError;

/// Publish one opaque, revision-tagged result to the owning host generation.
pub fn publish(revision: u64, bytes: &[u8]) -> Result<(), PublicationError> {
    render_publication::publish(revision, bytes)
}
