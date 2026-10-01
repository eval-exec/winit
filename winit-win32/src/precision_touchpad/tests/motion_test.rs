use super::*;

#[test]
fn fractions_both_axes_and_phases_are_preserved() {
    let mut state = Motion::default();
    assert_eq!(state.update([0.0, 0.0]), None);
    assert_eq!(
        state.update([0.125, -0.25]),
        Some(Packet { delta: [0.125, -0.25], phase: TouchPhase::Started })
    );
    assert_eq!(
        state.update([-0.125, -0.375]),
        Some(Packet { delta: [-0.25, -0.125], phase: TouchPhase::Moved })
    );
    assert_eq!(state.update([-0.125, -0.375]), None);
    assert_eq!(
        state.finish(TouchPhase::Ended),
        (Some(Packet { delta: [0.0; 2], phase: TouchPhase::Ended }), true)
    );
    assert_eq!(state.update([0.125, 0.0]).unwrap().phase, TouchPhase::Started);
}

#[test]
fn long_reversals_conserve_physical_distance() {
    let mut state = Motion::default();
    let mut sum = [0.0; 2];
    for i in (1..=10000).chain((0..10000).rev()) {
        let p = state.update([i as f32 * 0.125, -(i as f32) * 0.25]).unwrap();
        for a in 0..2 {
            sum[a] += p.delta[a];
        }
    }
    assert_eq!(sum, [0.0; 2]);
    assert!(state.finish(TouchPhase::Ended).0.is_some());
    assert!(!state.finish(TouchPhase::Ended).1);
}

#[test]
fn cancellation_and_invalid_transforms_cannot_leak_into_next_gesture() {
    let mut state = Motion::default();
    state.update([7.0, -9.0]);
    assert_eq!(state.update([f32::NAN, 0.0]).unwrap().phase, TouchPhase::Cancelled);
    assert_eq!(state.update([0.125, 0.25]).unwrap().delta, [0.125, 0.25]);
    assert_eq!(state.finish(TouchPhase::Cancelled).0.unwrap().phase, TouchPhase::Cancelled);
    assert_eq!(state.update([0.25, 0.125]).unwrap().phase, TouchPhase::Started);
    assert_eq!(state.update([f32::INFINITY, 0.0]).unwrap().phase, TouchPhase::Cancelled);
}
