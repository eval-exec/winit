//! Native queue delivery, including packets read by another connection user.
#![cfg(all(target_os = "linux", feature = "x11"))]

use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::platform::x11::EventLoopBuilderExtX11;
use winit::window::{Window, WindowAttributes, WindowId};

#[test]
#[ignore = "requires a bare X11 display (e.g. Xvfb) and xdotool"]
fn resize_delivery_survives_concurrent_geometry_queries() {
    let mut builder = EventLoop::builder();
    builder.with_x11().with_any_thread(true);
    let event_loop = builder.build().unwrap();
    let (window_tx, window_rx) = mpsc::channel::<Arc<dyn Window>>();
    let (resize_tx, resize_rx) = mpsc::channel();
    let (button_tx, button_rx) = mpsc::channel();
    let finished = Arc::new(AtomicBool::new(false));
    let shutdown = DriverShutdown { finished: finished.clone(), proxy: event_loop.create_proxy() };
    let driver = thread::spawn(move || {
        // Even a failed driver must let the native event loop exit.
        let _shutdown = shutdown;
        let window = window_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        thread::sleep(Duration::from_millis(100));

        thread::scope(|scope| {
            let (query_tx, query_rx) = mpsc::channel::<PhysicalSize<u32>>();
            let (observed_tx, observed_rx) = mpsc::channel();
            let query_window = window.clone();
            scope.spawn(move || {
                while let Ok(expected) = query_rx.recv() {
                    let deadline = Instant::now() + Duration::from_secs(2);
                    while query_window.surface_size() != expected {
                        assert!(Instant::now() < deadline, "native resize was not applied");
                        thread::sleep(Duration::from_micros(50));
                    }
                    let _ = observed_tx.send(());
                    // Stop making roundtrips after observing the size. More
                    // replies could otherwise wake the loop and mask the bug.
                }
            });

            for step in 0..64 {
                let expected = PhysicalSize::new(700 + step, 500 + step);
                query_tx.send(expected).unwrap();
                let status = Command::new("xdotool")
                    .args([
                        "windowsize",
                        "--sync",
                        &window.id().into_raw().to_string(),
                        &expected.width.to_string(),
                        &expected.height.to_string(),
                    ])
                    .status()
                    .expect("run xdotool");
                assert!(status.success(), "external resize failed");
                observed_rx.recv_timeout(Duration::from_secs(3)).unwrap();

                let deadline = Instant::now() + Duration::from_secs(1);
                loop {
                    let size = resize_rx
                        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                        .unwrap_or_else(|error| {
                            panic!("resize {step}: missing {expected:?} callback: {error}")
                        });
                    if size == expected {
                        break;
                    }
                }
            }
        });

        // XI2 buttons carry generic-event cookies. Queue readiness must not
        // consume their payload while waking the application thread.
        for step in 0..8 {
            let status = Command::new("xdotool")
                .args([
                    "mousemove",
                    "--sync",
                    "--window",
                    &window.id().into_raw().to_string(),
                    &(100 + step).to_string(),
                    "100",
                    "click",
                    "1",
                ])
                .status()
                .expect("send native pointer input");
            assert!(status.success());
            assert_eq!(
                button_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
                ElementState::Pressed
            );
            assert_eq!(
                button_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
                ElementState::Released
            );
        }

        // Let the queue go idle before exiting through the proxy. The reader
        // must shut down even though no further native event will arrive.
        thread::sleep(Duration::from_millis(100));
    });

    event_loop
        .run_app(ResizeApp { window: None, window_tx, resize_tx, button_tx, finished })
        .unwrap();
    driver.join().expect("resize driver failed");
}

#[test]
#[ignore = "requires a bare X11 display (e.g. Xvfb)"]
fn an_unrun_event_loop_can_be_dropped_without_native_input() {
    let mut builder = EventLoop::builder();
    builder.with_x11().with_any_thread(true);
    let event_loop = builder.build().unwrap();
    thread::sleep(Duration::from_millis(100));
    drop(event_loop);
}

struct DriverShutdown {
    finished: Arc<AtomicBool>,
    proxy: EventLoopProxy,
}

impl Drop for DriverShutdown {
    fn drop(&mut self) {
        self.finished.store(true, Ordering::Release);
        self.proxy.wake_up();
    }
}

struct ResizeApp {
    window: Option<Arc<dyn Window>>,
    window_tx: mpsc::Sender<Arc<dyn Window>>,
    resize_tx: mpsc::Sender<PhysicalSize<u32>>,
    button_tx: mpsc::Sender<ElementState>,
    finished: Arc<AtomicBool>,
}

impl ApplicationHandler for ResizeApp {
    fn can_create_surfaces(&mut self, event_loop: &dyn ActiveEventLoop) {
        event_loop.set_control_flow(ControlFlow::Wait);
        let window: Arc<dyn Window> = event_loop
            .create_window(
                WindowAttributes::default()
                    .with_title("X11 event delivery regression")
                    .with_surface_size(PhysicalSize::new(640, 480)),
            )
            .unwrap()
            .into();
        self.window_tx.send(window.clone()).unwrap();
        self.window = Some(window);
    }

    fn window_event(&mut self, _: &dyn ActiveEventLoop, _: WindowId, event: WindowEvent) {
        if let WindowEvent::SurfaceResized(size) = event {
            let _ = self.resize_tx.send(size);
        }
        if let WindowEvent::PointerButton { state, .. } = event {
            let _ = self.button_tx.send(state);
        }
    }

    fn proxy_wake_up(&mut self, event_loop: &dyn ActiveEventLoop) {
        if self.finished.load(Ordering::Acquire) {
            event_loop.exit();
        }
    }
}
