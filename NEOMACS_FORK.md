# Neomacs integration fork

`eval-exec/winit` uses `master` as its maintained Neomacs integration branch.
Keep upstream history and the required platform fixes together here; Neomacs
pins an exact commit from this branch. Topic branches are not separate release
channels. The local `origin` remote is upstream `rust-windowing/winit`; `fork`
is `eval-exec/winit`.

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

## X11 established-connection loss

Neomacs opts into one terminal-loss policy after normal graphical readiness.
XCB send/reply/check/discard/event/flush operations and Xlib fatal I/O converge
on that same connection generation before a local expect can turn transport
loss into a Rust panic. Healthy protocol errors, parsing errors, resource
failures and programmer invariants retain their existing policies. Request
cookies remain bound to the guarded connection through reply/check/drop;
transferring a cookie does not discard its request or leak received FDs.
This backend handoff is separate from Neomacs's evaluator-owned shutdown and
nonlocal Flow policy, and does not itself implement GNU's cleanup/exit result.

The process-global native I/O gate is a deliberately narrow exception approved
for this integration. Its entire payload is a fixed bounded set of atomic
Display addresses, the exact prior global I/O-handler function pointer, and
installation phase. It contains no Box, Arc, Rust/application owner, callback
object, user_data pointer or native-to-owner lookup. Its matching-address
branch merely returns to Xlib so Xlib can invoke that Display's own exit
handler. Unknown/retired addresses chain to the exact saved prior handler.
The short installer handshake performs no X I/O or application/display-lock
lookup. It is not permission for a general process-global owner registry.

The nonreturning Rust policy is owned ONLY through Xlib's per-Display exit
handler user_data. Binding occurs immediately after successful XOpenDisplay,
before requests/exposure to another thread; owner activation occurs later,
after the application's readiness and urgent publication are established.
The user_data allocation remains process-live for in-flight foreign callbacks;
normal retirement removes its native address BEFORE XCloseDisplay, preventing
address reuse from inheriting an old policy. Token retirement cannot cancel
an epoch that already entered Lost. A future reconnecting backend must revisit
this explicit lifetime model rather than accumulate retained owner allocations.
Callbacks may retain native/caller locks: they neither unwind through C nor
issue another X request, run Lisp/GUI cleanup or join a native reader. The
application owner must prove its own completion dependencies independently.

Xlib's default global I/O handler exits before a per-Display exit callback,
so installing only the latter is insufficient. XSetIOErrorExitHandler is
optional: resolve it from the SAME already-loaded Xlib mapping identified by
XOpenDisplay (dladdr plus RTLD_NOLOAD/dlsym), without a link-time dependency,
an alternate library or an RTLD_DEFAULT search. If unavailable, leave prior
native policy intact and refuse ownership with LossNotOwned. Startup, no-hook
and unknown auxiliary/driver-owned displays do not acquire an application
owner implicitly. These rules follow the inspected libX11 1.8.13 release
(XlibInt.c _XIOError; ErrHndlr.c XSetIOErrorHandler and
XSetIOErrorExitHandler), including its fatal user-level display reservation.

The gate saves its prior function once. Another library replacing the global
I/O handler after preparation is conflicting native policy ownership and is
unsupported; this fork does not repeatedly overwrite it or claim coexistence
without a new reviewed protocol. Existing protocol-error hooks remain a
different interface and preserve their behavior.

Pure adapter/state/source checks run with cargo nextest on the fork. Real
foreign reader/drain/BadWindow and paired GNU/Neomacs GUI-loss cases require
owned authenticated Xvfb and finite process/memory scopes in Neomacs. A
standalone callback-parking model proves only that model's mechanics; it is
not evaluator liveness, pending-reply cancellation, nonlocal Flow, or GUI
compatibility evidence. Quote only finished named test logs for validation.
