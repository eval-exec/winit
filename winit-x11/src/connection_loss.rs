//! Per-Display terminal-loss ownership and its minimal native Xlib gate.
//!
//! The gate contains ONLY atomic native Display addresses and the exact prior
//! global Xlib handler. It cannot reach a Rust owner, Box, Arc, or application
//! capability. The owner is reached solely through Xlib's per-Display exit
//! handler user_data. This matters because Xlib's default global I/O handler
//! exits before its per-Display exit handler (Xlib 1.8.13 XlibInt.c1311,
//! 1538-1543); replacing only the per-Display handler would be ineffective.

use std::ffi::{c_int, c_void};
use std::num::NonZeroU64;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

use x11rb::errors::ConnectionError;

use crate::ffi;

/// Opaque identity of one native X11 connection generation.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub struct X11ConnectionId(NonZeroU64);

/// Native interface that first reported an established connection's loss.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum X11ConnectionLossSource {
    /// Xlib entered the per-Display fatal I/O exit handler.
    XlibIo,
    /// XCB reported a transport I/O error.
    XcbIo,
}

/// The one terminal handoff for an established native connection.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct X11ConnectionLoss {
    /// Identity shared by the Xlib and XCB interfaces to this connection.
    pub connection_id: X11ConnectionId,
    /// Interface that won the terminal transition.
    pub source: X11ConnectionLossSource,
}

/// Terminal policy, owned by this Display's exit-handler user_data.
///
/// The callback can run on a native thread retaining native/caller locks. It
/// must publish through an already initialized lock-free owner handoff, then
/// stop that thread. It must not unwind, allocate, issue X requests, run Lisp
/// or GUI cleanup, or acquire application locks. It must never return.
///
/// A callback panic is caught at the foreign boundary and aborts the process;
/// that is a contract violation, never successful loss handling.
pub trait X11ConnectionLossHandler: Send + Sync {
    /// Publish this connection's terminal handoff and stop the reporting thread.
    fn connection_lost(&self, loss: X11ConnectionLoss) -> !;
}

/// Failure to bind or activate native connection-loss ownership.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum X11ConnectionLossRegistrationError {
    /// The event loop uses another backend.
    NotX11,
    /// Callback replacement and rearming are unsupported.
    AlreadyRegistered,
    /// The connection is already unavailable.
    ConnectionUnavailable,
    /// The loaded Xlib lacks the optional per-Display exit-handler interface.
    /// Native loss remains under the previous global policy.
    LossNotOwned,
    /// The bounded native Display-address set has no free entry.
    NativeGateCapacityExhausted,
}

impl std::fmt::Display for X11ConnectionLossRegistrationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::NotX11 => "the event loop does not use X11",
            Self::AlreadyRegistered => "an X11 connection-loss policy was already registered",
            Self::ConnectionUnavailable => "the X11 connection is unavailable",
            Self::LossNotOwned => {
                "loaded Xlib lacks XSetIOErrorExitHandler; native loss is not owned"
            },
            Self::NativeGateCapacityExhausted => {
                "the native X11 loss gate has no free Display entry"
            },
        })
    }
}

impl std::error::Error for X11ConnectionLossRegistrationError {}

/// Active lifetime of the per-Display policy.
///
/// Keep this until normal graphical shutdown. Drop retires an active policy
/// without making an X call, and cannot cancel a handoff already in progress.
/// The native address is removed separately, BEFORE XCloseDisplay, so address
/// reuse cannot make a new Display inherit an old policy.
#[derive(Debug)]
#[must_use = "dropping the registration retires the X11 connection-loss policy"]
pub struct X11ConnectionLossRegistration {
    state: Arc<ConnectionLossState>,
}

impl Drop for X11ConnectionLossRegistration {
    fn drop(&mut self) {
        let _ = self.state.phase.compare_exchange(
            Phase::Active as u8,
            Phase::Retired as u8,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
#[repr(u8)]
enum Phase {
    Unregistered,
    Installing,
    Active,
    Lost,
    Retired,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
#[repr(u8)]
enum NativeBinding {
    Unbound,
    Owned,
    LossNotOwned,
    CapacityExhausted,
    Retired,
}

pub(crate) struct ConnectionLossState {
    id: X11ConnectionId,
    // Opaque comparison only; no foreign callback dereferences this address.
    display: usize,
    handler: OnceLock<Box<dyn X11ConnectionLossHandler>>,
    phase: AtomicU8,
    native_binding: AtomicU8,
}

impl std::fmt::Debug for ConnectionLossState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ConnectionLossState")
            .field("id", &self.id)
            .field("phase", &self.phase.load(Ordering::Acquire))
            .field("native_binding", &self.native_binding.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}

static NEXT_CONNECTION_ID: AtomicU64 = AtomicU64::new(1);

impl ConnectionLossState {
    pub(crate) fn new(display: *mut ffi::Display) -> Self {
        // Exhaustion must never recycle a generation.
        let generation = NEXT_CONNECTION_ID
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |next| next.checked_add(1))
            .unwrap_or_else(|_| std::process::abort());
        let generation = NonZeroU64::new(generation).unwrap_or_else(|| std::process::abort());
        Self {
            id: X11ConnectionId(generation),
            display: display as usize,
            handler: OnceLock::new(),
            phase: AtomicU8::new(Phase::Unregistered as u8),
            native_binding: AtomicU8::new(NativeBinding::Unbound as u8),
        }
    }

    pub(crate) fn id(&self) -> X11ConnectionId {
        self.id
    }

    /// Bind immediately after successful XOpenDisplay, before any other X
    /// request or exposure to another thread. Owner policy is armed later.
    pub(crate) fn bind_native(
        self: &Arc<Self>,
        policy: &NativeLossPolicy,
    ) -> Result<(), X11ConnectionLossRegistrationError> {
        use X11ConnectionLossRegistrationError as Error;
        if self.display == 0 {
            return Err(Error::ConnectionUnavailable);
        }
        let Some(set_exit_handler) = policy.set_exit_handler else {
            self.native_binding.store(NativeBinding::LossNotOwned as u8, Ordering::Release);
            return Ok(());
        };
        if self.native_binding.load(Ordering::Acquire) != NativeBinding::Unbound as u8 {
            return Err(Error::AlreadyRegistered);
        }
        if !NATIVE_IO_GATE.register_display(self.display) {
            self.native_binding.store(NativeBinding::CapacityExhausted as u8, Ordering::Release);
            return Err(Error::NativeGateCapacityExhausted);
        }
        // Deliberate process-lifetime ownership: callbacks can retain user_data
        // after normal retirement/close. The address set is bounded and entries
        // are retired, but this per-Display allocation is never reclaimed, so an
        // in-flight foreign callback cannot observe freed Rust owner state.
        // X11_BACKEND retains one connection permanently; a future reconnecting
        // backend must reconsider this explicit lifetime policy.
        let data = Box::into_raw(Box::new(NativeUserData { state: Arc::clone(self) }));
        // SAFETY: Display just opened, user_data is process-live, and this
        // dynamic symbol has the XSetIOErrorExitHandler ABI. It changes only
        // the Display's callback fields (ErrHndlr.c97-114), without X I/O.
        unsafe {
            set_exit_handler(
                self.display as *mut ffi::Display,
                Some(xlib_exit_handler),
                data.cast(),
            )
        };
        self.native_binding.store(NativeBinding::Owned as u8, Ordering::Release);
        Ok(())
    }

    pub(crate) fn register(
        self: &Arc<Self>,
        handler: Box<dyn X11ConnectionLossHandler>,
    ) -> Result<X11ConnectionLossRegistration, X11ConnectionLossRegistrationError> {
        use X11ConnectionLossRegistrationError as Error;
        match self.native_binding.load(Ordering::Acquire) {
            binding if binding == NativeBinding::Owned as u8 => {},
            binding if binding == NativeBinding::CapacityExhausted as u8 => {
                return Err(Error::NativeGateCapacityExhausted);
            },
            binding if binding == NativeBinding::Retired as u8 => {
                return Err(Error::ConnectionUnavailable);
            },
            _ => return Err(Error::LossNotOwned),
        }
        self.phase
            .compare_exchange(
                Phase::Unregistered as u8,
                Phase::Installing as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map_err(|_| Error::AlreadyRegistered)?;
        if self.handler.set(handler).is_err() {
            self.phase.store(Phase::Retired as u8, Ordering::Release);
            return Err(Error::AlreadyRegistered);
        }
        self.phase.store(Phase::Active as u8, Ordering::Release);
        Ok(X11ConnectionLossRegistration { state: Arc::clone(self) })
    }

    /// Remove native ownership BEFORE the caller invokes XCloseDisplay.
    pub(crate) fn retire_native_binding(&self) {
        loop {
            let phase = self.phase.load(Ordering::Acquire);
            match phase {
                value if value == Phase::Lost as u8 => park_forever(),
                value if value == Phase::Installing as u8 => std::hint::spin_loop(),
                value if value == Phase::Retired as u8 => break,
                _ => {
                    if self
                        .phase
                        .compare_exchange(
                            phase,
                            Phase::Retired as u8,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_ok()
                    {
                        break;
                    }
                },
            }
        }
        if self.native_binding.swap(NativeBinding::Retired as u8, Ordering::AcqRel)
            == NativeBinding::Owned as u8
        {
            NATIVE_IO_GATE.retire_display(self.display);
        }
    }

    /// Called before/after every delegated XCB operation. A lost epoch always
    /// stops, including a later apparently successful result.
    pub(crate) fn observe_xcb(&self, error: Option<&ConnectionError>) {
        self.observe(
            X11ConnectionLossSource::XcbIo,
            matches!(error, Some(ConnectionError::IoError(_))),
        );
    }

    pub(crate) fn preflight_xlib(&self) {
        self.observe(X11ConnectionLossSource::XlibIo, false);
    }

    fn observe(&self, source: X11ConnectionLossSource, io_error: bool) {
        loop {
            match self.phase.load(Ordering::Acquire) {
                phase if phase == Phase::Installing as u8 => std::hint::spin_loop(),
                phase if phase == Phase::Lost as u8 => park_forever(),
                phase if phase == Phase::Active as u8 && io_error => {
                    if self
                        .phase
                        .compare_exchange(
                            Phase::Active as u8,
                            Phase::Lost as u8,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_ok()
                    {
                        let handler = self
                            .handler
                            .get()
                            .expect("active X11 loss policy has an immutable owner");
                        handler
                            .connection_lost(X11ConnectionLoss { connection_id: self.id, source });
                    }
                },
                _ => return,
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test(handler: fn(X11ConnectionLoss) -> !) -> Arc<Self> {
        let state = Self::for_test_inactive();
        state.native_binding.store(NativeBinding::Owned as u8, Ordering::Release);
        assert!(state.handler.set(Box::new(FnObserver(handler))).is_ok());
        state.phase.store(Phase::Active as u8, Ordering::Release);
        state
    }

    #[cfg(test)]
    pub(crate) fn for_test_inactive() -> Arc<Self> {
        Arc::new(Self::new(1usize as *mut ffi::Display))
    }
}

#[cfg(test)]
struct FnObserver(fn(X11ConnectionLoss) -> !);

#[cfg(test)]
impl X11ConnectionLossHandler for FnObserver {
    fn connection_lost(&self, loss: X11ConnectionLoss) -> ! {
        (self.0)(loss)
    }
}

type XlibIoErrorHandler = Option<unsafe extern "C" fn(*mut ffi::Display) -> c_int>;
type XlibIoExitHandler = Option<unsafe extern "C" fn(*mut ffi::Display, *mut c_void)>;
type SetIoExitHandler = unsafe extern "C" fn(*mut ffi::Display, XlibIoExitHandler, *mut c_void);

/// Resolved before the first owned XOpenDisplay. Missing optional symbols leave
/// the original global policy untouched and make later activation refuse.
#[derive(Debug, Copy, Clone)]
pub(crate) struct NativeLossPolicy {
    set_exit_handler: Option<SetIoExitHandler>,
}

impl NativeLossPolicy {
    pub(crate) fn prepare(xlib: &ffi::Xlib) -> Self {
        let set_exit_handler = resolve_exit_handler(xlib);
        if set_exit_handler.is_some() {
            NATIVE_IO_GATE.install(|| {
                // Xlib1.8.13 ErrHndlr.c67-88 takes only its global handler
                // mutex; it performs no display operation or X I/O. Preparing
                // before Open preserves independence from caller/display locks.
                unsafe { (xlib.XSetIOErrorHandler)(Some(xlib_io_gate)) }
            });
        }
        Self { set_exit_handler }
    }
}

fn resolve_exit_handler(xlib: &ffi::Xlib) -> Option<SetIoExitHandler> {
    // x11-dl caches and retains this exact library for the process lifetime,
    // but its DynamicLibrary handle is private. Identify its existing mapping
    // through a known function and use RTLD_NOLOAD on that exact pathname.
    // No link-time libX11 dependency, alternate library, or RTLD_DEFAULT search.
    unsafe {
        let mut info: libc::Dl_info = std::mem::zeroed();
        if libc::dladdr(xlib.XOpenDisplay as *const () as *const c_void, &mut info) == 0
            || info.dli_fname.is_null()
        {
            return None;
        }
        let handle = libc::dlopen(info.dli_fname, libc::RTLD_LAZY | libc::RTLD_NOLOAD);
        if handle.is_null() {
            return None;
        }
        let symbol = libc::dlsym(handle, c"XSetIOErrorExitHandler".as_ptr());
        libc::dlclose(handle);
        if symbol.is_null() {
            None
        } else {
            // POSIX dlsym yields the documented C function ABI. Closing only
            // our NOLOAD reference cannot unload x11-dl's permanent reference.
            Some(std::mem::transmute::<*mut c_void, SetIoExitHandler>(symbol))
        }
    }
}

// The allowed process-global exception is native-only. No application state,
// Box, Arc, observer, user_data, or native->owner lookup is reachable here.
const NATIVE_DISPLAY_CAPACITY: usize = 8;
const GATE_UNINSTALLED: u8 = 0;
const GATE_INSTALLING: u8 = 1;
const GATE_ARMED: u8 = 2;

struct NativeIoGate {
    displays: [AtomicUsize; NATIVE_DISPLAY_CAPACITY],
    prior: AtomicUsize,
    phase: AtomicU8,
}

impl NativeIoGate {
    const fn new() -> Self {
        Self {
            displays: [const { AtomicUsize::new(0) }; NATIVE_DISPLAY_CAPACITY],
            prior: AtomicUsize::new(0),
            phase: AtomicU8::new(GATE_UNINSTALLED),
        }
    }

    fn install(&self, installer: impl FnOnce() -> XlibIoErrorHandler) {
        if self
            .phase
            .compare_exchange(
                GATE_UNINSTALLED,
                GATE_INSTALLING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
        {
            let prior = installer();
            self.prior.store(prior.map_or(0, |handler| handler as usize), Ordering::Relaxed);
            self.phase.store(GATE_ARMED, Ordering::Release);
        } else {
            self.wait_until_armed();
        }
    }

    fn wait_until_armed(&self) {
        while self.phase.load(Ordering::Acquire) == GATE_INSTALLING {
            // Only the non-I/O XSetIOErrorHandler installer is in progress.
            // No Display/application mutex or owner capability is consulted.
            std::hint::spin_loop();
        }
    }

    fn register_display(&self, display: usize) -> bool {
        if display == 0
            || self.displays.iter().any(|entry| entry.load(Ordering::Acquire) == display)
        {
            return false;
        }
        for entry in &self.displays {
            match entry.compare_exchange(0, display, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return true,
                Err(existing) if existing == display => return false,
                Err(_) => {},
            }
        }
        false
    }

    fn retire_display(&self, display: usize) {
        for entry in &self.displays {
            if entry.compare_exchange(display, 0, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                return;
            }
        }
    }

    fn dispatch(&self, display: *mut ffi::Display) -> c_int {
        self.wait_until_armed();
        let address = display as usize;
        if address != 0
            && self.displays.iter().any(|entry| entry.load(Ordering::Acquire) == address)
        {
            // Let _XIOError reach THIS Display's exit_handler/user_data.
            return 0;
        }
        self.call_prior(display)
    }

    fn call_prior(&self, display: *mut ffi::Display) -> c_int {
        self.wait_until_armed();
        let address = self.prior.load(Ordering::Acquire);
        if address == 0 {
            // Matching Xlib1.8.13 always returns a non-null prior (including
            // its normalized default). Preserve an actual absent callback
            // without inventing a signal/exit policy for an unknown Display.
            return 0;
        }
        // SAFETY: published only from XSetIOErrorHandler's exact prior function.
        let prior = unsafe {
            std::mem::transmute::<usize, unsafe extern "C" fn(*mut ffi::Display) -> c_int>(address)
        };
        unsafe { prior(display) }
    }
}

static NATIVE_IO_GATE: NativeIoGate = NativeIoGate::new();

struct NativeUserData {
    state: Arc<ConnectionLossState>,
}

unsafe extern "C" fn xlib_io_gate(display: *mut ffi::Display) -> c_int {
    match catch_unwind(AssertUnwindSafe(|| NATIVE_IO_GATE.dispatch(display))) {
        Ok(result) => result,
        Err(_) => std::process::abort(),
    }
}

unsafe extern "C" fn xlib_exit_handler(display: *mut ffi::Display, data: *mut c_void) {
    let result = catch_unwind(AssertUnwindSafe(|| {
        if data.is_null() {
            std::process::abort();
        }
        // SAFETY: user_data comes from bind_native's process-lifetime Box.
        let data = unsafe { &*data.cast::<NativeUserData>() };
        if data.state.display != display as usize {
            std::process::abort();
        }
        data.state.observe(X11ConnectionLossSource::XlibIo, true);
        // Before owner arming (or after normal retirement), retain the exact
        // saved global policy. A fresh Display's native default exit handler
        // calls exit(1) if that prior handler returns (ErrHndlr.c97-114 and
        // XlibInt.c _XDefaultIOErrorExit). No GUI owner is implied before arm.
        NATIVE_IO_GATE.call_prior(display);
        unsafe { libc::exit(1) };
    }));
    if result.is_err() {
        std::process::abort();
    }
    // No known-owned exit callback can return into a failed native request.
    std::process::abort();
}

fn park_forever() -> ! {
    loop {
        // No TLS initialization, allocation, Rust/application mutex, X request,
        // native join, or unwind. A signal can interrupt pause; never resume.
        unsafe { libc::pause() };
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::os::unix::process::{CommandExt, ExitStatusExt};
    use std::process::{Child, Command, Stdio};
    use std::sync::atomic::{AtomicI32, AtomicUsize};
    use std::sync::{Barrier, mpsc};
    use std::thread;
    use std::time::{Duration, Instant};

    use super::*;

    fn unexpected_loss(_: X11ConnectionLoss) -> ! {
        panic!("healthy/inactive policy must not report loss")
    }

    fn io_error() -> ConnectionError {
        ConnectionError::IoError(io::Error::from(io::ErrorKind::BrokenPipe))
    }

    fn native_owned_state() -> Arc<ConnectionLossState> {
        let state = ConnectionLossState::for_test_inactive();
        // No Xlib call/address registration in these pure owner-state tests.
        state.native_binding.store(NativeBinding::Owned as u8, Ordering::Release);
        state
    }

    #[test]
    fn unregistered_connection_preserves_startup_io_errors() {
        let state = ConnectionLossState::for_test_inactive();
        state.observe_xcb(Some(&io_error()));
        state.observe_xcb(None);
        state.preflight_xlib();
        assert_eq!(state.phase.load(Ordering::Acquire), Phase::Unregistered as u8);
        assert!(state.handler.get().is_none());
    }

    #[test]
    fn active_connection_does_not_reclassify_non_io_errors() {
        let state = ConnectionLossState::for_test(unexpected_loss);
        for error in [
            ConnectionError::UnknownError,
            ConnectionError::UnsupportedExtension,
            ConnectionError::MaximumRequestLengthExceeded,
            ConnectionError::FdPassingFailed,
            ConnectionError::InsufficientMemory,
            ConnectionError::ParseError(x11rb::errors::ParseError::InvalidValue),
        ] {
            state.observe_xcb(Some(&error));
        }
        state.observe_xcb(None);
        state.preflight_xlib();
        assert_eq!(state.phase.load(Ordering::Acquire), Phase::Active as u8);
    }

    #[test]
    fn registration_is_single_use_and_retirement_keeps_identity() {
        let state = native_owned_state();
        let id = state.id();
        let token = state.register(Box::new(FnObserver(unexpected_loss))).unwrap();
        assert_eq!(state.phase.load(Ordering::Acquire), Phase::Active as u8);
        assert!(matches!(
            state.register(Box::new(FnObserver(unexpected_loss))),
            Err(X11ConnectionLossRegistrationError::AlreadyRegistered)
        ));
        drop(token);
        assert_eq!(state.id(), id);
        assert_eq!(state.phase.load(Ordering::Acquire), Phase::Retired as u8);
        state.observe_xcb(Some(&io_error()));
        assert!(matches!(
            state.register(Box::new(FnObserver(unexpected_loss))),
            Err(X11ConnectionLossRegistrationError::AlreadyRegistered)
        ));
        let reused_address = ConnectionLossState::for_test_inactive();
        assert_ne!(state.id(), reused_address.id());
    }

    #[test]
    fn missing_optional_exit_handler_refuses_ownership_without_global_override() {
        let before = NATIVE_IO_GATE.phase.load(Ordering::Acquire);
        let policy = NativeLossPolicy { set_exit_handler: None };
        let state = ConnectionLossState::for_test_inactive();
        state.bind_native(&policy).unwrap();
        assert_eq!(state.native_binding.load(Ordering::Acquire), NativeBinding::LossNotOwned as u8);
        assert_eq!(NATIVE_IO_GATE.phase.load(Ordering::Acquire), before);
        assert!(matches!(
            state.register(Box::new(FnObserver(unexpected_loss))),
            Err(X11ConnectionLossRegistrationError::LossNotOwned)
        ));
        state.observe_xcb(Some(&io_error()));
        assert_eq!(state.phase.load(Ordering::Acquire), Phase::Unregistered as u8);
    }

    #[test]
    fn native_address_set_refuses_capacity_and_releases_before_reuse() {
        let gate = NativeIoGate::new();
        for display in 1..=NATIVE_DISPLAY_CAPACITY {
            assert!(gate.register_display(display));
        }
        assert!(!gate.register_display(NATIVE_DISPLAY_CAPACITY + 1));
        assert!(!gate.register_display(1));
        gate.retire_display(1);
        assert!(gate.register_display(NATIVE_DISPLAY_CAPACITY + 1));
        let state = ConnectionLossState::for_test_inactive();
        state.native_binding.store(NativeBinding::CapacityExhausted as u8, Ordering::Release);
        assert!(matches!(
            state.register(Box::new(FnObserver(unexpected_loss))),
            Err(X11ConnectionLossRegistrationError::NativeGateCapacityExhausted)
        ));
    }

    static PRIOR_CALLS: AtomicUsize = AtomicUsize::new(0);
    static PRIOR_DISPLAY: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" fn prior_policy(display: *mut ffi::Display) -> c_int {
        PRIOR_CALLS.fetch_add(1, Ordering::SeqCst);
        PRIOR_DISPLAY.store(display as usize, Ordering::SeqCst);
        37
    }

    #[test]
    fn native_gate_chains_unknown_and_retired_displays_without_owner_lookup() {
        let gate = NativeIoGate::new();
        gate.install(|| Some(prior_policy));
        assert!(gate.register_display(1));
        assert_eq!(gate.dispatch(1usize as *mut ffi::Display), 0);
        assert_eq!(PRIOR_CALLS.load(Ordering::SeqCst), 0);
        assert_eq!(gate.dispatch(2usize as *mut ffi::Display), 37);
        assert_eq!(PRIOR_CALLS.load(Ordering::SeqCst), 1);
        assert_eq!(PRIOR_DISPLAY.load(Ordering::SeqCst), 2);
        gate.retire_display(1);
        assert_eq!(gate.dispatch(1usize as *mut ffi::Display), 37);
        assert_eq!(PRIOR_CALLS.load(Ordering::SeqCst), 2);
        assert_eq!(PRIOR_DISPLAY.load(Ordering::SeqCst), 1);
    }

    static RACE_PRIOR_CALLS: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" fn race_prior_policy(_: *mut ffi::Display) -> c_int {
        RACE_PRIOR_CALLS.fetch_add(1, Ordering::SeqCst);
        43
    }

    #[test]
    fn native_gate_concurrent_entry_waits_for_the_actual_prior_handler() {
        let gate = Arc::new(NativeIoGate::new());
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let installer = {
            let gate = Arc::clone(&gate);
            thread::spawn(move || {
                gate.install(|| {
                    entered_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    Some(race_prior_policy)
                })
            })
        };
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(gate.phase.load(Ordering::Acquire), GATE_INSTALLING);
        let (returned_tx, returned_rx) = mpsc::channel();
        let reporter = {
            let gate = Arc::clone(&gate);
            thread::spawn(move || {
                returned_tx.send(gate.dispatch(2usize as *mut ffi::Display)).unwrap()
            })
        };
        assert!(returned_rx.recv_timeout(Duration::from_millis(20)).is_err());
        assert_eq!(RACE_PRIOR_CALLS.load(Ordering::SeqCst), 0);
        release_tx.send(()).unwrap();
        installer.join().unwrap();
        assert_eq!(returned_rx.recv_timeout(Duration::from_secs(2)).unwrap(), 43);
        reporter.join().unwrap();
        assert_eq!(RACE_PRIOR_CALLS.load(Ordering::SeqCst), 1);
        // A second prepare cannot replace the exact saved handler.
        gate.install(|| panic!("global gate must be installed only once"));
    }

    struct CountedObserver(Arc<AtomicUsize>);

    impl X11ConnectionLossHandler for CountedObserver {
        fn connection_lost(&self, _: X11ConnectionLoss) -> ! {
            panic!("lifetime test must not report loss")
        }
    }

    impl Drop for CountedObserver {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn per_display_userdata_keeps_the_boxed_owner_alive_after_retirement() {
        let drops = Arc::new(AtomicUsize::new(0));
        let state = native_owned_state();
        let token = state.register(Box::new(CountedObserver(Arc::clone(&drops)))).unwrap();
        let data = Box::new(NativeUserData { state: Arc::clone(&state) });
        let weak = Arc::downgrade(&state);
        drop(token);
        state.retire_native_binding();
        drop(state);
        assert!(weak.upgrade().is_some());
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        // No native callback exists in this pure test, so reclaiming is safe
        // here. Production intentionally keeps this same allocation alive.
        drop(data);
        assert!(weak.upgrade().is_none());
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }
    struct OwnedChild(Option<Child>);

    impl Drop for OwnedChild {
        fn drop(&mut self) {
            if let Some(child) = self.0.as_mut() {
                if !matches!(child.try_wait(), Ok(Some(_))) {
                    // unshare's --kill-child removes the namespace's PID 1,
                    // including reporters parked in separate native stacks.
                    unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
                }
                let _ = child.wait();
            }
        }
    }

    fn run_isolated_child(test: &str, mode: &str) {
        assert_eq!(std::env::var("CARFRAME_TEST_IN_SANDBOX").as_deref(), Ok("1"));
        let cgroup = std::fs::read_to_string("/proc/self/cgroup").unwrap();
        let path = cgroup.lines().find_map(|line| line.strip_prefix("0::")).unwrap();
        let memory = std::fs::read_to_string(format!("/sys/fs/cgroup{path}/memory.max")).unwrap();
        assert!(
            memory.trim().parse::<u64>().is_ok_and(|limit| limit > 0),
            "finite outer memory scope required"
        );
        let mut child = OwnedChild(Some(
            Command::new("unshare")
                .args([
                    "--user",
                    "--map-current-user",
                    "--pid",
                    "--fork",
                    "--mount-proc",
                    "--kill-child",
                    "--",
                    "prlimit",
                    "--core=0",
                    "--",
                ])
                .arg(std::env::current_exe().unwrap())
                .args(["--exact", test, "--nocapture"])
                .env("WINIT_X11_LOSS_CHILD", mode)
                .process_group(0)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("start scoped nonreturning-loss child"),
        ));
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if child.0.as_mut().unwrap().try_wait().unwrap().is_some() {
                break;
            }
            assert!(Instant::now() < deadline, "nonreturning-loss child exceeded budget");
            thread::sleep(Duration::from_millis(5));
        }
        let output = child.0.take().unwrap().wait_with_output().unwrap();
        println!(
            "ISOLATED_LOSS mode={mode} status={} stdout={} stderr={}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        match mode {
            "panic" => assert_eq!(output.status.signal(), Some(libc::SIGABRT)),
            "cold" => {
                assert_eq!(output.status.code(), Some(1));
                assert!(String::from_utf8_lossy(&output.stdout).contains("EXACT_PRIOR_CALLED"));
            },
            _ => assert!(output.status.success()),
        }
    }

    static NOTICE_FD: AtomicI32 = AtomicI32::new(-1);

    fn notice_and_stop(loss: X11ConnectionLoss) -> ! {
        let notice = [
            loss.connection_id.0.get(),
            match loss.source {
                X11ConnectionLossSource::XlibIo => 1,
                X11ConnectionLossSource::XcbIo => 2,
            },
        ];
        let bytes = std::mem::size_of_val(&notice);
        // Test callback follows the same no-allocation/no-lock/no-X contract.
        let written = unsafe {
            libc::write(NOTICE_FD.load(Ordering::Relaxed), notice.as_ptr().cast(), bytes)
        };
        if written != bytes as isize {
            unsafe { libc::_exit(91) };
        }
        park_forever()
    }

    fn loss_child(mode: &str) -> ! {
        assert_eq!(std::process::id(), 1, "actual private PID namespace required");
        let cgroup = std::fs::read_to_string("/proc/self/cgroup").unwrap();
        let path = cgroup.lines().find_map(|line| line.strip_prefix("0::")).unwrap();
        let memory = std::fs::read_to_string(format!("/sys/fs/cgroup{path}/memory.max")).unwrap();
        assert!(memory.trim().parse::<u64>().is_ok_and(|limit| limit > 0));
        println!("ACTUAL_LOSS_CHILD_SCOPE PID=1 MEMORY_MAX={} CGROUP={path}", memory.trim());
        let mut pipe = [-1; 2];
        assert_eq!(
            unsafe { libc::pipe2(pipe.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) },
            0
        );
        NOTICE_FD.store(pipe[1], Ordering::Relaxed);
        let state = native_owned_state();
        let _token = state.register(Box::new(FnObserver(notice_and_stop))).unwrap();
        let returned = Arc::new(AtomicUsize::new(0));
        let count = if mode == "race" { 2 } else { 1 };
        let start = Arc::new(Barrier::new(count + 1));
        for reporter in 0..count {
            let state = Arc::clone(&state);
            let start = Arc::clone(&start);
            let returned = Arc::clone(&returned);
            let source_xlib = mode == "xlib" || (mode == "race" && reporter == 0);
            thread::spawn(move || {
                start.wait();
                if source_xlib {
                    let data =
                        Box::into_raw(Box::new(NativeUserData { state: Arc::clone(&state) }));
                    unsafe { xlib_exit_handler(state.display as *mut ffi::Display, data.cast()) };
                } else {
                    state.observe_xcb(Some(&io_error()));
                }
                returned.fetch_add(1, Ordering::SeqCst);
            });
        }
        start.wait();
        let mut notice = [0u64; 2];
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let received = unsafe {
                libc::read(pipe[0], notice.as_mut_ptr().cast(), std::mem::size_of_val(&notice))
            };
            if received > 0 {
                assert_eq!(received as usize, std::mem::size_of_val(&notice));
                break;
            }
            assert!(Instant::now() < deadline, "owner notice was not published");
            thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(notice[0], state.id().0.get());
        match mode {
            "xlib" => assert_eq!(notice[1], 1),
            "xcb" => assert_eq!(notice[1], 2),
            "race" => assert!([1, 2].contains(&notice[1])),
            _ => panic!("unknown child mode"),
        }
        assert_eq!(state.phase.load(Ordering::Acquire), Phase::Lost as u8);
        drop(_token);
        assert_eq!(
            state.phase.load(Ordering::Acquire),
            Phase::Lost as u8,
            "retiring an already lost epoch must not cancel its handoff"
        );
        // A later healthy result must not let another observer resume native work.
        {
            let state = Arc::clone(&state);
            let returned = Arc::clone(&returned);
            thread::spawn(move || {
                state.observe_xcb(None);
                returned.fetch_add(1, Ordering::SeqCst);
            });
        }
        {
            let state = Arc::clone(&state);
            let returned = Arc::clone(&returned);
            thread::spawn(move || {
                state.preflight_xlib();
                returned.fetch_add(1, Ordering::SeqCst);
            });
        }
        thread::sleep(Duration::from_millis(20));
        assert_eq!(returned.load(Ordering::SeqCst), 0);
        let duplicate = unsafe {
            libc::read(pipe[0], notice.as_mut_ptr().cast(), std::mem::size_of_val(&notice))
        };
        assert_eq!(duplicate, -1, "the epoch published more than one notice");
        assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::EAGAIN));
        println!(
            "LOSS_CHILD id={:?} source={} reports=1 all_reporters_stopped=true",
            state.id(),
            notice[1]
        );
        // Successful owner completion ends the disposable process, never joins
        // parked native stacks or leaves them in the parent test process.
        std::process::exit(0)
    }

    #[test]
    fn xlib_and_xcb_share_identity_and_never_return_after_loss() {
        if let Ok(mode) = std::env::var("WINIT_X11_LOSS_CHILD") {
            loss_child(&mode);
        }
        for mode in ["xlib", "xcb"] {
            run_isolated_child(
                "connection_loss::tests::xlib_and_xcb_share_identity_and_never_return_after_loss",
                mode,
            );
        }
    }

    #[test]
    fn simultaneous_xlib_and_xcb_reporters_publish_once_and_both_stop() {
        if let Ok(mode) = std::env::var("WINIT_X11_LOSS_CHILD") {
            loss_child(&mode);
        }
        run_isolated_child(
            "connection_loss::tests::simultaneous_xlib_and_xcb_reporters_publish_once_and_both_stop",
            "race",
        );
    }

    #[test]
    fn foreign_callback_owner_panic_aborts_without_unwinding_through_c() {
        if std::env::var("WINIT_X11_LOSS_CHILD").as_deref() == Ok("panic") {
            assert_eq!(std::env::var("WINIT_X11_LOSS_PANIC_SUPERVISED").as_deref(), Ok("1"));
            assert!(std::process::id() > 1, "fatal-signal victim must not be namespace init");
            let state = ConnectionLossState::for_test(unexpected_loss);
            let display = state.display as *mut ffi::Display;
            let data = Box::into_raw(Box::new(NativeUserData { state }));
            unsafe { xlib_exit_handler(display, data.cast()) };
            panic!("foreign loss callback returned");
        }
        assert_eq!(std::env::var("CARFRAME_TEST_IN_SANDBOX").as_deref(), Ok("1"));
        // Core-PC evidence on treeroot proves that putting this abort victim
        // at namespace PID1 reaches glibc's hlt fallback (SIGSEGV), not the
        // ordinary process SIGABRT contract. Keep init as a bounded supervisor.
        let mut child = OwnedChild(Some(
            Command::new("unshare")
                .args([
                    "--user", "--map-current-user", "--pid", "--fork",
                    "--mount-proc", "--kill-child", "--", "prlimit", "--core=0", "--",
                    "python3", "-I", "-c",
                    r#"
import os, signal, subprocess, sys
assert os.getpid() == 1
assert os.environ.get('CARFRAME_TEST_IN_SANDBOX') == '1'
with open('/proc/self/cgroup') as source:
    cgroup = next(line[3:].strip() for line in source if line.startswith('0::'))
with open('/sys/fs/cgroup' + cgroup + '/memory.max') as source:
    memory = source.read().strip()
assert memory != 'max' and int(memory) > 0
print('PANIC_SUPERVISOR_SCOPE PID=1 MEMORY_MAX=' + memory, flush=True)
environment = os.environ.copy()
environment['WINIT_X11_LOSS_PANIC_SUPERVISED'] = '1'
child = subprocess.Popen(sys.argv[1:], env=environment, start_new_session=True)
try:
    status = child.wait(timeout=6)
except BaseException:
    try:
        os.killpg(child.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    child.wait(timeout=2)
    raise
print('PANIC_VICTIM_RAW_RETURN=' + str(status), flush=True)
# No signal is manufactured by this supervisor. Only the Rust child's actual
# signaled SIGABRT satisfies the original strict expectation.
assert status == -signal.SIGABRT, 'wrong native panic-abort result: ' + str(status)
"#,
                ])
                .arg(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "connection_loss::tests::foreign_callback_owner_panic_aborts_without_unwinding_through_c",
                    "--nocapture",
                ])
                .env("WINIT_X11_LOSS_CHILD", "panic")
                .process_group(0)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("start bounded PID1 supervisor and real Rust panic victim"),
        ));
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if child.0.as_mut().unwrap().try_wait().unwrap().is_some() {
                break;
            }
            assert!(Instant::now() < deadline, "supervised panic child exceeded budget");
            thread::sleep(Duration::from_millis(5));
        }
        let output = child.0.take().unwrap().wait_with_output().unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        println!(
            "SUPERVISED_PANIC status={} stdout={} stderr={}",
            output.status,
            stdout,
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.status.success(), "native child did not satisfy strict SIGABRT");
        assert!(stdout.lines().any(|line| line == "PANIC_VICTIM_RAW_RETURN=-6"));
    }

    unsafe extern "C" fn cold_prior(_: *mut ffi::Display) -> c_int {
        let text = b"EXACT_PRIOR_CALLED\n";
        let _ = unsafe { libc::write(libc::STDOUT_FILENO, text.as_ptr().cast(), text.len()) };
        37
    }

    #[test]
    fn unarmed_exit_userdata_preserves_prior_then_fresh_display_default_exit() {
        if std::env::var("WINIT_X11_LOSS_CHILD").as_deref() == Ok("cold") {
            assert_eq!(std::process::id(), 1);
            NATIVE_IO_GATE.install(|| Some(cold_prior));
            let state = ConnectionLossState::for_test_inactive();
            let display = state.display as *mut ffi::Display;
            let data = Box::into_raw(Box::new(NativeUserData { state }));
            unsafe { xlib_exit_handler(display, data.cast()) };
            panic!("unarmed exit callback returned");
        }
        run_isolated_child(
            "connection_loss::tests::unarmed_exit_userdata_preserves_prior_then_fresh_display_default_exit",
            "cold",
        );
    }
}
