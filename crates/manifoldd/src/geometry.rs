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

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_close(left: f64, right: f64) {
        assert!((left - right).abs() <= f64::EPSILON);
    }

    #[test]
    fn host_extent_saturates_without_signed_wrap() {
        assert_eq!(host_extent_to_layout(0), 0);
        assert_eq!(host_extent_to_layout(17), 17);
        assert_eq!(host_extent_to_layout(u32::MAX), i32::MAX);
    }

    #[test]
    fn capture_extent_rejects_empty_and_negative_buffers() {
        assert_eq!(capture_extent("width", 7), Ok(7));
        assert!(capture_extent("width", 0).is_err());
        assert!(capture_extent("height", -1).is_err());
    }

    #[test]
    fn rounded_coordinates_define_fractional_and_non_finite_policy() {
        assert_eq!(rounded_layout_coordinate(1.49), 1);
        assert_eq!(rounded_layout_coordinate(1.5), 2);
        assert_eq!(rounded_layout_coordinate(-1.5), -2);
        assert_eq!(rounded_layout_coordinate(f64::NAN), 0);
        assert_eq!(rounded_layout_coordinate(f64::INFINITY), i32::MAX);
        assert_eq!(rounded_layout_coordinate(f64::NEG_INFINITY), i32::MIN);
        assert_eq!(rounded_layout_coordinate(f64::MAX), i32::MAX);
    }

    #[test]
    fn rounded_extents_preserve_empty_bounds_and_saturate_overflow() {
        assert_eq!(rounded_layout_extent(0.49, 100), 1);
        assert_eq!(rounded_layout_extent(1.5, 100), 2);
        assert_eq!(rounded_layout_extent(f64::NAN, 100), 0);
        assert_eq!(rounded_layout_extent(-1.0, 100), 0);
        assert_eq!(rounded_layout_extent(f64::INFINITY, 100), i32::MAX);
        assert_eq!(rounded_layout_extent(50.0, 0), 0);
        assert_eq!(rounded_layout_extent(50.0, -1), 0);
    }

    #[test]
    fn pointer_conversion_defines_invalid_and_empty_boundaries() {
        assert_close(physical_to_logical_coordinate(640.0, 1280, 2.0), 320.0);
        assert_close(physical_to_logical_coordinate(-1.0, 1280, 2.0), 0.0);
        assert_close(physical_to_logical_coordinate(f64::NAN, 1280, 2.0), 0.0);
        assert_close(
            physical_to_logical_coordinate(f64::INFINITY, 1280, 2.0),
            640.0,
        );
        assert_close(physical_to_logical_coordinate(10.0, 0, 2.0), 0.0);
        assert_close(physical_to_logical_coordinate(10.0, 100, 0.0), 10.0);
        assert_close(physical_to_logical_coordinate(10.0, 100, -2.0), 10.0);
        assert_close(physical_to_logical_coordinate(10.0, 100, f64::NAN), 10.0);
        assert_close(
            physical_to_logical_coordinate(10.0, 100, f64::INFINITY),
            10.0,
        );
        assert_close(
            physical_to_logical_coordinate(f64::MAX, u32::MAX, f64::MIN_POSITIVE),
            f64::MAX,
        );
    }

    #[test]
    fn axis_conversion_truncates_saturates_and_rejects_non_finite_values() {
        assert_eq!(truncating_axis_v120(119.9), Some(119));
        assert_eq!(truncating_axis_v120(-119.9), Some(-119));
        assert_eq!(truncating_axis_v120(f64::MAX), Some(i32::MAX));
        assert_eq!(truncating_axis_v120(-f64::MAX), Some(i32::MIN));
        assert_eq!(truncating_axis_v120(f64::NAN), None);
        assert_eq!(truncating_axis_v120(f64::INFINITY), None);
        assert_eq!(finite_axis_value(1.0), Some(1.0));
        assert_eq!(finite_axis_value(f64::NEG_INFINITY), None);
    }
}
