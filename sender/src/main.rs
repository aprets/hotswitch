use core_foundation::{base::TCFType, boolean::CFBoolean, runloop::CFRunLoop, string::CFString};
use core_graphics::{
    display::CGDisplay,
    event::{
        CGEvent, CGEventFlags, CGEventTap, CGEventTapLocation, CGEventTapOptions,
        CGEventTapPlacement, CGEventTapProxy, CGEventType, CGMouseButton, CallbackResult,
        EventField,
    },
    event_source::{CGEventSource, CGEventSourceStateID},
    geometry::CGPoint,
};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use crossbeam_queue::ArrayQueue;
use hotswitch_proto::{audio, Event};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{define_class, msg_send, sel, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSAlert, NSAlertFirstButtonReturn, NSTextField, NSWorkspace,
    NSWorkspaceScreensDidSleepNotification, NSWorkspaceSessionDidResignActiveNotification,
    NSWorkspaceWillSleepNotification,
};
use objc2_foundation::{
    ns_string, NSActivityOptions, NSDistributedNotificationCenter, NSNotification,
    NSNotificationCenter, NSObject, NSObjectProtocol, NSPoint, NSProcessInfo, NSRect, NSSize,
};
use std::{
    collections::HashSet,
    ffi::c_void,
    net::SocketAddr,
    net::UdpSocket,
    path::{Path, PathBuf},
    ptr,
    sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU8, Ordering},
    sync::{Arc, Mutex},
    thread,
    time::SystemTime,
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
    fn CGEventSourceSetLocalEventsSuppressionInterval(event_source: *mut c_void, seconds: f64);
}

const HOTKEY_KEYCODE: u16 = 0x35; // kVK_Escape
const HOTKEY_REQUIRES_CTRL: bool = true;
const AUDIO_BUFFER_CANDIDATES: [u32; 2] = [128, 256];
const AUDIO_QUEUE_CAPACITY: usize = (audio::SAMPLE_RATE as usize * audio::CHANNELS as usize) / 12; // ~83ms
const AUDIO_TARGET_FILL: usize = (audio::SAMPLE_RATE as usize * audio::CHANNELS as usize) / 66; // ~15ms
const AUDIO_BIAS_FILL: usize = (audio::SAMPLE_RATE as usize * audio::CHANNELS as usize) / 40; // ~25ms
const AUDIO_RESET_FILL: usize = (audio::SAMPLE_RATE as usize * audio::CHANNELS as usize) / 20; // ~50ms
const AUDIO_SEQ_RESET_STALE_PACKETS: u32 = 16;
const DEFAULT_RECEIVER_PLACEHOLDER: &str = "10.0.0.100:24801";
const RECEIVER_ADDRESS_KEY: &str = "receiver-address.txt";
const MACOS_RELEASE_ASSET: &str = "hotswitch-sender-aarch64-apple-darwin.tar.gz";

fn seq_is_newer(seq: u32, last: u32) -> bool {
    (seq.wrapping_sub(last) as i32) > 0
}

fn drain_audio_queue(queue: &ArrayQueue<f32>, buf_fill: &AtomicU32, primed: &AtomicBool) {
    while queue.pop().is_some() {}
    buf_fill.store(0, Ordering::Relaxed);
    primed.store(false, Ordering::Relaxed);
}

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
    ReleaseCapture(&'static str),
    UpdateAvailable(String),
    Menu(tray_icon::menu::MenuEvent),
}

#[derive(Clone)]
struct LifecycleObserverIvars {
    proxy: tao::event_loop::EventLoopProxy<UserEvent>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = LifecycleObserverIvars]
    struct LifecycleObserver;

    unsafe impl NSObjectProtocol for LifecycleObserver {}

    impl LifecycleObserver {
        #[unsafe(method(handleNotification:))]
        fn handle_notification(&self, _notification: &NSNotification) {
            let _ = self
                .ivars()
                .proxy
                .send_event(UserEvent::ReleaseCapture("macOS lifecycle"));
        }
    }
);

impl LifecycleObserver {
    fn new(
        mtm: MainThreadMarker,
        proxy: tao::event_loop::EventLoopProxy<UserEvent>,
    ) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(LifecycleObserverIvars { proxy });
        unsafe { msg_send![super(this), init] }
    }

    fn register(
        &self,
        workspace_center: &NSNotificationCenter,
        distributed_center: &NSDistributedNotificationCenter,
    ) {
        unsafe {
            workspace_center.addObserver_selector_name_object(
                self,
                sel!(handleNotification:),
                Some(NSWorkspaceWillSleepNotification),
                None,
            );
            workspace_center.addObserver_selector_name_object(
                self,
                sel!(handleNotification:),
                Some(NSWorkspaceScreensDidSleepNotification),
                None,
            );
            workspace_center.addObserver_selector_name_object(
                self,
                sel!(handleNotification:),
                Some(NSWorkspaceSessionDidResignActiveNotification),
                None,
            );
            distributed_center.addObserver_selector_name_object(
                self,
                sel!(handleNotification:),
                Some(ns_string!("com.apple.screenIsLocked")),
                None,
            );
            distributed_center.addObserver_selector_name_object(
                self,
                sel!(handleNotification:),
                Some(ns_string!("com.apple.screensaver.didstart")),
                None,
            );
        }
    }
}

struct PowerActivity {
    process_info: Retained<NSProcessInfo>,
    activity: Retained<ProtocolObject<dyn NSObjectProtocol>>,
}

impl PowerActivity {
    fn stop(&self) {
        unsafe {
            self.process_info.endActivity(&self.activity);
        }
    }
}

fn make_icon(r: u8, g: u8, b: u8, filled: bool) -> Icon {
    let (rgba, sz) = hotswitch_proto::icon::make_icon_rgba(r, g, b, filled, 256);
    Icon::from_rgba(rgba, sz, sz).unwrap()
}

fn configure_cursor_capture() {
    if let Ok(event_source) = CGEventSource::new(CGEventSourceStateID::CombinedSessionState) {
        unsafe {
            CGEventSourceSetLocalEventsSuppressionInterval(
                event_source.as_ref() as *const _ as *mut c_void,
                0.05,
            );
        }
    } else {
        eprintln!("WARNING: Failed to create CGEventSource — cursor warp may feel sticky");
    }
}

fn start_sleep_prevention() -> PowerActivity {
    let process_info = NSProcessInfo::processInfo();
    let activity = process_info.beginActivityWithOptions_reason(
        NSActivityOptions::IdleDisplaySleepDisabled | NSActivityOptions::IdleSystemSleepDisabled,
        ns_string!("Hotswitch sender active"),
    );
    eprintln!("power: preventing idle display and system sleep while sender is running");
    PowerActivity {
        process_info,
        activity,
    }
}

fn disengage_capture(
    reason: &str,
    capturing: &AtomicBool,
    capture_anchor: &Mutex<Option<CGPoint>>,
    held_keys: &Mutex<HashSet<u16>>,
    accum_dx: &Mutex<f64>,
    accum_dy: &Mutex<f64>,
    socket: &UdpSocket,
) -> bool {
    if !capturing.swap(false, Ordering::SeqCst) {
        eprintln!("capture release skipped ({reason}): already not capturing");
        return false;
    }

    let restore_pos = capture_anchor.lock().unwrap().take();
    *accum_dx.lock().unwrap() = 0.0;
    *accum_dy.lock().unwrap() = 0.0;

    let held: Vec<u16> = held_keys.lock().unwrap().drain().collect();
    eprintln!(
        "capture stopped ({reason}): restore_pos={:?}, held_keys={}",
        restore_pos,
        held.len()
    );

    let associate_result = unsafe { CGAssociateMouseAndMouseCursorPosition(true) };
    eprintln!("cursor restore ({reason}): associate result={associate_result}");

    let show_result = CGDisplay::show_cursor(&CGDisplay::main());
    eprintln!("cursor restore ({reason}): show result={show_result:?}");

    if let Some(pos) = restore_pos {
        if let Ok(source) = CGEventSource::new(CGEventSourceStateID::CombinedSessionState) {
            match CGEvent::new_mouse_event(
                source,
                CGEventType::MouseMoved,
                pos,
                CGMouseButton::Left,
            ) {
                Ok(event) => {
                    event.post(CGEventTapLocation::Session);
                    eprintln!("cursor restore ({reason}): posted mouse moved at {pos:?}");
                }
                Err(()) => {
                    eprintln!("cursor restore ({reason}): failed to create mouse moved event");
                }
            }
        } else {
            eprintln!("cursor restore ({reason}): failed to create event source");
        }
    } else {
        eprintln!("cursor restore ({reason}): no anchor to redraw at");
    }

    let mut buf = [0u8; 64];
    for keycode in held {
        let evt = Event::Key {
            keycode,
            pressed: false,
        };
        let len = evt.to_bytes(&mut buf);
        let _ = socket.send(&buf[..len]);
    }

    true
}

enum UpdateOutcome {
    AlreadyUpToDate,
    UpdatedAndRelaunching,
}

fn latest_release_version() -> Option<String> {
    let releases = self_update::backends::github::ReleaseList::configure()
        .repo_owner("aprets")
        .repo_name("hotswitch")
        .build()
        .ok()?
        .fetch()
        .ok()?;
    Some(releases.first()?.version.clone())
}

fn check_for_update() -> Option<String> {
    let latest = latest_release_version()?;
    if self_update::version::bump_is_greater(self_update::cargo_crate_version!(), &latest)
        .unwrap_or(false)
    {
        Some(latest)
    } else {
        None
    }
}

fn current_app_bundle_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let app_root = exe.parent()?.parent()?.parent()?;
    if app_root.extension().and_then(|ext| ext.to_str()) == Some("app") {
        Some(app_root.to_path_buf())
    } else {
        None
    }
}

fn update_work_dir(version: &str) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("hotswitch-update-{version}-{stamp}"))
}

fn run_command(command: &mut std::process::Command, description: &str) -> Result<(), String> {
    let output = command
        .output()
        .map_err(|err| format!("{description} failed to start: {err}"))?;
    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let detail = if !stderr.is_empty() {
        stderr
    } else if !stdout.is_empty() {
        stdout
    } else {
        format!("exit status {}", output.status)
    };
    Err(format!("{description} failed: {detail}"))
}

fn write_update_helper(helper_path: &Path) -> Result<(), String> {
    let script = "#!/bin/sh\nset -eu\npid=\"$1\"\napp_path=\"$2\"\nstaged_app=\"$3\"\nwork_dir=\"$4\"\nbackup_path=\"${app_path}.old\"\nwhile kill -0 \"$pid\" 2>/dev/null; do sleep 0.2; done\nrm -rf \"$backup_path\"\nmv \"$app_path\" \"$backup_path\"\nif mv \"$staged_app\" \"$app_path\"; then\n  rm -rf \"$backup_path\"\n  open \"$app_path\"\n  rm -rf \"$work_dir\"\n  rm -f \"$0\"\nelse\n  mv \"$backup_path\" \"$app_path\"\n  exit 1\nfi\n";
    std::fs::write(helper_path, script)
        .map_err(|err| format!("failed to write update helper: {err}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o700);
        std::fs::set_permissions(helper_path, perms)
            .map_err(|err| format!("failed to chmod update helper: {err}"))?;
    }
    Ok(())
}

fn prepare_app_bundle_update(app_root: &Path) -> Result<UpdateOutcome, String> {
    let version = match check_for_update() {
        Some(version) => version,
        None => return Ok(UpdateOutcome::AlreadyUpToDate),
    };

    let app_parent = app_root
        .parent()
        .ok_or_else(|| format!("app bundle path has no parent: {}", app_root.display()))?;
    let write_probe = app_parent.join(format!(".hotswitch-write-test-{}", std::process::id()));
    std::fs::write(&write_probe, b"").map_err(|err| {
        format!(
            "cannot update {}: parent directory is not writable ({err})",
            app_root.display()
        )
    })?;
    let _ = std::fs::remove_file(&write_probe);

    let work_dir = update_work_dir(&version);
    std::fs::create_dir_all(&work_dir).map_err(|err| {
        format!(
            "failed to create update workspace {}: {err}",
            work_dir.display()
        )
    })?;
    let archive_path = work_dir.join(MACOS_RELEASE_ASSET);
    let staged_app = work_dir.join("Hotswitch.app");
    let helper_path =
        std::env::temp_dir().join(format!("hotswitch-update-helper-{}.sh", std::process::id()));
    let download_url = format!(
        "https://github.com/aprets/hotswitch/releases/download/v{version}/{MACOS_RELEASE_ASSET}"
    );

    run_command(
        std::process::Command::new("curl").args([
            "-L",
            "--fail",
            "--silent",
            "--show-error",
            "-o",
            archive_path.to_str().ok_or("invalid archive path")?,
            &download_url,
        ]),
        "download update",
    )?;

    run_command(
        std::process::Command::new("tar").args([
            "-xzf",
            archive_path.to_str().ok_or("invalid archive path")?,
            "-C",
            work_dir.to_str().ok_or("invalid workspace path")?,
        ]),
        "extract update archive",
    )?;

    if !staged_app.is_dir() {
        return Err(format!(
            "update archive did not contain {}",
            staged_app.display()
        ));
    }

    run_command(
        std::process::Command::new("codesign").args([
            "--verify",
            "--deep",
            "--strict",
            staged_app.to_str().ok_or("invalid staged app path")?,
        ]),
        "verify app signature",
    )?;

    write_update_helper(&helper_path)?;

    std::process::Command::new("/bin/sh")
        .arg(&helper_path)
        .arg(std::process::id().to_string())
        .arg(app_root)
        .arg(&staged_app)
        .arg(&work_dir)
        .spawn()
        .map_err(|err| format!("failed to launch update helper: {err}"))?;

    Ok(UpdateOutcome::UpdatedAndRelaunching)
}

fn apply_update() -> Result<UpdateOutcome, Box<dyn std::error::Error>> {
    if let Some(app_root) = current_app_bundle_path() {
        return prepare_app_bundle_update(&app_root)
            .map_err(|err| -> Box<dyn std::error::Error> { err.into() });
    }

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
    if status.updated() {
        Ok(UpdateOutcome::UpdatedAndRelaunching)
    } else {
        Ok(UpdateOutcome::AlreadyUpToDate)
    }
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

fn app_support_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home)
        .join("Library/Application Support")
        .join("Hotswitch")
}

fn receiver_address_path() -> PathBuf {
    app_support_dir().join(RECEIVER_ADDRESS_KEY)
}

fn parse_receiver_address(value: &str) -> Result<String, &'static str> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err("Enter the Windows receiver address in IP:port form.");
    }

    trimmed
        .parse::<SocketAddr>()
        .map(|addr| addr.to_string())
        .map_err(|_| "Use a valid receiver address like 10.0.0.209:24801.")
}

fn load_receiver_address() -> Option<String> {
    let path = receiver_address_path();
    let raw = std::fs::read_to_string(&path).ok()?;
    match parse_receiver_address(&raw) {
        Ok(addr) => Some(addr),
        Err(err) => {
            eprintln!(
                "ignoring invalid receiver address at {}: {err}",
                path.display()
            );
            None
        }
    }
}

fn save_receiver_address(target_addr: &str) -> bool {
    let path = receiver_address_path();
    if let Some(parent) = path.parent() {
        if let Err(err) = std::fs::create_dir_all(parent) {
            eprintln!(
                "failed to create receiver address directory {}: {err}",
                parent.display()
            );
            return false;
        }
    }

    match std::fs::write(&path, format!("{target_addr}\n")) {
        Ok(()) => true,
        Err(err) => {
            eprintln!(
                "failed to save receiver address to {}: {err}",
                path.display()
            );
            false
        }
    }
}

fn prompt_for_receiver_address(current_value: Option<&str>) -> Option<String> {
    let mtm = MainThreadMarker::new().expect("sender must prompt on the main thread");
    let mut next_value = current_value.unwrap_or_default().to_string();
    let mut error_message = None::<&'static str>;

    loop {
        let alert = NSAlert::new(mtm);
        alert.setMessageText(ns_string!("Receiver Address"));
        let info = match error_message {
            Some(message) => message,
            None => "Enter the Windows receiver address in IP:port form.",
        };
        let informative_text = format!("{info}\n\nExample: {DEFAULT_RECEIVER_PLACEHOLDER}");
        let informative = objc2_foundation::NSString::from_str(&informative_text);
        alert.setInformativeText(&informative);
        alert.addButtonWithTitle(ns_string!("Save"));
        alert.addButtonWithTitle(ns_string!("Cancel"));

        let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(320.0, 24.0));
        let field = NSTextField::initWithFrame(NSTextField::alloc(mtm), frame);
        field.setPlaceholderString(Some(ns_string!(DEFAULT_RECEIVER_PLACEHOLDER)));
        let value = objc2_foundation::NSString::from_str(&next_value);
        field.setStringValue(&value);
        alert.setAccessoryView(Some(field.as_ref()));

        if alert.runModal() != NSAlertFirstButtonReturn {
            return None;
        }

        next_value = field.stringValue().to_string();
        match parse_receiver_address(&next_value) {
            Ok(addr) => return Some(addr),
            Err(message) => error_message = Some(message),
        }
    }
}

fn resolve_receiver_address() -> Option<String> {
    if let Some(arg_addr) = std::env::args().nth(1) {
        match parse_receiver_address(&arg_addr) {
            Ok(addr) => {
                let _ = save_receiver_address(&addr);
                return Some(addr);
            }
            Err(err) => eprintln!("ignoring invalid receiver address argument: {err}"),
        }
    }

    if let Some(saved_addr) = load_receiver_address() {
        return Some(saved_addr);
    }

    let addr = prompt_for_receiver_address(None)?;
    if !save_receiver_address(&addr) {
        eprintln!("receiver address prompt succeeded but saving failed");
    }
    Some(addr)
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

fn set_login_item(enabled: bool) -> bool {
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
            Ok(()) => {
                eprintln!("wrote launch agent: {}", path.display());
                true
            }
            Err(e) => {
                eprintln!("failed to write launch agent: {e}");
                false
            }
        }
    } else {
        match std::fs::remove_file(&path) {
            Ok(()) => true,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
            Err(e) => {
                eprintln!("failed to remove launch agent: {e}");
                false
            }
        }
    }
}

fn start_audio_playback() {
    thread::spawn(move || {
        let host = cpal::default_host();
        let device = match host.default_output_device() {
            Some(d) => d,
            None => {
                eprintln!("audio: no output device");
                return;
            }
        };
        let device_name = device.name().unwrap_or_default();
        eprintln!("audio: playing on {device_name}");

        let config = cpal::StreamConfig {
            channels: audio::CHANNELS,
            sample_rate: cpal::SampleRate(audio::SAMPLE_RATE),
            buffer_size: cpal::BufferSize::Default,
        };

        let queue = Arc::new(ArrayQueue::<f32>::new(AUDIO_QUEUE_CAPACITY));
        let buf_fill = Arc::new(AtomicU32::new(0));
        let primed = Arc::new(AtomicBool::new(false));
        let underruns = Arc::new(AtomicU32::new(0));
        let trimmed = Arc::new(AtomicU32::new(0));
        let stale_packets = Arc::new(AtomicU32::new(0));
        let needs_rebuild = Arc::new(AtomicBool::new(false));

        let socket = match UdpSocket::bind(format!("0.0.0.0:{}", audio::AUDIO_PORT)) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("audio: failed to bind on port {}: {e}", audio::AUDIO_PORT);
                return;
            }
        };
        socket
            .set_read_timeout(Some(Duration::from_millis(500)))
            .ok();
        eprintln!("audio: listening on port {}", audio::AUDIO_PORT);

        let build_stream = || {
            let device = match host.default_output_device() {
                Some(d) => d,
                None => {
                    eprintln!("audio: no output device");
                    return None;
                }
            };
            let device_name = device.name().unwrap_or_default();
            eprintln!("audio: playing on {device_name}");

            let make_stream = |config: &cpal::StreamConfig| {
                let queue = queue.clone();
                let buf_fill = buf_fill.clone();
                let primed = primed.clone();
                let underruns = underruns.clone();
                let trimmed = trimmed.clone();
                let needs_rebuild = needs_rebuild.clone();
                let mut last_callback_frames = 0u32;
                let mut soft_trim_phase = 0u32;
                device.build_output_stream(
                    config,
                    move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                        let callback_frames = (data.len() / audio::CHANNELS as usize) as u32;
                        if callback_frames != last_callback_frames {
                            let callback_ms =
                                callback_frames as f32 / audio::SAMPLE_RATE as f32 * 1000.0;
                            eprintln!(
                                "audio: output callback {callback_frames} frames ({callback_ms:.1}ms)"
                            );
                            last_callback_frames = callback_frames;
                        }

                        let mut buffered = buf_fill.load(Ordering::Relaxed) as usize;

                        if buffered > AUDIO_RESET_FILL {
                            let to_drop = buffered.saturating_sub(AUDIO_TARGET_FILL);
                            for _ in 0..to_drop {
                                if queue.pop().is_some() {
                                    buf_fill.fetch_sub(1, Ordering::Relaxed);
                                    trimmed.fetch_add(1, Ordering::Relaxed);
                                } else {
                                    break;
                                }
                            }
                            buffered = buf_fill.load(Ordering::Relaxed) as usize;
                        }

                        if !primed.load(Ordering::Relaxed) {
                            if buffered < AUDIO_TARGET_FILL {
                                data.fill(0.0);
                                return;
                            }
                            primed.store(true, Ordering::Relaxed);
                        }

                        if buffered > AUDIO_BIAS_FILL {
                            soft_trim_phase = soft_trim_phase.wrapping_add(1);
                            if soft_trim_phase % 8 == 0 {
                                for _ in 0..audio::CHANNELS as usize {
                                    if queue.pop().is_some() {
                                        buf_fill.fetch_sub(1, Ordering::Relaxed);
                                        trimmed.fetch_add(1, Ordering::Relaxed);
                                    } else {
                                        break;
                                    }
                                }
                            }
                        }

                        let mut underrun = false;
                        for sample in data.iter_mut() {
                            if let Some(v) = queue.pop() {
                                *sample = v;
                                buf_fill.fetch_sub(1, Ordering::Relaxed);
                            } else {
                                *sample = 0.0;
                                underrun = true;
                            }
                        }

                        if underrun {
                            underruns.fetch_add(1, Ordering::Relaxed);
                            primed.store(false, Ordering::Relaxed);
                            buf_fill.store(queue.len() as u32, Ordering::Relaxed);
                        }
                    },
                    move |err| {
                        eprintln!("audio: playback error: {err}");
                        needs_rebuild.store(true, Ordering::SeqCst);
                    },
                    None,
                )
            };

            let mut chosen = "default".to_string();
            let mut stream = None;
            for frames in AUDIO_BUFFER_CANDIDATES {
                let trial = cpal::StreamConfig {
                    channels: audio::CHANNELS,
                    sample_rate: cpal::SampleRate(audio::SAMPLE_RATE),
                    buffer_size: cpal::BufferSize::Fixed(frames),
                };
                match make_stream(&trial) {
                    Ok(s) => {
                        chosen = format!("{frames} frames");
                        stream = Some(s);
                        break;
                    }
                    Err(e) => eprintln!("audio: output buffer {frames} frames unsupported: {e}"),
                }
            }
            let stream = match stream {
                Some(s) => s,
                None => match make_stream(&config) {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("audio: failed to build output stream: {e}");
                        return None;
                    }
                },
            };
            eprintln!("audio: using output buffer {chosen}");

            if let Err(e) = stream.play() {
                eprintln!("audio: failed to start playback: {e}");
                return None;
            }

            Some((stream, device_name))
        };

        let mut stream = None;
        let mut active_device_name = String::new();
        let mut last_device_poll = Instant::now() - Duration::from_secs(10);

        let buf_capacity = AUDIO_QUEUE_CAPACITY;
        let pkt_count = Arc::new(std::sync::atomic::AtomicU64::new(0));
        {
            let pkt_count = pkt_count.clone();
            let buf_fill = buf_fill.clone();
            let underruns = underruns.clone();
            let trimmed = trimmed.clone();
            let stale_packets = stale_packets.clone();
            thread::spawn(move || loop {
                thread::sleep(Duration::from_secs(5));
                let pkts = pkt_count.swap(0, std::sync::atomic::Ordering::Relaxed);
                let fill = buf_fill.load(std::sync::atomic::Ordering::Relaxed) as usize;
                let latency_ms =
                    fill as f32 / (audio::SAMPLE_RATE as f32 * audio::CHANNELS as f32) * 1000.0;
                let underruns = underruns.swap(0, Ordering::Relaxed);
                let trimmed = trimmed.swap(0, Ordering::Relaxed);
                let stale = stale_packets.swap(0, Ordering::Relaxed);
                eprintln!(
                    "audio: {pkts} pkts, buf {fill}/{buf_capacity} ({latency_ms:.1}ms), underruns {underruns}, trimmed {trimmed}, stale {stale}"
                );
            });
        }

        let mut buf = [0u8; 1500];
        let mut last_seq = None;
        let mut stale_seq_run = 0u32;
        loop {
            if last_device_poll.elapsed() >= Duration::from_secs(2) {
                last_device_poll = Instant::now();
                let current_default = host.default_output_device().and_then(|d| d.name().ok());
                let device_changed = match current_default {
                    Some(ref name) => name != &active_device_name,
                    None => !active_device_name.is_empty(),
                };
                let should_rebuild = stream.is_none() || device_changed;
                if should_rebuild || needs_rebuild.swap(false, Ordering::SeqCst) {
                    if device_changed {
                        match current_default {
                            Some(ref name) => {
                                eprintln!(
                                    "audio: default output changed from {} to {}",
                                    active_device_name, name
                                );
                            }
                            None => {
                                eprintln!(
                                    "audio: default output {} disappeared",
                                    active_device_name
                                );
                            }
                        }
                    }
                    if let Some((new_stream, new_device_name)) = build_stream() {
                        stream = Some(new_stream);
                        active_device_name = new_device_name;
                    } else {
                        stream = None;
                        active_device_name.clear();
                    }
                }
            }

            let n = match socket.recv(&mut buf) {
                Ok(n) => n,
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut =>
                {
                    continue;
                }
                Err(_) => continue,
            };
            if let Some((seq, _channels, raw)) = audio::audio_from_bytes(&buf[..n]) {
                if let Some(prev) = last_seq {
                    if !seq_is_newer(seq, prev) {
                        stale_seq_run = stale_seq_run.saturating_add(1);
                        if stale_seq_run >= AUDIO_SEQ_RESET_STALE_PACKETS {
                            eprintln!(
                                "audio: sequence reset detected after {stale_seq_run} stale packets ({prev} -> {seq})"
                            );
                            stale_seq_run = 0;
                            drain_audio_queue(&queue, &buf_fill, &primed);
                        } else {
                            stale_packets.fetch_add(1, Ordering::Relaxed);
                            continue;
                        }
                    } else {
                        stale_seq_run = 0;
                    }
                }
                last_seq = Some(seq);
                pkt_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                for sample in audio::raw_to_samples(raw) {
                    while queue.len() >= AUDIO_QUEUE_CAPACITY {
                        if queue.pop().is_some() {
                            buf_fill.fetch_sub(1, Ordering::Relaxed);
                            primed.store(false, Ordering::Relaxed);
                            trimmed.fetch_add(1, Ordering::Relaxed);
                        } else {
                            break;
                        }
                    }
                    if queue.push(sample).is_ok() {
                        buf_fill.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }
    });
}

fn main() {
    let log_file_path = redirect_stdio_to_log();

    let target_addr = match resolve_receiver_address() {
        Some(addr) => addr,
        None => return,
    };

    if is_login_item() {
        let _ = set_login_item(true);
    }

    eprintln!("hotswitch sender starting, target: {target_addr}");

    let socket = UdpSocket::bind("0.0.0.0:0").expect("Failed to bind UDP socket");
    socket
        .connect(&target_addr)
        .expect("Failed to connect UDP socket");
    socket.set_nonblocking(true).ok();

    start_audio_playback();
    configure_cursor_capture();
    let power_activity = start_sleep_prevention();

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
    let capture_anchor = Arc::new(std::sync::Mutex::new(None::<CGPoint>));

    // Tao event loop
    let mut event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    event_loop.set_activation_policy(ActivationPolicy::Accessory);
    let proxy = event_loop.create_proxy();
    let mtm = MainThreadMarker::new().expect("sender must start on the main thread");

    let workspace = NSWorkspace::sharedWorkspace();
    let workspace_center = workspace.notificationCenter();
    let distributed_center = NSDistributedNotificationCenter::defaultCenter();
    let lifecycle_observer = LifecycleObserver::new(mtm, proxy.clone());
    lifecycle_observer.register(&workspace_center, &distributed_center);

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
    let anchor = capture_anchor.clone();
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

    let event_tap_callback = move |_proxy: CGEventTapProxy,
                                   event_type: CGEventType,
                                   cg_ev: &CGEvent|
          -> CallbackResult {
        if matches!(
            event_type,
            CGEventType::TapDisabledByTimeout | CGEventType::TapDisabledByUserInput
        ) {
            eprintln!("WARNING: CGEventTap was disabled, re-enabling");
            let port = tp.load(Ordering::SeqCst);
            if !port.is_null() {
                unsafe {
                    CGEventTapEnable(port, true);
                }
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

                    if now_capturing {
                        cap.store(true, Ordering::SeqCst);
                        let cursor_pos = cg_ev.location();
                        eprintln!("capture started: anchor={cursor_pos:?}");
                        *anchor.lock().unwrap() = Some(cursor_pos);
                        let associate_result =
                            unsafe { CGAssociateMouseAndMouseCursorPosition(false) };
                        eprintln!("cursor capture: associate result={associate_result}");
                        let hide_result = CGDisplay::hide_cursor(&CGDisplay::main());
                        eprintln!("cursor capture: hide result={hide_result:?}");
                        let warp_result = CGDisplay::warp_mouse_cursor_position(cursor_pos);
                        eprintln!("cursor capture: warp result={warp_result:?}");
                        *adx.lock().unwrap() = 0.0;
                        *ady.lock().unwrap() = 0.0;
                    } else {
                        let _ = disengage_capture(
                            "hotkey",
                            cap.as_ref(),
                            anchor.as_ref(),
                            keys.as_ref(),
                            adx.as_ref(),
                            ady.as_ref(),
                            &sock,
                        );
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
                    if let Some(anchor_pos) = *anchor.lock().unwrap() {
                        let _ = CGDisplay::warp_mouse_cursor_position(anchor_pos);
                    }
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
                let v = cg_ev.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_1)
                    as i16;
                let h = cg_ev.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_2)
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

    tap_port.store(
        tap.mach_port().as_CFTypeRef() as *mut c_void,
        Ordering::SeqCst,
    );

    let source = tap
        .mach_port()
        .create_runloop_source(0)
        .expect("Failed to create runloop source");

    unsafe {
        CFRunLoop::get_current()
            .add_source(&source, core_foundation::runloop::kCFRunLoopCommonModes);
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
    let address_item = MenuItem::new("Receiver Address...", true, None);
    let log_item = MenuItem::new("Show Log", true, None);
    let login_item = CheckMenuItem::new("Start on Login", true, is_login_item(), None);
    let quit_item = MenuItem::new("Quit", true, None);
    let _ = menu.append_items(&[
        &status_item,
        &PredefinedMenuItem::separator(),
        &update_item,
        &address_item,
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
    let address_id = address_item.id().clone();
    let log_id = log_item.id().clone();
    let login_id = login_item.id().clone();
    let quit_id = quit_item.id().clone();
    let mut login_checked = is_login_item();
    let mut configured_target_addr = target_addr.clone();
    let _lifecycle_guard = (workspace_center, distributed_center, lifecycle_observer);
    let mut power_activity = Some(power_activity);

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

        if let tao::event::Event::NewEvents(tao::event::StartCause::ResumeTimeReached { .. }) =
            &event
        {
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
                    let _ =
                        tray.set_icon_with_as_template(Some(make_icon(220, 38, 38, true)), false);
                    let _ = tray.set_tooltip(Some("Hotswitch — No receiver"));
                    let deadline = Instant::now() + Duration::from_secs(2);
                    flash_until = Some(deadline);
                    *control_flow = ControlFlow::WaitUntil(deadline);
                }
                UserEvent::ReleaseCapture(reason) => {
                    if disengage_capture(
                        reason,
                        capturing.as_ref(),
                        capture_anchor.as_ref(),
                        held_keys.as_ref(),
                        accum_dx.as_ref(),
                        accum_dy.as_ref(),
                        &socket,
                    ) {
                        let new_state = compute_state();
                        if new_state != last_state {
                            let (icon, tmpl) = new_state.icon();
                            let _ = tray.set_icon_with_as_template(Some(icon), tmpl);
                            let _ = tray.set_tooltip(Some(new_state.tooltip()));
                            status_item.set_text(new_state.status_text());
                            last_state = new_state;
                        }
                    }
                }
                UserEvent::Menu(me) => {
                    if me.id == quit_id {
                        let _ = disengage_capture(
                            "quit",
                            capturing.as_ref(),
                            capture_anchor.as_ref(),
                            held_keys.as_ref(),
                            accum_dx.as_ref(),
                            accum_dy.as_ref(),
                            &socket,
                        );
                        if let Some(activity) = power_activity.take() {
                            activity.stop();
                        }
                        *control_flow = ControlFlow::Exit;
                    } else if me.id == update_id {
                        let exe = std::env::current_exe().expect("Failed to get current exe path");
                        update_item.set_text("Updating...");
                        update_item.set_enabled(false);
                        match apply_update() {
                            Ok(outcome) => match outcome {
                                UpdateOutcome::UpdatedAndRelaunching => {
                                    eprintln!("relaunching: {exe:?}");
                                    if current_app_bundle_path().is_none() {
                                        match std::process::Command::new(&exe).spawn() {
                                            Ok(_) => {}
                                            Err(e) => eprintln!("relaunch failed: {e}"),
                                        }
                                    }
                                    if let Some(activity) = power_activity.take() {
                                        activity.stop();
                                    }
                                    *control_flow = ControlFlow::Exit;
                                    return;
                                }
                                UpdateOutcome::AlreadyUpToDate => {
                                    update_item.set_text("Already up to date");
                                    let deadline = Instant::now() + Duration::from_secs(5);
                                    reset_update_at = Some(deadline);
                                    *control_flow = ControlFlow::WaitUntil(deadline);
                                }
                            },
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
                        let _ = std::process::Command::new("open")
                            .arg("-t")
                            .arg(&log_file_path)
                            .spawn();
                    } else if me.id == address_id {
                        if let Some(new_addr) =
                            prompt_for_receiver_address(Some(&configured_target_addr))
                        {
                            if new_addr != configured_target_addr
                                && save_receiver_address(&new_addr)
                            {
                                configured_target_addr = new_addr;
                                if login_checked {
                                    let _ = set_login_item(true);
                                }
                                let _ = disengage_capture(
                                    "receiver address changed",
                                    capturing.as_ref(),
                                    capture_anchor.as_ref(),
                                    held_keys.as_ref(),
                                    accum_dx.as_ref(),
                                    accum_dy.as_ref(),
                                    &socket,
                                );
                                if let Some(activity) = power_activity.take() {
                                    activity.stop();
                                }
                                match std::process::Command::new(
                                    std::env::current_exe()
                                        .expect("Failed to get current exe path"),
                                )
                                .spawn()
                                {
                                    Ok(_) => *control_flow = ControlFlow::Exit,
                                    Err(err) => {
                                        eprintln!("relaunch after address change failed: {err}")
                                    }
                                }
                            }
                        }
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
