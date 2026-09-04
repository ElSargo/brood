//! Managed-UI publication helpers.

use serde::Serialize;

use crate::exports::brood::plugin::actor::Command;

pub use brood_protocol::managed_ui::{
    MANAGED_UI_PROTOCOL_VERSION, UiAccessibility, UiAccessibilityRole, UiAction, UiActionValue,
    UiAlignment, UiAxis, UiColorToken, UiDocument, UiInsets, UiNode, UiNodeKind, UiOperation,
    UiPatch, UiPublication, UiStateMode, UiStyle, UiTextRole,
};

#[derive(Serialize)]
#[serde(rename_all = "kebab-case", tag = "type")]
enum UiHostMessage {
    PublishManagedUi(UiPublication),
}

/// Publish one snapshot, atomic patch, or pane removal through the actor's
/// authenticated host outbox.
pub fn publish(publication: UiPublication) -> Result<Command, serde_json::Error> {
    serde_json::to_vec(&UiHostMessage::PublishManagedUi(publication)).map(Command::SendHostMessage)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publication_uses_the_stable_host_discriminant() {
        let command = publish(UiPublication::RemovePane { pane: 7 });
        assert!(matches!(command, Ok(Command::SendHostMessage(_))));
        if let Ok(Command::SendHostMessage(bytes)) = command {
            assert!(String::from_utf8_lossy(&bytes).contains("publish-managed-ui"));
        }
    }
}
