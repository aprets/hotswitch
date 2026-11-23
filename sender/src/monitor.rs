//! Which monitor input a mode holds, and handing it back.
//!
//! One worker thread does all of the app's DDC. The app only says what it wants: hold an input
//! on a monitor, or release whatever is held. The worker keeps one claim (the monitor, the input
//! it set, the input to put back, whether it detached the display) in `monitor-claim.json`. The
//! claim is written before the monitor is switched and removed only after the hand-back write
//! succeeds, so a crash, quit, sleep or unplugged dock never loses it. Settings edits don't
//! reach a claim: it keeps what it was made with.
//!
//! Nothing reads the monitor. One showing another input (the PS5's HDMI) can't be trusted to
//! report it: a reply that didn't match once made hand-back skip its write. Entering writes the
//! mode's input; leaving writes the Mac's input back, as the receiver's BetterDisplay hooks do.

use crate::ddc::{self, Identity, Layout, Target, VCP_INPUT};
use crate::settings::{dry_run, write_atomically};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

/// Least time between two DDC operations. Monitors behind a dock can drop off when pushed.
const SPACING: Duration = Duration::from_secs(1);
/// After re-attaching a display, or at launch with a hand-back owed, let the displays settle.
const SETTLE: Duration = Duration::from_secs(2);
const BETTERDISPLAY: &str = "/Applications/BetterDisplay.app/Contents/MacOS/BetterDisplay";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claim {
    pub monitor: Identity,
    /// The input this claim put on the monitor.
    pub input: u16,
    /// The input hand-back writes: the Mac's, fixed when the claim was made.
    pub restore: u16,
    /// The display was detached from macOS and is re-attached before hand-back.
    #[serde(default)]
    pub detached: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Want {
    Release,
    /// Hold `input` on the monitor with this serial (none saved: refused) and write `restore`
    /// on release. `detach` also detaches the display from macOS. All fixed when sent.
    Hold {
        monitor: Option<String>,
        input: u16,
        restore: u16,
        detach: bool,
    },
}

pub enum Job {
    Want(Want),
    /// Displays came back or the Mac woke: retry an owed hand-back. Never retries a take.
    Reconcile,
    /// Answered once everything before it is done.
    Flush(Sender<()>),
}

#[derive(Debug)]
pub enum Report {
    Holding {
        name: String,
        input: u16,
    },
    Released {
        name: String,
        input: u16,
    },
    /// The take didn't happen.
    Refused(String),
    /// A hand-back is still owed; it is retried on wake, display changes and the next launch.
    Owed(String),
}

/// The one DDC operation a take or hand-back needs. `Target` is the real one; tests use a fake.
pub trait InputPort {
    fn write_input(&mut self, value: u16) -> Result<(), String>;
}

impl InputPort for Target {
    fn write_input(&mut self, value: u16) -> Result<(), String> {
        self.write(VCP_INPUT, value)
    }
}

type SaveClaim<'a> = dyn FnMut(Option<&Claim>) -> std::io::Result<()> + 'a;

/// Stop if the request was withdrawn. Save the claim; if that fails, stop. Only then switch.
/// Returns the report (`None` when withdrawn) and the claim now held.
pub fn take_steps(
    port: &mut dyn InputPort,
    save: &mut SaveClaim,
    still_wanted: &mut dyn FnMut() -> bool,
    claim: Claim,
    existing: Option<Claim>,
) -> (Option<Report>, Option<Claim>) {
    if !still_wanted() {
        return (None, existing);
    }
    let (name, input) = (claim.monitor.name.clone(), claim.input);
    if let Err(e) = save(Some(&claim)) {
        let reason = format!("Couldn't save the hand-back record ({e}), so {name} wasn't switched");
        return (Some(Report::Refused(reason)), existing);
    }
    eprintln!("monitor: saved claim {claim:?}, switching");
    match port.write_input(input) {
        // The claim stays: a hand-back write is harmless if this one never landed.
        Err(e) => (Some(Report::Refused(e)), Some(claim)),
        Ok(()) => (Some(Report::Holding { name, input }), Some(claim)),
    }
}

/// Write the Mac's input back. Only a successful write clears the claim.
pub fn hand_back_steps(
    port: &mut dyn InputPort,
    save: &mut SaveClaim,
    claim: Claim,
) -> (Report, Option<Claim>) {
    let name = claim.monitor.name.clone();
    if let Err(e) = port.write_input(claim.restore) {
        let reason = format!("{e}. {name} will be handed back later");
        return (Report::Owed(reason), Some(claim));
    }
    if let Err(e) = save(None) {
        eprintln!("monitor: couldn't remove the claim file: {e}");
    }
    let input = claim.restore;
    (Report::Released { name, input }, None)
}

/// Whether a held claim matches `Hold { monitor, input }`.
pub fn claim_satisfies(claim: &Claim, monitor: Option<&str>, input: u16) -> bool {
    claim.input == input && monitor.is_none_or(|m| m == claim.monitor.serial)
}

/// How the worker finds, switches and detaches monitors: the hardware, or a fake in tests.
pub trait Monitors: Send {
    fn resolve(&mut self, wanted: Option<&str>) -> Result<Identity, String>;
    fn open(&mut self, identity: &Identity) -> Result<Box<dyn InputPort>, String>;
    /// Attach or detach the display in macOS, as the receiver hooks do with BetterDisplay.
    fn set_attached(&mut self, identity: &Identity, attached: bool) -> Result<(), String>;
    fn settle(&mut self);
}

struct Hardware;

impl Monitors for Hardware {
    fn resolve(&mut self, wanted: Option<&str>) -> Result<Identity, String> {
        // Registry only: no DDC until the target is settled.
        let layout = Layout::read();
        ddc::resolve(&layout.found(), wanted)
    }

    fn open(&mut self, identity: &Identity) -> Result<Box<dyn InputPort>, String> {
        Ok(Box::new(Target::open(identity)?))
    }

    fn set_attached(&mut self, identity: &Identity, attached: bool) -> Result<(), String> {
        let value = if attached { "on" } else { "off" };
        if dry_run() {
            eprintln!("monitor dry run: {} connected={value}", identity.label());
            return Ok(());
        }
        let output = Command::new(BETTERDISPLAY)
            .arg("set")
            .arg(format!("-alphanumericSerial={}", identity.serial))
            .arg(format!("-connected={value}"))
            .output()
            .map_err(|e| format!("BetterDisplay: {e}"))?;
        if output.status.success() {
            eprintln!("monitor: {} connected={value}", identity.label());
            Ok(())
        } else {
            Err(format!(
                "BetterDisplay couldn't set {} connected={value}",
                identity.name
            ))
        }
    }

    fn settle(&mut self) {
        thread::sleep(SETTLE);
    }
}

pub fn claim_path(dir: &Path) -> PathBuf {
    dir.join("monitor-claim.json")
}

pub fn load_claim(dir: &Path) -> Option<Claim> {
    let raw = std::fs::read(claim_path(dir)).ok()?;
    match serde_json::from_slice(&raw) {
        Ok(claim) => Some(claim),
        Err(e) => {
            eprintln!("monitor: ignoring unreadable claim: {e}");
            None
        }
    }
}

fn save_claim(dir: &Path, claim: Option<&Claim>) -> std::io::Result<()> {
    match claim {
        Some(c) => {
            let json = serde_json::to_vec_pretty(c).map_err(std::io::Error::other)?;
            write_atomically(&claim_path(dir), &json)
        }
        None => match std::fs::remove_file(claim_path(dir)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        },
    }
}

pub fn spawn(dir: PathBuf, report: impl Fn(Report) + Send + 'static) -> Sender<Job> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || Worker::new(dir, Box::new(Hardware), rx, Box::new(report)).run());
    tx
}

struct Worker {
    dir: PathBuf,
    monitors: Box<dyn Monitors>,
    rx: Receiver<Job>,
    report: Box<dyn Fn(Report) + Send>,
    claim: Option<Claim>,
    want: Want,
    /// The request last carried out. A request equal to it isn't repeated.
    done: Option<Want>,
    /// A wake or display change asked to retry an owed hand-back.
    event: bool,
    flushes: Vec<Sender<()>>,
    last_ddc: Option<Instant>,
}

impl Worker {
    fn new(
        dir: PathBuf,
        monitors: Box<dyn Monitors>,
        rx: Receiver<Job>,
        report: Box<dyn Fn(Report) + Send>,
    ) -> Self {
        let claim = load_claim(&dir);
        if let Some(c) = &claim {
            eprintln!("monitor: a hand-back is owed from before: {c:?}");
        }
        Worker {
            dir,
            monitors,
            rx,
            report,
            // A claim left by a crash is handed back first; the app starts in Mac mode.
            event: claim.is_some(),
            claim,
            want: Want::Release,
            done: Some(Want::Release),
            flushes: Vec::new(),
            last_ddc: None,
        }
    }

    fn run(mut self) {
        if self.event {
            // Launched with a hand-back owed: let the displays settle first.
            self.monitors.settle();
        }
        loop {
            if !self.needs_work() {
                for done in self.flushes.drain(..) {
                    let _ = done.send(());
                }
                match self.rx.recv() {
                    Ok(job) => self.take_job(job),
                    Err(_) => return,
                }
                self.drain();
                if !self.needs_work() {
                    continue;
                }
            }
            // Wait out the spacing first, then act on whatever is wanted by the end of it.
            self.cool_down();
            self.drain();
            if self.needs_work() {
                let wanted = self.want.clone();
                self.event = false;
                if self.reconcile(&wanted) {
                    self.done = Some(wanted);
                }
            }
        }
    }

    fn needs_work(&self) -> bool {
        self.done.as_ref() != Some(&self.want)
            || (self.event && self.want == Want::Release && self.claim.is_some())
    }

    fn take_job(&mut self, job: Job) {
        match job {
            Job::Want(want) => self.want = want,
            Job::Reconcile => self.event = true,
            Job::Flush(done) => self.flushes.push(done),
        }
    }

    fn drain(&mut self) {
        while let Ok(job) = self.rx.try_recv() {
            self.take_job(job);
        }
    }

    /// True once the request being carried out has been replaced by a newer one.
    fn superseded(&mut self, wanted: &Want) -> bool {
        self.drain();
        self.want != *wanted
    }

    fn cool_down(&self) {
        if let Some(last) = self.last_ddc {
            thread::sleep(SPACING.saturating_sub(last.elapsed()));
        }
    }

    /// Carry out `wanted`. False when a newer request replaced it before it finished.
    fn reconcile(&mut self, wanted: &Want) -> bool {
        let Want::Hold {
            monitor,
            input,
            restore,
            detach,
        } = wanted.clone()
        else {
            return match self.claim.clone() {
                None => true,
                Some(claim) => self.hand_back(claim, wanted),
            };
        };
        let existing = match self.claim.clone() {
            Some(c) if claim_satisfies(&c, monitor.as_deref(), input) => Some(c),
            Some(c) => {
                // An older claim on another monitor or input goes back first.
                if !self.hand_back(c, wanted) {
                    return false;
                }
                if self.claim.is_some() {
                    return true;
                }
                None
            }
            None => None,
        };
        let identity = match &existing {
            Some(c) => Ok(c.monitor.clone()),
            None => self.monitors.resolve(monitor.as_deref()),
        };
        let monitor = match identity {
            Ok(id) => id,
            Err(e) => {
                (self.report)(Report::Refused(e));
                return true;
            }
        };
        // An owed claim keeps the input it promised to put back, and its detached state.
        let claim = Claim {
            monitor,
            input,
            restore: existing.as_ref().map_or(restore, |c| c.restore),
            detached: existing.as_ref().is_some_and(|c| c.detached),
        };
        self.take(claim, existing, detach, wanted)
    }

    fn take(&mut self, claim: Claim, existing: Option<Claim>, detach: bool, wanted: &Want) -> bool {
        let mut port = match self.open(&claim.monitor) {
            Ok(port) => port,
            Err(e) => {
                (self.report)(Report::Refused(e));
                return true;
            }
        };
        let dir = self.dir.clone();
        // Waiting for the lock can take a while; the user may have moved on.
        let (report, held) = take_steps(
            port.as_mut(),
            &mut |c| save_claim(&dir, c),
            &mut || !self.superseded(wanted),
            claim,
            existing,
        );
        drop(port); // releases the DDC lock before BetterDisplay talks to the monitor
        self.last_ddc = Some(Instant::now());
        self.claim = held;
        let Some(report) = report else {
            return false;
        };
        if detach && matches!(report, Report::Holding { .. }) {
            self.detach();
        }
        (self.report)(report);
        true
    }

    /// Detach the display from macOS once the switch has landed. Recorded first, so a crash
    /// still re-attaches it; taken out of the record again if BetterDisplay refuses.
    fn detach(&mut self) {
        let Some(mut claim) = self.claim.clone().filter(|c| !c.detached) else {
            return;
        };
        claim.detached = true;
        if let Err(e) = save_claim(&self.dir, Some(&claim)) {
            eprintln!("monitor: not detaching, the record couldn't be saved: {e}");
            return;
        }
        self.claim = Some(claim.clone());
        if let Err(e) = self.monitors.set_attached(&claim.monitor, false) {
            eprintln!("monitor: {e}");
            claim.detached = false;
            let _ = save_claim(&self.dir, Some(&claim));
            self.claim = Some(claim);
        }
    }

    /// Re-attach first, so the Mac has the display before its input comes back. Reports
    /// `Owed` and keeps the claim whenever a step fails.
    fn hand_back(&mut self, mut claim: Claim, wanted: &Want) -> bool {
        let name = claim.monitor.name.clone();
        if claim.detached {
            if let Err(e) = self.monitors.set_attached(&claim.monitor, true) {
                (self.report)(Report::Owed(format!(
                    "{e}. {name} will be handed back later"
                )));
                return true;
            }
            claim.detached = false;
            if let Err(e) = save_claim(&self.dir, Some(&claim)) {
                eprintln!("monitor: couldn't record the re-attach: {e}");
            }
            self.claim = Some(claim.clone());
            self.monitors.settle();
        }
        let mut port = match self.open(&claim.monitor) {
            Ok(port) => port,
            Err(e) => {
                (self.report)(Report::Owed(format!(
                    "{e}. {name} will be handed back later"
                )));
                return true;
            }
        };
        if self.superseded(wanted) {
            return false;
        }
        let dir = self.dir.clone();
        let (report, kept) = hand_back_steps(port.as_mut(), &mut |c| save_claim(&dir, c), claim);
        self.last_ddc = Some(Instant::now());
        self.claim = kept;
        (self.report)(report);
        true
    }

    /// Wait out the spacing, then open the target (which also takes the cross-process lock).
    fn open(&mut self, identity: &Identity) -> Result<Box<dyn InputPort>, String> {
        self.cool_down();
        self.monitors.open(identity)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::{Arc, Mutex};

    fn monitor() -> Identity {
        Identity {
            name: "Example 27".into(),
            serial: "TEST0001".into(),
            serial_number: 12345678,
            vendor: 4242,
            product: 1001,
        }
    }

    fn claim() -> Claim {
        Claim {
            monitor: monitor(),
            input: 17,
            restore: 15,
            detached: false,
        }
    }

    /// A monitor that logs every write into a log it shares with the fake claim store.
    struct Fake {
        log: Rc<RefCell<Vec<String>>>,
        write_ok: bool,
    }

    impl InputPort for Fake {
        fn write_input(&mut self, value: u16) -> Result<(), String> {
            self.log.borrow_mut().push(format!("write {value}"));
            self.write_ok.then_some(()).ok_or("write failed".into())
        }
    }

    fn fake(write_ok: bool) -> (Fake, Rc<RefCell<Vec<String>>>) {
        let log = Rc::new(RefCell::new(Vec::new()));
        let port = Fake {
            log: log.clone(),
            write_ok,
        };
        (port, log)
    }

    fn store(
        log: &Rc<RefCell<Vec<String>>>,
        ok: bool,
    ) -> impl FnMut(Option<&Claim>) -> std::io::Result<()> {
        let log = log.clone();
        move |c| {
            log.borrow_mut().push(match c {
                Some(c) => format!("save {}->{}", c.restore, c.input),
                None => "clear".into(),
            });
            ok.then_some(())
                .ok_or_else(|| std::io::Error::other("disk full"))
        }
    }

    #[test]
    fn take_saves_the_claim_before_switching() {
        let (mut port, log) = fake(true);
        let (report, held) = take_steps(
            &mut port,
            &mut store(&log, true),
            &mut || true,
            claim(),
            None,
        );
        assert!(matches!(report, Some(Report::Holding { input: 17, .. })));
        assert_eq!(held, Some(claim()));
        assert_eq!(*log.borrow(), ["save 15->17", "write 17"]);
    }

    #[test]
    fn take_never_writes_when_the_claim_cant_be_saved() {
        let (mut port, log) = fake(true);
        let (report, held) = take_steps(
            &mut port,
            &mut store(&log, false),
            &mut || true,
            claim(),
            None,
        );
        assert!(matches!(report, Some(Report::Refused(_))));
        assert_eq!(held, None);
        assert_eq!(*log.borrow(), ["save 15->17"]);
    }

    #[test]
    fn take_stops_when_withdrawn() {
        let (mut port, log) = fake(true);
        let (report, held) = take_steps(
            &mut port,
            &mut store(&log, true),
            &mut || false,
            claim(),
            None,
        );
        assert!(report.is_none());
        assert_eq!(held, None);
        assert!(log.borrow().is_empty());
    }

    #[test]
    fn take_keeps_the_claim_when_the_write_fails() {
        let (mut port, log) = fake(false);
        let (report, held) = take_steps(
            &mut port,
            &mut store(&log, true),
            &mut || true,
            claim(),
            None,
        );
        assert!(matches!(report, Some(Report::Refused(_))));
        assert_eq!(held, Some(claim()));
        assert_eq!(*log.borrow(), ["save 15->17", "write 17"]);
    }

    #[test]
    fn hand_back_writes_the_mac_input_then_clears() {
        let (mut port, log) = fake(true);
        let (report, kept) = hand_back_steps(&mut port, &mut store(&log, true), claim());
        assert!(matches!(report, Report::Released { input: 15, .. }));
        assert_eq!(kept, None);
        assert_eq!(*log.borrow(), ["write 15", "clear"]);
    }

    #[test]
    fn a_failed_hand_back_stays_owed() {
        let (mut port, log) = fake(false);
        let (report, kept) = hand_back_steps(&mut port, &mut store(&log, true), claim());
        assert!(matches!(report, Report::Owed(_)));
        assert_eq!(kept, Some(claim()));
        assert_eq!(*log.borrow(), ["write 15"]);
    }

    #[test]
    fn settings_edits_dont_rewrite_a_claim() {
        assert!(claim_satisfies(&claim(), None, 17));
        assert!(claim_satisfies(&claim(), Some("TEST0001"), 17));
        assert!(!claim_satisfies(&claim(), Some("OTHER"), 17));
        assert!(!claim_satisfies(&claim(), None, 18));
    }

    #[test]
    fn claim_file_round_trip() {
        let dir = std::env::temp_dir().join(format!("hotswitch-claim-{}", std::process::id()));
        assert_eq!(load_claim(&dir), None);
        save_claim(&dir, Some(&claim())).unwrap();
        assert_eq!(load_claim(&dir), Some(claim()));
        save_claim(&dir, None).unwrap();
        assert_eq!(load_claim(&dir), None);
        save_claim(&dir, None).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn claims_from_the_read_based_build_are_ignored() {
        // That build stored `original`, not `restore`: nothing is written back from it.
        let dir = std::env::temp_dir().join(format!("hotswitch-old-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let old = r#"{"monitor":{"name":"X","serial":"S","serial_number":1,"vendor":1,"product":1},"input":17,"original":15}"#;
        std::fs::write(claim_path(&dir), old).unwrap();
        assert_eq!(load_claim(&dir), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The real worker loop against fake monitors that log writes and attach changes.
    struct FakeMonitors {
        log: Arc<Mutex<Vec<String>>>,
        attach_ok: bool,
    }

    struct FakePort {
        log: Arc<Mutex<Vec<String>>>,
    }

    impl InputPort for FakePort {
        fn write_input(&mut self, value: u16) -> Result<(), String> {
            self.log.lock().unwrap().push(format!("write {value}"));
            Ok(())
        }
    }

    impl Monitors for FakeMonitors {
        fn resolve(&mut self, _wanted: Option<&str>) -> Result<Identity, String> {
            Ok(monitor())
        }
        fn open(&mut self, _identity: &Identity) -> Result<Box<dyn InputPort>, String> {
            let log = self.log.clone();
            Ok(Box::new(FakePort { log }))
        }
        fn set_attached(&mut self, _identity: &Identity, attached: bool) -> Result<(), String> {
            let step = if attached { "attach" } else { "detach" };
            self.log.lock().unwrap().push(step.into());
            self.attach_ok
                .then_some(())
                .ok_or("BetterDisplay failed".into())
        }
        fn settle(&mut self) {}
    }

    type Parts = (Worker, Sender<Job>, Arc<Mutex<Vec<String>>>, PathBuf);

    /// A worker on a fresh temp folder; `seed` is a claim left there before it starts.
    fn worker(name: &str, attach_ok: bool, seed: Option<Claim>) -> Parts {
        let dir = std::env::temp_dir().join(format!("hotswitch-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        if let Some(c) = seed {
            save_claim(&dir, Some(&c)).unwrap();
        }
        let log = Arc::new(Mutex::new(Vec::new()));
        let monitors = FakeMonitors {
            log: log.clone(),
            attach_ok,
        };
        let (tx, rx) = mpsc::channel();
        let w = Worker::new(dir.clone(), Box::new(monitors), rx, Box::new(|_| {}));
        (w, tx, log, dir)
    }

    fn hold(detach: bool) -> Want {
        Want::Hold {
            monitor: None,
            input: 17,
            restore: 15,
            detach,
        }
    }

    #[test]
    fn ps5_mode_detaches_after_the_switch_and_reattaches_before_hand_back() {
        let (mut w, tx, log, dir) = worker("detach", true, None);
        drop(tx);
        w.want = hold(true);
        assert!(w.reconcile(&hold(true)));
        assert_eq!(*log.lock().unwrap(), ["write 17", "detach"]);
        assert!(load_claim(&dir).is_some_and(|c| c.detached));
        w.want = Want::Release;
        assert!(w.reconcile(&Want::Release));
        assert_eq!(
            *log.lock().unwrap(),
            ["write 17", "detach", "attach", "write 15"]
        );
        assert_eq!(load_claim(&dir), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_refused_detach_isnt_undone_later() {
        let (mut w, tx, log, dir) = worker("nodetach", false, None);
        drop(tx);
        w.want = hold(true);
        assert!(w.reconcile(&hold(true)));
        assert!(load_claim(&dir).is_some_and(|c| !c.detached));
        w.want = Want::Release;
        assert!(w.reconcile(&Want::Release));
        assert_eq!(*log.lock().unwrap(), ["write 17", "detach", "write 15"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_reattach_keeps_the_hand_back_owed() {
        let detached = Claim {
            detached: true,
            ..claim()
        };
        let (mut w, tx, log, dir) = worker("noattach", false, Some(detached.clone()));
        drop(tx);
        assert!(w.reconcile(&Want::Release));
        assert_eq!(*log.lock().unwrap(), ["attach"]);
        assert_eq!(load_claim(&dir), Some(detached));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_release_queued_during_the_cool_down_wins() {
        let (mut w, tx, log, dir) = worker("cooldown", true, None);
        w.last_ddc = Some(Instant::now()); // as if a DDC job just finished
        tx.send(Job::Want(hold(false))).unwrap();
        let runner = thread::spawn(move || w.run());
        thread::sleep(Duration::from_millis(200));
        tx.send(Job::Want(Want::Release)).unwrap();
        drop(tx);
        runner.join().unwrap();
        assert!(log.lock().unwrap().is_empty(), "{:?}", log.lock().unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_crash_leaves_a_hand_back_that_runs_at_launch() {
        let detached = Claim {
            detached: true,
            ..claim()
        };
        let (w, tx, log, dir) = worker("crash", true, Some(detached));
        drop(tx);
        w.run();
        assert_eq!(*log.lock().unwrap(), ["attach", "write 15"]);
        assert_eq!(load_claim(&dir), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn re_entering_with_an_owed_claim_switches_again() {
        let (mut w, tx, log, dir) = worker("owed", true, None);
        w.claim = Some(claim());
        tx.send(Job::Want(hold(false))).unwrap();
        drop(tx);
        w.run();
        assert_eq!(*log.lock().unwrap(), ["write 17"]);
        assert_eq!(load_claim(&dir), Some(claim()));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
