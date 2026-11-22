mod ddc;
mod monitor;
mod passthrough;
mod settings;
mod settings_window;

use core_foundation::{
    base::TCFType, boolean::CFBoolean, dictionary::CFDictionary, runloop::CFRunLoop,
    string::CFString,
};
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
use monitor::{Job, Report, Want};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{define_class, msg_send, sel, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSWorkspace, NSWorkspaceDidWakeNotification, NSWorkspaceScreensDidSleepNotification,
    NSWorkspaceScreensDidWakeNotification, NSWorkspaceSessionDidResignActiveNotification,
    NSWorkspaceWillSleepNotification,
};
use objc2_foundation::{
    ns_string, NSActivityOptions, NSDistributedNotificationCenter, NSNotification,
    NSNotificationCenter, NSObject, NSObjectProtocol, NSProcessInfo,
};
use passthrough::{MicAccess, Passthrough};
use settings::{
    parse_receiver_address, AudioRoute, Hotkey, Mode, Settings, KEY_DELETE, KEY_ESCAPE,
};
use settings_window::{Action, SettingsWindow};
use std::{
    collections::HashSet,
    ffi::c_void,
    net::UdpSocket,
    path::{Path, PathBuf},
    ptr,
    sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU8, Ordering},
    sync::{mpsc, Arc, Mutex},
    thread,
    time::SystemTime,
    time::{Duration, Instant},
};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tao::platform::macos::{ActivationPolicy, EventLoopExtMacOS};
use tray_icon::{
    menu::{
        accelerator::{Accelerator, Code, Modifiers},
        CheckMenuItem, IsMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem,
    },
    Icon, TrayIcon, TrayIconBuilder,
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

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXIsProcessTrustedWithOptions(options: *const c_void) -> bool;
}

const AUDIO_BUFFER_CANDIDATES: [u32; 2] = [128, 256];
const AUDIO_QUEUE_CAPACITY: usize = (audio::SAMPLE_RATE as usize * audio::CHANNELS as usize) / 12; // ~83ms
const AUDIO_TARGET_FILL: usize = (audio::SAMPLE_RATE as usize * audio::CHANNELS as usize) / 66; // ~15ms
const AUDIO_BIAS_FILL: usize = (audio::SAMPLE_RATE as usize * audio::CHANNELS as usize) / 40; // ~25ms
const AUDIO_RESET_FILL: usize = (audio::SAMPLE_RATE as usize * audio::CHANNELS as usize) / 20; // ~50ms
const AUDIO_SEQ_RESET_STALE_PACKETS: u32 = 16;
const MACOS_RELEASE_ASSET: &str = "hotswitch-sender-aarch64-apple-darwin.tar.gz";
/// Device changes come in bursts (a dock rebooting); act once they settle.
const AUDIO_SETTLE: Duration = Duration::from_secs(1);
/// Displays reconnecting or the Mac waking come in bursts too; an owed hand-back waits this long.
const DISPLAY_SETTLE: Duration = Duration::from_secs(2);
/// Retry a passthrough that failed to start (no DDC involved).
const PASSTHROUGH_RETRY: Duration = Duration::from_secs(5);
/// Restart a passthrough whose input has delivered nothing for this long.
const PASSTHROUGH_STALL: Duration = Duration::from_secs(2);
/// How long quitting waits for an owed hand-back.
const HAND_BACK_WAIT: Duration = Duration::from_secs(6);

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

#[derive(Debug)]
enum UserEvent {
    StateChanged,
    ReleaseCapture(&'static str),
    UpdateAvailable(String),
    Menu(tray_icon::menu::MenuEvent),
    /// A mode's shortcut, from the event tap.
    Hotkey(Mode),
    HotkeyRecorded(Mode, Recorded),
    Monitor(Report),
    /// The Mac is about to sleep.
    WillSleep,
    /// The Mac or its screens woke, or displays were reconfigured.
    DisplaysChanged,
    Settings(Action),
    /// Audio devices came or went, or the default output changed.
    AudioDevicesChanged,
}

#[derive(Debug)]
enum Recorded {
    Set(Hotkey),
    Clear,
    Cancel,
}

#[derive(Clone, Copy, Debug)]
enum HookKind {
    ReceiverConnected,
    ReceiverDisconnected,
    ModeChanged,
}

impl HookKind {
    fn filename(self) -> &'static str {
        match self {
            Self::ReceiverConnected => "on-receiver-connected",
            Self::ReceiverDisconnected => "on-receiver-disconnected",
            Self::ModeChanged => "on-mode-changed",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::ReceiverConnected => "receiver-connected",
            Self::ReceiverDisconnected => "receiver-disconnected",
            Self::ModeChanged => "mode-changed",
        }
    }
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

        #[unsafe(method(handleSleep:))]
        fn handle_sleep(&self, _notification: &NSNotification) {
            let proxy = &self.ivars().proxy;
            let _ = proxy.send_event(UserEvent::ReleaseCapture("macOS lifecycle"));
            let _ = proxy.send_event(UserEvent::WillSleep);
        }

        #[unsafe(method(handleWake:))]
        fn handle_wake(&self, _notification: &NSNotification) {
            let _ = self.ivars().proxy.send_event(UserEvent::DisplaysChanged);
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
                sel!(handleSleep:),
                Some(NSWorkspaceWillSleepNotification),
                None,
            );
            for wake in [
                NSWorkspaceDidWakeNotification,
                NSWorkspaceScreensDidWakeNotification,
            ] {
                workspace_center.addObserver_selector_name_object(
                    self,
                    sel!(handleWake:),
                    Some(wake),
                    None,
                );
            }
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

fn engage_capture(
    cursor_pos: CGPoint,
    capturing: &AtomicBool,
    capture_anchor: &Mutex<Option<CGPoint>>,
    accum_dx: &Mutex<f64>,
    accum_dy: &Mutex<f64>,
) {
    capturing.store(true, Ordering::SeqCst);
    eprintln!("capture started: anchor={cursor_pos:?}");
    *capture_anchor.lock().unwrap() = Some(cursor_pos);
    let associate_result = unsafe { CGAssociateMouseAndMouseCursorPosition(false) };
    eprintln!("cursor capture: associate result={associate_result}");
    let hide_result = CGDisplay::hide_cursor(&CGDisplay::main());
    eprintln!("cursor capture: hide result={hide_result:?}");
    let warp_result = CGDisplay::warp_mouse_cursor_position(cursor_pos);
    eprintln!("cursor capture: warp result={warp_result:?}");
    *accum_dx.lock().unwrap() = 0.0;
    *accum_dy.lock().unwrap() = 0.0;
}

fn cursor_position() -> CGPoint {
    CGEventSource::new(CGEventSourceStateID::CombinedSessionState)
        .ok()
        .and_then(|source| CGEvent::new(source).ok())
        .map(|event| event.location())
        .unwrap_or(CGPoint::new(0.0, 0.0))
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
    settings::support_dir()
}

// --- Hooks ---

fn hooks_dir() -> PathBuf {
    app_support_dir().join("hooks")
}

fn hook_path(kind: HookKind) -> PathBuf {
    hooks_dir().join(kind.filename())
}

fn run_hook(kind: HookKind, wait: bool, args: &[&str]) {
    let path = hook_path(kind);
    let label = kind.label();
    // Hooks switch real hardware (BetterDisplay, DDC), so a dry run never runs them.
    if settings::dry_run() {
        eprintln!("hook {label}: skipped in a dry run");
        return;
    }
    let metadata = match std::fs::metadata(&path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("hook {label}: no script at {}", path.display());
            return;
        }
        Err(e) => {
            eprintln!("hook {label}: failed to inspect {}: {e}", path.display());
            return;
        }
    };

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            eprintln!("hook {label}: script is not executable: {}", path.display());
            return;
        }
    }

    let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
    let run = move || {
        let start = Instant::now();
        eprintln!(
            "hook {label}: running {} {}",
            path.display(),
            args.join(" ")
        );
        match std::process::Command::new(&path).args(&args).status() {
            Ok(status) => {
                eprintln!(
                    "hook {label}: exited {status} in {}ms",
                    start.elapsed().as_millis()
                );
            }
            Err(e) => eprintln!("hook {label}: failed to run {}: {e}", path.display()),
        }
    };

    if wait {
        run();
    } else {
        thread::spawn(run);
    }
}

// --- Log file ---

fn log_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home)
        .join("Library/Logs")
        .join(if settings::dry_run() {
            "hotswitch-sender-dry-run.log"
        } else {
            "hotswitch-sender.log"
        })
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
    if settings::dry_run() {
        eprintln!("set_login_item: skipped in a dry run");
        return false;
    }
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

// --- Event tap ---

#[derive(Clone)]
struct TapContext {
    capturing: Arc<AtomicBool>,
    held_keys: Arc<Mutex<HashSet<u16>>>,
    accum_dx: Arc<Mutex<f64>>,
    accum_dy: Arc<Mutex<f64>>,
    capture_anchor: Arc<Mutex<Option<CGPoint>>>,
    socket: Arc<UdpSocket>,
    hotkeys: Arc<Mutex<[Option<Hotkey>; 3]>>,
    /// 0, or the index + 1 of the mode whose shortcut the settings window is recording.
    recording: Arc<AtomicU8>,
    proxy: EventLoopProxy<UserEvent>,
}

/// The keyboard and mouse tap. Fails until Hotswitch has Accessibility access.
fn create_event_tap(ctx: &TapContext) -> Option<CGEventTap<'static>> {
    let ctx = ctx.clone();
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
            let hotkey = Hotkey::from_event(keycode, cg_ev.get_flags());

            let recording = ctx.recording.load(Ordering::SeqCst) as usize;
            if let Some(&mode) = recording.checked_sub(1).and_then(|i| Mode::ALL.get(i)) {
                let plain = !(hotkey.ctrl || hotkey.option || hotkey.shift || hotkey.command);
                let result = match keycode {
                    KEY_ESCAPE if plain => Some(Recorded::Cancel),
                    KEY_DELETE if plain => Some(Recorded::Clear),
                    _ if hotkey.is_usable() => Some(Recorded::Set(hotkey)),
                    _ => None,
                };
                if let Some(result) = result {
                    ctx.recording.store(0, Ordering::SeqCst);
                    let _ = ctx
                        .proxy
                        .send_event(UserEvent::HotkeyRecorded(mode, result));
                }
                return CallbackResult::Drop;
            }

            let mode = ctx
                .hotkeys
                .lock()
                .unwrap()
                .iter()
                .position(|h| h.as_ref() == Some(&hotkey));
            if let Some(i) = mode {
                if cg_ev.get_integer_value_field(EventField::KEYBOARD_EVENT_AUTOREPEAT) == 0 {
                    let _ = ctx.proxy.send_event(UserEvent::Hotkey(Mode::ALL[i]));
                }
                return CallbackResult::Drop;
            }
        }

        if !ctx.capturing.load(Ordering::SeqCst) {
            return CallbackResult::Keep;
        }

        let sock = &ctx.socket;
        let mut buf = [0u8; 64];

        match event_type {
            CGEventType::MouseMoved
            | CGEventType::LeftMouseDragged
            | CGEventType::RightMouseDragged
            | CGEventType::OtherMouseDragged => {
                let raw_dx = cg_ev.get_double_value_field(EventField::MOUSE_EVENT_DELTA_X);
                let raw_dy = cg_ev.get_double_value_field(EventField::MOUSE_EVENT_DELTA_Y);
                let mut ax = ctx.accum_dx.lock().unwrap();
                let mut ay = ctx.accum_dy.lock().unwrap();
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
                    if let Some(anchor_pos) = *ctx.capture_anchor.lock().unwrap() {
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
                ctx.held_keys.lock().unwrap().insert(keycode);
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
                ctx.held_keys.lock().unwrap().remove(&keycode);
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
                let mut k = ctx.held_keys.lock().unwrap();
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
    .ok()?;

    tap_port.store(
        tap.mach_port().as_CFTypeRef() as *mut c_void,
        Ordering::SeqCst,
    );

    let source = tap.mach_port().create_runloop_source(0).ok()?;
    unsafe {
        CFRunLoop::get_current()
            .add_source(&source, core_foundation::runloop::kCFRunLoopCommonModes);
    }
    Some(tap)
}

/// Tells the app when displays are added, removed or rearranged (a dock coming back).
fn watch_displays(proxy: EventLoopProxy<UserEvent>) {
    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGDisplayRegisterReconfigurationCallback(
            callback: unsafe extern "C" fn(u32, u32, *mut c_void),
            user_info: *mut c_void,
        ) -> i32;
    }
    unsafe extern "C" fn changed(_display: u32, flags: u32, user_info: *mut c_void) {
        // kCGDisplayBeginConfigurationFlag: wait for the matching "done" call.
        if flags & 1 == 0 {
            let proxy = &*(user_info as *const Mutex<EventLoopProxy<UserEvent>>);
            let _ = proxy.lock().unwrap().send_event(UserEvent::DisplaysChanged);
        }
    }
    let user_info = Box::into_raw(Box::new(Mutex::new(proxy))) as *mut c_void;
    unsafe { CGDisplayRegisterReconfigurationCallback(changed, user_info) };
}

/// Shows macOS's "allow Accessibility" prompt.
fn request_accessibility() {
    let key = CFString::new("AXTrustedCheckOptionPrompt");
    let options = CFDictionary::from_CFType_pairs(&[(key, CFBoolean::true_value())]);
    unsafe { AXIsProcessTrustedWithOptions(options.as_concrete_TypeRef() as *const c_void) };
}

/// Holds an exclusive lock on `instance.lock` for the life of the process.
fn single_instance() -> Option<std::fs::File> {
    let dir = settings::real_support_dir();
    std::fs::create_dir_all(&dir).ok()?;
    let file = std::fs::File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("instance.lock"))
        .ok()?;
    file.try_lock().ok()?;
    Some(file)
}

/// Publish "connected" only if the receiver address hasn't changed since `seen`. Holding the
/// generation lock while publishing means an address change can't slip in between.
fn publish_if_current(generation: &Mutex<u32>, seen: u32, publish: impl FnOnce()) -> bool {
    let current = generation.lock().unwrap();
    if *current == seen {
        publish();
        true
    } else {
        false
    }
}

/// A new receiver address: not connected until a heartbeat arrives from it.
fn start_new_receiver(generation: &Mutex<u32>, connected: &AtomicBool) {
    let mut current = generation.lock().unwrap();
    *current += 1;
    connected.store(false, Ordering::SeqCst);
}

fn store_state(
    capturing: &AtomicBool,
    receiver_connected: &AtomicBool,
    app_state: &AtomicU8,
) -> AppState {
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

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("ddc") {
        ddc::cli(&args[2..]);
        return;
    }

    // One Hotswitch at a time (dry run or not), so only one process owns monitor claims.
    // Taken before the log is opened, so a second copy doesn't truncate the first one's log.
    let Some(_instance) = single_instance() else {
        eprintln!("Hotswitch is already running");
        return;
    };
    let log_file_path = redirect_stdio_to_log();
    let support_dir = app_support_dir();
    let mut settings = Settings::load(&support_dir);
    if let Some(arg) = args.get(1) {
        match parse_receiver_address(arg) {
            Ok(addr) => settings.receiver = Some(addr),
            Err(err) => eprintln!("ignoring invalid receiver address argument: {err}"),
        }
    }
    if let Err(e) = settings.save(&support_dir) {
        eprintln!("settings: failed to save: {e}");
    }

    if is_login_item() {
        let _ = set_login_item(true);
    }

    eprintln!(
        "hotswitch sender starting, target: {}",
        settings.receiver.as_deref().unwrap_or("none")
    );

    let socket = UdpSocket::bind("0.0.0.0:0").expect("Failed to bind UDP socket");
    if let Some(addr) = &settings.receiver {
        if let Err(err) = socket.connect(addr) {
            eprintln!("failed to connect UDP socket to {addr}: {err}");
        }
    }
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
    // Bumped when the receiver address changes, so the heartbeat starts over.
    let receiver_generation = Arc::new(Mutex::new(0u32));
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
        move || -> AppState { store_state(&capturing, &receiver_connected, &app_state) }
    };

    // --- Heartbeat listener thread ---
    let sock_rx = socket.try_clone().expect("Failed to clone socket");
    let rc = receiver_connected.clone();
    let proxy_hb = proxy.clone();
    let compute_hb = compute_state.clone();
    let generation = receiver_generation.clone();
    thread::spawn(move || {
        eprintln!("waiting for receiver...");
        let mut buf = [0u8; 8];
        let mut was_connected = false;
        let mut last_recv = Instant::now();
        let mut seen_generation = 0;
        loop {
            let current = *generation.lock().unwrap();
            if current != seen_generation {
                seen_generation = current;
                was_connected = false;
                last_recv = Instant::now();
            }
            match sock_rx.recv(&mut buf) {
                Ok(n) if Event::from_bytes(&buf[..n]).is_some() => {
                    publish_if_current(&generation, seen_generation, || {
                        if !was_connected {
                            eprintln!("receiver connected");
                            rc.store(true, Ordering::SeqCst);
                            was_connected = true;
                            compute_hb();
                            let _ = proxy_hb.send_event(UserEvent::StateChanged);
                        }
                        last_recv = Instant::now();
                    });
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

    {
        let proxy = Mutex::new(proxy.clone());
        passthrough::watch_devices(move || {
            let _ = proxy
                .lock()
                .unwrap()
                .send_event(UserEvent::AudioDevicesChanged);
        });
    }
    watch_displays(proxy.clone());
    let monitor_jobs = {
        let proxy = Mutex::new(proxy.clone());
        monitor::spawn(support_dir.clone(), move |report| {
            eprintln!("monitor: {report:?}");
            let _ = proxy.lock().unwrap().send_event(UserEvent::Monitor(report));
        })
    };

    // --- Event tap ---
    let tap_ctx = TapContext {
        capturing: capturing.clone(),
        held_keys: held_keys.clone(),
        accum_dx: accum_dx.clone(),
        accum_dy: accum_dy.clone(),
        capture_anchor: capture_anchor.clone(),
        socket: Arc::new(socket.try_clone().expect("Failed to clone socket")),
        hotkeys: Arc::new(Mutex::new(settings.hotkeys)),
        recording: Arc::new(AtomicU8::new(0)),
        proxy: proxy.clone(),
    };
    let tap = create_event_tap(&tap_ctx);
    if tap.is_none() {
        eprintln!("event tap unavailable: Hotswitch needs Accessibility access");
        request_accessibility();
    }

    // --- Tray icon ---
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

    let ui = MenuUi {
        menu: Menu::new(),
        modes: Mode::ALL.map(|mode| CheckMenuItem::new(mode.label(), true, false, None)),
        monitor_line: MenuItem::new("", false, None),
        level_line: MenuItem::new("", false, None),
        route_line: MenuItem::new("", false, None),
        pc_line: MenuItem::new("", false, None),
        access_item: MenuItem::new("Allow Accessibility Access…", true, None),
        settings_item: MenuItem::new(
            "Settings…",
            true,
            Some(Accelerator::new(Some(Modifiers::SUPER), Code::Comma)),
        ),
        log_item: MenuItem::new("Show Log", true, None),
        update_item,
        quit_item: MenuItem::new(
            "Quit Hotswitch",
            true,
            Some(Accelerator::new(Some(Modifiers::SUPER), Code::KeyQ)),
        ),
        ps5_shown: false,
        access_shown: false,
    };
    if settings::dry_run() {
        let _ = ui.menu.append_items(&[
            &MenuItem::new("Dry run: monitors simulated, hooks off", false, None),
            &PredefinedMenuItem::separator(),
        ]);
    }
    let _ = ui.menu.append_items(&[
        &ui.modes[0],
        &ui.modes[1],
        &ui.modes[2],
        &PredefinedMenuItem::separator(),
        &ui.pc_line,
        &PredefinedMenuItem::separator(),
        &ui.settings_item,
        &ui.log_item,
        &ui.update_item,
        &PredefinedMenuItem::separator(),
        &ui.quit_item,
    ]);

    let menu_proxy = proxy.clone();
    MenuEvent::set_event_handler(Some(move |evt| {
        let _ = menu_proxy.send_event(UserEvent::Menu(evt));
    }));

    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(ui.menu.clone()))
        .with_tooltip("Hotswitch")
        .with_icon(make_icon(0, 0, 0, false))
        .with_icon_as_template(true)
        .with_title(if settings::dry_run() { "DRY RUN" } else { "" })
        .build()
        .expect("Failed to create tray icon");

    let mut app = App {
        mtm,
        support_dir,
        mode: Mode::Mac,
        monitor_jobs,
        receiver_generation,
        display_check_at: None,
        passthrough_retry_at: None,
        input_watch: None,
        torn_down: false,
        settings,
        socket,
        tap_ctx,
        tap,
        receiver_connected,
        app_state,
        passthrough: None,
        passthrough_error: None,
        monitor_line: String::new(),
        awaiting_mic: None,
        ui,
        tray,
        icon_look: None,
        settings_window: None,
        proxy,
        login_checked: is_login_item(),
        receiver_hook_active: false,
        flash_until: None,
        reset_update_at: None,
        next_tick: None,
        audio_check_at: None,
        quitting: false,
        power_activity: Some(power_activity),
        log_file_path,
        _lifecycle_guard: (workspace_center, distributed_center, lifecycle_observer),
    };

    app.refresh_menu();
    app.schedule_tick();

    event_loop.run(move |event, _, control_flow| app.handle(event, control_flow));
}

/// Items above the modes: the dry-run line and its separator.
fn menu_top() -> usize {
    if settings::dry_run() {
        2
    } else {
        0
    }
}

struct MenuUi {
    menu: Menu,
    modes: [CheckMenuItem; 3],
    monitor_line: MenuItem,
    level_line: MenuItem,
    route_line: MenuItem,
    pc_line: MenuItem,
    access_item: MenuItem,
    settings_item: MenuItem,
    log_item: MenuItem,
    update_item: MenuItem,
    quit_item: MenuItem,
    ps5_shown: bool,
    access_shown: bool,
}

struct App {
    mtm: MainThreadMarker,
    support_dir: PathBuf,
    settings: Settings,
    mode: Mode,
    socket: UdpSocket,
    tap_ctx: TapContext,
    tap: Option<CGEventTap<'static>>,
    receiver_connected: Arc<AtomicBool>,
    app_state: Arc<AtomicU8>,
    passthrough: Option<Passthrough>,
    passthrough_error: Option<String>,
    /// The DDC worker. A claim left by a crash is handed back as it starts.
    monitor_jobs: mpsc::Sender<Job>,
    receiver_generation: Arc<Mutex<u32>>,
    /// Debounced retry of an owed hand-back after wake or display changes.
    display_check_at: Option<Instant>,
    passthrough_retry_at: Option<Instant>,
    /// Watchdog: the input's frame count and when it last moved.
    input_watch: Option<(usize, Instant)>,
    torn_down: bool,
    monitor_line: String,
    /// PS5 mode is waiting for the microphone prompt before switching the monitor away.
    awaiting_mic: Option<Instant>,
    ui: MenuUi,
    tray: TrayIcon,
    icon_look: Option<(Mode, bool)>,
    settings_window: Option<SettingsWindow>,
    proxy: EventLoopProxy<UserEvent>,
    login_checked: bool,
    receiver_hook_active: bool,
    flash_until: Option<Instant>,
    reset_update_at: Option<Instant>,
    next_tick: Option<Instant>,
    /// Debounced check of the passthrough's devices after a device change.
    audio_check_at: Option<Instant>,
    quitting: bool,
    power_activity: Option<PowerActivity>,
    log_file_path: PathBuf,
    _lifecycle_guard: (
        Retained<NSNotificationCenter>,
        Retained<NSDistributedNotificationCenter>,
        Retained<LifecycleObserver>,
    ),
}

impl App {
    fn handle(&mut self, event: tao::event::Event<UserEvent>, control_flow: &mut ControlFlow) {
        match event {
            // Also the path for a native termination (Tao's applicationWillTerminate).
            tao::event::Event::LoopDestroyed => self.teardown("exit"),
            tao::event::Event::UserEvent(ue) => self.on_user_event(ue),
            _ => {}
        }
        // Due timers run whatever woke the loop, so a late one can't leave it spinning.
        self.on_timers();

        let next_wake = [
            self.flash_until,
            self.reset_update_at,
            self.next_tick,
            self.audio_check_at,
            self.display_check_at,
            self.passthrough_retry_at,
        ]
        .into_iter()
        .flatten()
        .min();
        *control_flow = match (self.quitting, next_wake) {
            (true, _) => ControlFlow::Exit,
            (false, Some(t)) => ControlFlow::WaitUntil(t),
            (false, None) => ControlFlow::Wait,
        };
    }

    fn on_timers(&mut self) {
        let now = Instant::now();
        if self.flash_until.is_some_and(|t| now >= t) {
            self.flash_until = None;
            self.icon_look = None;
            self.refresh_icon();
        }
        if self.reset_update_at.is_some_and(|t| now >= t) {
            self.reset_update_at = None;
            self.ui.update_item.set_text("Check for Updates");
        }
        if self.audio_check_at.is_some_and(|t| now >= t) {
            self.audio_check_at = None;
            self.check_audio();
        }
        if self.passthrough_retry_at.is_some_and(|t| now >= t) {
            self.passthrough_retry_at = None;
            if self.wants_passthrough() && self.passthrough.is_none() {
                self.start_passthrough();
            }
        }
        if self.display_check_at.is_some_and(|t| now >= t) {
            self.display_check_at = None;
            // Only acts on an owed hand-back; a refused take is never retried on its own.
            let _ = self.monitor_jobs.send(Job::Reconcile);
        }
        if self.next_tick.is_some_and(|t| now >= t) {
            self.tick();
        }
    }

    /// After audio devices settle: rebind input and output if theirs went away and came back,
    /// otherwise follow the default output, or start if the input has just appeared.
    fn check_audio(&mut self) {
        if !self.wants_passthrough() {
            return;
        }
        match self.passthrough.as_ref().map(|p| p.needs_rebind()) {
            Some(true) => {
                eprintln!("passthrough: audio devices changed, rebinding input and output");
                self.passthrough = None;
                self.start_passthrough();
            }
            Some(false) => {
                if let Some(p) = &mut self.passthrough {
                    p.follow_default_output();
                }
            }
            None => self.start_passthrough(),
        }
        self.refresh_status_lines();
    }

    fn on_user_event(&mut self, event: UserEvent) {
        match event {
            UserEvent::StateChanged => {
                let receiver_is_connected = self.receiver_connected.load(Ordering::SeqCst);
                if receiver_is_connected && !self.receiver_hook_active {
                    self.receiver_hook_active = true;
                    run_hook(HookKind::ReceiverConnected, false, &[]);
                } else if !receiver_is_connected && self.receiver_hook_active {
                    self.receiver_hook_active = false;
                    run_hook(HookKind::ReceiverDisconnected, false, &[]);
                }
                self.refresh_menu();
                self.refresh_settings_note();
            }
            UserEvent::UpdateAvailable(ver) => {
                self.ui.update_item.set_text(format!("Update to v{ver}"));
            }
            UserEvent::ReleaseCapture(reason) => {
                if self.release_capture(reason) && self.mode == Mode::Windows {
                    self.mode = Mode::Mac;
                    run_hook(HookKind::ModeChanged, false, &["windows", "mac"]);
                    self.refresh_menu();
                }
            }
            UserEvent::Hotkey(mode) => self.switch_to(mode, true),
            UserEvent::HotkeyRecorded(mode, result) => self.hotkey_recorded(mode, result),
            UserEvent::Monitor(result) => self.monitor_result(result),
            UserEvent::Settings(action) => self.settings_action(action),
            UserEvent::AudioDevicesChanged => {
                self.audio_check_at = Some(Instant::now() + AUDIO_SETTLE);
            }
            // Sleep ends PS5 mode: its audio stops anyway. If the hand-back can't finish
            // before the Mac sleeps, it is owed and runs after wake.
            UserEvent::WillSleep => {
                if self.mode == Mode::Ps5 {
                    self.switch_to(Mode::Mac, false);
                }
            }
            UserEvent::DisplaysChanged => {
                self.display_check_at = Some(Instant::now() + DISPLAY_SETTLE);
            }
            UserEvent::Menu(me) => self.menu_event(me),
        }
    }

    fn menu_event(&mut self, me: MenuEvent) {
        if let Some(i) = self.ui.modes.iter().position(|m| m.id() == &me.id) {
            self.switch_to(Mode::ALL[i], false);
            self.refresh_menu();
        } else if me.id == *self.ui.quit_item.id() {
            self.teardown("quit");
            self.quitting = true;
        } else if me.id == *self.ui.settings_item.id() {
            self.open_settings();
        } else if me.id == *self.ui.access_item.id() {
            request_accessibility();
            let _ = std::process::Command::new("open")
                .arg(
                    "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility",
                )
                .spawn();
        } else if me.id == *self.ui.log_item.id() {
            let _ = std::process::Command::new("open")
                .arg("-t")
                .arg(&self.log_file_path)
                .spawn();
        } else if me.id == *self.ui.update_item.id() {
            self.check_for_updates();
        }
    }

    /// `toggle`: a shortcut for the current mode goes back to Mac.
    fn switch_to(&mut self, requested: Mode, toggle: bool) {
        let target = if toggle && requested == self.mode {
            Mode::Mac
        } else {
            requested
        };
        if target == self.mode {
            return;
        }
        if target == Mode::Windows
            && !(self.receiver_connected.load(Ordering::SeqCst) && self.tap.is_some())
        {
            self.flash_blocked();
            return;
        }

        let from = self.mode;
        eprintln!("mode: {} -> {}", from.key(), target.key());
        if from == Mode::Windows {
            self.release_capture("mode switch");
        }
        self.mode = target;

        // Audio follows the mode's route; the UDP listener runs regardless.
        self.passthrough = None;
        self.passthrough_error = None;
        self.awaiting_mic = None;
        let wants_passthrough = self.wants_passthrough();
        if wants_passthrough {
            self.start_passthrough();
        }

        // Monitor input.
        match self.settings.monitor_input(target) {
            Some(input) => {
                if wants_passthrough && matches!(passthrough::mic_access(), MicAccess::Undetermined)
                {
                    passthrough::request_mic_access();
                    self.awaiting_mic = Some(Instant::now());
                    self.monitor_line = "Allow the microphone to continue".into();
                } else {
                    self.take_monitor(input);
                }
            }
            // The worker hands back whatever it holds, using the claim's own monitor and inputs.
            None => {
                let _ = self.monitor_jobs.send(Job::Want(Want::Release));
            }
        }

        if target == Mode::Windows {
            let (anchor, dx, dy) = (
                &self.tap_ctx.capture_anchor,
                &self.tap_ctx.accum_dx,
                &self.tap_ctx.accum_dy,
            );
            engage_capture(cursor_position(), &self.tap_ctx.capturing, anchor, dx, dy);
        }
        store_state(
            &self.tap_ctx.capturing,
            &self.receiver_connected,
            &self.app_state,
        );
        run_hook(HookKind::ModeChanged, false, &[from.key(), target.key()]);
        self.refresh_menu();
        self.schedule_tick();
    }

    /// The monitor and input are fixed here; later Settings edits don't change this request.
    fn take_monitor(&mut self, input: u16) {
        self.monitor_line = format!("Switching to {}…", ddc::input_name(input));
        let _ = self.monitor_jobs.send(Job::Want(Want::Hold {
            monitor: self.settings.monitor.clone(),
            input,
            restore: self.settings.mac_input,
            detach: self.settings.detach_display,
        }));
    }

    fn wants_passthrough(&self) -> bool {
        matches!(
            self.settings.audio_route(self.mode),
            Some(AudioRoute::Passthrough { .. })
        )
    }

    fn start_passthrough(&mut self) {
        let Some(AudioRoute::Passthrough { input }) = self.settings.audio_route(self.mode) else {
            return;
        };
        match Passthrough::start(&input) {
            Ok(p) => {
                eprintln!("passthrough: started from {input}");
                self.passthrough = Some(p);
                self.passthrough_error = None;
                self.passthrough_retry_at = None;
                self.input_watch = None;
            }
            Err(e) => {
                if self.passthrough_error.as_deref() != Some(e.as_str()) {
                    eprintln!("passthrough: {e}");
                }
                self.passthrough_error = Some(e);
                self.passthrough_retry_at = Some(Instant::now() + PASSTHROUGH_RETRY);
            }
        }
    }

    fn release_capture(&self, reason: &str) -> bool {
        let ctx = &self.tap_ctx;
        disengage_capture(
            reason,
            ctx.capturing.as_ref(),
            ctx.capture_anchor.as_ref(),
            ctx.held_keys.as_ref(),
            ctx.accum_dx.as_ref(),
            ctx.accum_dy.as_ref(),
            &self.socket,
        )
    }

    fn flash_blocked(&mut self) {
        let _ = self
            .tray
            .set_icon_with_as_template(Some(make_icon(220, 38, 38, true)), false);
        let reason = if self.tap.is_none() {
            "Hotswitch: allow Accessibility first"
        } else {
            "Hotswitch: PC not reachable"
        };
        let _ = self.tray.set_tooltip(Some(reason));
        self.flash_until = Some(Instant::now() + Duration::from_secs(2));
    }

    /// Housekeeping while something needs watching: the tap, PS5 audio, the mic prompt.
    fn tick(&mut self) {
        self.next_tick = None;
        if self.tap.is_none() {
            self.tap = create_event_tap(&self.tap_ctx);
            if self.tap.is_some() {
                eprintln!("event tap ready");
                self.refresh_menu();
            }
        }
        if let Some(since) = self.awaiting_mic {
            let decided = !matches!(passthrough::mic_access(), MicAccess::Undetermined);
            if decided || since.elapsed() > Duration::from_secs(60) {
                self.awaiting_mic = None;
                if let Some(input) = self.settings.monitor_input(self.mode) {
                    self.take_monitor(input);
                }
            }
        }
        // Safety nets: a device that dies without a device-list change, and an input that
        // stops delivering while its device still looks alive.
        let dead = self.passthrough.as_ref().is_some_and(|p| !p.is_alive());
        if dead && self.audio_check_at.is_none() {
            self.audio_check_at = Some(Instant::now() + AUDIO_SETTLE);
        }
        if let Some(frames) = self.passthrough.as_ref().map(|p| p.frames_in()) {
            match self.input_watch {
                Some((seen, since)) if seen == frames => {
                    if since.elapsed() > PASSTHROUGH_STALL {
                        eprintln!("passthrough: input stalled, restarting");
                        self.passthrough = None;
                        self.start_passthrough();
                    }
                }
                _ => self.input_watch = Some((frames, Instant::now())),
            }
        }
        self.refresh_status_lines();
        self.schedule_tick();
    }

    fn schedule_tick(&mut self) {
        let every = if self.mode == Mode::Ps5 || self.awaiting_mic.is_some() {
            Some(Duration::from_millis(500))
        } else if self.tap.is_none() {
            Some(Duration::from_secs(2))
        } else {
            None
        };
        self.next_tick = every.map(|d| Instant::now() + d);
    }

    fn monitor_result(&mut self, report: Report) {
        self.monitor_line = match report {
            Report::Holding { name, input } => format!("{name} on {}", ddc::input_name(input)),
            Report::Released { name, input } => {
                format!("{name} back on {}", ddc::input_name(input))
            }
            Report::Refused(e) => e,
            Report::Owed(e) => e,
        };
        self.refresh_status_lines();
    }

    fn hotkey_recorded(&mut self, mode: Mode, result: Recorded) {
        match result {
            Recorded::Set(hotkey) => {
                for h in self.settings.hotkeys.iter_mut() {
                    if *h == Some(hotkey) {
                        *h = None;
                    }
                }
                self.settings.hotkeys[mode.index()] = Some(hotkey);
            }
            Recorded::Clear => self.settings.hotkeys[mode.index()] = None,
            Recorded::Cancel => {}
        }
        *self.tap_ctx.hotkeys.lock().unwrap() = self.settings.hotkeys;
        self.save();
        if let Some(window) = &self.settings_window {
            for m in Mode::ALL {
                window.set_hotkey_title(m, &self.settings, false);
            }
        }
        self.refresh_menu();
    }

    fn open_settings(&mut self) {
        let (mtm, proxy) = (self.mtm, self.proxy.clone());
        let window = self.settings_window.get_or_insert_with(|| {
            SettingsWindow::new(mtm, move |action| {
                let _ = proxy.send_event(UserEvent::Settings(action));
            })
        });
        window.show(&self.settings, self.login_checked, "");
        self.refresh_settings_note();
    }

    fn refresh_settings_note(&self) {
        if let Some(window) = self.settings_window.as_ref().filter(|w| w.is_visible()) {
            window.set_receiver_note(&self.pc_status(), false);
        }
    }

    fn settings_action(&mut self, action: Action) {
        match action {
            Action::Receiver(text) => self.set_receiver(&text),
            Action::Monitor(id) => {
                self.settings.monitor = id;
                self.save();
            }
            Action::Ps5Input(input) => {
                self.settings.ps5_input = input;
                self.save();
            }
            Action::MacInput(input) => {
                self.settings.mac_input = input;
                self.save();
            }
            Action::AudioInput(name) => {
                self.settings.audio_input = name;
                self.save();
                if self.wants_passthrough() {
                    self.passthrough = None;
                    self.start_passthrough();
                }
            }
            Action::RecordHotkey(mode) => {
                let previous = self
                    .tap_ctx
                    .recording
                    .swap(mode.index() as u8 + 1, Ordering::SeqCst);
                if let Some(window) = &self.settings_window {
                    if let Some(&prev) = (previous as usize)
                        .checked_sub(1)
                        .and_then(|i| Mode::ALL.get(i))
                    {
                        window.set_hotkey_title(prev, &self.settings, false);
                    }
                    window.set_hotkey_title(mode, &self.settings, true);
                }
            }
            Action::StartOnLogin(on) => {
                if set_login_item(on) {
                    self.login_checked = on;
                }
            }
            Action::Closed => {
                self.tap_ctx.recording.store(0, Ordering::SeqCst);
                if let Some(text) = self.settings_window.as_ref().map(|w| w.receiver_text()) {
                    if !text.trim().is_empty()
                        && Some(text.trim()) != self.settings.receiver.as_deref()
                    {
                        self.set_receiver(&text);
                    }
                }
            }
        }
    }

    fn set_receiver(&mut self, text: &str) {
        let note = |app: &Self, text: &str, error: bool| {
            if let Some(window) = &app.settings_window {
                window.set_receiver_note(text, error);
            }
        };
        match parse_receiver_address(text) {
            Ok(addr) if Some(&addr) == self.settings.receiver.as_ref() => {
                note(self, &self.pc_status(), false)
            }
            Ok(addr) => {
                if self.mode == Mode::Windows {
                    self.switch_to(Mode::Mac, false);
                }
                // All clones share this socket, so heartbeats and input follow the new address.
                if let Err(e) = self.socket.connect(&addr) {
                    note(self, &format!("Couldn't use {addr}: {e}"), true);
                    return;
                }
                eprintln!("receiver address changed to {addr}");
                // Forget the old receiver: not connected until the new one answers.
                start_new_receiver(&self.receiver_generation, &self.receiver_connected);
                store_state(
                    &self.tap_ctx.capturing,
                    &self.receiver_connected,
                    &self.app_state,
                );
                if self.receiver_hook_active {
                    self.receiver_hook_active = false;
                    run_hook(HookKind::ReceiverDisconnected, false, &[]);
                }
                self.settings.receiver = Some(addr);
                self.save();
                note(self, "Saved. Looking for the PC…", false);
                self.refresh_menu();
            }
            Err(message) => note(self, message, true),
        }
    }

    fn check_for_updates(&mut self) {
        let exe = std::env::current_exe().expect("Failed to get current exe path");
        self.ui.update_item.set_text("Updating...");
        self.ui.update_item.set_enabled(false);
        match apply_update() {
            Ok(UpdateOutcome::UpdatedAndRelaunching) => {
                eprintln!("relaunching: {exe:?}");
                if current_app_bundle_path().is_none() {
                    if let Err(e) = std::process::Command::new(&exe).spawn() {
                        eprintln!("relaunch failed: {e}");
                    }
                }
                self.teardown("update");
                self.quitting = true;
                return;
            }
            Ok(UpdateOutcome::AlreadyUpToDate) => {
                self.ui.update_item.set_text("Already up to date");
            }
            Err(e) => {
                eprintln!("update failed: {e}");
                self.ui.update_item.set_text("Update failed");
            }
        }
        self.reset_update_at = Some(Instant::now() + Duration::from_secs(5));
        self.ui.update_item.set_enabled(true);
    }

    /// Leave the current mode cleanly before quitting or relaunching. Waits for any owed
    /// hand-back, whatever the mode is now. Runs once.
    fn teardown(&mut self, reason: &str) {
        if std::mem::replace(&mut self.torn_down, true) {
            return;
        }
        self.release_capture(reason);
        self.passthrough = None;
        // The hand-back goes first, so no hook can hold it up.
        let (done, wait) = mpsc::channel();
        let _ = self.monitor_jobs.send(Job::Want(Want::Release));
        let _ = self.monitor_jobs.send(Job::Flush(done));
        if wait.recv_timeout(HAND_BACK_WAIT).is_err() {
            eprintln!(
                "monitor: hand-back still running at {reason}; it is owed to the next launch"
            );
        }
        if self.mode != Mode::Mac {
            run_hook(HookKind::ModeChanged, true, &[self.mode.key(), "mac"]);
            self.mode = Mode::Mac;
        }
        if let Some(activity) = self.power_activity.take() {
            activity.stop();
        }
        if self.receiver_hook_active {
            self.receiver_hook_active = false;
            run_hook(HookKind::ReceiverDisconnected, true, &[]);
        }
    }

    fn save(&self) {
        if let Err(e) = self.settings.save(&self.support_dir) {
            eprintln!("settings: failed to save: {e}");
        }
    }

    fn pc_status(&self) -> String {
        match (
            &self.settings.receiver,
            self.receiver_connected.load(Ordering::SeqCst),
        ) {
            (None, _) => "No PC address set".into(),
            (Some(_), true) => "PC connected".into(),
            (Some(addr), false) => format!("PC not reachable at {addr}"),
        }
    }

    fn refresh_menu(&mut self) {
        let connected = self.receiver_connected.load(Ordering::SeqCst);
        for (item, mode) in self.ui.modes.iter().zip(Mode::ALL) {
            item.set_checked(mode == self.mode);
            item.set_enabled(
                mode != Mode::Windows
                    || self.mode == Mode::Windows
                    || (connected && self.tap.is_some()),
            );
            let _ = item
                .set_accelerator(self.settings.hotkeys[mode.index()].and_then(|h| h.accelerator()));
        }

        // PS5 status lines sit between the modes and the PC line, only in PS5 mode.
        let ps5 = self.mode == Mode::Ps5;
        if ps5 != self.ui.ps5_shown {
            let lines: [&dyn IsMenuItem; 3] = [
                &self.ui.monitor_line,
                &self.ui.level_line,
                &self.ui.route_line,
            ];
            if ps5 {
                let _ = self.ui.menu.insert_items(&lines, 4 + menu_top());
            } else {
                for line in lines {
                    let _ = self.ui.menu.remove(line);
                }
            }
            self.ui.ps5_shown = ps5;
        }
        let need_access = self.tap.is_none();
        if need_access != self.ui.access_shown {
            if need_access {
                let at = menu_top() + if ps5 { 8 } else { 5 };
                let _ = self.ui.menu.insert(&self.ui.access_item, at);
            } else {
                let _ = self.ui.menu.remove(&self.ui.access_item);
            }
            self.ui.access_shown = need_access;
        }
        self.ui.pc_line.set_text(self.pc_status());
        self.refresh_status_lines();
        self.refresh_icon();
    }

    fn refresh_status_lines(&self) {
        if !self.ui.ps5_shown {
            return;
        }
        self.ui.monitor_line.set_text(&self.monitor_line);
        let (level, route) = match (&self.passthrough, &self.passthrough_error) {
            (Some(p), _) => {
                let status = p.status();
                let level = match passthrough::mic_access() {
                    MicAccess::Undetermined => "Waiting for microphone access".to_string(),
                    MicAccess::Denied => "Microphone access is off".to_string(),
                    MicAccess::Granted => match status.peak_db {
                        Some(db) => format!("Signal {db:.0} dB"),
                        None => "No signal".to_string(),
                    },
                };
                let route = format!(
                    "{} → {} · {:.0} ms",
                    self.settings.audio_input, status.output, status.loop_ms
                );
                (level, route)
            }
            (None, Some(e)) => (e.clone(), "Waiting for it to come back".to_string()),
            (None, None) => ("Starting audio…".to_string(), String::new()),
        };
        self.ui.level_line.set_text(level);
        self.ui.route_line.set_text(route);
    }

    fn refresh_icon(&mut self) {
        if self.flash_until.is_some() {
            return;
        }
        let connected = self.receiver_connected.load(Ordering::SeqCst);
        let look = (self.mode, connected);
        if self.icon_look == Some(look) {
            return;
        }
        self.icon_look = Some(look);
        let (icon, template) = match self.mode {
            Mode::Mac => (make_icon(0, 0, 0, connected), true),
            Mode::Windows => (make_icon(234, 179, 8, true), false),
            Mode::Ps5 => (make_icon(0, 112, 209, true), false),
        };
        let _ = self.tray.set_icon_with_as_template(Some(icon), template);
        let _ = self
            .tray
            .set_tooltip(Some(format!("Hotswitch: {}", self.mode.label())));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_heartbeat_from_the_old_receiver_cant_reconnect() {
        let generation = Mutex::new(0);
        let connected = AtomicBool::new(true);
        // The heartbeat thread received a packet for generation 0, then the address changed.
        start_new_receiver(&generation, &connected);
        let published =
            publish_if_current(&generation, 0, || connected.store(true, Ordering::SeqCst));
        assert!(!published);
        assert!(!connected.load(Ordering::SeqCst));
        // A heartbeat for the new address does publish.
        assert!(publish_if_current(&generation, 1, || connected.store(true, Ordering::SeqCst)));
        assert!(connected.load(Ordering::SeqCst));
    }
}
