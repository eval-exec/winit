# Neomacs integration fork

`eval-exec/winit` uses `master` as its maintained Neomacs integration branch.
Keep upstream history and the required platform fixes together here; Neomacs
pins an exact commit from this branch. Topic branches are not separate release
channels. The local `origin` remote is upstream `rust-windowing/winit`; `fork`
is `eval-exec/winit`.

## Native input timestamps

The device input events (`KeyboardInput`, `PointerMoved`, `PointerButton`,
`MouseWheel`) now carry `timestamp: Option<EventTimestamp>`, and
`WindowEvent::timestamp()` returns it. `EventTimestamp { ms, clock }` is the
window system's value passed through unchanged, in milliseconds:

| Backend | `EventClock` | Source |
|---|---|---|
| X11 | `X11Server` | core `xev.time` / XI2 `XIDeviceEvent.time` (server ms) |
| Wayland | `WaylandSeat` | SCTK pointer/keyboard/touch `time` (`wl_pointer.time`, `wl_keyboard.key`, `wl_touch`) |
| Windows | `WindowsMessage` | `GetMessageTime()` (`MSG.time`) |
| macOS | `MacUptime` | `NSEvent.timestamp * 1000`, exactly GNU's `EV_TIMESTAMP` |

`None` means the backend has no native time for the event: winit-synthesized
events (focus key synthesis, X11 pressed-key replay, Wayland key-repeat
timers), IME events, the Wayland tablet protocol, and the
Android/iOS/web/orbital backends. Zero is a real timestamp in server-relative
clocks, so consumers must treat `None`, not zero, as "unknown".

Neomacs consumes these values for GNU's event-time semantics (double-click and
drag classification, Lisp `posn-timestamp`).

Verification:

```sh
cargo nextest run -p winit-core
cargo check -p winit --tests --target x86_64-unknown-linux-gnu
cargo check -p winit-x11 -p winit-wayland --target x86_64-unknown-linux-gnu
cargo check -p winit-win32 --target x86_64-pc-windows-msvc
cargo check -p winit-uikit --target aarch64-apple-ios
cargo check -p winit-web --target wasm32-unknown-unknown
cargo check -p winit-orbital --target x86_64-unknown-linux-gnu
```

Android needs an NDK toolchain (`--features game-activity`), so it is checked
only on the Android builder.

## Wayland popup lifetime

Base: upstream `a98b2b217c901f8776a2bdb4ebc35ea7611998ce`.

Window creation can roundtrip Wayland from an application event callback.
That can enqueue compositor updates for a popup which is removed later in
the same iteration, after the dispatch batch was drained. The next iteration
used to unwrap the missing window while processing its scale update.

Retirement now removes that window's pending compositor, synthetic, and input
events before delivering `Destroyed`. Dispatch also filters absent windows,
including late events from handles whose native window was closed by its
parent. Device events and events for live windows keep their ordering.

CryZe reported the same failure and suggested a dispatch guard during
[upstream popup review](https://github.com/rust-windowing/winit/pull/4543#issuecomment-4692260604).
This patch additionally retires pending queues at native destruction.
No upstream PR is being submitted for this fork.

## Verification

```sh
cargo nextest run -p winit -p winit-wayland \
  --features softbuffer/wayland,softbuffer/wayland-dlopen --run-ignored all
```

The ignored native stress test requires a live Linux Wayland compositor and
creates temporary windows. It checks repeated resize-callback replacement
and absence of events after destruction. This simpler softbuffer stress test
also passes on the unpatched backend; it is supplementary coverage, not the
exact panic reproducer.

The red-capable GPU reproducer lives in Neomacs:

```sh
RUST_LOG=warn cargo nextest run -p neomacs-display-runtime \
  --run-ignored only -E 'test(linux_wayland_native_menu_replacement_stress)'
```

That test deliberately commits popups inside resize callbacks, independently
of Neomacs's production post-event scheduling. It panicked in the original
backend at `event_loop/mod.rs:386:58` and passed with this patch. Both projects
use `cargo nextest`.

## Android named keys and modifiers

Android's character map can report LF for Enter. Preserve the named Enter
identity before Unicode lookup, keeping it distinct from an actual Ctrl-J.
The same rule applies to other named control keys. Publish each changed
Android modifier sample before its keyboard event, and clear modifiers when
focus is lost.

The Enter regression failed on a Redmi K20 Pro (Android 12), returning
`Character("\n")` instead of `Named(Enter)`. After the fix, all three Android
key translation tests passed on-device, including Ctrl-J distinction and
Ctrl+Shift modifier sampling. These are backend tests, not a claim that
Neomacs's complete Android input/IME integration is finished.

Run the tests with an Android NDK toolchain and an ADB runner configured:

```sh
cargo nextest run --target aarch64-linux-android -p winit-android \
  --features game-activity --lib -E 'test(/keycodes::tests/)' --test-threads 1
```
