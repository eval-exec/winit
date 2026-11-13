//! Keep every XCB request and its cookies on the registered connection-loss route.

use std::ffi::c_void;
use std::fmt;
use std::io::IoSlice;
use std::sync::Arc;

use x11rb::connection::{
    BufWithFds, Connection, DiscardMode, RawEventAndSeqNumber, ReplyOrError, RequestConnection,
    RequestKind, SequenceNumber,
};
use x11rb::cookie::{Cookie, CookieWithFds, VoidCookie};
use x11rb::errors::{ConnectionError, ParseError, ReplyOrIdError};
use x11rb::protocol::Event;
use x11rb::protocol::xproto::Setup;
use x11rb::utils::RawFdContainer;
use x11rb::x11_utils::{ExtensionInformation, TryParse, TryParseFd, X11Error};
use x11rb::xcb_ffi::XCBConnection;

use crate::connection_loss::ConnectionLossState;

/// The native connection can fail even when an operation returned success.
#[doc(hidden)]
pub trait NativeHealth {
    fn native_error(&self) -> Option<ConnectionError>;
}

impl NativeHealth for XCBConnection {
    fn native_error(&self) -> Option<ConnectionError> {
        self.has_error()
    }
}

/// Inferred connection type in existing public X11 request-cookie results.
/// Construction and the wrapped native connection remain private.
#[doc(hidden)]
pub struct GuardedConnection<C> {
    inner: C,
    state: Arc<ConnectionLossState>,
}

pub(crate) type GuardedXcbConnection = GuardedConnection<XCBConnection>;

impl<C: fmt::Debug> fmt::Debug for GuardedConnection<C> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GuardedConnection")
            .field("inner", &self.inner)
            .finish_non_exhaustive()
    }
}

impl<C: Connection + NativeHealth> GuardedConnection<C> {
    pub(crate) fn new(inner: C, state: Arc<ConnectionLossState>) -> Self {
        Self { inner, state }
    }

    fn observe_health(&self) {
        self.state.observe_xcb(self.inner.native_error().as_ref());
    }

    fn complete<T>(&self, result: Result<T, ConnectionError>) -> Result<T, ConnectionError> {
        self.state.observe_xcb(result.as_ref().err());
        self.observe_health();
        result
    }

    fn complete_id<T>(&self, result: Result<T, ReplyOrIdError>) -> Result<T, ReplyOrIdError> {
        if let Err(ReplyOrIdError::ConnectionError(error)) = &result {
            self.state.observe_xcb(Some(error));
        }
        self.observe_health();
        result
    }
}

impl<C: NativeHealth> NativeHealth for GuardedConnection<C> {
    fn native_error(&self) -> Option<ConnectionError> {
        self.inner.native_error()
    }
}

impl GuardedXcbConnection {
    /// The only raw-XCB exception: synchronous XKB APIs need the borrowed pointer.
    /// Callers must neither retain nor return it. Both failure and successful
    /// native returns are checked before control leaves this scope.
    pub(crate) fn with_xkb_connection<T>(&self, operation: impl FnOnce(*mut c_void) -> T) -> T {
        self.observe_health();
        let result = operation(self.inner.get_raw_xcb_connection());
        self.observe_health();
        result
    }
}

impl<C: Connection + NativeHealth> RequestConnection for GuardedConnection<C> {
    type Buf = C::Buf;

    fn send_request_with_reply<R>(
        &self,
        bufs: &[IoSlice<'_>],
        fds: Vec<RawFdContainer>,
    ) -> Result<Cookie<'_, Self, R>, ConnectionError>
    where
        R: TryParse,
    {
        self.observe_health();
        let original = self.complete(self.inner.send_request_with_reply::<R>(bufs, fds))?;
        let sequence = original.sequence_number();
        // The cookie owns only a borrow and sequence (plus PhantomData). Transfer
        // its one eventual reply/discard responsibility, without prematurely
        // running the inner connection's discard-on-Drop.
        std::mem::forget(original);
        Ok(Cookie::new(self, sequence))
    }

    fn send_request_with_reply_with_fds<R>(
        &self,
        bufs: &[IoSlice<'_>],
        fds: Vec<RawFdContainer>,
    ) -> Result<CookieWithFds<'_, Self, R>, ConnectionError>
    where
        R: TryParseFd,
    {
        self.observe_health();
        let original =
            self.complete(self.inner.send_request_with_reply_with_fds::<R>(bufs, fds))?;
        let sequence = original.sequence_number();
        // No received FDs live in this cookie; they are acquired by raw_reply.
        std::mem::forget(original);
        Ok(CookieWithFds::new(self, sequence))
    }

    fn send_request_without_reply(
        &self,
        bufs: &[IoSlice<'_>],
        fds: Vec<RawFdContainer>,
    ) -> Result<VoidCookie<'_, Self>, ConnectionError> {
        self.observe_health();
        let original = self.complete(self.inner.send_request_without_reply(bufs, fds))?;
        let sequence = original.sequence_number();
        std::mem::forget(original);
        Ok(VoidCookie::new(self, sequence))
    }

    fn discard_reply(&self, sequence: SequenceNumber, kind: RequestKind, mode: DiscardMode) {
        self.observe_health();
        self.inner.discard_reply(sequence, kind, mode);
        self.observe_health();
    }

    fn prefetch_extension_information(
        &self,
        extension_name: &'static str,
    ) -> Result<(), ConnectionError> {
        self.observe_health();
        self.complete(self.inner.prefetch_extension_information(extension_name))
    }

    fn extension_information(
        &self,
        extension_name: &'static str,
    ) -> Result<Option<ExtensionInformation>, ConnectionError> {
        self.observe_health();
        self.complete(self.inner.extension_information(extension_name))
    }

    fn wait_for_reply_or_raw_error(
        &self,
        sequence: SequenceNumber,
    ) -> Result<ReplyOrError<Self::Buf>, ConnectionError> {
        self.observe_health();
        self.complete(self.inner.wait_for_reply_or_raw_error(sequence))
    }

    fn wait_for_reply(
        &self,
        sequence: SequenceNumber,
    ) -> Result<Option<Self::Buf>, ConnectionError> {
        self.observe_health();
        self.complete(self.inner.wait_for_reply(sequence))
    }

    fn wait_for_reply_with_fds_raw(
        &self,
        sequence: SequenceNumber,
    ) -> Result<ReplyOrError<BufWithFds<Self::Buf>, Self::Buf>, ConnectionError> {
        self.observe_health();
        self.complete(self.inner.wait_for_reply_with_fds_raw(sequence))
    }

    fn check_for_raw_error(
        &self,
        sequence: SequenceNumber,
    ) -> Result<Option<Self::Buf>, ConnectionError> {
        self.observe_health();
        self.complete(self.inner.check_for_raw_error(sequence))
    }

    fn prefetch_maximum_request_bytes(&self) {
        self.observe_health();
        self.inner.prefetch_maximum_request_bytes();
        self.observe_health();
    }

    fn maximum_request_bytes(&self) -> usize {
        self.observe_health();
        let result = self.inner.maximum_request_bytes();
        self.observe_health();
        result
    }

    fn parse_error(&self, error: &[u8]) -> Result<X11Error, ParseError> {
        self.observe_health();
        let result = self.inner.parse_error(error);
        self.observe_health();
        result
    }

    fn parse_event(&self, event: &[u8]) -> Result<Event, ParseError> {
        self.observe_health();
        let result = self.inner.parse_event(event);
        self.observe_health();
        result
    }
}

impl<C: Connection + NativeHealth> Connection for GuardedConnection<C> {
    fn wait_for_raw_event_with_sequence(
        &self,
    ) -> Result<RawEventAndSeqNumber<Self::Buf>, ConnectionError> {
        self.observe_health();
        self.complete(self.inner.wait_for_raw_event_with_sequence())
    }

    fn poll_for_raw_event_with_sequence(
        &self,
    ) -> Result<Option<RawEventAndSeqNumber<Self::Buf>>, ConnectionError> {
        self.observe_health();
        self.complete(self.inner.poll_for_raw_event_with_sequence())
    }

    fn flush(&self) -> Result<(), ConnectionError> {
        self.observe_health();
        self.complete(self.inner.flush())
    }

    fn setup(&self) -> &Setup {
        self.observe_health();
        let result = self.inner.setup();
        self.observe_health();
        result
    }

    fn generate_id(&self) -> Result<u32, ReplyOrIdError> {
        self.observe_health();
        self.complete_id(self.inner.generate_id())
    }
}

#[cfg(test)]
#[path = "guarded_connection_tests.rs"]
mod tests;
