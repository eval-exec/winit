//! Opt-in compositor regression for popup replacement during event dispatch.
#![cfg(all(target_os = "linux", feature = "wayland"))]

use std::collections::HashSet;
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::{Duration, Instant};

use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, OwnedDisplayHandle};
use winit::platform::wayland::EventLoopBuilderExtWayland;
use winit::raw_window_handle::HasWindowHandle;
use winit::window::{Window, WindowAttributes, WindowId, WindowType};

#[test]
#[ignore = "requires live Wayland with fractional scaling; enable softbuffer/wayland"]
fn replacing_popup_in_resize_callback_does_not_dispatch_to_destroyed_windows() {
    let mut builder = EventLoop::builder();
    builder.with_wayland().with_any_thread(true);
    let app = Replacement {
        popup: None,
        surface: None,
        parent: None,
        deadline: Instant::now() + Duration::from_secs(4),
        destroyed: HashSet::new(),
        replacements: 0,
    };
    builder.build().unwrap().run_app(app).unwrap();
}

struct Replacement {
    // Children and their buffers precede their retained parent in drop order.
    popup: Option<softbuffer::Surface<OwnedDisplayHandle, Arc<dyn Window>>>,
    surface: Option<softbuffer::Surface<OwnedDisplayHandle, Arc<dyn Window>>>,
    parent: Option<Arc<dyn Window>>,
    deadline: Instant,
    destroyed: HashSet<WindowId>,
    replacements: usize,
}

impl Replacement {
    fn replace(&mut self, event_loop: &dyn ActiveEventLoop) {
        self.popup.take();
        let parent = self.parent.as_ref().unwrap();
        let attrs = WindowAttributes::default()
            .with_window_type(WindowType::Popup)
            .with_surface_size(LogicalSize::new(160, 80));
        // SAFETY: parent outlives every popup, including during app teardown.
        let attrs =
            unsafe { attrs.with_parent_window(Some(parent.window_handle().unwrap().as_raw())) };
        let popup: Arc<dyn Window> = Arc::from(event_loop.create_window(attrs).unwrap());
        // WindowId may be reused, but no event for a retired incarnation may
        // arrive until a new window with that ID has actually been created.
        self.destroyed.remove(&popup.id());
        let context = softbuffer::Context::new(event_loop.owned_display_handle()).unwrap();
        let mut surface = softbuffer::Surface::new(&context, popup.clone()).unwrap();
        let size = popup.surface_size();
        surface
            .resize(
                NonZeroU32::new(size.width.max(1)).unwrap(),
                NonZeroU32::new(size.height.max(1)).unwrap(),
            )
            .unwrap();
        let mut buffer = surface.buffer_mut().unwrap();
        buffer.fill(0x00405060);
        popup.pre_present_notify();
        buffer.present().unwrap();
        self.popup = Some(surface);
        self.replacements += 1;
    }
}

impl ApplicationHandler for Replacement {
    fn can_create_surfaces(&mut self, event_loop: &dyn ActiveEventLoop) {
        let parent: Arc<dyn Window> = Arc::from(
            event_loop
                .create_window(
                    WindowAttributes::default()
                        .with_title("Wayland popup lifetime regression")
                        .with_surface_size(LogicalSize::new(240, 120)),
                )
                .unwrap(),
        );
        let context = softbuffer::Context::new(event_loop.owned_display_handle()).unwrap();
        self.surface = Some(softbuffer::Surface::new(&context, parent.clone()).unwrap());
        parent.request_redraw();
        self.parent = Some(parent);
        event_loop.set_control_flow(ControlFlow::Poll);
    }

    fn window_event(&mut self, event_loop: &dyn ActiveEventLoop, id: WindowId, event: WindowEvent) {
        assert!(!self.destroyed.contains(&id), "event after Destroyed: {event:?}");
        if matches!(event, WindowEvent::Destroyed) {
            self.destroyed.insert(id);
        }
        if self.parent.as_ref().is_some_and(|parent| parent.id() == id) {
            if matches!(event, WindowEvent::RedrawRequested) {
                let parent = self.parent.as_ref().unwrap();
                let size = parent.surface_size();
                let surface = self.surface.as_mut().unwrap();
                surface
                    .resize(
                        NonZeroU32::new(size.width.max(1)).unwrap(),
                        NonZeroU32::new(size.height.max(1)).unwrap(),
                    )
                    .unwrap();
                let mut buffer = surface.buffer_mut().unwrap();
                buffer.fill(0x00203040);
                parent.pre_present_notify();
                buffer.present().unwrap();
                if self.popup.is_none() {
                    self.replace(event_loop);
                }
            }
        } else if self.popup.as_ref().is_some_and(|popup| popup.window().id() == id)
            && matches!(event, WindowEvent::SurfaceResized(_))
            && Instant::now() < self.deadline
        {
            // Creation roundtrips while the old native popup awaits teardown.
            self.replace(event_loop);
        }
    }

    fn about_to_wait(&mut self, event_loop: &dyn ActiveEventLoop) {
        if Instant::now() >= self.deadline {
            assert!(self.replacements >= 20, "resize-callback replacement was not exercised");
            self.popup.take();
            event_loop.exit();
        }
    }
}
