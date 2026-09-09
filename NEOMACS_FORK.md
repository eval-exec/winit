# Neomacs Wayland popup lifetime patch

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
use `cargo nextest`; no other platform has been runtime-verified here.
