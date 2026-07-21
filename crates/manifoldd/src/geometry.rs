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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowSizeNegotiation {
    Requested,
    AspectPreservingMinimum,
    Minimum,
}

/// Negotiate a requested content extent against an XDG toplevel's size constraints.
///
/// The requested size wins when it is already valid. Otherwise, prefer the smallest size that
/// satisfies the minimum while retaining the requested aspect ratio. If that candidate exceeds
/// the advertised maximum or cannot be represented, fall back to the raw minimum. A zero
/// constraint is unspecified by XDG shell; configured extents still retain a one-pixel minimum.
pub fn negotiate_window_size(
    requested: (i32, i32),
    minimum: (i32, i32),
    maximum: (i32, i32),
) -> ((i32, i32), WindowSizeNegotiation) {
    let requested = (requested.0.max(1), requested.1.max(1));
    let minimum = (minimum.0.max(1), minimum.1.max(1));

    if size_satisfies_constraints(requested, minimum, maximum) {
        return (requested, WindowSizeNegotiation::Requested);
    }

    if let Some(candidate) = aspect_preserving_minimum(requested, minimum)
        && size_satisfies_constraints(candidate, minimum, maximum)
    {
        return (candidate, WindowSizeNegotiation::AspectPreservingMinimum);
    }

    (minimum, WindowSizeNegotiation::Minimum)
}

const fn size_satisfies_constraints(
    size: (i32, i32),
    minimum: (i32, i32),
    maximum: (i32, i32),
) -> bool {
    size.0 >= minimum.0
        && size.1 >= minimum.1
        && (maximum.0 == 0 || size.0 <= maximum.0)
        && (maximum.1 == 0 || size.1 <= maximum.1)
}

fn aspect_preserving_minimum(requested: (i32, i32), minimum: (i32, i32)) -> Option<(i32, i32)> {
    let (requested_width, requested_height) = (i64::from(requested.0), i64::from(requested.1));
    let (minimum_width, minimum_height) = (i64::from(minimum.0), i64::from(minimum.1));

    let candidate = if minimum_width * requested_height >= minimum_height * requested_width {
        (
            minimum_width,
            div_ceil(minimum_width * requested_height, requested_width),
        )
    } else {
        (
            div_ceil(minimum_height * requested_width, requested_height),
            minimum_height,
        )
    };

    Some((
        i32::try_from(candidate.0).ok()?,
        i32::try_from(candidate.1).ok()?,
    ))
}

const fn div_ceil(numerator: i64, denominator: i64) -> i64 {
    numerator / denominator + if numerator % denominator == 0 { 0 } else { 1 }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_requested_window_size_has_priority() {
        assert_eq!(
            negotiate_window_size((960, 540), (800, 450), (1_200, 700)),
            ((960, 540), WindowSizeNegotiation::Requested)
        );
    }

    #[test]
    fn undersized_window_request_scales_to_minimum_at_requested_aspect() {
        assert_eq!(
            negotiate_window_size((640, 360), (800, 600), (0, 0)),
            ((1_067, 600), WindowSizeNegotiation::AspectPreservingMinimum)
        );
        assert_eq!(
            negotiate_window_size((600, 900), (800, 600), (0, 0)),
            ((800, 1_200), WindowSizeNegotiation::AspectPreservingMinimum)
        );
    }

    #[test]
    fn minimum_wins_when_aspect_candidate_exceeds_maximum() {
        assert_eq!(
            negotiate_window_size((640, 360), (800, 600), (1_000, 700)),
            ((800, 600), WindowSizeNegotiation::Minimum)
        );
    }

    #[test]
    fn unspecified_constraints_retain_positive_configure_extents() {
        assert_eq!(
            negotiate_window_size((0, -4), (0, 0), (0, 0)),
            ((1, 1), WindowSizeNegotiation::Requested)
        );
    }

    #[test]
    fn unrepresentable_aspect_candidate_falls_back_to_minimum() {
        assert_eq!(
            negotiate_window_size((1, i32::MAX), (i32::MAX, 1), (0, 0)),
            ((i32::MAX, 1), WindowSizeNegotiation::Minimum)
        );
    }
}
