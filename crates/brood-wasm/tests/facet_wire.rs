use anyhow::{Result, ensure};
use brood_protocol::{
    facet_codec::{DecodeLimits, encode_input_response},
    facets::{InputResponse, InputToken, Scope},
};
use brood_wasm::{
    facet_wire,
    snapshot_budget::{BudgetedBytes, ByteBudget, ByteBudgetLimits},
};

fn budget(bytes: usize) -> ByteBudget {
    ByteBudget::new(
        1,
        ByteBudgetLimits {
            max_live_bytes: bytes,
            max_live_allocations: 8,
            max_buffer_bytes: bytes,
        },
    )
}

fn response() -> InputResponse {
    InputResponse {
        token: InputToken {
            scope: Scope {
                client_generation: 2,
                actor_generation: 1,
                facet_generation: 3,
            },
            input_sequence: 1,
            frame: 4,
            pane: 5,
            snapshot_revision: 6,
            sequence_revision: 0,
        },
        next_sequence_state: vec![1, 2],
        events: vec![vec![3, 4]],
        requests: vec![],
    }
}

#[test]
fn decoded_graph_stays_charged_after_raw_output_is_released() -> Result<()> {
    let budget = budget(4096);
    let response = response();
    let raw = BudgetedBytes::capture_bounded(&budget, 512, || {
        let mut raw = vec![0; 512];
        let len = encode_input_response(&response, &mut raw)?;
        raw.truncate(len);
        Ok::<_, brood_protocol::facet_codec::CodecError>(raw)
    })?;
    let decoded = facet_wire::input_response(
        &raw,
        &budget,
        DecodeLimits {
            max_wire_bytes: 512,
            max_allocated_bytes: 1024,
            max_allocations: 8,
        },
    )?;
    ensure!(*decoded.value()? == response);
    drop(raw);
    ensure!(budget.retention().live_bytes == 1024);
    budget.invalidate();
    ensure!(decoded.value().is_err());
    ensure!(budget.retention().live_bytes == 1024);
    drop(decoded);
    ensure!(budget.retention().live_bytes == 0);
    Ok(())
}

#[test]
fn rejected_decode_does_not_leak_aggregate_credit() -> Result<()> {
    let budget = budget(4096);
    let raw = BudgetedBytes::copy_from_slice(&budget, b"malformed")?;
    let before = budget.retention();
    ensure!(
        facet_wire::input_response(
            &raw,
            &budget,
            DecodeLimits {
                max_wire_bytes: 512,
                max_allocated_bytes: 1024,
                max_allocations: 8
            }
        )
        .is_err()
    );
    ensure!(budget.retention() == before);
    Ok(())
}
