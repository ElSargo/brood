#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    reason = "fixed codec fixtures assert lengths before indexing"
)]

use brood_protocol::{facet_codec::*, facets::*};

const GENEROUS: DecodeLimits = DecodeLimits {
    max_wire_bytes: 64 * 1024,
    max_allocated_bytes: 64 * 1024,
    max_allocations: 64,
};
const INVOCATION_LIMITS: InvocationLimits = InvocationLimits {
    max_output_bytes: 4096,
    max_packet_bytes: 2048,
    max_interaction_bytes: 1024,
    max_sequence_bytes: 512,
    max_events: 8,
    max_event_bytes: 256,
    max_host_requests: 4,
    max_resources: 16,
    max_resource_bytes: 65_536,
};

const fn scope() -> Scope {
    Scope {
        client_generation: 1,
        actor_generation: 2,
        facet_generation: 3,
    }
}
const fn render_token() -> RenderToken {
    RenderToken {
        scope: scope(),
        invocation: 4,
        snapshot_revision: 5,
        candidate: 6,
        pane_membership_revision: 7,
        geometry_revision: 8,
        host_configuration_revision: 9,
        renderer_generation: 10,
        interaction_revision: 11,
    }
}
const fn input_token() -> InputToken {
    InputToken {
        scope: scope(),
        input_sequence: 12,
        frame: 13,
        pane: 14,
        snapshot_revision: 15,
        sequence_revision: 16,
    }
}
const fn pane() -> PaneTarget {
    PaneTarget {
        pane: 14,
        logical_width: 80.5,
        logical_height: 24.25,
        physical_width: 161,
        physical_height: 49,
        scale: 2.0,
        clip: [1.0, 2.0, 70.0, 20.0],
        visible: true,
        focused: false,
    }
}
fn encoded<T>(
    encode: impl FnOnce(&T, &mut [u8]) -> Result<usize, CodecError>,
    value: &T,
) -> Vec<u8> {
    let mut bytes = vec![0; GENEROUS.max_wire_bytes];
    let len = encode(value, &mut bytes).expect("fixture must encode");
    bytes.truncate(len);
    bytes
}

#[test]
fn all_four_message_shapes_round_trip() {
    let render_request = RenderRequest {
        token: render_token(),
        panes: vec![pane()],
        target_time_ns: 17,
        cutoff_ns: 18,
        configuration: b"config".to_vec(),
        application_view: b"view".to_vec(),
        sequence_state: b"sequence".to_vec(),
        limits: INVOCATION_LIMITS,
    };
    let bytes = encoded(encode_render_request, &render_request);
    assert_eq!(
        encode_render_request_owned(&render_request, bytes.len()),
        Ok(bytes.clone())
    );
    assert_eq!(
        encode_render_request_owned(&render_request, bytes.len() - 1),
        Err(CodecError::Limit)
    );
    assert_eq!(decode_render_request(&bytes, GENEROUS), Ok(render_request));

    let render_response = RenderResponse {
        token: render_token(),
        panes: vec![
            PaneOutput {
                pane: 14,
                content: PaneContent::Draw {
                    packet: b"packet".to_vec(),
                    interaction: b"interaction".to_vec(),
                },
            },
            PaneOutput {
                pane: 15,
                content: PaneContent::NoChange,
            },
        ],
        next_frame_ns: Some(19),
    };
    let bytes = encoded(encode_render_response, &render_response);
    assert_eq!(
        decode_render_response(&bytes, GENEROUS),
        Ok(render_response)
    );

    let input_request = InputRequest {
        token: input_token(),
        geometry: pane(),
        event: InputEvent::Text {
            text: "hello".into(),
        },
        interaction: b"map".to_vec(),
        sequence_state: b"keys".to_vec(),
        limits: INVOCATION_LIMITS,
    };
    let bytes = encoded(encode_input_request, &input_request);
    assert_eq!(
        encode_input_request_owned(&input_request, bytes.len()),
        Ok(bytes.clone())
    );
    assert_eq!(
        encode_input_request_owned(&input_request, bytes.len() - 1),
        Err(CodecError::Limit)
    );
    assert_eq!(decode_input_request(&bytes, GENEROUS), Ok(input_request));

    let input_response = InputResponse {
        token: input_token(),
        next_sequence_state: b"next".to_vec(),
        events: vec![b"one".to_vec(), b"two".to_vec()],
        requests: vec![
            HostRequest::Capture,
            HostRequest::ReleaseCapture,
            HostRequest::Focus,
            HostRequest::Render,
        ],
    };
    let bytes = encoded(encode_input_response, &input_response);
    assert_eq!(decode_input_response(&bytes, GENEROUS), Ok(input_response));
}

#[test]
fn every_input_event_variant_round_trips() {
    let events = vec![
        InputEvent::PointerMotion { x: 1.5, y: -2.0 },
        InputEvent::PointerButton {
            x: 3.0,
            y: 4.0,
            button: 2,
            pressed: true,
            modifiers: 0x13,
        },
        InputEvent::PointerScroll {
            x: 5.0,
            y: 6.0,
            horizontal: -1.25,
            vertical: 8.5,
            modifiers: 7,
        },
        InputEvent::Key {
            code: 42,
            logical_key: "Enter".into(),
            modifiers: 3,
            pressed: true,
            repeat: false,
        },
        InputEvent::Text { text: "λ".into() },
        InputEvent::Paste {
            text: "paste\ntext".into(),
        },
        InputEvent::Image {
            name: "pixel".into(),
            mime_type: "image/rgba".into(),
            bytes: vec![1, 2, 3],
            preview_width: 1,
            preview_height: 1,
            preview_rgba8: vec![4, 5, 6, 7],
        },
        InputEvent::Composition {
            text: "compose".into(),
            committed: false,
        },
        InputEvent::FocusLost,
        InputEvent::CaptureLost,
        InputEvent::PointerLeave,
    ];
    for event in events {
        let value = InputRequest {
            token: input_token(),
            geometry: pane(),
            event,
            interaction: vec![1],
            sequence_state: vec![2],
            limits: INVOCATION_LIMITS,
        };
        let bytes = encoded(encode_input_request, &value);
        assert_eq!(decode_input_request(&bytes, GENEROUS), Ok(value));
    }
}

#[test]
fn rejects_truncation_trailing_version_and_kind() {
    let value = RenderResponse {
        token: render_token(),
        panes: Vec::new(),
        next_frame_ns: None,
    };
    let bytes = encoded(encode_render_response, &value);
    for end in 0..bytes.len() {
        assert_eq!(
            decode_render_response(&bytes[..end], GENEROUS),
            Err(CodecError::Invalid)
        );
    }
    let mut trailing = bytes.clone();
    trailing.push(0);
    assert_eq!(
        decode_render_response(&trailing, GENEROUS),
        Err(CodecError::Invalid)
    );
    let mut wrong_version = bytes.clone();
    wrong_version[3] = 2;
    assert_eq!(
        decode_render_response(&wrong_version, GENEROUS),
        Err(CodecError::Invalid)
    );
    assert_eq!(
        decode_render_request(&bytes, GENEROUS),
        Err(CodecError::Invalid)
    );
}

#[test]
fn malformed_counts_and_limits_fail_before_useful_allocation() {
    let value = RenderResponse {
        token: render_token(),
        panes: vec![PaneOutput {
            pane: 14,
            content: PaneContent::Draw {
                packet: vec![1; 32],
                interaction: vec![2; 16],
            },
        }],
        next_frame_ns: None,
    };
    let bytes = encoded(encode_render_response, &value);

    let mut malformed = bytes.clone();
    let pane_count_offset = 5 + 88;
    malformed[pane_count_offset..pane_count_offset + 4].copy_from_slice(&u32::MAX.to_le_bytes());
    assert_eq!(
        decode_render_response(&malformed, GENEROUS),
        Err(CodecError::Invalid)
    );

    let wire_limited = DecodeLimits {
        max_wire_bytes: bytes.len() - 1,
        ..GENEROUS
    };
    assert_eq!(
        decode_render_response(&bytes, wire_limited),
        Err(CodecError::Limit)
    );
    let allocation_limited = DecodeLimits {
        max_allocated_bytes: 1,
        ..GENEROUS
    };
    assert_eq!(
        decode_render_response(&bytes, allocation_limited),
        Err(CodecError::Limit)
    );
    let count_limited = DecodeLimits {
        max_allocations: 1,
        ..GENEROUS
    };
    assert_eq!(
        decode_render_response(&bytes, count_limited),
        Err(CodecError::Limit)
    );
}

#[test]
fn encode_uses_only_the_caller_slice() {
    let value = InputResponse {
        token: input_token(),
        next_sequence_state: vec![1, 2, 3],
        events: vec![vec![4]],
        requests: vec![HostRequest::Render],
    };
    let mut exact = vec![0; 512];
    let len = encode_input_response(&value, &mut exact).expect("encode");
    let mut short = vec![0; len - 1];
    assert_eq!(
        encode_input_response(&value, &mut short),
        Err(CodecError::Limit)
    );
    assert_eq!(decode_input_response(&exact[..len], GENEROUS), Ok(value));
}

#[test]
fn owned_response_encoding_uses_exact_length_and_enforces_ceiling() {
    let input = InputResponse {
        token: input_token(),
        next_sequence_state: vec![1, 2],
        events: vec![vec![3; 100]],
        requests: vec![HostRequest::Render],
    };
    let mut supplied = vec![0; 4096];
    let len = encode_input_response(&input, &mut supplied).expect("encode input");
    let owned = encode_input_response_owned(&input, 16 * 1024 * 1024).expect("sized input");
    assert_eq!(owned.len(), len);
    assert_eq!(owned, &supplied[..len]);
    assert_eq!(
        encode_input_response_owned(&input, len - 1),
        Err(CodecError::Limit)
    );
    let render = RenderResponse {
        token: render_token(),
        panes: vec![PaneOutput {
            pane: 9,
            content: PaneContent::Draw {
                packet: vec![4; 256],
                interaction: vec![5; 16],
            },
        }],
        next_frame_ns: Some(42),
    };
    let len = encode_render_response(&render, &mut supplied).expect("encode render");
    let owned = encode_render_response_owned(&render, 16 * 1024 * 1024).expect("sized render");
    assert_eq!(owned.len(), len);
    assert_eq!(owned, &supplied[..len]);
    assert_eq!(
        encode_render_response_owned(&render, len - 1),
        Err(CodecError::Limit)
    );
}
