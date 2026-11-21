//! Which monitor input a mode holds, and handing it back.
//!
//! One worker thread does all of the app's DDC. The app only says what it wants: hold an input
//! on a monitor, or release whatever is held. The worker keeps one claim (the monitor, the input
//! it set, the input it found) in `monitor-claim.json`. The claim is written to disk before the
//! monitor is switched and removed only once the monitor is handed back or found moved on, so a
//! crash, quit, sleep or unplugged dock never loses it. Settings edits don't reach a claim:
//! it keeps the monitor and inputs it was made with.

use crate::ddc::{self, Identity, Layout, Target, VCP_INPUT};
use crate::settings::write_atomically;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

/// Least time between two DDC operations. Monitors behind a dock can drop off when pushed.
const SPACING: Duration = Duration::from_secs(1);
/// Pause between reading a monitor's input and writing a new one.
const STEP: Duration = Duration::from_millis(100);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claim {
    pub monitor: Identity,
    /// The input this claim put on the monitor.
    pub input: u16,
    /// The input the monitor showed before; hand-back restores it.
    pub original: u16,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Want {
    Release,
    /// Hold `input` on the monitor with this serial (or the only usable one), fixed when sent.
    Hold {
        monitor: Option<String>,
        input: u16,
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
    /// The monitor is free. `input` is what hand-back set, `None` when it was left as found.
    Released {
        name: String,
        input: Option<u16>,
    },
    /// The take didn't happen; the monitor wasn't switched.
    Refused(String),
    /// A hand-back is still owed; it is retried on wake, display changes and the next launch.
    Owed(String),
}

/// What a take does once it has read the monitor's input.
#[derive(Debug, PartialEq)]
pub enum TakeStep {
    /// Already on the wanted input: nothing to write and nothing to hand back later.
    AlreadyThere,
    /// Save this claim, then switch.
    Switch(Claim),
}

pub fn plan_take(monitor: Identity, current: u16, input: u16) -> TakeStep {
    if current == input {
        TakeStep::AlreadyThere
    } else {
        TakeStep::Switch(Claim {
            monitor,
            input,
            original: current,
        })
    }
}

/// What a hand-back does once it has read the monitor's input.
#[derive(Debug, PartialEq)]
pub enum HandBackStep {
    /// Still on our input: put the original back.
    Restore(u16),
    /// Someone moved it (the receiver hooks, the monitor's buttons): leave it alone.
    MovedOn,
}

pub fn plan_hand_back(claim: &Claim, current: u16) -> HandBackStep {
    if current == claim.input {
        HandBackStep::Restore(claim.original)
    } else {
        HandBackStep::MovedOn
    }
}

/// Whether the held claim already satisfies `Hold { monitor, input }`.
pub fn claim_satisfies(claim: &Claim, monitor: Option<&str>, input: u16) -> bool {
    claim.input == input && monitor.is_none_or(|m| m == claim.monitor.serial)
}

/// The two DDC operations a take or hand-back needs. `Target` is the real one; tests use a fake
/// to check the order of steps without hardware.
pub trait InputPort {
    fn read_input(&mut self) -> Result<u16, String>;
    fn write_input(&mut self, value: u16) -> Result<(), String>;
}

impl InputPort for Target {
    fn read_input(&mut self) -> Result<u16, String> {
        self.read(VCP_INPUT)
    }

    fn write_input(&mut self, value: u16) -> Result<(), String> {
        thread::sleep(STEP);
        self.write(VCP_INPUT, value)
    }
}

type SaveClaim<'a> = dyn FnMut(Option<&Claim>) -> std::io::Result<()> + 'a;

/// Read the input; if that fails, stop. If the request was withdrawn meanwhile, stop. Save the
/// claim; if that fails, stop. Only then switch. `existing` is a claim already held for this
/// monitor and input: it is trusted only once the read shows the monitor on that input.
/// Returns the report (`None` when withdrawn) and the claim now held.
pub fn take_steps(
    port: &mut dyn InputPort,
    save: &mut SaveClaim,
    still_wanted: &mut dyn FnMut() -> bool,
    identity: Identity,
    input: u16,
    existing: Option<Claim>,
) -> (Option<Report>, Option<Claim>) {
    let name = identity.name.clone();
    let current = match port.read_input() {
        Ok(v) => v,
        Err(e) => {
            let reason = format!("{e}, so {name} wasn't switched");
            return (Some(Report::Refused(reason)), existing);
        }
    };
    if !still_wanted() {
        return (None, existing);
    }
    let claim = match plan_take(identity, current, input) {
        TakeStep::AlreadyThere => return (Some(Report::Holding { name, input }), existing),
        TakeStep::Switch(claim) => claim,
    };
    if let Err(e) = save(Some(&claim)) {
        let reason = format!("Couldn't save the hand-back record ({e}), so {name} wasn't switched");
        return (Some(Report::Refused(reason)), existing);
    }
    eprintln!("monitor: saved claim {claim:?}, switching");
    match port.write_input(input) {
        // The claim stays: hand-back reads first and leaves the monitor alone if this write
        // never landed.
        Err(e) => (Some(Report::Refused(e)), Some(claim)),
        Ok(()) => (Some(Report::Holding { name, input }), Some(claim)),
    }
}

/// Read the input; if that fails, keep the claim and write nothing. Restore only if the
/// monitor still shows our input. Returns the report and the claim still owed, if any.
pub fn hand_back_steps(
    port: &mut dyn InputPort,
    save: &mut SaveClaim,
    claim: Claim,
) -> (Report, Option<Claim>) {
    let name = claim.monitor.name.clone();
    let current = match port.read_input() {
        Ok(v) => v,
        Err(e) => {
            return (
                Report::Owed(format!("{e}, so {name} was left alone for now")),
                Some(claim),
            )
        }
    };
    let set = match plan_hand_back(&claim, current) {
        HandBackStep::MovedOn => None,
        HandBackStep::Restore(original) => {
            if let Err(e) = port.write_input(original) {
                let reason = format!("{e}. {name} will be handed back later");
                return (Report::Owed(reason), Some(claim));
            }
            Some(original)
        }
    };
    if let Err(e) = save(None) {
        // Harmless: next time the monitor won't show our input, so it is left alone.
        eprintln!("monitor: couldn't remove the claim file: {e}");
    }
    (Report::Released { name, input: set }, None)
}

/// How the worker finds and opens monitors: the hardware, or a fake in tests.
pub trait Monitors: Send {
    fn resolve(&mut self, wanted: Option<&str>) -> Result<Identity, String>;
    fn open(&mut self, identity: &Identity) -> Result<Box<dyn InputPort>, String>;
}

struct Hardware;

impl Monitors for Hardware {
    fn resolve(&mut self, wanted: Option<&str>) -> Result<Identity, String> {
        // Registry only: no DDC until the target is settled.
        ddc::resolve(&Layout::read().targetable(), wanted)
    }

    fn open(&mut self, identity: &Identity) -> Result<Box<dyn InputPort>, String> {
        Ok(Box::new(Target::open(identity)?))
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

    /// Carry out `wanted`. False when a newer request replaced it before any write.
    fn reconcile(&mut self, wanted: &Want) -> bool {
        match wanted.clone() {
            Want::Release => match self.claim.clone() {
                None => true,
                Some(claim) => self.hand_back(claim, wanted),
            },
            Want::Hold { monitor, input } => {
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
                self.take(monitor.as_deref(), input, existing, wanted)
            }
        }
    }

    fn take(
        &mut self,
        monitor: Option<&str>,
        input: u16,
        existing: Option<Claim>,
        wanted: &Want,
    ) -> bool {
        let identity = match &existing {
            Some(c) => Ok(c.monitor.clone()),
            None => self.monitors.resolve(monitor),
        };
        let mut port = match identity.clone().and_then(|id| self.open(&id)) {
            Ok(port) => port,
            Err(e) => {
                (self.report)(Report::Refused(e));
                return true;
            }
        };
        // Waiting for the lock can take a while; the user may have moved on.
        if self.superseded(wanted) {
            return false;
        }
        let dir = self.dir.clone();
        let (report, held) = take_steps(
            port.as_mut(),
            &mut |c| save_claim(&dir, c),
            &mut || !self.superseded(wanted),
            identity.expect("opened"),
            input,
            existing,
        );
        self.last_ddc = Some(Instant::now());
        self.claim = held;
        match report {
            Some(report) => {
                (self.report)(report);
                true
            }
            None => false,
        }
    }

    /// Reports `Owed` and keeps the claim whenever it can't be sure the monitor is free.
    fn hand_back(&mut self, claim: Claim, wanted: &Want) -> bool {
        let mut port = match self.open(&claim.monitor) {
            Ok(port) => port,
            Err(e) => {
                let name = &claim.monitor.name;
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

    use std::cell::RefCell;
    use std::rc::Rc;

    /// A monitor that logs every step, sharing the log with the fake claim store.
    struct Fake {
        log: Rc<RefCell<Vec<String>>>,
        current: Result<u16, String>,
        write_ok: bool,
    }

    impl InputPort for Fake {
        fn read_input(&mut self) -> Result<u16, String> {
            self.log.borrow_mut().push("read".into());
            self.current.clone()
        }
        fn write_input(&mut self, value: u16) -> Result<(), String> {
            self.log.borrow_mut().push(format!("write {value}"));
            self.write_ok.then_some(()).ok_or("write failed".into())
        }
    }

    fn fake(current: Result<u16, String>, write_ok: bool) -> (Fake, Rc<RefCell<Vec<String>>>) {
        let log = Rc::new(RefCell::new(Vec::new()));
        (
            Fake {
                log: log.clone(),
                current,
                write_ok,
            },
            log,
        )
    }

    fn store(
        log: &Rc<RefCell<Vec<String>>>,
        ok: bool,
    ) -> impl FnMut(Option<&Claim>) -> std::io::Result<()> {
        let log = log.clone();
        move |c| {
            log.borrow_mut().push(match c {
                Some(c) => format!("save {}->{}", c.original, c.input),
                None => "clear".into(),
            });
            if ok {
                Ok(())
            } else {
                Err(std::io::Error::other("disk full"))
            }
        }
    }

    fn claim() -> Claim {
        Claim {
            monitor: monitor(),
            input: 17,
            original: 15,
        }
    }

    #[test]
    fn take_never_writes_after_a_failed_read() {
        let (mut port, log) = fake(Err("DDC read failed".into()), true);
        let (report, held) = take_steps(
            &mut port,
            &mut store(&log, true),
            &mut || true,
            monitor(),
            17,
            None,
        );
        assert!(matches!(report, Some(Report::Refused(_))));
        assert_eq!(held, None);
        assert_eq!(*log.borrow(), ["read"]);
    }

    #[test]
    fn take_saves_the_claim_before_switching() {
        let (mut port, log) = fake(Ok(15), true);
        let (report, held) = take_steps(
            &mut port,
            &mut store(&log, true),
            &mut || true,
            monitor(),
            17,
            None,
        );
        assert!(matches!(report, Some(Report::Holding { input: 17, .. })));
        assert_eq!(held, Some(claim()));
        assert_eq!(*log.borrow(), ["read", "save 15->17", "write 17"]);
    }

    #[test]
    fn take_never_writes_when_the_claim_cant_be_saved() {
        let (mut port, log) = fake(Ok(15), true);
        let (report, held) = take_steps(
            &mut port,
            &mut store(&log, false),
            &mut || true,
            monitor(),
            17,
            None,
        );
        assert!(matches!(report, Some(Report::Refused(_))));
        assert_eq!(held, None);
        assert_eq!(*log.borrow(), ["read", "save 15->17"]);
    }

    #[test]
    fn take_keeps_the_claim_when_the_write_fails() {
        let (mut port, log) = fake(Ok(15), false);
        let (_, held) = take_steps(
            &mut port,
            &mut store(&log, true),
            &mut || true,
            monitor(),
            17,
            None,
        );
        assert_eq!(held, Some(claim()));
        assert_eq!(*log.borrow(), ["read", "save 15->17", "write 17"]);
    }

    #[test]
    fn take_of_an_input_already_shown_claims_nothing() {
        let (mut port, log) = fake(Ok(17), true);
        let (report, held) = take_steps(
            &mut port,
            &mut store(&log, true),
            &mut || true,
            monitor(),
            17,
            None,
        );
        assert!(matches!(report, Some(Report::Holding { .. })));
        assert_eq!(held, None);
        assert_eq!(*log.borrow(), ["read"]);
    }

    #[test]
    fn hand_back_never_writes_after_a_failed_read() {
        let (mut port, log) = fake(Err("DDC read failed".into()), true);
        let (report, kept) = hand_back_steps(&mut port, &mut store(&log, true), claim());
        assert!(matches!(report, Report::Owed(_)));
        assert_eq!(kept, Some(claim()));
        assert_eq!(*log.borrow(), ["read"]);
    }

    #[test]
    fn hand_back_restores_then_clears() {
        let (mut port, log) = fake(Ok(17), true);
        let (report, kept) = hand_back_steps(&mut port, &mut store(&log, true), claim());
        assert!(matches!(
            report,
            Report::Released {
                input: Some(15),
                ..
            }
        ));
        assert_eq!(kept, None);
        assert_eq!(*log.borrow(), ["read", "write 15", "clear"]);
    }

    #[test]
    fn hand_back_leaves_a_monitor_someone_moved() {
        let (mut port, log) = fake(Ok(16), true);
        let (report, kept) = hand_back_steps(&mut port, &mut store(&log, true), claim());
        assert!(matches!(report, Report::Released { input: None, .. }));
        assert_eq!(kept, None);
        assert_eq!(*log.borrow(), ["read", "clear"]);
    }

    #[test]
    fn failed_hand_back_stays_owed() {
        let (mut port, log) = fake(Ok(17), false);
        let (report, kept) = hand_back_steps(&mut port, &mut store(&log, true), claim());
        assert!(matches!(report, Report::Owed(_)));
        assert_eq!(kept, Some(claim()));
        assert_eq!(*log.borrow(), ["read", "write 15"]);
    }

    #[test]
    fn take_stops_when_withdrawn_after_the_read() {
        let (mut port, log) = fake(Ok(15), true);
        let (report, held) = take_steps(
            &mut port,
            &mut store(&log, true),
            &mut || false,
            monitor(),
            17,
            None,
        );
        assert!(report.is_none());
        assert_eq!(held, None);
        assert_eq!(*log.borrow(), ["read"]);
    }

    #[test]
    fn an_owed_claim_is_checked_not_trusted() {
        // Still on our input: holding, and the original obligation stays.
        let (mut port, log) = fake(Ok(17), true);
        let (report, held) = take_steps(
            &mut port,
            &mut store(&log, true),
            &mut || true,
            monitor(),
            17,
            Some(claim()),
        );
        assert!(matches!(report, Some(Report::Holding { .. })));
        assert_eq!(held, Some(claim()));
        assert_eq!(*log.borrow(), ["read"]);
        // Back on the original (the earlier write never landed): switch again.
        let (mut port, log) = fake(Ok(15), true);
        let (_, held) = take_steps(
            &mut port,
            &mut store(&log, true),
            &mut || true,
            monitor(),
            17,
            Some(claim()),
        );
        assert_eq!(held, Some(claim()));
        assert_eq!(*log.borrow(), ["read", "save 15->17", "write 17"]);
    }

    /// The real worker loop against a fake monitor that remembers its input.
    struct FakeMonitors {
        log: Arc<Mutex<Vec<String>>>,
        input: Arc<Mutex<u16>>,
    }

    struct FakePort {
        log: Arc<Mutex<Vec<String>>>,
        input: Arc<Mutex<u16>>,
    }

    impl InputPort for FakePort {
        fn read_input(&mut self) -> Result<u16, String> {
            self.log.lock().unwrap().push("read".into());
            Ok(*self.input.lock().unwrap())
        }
        fn write_input(&mut self, value: u16) -> Result<(), String> {
            self.log.lock().unwrap().push(format!("write {value}"));
            *self.input.lock().unwrap() = value;
            Ok(())
        }
    }

    impl Monitors for FakeMonitors {
        fn resolve(&mut self, _wanted: Option<&str>) -> Result<Identity, String> {
            Ok(monitor())
        }
        fn open(&mut self, _identity: &Identity) -> Result<Box<dyn InputPort>, String> {
            Ok(Box::new(FakePort {
                log: self.log.clone(),
                input: self.input.clone(),
            }))
        }
    }

    fn worker(name: &str, input: u16) -> (Worker, Sender<Job>, Arc<Mutex<Vec<String>>>, PathBuf) {
        let dir = std::env::temp_dir().join(format!("hotswitch-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let log = Arc::new(Mutex::new(Vec::new()));
        let monitors = FakeMonitors {
            log: log.clone(),
            input: Arc::new(Mutex::new(input)),
        };
        let (tx, rx) = mpsc::channel();
        let w = Worker::new(dir.clone(), Box::new(monitors), rx, Box::new(|_| {}));
        (w, tx, log, dir)
    }

    #[test]
    fn a_release_queued_during_the_cool_down_wins() {
        let (mut w, tx, log, dir) = worker("cooldown", 15);
        w.last_ddc = Some(Instant::now()); // as if a DDC job just finished
        tx.send(Job::Want(Want::Hold {
            monitor: None,
            input: 17,
        }))
        .unwrap();
        let runner = thread::spawn(move || w.run());
        thread::sleep(Duration::from_millis(200));
        tx.send(Job::Want(Want::Release)).unwrap();
        drop(tx);
        runner.join().unwrap();
        assert!(log.lock().unwrap().is_empty(), "{:?}", log.lock().unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_owed_claim_is_read_before_holding() {
        let (mut w, tx, log, dir) = worker("owed", 15);
        // A failed hand-back kept this claim, but the monitor is on 15 again.
        w.claim = Some(claim());
        tx.send(Job::Want(Want::Hold {
            monitor: None,
            input: 17,
        }))
        .unwrap();
        drop(tx);
        w.run();
        assert_eq!(*log.lock().unwrap(), ["read", "write 17"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn take_saves_the_input_it_found() {
        assert_eq!(plan_take(monitor(), 0x11, 0x11), TakeStep::AlreadyThere);
        assert_eq!(
            plan_take(monitor(), 0x0F, 0x11),
            TakeStep::Switch(Claim {
                monitor: monitor(),
                input: 0x11,
                original: 0x0F
            })
        );
    }

    #[test]
    fn hand_back_leaves_a_moved_monitor_alone() {
        let claim = Claim {
            monitor: monitor(),
            input: 0x11,
            original: 0x0F,
        };
        assert_eq!(plan_hand_back(&claim, 0x11), HandBackStep::Restore(0x0F));
        assert_eq!(plan_hand_back(&claim, 0x10), HandBackStep::MovedOn);
        assert_eq!(plan_hand_back(&claim, 0x0F), HandBackStep::MovedOn);
    }

    #[test]
    fn settings_edits_dont_rewrite_a_claim() {
        let claim = Claim {
            monitor: monitor(),
            input: 0x11,
            original: 0x0F,
        };
        assert!(claim_satisfies(&claim, None, 0x11));
        assert!(claim_satisfies(&claim, Some("TEST0001"), 0x11));
        assert!(!claim_satisfies(&claim, Some("OTHER"), 0x11));
        assert!(!claim_satisfies(&claim, None, 0x12));
        // Hand-back uses the claim's own values, whatever the settings say now.
        assert_eq!(plan_hand_back(&claim, 0x11), HandBackStep::Restore(0x0F));
    }

    #[test]
    fn claim_file_round_trip() {
        let dir = std::env::temp_dir().join(format!("hotswitch-claim-{}", std::process::id()));
        let claim = Claim {
            monitor: monitor(),
            input: 0x11,
            original: 0x0F,
        };
        assert_eq!(load_claim(&dir), None);
        save_claim(&dir, Some(&claim)).unwrap();
        assert_eq!(load_claim(&dir), Some(claim));
        save_claim(&dir, None).unwrap();
        assert_eq!(load_claim(&dir), None);
        save_claim(&dir, None).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
