//! Checked conversions at the boundaries between normalized, logical, and host geometry.
//!
//! Layout is signed because coordinates can extend around an origin. Host windows and image
//! buffers are unsigned. Floating-point input may be supplied by devices or plugins. Keeping
//! those policies here makes each lossy conversion visible and independently testable.

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

/// Accept only finite continuous axis values.
pub const fn finite_axis_value(value: f64) -> Option<f64> {
    if value.is_finite() { Some(value) } else { None }
}
