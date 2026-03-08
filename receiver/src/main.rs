use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use hotswitch_proto::{audio, keymap, Event};
use std::collections::HashSet;
use std::net::{SocketAddr, UdpSocket};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
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
    fn icon(self) -> (Icon, bool) {
        match self {
            Self::Listening => (make_icon(0, 0, 0, false), true),
            Self::Connected => (make_icon(0, 0, 0, true), true),
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
    UpdateAvailable(String),
    ResetUpdateText,
    Menu(tray_icon::menu::MenuEvent),
}

fn icon_size() -> u32 {
    use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSMICON};
    let size = unsafe { GetSystemMetrics(SM_CXSMICON) };
    if size > 0 { (size as u32) * 2 } else { 32 }
}

fn make_icon(r: u8, g: u8, b: u8, filled: bool) -> Icon {
    let (rgba, sz) = hotswitch_proto::icon::make_icon_rgba(r, g, b, filled, icon_size());
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
        .bin_name("hotswitch-receiver")
        .current_version(self_update::cargo_crate_version!())
        .no_confirm(true)
        .show_download_progress(false)
        .show_output(false)
        .build()?
        .update()?;
    Ok(status)
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
    let _ = std::process::Command::new(cmd).arg(path).spawn();
}

// --- Start on Login (Task Scheduler, runs elevated) ---

const TASK_NAME: &str = "Hotswitch";

#[cfg(windows)]
fn is_login_item() -> bool {
    std::process::Command::new("schtasks")
        .args(["/Query", "/TN", TASK_NAME])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(windows)]
fn set_login_item(enabled: bool) -> bool {
    if enabled {
        let exe = match std::env::current_exe() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("failed to get current exe path: {e}");
                return false;
            }
        };
        let exe_str = exe.to_string_lossy().to_string();
        std::process::Command::new("schtasks")
            .args([
                "/Create", "/F",
                "/TN", TASK_NAME,
                "/TR", &exe_str,
                "/SC", "ONLOGON",
                "/RL", "HIGHEST",
                "/IT",
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    } else {
        std::process::Command::new("schtasks")
            .args(["/Delete", "/F", "/TN", TASK_NAME])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
}

#[cfg(not(windows))]
fn is_login_item() -> bool { false }
#[cfg(not(windows))]
fn set_login_item(_enabled: bool) -> bool { true }

fn start_audio_capture(target: SocketAddr, running: Arc<AtomicBool>) {
    thread::spawn(move || {
        let host = cpal::default_host();

        // WASAPI loopback: use the default *output* device as an input source
        let device = match host.default_output_device() {
            Some(d) => d,
            None => {
                eprintln!("audio: no output device for loopback capture");
                return;
            }
        };
        let device_name = device.name().unwrap_or_default();
        eprintln!("audio: capturing from {device_name}");

        let config = cpal::StreamConfig {
            channels: audio::CHANNELS,
            sample_rate: cpal::SampleRate(audio::SAMPLE_RATE),
            buffer_size: cpal::BufferSize::Default,
        };

        let socket = match UdpSocket::bind("0.0.0.0:0") {
            Ok(s) => Arc::new(s),
            Err(e) => {
                eprintln!("audio: failed to bind socket: {e}");
                return;
            }
        };

        let sock = socket.clone();
        let run = running.clone();
        let packets_sent = Arc::new(AtomicU64::new(0));
        let pkt_count = packets_sent.clone();
        let stream = device.build_input_stream(
            &config,
            move |data: &[f32], _: &cpal::InputCallbackInfo| {
                if !run.load(Ordering::Relaxed) {
                    return;
                }
                let mut buf = [0u8; 1472];
                for chunk in data.chunks(audio::MAX_SAMPLES_PER_PACKET) {
                    let len = audio::audio_to_bytes(audio::CHANNELS, chunk, &mut buf);
                    let _ = sock.send_to(&buf[..len], target);
                    pkt_count.fetch_add(1, Ordering::Relaxed);
                }
            },
            |err| eprintln!("audio: stream error: {err}"),
            None,
        );

        let stream = match stream {
            Ok(s) => s,
            Err(e) => {
                eprintln!("audio: failed to build input stream: {e}");
                return;
            }
        };

        if let Err(e) = stream.play() {
            eprintln!("audio: failed to start stream: {e}");
            return;
        }

        eprintln!("audio: streaming to {target}");
        let mut last_log = Instant::now();
        while running.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_millis(100));
            if last_log.elapsed().as_secs() >= 5 {
                let pkts = packets_sent.swap(0, Ordering::Relaxed);
                eprintln!("audio: {pkts} pkts sent (5s)");
                last_log = Instant::now();
            }
        }
        drop(stream);
        eprintln!("audio: stopped");
    });
}

fn main() {
    let log_file_path = redirect_stdio_to_log();

    let listen_addr = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "0.0.0.0:24801".to_string());

    let socket = {
        let mut attempts = 0;
        loop {
            match UdpSocket::bind(&listen_addr) {
                Ok(s) => break s,
                Err(e) if attempts < 10 => {
                    attempts += 1;
                    eprintln!("bind attempt {attempts}/10 failed: {e}, retrying...");
                    thread::sleep(Duration::from_millis(500));
                }
                Err(e) => panic!("Failed to bind UDP socket after {attempts} attempts: {e}"),
            }
        }
    };
    socket.set_read_timeout(Some(Duration::from_secs(2))).ok();
    println!("hotswitch receiver listening on {listen_addr}");

    let app_state = Arc::new(AtomicU8::new(AppState::Listening as u8));

    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();

    // --- Receiver network thread ---
    let state = app_state.clone();
    let net_proxy = proxy.clone();
    thread::spawn(move || {
        let mut buf = [0u8; 512];
        let mut hb_buf = [0u8; 1];
        let hb_len = Event::Heartbeat.to_bytes(&mut hb_buf);
        let mut held_keys: HashSet<u16> = HashSet::new();
        let mut sender_connected = false;
        let mut last_heartbeat = Instant::now();
        let mut sender_addr: Option<SocketAddr> = None;
        let audio_running = Arc::new(AtomicBool::new(false));
        let mut audio_target: Option<SocketAddr> = None;

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
                        audio_running.store(false, Ordering::SeqCst);
                        audio_target = None;
                        eprintln!("WARNING: sender disconnected");
                        sender_connected = false;
                        state.store(AppState::Listening as u8, Ordering::SeqCst);
                        let _ = net_proxy.send_event(UserEvent::StateChanged);
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
                    audio_running.store(false, Ordering::SeqCst);
                    audio_target = None;
                }
                println!("sender connected from {src}");
                sender_addr = Some(src);
                sender_connected = true;
                last_heartbeat = Instant::now();
                state.store(AppState::Connected as u8, Ordering::SeqCst);
                let _ = net_proxy.send_event(UserEvent::StateChanged);

                let target = SocketAddr::new(src.ip(), audio::AUDIO_PORT);
                if audio_target != Some(target) {
                    audio_running.store(false, Ordering::SeqCst);
                    thread::sleep(Duration::from_millis(50));
                    audio_running.store(true, Ordering::SeqCst);
                    start_audio_capture(target, audio_running.clone());
                    audio_target = Some(target);
                }
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
    let status_item = MenuItem::new(AppState::Listening.status_text(), false, None);
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

    let initial_state = AppState::Listening;
    let (init_icon, init_template) = initial_state.icon();
    let _tray = TrayIconBuilder::new()
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
    let mut login_checked = is_login_item();

    let mut last_state = initial_state;
    let reset_proxy = proxy.clone();

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;

        if let tao::event::Event::UserEvent(ue) = &event {
            match ue {
                UserEvent::StateChanged => {
                    let new_state = AppState::from_u8(app_state.load(Ordering::SeqCst));
                    if new_state != last_state {
                        let (icon, tmpl) = new_state.icon();
                        let _ = _tray.set_tooltip(Some(new_state.tooltip()));
                        let _ = _tray.set_icon_with_as_template(Some(icon), tmpl);
                        status_item.set_text(new_state.status_text());
                        last_state = new_state;
                    }
                }
                UserEvent::UpdateAvailable(ver) => {
                    update_item.set_text(format!("Update to v{ver}"));
                }
                UserEvent::ResetUpdateText => {
                    update_item.set_text("Check for Updates");
                }
                UserEvent::Menu(me) => {
                    if me.id == quit_id {
                        *control_flow = ControlFlow::Exit;
                    } else if me.id == update_id {
                        let exe = std::env::current_exe().expect("Failed to get current exe path");
                        update_item.set_text("Updating...");
                        update_item.set_enabled(false);
                        match apply_update() {
                            Ok(status) => {
                                eprintln!("update result: {status}");
                                if status.updated() {
                                    let args: Vec<String> = std::env::args().skip(1).collect();
                                    eprintln!("relaunching: {exe:?} {args:?}");
                                    match std::process::Command::new(&exe).args(&args).spawn() {
                                        Ok(_) => {},
                                        Err(e) => eprintln!("relaunch failed: {e}"),
                                    }
                                    *control_flow = ControlFlow::Exit;
                                    return;
                                } else {
                                    update_item.set_text("Already up to date");
                                    let p = reset_proxy.clone();
                                    thread::spawn(move || {
                                        thread::sleep(Duration::from_secs(5));
                                        let _ = p.send_event(UserEvent::ResetUpdateText);
                                    });
                                }
                            }
                            Err(e) => {
                                eprintln!("update failed: {e}");
                                update_item.set_text("Update failed");
                                let p = reset_proxy.clone();
                                thread::spawn(move || {
                                    thread::sleep(Duration::from_secs(5));
                                    let _ = p.send_event(UserEvent::ResetUpdateText);
                                });
                            }
                        }
                        update_item.set_enabled(true);
                    } else if me.id == log_id {
                        open_log(&log_file_path);
                    } else if me.id == login_id {
                        if set_login_item(!login_checked) {
                            login_checked = !login_checked;
                            login_item.set_checked(login_checked);
                        }
                    }
                }
            }
        }
    });
}
