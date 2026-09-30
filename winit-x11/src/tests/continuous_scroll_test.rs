use super::*;

#[test]
fn xi2_scroll_units_remain_continuous_at_fractional_and_whole_increments() {
    for units in [0.125, 1.0, 3.75, -0.125, -2.0] {
        assert_eq!(
            continuous_scroll_delta(ScrollOrientation::Vertical, units),
            MouseScrollDelta::ContinuousLineDelta(0.0, -units as f32),
        );
        assert_eq!(
            continuous_scroll_delta(ScrollOrientation::Horizontal, units),
            MouseScrollDelta::ContinuousLineDelta(-units as f32, 0.0),
        );
    }
}
