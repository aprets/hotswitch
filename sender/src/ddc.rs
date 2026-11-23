//! DDC/CI monitor control on Apple Silicon, through the private IOAVService calls
//! that m1ddc and MonitorControl use. Only external displays answer.
//!
//! Safety rules, because monitors behind a dock can drop off when pushed:
//! - discovery reads IORegistry properties only, with no EDID or I2C traffic;
//! - a monitor that could be showing the Mac's main display is never touched, and when the
//!   main display can't be identified, no monitor is;
//! - a monitor is targeted only by a serial number that no other monitor shares;
//! - before every single I2C transaction the display layout is read again, and the operation
//!   stops if anything changed since the target was opened;
//! - one DDC operation at a time across processes (a lock file); only writes, each sent once.
//!   Nothing reads the monitor: one showing another input can't be trusted to answer.
//!
//! In a dry run (`HOTSWITCH_DRY_RUN=1`) every rule above still applies, but no DDC service
//! is ever opened and writes are only logged.

use crate::settings::{dry_run, support_dir};
use core_foundation::base::{CFRelease, CFType, CFTypeRef, TCFType};
use core_foundation::dictionary::CFDictionary;
use core_foundation::number::CFNumber;
use core_foundation::string::{CFString, CFStringRef};
use core_graphics::display::CGDisplay;
use serde::{Deserialize, Serialize};
use std::ffi::{c_char, c_void, CStr};
use std::fs::File;
use std::ptr;

/// VCP code for the input source (MCCS).
pub const VCP_INPUT: u8 = 0x60;

/// Common VCP 0x60 values. Monitors may use others; the settings window lists these.
pub const INPUTS: [(u16, &str); 5] = [
    (0x11, "HDMI 1"),
    (0x12, "HDMI 2"),
    (0x0F, "DisplayPort 1"),
    (0x10, "DisplayPort 2"),
    (0x1B, "USB-C"),
];

pub fn input_name(value: u16) -> String {
    INPUTS
        .iter()
        .find(|(v, _)| *v == value)
        .map(|(_, name)| name.to_string())
        .unwrap_or_else(|| format!("input {value}"))
}

type IoObject = u32;

#[link(name = "IOKit", kind = "framework")]
extern "C" {
    fn IOServiceMatching(name: *const c_char) -> *mut c_void;
    fn IOServiceGetMatchingServices(port: u32, matching: *mut c_void, iter: *mut IoObject) -> i32;
    fn IOIteratorNext(iter: IoObject) -> IoObject;
    fn IOObjectRelease(obj: IoObject) -> i32;
    fn IORegistryEntryGetParentEntry(
        entry: IoObject,
        plane: *const c_char,
        parent: *mut IoObject,
    ) -> i32;
    fn IORegistryEntryGetName(entry: IoObject, name: *mut c_char) -> i32;
    fn IORegistryEntryCreateCFProperty(
        entry: IoObject,
        key: CFStringRef,
        allocator: *const c_void,
        options: u32,
    ) -> CFTypeRef;
    fn IOAVServiceCreateWithService(allocator: *const c_void, service: IoObject) -> CFTypeRef;
    fn IOAVServiceWriteI2C(
        service: CFTypeRef,
        chip: u32,
        addr: u32,
        buf: *const u8,
        len: u32,
    ) -> i32;
}

/// The display's DDC/CI address (7-bit form of 0x6E).
const CHIP: u32 = 0x37;
/// Destination address as written on the wire.
const DEST: u8 = 0x6E;
/// The host's source address; it is sent and is part of the checksum.
const SOURCE: u8 = 0x51;

/// A monitor as the IORegistry describes it. Persisted in the hand-back record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    pub name: String,
    /// The alphanumeric serial, or the numeric one as text. Empty when the monitor has none.
    pub serial: String,
    pub serial_number: i64,
    pub vendor: i64,
    pub product: i64,
}

/// The main display as CoreGraphics sees it: vendor, model, serial.
pub type MainDisplay = (i64, i64, i64);

impl Identity {
    /// Same model as the main display, and the serials don't tell them apart.
    fn could_be(&self, (vendor, product, serial): MainDisplay) -> bool {
        self.vendor == vendor
            && self.product == product
            && (self.serial_number == serial || self.serial_number == 0 || serial == 0)
    }

    pub fn label(&self) -> String {
        format!("{} · {}", self.name, self.serial)
    }
}

/// Whether DDC may touch this monitor at all, given the main display.
pub fn usable(main: Option<MainDisplay>, id: &Identity) -> Result<(), String> {
    let Some(main) = main else {
        return Err("the main display can't be identified, so no monitor is touched".into());
    };
    if id.serial.is_empty() {
        return Err(format!(
            "{} has no serial number, so it can't be told apart",
            id.name
        ));
    }
    if id.could_be(main) {
        return Err(format!("{} could be the main display", id.name));
    }
    Ok(())
}

/// Monitors that can be targeted: usable, and the only one with their serial.
pub fn targetable(main: Option<MainDisplay>, found: &[Identity]) -> Vec<Identity> {
    found
        .iter()
        .filter(|id| usable(main, id).is_ok())
        .filter(|id| found.iter().filter(|o| o.serial == id.serial).count() == 1)
        .cloned()
        .collect()
}

/// The monitor saved for PS5 mode, if it can be switched right now. Never picks one on its
/// own: with no serial saved it refuses, because the main display can move to any monitor.
pub fn resolve(
    main: Option<MainDisplay>,
    found: &[Identity],
    wanted: Option<&str>,
) -> Result<Identity, String> {
    let Some(serial) = wanted else {
        return Err("Choose the PS5 monitor in Settings".into());
    };
    let mut matches = found.iter().filter(|id| id.serial == serial);
    let id = matches
        .next()
        .ok_or_else(|| format!("Monitor {serial} isn't connected"))?;
    if matches.next().is_some() {
        return Err(format!("Another monitor shares serial {serial}"));
    }
    usable(main, id)?;
    Ok(id.clone())
}

/// What decides whether a running DDC operation may continue. Compared before every transaction.
#[derive(Clone, Debug, PartialEq)]
pub struct Layout {
    main: Option<MainDisplay>,
    main_id: u32,
    active: Vec<u32>,
    /// (framebuffer port like "dispext0", identity), sorted.
    framebuffers: Vec<(String, Identity)>,
}

impl Layout {
    pub fn read() -> Layout {
        let main = CGDisplay::main();
        let (vendor, product, serial) = (
            main.vendor_number() as i64,
            main.model_number() as i64,
            main.serial_number() as i64,
        );
        let known = main.id != 0 && !(vendor == 0 && product == 0);
        let mut active = CGDisplay::active_displays().unwrap_or_default();
        active.sort_unstable();
        let mut framebuffers = Vec::new();
        unsafe {
            for_each_service(c"IOMobileFramebufferShim", |entry| {
                if let (Some(port), Some(id)) = (parent_name(entry), identity(entry)) {
                    framebuffers.push((port, id));
                }
            });
        }
        framebuffers.sort_by(|a, b| a.0.cmp(&b.0));
        Layout {
            main: known.then_some((vendor, product, serial)),
            main_id: main.id,
            active,
            framebuffers,
        }
    }

    pub fn main(&self) -> Option<MainDisplay> {
        self.main
    }

    /// Monitors with a DDC service (registry only).
    pub fn found(&self) -> Vec<Identity> {
        let mut out = Vec::new();
        unsafe {
            for_each_service(c"DCPAVServiceProxy", |entry| {
                if let Some(id) = self.proxy_identity(entry) {
                    out.push(id.clone());
                }
            });
        }
        out
    }

    pub fn targetable(&self) -> Vec<Identity> {
        targetable(self.main, &self.found())
    }

    unsafe fn proxy_identity(&self, entry: IoObject) -> Option<&Identity> {
        if registry_string(entry, "Location").as_deref() != Some("External") {
            return None;
        }
        // The proxy's parent is "dispextN:dcpav-service-epic:0"; its framebuffer's is "dispextN".
        let port = parent_name(entry)?.split(':').next()?.to_owned();
        self.framebuffers
            .iter()
            .find(|(p, _)| *p == port)
            .map(|(_, id)| id)
    }
}

/// An open DDC target. Holds the cross-process DDC lock while it lives.
pub struct Target {
    pub identity: Identity,
    layout: Layout,
    service: CFTypeRef,
    _lock: File,
}

// The IOAVService handle is a plain CF object; it is used from one thread at a time.
unsafe impl Send for Target {}

impl Drop for Target {
    fn drop(&mut self) {
        if !self.service.is_null() {
            unsafe { CFRelease(self.service) }
        }
    }
}

impl Target {
    /// Open `identity` for DDC if it is usable now and the only monitor with its serial.
    pub fn open(identity: &Identity) -> Result<Target, String> {
        let lock = lock()?;
        let layout = Layout::read();
        if !layout.targetable().contains(identity) {
            usable(layout.main, identity)?;
            return Err(format!(
                "{} isn't connected, or another monitor shares its serial",
                identity.name
            ));
        }
        let mut service: CFTypeRef = ptr::null();
        if !dry_run() {
            unsafe {
                for_each_service(c"DCPAVServiceProxy", |entry| {
                    if service.is_null() && layout.proxy_identity(entry) == Some(identity) {
                        service = IOAVServiceCreateWithService(ptr::null(), entry);
                    }
                });
            }
            if service.is_null() {
                return Err(format!("{} has no DDC service", identity.name));
            }
        }
        Ok(Target {
            identity: identity.clone(),
            layout,
            service,
            _lock: lock,
        })
    }

    /// Stop when anything about the displays changed since the target was opened.
    fn guard(&self) -> Result<(), String> {
        if Layout::read() == self.layout {
            Ok(())
        } else {
            Err("The displays changed, so DDC stopped".into())
        }
    }

    /// One write, no retries.
    pub fn write(&self, vcp: u8, value: u16) -> Result<(), String> {
        self.guard()?;
        if dry_run() {
            eprintln!(
                "ddc dry run: {} vcp {vcp:#04x} = {value}",
                self.identity.label()
            );
            return Ok(());
        }
        if unsafe { self.send(&set_request(vcp, value)) } == 0 {
            Ok(())
        } else {
            Err(format!("DDC write to {} failed", self.identity.name))
        }
    }

    unsafe fn send(&self, packet: &[u8]) -> i32 {
        if self.service.is_null() {
            return -1;
        }
        IOAVServiceWriteI2C(
            self.service,
            CHIP,
            SOURCE as u32,
            packet.as_ptr(),
            packet.len() as u32,
        )
    }
}

fn checksum(seed: u8, bytes: &[u8]) -> u8 {
    bytes.iter().fold(seed, |c, b| c ^ b)
}

/// Set VCP Feature request.
fn set_request(vcp: u8, value: u16) -> [u8; 6] {
    let mut p = [0x84, 0x03, vcp, (value >> 8) as u8, value as u8, 0];
    p[5] = checksum(DEST ^ SOURCE, &p[..5]);
    p
}

/// One DDC operation at a time, across the app, the CLI and hooks.
fn lock() -> Result<File, String> {
    let dir = support_dir();
    let _ = std::fs::create_dir_all(&dir);
    let file = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("ddc.lock"))
        .map_err(|e| format!("DDC lock: {e}"))?;
    file.lock().map_err(|e| format!("DDC lock: {e}"))?;
    Ok(file)
}

unsafe fn identity(entry: IoObject) -> Option<Identity> {
    let key = CFString::new("DisplayAttributes");
    let value = IORegistryEntryCreateCFProperty(entry, key.as_concrete_TypeRef(), ptr::null(), 0);
    if value.is_null() {
        return None;
    }
    let attributes = CFType::wrap_under_create_rule(value).downcast::<CFDictionary>()?;
    let product = dict_get(&attributes, "ProductAttributes")?.downcast::<CFDictionary>()?;
    let text = |key| {
        dict_get(&product, key)
            .and_then(|v| v.downcast::<CFString>())
            .map(|s| s.to_string())
    };
    let number = |key| {
        dict_get(&product, key)
            .and_then(|v| v.downcast::<CFNumber>())
            .and_then(|n| n.to_i64())
    };
    let serial_number = number("SerialNumber").unwrap_or(0);
    let serial = text("AlphanumericSerialNumber")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| match serial_number {
            0 => String::new(),
            n => n.to_string(),
        });
    Some(Identity {
        name: text("ProductName").unwrap_or_else(|| "External display".into()),
        serial,
        serial_number,
        vendor: number("LegacyManufacturerID")?,
        product: number("ProductID")?,
    })
}

fn dict_get(dict: &CFDictionary, key: &str) -> Option<CFType> {
    let key = CFString::new(key);
    dict.find(key.as_CFTypeRef() as *const c_void)
        .map(|v| unsafe { CFType::wrap_under_get_rule(*v as CFTypeRef) })
}

unsafe fn for_each_service(class: &CStr, mut f: impl FnMut(IoObject)) {
    let mut iter: IoObject = 0;
    if IOServiceGetMatchingServices(0, IOServiceMatching(class.as_ptr()), &mut iter) != 0 {
        return;
    }
    loop {
        let entry = IOIteratorNext(iter);
        if entry == 0 {
            break;
        }
        f(entry);
        IOObjectRelease(entry);
    }
    IOObjectRelease(iter);
}

unsafe fn parent_name(entry: IoObject) -> Option<String> {
    let mut parent: IoObject = 0;
    if IORegistryEntryGetParentEntry(entry, c"IOService".as_ptr(), &mut parent) != 0 {
        return None;
    }
    let mut name = [0 as c_char; 128];
    let ok = IORegistryEntryGetName(parent, name.as_mut_ptr()) == 0;
    IOObjectRelease(parent);
    ok.then(|| CStr::from_ptr(name.as_ptr()).to_string_lossy().into_owned())
}

unsafe fn registry_string(entry: IoObject, key: &str) -> Option<String> {
    let key = CFString::new(key);
    let value = IORegistryEntryCreateCFProperty(entry, key.as_concrete_TypeRef(), ptr::null(), 0);
    if value.is_null() {
        return None;
    }
    CFType::wrap_under_create_rule(value)
        .downcast::<CFString>()
        .map(|s| s.to_string())
}

/// `hotswitch-sender ddc [set <vcp> <value> <serial>]`, for debugging
/// and hooks. The plain listing sends nothing to any monitor and says why a monitor can't be
/// used. The same safety rules apply here as in the app.
pub fn cli(args: &[String]) {
    let parse = |s: &str| match s.strip_prefix("0x") {
        Some(hex) => u16::from_str_radix(hex, 16).ok(),
        None => s.parse().ok(),
    };
    let open = |serial: Option<&String>| -> Result<Target, String> {
        let layout = Layout::read();
        Target::open(&resolve(
            layout.main(),
            &layout.found(),
            serial.map(String::as_str),
        )?)
    };
    let run = || -> Result<(), String> {
        match args {
            [] => {
                let layout = Layout::read();
                let found = layout.found();
                let targetable = targetable(layout.main(), &found);
                for id in &found {
                    let note = match usable(layout.main(), id) {
                        Err(e) => format!("not used: {e}"),
                        Ok(()) if !targetable.contains(id) => "not used: serial not unique".into(),
                        Ok(()) => "usable".into(),
                    };
                    println!("{}\tserial {}\t{note}", id.name, id.serial);
                }
                Ok(())
            }
            [cmd, vcp, value, serial] if cmd == "set" => {
                let (vcp, value) = parse(vcp).zip(parse(value)).ok_or("bad arguments")?;
                open(Some(serial))?.write(vcp as u8, value)
            }
            _ => Err("usage: hotswitch-sender ddc [set <vcp> <value> <serial>]".into()),
        }
    };
    if let Err(e) = run() {
        eprintln!("{e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn monitor(serial: &str, serial_number: i64) -> Identity {
        Identity {
            name: "Example 27".into(),
            serial: serial.into(),
            serial_number,
            vendor: 4242,
            product: 1001,
        }
    }

    #[test]
    fn set_request_checksum() {
        assert_eq!(
            set_request(0x60, 0x0F),
            [0x84, 0x03, 0x60, 0x00, 0x0F, 0xD7]
        );
        assert_eq!(
            set_request(0x60, 0x11),
            [0x84, 0x03, 0x60, 0x00, 0x11, 0xC9]
        );
    }

    #[test]
    fn main_display_guard() {
        let m = monitor("TEST0001", 12345678);
        assert!(
            usable(None, &m).is_err(),
            "unknown main display touches nothing"
        );
        assert!(
            usable(Some((4242, 1001, 12345678)), &m).is_err(),
            "is the main display"
        );
        assert!(
            usable(Some((4242, 1001, 0)), &m).is_err(),
            "main has no serial"
        );
        assert!(
            usable(Some((4242, 1001, 87654321)), &m).is_ok(),
            "a twin with another serial"
        );
        assert!(
            usable(Some((4242, 1002, 12345678)), &m).is_ok(),
            "another model"
        );
        let no_serial = monitor("", 0);
        assert!(
            usable(Some((1, 2, 3)), &no_serial).is_err(),
            "no serial at all"
        );
        assert!(
            usable(Some((4242, 1001, 87654321)), &monitor("X", 0)).is_err(),
            "twin, numeric serial missing"
        );
    }

    #[test]
    fn duplicate_serials_are_never_targets() {
        let main = Some((1, 2, 3));
        let found = [monitor("SAME", 1), monitor("SAME", 2), monitor("OTHER", 3)];
        assert_eq!(targetable(main, &found), vec![monitor("OTHER", 3)]);
        assert!(resolve(main, &found, Some("SAME")).is_err());
        assert_eq!(
            resolve(main, &found, Some("OTHER")),
            Ok(monitor("OTHER", 3))
        );
        assert!(resolve(main, &found, Some("MISSING")).is_err());
    }

    #[test]
    fn ps5_mode_never_picks_a_monitor_on_its_own() {
        // 23 Nov: macOS moved the main display to the PS5 monitor, leaving its twin as the only
        // usable monitor. With no serial saved, that twin (the wrong screen) must not be picked.
        let ps5 = monitor("PS5MON", 111);
        let other = monitor("OTHERMON", 222);
        let found = [ps5.clone(), other.clone()];
        let ps5_is_main = Some((4242, 1001, 111));
        assert_eq!(targetable(ps5_is_main, &found), vec![other.clone()]);
        assert_eq!(
            resolve(ps5_is_main, &found, None),
            Err("Choose the PS5 monitor in Settings".into())
        );
        // Saved, but showing the menu bar right now: refused, with the reason.
        let refused = resolve(ps5_is_main, &found, Some("PS5MON")).unwrap_err();
        assert!(refused.contains("main display"), "{refused}");
        // Saved, and the other monitor is the main display: fine.
        let other_is_main = Some((4242, 1001, 222));
        assert_eq!(resolve(other_is_main, &found, Some("PS5MON")), Ok(ps5));
    }
}
