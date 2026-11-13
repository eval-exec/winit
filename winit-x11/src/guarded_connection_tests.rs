use std::cell::{Cell, RefCell};
use std::io::{Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use x11rb::errors::ReplyError;
use x11rb::protocol::ErrorKind;

use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Operation {
    SendReply,
    SendFds,
    SendVoid,
    Reply,
    UncheckedReply,
    ReplyFds,
    Check,
    Discard,
    PrefetchExtension,
    Extension,
    PrefetchMaximum,
    Maximum,
    ParseError,
    ParseEvent,
    WaitEvent,
    PollEvent,
    Flush,
    Setup,
    GenerateId,
}

const OPERATIONS: &[Operation] = &[
    Operation::SendReply,
    Operation::SendFds,
    Operation::SendVoid,
    Operation::Reply,
    Operation::UncheckedReply,
    Operation::ReplyFds,
    Operation::Check,
    Operation::Discard,
    Operation::PrefetchExtension,
    Operation::Extension,
    Operation::PrefetchMaximum,
    Operation::Maximum,
    Operation::ParseError,
    Operation::ParseEvent,
    Operation::WaitEvent,
    Operation::PollEvent,
    Operation::Flush,
    Operation::Setup,
    Operation::GenerateId,
];

#[derive(Clone, Copy, Debug)]
enum Failure {
    Io,
    NonIo(u8),
    PostIo,
}

static CHILD_OPERATIONS: AtomicUsize = AtomicUsize::new(0);
static EXPECT_BEFORE_OPERATION: AtomicBool = AtomicBool::new(false);

struct FakeConnection {
    calls: RefCell<Vec<String>>,
    sequence: Cell<u64>,
    fault: Cell<Option<(Operation, Failure)>>,
    health: Cell<Option<u8>>,
    bad_window: Cell<bool>,
    reply_fd: RefCell<Option<OwnedFd>>,
    setup: Setup,
}

impl fmt::Debug for FakeConnection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("FakeConnection").field("calls", &self.calls).finish_non_exhaustive()
    }
}

fn non_io_error(kind: u8) -> ConnectionError {
    match kind {
        0 => ConnectionError::UnknownError,
        1 => ConnectionError::UnsupportedExtension,
        2 => ConnectionError::MaximumRequestLengthExceeded,
        3 => ConnectionError::InsufficientMemory,
        4 => ConnectionError::FdPassingFailed,
        _ => ConnectionError::ParseError(ParseError::InsufficientData),
    }
}

fn io_error() -> ConnectionError {
    std::io::Error::new(std::io::ErrorKind::BrokenPipe, "injected native transport loss").into()
}

impl FakeConnection {
    fn new() -> Self {
        Self {
            calls: RefCell::new(Vec::new()),
            sequence: Cell::new(0),
            fault: Cell::new(None),
            health: Cell::new(None),
            bad_window: Cell::new(false),
            reply_fd: RefCell::new(None),
            setup: Setup::default(),
        }
    }

    fn touch(&self, operation: Operation) -> Result<(), ConnectionError> {
        CHILD_OPERATIONS.fetch_add(1, Ordering::Relaxed);
        self.calls.borrow_mut().push(format!("{operation:?}"));
        if let Some((target, failure)) = self.fault.get() {
            if target == operation {
                match failure {
                    Failure::Io => return Err(io_error()),
                    Failure::NonIo(kind) => return Err(non_io_error(kind)),
                    Failure::PostIo => self.health.set(Some(255)),
                }
            }
        }
        Ok(())
    }

    fn touch_void(&self, operation: Operation) {
        self.touch(operation).expect("use PostIo for an infallible native operation");
    }

    fn next_sequence(&self) -> SequenceNumber {
        let sequence = self.sequence.get() + 1;
        self.sequence.set(sequence);
        sequence
    }

    fn record(&self, value: String) {
        self.calls.borrow_mut().push(value);
    }

    fn arm(&self, operation: Operation, failure: Failure) {
        self.fault.set(Some((operation, failure)));
    }
}

impl NativeHealth for FakeConnection {
    fn native_error(&self) -> Option<ConnectionError> {
        self.health.get().map(|kind| if kind == 255 { io_error() } else { non_io_error(kind) })
    }
}

impl RequestConnection for FakeConnection {
    type Buf = Vec<u8>;

    fn send_request_with_reply<R: TryParse>(
        &self,
        _: &[IoSlice<'_>],
        fds: Vec<RawFdContainer>,
    ) -> Result<Cookie<'_, Self, R>, ConnectionError> {
        self.touch(Operation::SendReply)?;
        self.record(format!("sent-fds:{}", fds.len()));
        Ok(Cookie::new(self, self.next_sequence()))
    }

    fn send_request_with_reply_with_fds<R: TryParseFd>(
        &self,
        _: &[IoSlice<'_>],
        fds: Vec<RawFdContainer>,
    ) -> Result<CookieWithFds<'_, Self, R>, ConnectionError> {
        self.touch(Operation::SendFds)?;
        self.record(format!("sent-fds:{}", fds.len()));
        Ok(CookieWithFds::new(self, self.next_sequence()))
    }

    fn send_request_without_reply(
        &self,
        _: &[IoSlice<'_>],
        fds: Vec<RawFdContainer>,
    ) -> Result<VoidCookie<'_, Self>, ConnectionError> {
        self.touch(Operation::SendVoid)?;
        self.record(format!("sent-fds:{}", fds.len()));
        Ok(VoidCookie::new(self, self.next_sequence()))
    }

    fn discard_reply(&self, sequence: SequenceNumber, kind: RequestKind, mode: DiscardMode) {
        self.touch_void(Operation::Discard);
        self.record(format!("discard:{sequence}:{kind:?}:{mode:?}"));
    }

    fn prefetch_extension_information(&self, _: &'static str) -> Result<(), ConnectionError> {
        self.touch(Operation::PrefetchExtension)
    }

    fn extension_information(
        &self,
        _: &'static str,
    ) -> Result<Option<ExtensionInformation>, ConnectionError> {
        self.touch(Operation::Extension)?;
        Ok(None)
    }

    fn wait_for_reply_or_raw_error(
        &self,
        sequence: SequenceNumber,
    ) -> Result<ReplyOrError<Self::Buf>, ConnectionError> {
        self.touch(Operation::Reply)?;
        self.record(format!("reply:{sequence}"));
        if self.bad_window.get() {
            Ok(ReplyOrError::Error(vec![0; 32]))
        } else {
            Ok(ReplyOrError::Reply(vec![42]))
        }
    }

    fn wait_for_reply(
        &self,
        sequence: SequenceNumber,
    ) -> Result<Option<Self::Buf>, ConnectionError> {
        self.touch(Operation::UncheckedReply)?;
        self.record(format!("unchecked:{sequence}"));
        Ok(Some(vec![42]))
    }

    fn wait_for_reply_with_fds_raw(
        &self,
        sequence: SequenceNumber,
    ) -> Result<ReplyOrError<BufWithFds<Self::Buf>, Self::Buf>, ConnectionError> {
        self.touch(Operation::ReplyFds)?;
        self.record(format!("fds-reply:{sequence}"));
        Ok(ReplyOrError::Reply((vec![42], self.reply_fd.borrow_mut().take().into_iter().collect())))
    }

    fn check_for_raw_error(
        &self,
        sequence: SequenceNumber,
    ) -> Result<Option<Self::Buf>, ConnectionError> {
        self.touch(Operation::Check)?;
        self.record(format!("check:{sequence}"));
        Ok(self.bad_window.get().then(|| vec![0; 32]))
    }

    fn prefetch_maximum_request_bytes(&self) {
        self.touch_void(Operation::PrefetchMaximum);
    }

    fn maximum_request_bytes(&self) -> usize {
        self.touch_void(Operation::Maximum);
        4096
    }

    fn parse_error(&self, bytes: &[u8]) -> Result<X11Error, ParseError> {
        self.touch_void(Operation::ParseError);
        if bytes.len() < 32 {
            return Err(ParseError::InsufficientData);
        }
        Ok(X11Error {
            error_kind: ErrorKind::Window,
            error_code: 3,
            sequence: 1,
            bad_value: u32::MAX,
            minor_opcode: 0,
            major_opcode: 4,
            extension_name: None,
            request_name: Some("DestroyWindow"),
        })
    }

    fn parse_event(&self, _: &[u8]) -> Result<Event, ParseError> {
        self.touch_void(Operation::ParseEvent);
        Err(ParseError::InsufficientData)
    }
}

impl Connection for FakeConnection {
    fn wait_for_raw_event_with_sequence(
        &self,
    ) -> Result<RawEventAndSeqNumber<Self::Buf>, ConnectionError> {
        self.touch(Operation::WaitEvent)?;
        Ok((vec![42], 1))
    }

    fn poll_for_raw_event_with_sequence(
        &self,
    ) -> Result<Option<RawEventAndSeqNumber<Self::Buf>>, ConnectionError> {
        self.touch(Operation::PollEvent)?;
        Ok(None)
    }

    fn flush(&self) -> Result<(), ConnectionError> {
        self.touch(Operation::Flush)
    }

    fn setup(&self) -> &Setup {
        self.touch_void(Operation::Setup);
        &self.setup
    }

    fn generate_id(&self) -> Result<u32, ReplyOrIdError> {
        self.touch(Operation::GenerateId)?;
        Ok(19)
    }
}

#[derive(Debug, PartialEq, Eq)]
struct ByteReply(u8);

impl TryParse for ByteReply {
    fn try_parse(bytes: &[u8]) -> Result<(Self, &[u8]), ParseError> {
        let (first, remaining) = bytes.split_first().ok_or(ParseError::InsufficientData)?;
        Ok((Self(*first), remaining))
    }
}

#[derive(Debug)]
struct FdReply(OwnedFd);

impl TryParseFd for FdReply {
    fn try_parse_fd<'a>(
        bytes: &'a [u8],
        fds: &mut Vec<RawFdContainer>,
    ) -> Result<(Self, &'a [u8]), ParseError> {
        if fds.len() != 1 {
            return Err(ParseError::InsufficientData);
        }
        Ok((Self(fds.remove(0)), bytes))
    }
}

fn unexpected_loss(_: crate::X11ConnectionLoss) -> ! {
    panic!("healthy/control connection entered the loss sink")
}

fn active() -> GuardedConnection<FakeConnection> {
    GuardedConnection::new(FakeConnection::new(), ConnectionLossState::for_test(unexpected_loss))
}

#[test]
fn cookies_transfer_one_reply_or_discard_responsibility() {
    let connection = active();
    let cookie = connection.send_request_with_reply::<ByteReply>(&[], vec![]).unwrap();
    assert_eq!(cookie.sequence_number(), 1);
    assert_eq!(&*connection.inner.calls.borrow(), &["SendReply", "sent-fds:0"]);
    assert_eq!(cookie.reply().unwrap(), ByteReply(42));
    assert_eq!(&*connection.inner.calls.borrow(), &["SendReply", "sent-fds:0", "Reply", "reply:1"]);
    connection.inner.calls.borrow_mut().clear();
    let cookie = connection.send_request_with_reply::<ByteReply>(&[], vec![]).unwrap();
    drop(cookie);
    assert_eq!(
        &*connection.inner.calls.borrow(),
        &["SendReply", "sent-fds:0", "Discard", "discard:2:HasResponse:DiscardReply"]
    );
    connection.inner.calls.borrow_mut().clear();
    connection
        .send_request_with_reply::<ByteReply>(&[], vec![])
        .unwrap()
        .discard_reply_and_errors();
    assert_eq!(
        &*connection.inner.calls.borrow(),
        &["SendReply", "sent-fds:0", "Discard", "discard:3:HasResponse:DiscardReplyAndError"]
    );
    connection.inner.calls.borrow_mut().clear();
    assert_eq!(
        connection
            .send_request_with_reply::<ByteReply>(&[], vec![])
            .unwrap()
            .reply_unchecked()
            .unwrap(),
        Some(ByteReply(42))
    );
    assert_eq!(
        &*connection.inner.calls.borrow(),
        &["SendReply", "sent-fds:0", "UncheckedReply", "unchecked:4"]
    );
    connection.inner.calls.borrow_mut().clear();
    connection.send_request_without_reply(&[], vec![]).unwrap().check().unwrap();
    assert_eq!(&*connection.inner.calls.borrow(), &["SendVoid", "sent-fds:0", "Check", "check:5"]);
    connection.inner.calls.borrow_mut().clear();
    connection.send_request_without_reply(&[], vec![]).unwrap().ignore_error();
    assert_eq!(
        &*connection.inner.calls.borrow(),
        &["SendVoid", "sent-fds:0", "Discard", "discard:6:IsVoid:DiscardReplyAndError"]
    );
    connection.inner.calls.borrow_mut().clear();
    drop(connection.send_request_without_reply(&[], vec![]).unwrap());
    assert_eq!(
        &*connection.inner.calls.borrow(),
        &["SendVoid", "sent-fds:0", "Discard", "discard:7:IsVoid:DiscardReply"]
    );
    connection.inner.calls.borrow_mut().clear();
    drop(connection.send_request_with_reply_with_fds::<FdReply>(&[], vec![]).unwrap());
    assert_eq!(
        &*connection.inner.calls.borrow(),
        &["SendFds", "sent-fds:0", "Discard", "discard:8:HasResponse:DiscardReply"]
    );
}

#[test]
fn fd_cookies_preserve_reply_and_outgoing_fd_ownership() {
    let connection = active();
    let (mut writer, reader) = UnixStream::pair().unwrap();
    writer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    *connection.inner.reply_fd.borrow_mut() = Some(reader.into());
    let cookie = connection.send_request_with_reply_with_fds::<FdReply>(&[], vec![]).unwrap();
    assert_eq!(&*connection.inner.calls.borrow(), &["SendFds", "sent-fds:0"]);
    let FdReply(fd) = cookie.reply().unwrap();
    let mut delivered = UnixStream::from(fd);
    writer.write_all(b"x").unwrap();
    let mut byte = [0];
    delivered.read_exact(&mut byte).unwrap();
    assert_eq!(&byte, b"x");
    drop(delivered);
    assert_eq!(writer.read(&mut byte).unwrap(), 0, "reply FD leaked after ownership ended");
    assert_eq!(
        &*connection.inner.calls.borrow(),
        &["SendFds", "sent-fds:0", "ReplyFds", "fds-reply:1"]
    );
    let (mut peer, outgoing) = UnixStream::pair().unwrap();
    peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    connection.send_request_without_reply(&[], vec![outgoing.into()]).unwrap().check().unwrap();
    assert_eq!(peer.read(&mut byte).unwrap(), 0, "outgoing FD was not consumed by native send");
    assert!(connection.inner.calls.borrow().iter().any(|call| call == "sent-fds:1"));
}

#[test]
fn healthy_protocol_parse_and_non_io_errors_remain_unchanged() {
    let connection = active();
    assert_eq!(connection.poll_for_raw_event_with_sequence().unwrap(), None);
    assert!(connection.extension_information("MISSING").unwrap().is_none());
    assert_eq!(connection.maximum_request_bytes(), 4096);
    connection.prefetch_maximum_request_bytes();
    connection.inner.bad_window.set(true);
    let error =
        connection.send_request_with_reply::<ByteReply>(&[], vec![]).unwrap().reply().unwrap_err();
    assert!(
        matches!(error, ReplyError::X11Error(ref error) if error.error_kind == ErrorKind::Window)
    );
    let error = connection.send_request_without_reply(&[], vec![]).unwrap().check().unwrap_err();
    assert!(matches!(error, ReplyError::X11Error(ref error) if error.error_code == 3));
    assert!(matches!(connection.parse_event(&[]), Err(ParseError::InsufficientData)));
    assert!(matches!(connection.parse_error(&[]), Err(ParseError::InsufficientData)));
    for kind in 0..=5 {
        connection.inner.arm(Operation::Flush, Failure::NonIo(kind));
        let actual = connection.flush().unwrap_err();
        assert_eq!(format!("{actual:?}"), format!("{:?}", non_io_error(kind)));
        // A native health state with a non-I/O error must not replace a healthy
        // result or get classified as a transport loss.
        connection.inner.health.set(Some(kind));
        assert_eq!(connection.maximum_request_bytes(), 4096);
        connection.inner.health.set(None);
    }
}

#[test]
fn inactive_transport_errors_return_without_a_sink() {
    let connection =
        GuardedConnection::new(FakeConnection::new(), ConnectionLossState::for_test_inactive());
    connection.inner.arm(Operation::Flush, Failure::Io);
    assert!(matches!(connection.flush(), Err(ConnectionError::IoError(_))));
    connection.inner.arm(Operation::SendReply, Failure::Io);
    assert!(matches!(
        connection.send_request_with_reply::<ByteReply>(&[], vec![]),
        Err(ConnectionError::IoError(_))
    ));
    connection.inner.arm(Operation::Check, Failure::Io);
    assert!(matches!(connection.check_for_raw_error(1), Err(ConnectionError::IoError(_))));
    connection.inner.arm(Operation::Maximum, Failure::PostIo);
    assert_eq!(connection.maximum_request_bytes(), 4096);
}

fn child_loss(_: crate::X11ConnectionLoss) -> ! {
    if EXPECT_BEFORE_OPERATION.load(Ordering::Relaxed)
        && CHILD_OPERATIONS.load(Ordering::Relaxed) != 0
    {
        std::process::exit(85);
    }
    println!("GUARDED_CONNECTION_NATIVE_CHILD_LOSS");
    std::process::exit(86)
}

fn run_native_child(scenario: &str, operation: Operation) {
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "guarded_connection::tests::native_child_entry", "--nocapture"])
        .env("WINIT_X11_GUARDED_CHILD", format!("{scenario}:{operation:?}"))
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert_eq!(
                status.code(),
                Some(86),
                "{scenario}:{operation:?} escaped the loss boundary"
            );
            break;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("native child timed out: {scenario}:{operation:?}");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn native_transport_errors_enter_loss_before_expect() {
    for operation in OPERATIONS {
        if !matches!(
            operation,
            Operation::Discard
                | Operation::PrefetchMaximum
                | Operation::Maximum
                | Operation::ParseError
                | Operation::ParseEvent
                | Operation::Setup
        ) {
            run_native_child("io", *operation);
        }
    }
}

#[test]
fn native_success_and_void_operations_detect_post_call_loss() {
    for operation in OPERATIONS {
        run_native_child("post", *operation);
    }
}

#[test]
fn native_preflight_stops_an_already_failed_inner() {
    run_native_child("pre", Operation::Flush);
}

#[test]
fn native_child_entry() {
    let Ok(scenario) = std::env::var("WINIT_X11_GUARDED_CHILD") else { return };
    let (kind, name) = scenario.split_once(':').unwrap();
    let operation = *OPERATIONS.iter().find(|candidate| format!("{candidate:?}") == name).unwrap();
    let connection =
        GuardedConnection::new(FakeConnection::new(), ConnectionLossState::for_test(child_loss));
    if kind == "pre" {
        EXPECT_BEFORE_OPERATION.store(true, Ordering::Relaxed);
        connection.inner.health.set(Some(255));
    } else {
        connection.inner.arm(operation, if kind == "post" { Failure::PostIo } else { Failure::Io });
    }
    match operation {
        Operation::SendReply => {
            connection
                .send_request_with_reply::<ByteReply>(&[], vec![])
                .expect("send transport escaped");
        },
        Operation::SendFds => {
            connection
                .send_request_with_reply_with_fds::<FdReply>(&[], vec![])
                .expect("FD send transport escaped");
        },
        Operation::SendVoid => {
            connection
                .send_request_without_reply(&[], vec![])
                .expect("void send transport escaped");
        },
        Operation::Reply => {
            connection
                .send_request_with_reply::<ByteReply>(&[], vec![])
                .unwrap()
                .reply()
                .expect("reply transport escaped");
        },
        Operation::UncheckedReply => {
            connection
                .send_request_with_reply::<ByteReply>(&[], vec![])
                .unwrap()
                .reply_unchecked()
                .expect("unchecked transport escaped");
        },
        Operation::ReplyFds => {
            connection
                .send_request_with_reply_with_fds::<FdReply>(&[], vec![])
                .unwrap()
                .raw_reply()
                .expect("FD reply transport escaped");
        },
        Operation::Check => {
            connection
                .send_request_without_reply(&[], vec![])
                .unwrap()
                .check()
                .expect("void check transport escaped");
        },
        Operation::Discard => {
            connection.send_request_without_reply(&[], vec![]).unwrap().ignore_error();
        },
        Operation::PrefetchExtension => {
            connection.prefetch_extension_information("TEST").expect("prefetch transport escaped");
        },
        Operation::Extension => {
            connection.extension_information("TEST").expect("extension transport escaped");
        },
        Operation::PrefetchMaximum => connection.prefetch_maximum_request_bytes(),
        Operation::Maximum => {
            connection.maximum_request_bytes();
        },
        Operation::ParseError => {
            let _ = connection.parse_error(&[0; 32]);
        },
        Operation::ParseEvent => {
            let _ = connection.parse_event(&[]);
        },
        Operation::WaitEvent => {
            connection.wait_for_raw_event_with_sequence().expect("event transport escaped");
        },
        Operation::PollEvent => {
            connection.poll_for_raw_event_with_sequence().expect("poll transport escaped");
        },
        Operation::Flush => {
            connection.flush().expect("flush transport escaped");
        },
        Operation::Setup => {
            connection.setup();
        },
        Operation::GenerateId => {
            connection.generate_id().expect("ID transport escaped");
        },
    }
    // Both ordinary successful continuation and an unconsumed transport error
    // are failures. Only the registered nonreturning callback can exit86.
    std::process::exit(87)
}
