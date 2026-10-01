//! Precision-touchpad gesture recognition on the HWND-owning thread.
//! DirectManipulation owns consumed touchpad contacts; independent wheel messages
//! keep their existing path. All queued deltas are physical, fractional pixels.
mod motion;

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::LazyLock;
use windows::Win32::Foundation::{E_POINTER, HWND, RECT};
use windows::Win32::Graphics::DirectManipulation::*;
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
    CoUninitialize,
};
use windows_core::{Ref, Result};
use winit_core::event::TouchPhase;

use motion::Motion;
pub(crate) use motion::Packet;

// Dynamic lookup retains the backend's Windows 7 startup fallback.
type GetPointerType = unsafe extern "system" fn(u32, *mut i32) -> i32;
static GET_POINTER_TYPE: LazyLock<Option<GetPointerType>> = LazyLock::new(|| {
    crate::util::get_function_impl("user32.dll\0", "GetPointerType\0")
        .map(|p| unsafe { std::mem::transmute::<*const std::ffi::c_void, GetPointerType>(p) })
});

struct Apartment;
impl Drop for Apartment {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

#[derive(Default)]
struct Shared {
    motion: RefCell<Motion>,
    packets: RefCell<Vec<Packet>>,
    contact: Cell<bool>,
    resetting: Cell<bool>,
    bounds: Cell<[i32; 2]>,
    failed: Cell<bool>,
}
impl Shared {
    fn finish(&self, phase: TouchPhase) -> bool {
        let (packet, changed) = self.motion.borrow_mut().finish(phase);
        if let Some(packet) = packet {
            self.packets.borrow_mut().push(packet);
        }
        self.contact.set(false);
        changed
    }
}

pub(crate) struct PrecisionTouchpad {
    manager: IDirectManipulationManager,
    updates: IDirectManipulationUpdateManager,
    viewport: IDirectManipulationViewport,
    cookie: Option<u32>,
    window: HWND,
    shared: Rc<Shared>,
    // Declared last so every COM interface is released before uninitialization.
    _apartment: Apartment,
}

impl PrecisionTouchpad {
    pub(crate) fn new(window: *mut std::ffi::c_void) -> Result<Self> {
        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.ok()?;
        let apartment = Apartment;
        let window = HWND(window);
        let manager: IDirectManipulationManager =
            unsafe { CoCreateInstance(&DirectManipulationManager, None, CLSCTX_INPROC_SERVER)? };
        let updates = unsafe { manager.GetUpdateManager()? };
        let viewport = unsafe { manager.CreateViewport(None, window)? };
        let shared = Rc::new(Shared::default());
        shared.bounds.set([1000, 1000]);
        // Own partial initialization before any operation that can fail.
        let bridge = Self {
            manager,
            updates,
            viewport,
            cookie: None,
            window,
            shared,
            _apartment: apartment,
        };
        unsafe {
            bridge.viewport.ActivateConfiguration(
                DIRECTMANIPULATION_CONFIGURATION_INTERACTION
                    | DIRECTMANIPULATION_CONFIGURATION_TRANSLATION_X
                    | DIRECTMANIPULATION_CONFIGURATION_TRANSLATION_Y,
            )?;
            bridge.viewport.SetViewportOptions(
                DIRECTMANIPULATION_VIEWPORT_OPTIONS_MANUALUPDATE
                    | DIRECTMANIPULATION_VIEWPORT_OPTIONS_DISABLEPIXELSNAPPING,
            )?;
            bridge.viewport.SetViewportRect(&RECT {
                left: 0,
                top: 0,
                right: 1000,
                bottom: 1000,
            })?;
            bridge.manager.Activate(window)?;
            bridge.viewport.Enable()?;
        }
        let handler: IDirectManipulationViewportEventHandler =
            Handler { shared: bridge.shared.clone() }.into();
        let cookie = unsafe { bridge.viewport.AddEventHandler(Some(window), &handler)? };
        let mut bridge = bridge;
        bridge.cookie = Some(cookie);
        Ok(bridge)
    }

    pub(crate) fn claim_contact(&self, pointer_id: u32) -> bool {
        if self.shared.failed.get() {
            return false;
        }
        let Some(get_type) = *GET_POINTER_TYPE else {
            return false;
        };
        let mut kind = 0;
        if unsafe { get_type(pointer_id, &mut kind) } == 0
            || kind != windows_sys::Win32::UI::WindowsAndMessaging::PT_TOUCHPAD
        {
            return false;
        }
        // SetContact can synchronously enter the handler. No state borrow or
        // window mutex is held across any COM call.
        let was_contact = self.shared.contact.replace(true);
        if unsafe { self.viewport.SetContact(pointer_id) }.is_err() {
            self.shared.contact.set(was_contact);
            return false;
        }
        true
    }

    pub(crate) fn needs_update(&self) -> bool {
        !self.shared.failed.get() && (self.shared.contact.get() || self.shared.resetting.get())
    }

    pub(crate) fn poll(&self) -> Vec<Packet> {
        if self.needs_update() {
            if let Err(error) = unsafe { self.updates.Update(None) } {
                tracing::warn!(%error, "precision touchpad update failed; restoring wheel fallback");
                self.shared.failed.set(true);
                self.shared.finish(TouchPhase::Cancelled);
                self.shared.resetting.set(false);
                unsafe {
                    let _ = self.viewport.ReleaseAllContacts();
                    let _ = self.viewport.Stop();
                }
            }
        }
        // Rebasing an already-identity viewport need not emit another status
        // callback. End its update deadline explicitly instead of polling forever.
        if self.shared.resetting.get()
            && unsafe { self.viewport.GetStatus() }.ok() == Some(DIRECTMANIPULATION_READY)
        {
            self.shared.resetting.set(false);
        }
        std::mem::take(&mut *self.shared.packets.borrow_mut())
    }

    pub(crate) fn cancel(&self) {
        let changed = self.shared.finish(TouchPhase::Cancelled);
        unsafe {
            let _ = self.viewport.ReleaseAllContacts();
            let _ = self.viewport.Stop();
        }
        if changed && !self.shared.resetting.get() {
            let _ = self.rebase();
        }
    }

    pub(crate) fn resize(&self, width: u32, height: u32) {
        self.cancel();
        let size =
            [width.max(1).min(i32::MAX as u32) as i32, height.max(1).min(i32::MAX as u32) as i32];
        self.shared.bounds.set(size);
        if let Err(error) = unsafe {
            self.viewport.SetViewportRect(&RECT {
                left: 0,
                top: 0,
                right: size[0],
                bottom: size[1],
            })
        } {
            tracing::warn!(%error, "precision touchpad viewport resize failed");
            self.shared.failed.set(true);
        }
    }

    fn rebase(&self) -> Result<()> {
        rebase_viewport(&self.shared, &self.viewport)
    }
}

impl Drop for PrecisionTouchpad {
    fn drop(&mut self) {
        unsafe {
            if let Some(cookie) = self.cookie {
                let _ = self.viewport.RemoveEventHandler(cookie);
            }
            let _ = self.viewport.ReleaseAllContacts();
            let _ = self.viewport.Stop();
            let _ = self.viewport.Abandon();
            let _ = self.manager.Deactivate(self.window);
        }
    }
}

// A failed reset cannot safely establish the origin of a later gesture.
// Restore wheel fallback rather than expose the previous native translation.
fn rebase_viewport(shared: &Shared, viewport: &IDirectManipulationViewport) -> Result<()> {
    shared.resetting.set(true);
    let [w, h] = shared.bounds.get();
    if let Err(error) = unsafe { viewport.ZoomToRect(0.0, 0.0, w as f32, h as f32, false) } {
        shared.resetting.set(false);
        shared.failed.set(true);
        tracing::warn!(%error, "precision touchpad reset failed; restoring wheel fallback");
        return Err(error);
    }
    Ok(())
}

#[windows_core::implement(IDirectManipulationViewportEventHandler, Agile = false)]
struct Handler {
    shared: Rc<Shared>,
}
impl IDirectManipulationViewportEventHandler_Impl for Handler_Impl {
    fn OnViewportStatusChanged(
        &self,
        viewport: Ref<'_, IDirectManipulationViewport>,
        current: DIRECTMANIPULATION_STATUS,
        previous: DIRECTMANIPULATION_STATUS,
    ) -> Result<()> {
        if current == previous {
            return Ok(());
        }
        if current == DIRECTMANIPULATION_READY {
            // Programmatic rebasing itself has a RUNNING -> READY cycle.
            if self.shared.resetting.replace(false) {
                return Ok(());
            }
            if self.shared.finish(TouchPhase::Ended) {
                rebase_viewport(&self.shared, viewport.as_ref().ok_or(E_POINTER)?)?;
            }
        } else if current == DIRECTMANIPULATION_DISABLED || current == DIRECTMANIPULATION_SUSPENDED
        {
            let changed = self.shared.finish(TouchPhase::Cancelled);
            self.shared.resetting.set(false);
            if changed {
                rebase_viewport(&self.shared, viewport.as_ref().ok_or(E_POINTER)?)?;
            }
        }

        Ok(())
    }
    fn OnViewportUpdated(&self, _viewport: Ref<'_, IDirectManipulationViewport>) -> Result<()> {
        Ok(())
    }
    fn OnContentUpdated(
        &self,
        _viewport: Ref<'_, IDirectManipulationViewport>,
        content: Ref<'_, IDirectManipulationContent>,
    ) -> Result<()> {
        if !self.shared.contact.get() || self.shared.resetting.get() {
            return Ok(());
        }
        let content = content.as_ref().ok_or(E_POINTER)?;
        let mut transform = [0.0; 6];
        unsafe {
            content.GetContentTransform(&mut transform)?;
        }
        if let Some(packet) = self.shared.motion.borrow_mut().update([transform[4], transform[5]]) {
            self.shared.packets.borrow_mut().push(packet);
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "precision_touchpad/tests/native_test.rs"]
mod native_tests;
