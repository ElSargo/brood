//! Bounded mailbox admission while hosted actors finish starting.

use std::collections::BTreeMap;

use anyhow::{Result, ensure};

type Message = (String, Vec<u8>);

#[derive(Default)]
pub struct StartupMessages {
    actors: BTreeMap<String, Vec<Message>>,
    bytes: usize,
}

impl StartupMessages {
    pub fn push(&mut self, actor: &str, sender: String, body: Vec<u8>) -> Result<()> {
        ensure!(
            body.len() <= brood_wasm::MAX_MAILBOX_MESSAGE_BYTES,
            "actor startup message exceeds mailbox byte limit"
        );
        ensure!(
            self.actors.get(actor).map_or(0, Vec::len) < 64,
            "actor startup mailbox is full"
        );
        let bytes = sender.len().saturating_add(body.len());
        ensure!(
            self.bytes.saturating_add(bytes) <= brood_wasm::MAX_MAILBOX_GENERATION_BYTES,
            "actor startup mailboxes exceed byte budget"
        );
        self.actors
            .entry(actor.to_string())
            .or_default()
            .push((sender, body));
        self.bytes += bytes;
        Ok(())
    }

    /// Called only for the matching start request. Taking on failure or
    /// cancellation discards that candidate's messages instead of leaking
    /// subscriptions into a later actor with the same logical name.
    pub fn take(&mut self, actor: &str) -> Vec<Message> {
        let messages = self.actors.remove(actor).unwrap_or_default();
        for (sender, body) in &messages {
            self.bytes -= sender.len() + body.len();
        }
        messages
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_messages_keep_authenticated_senders_binary_bodies_and_order() {
        let mut messages = StartupMessages::default();
        assert!(
            messages
                .push("init", "presentation:client".into(), vec![0, 255])
                .is_ok()
        );
        assert!(
            messages
                .push("init", "host".into(), b"second".to_vec())
                .is_ok()
        );
        assert!(
            messages
                .push("other", "host".into(), b"isolated".to_vec())
                .is_ok()
        );
        assert_eq!(
            messages.take("init"),
            vec![
                ("presentation:client".into(), vec![0, 255]),
                ("host".into(), b"second".to_vec()),
            ]
        );
        assert!(messages.take("init").is_empty());
        assert_eq!(
            messages.take("other"),
            vec![("host".into(), b"isolated".to_vec())]
        );
        assert_eq!(messages.bytes, 0);
    }

    #[test]
    fn full_startup_mailbox_rejects_new_input_without_losing_admitted_messages() {
        let mut messages = StartupMessages::default();
        for value in 0..64 {
            assert!(messages.push("init", "client".into(), vec![value]).is_ok());
        }
        assert!(messages.push("init", "client".into(), vec![64]).is_err());
        let retained = messages.take("init");
        assert_eq!(retained.len(), 64);
        assert_eq!(retained.last(), Some(&("client".into(), vec![63])));
        assert_eq!(messages.bytes, 0);
        assert!(
            messages
                .push("init", "replacement".into(), vec![65])
                .is_ok()
        );
        assert_eq!(
            messages.take("init"),
            vec![("replacement".into(), vec![65])]
        );
    }
}
