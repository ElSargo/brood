use brood_wasm::{
    CursorRole, DividerAxis, DividerRole, InteractionZone, LayoutDivider, LayoutItemId, LocalPoint,
    LocalTransform, PointerEventMask, ResolvedLayout, ZoneInputBehavior,
};

const fn transform(u: f64, v: f64, width: f64, height: f64) -> LocalTransform {
    LocalTransform {
        offset: LocalPoint { u, v },
        size: LocalPoint {
            u: width,
            v: height,
        },
    }
}

#[test]
fn legacy_layout_json_defaults_zones_and_dividers_without_changing_shape() {
    let parsed = serde_json::from_str::<ResolvedLayout>(r#"{"items":[]}"#);
    assert!(parsed.as_ref().is_ok_and(|layout| {
        layout.items.is_empty() && layout.zones.is_empty() && layout.dividers.is_empty()
    }));
    let serialized = parsed.and_then(|layout| serde_json::to_string(&layout));
    assert!(serialized.is_ok_and(|json| json == r#"{"items":[]}"#));
}

#[test]
fn interaction_chrome_round_trips_with_all_initial_event_classes() {
    let layout = ResolvedLayout {
        items: Vec::new(),
        zones: vec![InteractionZone {
            id: LayoutItemId(1),
            transform: transform(-0.05, -1.0, 0.1, 2.0),
            z: 1.0,
            event_mask: PointerEventMask::BUTTON
                | PointerEventMask::MOTION
                | PointerEventMask::RELEASE
                | PointerEventMask::CANCEL,
            cursor: CursorRole::ColResize,
            input: ZoneInputBehavior::Drag,
        }],
        dividers: vec![LayoutDivider {
            id: LayoutItemId(2),
            zone: LayoutItemId(1),
            z: 2.0,
            transform: transform(-0.005, -1.0, 0.01, 2.0),
            axis: DividerAxis::Vertical,
            role: DividerRole::Split,
        }],
    };

    let round_trip = serde_json::to_vec(&layout)
        .and_then(|encoded| serde_json::from_slice::<ResolvedLayout>(&encoded));
    assert!(round_trip.as_ref().is_ok_and(|decoded| decoded == &layout));
    assert!(round_trip.is_ok_and(|decoded| {
        decoded
            .zones
            .first()
            .is_some_and(|zone| zone.event_mask.contains(PointerEventMask::SUPPORTED))
    }));
}
