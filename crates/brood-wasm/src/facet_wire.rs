//! Host allocation ownership for the versioned facet wire protocol.

use anyhow::Result;
use brood_protocol::{
    facet_codec::{self, DecodeLimits},
    facets::{InputRequest, InputResponse, RenderRequest, RenderResponse},
};

use crate::snapshot_budget::{BudgetedBytes, BudgetedValue, ByteBudget};

pub fn render_request(
    request: &RenderRequest,
    budget: &ByteBudget,
    max_bytes: usize,
) -> Result<BudgetedBytes> {
    Ok(BudgetedBytes::capture_bounded(budget, max_bytes, || {
        facet_codec::encode_render_request_owned(request, max_bytes)
    })?)
}

pub fn input_request(
    request: &InputRequest,
    budget: &ByteBudget,
    max_bytes: usize,
) -> Result<BudgetedBytes> {
    Ok(BudgetedBytes::capture_bounded(budget, max_bytes, || {
        facet_codec::encode_input_request_owned(request, max_bytes)
    })?)
}

pub fn render_response(
    bytes: &BudgetedBytes,
    budget: &ByteBudget,
    limits: DecodeLimits,
) -> Result<BudgetedValue<RenderResponse>> {
    let bytes = bytes.bytes()?;
    Ok(BudgetedValue::capture(
        budget,
        limits.max_allocated_bytes,
        || facet_codec::decode_render_response(bytes, limits),
    )?)
}

pub fn input_response(
    bytes: &BudgetedBytes,
    budget: &ByteBudget,
    limits: DecodeLimits,
) -> Result<BudgetedValue<InputResponse>> {
    let bytes = bytes.bytes()?;
    Ok(BudgetedValue::capture(
        budget,
        limits.max_allocated_bytes,
        || facet_codec::decode_input_response(bytes, limits),
    )?)
}
