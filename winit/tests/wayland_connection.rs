//! Explicit-connection selection and failed-initialization recovery.
#![cfg(all(target_os = "linux", feature = "wayland"))]

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::os::unix::net::UnixStream;

use winit::error::EventLoopError;
use winit::event_loop::{EventLoop, EventLoopBuilder};
use winit::platform::wayland::{
    EventLoopBuilderExtWayland, EventLoopExtWayland, WaylandConnection,
};

fn disconnected_connection() -> WaylandConnection {
    let (client, server) = UnixStream::pair().unwrap();
    drop(server);
    WaylandConnection::from_socket(client).unwrap()
}

fn hash(builder: &EventLoopBuilder) -> u64 {
    let mut state = DefaultHasher::new();
    builder.hash(&mut state);
    state.finish()
}

#[test]
fn builder_connection_identity_preserves_equality_and_hashing() {
    let connection = disconnected_connection();
    let mut first = EventLoop::builder();
    let mut second = EventLoop::builder();
    first.with_wayland_connection(connection.clone());
    second.with_wayland_connection(connection);
    assert_eq!(first, second);
    assert_eq!(hash(&first), hash(&second));
    second.with_wayland_connection(disconnected_connection());
    assert_ne!(first, second);
    assert_ne!(first, EventLoop::builder());
}

#[test]
fn failed_explicit_initialization_can_retry_without_relaxing_thread_policy() {
    let mut builder = EventLoop::builder();
    builder.with_wayland_connection(disconnected_connection());
    // Rust test functions run off the OS main thread. Connection injection must
    // not bypass the ordinary platform check, or consume the creation allowance.
    assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| builder.build())).is_err());
    builder.with_any_thread(true);
    for _ in 0..2 {
        let error = builder.build().unwrap_err();
        assert!(matches!(error, EventLoopError::Os(_)), "unexpected error: {error}");
        builder.with_wayland_connection(disconnected_connection());
    }
}

#[test]
fn environment_connection_keeps_native_one_shot_failure_policy() {
    const CHILD: &str = "WINIT_TEST_DEFAULT_CONNECTION_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "environment_connection_keeps_native_one_shot_failure_policy"])
            .env(CHILD, "1")
            .env("WAYLAND_DISPLAY", "/nonexistent/winit-test-wayland")
            .env_remove("WAYLAND_SOCKET")
            .env_remove("DISPLAY")
            .status()
            .unwrap();
        assert!(status.success());
        return;
    }
    let mut builder = EventLoop::builder();
    builder.with_wayland().with_any_thread(true);
    assert!(matches!(builder.build(), Err(EventLoopError::Os(_))));
    assert!(matches!(builder.build(), Err(EventLoopError::RecreationAttempt)));
    builder.with_wayland_connection(disconnected_connection());
    assert!(matches!(builder.build(), Err(EventLoopError::RecreationAttempt)));
}

#[test]
#[ignore = "requires a private compositor socket in WINIT_TEST_WAYLAND_SOCKET"]
fn failed_connection_then_real_explicit_display_and_permanent_success_guard() {
    use winit::raw_window_handle::{HasDisplayHandle, RawDisplayHandle};

    let socket = std::env::var_os("WINIT_TEST_WAYLAND_SOCKET")
        .expect("select an isolated compositor explicitly");
    let mut builder = EventLoop::builder();
    builder.with_any_thread(true).with_wayland_connection(disconnected_connection());
    assert!(matches!(builder.build(), Err(EventLoopError::Os(_))));

    let connection = WaylandConnection::from_socket(UnixStream::connect(socket).unwrap()).unwrap();
    let display = connection.backend().display_ptr();
    builder.with_wayland_connection(connection);
    let event_loop = builder.build().unwrap();
    assert!(event_loop.is_wayland());
    let handle = event_loop.owned_display_handle();
    let RawDisplayHandle::Wayland(raw) = handle.display_handle().unwrap().as_raw() else {
        panic!("explicit connection selected another backend");
    };
    assert_eq!(raw.display.as_ptr(), display.cast());
    assert!(matches!(builder.build(), Err(EventLoopError::RecreationAttempt)));
    drop(handle);
    drop(event_loop);
    assert!(matches!(builder.build(), Err(EventLoopError::RecreationAttempt)));
}
