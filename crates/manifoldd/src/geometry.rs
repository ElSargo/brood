//! Checked conversions at the boundaries between normalized, logical, and host geometry.
//!
//! Layout is signed because coordinates can extend around an origin. Host windows and image
//! buffers are unsigned. Floating-point input may be supplied by devices or plugins. Keeping
//! those policies here makes each lossy conversion visible and independently testable.

use num_traits::ToPrimitive;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum GeometryConversionError {
    #[error("{axis} capture dimension must be positive, got {value}")]
    NonPositiveCaptureDimension { axis: &'static str, value: i32 },
}

/// Convert an unsigned host extent to signed layout pixels.
///
/// Host sizes larger than the signed layout domain saturate at `i32::MAX`; they must never wrap
/// to a negative compositor size.
pub fn host_extent_to_layout(value: u32) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}

/// Convert a signed renderer extent to a positive image-buffer dimension.
///
/// Capturing an empty or negative framebuffer is an error rather than an implicit unsigned wrap.
pub fn capture_extent(axis: &'static str, value: i32) -> Result<u32, GeometryConversionError> {
    let value = u32::try_from(value).map_err(|_conversion_error| {
        GeometryConversionError::NonPositiveCaptureDimension { axis, value }
    })?;
    if value == 0 {
        return Err(GeometryConversionError::NonPositiveCaptureDimension { axis, value: 0 });
    }
    Ok(value)
}

/// Round normalized layout math to the nearest signed pixel and saturate overflow.
///
/// `NaN` has no meaningful direction and maps to the origin. Infinities and finite values outside
/// the signed layout domain saturate toward their sign.
pub fn rounded_layout_coordinate(value: f64) -> i32 {
    if value.is_nan() {
        return 0;
    }
    value.round().to_i32().unwrap_or_else(|| {
        if value.is_sign_negative() {
            i32::MIN
        } else {
            i32::MAX
        }
    })
}

/// Round a normalized extent to pixels.
///
/// Positive presentation bounds retain a one-pixel minimum. Empty bounds stay empty. Invalid or
/// negative calculated extents collapse to zero, and positive overflow saturates at `i32::MAX`.
pub fn rounded_layout_extent(value: f64, bounds: i32) -> i32 {
    if bounds <= 0 || value.is_nan() || value <= 0.0 {
        return 0;
    }
    value
        .round()
        .to_i32()
        .unwrap_or(i32::MAX)
        .clamp(1, i32::MAX)
}

/// Convert a physical pointer coordinate to logical space and clamp it to the presentation.
///
/// A zero-sized presentation collapses to the origin. `NaN` and negative infinity map to the
/// origin; positive infinity maps to the far edge.
pub fn physical_to_logical_coordinate(
    position: f64,
    physical_extent: u32,
    scale_factor: f64,
) -> f64 {
    if physical_extent == 0 {
        return 0.0;
    }
    let scale_factor = if scale_factor.is_finite() && scale_factor > 0.0 {
        scale_factor
    } else {
        1.0
    };
    let logical_extent = (f64::from(physical_extent) / scale_factor).min(f64::MAX);
    let logical_position = position / scale_factor;
    if logical_position.is_nan() || logical_position == f64::NEG_INFINITY {
        0.0
    } else if logical_position == f64::INFINITY {
        logical_extent
    } else {
        logical_position.clamp(0.0, logical_extent)
    }
}

/// Truncate a wheel delta toward zero for Wayland's v120 protocol field.
///
/// Non-finite device values are ignored. Finite overflow saturates so it cannot wrap direction.
pub fn truncating_axis_v120(value: f64) -> Option<i32> {
    if !value.is_finite() {
        return None;
    }
    Some(value.to_i32().unwrap_or_else(|| {
        if value.is_sign_negative() {
            i32::MIN
        } else {
            i32::MAX
        }
    }))
}

/// Accept only finite continuous axis values.
pub const fn finite_axis_value(value: f64) -> Option<f64> {
    if value.is_finite() { Some(value) } else { None }
}
