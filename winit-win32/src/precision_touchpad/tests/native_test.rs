use super::*;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, WS_OVERLAPPEDWINDOW,
};

struct NativeWindow(*mut std::ffi::c_void);
impl NativeWindow {
    fn new() -> Self {
        let window = unsafe {
            CreateWindowExW(
                0,
                windows_core::w!("STATIC").as_ptr(),
                windows_core::w!("Precision touchpad contract").as_ptr(),
                WS_OVERLAPPEDWINDOW,
                0,
                0,
                640,
                480,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null(),
            )
        };
        assert!(!window.is_null(), "failed to create native HWND");
        Self(window)
    }
}
impl Drop for NativeWindow {
    fn drop(&mut self) {
        unsafe {
            DestroyWindow(self.0);
        }
    }
}

#[test]
fn native_viewport_initializes_resizes_and_drops_its_handler() {
    let window = NativeWindow::new();
    let bridge =
        PrecisionTouchpad::new(window.0).expect("native DirectManipulation initialization");
    assert!(!bridge.needs_update(), "idle viewport must not schedule polling");
    bridge.resize(1280, 720);
    let rect = unsafe { bridge.viewport.GetViewportRect() }.unwrap();
    assert_eq!((rect.right, rect.bottom), (1280, 720));
    assert!(!bridge.needs_update());
    let shared = bridge.shared.clone();
    assert!(Rc::strong_count(&shared) >= 3, "native handler was not retained");
    drop(bridge);
    assert_eq!(Rc::strong_count(&shared), 1, "native handler leaked on teardown");
}

#[test]
fn invalid_contact_does_not_claim_or_disable_wheel_fallback() {
    let window = NativeWindow::new();
    let bridge = PrecisionTouchpad::new(window.0).unwrap();
    assert!(!bridge.claim_contact(u32::MAX));
    assert!(!bridge.needs_update());
    assert!(bridge.poll().is_empty());
}

#[test]
fn cancellation_delivers_terminal_phase_and_resets_native_transform() {
    let window = NativeWindow::new();
    let bridge = PrecisionTouchpad::new(window.0).unwrap();
    // This exercises real viewport rebasing/teardown; it does not simulate a
    // physical precision-touchpad contact or prove OS gesture recognition.
    bridge.shared.contact.set(true);
    bridge.shared.motion.borrow_mut().update([0.125, -0.25]);
    bridge.cancel();
    let packets = bridge.poll();
    assert!(!bridge.needs_update(), "identity reset left idle polling enabled");
    assert_eq!(packets.len(), 1);
    assert_eq!(packets[0].phase, TouchPhase::Cancelled);
    assert_eq!(packets[0].delta, [0.0; 2]);
    assert_eq!(bridge.shared.motion.borrow_mut().finish(TouchPhase::Ended).0, None);
}

#[test]
fn native_suspension_cancels_and_rebases_the_next_gesture_origin() {
    let window = NativeWindow::new();
    let bridge = PrecisionTouchpad::new(window.0).unwrap();
    bridge.shared.contact.set(true);
    bridge.shared.motion.borrow_mut().update([0.125, -0.25]);
    // Synthetic status delivered through the real COM callback interface.
    // This does not inject or prove physical touchpad delivery.
    let handler: IDirectManipulationViewportEventHandler =
        Handler { shared: bridge.shared.clone() }.into();
    unsafe {
        handler
            .OnViewportStatusChanged(
                &bridge.viewport,
                DIRECTMANIPULATION_SUSPENDED,
                DIRECTMANIPULATION_RUNNING,
            )
            .unwrap();
    }
    assert!(!bridge.shared.contact.get());
    let packets = bridge.poll();
    assert_eq!(packets.len(), 1);
    assert_eq!(packets[0].phase, TouchPhase::Cancelled);
    assert!(!bridge.needs_update());
    assert_eq!(
        bridge.shared.motion.borrow_mut().update([0.125, 0.25]).unwrap().delta,
        [0.125, 0.25]
    );
}
