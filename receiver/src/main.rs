use hotswitch_proto::{keymap, Event};
use std::collections::HashSet;
use std::net::UdpSocket;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tray_icon::{
    menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem},
    Icon, TrayIconBuilder,
};

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

#[cfg(not(windows))]
mod inject {
    pub fn rel_mouse(_dx: i32, _dy: i32) {}
    pub fn mouse_button(_button: u8, _pressed: bool) {}
    pub fn scroll(_dx: i16, _dy: i16) {}
    pub fn key_event(_scancode: u16, _extended: bool, _pressed: bool) {}
}

#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum AppState {
    Listening = 0,
    Connected = 1,
}

impl AppState {
    fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Connected,
            _ => Self::Listening,
        }
    }
    fn tooltip(self) -> &'static str {
        match self {
            Self::Listening => "Hotswitch — Listening",
            Self::Connected => "Hotswitch — Connected",
        }
    }
    fn icon(self) -> Icon {
        match self {
            Self::Listening => make_dot_icon(128, 128, 128),
            Self::Connected => make_dot_icon(34, 197, 94),
        }
    }
    fn status_text(self) -> &'static str {
        match self {
            Self::Listening => "Listening...",
            Self::Connected => "Connected",
        }
    }
}

#[derive(Debug)]
enum UserEvent {
    StateChanged,
}

fn make_dot_icon(r: u8, g: u8, b: u8) -> Icon {
    let size = 16u32;
    let mut rgba = vec![0u8; (size * size * 4) as usize];
    let center = size as f32 / 2.0;
    let radius = center - 1.0;
    for y in 0..size {
        for x in 0..size {
            let dx = x as f32 - center + 0.5;
            let dy = y as f32 - center + 0.5;
            let i = ((y * size + x) * 4) as usize;
            if dx * dx + dy * dy <= radius * radius {
                rgba[i] = r;
                rgba[i + 1] = g;
                rgba[i + 2] = b;
                rgba[i + 3] = 255;
            }
        }
    }
    Icon::from_rgba(rgba, size, size).unwrap()
}

// --- Log file ---

fn log_path() -> PathBuf {
    #[cfg(windows)]
    {
        let appdata = std::env::var("APPDATA").unwrap_or_else(|_| "C:\\".to_string());
        let dir = PathBuf::from(appdata).join("hotswitch");
        let _ = std::fs::create_dir_all(&dir);
        dir.join("receiver.log")
    }
    #[cfg(not(windows))]
    {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
        let dir = PathBuf::from(home).join("Library/Logs");
        let _ = std::fs::create_dir_all(&dir);
        dir.join("hotswitch-receiver.log")
    }
}

fn redirect_stdio_to_log() -> PathBuf {
    let path = log_path();
    if let Ok(file) = std::fs::File::create(&path) {
        #[cfg(unix)]
        {
            extern "C" {
                fn dup2(oldfd: i32, newfd: i32) -> i32;
                fn close(fd: i32) -> i32;
            }
            use std::os::unix::io::IntoRawFd;
            let fd = file.into_raw_fd();
            unsafe {
                dup2(fd, 1);
                dup2(fd, 2);
                close(fd);
            }
        }
        #[cfg(windows)]
        {
            extern "C" {
                fn _open_osfhandle(osfhandle: isize, flags: i32) -> i32;
                fn _dup2(fd1: i32, fd2: i32) -> i32;
                fn _close(fd: i32) -> i32;
            }
            use std::os::windows::io::IntoRawHandle;
            let handle = file.into_raw_handle() as isize;
            let fd = unsafe { _open_osfhandle(handle, 0) };
            if fd != -1 {
                unsafe {
                    _dup2(fd, 1);
                    _dup2(fd, 2);
                    _close(fd);
                }
            }
        }
    }
    path
}

fn open_log(path: &PathBuf) {
    #[cfg(windows)]
    let cmd = "notepad";
    #[cfg(target_os = "macos")]
    let cmd = "open";
    #[cfg(target_os = "linux")]
    let cmd = "xdg-open";
    let _ = std::process::Command::new(cmd).arg(path).spawn();
}

// --- Start on Login (Windows registry) ---

#[cfg(windows)]
fn is_login_item() -> bool {
    use windows::Win32::System::Registry::*;
    use windows::core::HSTRING;
    let key_path = HSTRING::from("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
    let mut hkey = HKEY::default();
    let result = unsafe {
        RegOpenKeyExW(HKEY_CURRENT_USER, &key_path, 0, KEY_READ, &mut hkey)
    };
    if result.is_err() {
        return false;
    }
    let value_name = HSTRING::from("Hotswitch");
    let exists = unsafe {
        RegQueryValueExW(hkey, &value_name, None, None, None, None).is_ok()
    };
    unsafe { let _ = RegCloseKey(hkey); }
    exists
}

#[cfg(windows)]
fn set_login_item(enabled: bool) {
    use windows::Win32::System::Registry::*;
    use windows::core::HSTRING;
    let key_path = HSTRING::from("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
    let mut hkey = HKEY::default();
    let result = unsafe {
        RegOpenKeyExW(HKEY_CURRENT_USER, &key_path, 0, KEY_WRITE, &mut hkey)
    };
    if result.is_err() {
        eprintln!("Failed to open registry key");
        return;
    }
    let value_name = HSTRING::from("Hotswitch");
    if enabled {
        let exe = match std::env::current_exe() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("Failed to get current exe path: {e}");
                unsafe { let _ = RegCloseKey(hkey); }
                return;
            }
        };
        let exe_str = exe.to_string_lossy();
        let wide: Vec<u16> = exe_str.encode_utf16().chain(std::iter::once(0)).collect();
        let bytes: &[u8] = unsafe {
            std::slice::from_raw_parts(wide.as_ptr() as *const u8, wide.len() * 2)
        };
        let _ = unsafe {
            RegSetValueExW(hkey, &value_name, 0, REG_SZ, Some(bytes))
        };
    } else {
        let _ = unsafe { RegDeleteValueW(hkey, &value_name) };
    }
    unsafe { let _ = RegCloseKey(hkey); }
}

#[cfg(not(windows))]
fn is_login_item() -> bool { false }
#[cfg(not(windows))]
fn set_login_item(_enabled: bool) {}

fn main() {
    let log_file_path = redirect_stdio_to_log();

    let listen_addr = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "0.0.0.0:24801".to_string());

    let socket = UdpSocket::bind(&listen_addr).expect("Failed to bind UDP socket");
    socket.set_read_timeout(Some(Duration::from_secs(2))).ok();
    println!("hotswitch receiver listening on {listen_addr}");

    let app_state = Arc::new(AtomicU8::new(AppState::Listening as u8));

    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();

    // --- Receiver network thread ---
    let state = app_state.clone();
    thread::spawn(move || {
        let mut buf = [0u8; 512];
        let mut hb_buf = [0u8; 1];
        let hb_len = Event::Heartbeat.to_bytes(&mut hb_buf);
        let mut held_keys: HashSet<u16> = HashSet::new();
        let mut sender_connected = false;
        let mut last_heartbeat = Instant::now();
        let mut sender_addr = None;

        let release_all_keys = |keys: &mut HashSet<u16>| {
            for &k in keys.iter() {
                if let Some((sc, ext)) = keymap::cg_to_win_scancode(k) {
                    inject::key_event(sc, ext, false);
                }
            }
            keys.clear();
        };

        loop {
            let (n, src) = match socket.recv_from(&mut buf) {
                Ok(v) => v,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
                {
                    if sender_connected && last_heartbeat.elapsed().as_secs() > 5 {
                        release_all_keys(&mut held_keys);
                        eprintln!("WARNING: sender disconnected");
                        sender_connected = false;
                        state.store(AppState::Listening as u8, Ordering::SeqCst);
                        let _ = proxy.send_event(UserEvent::StateChanged);
                    }
                    continue;
                }
                Err(e) => {
                    eprintln!("recv error: {e}");
                    continue;
                }
            };

            if !sender_connected || sender_addr != Some(src) {
                if sender_connected {
                    release_all_keys(&mut held_keys);
                }
                println!("sender connected from {src}");
                sender_addr = Some(src);
                sender_connected = true;
                last_heartbeat = Instant::now();
                state.store(AppState::Connected as u8, Ordering::SeqCst);
                let _ = proxy.send_event(UserEvent::StateChanged);
            }

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
                    for &k in held_keys.difference(&synced) {
                        if let Some((scancode, extended)) = keymap::cg_to_win_scancode(k) {
                            inject::key_event(scancode, extended, false);
                        }
                    }
                    for &k in synced.difference(&held_keys) {
                        if let Some((scancode, extended)) = keymap::cg_to_win_scancode(k) {
                            inject::key_event(scancode, extended, true);
                        }
                    }
                    held_keys = synced;
                }
                Some(Event::Heartbeat) => {
                    last_heartbeat = Instant::now();
                    let _ = socket.send_to(&hb_buf[..hb_len], src);
                }
                None => {
                    eprintln!("unknown packet ({n} bytes)");
                }
            }
        }
    });

    // --- Tray icon setup + event loop ---
    let menu = Menu::new();
    let status_item = MenuItem::new(AppState::Listening.status_text(), false, None);
    let log_item = MenuItem::new("Show Log", true, None);
    let login_item = CheckMenuItem::new("Start on Login", true, is_login_item(), None);
    let quit_item = MenuItem::new("Quit", true, None);
    let _ = menu.append_items(&[
        &status_item,
        &PredefinedMenuItem::separator(),
        &log_item,
        &login_item,
        &PredefinedMenuItem::separator(),
        &quit_item,
    ]);

    let initial_state = AppState::Listening;
    let _tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip(initial_state.tooltip())
        .with_icon(initial_state.icon())
        .build()
        .expect("Failed to create tray icon");

    let menu_rx = MenuEvent::receiver();
    let log_id = log_item.id().clone();
    let login_id = login_item.id().clone();
    let quit_id = quit_item.id().clone();

    let mut last_state = initial_state;

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;

        if let tao::event::Event::UserEvent(UserEvent::StateChanged) = &event {
            let new_state = AppState::from_u8(app_state.load(Ordering::SeqCst));
            if new_state != last_state {
                let _ = _tray.set_tooltip(Some(new_state.tooltip()));
                let _ = _tray.set_icon(Some(new_state.icon()));
                status_item.set_text(new_state.status_text());
                last_state = new_state;
            }
        }

        if let Ok(event) = menu_rx.try_recv() {
            if event.id == quit_id {
                *control_flow = ControlFlow::Exit;
            } else if event.id == log_id {
                open_log(&log_file_path);
            } else if event.id == login_id {
                let now_checked = !login_item.is_checked();
                login_item.set_checked(now_checked);
                set_login_item(now_checked);
            }
        }
    });
}
