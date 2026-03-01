use hotswitch_proto::{keymap, Event};
use std::collections::HashSet;
use std::net::UdpSocket;
use std::time::Instant;

#[cfg(windows)]
mod inject {
    use std::ops::BitOrAssign;
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY,
        KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN,
        MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE,
        MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN,
        MOUSEEVENTF_XUP, MOUSEINPUT, SendInput,
    };
    use windows::Win32::UI::WindowsAndMessaging::{XBUTTON1, XBUTTON2};

    fn send_input_safe(input: INPUT) {
        unsafe {
            for attempt in 0..5u32 {
                if SendInput(&[input], std::mem::size_of::<INPUT>() as i32) > 0 {
                    return;
                }
                if attempt < 4 {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
            }
            eprintln!("SendInput failed after 5 attempts");
        }
    }

    fn send_mouse(mi: MOUSEINPUT) {
        send_input_safe(INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 { mi },
        });
    }

    fn send_key(ki: KEYBDINPUT) {
        send_input_safe(INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 { ki },
        });
    }

    pub fn rel_mouse(dx: i32, dy: i32) {
        send_mouse(MOUSEINPUT {
            dx,
            dy,
            mouseData: 0,
            dwFlags: MOUSEEVENTF_MOVE,
            time: 0,
            dwExtraInfo: 0,
        });
    }

    pub fn mouse_button(button: u8, pressed: bool) {
        let dw_flags = match (button, pressed) {
            (0, true) => MOUSEEVENTF_LEFTDOWN,
            (0, false) => MOUSEEVENTF_LEFTUP,
            (1, true) => MOUSEEVENTF_RIGHTDOWN,
            (1, false) => MOUSEEVENTF_RIGHTUP,
            (2, true) => MOUSEEVENTF_MIDDLEDOWN,
            (2, false) => MOUSEEVENTF_MIDDLEUP,
            (3, true) => MOUSEEVENTF_XDOWN,
            (3, false) => MOUSEEVENTF_XUP,
            (4, true) => MOUSEEVENTF_XDOWN,
            (4, false) => MOUSEEVENTF_XUP,
            _ => return,
        };
        let mouse_data = match button {
            3 => XBUTTON1 as u32,
            4 => XBUTTON2 as u32,
            _ => 0,
        };
        send_mouse(MOUSEINPUT {
            dx: 0,
            dy: 0,
            mouseData: mouse_data,
            dwFlags: dw_flags,
            time: 0,
            dwExtraInfo: 0,
        });
    }

    pub fn scroll(dx: i16, dy: i16) {
        // Vertical scroll (dy): positive = scroll up in macOS, which maps to positive WHEEL_DELTA
        if dy != 0 {
            send_mouse(MOUSEINPUT {
                dx: 0,
                dy: 0,
                mouseData: (dy as i32 * 120) as u32,
                dwFlags: MOUSEEVENTF_WHEEL,
                time: 0,
                dwExtraInfo: 0,
            });
        }
        // Horizontal scroll (dx)
        if dx != 0 {
            send_mouse(MOUSEINPUT {
                dx: 0,
                dy: 0,
                mouseData: (dx as i32 * 120) as u32,
                dwFlags: MOUSEEVENTF_HWHEEL,
                time: 0,
                dwExtraInfo: 0,
            });
        }
    }

    pub fn key_event(scancode: u16, extended: bool, pressed: bool) {
        let mut flags = KEYEVENTF_SCANCODE;
        if extended {
            flags.bitor_assign(KEYEVENTF_EXTENDEDKEY);
        }
        if !pressed {
            flags.bitor_assign(KEYEVENTF_KEYUP);
        }
        send_key(KEYBDINPUT {
            wVk: Default::default(),
            wScan: scancode,
            dwFlags: flags,
            time: 0,
            dwExtraInfo: 0,
        });
    }
}

// Stub for compiling on non-Windows (e.g. cargo check on Mac)
#[cfg(not(windows))]
mod inject {
    pub fn rel_mouse(_dx: i32, _dy: i32) {}
    pub fn mouse_button(_button: u8, _pressed: bool) {}
    pub fn scroll(_dx: i16, _dy: i16) {}
    pub fn key_event(_scancode: u16, _extended: bool, _pressed: bool) {}
}

fn main() {
    let listen_addr = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "0.0.0.0:24801".to_string());

    let socket = UdpSocket::bind(&listen_addr).expect("Failed to bind UDP socket");
    println!("hotswitch receiver listening on {listen_addr}");

    let mut buf = [0u8; 512];
    let mut held_keys: HashSet<u16> = HashSet::new(); // CGKeyCodes currently held
    let mut last_heartbeat = Instant::now();

    loop {
        let n = match socket.recv(&mut buf) {
            Ok(n) => n,
            Err(e) => {
                eprintln!("recv error: {e}");
                continue;
            }
        };

        match Event::from_bytes(&buf[..n]) {
            Some(Event::MouseMove { dx, dy }) => {
                inject::rel_mouse(dx as i32, dy as i32);
            }
            Some(Event::MouseButton { button, pressed }) => {
                inject::mouse_button(button, pressed);
            }
            Some(Event::Scroll { dx, dy }) => {
                inject::scroll(dx, dy);
            }
            Some(Event::Key { keycode, pressed }) => {
                if let Some((scancode, extended)) = keymap::cg_to_win_scancode(keycode) {
                    inject::key_event(scancode, extended, pressed);
                    if pressed {
                        held_keys.insert(keycode);
                    } else {
                        held_keys.remove(&keycode);
                    }
                } else {
                    eprintln!("unmapped CGKeyCode: 0x{keycode:02X}");
                }
            }
            Some(Event::KeySync { keys }) => {
                let synced: HashSet<u16> = keys.into_iter().collect();
                // Release keys we think are held but sender says aren't
                for &k in held_keys.difference(&synced) {
                    if let Some((scancode, extended)) = keymap::cg_to_win_scancode(k) {
                        inject::key_event(scancode, extended, false);
                    }
                }
                // Inject key-downs the sender reports held but we missed (dropped packet recovery)
                for &k in synced.difference(&held_keys) {
                    if let Some((scancode, extended)) = keymap::cg_to_win_scancode(k) {
                        inject::key_event(scancode, extended, true);
                    }
                }
                held_keys = synced;
            }
            Some(Event::Heartbeat) => {
                last_heartbeat = Instant::now();
            }
            None => {
                eprintln!("unknown packet ({n} bytes)");
            }
        }

        // Warn if heartbeat is stale
        if last_heartbeat.elapsed().as_secs() > 5 {
            eprintln!("WARNING: no heartbeat for 5s — sender may be disconnected");
            last_heartbeat = Instant::now(); // avoid spam
        }
    }
}
