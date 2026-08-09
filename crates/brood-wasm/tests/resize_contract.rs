use brood_wasm::{
    HostMessage, InteractionId, LayoutLeaseToken, PresentationMode, ResizeEpoch, ResolvedLayout,
    ReusePaneContent, SubmitSynchronizedLayout, SynchronizedLayoutAccepted,
    SynchronizedLayoutFragment, SynchronizedLayoutNotification,
};
use serde_json::{Value, json};

#[test]
fn synchronized_layout_host_message_is_flat_and_round_trips() {
    let message = HostMessage::SubmitSynchronizedLayout(SubmitSynchronizedLayout {
        request_id: 17,
        interaction: Some(InteractionId(23)),
        event_sequence: Some(29),
        native_resize_revision: Some(30),
        lease: LayoutLeaseToken {
            id: 31,
            generation: 37,
        },
        inventory_revision: 41,
        base_layout_revision: 43,
        fragment: SynchronizedLayoutFragment {
            layout: ResolvedLayout::empty(),
            rest: None,
        },
        focus: Some(47),
        wait_for_panes: vec![47, 53],
        presentation: PresentationMode::ScaleThenAtomic,
    });

    let encoded = serde_json::to_value(&message);
    assert!(encoded.is_ok());
    let Ok(encoded) = encoded else {
        return;
    };
    assert_eq!(
        encoded.get("type").and_then(Value::as_str),
        Some("submit-synchronized-layout")
    );
    assert_eq!(encoded.get("request_id").and_then(Value::as_u64), Some(17));
    assert_eq!(encoded.get("interaction").and_then(Value::as_u64), Some(23));
    assert_eq!(
        encoded
            .get("native_resize_revision")
            .and_then(Value::as_u64),
        Some(30)
    );
    assert_eq!(encoded.get("focus").and_then(Value::as_u64), Some(47));
    assert_eq!(
        encoded.get("presentation").and_then(Value::as_str),
        Some("scale-then-atomic")
    );
    assert!(encoded.get("payload").is_none());
    assert_eq!(
        serde_json::from_value::<HostMessage>(encoded).ok(),
        Some(message)
    );
}

#[test]
fn synchronized_layout_defaults_to_scale_then_atomic() {
    let encoded = json!({
        "type": "submit-synchronized-layout",
        "request_id": 17,
        "lease": {"id": 31, "generation": 37},
        "inventory_revision": 41,
        "base_layout_revision": 43,
        "fragment": {"layout": {"items": []}},
        "wait_for_panes": [],
    });
    let decoded = serde_json::from_value::<HostMessage>(encoded);
    assert!(decoded.is_ok());
    let Ok(HostMessage::SubmitSynchronizedLayout(request)) = decoded else {
        return;
    };
    assert_eq!(request.presentation, PresentationMode::ScaleThenAtomic);
    assert_eq!(request.interaction, None);
    assert_eq!(request.event_sequence, None);
    assert_eq!(request.native_resize_revision, None);
    assert_eq!(request.fragment.rest, None);
    assert_eq!(request.focus, None);
}

#[test]
fn optional_native_resize_and_focus_fields_do_not_change_pointer_request_shape() {
    let message = HostMessage::SubmitSynchronizedLayout(SubmitSynchronizedLayout {
        request_id: 17,
        interaction: Some(InteractionId(23)),
        event_sequence: Some(29),
        native_resize_revision: None,
        lease: LayoutLeaseToken {
            id: 31,
            generation: 37,
        },
        inventory_revision: 41,
        base_layout_revision: 43,
        fragment: SynchronizedLayoutFragment {
            layout: ResolvedLayout::empty(),
            rest: None,
        },
        focus: None,
        wait_for_panes: vec![47, 53],
        presentation: PresentationMode::ScaleThenAtomic,
    });

    let encoded = serde_json::to_value(message).expect("synchronized layout request encodes");
    assert!(encoded.get("native_resize_revision").is_none());
    assert!(encoded.get("focus").is_none());
}

#[test]
fn pane_content_reuse_host_message_is_flat_and_round_trips() {
    let message = HostMessage::ReusePaneContent(ReusePaneContent {
        pane: 71,
        resize_epoch: 73,
    });
    let encoded = serde_json::to_value(&message);
    assert_eq!(
        encoded.as_ref().ok(),
        Some(&json!({
            "type": "reuse-pane-content",
            "pane": 71,
            "resize_epoch": 73,
        }))
    );
    let Ok(encoded) = encoded else {
        return;
    };
    assert_eq!(
        serde_json::from_value::<HostMessage>(encoded).ok(),
        Some(message)
    );
}

#[test]
fn existing_host_message_shape_is_unchanged() {
    let message = HostMessage::LayoutAction {
        action: "split-right".to_string(),
    };
    assert_eq!(
        serde_json::to_value(&message).ok(),
        Some(json!({"type": "layout-action", "action": "split-right"}))
    );
}

#[test]
fn synchronized_layout_notification_is_typed_and_round_trips() {
    let notification = SynchronizedLayoutNotification::Accepted(SynchronizedLayoutAccepted {
        request_id: 59,
        resize_epoch: ResizeEpoch {
            id: 61,
            lease: 31,
            lease_generation: 37,
            base_layout_revision: 43,
            target_layout_revision: 67,
        },
        affected_panes: vec![47, 53],
        wait_for_panes: vec![47],
    });

    let encoded = serde_json::to_value(&notification);
    assert!(encoded.is_ok());
    let Ok(encoded) = encoded else {
        return;
    };
    assert_eq!(
        encoded.get("type").and_then(Value::as_str),
        Some("accepted")
    );
    assert_eq!(encoded.get("request_id").and_then(Value::as_u64), Some(59));
    assert!(encoded.get("payload").is_none());
    assert_eq!(
        serde_json::from_value::<SynchronizedLayoutNotification>(encoded).ok(),
        Some(notification)
    );
}
