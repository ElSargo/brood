use brood_protocol::facets::{
    HostRequest, InputEvent, InputRequest, InputResponse, InputToken, InvocationLimits,
    PaneContent, PaneOutput, PaneTarget, RenderRequest, RenderResponse, RenderToken, Scope,
};

const LIMITS: InvocationLimits = InvocationLimits {
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

const fn pane() -> PaneTarget {
    PaneTarget {
        pane: 41,
        logical_width: 640.0,
        logical_height: 480.0,
        physical_width: 1280,
        physical_height: 960,
        scale: 2.0,
        clip: [0.0, 0.0, 640.0, 480.0],
        visible: true,
        focused: true,
    }
}

const fn token() -> RenderToken {
    RenderToken {
        scope: Scope {
            client_generation: 3,
            actor_generation: 5,
            facet_generation: 7,
        },
        invocation: 11,
        snapshot_revision: 13,
        candidate: 17,
        pane_membership_revision: 19,
        geometry_revision: 23,
        host_configuration_revision: 29,
        renderer_generation: 31,
        interaction_revision: 37,
    }
}

#[test]
fn pane_geometry_accepts_zero_clip_extent() {
    let mut value = pane();
    value.logical_width = 0.0;
    value.logical_height = 0.0;
    value.clip = [-4.0, -8.0, 0.0, 0.0];
    assert!(value.validate().is_ok());
}

#[test]
fn pane_geometry_rejects_invalid_extents_scale_and_clip() {
    let mut value = pane();
    value.logical_width = -1.0;
    assert!(value.validate().is_err());
    value = pane();
    value.logical_height = f64::NAN;
    assert!(value.validate().is_err());
    value = pane();
    value.scale = 0.0;
    assert!(value.validate().is_err());
    value = pane();
    value.scale = f64::INFINITY;
    assert!(value.validate().is_err());
    value = pane();
    value.clip[0] = f64::NEG_INFINITY;
    assert!(value.validate().is_err());
    value = pane();
    value.clip[2] = -0.1;
    assert!(value.validate().is_err());
}

#[test]
#[allow(
    clippy::panic_in_result_fn,
    reason = "round-trip equality assertions are the purpose of this test"
)]
fn serde_roundtrip_preserves_render_and_input_identity() -> serde_json::Result<()> {
    let render_request = RenderRequest {
        token: token(),
        panes: vec![pane()],
        target_time_ns: 101,
        cutoff_ns: 97,
        configuration: vec![1, 2, 3],
        application_view: b"view-revision-13".to_vec(),
        sequence_state: b"sequence".to_vec(),
        limits: LIMITS,
    };
    let render_response = RenderResponse {
        token: token(),
        panes: vec![
            PaneOutput {
                pane: 41,
                content: PaneContent::Draw {
                    packet: vec![9, 8],
                    interaction: b"stable-session-41".to_vec(),
                },
            },
            PaneOutput {
                pane: 42,
                content: PaneContent::NoChange,
            },
        ],
        next_frame_ns: Some(200),
    };
    let input_request = InputRequest {
        token: InputToken {
            scope: token().scope,
            input_sequence: 43,
            frame: 47,
            pane: 41,
            snapshot_revision: 13,
            sequence_revision: 53,
        },
        geometry: pane(),
        event: InputEvent::PointerButton {
            x: 12.5,
            y: 33.0,
            button: 1,
            pressed: true,
            modifiers: 0,
        },
        interaction: b"session-id-41".to_vec(),
        sequence_state: b"capture-state".to_vec(),
        limits: LIMITS,
    };
    let input_response = InputResponse {
        token: input_request.token.clone(),
        next_sequence_state: b"next".to_vec(),
        events: vec![b"activate-session:41".to_vec()],
        requests: vec![HostRequest::Focus, HostRequest::Render],
    };

    let render_request_json = serde_json::to_vec(&render_request)?;
    let render_response_json = serde_json::to_vec(&render_response)?;
    let input_request_json = serde_json::to_vec(&input_request)?;
    let input_response_json = serde_json::to_vec(&input_response)?;
    assert_eq!(
        serde_json::from_slice::<RenderRequest>(&render_request_json)?,
        render_request
    );
    assert_eq!(
        serde_json::from_slice::<RenderResponse>(&render_response_json)?,
        render_response
    );
    assert_eq!(
        serde_json::from_slice::<InputRequest>(&input_request_json)?,
        input_request
    );
    assert_eq!(
        serde_json::from_slice::<InputResponse>(&input_response_json)?,
        input_response
    );
    Ok(())
}
