use super::*;

#[test]
fn retiring_a_window_preserves_other_windows_and_device_events() {
    let retired = WindowId::from_raw(1);
    let live = WindowId::from_raw(2);
    let mut sink = EventSink::new();
    sink.push_window_event(WindowEvent::CloseRequested, retired);
    sink.push_window_event(WindowEvent::Focused(true), live);
    sink.push_device_event(DeviceEvent::PointerMotion { delta: (3.0, 4.0) });
    sink.push_window_event(WindowEvent::Focused(false), retired);
    sink.push_window_event(WindowEvent::CloseRequested, live);

    sink.remove_window(retired);
    // Retirement is idempotent, including an ID not present in the queue.
    sink.remove_window(retired);
    sink.remove_window(WindowId::from_raw(3));
    let events: Vec<_> = sink.drain().collect();
    assert!(matches!(events.as_slice(), [
        Event::WindowEvent { window_id: first, event: WindowEvent::Focused(true) },
        Event::DeviceEvent { event: DeviceEvent::PointerMotion { delta: (3.0, 4.0) } },
        Event::WindowEvent { window_id: last, event: WindowEvent::CloseRequested },
    ] if *first == live && *last == live));
    assert!(sink.is_empty());
}
