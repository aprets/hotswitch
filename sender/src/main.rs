use core_foundation::{base::TCFType, boolean::CFBoolean, runloop::CFRunLoop, string::CFString};
use core_graphics::{
    display::CGDisplay,
    event::{
        CallbackResult, CGEvent, CGEventFlags, CGEventTap, CGEventTapLocation, CGEventTapOptions,
        CGEventTapPlacement, CGEventTapProxy, CGEventType, EventField,
    },
};
use hotswitch_proto::Event;
use std::{
    collections::HashSet,
    ffi::c_void,
    net::UdpSocket,
    ptr,
    sync::atomic::{AtomicBool, AtomicPtr, Ordering},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

// CoreGraphics / CoreGraphicsServer functions not exposed by the crate
extern "C" {
    fn CGAssociateMouseAndMouseCursorPosition(connected: bool) -> i32;
    fn CGEventTapEnable(tap: *mut c_void, enable: bool);
    fn _CGSDefaultConnection() -> i32;
    fn CGSSetConnectionProperty(connection: i32, target: i32, key: *const c_void, value: *const c_void) -> i32;
}

/// Hotkey: Ctrl + Escape (matching the plan)
const HOTKEY_KEYCODE: u16 = 0x35; // kVK_Escape
const HOTKEY_REQUIRES_CTRL: bool = true;

/// Mouse button mapping: CGEvent button number → our protocol button index
/// 0 = left, 1 = right, 2 = middle, 3 = back, 4 = forward
fn map_mouse_button(event_type: &CGEventType, ev: &CGEvent) -> Option<(u8, bool)> {
    match event_type {
        CGEventType::LeftMouseDown => Some((0, true)),
        CGEventType::LeftMouseUp => Some((0, false)),
        CGEventType::RightMouseDown => Some((1, true)),
        CGEventType::RightMouseUp => Some((1, false)),
        CGEventType::OtherMouseDown | CGEventType::OtherMouseUp => {
            let btn = ev.get_integer_value_field(EventField::MOUSE_EVENT_BUTTON_NUMBER);
            let mapped = match btn {
                2 => 2,  // middle
                3 => 3,  // back
                4 => 4,  // forward
                n => n as u8,
            };
            let pressed = matches!(event_type, CGEventType::OtherMouseDown);
            Some((mapped, pressed))
        }
        _ => None,
    }
}

fn main() {
    // TODO: read from config.toml
    let target_addr = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "10.0.0.100:24801".to_string());

    let socket = UdpSocket::bind("0.0.0.0:0").expect("Failed to bind UDP socket");
    socket
        .connect(&target_addr)
        .expect("Failed to connect UDP socket");
    socket.set_nonblocking(true).ok();

    // Allow CGDisplayHideCursor to work from a background/terminal process (private CGS API, used by Barrier and lan-mouse)
    unsafe {
        let conn = _CGSDefaultConnection();
        let key = CFString::new("SetsCursorInBackground");
        let val = CFBoolean::true_value();
        if CGSSetConnectionProperty(conn, conn, key.as_CFTypeRef(), val.as_CFTypeRef()) != 0 {
            eprintln!("WARNING: Failed to set SetsCursorInBackground — cursor may not hide");
        }
    }

    println!("hotswitch sender → {target_addr}");
    println!("Press Ctrl+Escape to toggle capture");

    let capturing = Arc::new(AtomicBool::new(false));
    let held_keys: Arc<std::sync::Mutex<HashSet<u16>>> =
        Arc::new(std::sync::Mutex::new(HashSet::new()));

    // Fractional mouse delta accumulators (Moonlight-style)
    let accum_dx = Arc::new(std::sync::Mutex::new(0.0f64));
    let accum_dy = Arc::new(std::sync::Mutex::new(0.0f64));

    // Clone references for the event tap callback
    let cap = capturing.clone();
    let sock = socket.try_clone().expect("Failed to clone socket");
    let keys = held_keys.clone();
    let adx = accum_dx.clone();
    let ady = accum_dy.clone();
    let tap_port: Arc<AtomicPtr<c_void>> = Arc::new(AtomicPtr::new(ptr::null_mut()));
    let tp = tap_port.clone();

    let cg_events_of_interest: Vec<CGEventType> = vec![
        CGEventType::LeftMouseDown,
        CGEventType::LeftMouseUp,
        CGEventType::RightMouseDown,
        CGEventType::RightMouseUp,
        CGEventType::OtherMouseDown,
        CGEventType::OtherMouseUp,
        CGEventType::MouseMoved,
        CGEventType::LeftMouseDragged,
        CGEventType::RightMouseDragged,
        CGEventType::OtherMouseDragged,
        CGEventType::ScrollWheel,
        CGEventType::KeyDown,
        CGEventType::KeyUp,
        CGEventType::FlagsChanged,
    ];

    let event_tap_callback =
        move |_proxy: CGEventTapProxy, event_type: CGEventType, cg_ev: &CGEvent| -> CallbackResult {
            if matches!(
                event_type,
                CGEventType::TapDisabledByTimeout | CGEventType::TapDisabledByUserInput
            ) {
                eprintln!("WARNING: CGEventTap was disabled, re-enabling");
                let port = tp.load(Ordering::SeqCst);
                if !port.is_null() {
                    unsafe { CGEventTapEnable(port, true); }
                }
                return CallbackResult::Keep;
            }

            // Check for hotkey (Ctrl + Escape)
            if matches!(event_type, CGEventType::KeyDown) {
                let keycode = cg_ev.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE) as u16;
                if keycode == HOTKEY_KEYCODE {
                    let flags = cg_ev.get_flags();
                    let ctrl_held = flags.contains(CGEventFlags::CGEventFlagControl);
                    if ctrl_held || !HOTKEY_REQUIRES_CTRL {
                        let was_capturing = cap.load(Ordering::SeqCst);
                        let now_capturing = !was_capturing;
                        cap.store(now_capturing, Ordering::SeqCst);

                        if now_capturing {
                            unsafe { CGAssociateMouseAndMouseCursorPosition(false); }
                            let _ = CGDisplay::hide_cursor(&CGDisplay::main());
                            // Reset accumulators
                            *adx.lock().unwrap() = 0.0;
                            *ady.lock().unwrap() = 0.0;
                            eprintln!("CAPTURE ON → sending input to remote");
                        } else {
                            unsafe { CGAssociateMouseAndMouseCursorPosition(true); }
                            let _ = CGDisplay::show_cursor(&CGDisplay::main());
                            // Release all held keys on the remote
                            let held: Vec<u16> = keys.lock().unwrap().drain().collect();
                            let mut buf = [0u8; 64];
                            for keycode in held {
                                let evt = Event::Key { keycode, pressed: false };
                                let len = evt.to_bytes(&mut buf);
                                let _ = sock.send(&buf[..len]);
                            }
                            eprintln!("CAPTURE OFF → input back to Mac");
                        }
                        return CallbackResult::Drop;
                    }
                }
            }

            if !cap.load(Ordering::SeqCst) {
                return CallbackResult::Keep;
            }

            // We are capturing — serialize and send the event, then swallow it
            let mut buf = [0u8; 64];

            match event_type {
                // Mouse movement
                CGEventType::MouseMoved
                | CGEventType::LeftMouseDragged
                | CGEventType::RightMouseDragged
                | CGEventType::OtherMouseDragged => {
                    let raw_dx = cg_ev.get_double_value_field(EventField::MOUSE_EVENT_DELTA_X);
                    let raw_dy = cg_ev.get_double_value_field(EventField::MOUSE_EVENT_DELTA_Y);

                    // Accumulate fractional deltas (Moonlight approach)
                    let mut ax = adx.lock().unwrap();
                    let mut ay = ady.lock().unwrap();
                    *ax += raw_dx;
                    *ay += raw_dy;

                    let dx = (*ax).clamp(i16::MIN as f64, i16::MAX as f64) as i16;
                    let dy = (*ay).clamp(i16::MIN as f64, i16::MAX as f64) as i16;
                    *ax -= dx as f64;
                    *ay -= dy as f64;

                    if dx != 0 || dy != 0 {
                        let evt = Event::MouseMove { dx, dy };
                        let len = evt.to_bytes(&mut buf);
                        let _ = sock.send(&buf[..len]);
                    }
                }

                // Mouse buttons
                CGEventType::LeftMouseDown
                | CGEventType::LeftMouseUp
                | CGEventType::RightMouseDown
                | CGEventType::RightMouseUp
                | CGEventType::OtherMouseDown
                | CGEventType::OtherMouseUp => {
                    if let Some((button, pressed)) = map_mouse_button(&event_type, cg_ev) {
                        let evt = Event::MouseButton { button, pressed };
                        let len = evt.to_bytes(&mut buf);
                        let _ = sock.send(&buf[..len]);
                    }
                }

                // Scroll wheel
                CGEventType::ScrollWheel => {
                    let v = cg_ev
                        .get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_1)
                        as i16;
                    let h = cg_ev
                        .get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_2)
                        as i16;
                    if v != 0 || h != 0 {
                        let evt = Event::Scroll { dx: h, dy: v };
                        let len = evt.to_bytes(&mut buf);
                        let _ = sock.send(&buf[..len]);
                    }
                }

                // Keyboard
                CGEventType::KeyDown => {
                    let keycode =
                        cg_ev.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE) as u16;
                    keys.lock().unwrap().insert(keycode);
                    let evt = Event::Key {
                        keycode,
                        pressed: true,
                    };
                    let len = evt.to_bytes(&mut buf);
                    let _ = sock.send(&buf[..len]);
                }
                CGEventType::KeyUp => {
                    let keycode =
                        cg_ev.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE) as u16;
                    keys.lock().unwrap().remove(&keycode);
                    let evt = Event::Key {
                        keycode,
                        pressed: false,
                    };
                    let len = evt.to_bytes(&mut buf);
                    let _ = sock.send(&buf[..len]);
                }
                CGEventType::FlagsChanged => {
                    let keycode =
                        cg_ev.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE) as u16;
                    let flags = cg_ev.get_flags();
                    let mut k = keys.lock().unwrap();
                    let pressed = match keycode {
                        0x38 | 0x3C => flags.contains(CGEventFlags::CGEventFlagShift),
                        0x3B | 0x3E => flags.contains(CGEventFlags::CGEventFlagControl),
                        0x3A | 0x3D => flags.contains(CGEventFlags::CGEventFlagAlternate),
                        0x37 | 0x36 => flags.contains(CGEventFlags::CGEventFlagCommand),
                        0x39 => flags.contains(CGEventFlags::CGEventFlagAlphaShift),
                        _ => !k.contains(&keycode),
                    };
                    if pressed {
                        k.insert(keycode);
                    } else {
                        k.remove(&keycode);
                    }
                    drop(k);
                    let evt = Event::Key { keycode, pressed };
                    let len = evt.to_bytes(&mut buf);
                    let _ = sock.send(&buf[..len]);
                }

                _ => {}
            }

            CallbackResult::Drop
        };

    let tap = CGEventTap::new(
        CGEventTapLocation::Session,
        CGEventTapPlacement::HeadInsertEventTap,
        CGEventTapOptions::Default,
        cg_events_of_interest,
        event_tap_callback,
    )
    .expect("Failed to create CGEventTap — is Accessibility permission granted?");

    tap_port.store(tap.mach_port().as_CFTypeRef() as *mut c_void, Ordering::SeqCst);

    let source = tap
        .mach_port()
        .create_runloop_source(0)
        .expect("Failed to create runloop source");

    unsafe {
        CFRunLoop::get_current().add_source(&source, core_foundation::runloop::kCFRunLoopCommonModes);
    }

    // Receiver heartbeat listener — polls the same (nonblocking) socket for reply heartbeats
    let sock_rx = socket.try_clone().expect("Failed to clone socket");
    thread::spawn(move || {
        let mut buf = [0u8; 8];
        let mut was_connected = false;
        let mut last_recv = Instant::now();
        eprintln!("waiting for receiver...");
        loop {
            match sock_rx.recv(&mut buf) {
                Ok(n) if Event::from_bytes(&buf[..n]).is_some() => {
                    if !was_connected {
                        eprintln!("receiver connected");
                        was_connected = true;
                    }
                    last_recv = Instant::now();
                }
                _ => {}
            }
            if was_connected && last_recv.elapsed().as_secs() > 3 {
                eprintln!("WARNING: receiver disconnected");
                was_connected = false;
            }
            thread::sleep(Duration::from_millis(500));
        }
    });

    // Heartbeat + key sync thread
    let cap2 = capturing.clone();
    let keys2 = held_keys.clone();
    let sock2 = socket.try_clone().expect("Failed to clone socket");
    thread::spawn(move || {
        let mut buf = [0u8; 512];
        let mut tick = 0u32;
        loop {
            thread::sleep(Duration::from_millis(100));
            tick += 1;

            if cap2.load(Ordering::SeqCst) {
                // Key sync every 100ms
                let held: Vec<u16> = keys2.lock().unwrap().iter().copied().collect();
                let evt = Event::KeySync { keys: held };
                let len = evt.to_bytes(&mut buf);
                let _ = sock2.send(&buf[..len]);
            }

            // Heartbeat every 1s (every 10th tick)
            if tick % 10 == 0 {
                let evt = Event::Heartbeat;
                let len = evt.to_bytes(&mut buf);
                let _ = sock2.send(&buf[..len]);
            }
        }
    });

    println!("Event tap running. Ctrl+C to quit.");
    CFRunLoop::run_current();
}
