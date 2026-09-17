//! Readiness for the native event queue, not just its underlying socket.
//!
//! XCB reply readers (including graphics libraries using the exported display)
//! can receive events and empty the socket while our event loop sleeps. An Xlib
//! waiter participates in XCB's reader coordination; polling the fd does not.
//! A blocking peek observes readiness without removing events. A drain guard
//! pauses that reader while the application processes the queue, so filtering
//! and ownership of dispatched event cookies stay on the application thread.

use std::mem::MaybeUninit;
use std::os::raw::{c_char, c_int};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};

use calloop::ping::Ping;
use winit_core::error::EventLoopError;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    ClientMessageEvent, ConnectionExt as _, CreateWindowAux, EventMask, Window, WindowClass,
};

use crate::event_loop::X11Error;
use crate::ffi;
use crate::util::cookie::GenericEventCookie;
use crate::xdisplay::XConnection;

/// Owns the reader and its private shutdown endpoint as one lifetime.
#[derive(Debug)]
pub(crate) struct NativeEventQueue {
    reader: Option<JoinHandle<()>>,
    stop: StopWindow,
    readiness: Arc<Readiness>,
}

#[derive(Debug)]
struct Readiness {
    state: Mutex<ReaderState>,
    changed: Condvar,
}

#[derive(Debug, PartialEq, Eq)]
enum ReaderState {
    Waiting,
    Ready,
    Stopped,
}

impl NativeEventQueue {
    pub(crate) fn new(xconn: Arc<XConnection>, waker: Ping) -> Result<Self, EventLoopError> {
        let stop = StopWindow::new(xconn.clone()).map_err(|err| os_error!(err))?;
        let readiness = Arc::new(Readiness {
            state: Mutex::new(ReaderState::Waiting),
            changed: Condvar::new(),
        });
        let reader_readiness = readiness.clone();
        let reader = thread::Builder::new()
            .name("winit X11 events".into())
            .spawn(move || {
                loop {
                    let mut event = MaybeUninit::<ffi::XEvent>::uninit();
                    // SAFETY: XInitThreads precedes opening xconn. XPeekEvent
                    // releases the display lock while waiting. Unlike
                    // XIfEvent, it does not keep the predicate lock held.
                    let event = unsafe {
                        (xconn.xlib.XPeekEvent)(xconn.display, event.as_mut_ptr());
                        event.assume_init()
                    };
                    if event.get_type() == ffi::GenericEvent {
                        // A peek copies a generic cookie. Release that copy;
                        // the original event and cookie remain in Xlib's queue.
                        drop(GenericEventCookie::from_event(xconn.clone(), event));
                    }

                    let mut state = reader_readiness.state.lock().unwrap();
                    if *state == ReaderState::Stopped {
                        break;
                    }
                    *state = ReaderState::Ready;
                    waker.ping();
                    state = reader_readiness
                        .changed
                        .wait_while(state, |state| *state == ReaderState::Ready)
                        .unwrap();
                    if *state == ReaderState::Stopped {
                        break;
                    }
                }
            })
            .map_err(|err| os_error!(err))?;
        Ok(Self { reader: Some(reader), stop, readiness })
    }

    pub(crate) fn is_ready(&self) -> bool {
        *self.readiness.state.lock().unwrap() == ReaderState::Ready
    }

    /// Exclusive queue access, valid until the application finishes this batch.
    pub(crate) fn try_drain(&mut self) -> Option<PendingEvents<'_>> {
        self.is_ready().then(|| PendingEvents { queue: self })
    }
}

pub(crate) struct PendingEvents<'a> {
    queue: &'a mut NativeEventQueue,
}

impl PendingEvents<'_> {
    pub(crate) fn next<'a>(
        &'a mut self,
        event: &'a mut MaybeUninit<ffi::XEvent>,
    ) -> Option<&'a mut ffi::XEvent> {
        unsafe extern "C" fn any_event(
            _: *mut ffi::Display,
            _: *mut ffi::XEvent,
            _: *mut c_char,
        ) -> c_int {
            ffi::True
        }
        let xconn = &self.queue.stop.xconn;
        // SAFETY: this guard pauses the reader, and XCheckIfEvent atomically
        // removes one event without blocking for further server input.
        let initialized = unsafe {
            (xconn.xlib.XCheckIfEvent)(
                xconn.display,
                event.as_mut_ptr(),
                Some(any_event),
                std::ptr::null_mut(),
            ) != 0
        };
        initialized.then(|| unsafe { event.assume_init_mut() })
    }
}

impl Drop for PendingEvents<'_> {
    fn drop(&mut self) {
        *self.queue.readiness.state.lock().unwrap() = ReaderState::Waiting;
        self.queue.readiness.changed.notify_one();
    }
}

impl Drop for NativeEventQueue {
    fn drop(&mut self) {
        let was_waiting = {
            let mut state = self.readiness.state.lock().unwrap();
            std::mem::replace(&mut *state, ReaderState::Stopped) == ReaderState::Waiting
        };
        self.readiness.changed.notify_one();
        if was_waiting {
            // Wake a blocking peek; a reader awaiting its drain guard only
            // needs the condition-variable notification above.
            self.stop.wake_reader();
        }
        if let Some(reader) = self.reader.take() {
            if reader.join().is_err() {
                tracing::error!("X11 event queue reader panicked");
            }
        }
    }
}

#[derive(Debug)]
struct StopWindow {
    xconn: Arc<XConnection>,
    window: Window,
}

impl StopWindow {
    fn new(xconn: Arc<XConnection>) -> Result<Self, X11Error> {
        let connection = xconn.xcb_connection();
        let window = connection.generate_id()?;
        connection
            .create_window(
                0,
                window,
                xconn.default_root().root,
                0,
                0,
                1,
                1,
                0,
                WindowClass::INPUT_ONLY,
                0,
                &CreateWindowAux::default(),
            )?
            .check()?;
        Ok(Self { xconn, window })
    }

    fn wake_reader(&self) {
        let event = ClientMessageEvent::new(32, self.window, 0_u32, [0_u32; 5]);
        if let Err(error) = self
            .xconn
            .xcb_connection()
            .send_event(false, self.window, EventMask::NO_EVENT, event)
            .map_err(X11Error::from)
            .and_then(|cookie| cookie.check().map_err(X11Error::from))
        {
            tracing::error!("Failed to stop X11 event queue reader: {error}");
        }
    }
}

impl Drop for StopWindow {
    fn drop(&mut self) {
        if let Err(error) = self
            .xconn
            .xcb_connection()
            .destroy_window(self.window)
            .map_err(X11Error::from)
            .and_then(|cookie| cookie.check().map_err(X11Error::from))
        {
            tracing::error!("Failed to destroy X11 event queue stop window: {error}");
        }
    }
}
