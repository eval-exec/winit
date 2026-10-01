//! Physical transform differencing; no DPI conversion or integer rounding here.
use winit_core::event::TouchPhase;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Packet {
    pub(crate) delta: [f64; 2],
    pub(crate) phase: TouchPhase,
}

#[derive(Default)]
pub(super) struct Motion {
    translation: [f32; 2],
    started: bool,
}

impl Motion {
    pub(super) fn update(&mut self, translation: [f32; 2]) -> Option<Packet> {
        if !translation.iter().all(|v| v.is_finite()) {
            return self.finish(TouchPhase::Cancelled).0;
        }
        let delta = std::array::from_fn(|i| translation[i] as f64 - self.translation[i] as f64);
        self.translation = translation;
        if delta == [0.0; 2] {
            return None;
        }
        let phase = if self.started { TouchPhase::Moved } else { TouchPhase::Started };
        self.started = true;
        Some(Packet { delta, phase })
    }

    /// Return the terminal packet and whether the native transform needs rebasing.
    pub(super) fn finish(&mut self, phase: TouchPhase) -> (Option<Packet>, bool) {
        let changed = self.translation != [0.0; 2];
        let packet = self.started.then_some(Packet { delta: [0.0; 2], phase });
        *self = Self::default();
        (packet, changed)
    }
}

#[cfg(test)]
#[path = "tests/motion_test.rs"]
mod tests;
