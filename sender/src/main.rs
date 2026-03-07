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
    path::PathBuf,
    ptr,
    sync::atomic::{AtomicBool, AtomicPtr, AtomicU8, Ordering},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tao::platform::macos::{ActivationPolicy, EventLoopExtMacOS};
use tray_icon::{
    menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem},
    Icon, TrayIconBuilder,
};

extern "C" {
    fn CGAssociateMouseAndMouseCursorPosition(connected: bool) -> i32;
    fn CGEventTapEnable(tap: *mut c_void, enable: bool);
    fn _CGSDefaultConnection() -> i32;
    fn CGSSetConnectionProperty(
        connection: i32,
        target: i32,
        key: *const c_void,
        value: *const c_void,
    ) -> i32;
}

const HOTKEY_KEYCODE: u16 = 0x35; // kVK_Escape
const HOTKEY_REQUIRES_CTRL: bool = true;

#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum AppState {
    Waiting = 0,
    Connected = 1,
    Capturing = 2,
}

impl AppState {
    fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Connected,
            2 => Self::Capturing,
            _ => Self::Waiting,
        }
    }
    fn tooltip(self) -> &'static str {
        match self {
            Self::Waiting => "Hotswitch — Waiting for receiver",
            Self::Connected => "Hotswitch — Connected",
            Self::Capturing => "Hotswitch — Capturing",
        }
    }
    fn icon(self) -> (Icon, bool) {
        match self {
            Self::Waiting => (make_icon(0, 0, 0, false), true),
            Self::Connected => (make_icon(0, 0, 0, true), true),
            Self::Capturing => (make_icon(234, 179, 8, true), false),
        }
    }
    fn status_text(self) -> &'static str {
        match self {
            Self::Waiting => "Waiting for receiver...",
            Self::Connected => "Connected",
            Self::Capturing => "Capturing",
        }
    }
}

#[derive(Debug)]
enum UserEvent {
    StateChanged,
    CaptureBlocked,
    UpdateAvailable(String),
    Menu(tray_icon::menu::MenuEvent),
}

fn make_icon(r: u8, g: u8, b: u8, filled: bool) -> Icon {
    let (rgba, sz) = hotswitch_proto::icon::make_icon_rgba(r, g, b, filled);
    Icon::from_rgba(rgba, sz, sz).unwrap()
}

fn check_for_update() -> Option<String> {
    let releases = self_update::backends::github::ReleaseList::configure()
        .repo_owner("aprets")
        .repo_name("hotswitch")
        .build()
        .ok()?
        .fetch()
        .ok()?;
    let latest = releases.first()?;
    if self_update::version::bump_is_greater(
        self_update::cargo_crate_version!(),
        &latest.version,
    )
    .unwrap_or(false)
    {
        Some(latest.version.clone())
    } else {
        None
    }
}

fn apply_update() -> Result<self_update::Status, Box<dyn std::error::Error>> {
    let status = self_update::backends::github::Update::configure()
        .repo_owner("aprets")
        .repo_name("hotswitch")
        .bin_name("hotswitch-sender")
        .current_version(self_update::cargo_crate_version!())
        .no_confirm(true)
        .show_download_progress(false)
        .show_output(false)
        .build()?
        .update()?;
    Ok(status)
}

fn map_mouse_button(event_type: &CGEventType, ev: &CGEvent) -> Option<(u8, bool)> {
    match event_type {
        CGEventType::LeftMouseDown => Some((0, true)),
        CGEventType::LeftMouseUp => Some((0, false)),
        CGEventType::RightMouseDown => Some((1, true)),
        CGEventType::RightMouseUp => Some((1, false)),
        CGEventType::OtherMouseDown | CGEventType::OtherMouseUp => {
            let btn = ev.get_integer_value_field(EventField::MOUSE_EVENT_BUTTON_NUMBER);
            let mapped = match btn {
                2 => 2,
                3 => 3,
                4 => 4,
                n => n as u8,
            };
            let pressed = matches!(event_type, CGEventType::OtherMouseDown);
            Some((mapped, pressed))
        }
        _ => None,
    }
}

// --- Log file ---

fn log_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home)
        .join("Library/Logs")
        .join("hotswitch-sender.log")
}

fn redirect_stdio_to_log() -> PathBuf {
    extern "C" {
        fn dup2(oldfd: i32, newfd: i32) -> i32;
        fn close(fd: i32) -> i32;
    }
    let path = log_path();
    if let Ok(file) = std::fs::File::create(&path) {
        use std::os::unix::io::IntoRawFd;
        let fd = file.into_raw_fd();
        unsafe {
            dup2(fd, 1);
            dup2(fd, 2);
            close(fd);
        }
    }
    path
}

// --- Start on Login (macOS LaunchAgent) ---

fn launch_agent_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home)
        .join("Library/LaunchAgents")
        .join("com.hotswitch.sender.plist")
}

fn is_login_item() -> bool {
    launch_agent_path().exists()
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn set_login_item(enabled: bool, target_addr: &str) -> bool {
    let path = launch_agent_path();
    eprintln!("set_login_item: enabled={enabled}, path={}", path.display());
    if enabled {
        let exe = match std::env::current_exe() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("Failed to get current exe path: {e}");
                return false;
            }
        };
        let exe_escaped = xml_escape(&exe.display().to_string());
        let addr_escaped = xml_escape(target_addr);
        let plist = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.hotswitch.sender</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe_escaped}</string>
        <string>{addr_escaped}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <false/>
</dict>
</plist>"#
        );
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match std::fs::write(&path, plist) {
            Ok(()) => { eprintln!("wrote launch agent: {}", path.display()); true }
            Err(e) => { eprintln!("failed to write launch agent: {e}"); false }
        }
    } else {
        match std::fs::remove_file(&path) {
            Ok(()) => true,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
            Err(e) => { eprintln!("failed to remove launch agent: {e}"); false }
        }
    }
}

fn main() {
    let log_file_path = redirect_stdio_to_log();

    let target_addr = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "10.0.0.100:24801".to_string());

    eprintln!("hotswitch sender starting, target: {target_addr}");

    let socket = UdpSocket::bind("0.0.0.0:0").expect("Failed to bind UDP socket");
    socket
        .connect(&target_addr)
        .expect("Failed to connect UDP socket");
    socket.set_nonblocking(true).ok();

    unsafe {
        let conn = _CGSDefaultConnection();
        let key = CFString::new("SetsCursorInBackground");
        let val = CFBoolean::true_value();
        if CGSSetConnectionProperty(conn, conn, key.as_CFTypeRef(), val.as_CFTypeRef()) != 0 {
            eprintln!("WARNING: Failed to set SetsCursorInBackground — cursor may not hide");
        }
    }

    // Shared state: Waiting / Connected / Capturing
    let app_state = Arc::new(AtomicU8::new(AppState::Waiting as u8));
    let capturing = Arc::new(AtomicBool::new(false));
    let receiver_connected = Arc::new(AtomicBool::new(false));
    let held_keys: Arc<std::sync::Mutex<HashSet<u16>>> =
        Arc::new(std::sync::Mutex::new(HashSet::new()));
    let accum_dx = Arc::new(std::sync::Mutex::new(0.0f64));
    let accum_dy = Arc::new(std::sync::Mutex::new(0.0f64));

    // Tao event loop
    let mut event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    event_loop.set_activation_policy(ActivationPolicy::Accessory);
    let proxy = event_loop.create_proxy();

    // Helper to recompute and store combined AppState
    let compute_state = {
        let capturing = capturing.clone();
        let receiver_connected = receiver_connected.clone();
        let app_state = app_state.clone();
        move || -> AppState {
            let s = if capturing.load(Ordering::SeqCst) {
                AppState::Capturing
            } else if receiver_connected.load(Ordering::SeqCst) {
                AppState::Connected
            } else {
                AppState::Waiting
            };
            app_state.store(s as u8, Ordering::SeqCst);
            s
        }
    };

    // --- CGEventTap setup ---
    let cap = capturing.clone();
    let rc_tap = receiver_connected.clone();
    let sock = socket.try_clone().expect("Failed to clone socket");
    let keys = held_keys.clone();
    let adx = accum_dx.clone();
    let ady = accum_dy.clone();
    let tap_port: Arc<AtomicPtr<c_void>> = Arc::new(AtomicPtr::new(ptr::null_mut()));
    let tp = tap_port.clone();
    let proxy_tap = proxy.clone();
    let compute_tap = compute_state.clone();

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

            if matches!(event_type, CGEventType::KeyDown) {
                let keycode = cg_ev.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE) as u16;
                if keycode == HOTKEY_KEYCODE {
                    let flags = cg_ev.get_flags();
                    let ctrl_held = flags.contains(CGEventFlags::CGEventFlagControl);
                    if ctrl_held || !HOTKEY_REQUIRES_CTRL {
                        let was_capturing = cap.load(Ordering::SeqCst);
                        if !was_capturing && !rc_tap.load(Ordering::SeqCst) {
                            let _ = proxy_tap.send_event(UserEvent::CaptureBlocked);
                            return CallbackResult::Drop;
                        }
                        let now_capturing = !was_capturing;
                        cap.store(now_capturing, Ordering::SeqCst);
                        eprintln!("capture {}", if now_capturing { "started" } else { "stopped" });

                        if now_capturing {
                            unsafe { CGAssociateMouseAndMouseCursorPosition(false); }
                            let _ = CGDisplay::hide_cursor(&CGDisplay::main());
                            *adx.lock().unwrap() = 0.0;
                            *ady.lock().unwrap() = 0.0;
                        } else {
                            unsafe { CGAssociateMouseAndMouseCursorPosition(true); }
                            let _ = CGDisplay::show_cursor(&CGDisplay::main());
                            let held: Vec<u16> = keys.lock().unwrap().drain().collect();
                            let mut buf = [0u8; 64];
                            for keycode in held {
                                let evt = Event::Key { keycode, pressed: false };
                                let len = evt.to_bytes(&mut buf);
                                let _ = sock.send(&buf[..len]);
                            }
                        }
                        compute_tap();
                        let _ = proxy_tap.send_event(UserEvent::StateChanged);
                        return CallbackResult::Drop;
                    }
                }
            }

            if !cap.load(Ordering::SeqCst) {
                return CallbackResult::Keep;
            }

            let mut buf = [0u8; 64];

            match event_type {
                CGEventType::MouseMoved
                | CGEventType::LeftMouseDragged
                | CGEventType::RightMouseDragged
                | CGEventType::OtherMouseDragged => {
                    let raw_dx = cg_ev.get_double_value_field(EventField::MOUSE_EVENT_DELTA_X);
                    let raw_dy = cg_ev.get_double_value_field(EventField::MOUSE_EVENT_DELTA_Y);
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

                CGEventType::KeyDown => {
                    let keycode =
                        cg_ev.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE) as u16;
                    keys.lock().unwrap().insert(keycode);
                    let evt = Event::Key { keycode, pressed: true };
                    let len = evt.to_bytes(&mut buf);
                    let _ = sock.send(&buf[..len]);
                }
                CGEventType::KeyUp => {
                    let keycode =
                        cg_ev.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE) as u16;
                    keys.lock().unwrap().remove(&keycode);
                    let evt = Event::Key { keycode, pressed: false };
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
                    if pressed { k.insert(keycode); } else { k.remove(&keycode); }
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

    // --- Heartbeat listener thread ---
    let sock_rx = socket.try_clone().expect("Failed to clone socket");
    let rc = receiver_connected.clone();
    let proxy_hb = proxy.clone();
    let compute_hb = compute_state.clone();
    thread::spawn(move || {
        eprintln!("waiting for receiver...");
        let mut buf = [0u8; 8];
        let mut was_connected = false;
        let mut last_recv = Instant::now();
        loop {
            match sock_rx.recv(&mut buf) {
                Ok(n) if Event::from_bytes(&buf[..n]).is_some() => {
                    if !was_connected {
                        eprintln!("receiver connected");
                        rc.store(true, Ordering::SeqCst);
                        was_connected = true;
                        compute_hb();
                        let _ = proxy_hb.send_event(UserEvent::StateChanged);
                    }
                    last_recv = Instant::now();
                }
                _ => {}
            }
            if was_connected && last_recv.elapsed().as_secs() > 3 {
                eprintln!("receiver disconnected");
                rc.store(false, Ordering::SeqCst);
                was_connected = false;
                compute_hb();
                let _ = proxy_hb.send_event(UserEvent::StateChanged);
            }
            thread::sleep(Duration::from_millis(500));
        }
    });

    // --- Heartbeat + key sync thread ---
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
                let held: Vec<u16> = keys2.lock().unwrap().iter().copied().collect();
                let evt = Event::KeySync { keys: held };
                let len = evt.to_bytes(&mut buf);
                let _ = sock2.send(&buf[..len]);
            }
            if tick % 10 == 0 {
                let evt = Event::Heartbeat;
                let len = evt.to_bytes(&mut buf);
                let _ = sock2.send(&buf[..len]);
            }
        }
    });

    // --- Tray icon setup + event loop ---
    let update_item = MenuItem::new("Check for Updates", true, None);
    {
        let proxy = proxy.clone();
        thread::spawn(move || {
            if let Some(ver) = check_for_update() {
                eprintln!("update available: v{ver}");
                let _ = proxy.send_event(UserEvent::UpdateAvailable(ver));
            }
        });
    }

    let menu = Menu::new();
    let status_item = MenuItem::new(AppState::Waiting.status_text(), false, None);
    let log_item = MenuItem::new("Show Log", true, None);
    let login_item = CheckMenuItem::new("Start on Login", true, is_login_item(), None);
    let quit_item = MenuItem::new("Quit", true, None);
    let _ = menu.append_items(&[
        &status_item,
        &PredefinedMenuItem::separator(),
        &update_item,
        &log_item,
        &login_item,
        &PredefinedMenuItem::separator(),
        &quit_item,
    ]);

    let menu_proxy = proxy.clone();
    MenuEvent::set_event_handler(Some(move |evt| {
        let _ = menu_proxy.send_event(UserEvent::Menu(evt));
    }));

    let initial_state = AppState::Waiting;
    let (init_icon, init_template) = initial_state.icon();
    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip(initial_state.tooltip())
        .with_icon(init_icon)
        .with_icon_as_template(init_template)
        .build()
        .expect("Failed to create tray icon");

    let update_id = update_item.id().clone();
    let log_id = log_item.id().clone();
    let login_id = login_item.id().clone();
    let quit_id = quit_item.id().clone();
    let addr_for_login = target_addr.clone();
    let mut login_checked = is_login_item();

    let mut last_state = initial_state;
    let mut flash_until: Option<Instant> = None;
    let mut reset_update_at: Option<Instant> = None;

    event_loop.run(move |event, _, control_flow| {
        let next_wake = [flash_until, reset_update_at]
            .iter()
            .filter_map(|t| *t)
            .min();
        *control_flow = match next_wake {
            Some(d) if d > Instant::now() => ControlFlow::WaitUntil(d),
            _ => ControlFlow::Wait,
        };

        if let tao::event::Event::NewEvents(tao::event::StartCause::ResumeTimeReached { .. }) = &event {
            let now = Instant::now();
            if flash_until.map_or(false, |d| now >= d) {
                flash_until = None;
                let (icon, tmpl) = last_state.icon();
                let _ = tray.set_icon_with_as_template(Some(icon), tmpl);
                let _ = tray.set_tooltip(Some(last_state.tooltip()));
            }
            if reset_update_at.map_or(false, |d| now >= d) {
                reset_update_at = None;
                update_item.set_text("Check for Updates");
            }
        }

        if let tao::event::Event::UserEvent(ue) = &event {
            match ue {
                UserEvent::StateChanged => {
                    let new_state = AppState::from_u8(app_state.load(Ordering::SeqCst));
                    if new_state != last_state {
                        if flash_until.is_none() {
                            let (icon, tmpl) = new_state.icon();
                            let _ = tray.set_icon_with_as_template(Some(icon), tmpl);
                            let _ = tray.set_tooltip(Some(new_state.tooltip()));
                        }
                        status_item.set_text(new_state.status_text());
                        last_state = new_state;
                    }
                }
                UserEvent::UpdateAvailable(ver) => {
                    update_item.set_text(format!("Update to v{ver}"));
                }
                UserEvent::CaptureBlocked => {
                    let _ = tray.set_icon_with_as_template(Some(make_icon(220, 38, 38, true)), false);
                    let _ = tray.set_tooltip(Some("Hotswitch — No receiver"));
                    let deadline = Instant::now() + Duration::from_secs(2);
                    flash_until = Some(deadline);
                    *control_flow = ControlFlow::WaitUntil(deadline);
                }
                UserEvent::Menu(me) => {
                    if me.id == quit_id {
                        if capturing.load(Ordering::SeqCst) {
                            unsafe { CGAssociateMouseAndMouseCursorPosition(true); }
                            let _ = CGDisplay::show_cursor(&CGDisplay::main());
                        }
                        *control_flow = ControlFlow::Exit;
                    } else if me.id == update_id {
                        update_item.set_text("Updating...");
                        update_item.set_enabled(false);
                        match apply_update() {
                            Ok(status) => {
                                eprintln!("update result: {status}");
                                if status.updated() {
                                    let exe = std::env::current_exe().expect("Failed to get current exe path");
                                    let args: Vec<String> = std::env::args().skip(1).collect();
                                    let _ = std::process::Command::new(exe).args(&args).spawn();
                                    *control_flow = ControlFlow::Exit;
                                    return;
                                } else {
                                    update_item.set_text("Already up to date");
                                    let deadline = Instant::now() + Duration::from_secs(5);
                                    reset_update_at = Some(deadline);
                                    *control_flow = ControlFlow::WaitUntil(deadline);
                                }
                            }
                            Err(e) => {
                                eprintln!("update failed: {e}");
                                update_item.set_text("Update failed");
                                let deadline = Instant::now() + Duration::from_secs(5);
                                reset_update_at = Some(deadline);
                                *control_flow = ControlFlow::WaitUntil(deadline);
                            }
                        }
                        update_item.set_enabled(true);
                    } else if me.id == log_id {
                        let _ = std::process::Command::new("open").arg("-t").arg(&log_file_path).spawn();
                    } else if me.id == login_id {
                        if set_login_item(!login_checked, &addr_for_login) {
                            login_checked = !login_checked;
                            login_item.set_checked(login_checked);
                        }
                    }
                }
            }
        }
    });
}
