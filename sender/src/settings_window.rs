//! The settings window: one small native AppKit form instead of chains of prompts.

use crate::settings::{Mode, Settings};
use crate::{ddc, passthrough};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{define_class, msg_send, sel, DefinedClass, MainThreadMarker, MainThreadOnly, Message};
use objc2_app_kit::{
    NSApplication, NSBackingStoreType, NSButton, NSColor, NSControlStateValueOff,
    NSControlStateValueOn, NSFont, NSGridCellPlacement, NSGridRowAlignment, NSGridView,
    NSPopUpButton, NSTextField, NSView, NSWindow, NSWindowDelegate, NSWindowStyleMask,
};
use objc2_foundation::{
    NSArray, NSNotification, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString,
};

#[derive(Debug)]
pub enum Action {
    Receiver(String),
    Monitor(Option<String>),
    Ps5Input(u16),
    MacInput(u16),
    AudioInput(String),
    RecordHotkey(Mode),
    StartOnLogin(bool),
    Closed,
}

struct Ivars {
    on_action: Box<dyn Fn(Action)>,
    monitor_ids: std::cell::RefCell<Vec<Option<String>>>,
    audio_names: std::cell::RefCell<Vec<String>>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "HotswitchSettingsHandler"]
    #[ivars = Ivars]
    struct Handler;

    unsafe impl NSObjectProtocol for Handler {}

    unsafe impl NSWindowDelegate for Handler {
        #[unsafe(method(windowWillClose:))]
        fn window_will_close(&self, _notification: &NSNotification) {
            (self.ivars().on_action)(Action::Closed);
        }
    }

    impl Handler {
        #[unsafe(method(receiverChanged:))]
        fn receiver_changed(&self, sender: &NSTextField) {
            (self.ivars().on_action)(Action::Receiver(sender.stringValue().to_string()));
        }

        #[unsafe(method(monitorChanged:))]
        fn monitor_changed(&self, sender: &NSPopUpButton) {
            let ids = self.ivars().monitor_ids.borrow();
            if let Some(id) = ids.get(sender.indexOfSelectedItem() as usize) {
                (self.ivars().on_action)(Action::Monitor(id.clone()));
            }
        }

        #[unsafe(method(inputChanged:))]
        fn input_changed(&self, sender: &NSPopUpButton) {
            if let Some((value, _)) = ddc::INPUTS.get(sender.indexOfSelectedItem() as usize) {
                (self.ivars().on_action)(Action::Ps5Input(*value));
            }
        }

        #[unsafe(method(macInputChanged:))]
        fn mac_input_changed(&self, sender: &NSPopUpButton) {
            if let Some((value, _)) = ddc::INPUTS.get(sender.indexOfSelectedItem() as usize) {
                (self.ivars().on_action)(Action::MacInput(*value));
            }
        }

        #[unsafe(method(audioChanged:))]
        fn audio_changed(&self, sender: &NSPopUpButton) {
            let names = self.ivars().audio_names.borrow();
            if let Some(name) = names.get(sender.indexOfSelectedItem() as usize) {
                (self.ivars().on_action)(Action::AudioInput(name.clone()));
            }
        }

        #[unsafe(method(recordHotkey:))]
        fn record_hotkey(&self, sender: &NSButton) {
            if let Some(mode) = Mode::ALL.get(sender.tag() as usize) {
                (self.ivars().on_action)(Action::RecordHotkey(*mode));
            }
        }

        #[unsafe(method(loginToggled:))]
        fn login_toggled(&self, sender: &NSButton) {
            (self.ivars().on_action)(Action::StartOnLogin(sender.state() == NSControlStateValueOn));
        }
    }
);

pub struct SettingsWindow {
    mtm: MainThreadMarker,
    window: Retained<NSWindow>,
    handler: Retained<Handler>,
    receiver: Retained<NSTextField>,
    receiver_note: Retained<NSTextField>,
    monitor: Retained<NSPopUpButton>,
    ps5_input: Retained<NSPopUpButton>,
    mac_input: Retained<NSPopUpButton>,
    audio_input: Retained<NSPopUpButton>,
    hotkeys: Vec<Retained<NSButton>>,
    login: Retained<NSButton>,
}

fn ns(s: &str) -> Retained<NSString> {
    NSString::from_str(s)
}

impl SettingsWindow {
    pub fn new(mtm: MainThreadMarker, on_action: impl Fn(Action) + 'static) -> Self {
        let handler: Retained<Handler> = {
            let this = Handler::alloc(mtm).set_ivars(Ivars {
                on_action: Box::new(on_action),
                monitor_ids: Default::default(),
                audio_names: Default::default(),
            });
            unsafe { msg_send![super(this), init] }
        };
        let target: Option<&AnyObject> = Some(&handler);

        let label = |text: &str| -> Retained<NSView> {
            Retained::into_super(Retained::into_super(NSTextField::labelWithString(
                &ns(text),
                mtm,
            )))
        };
        let popup = |action| {
            let p = NSPopUpButton::initWithFrame_pullsDown(
                NSPopUpButton::alloc(mtm),
                NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(260.0, 26.0)),
                false,
            );
            unsafe {
                p.setTarget(target);
                p.setAction(Some(action));
            }
            p
        };

        let receiver = NSTextField::textFieldWithString(&ns(""), mtm);
        receiver.setPlaceholderString(Some(&ns("10.0.0.100:24801")));
        unsafe {
            receiver.setTarget(target);
            receiver.setAction(Some(sel!(receiverChanged:)));
        }
        let receiver_note = NSTextField::labelWithString(&ns(""), mtm);
        receiver_note.setFont(Some(&NSFont::systemFontOfSize(
            NSFont::smallSystemFontSize(),
        )));
        receiver_note.setTextColor(Some(&NSColor::secondaryLabelColor()));

        let monitor = popup(sel!(monitorChanged:));
        let ps5_input = popup(sel!(inputChanged:));
        let mac_input = popup(sel!(macInputChanged:));
        for (_, name) in ddc::INPUTS {
            ps5_input.addItemWithTitle(&ns(name));
            mac_input.addItemWithTitle(&ns(name));
        }
        let audio_input = popup(sel!(audioChanged:));

        let hotkeys: Vec<Retained<NSButton>> = Mode::ALL
            .iter()
            .map(|mode| {
                let b = unsafe {
                    NSButton::buttonWithTitle_target_action(
                        &ns("Record Shortcut"),
                        target,
                        Some(sel!(recordHotkey:)),
                        mtm,
                    )
                };
                b.setTag(mode.index() as isize);
                b
            })
            .collect();
        let hint = NSTextField::wrappingLabelWithString(
            &ns(
                "Click a shortcut, then press the new keys. Delete clears it, Escape cancels. \
                 Pressing the current mode's shortcut again goes back to Mac.",
            ),
            mtm,
        );
        hint.setFont(Some(&NSFont::systemFontOfSize(
            NSFont::smallSystemFontSize(),
        )));
        hint.setTextColor(Some(&NSColor::secondaryLabelColor()));
        hint.setPreferredMaxLayoutWidth(260.0);

        let login = unsafe {
            NSButton::checkboxWithTitle_target_action(
                &ns("Start Hotswitch at login"),
                target,
                Some(sel!(loginToggled:)),
                mtm,
            )
        };

        let view = |v: &NSView| -> Retained<NSView> { v.retain() };
        let empty = || NSView::new(mtm);
        let rows: Vec<Retained<NSArray<NSView>>> = vec![
            NSArray::from_retained_slice(&[label("Windows PC:"), view(&receiver)]),
            NSArray::from_retained_slice(&[empty(), view(&receiver_note)]),
            NSArray::from_retained_slice(&[label("Monitor:"), view(&monitor)]),
            NSArray::from_retained_slice(&[label("PS5 input:"), view(&ps5_input)]),
            NSArray::from_retained_slice(&[label("Mac input:"), view(&mac_input)]),
            NSArray::from_retained_slice(&[label("PS5 audio from:"), view(&audio_input)]),
            NSArray::from_retained_slice(&[label("Switch to Mac:"), view(&hotkeys[0])]),
            NSArray::from_retained_slice(&[label("Switch to PC:"), view(&hotkeys[1])]),
            NSArray::from_retained_slice(&[label("Switch to PS5:"), view(&hotkeys[2])]),
            NSArray::from_retained_slice(&[empty(), view(&hint)]),
            NSArray::from_retained_slice(&[empty(), view(&login)]),
        ];
        let grid = NSGridView::gridViewWithViews(&NSArray::from_retained_slice(&rows), mtm);
        grid.setRowSpacing(8.0);
        grid.setColumnSpacing(8.0);
        grid.setRowAlignment(NSGridRowAlignment::FirstBaseline);
        grid.columnAtIndex(0)
            .setXPlacement(NSGridCellPlacement::Trailing);
        grid.columnAtIndex(1)
            .setXPlacement(NSGridCellPlacement::Fill);
        grid.columnAtIndex(1).setWidth(260.0);
        for b in hotkeys.iter().map(|b| view(b)).chain([view(&login)]) {
            if let Some(cell) = grid.cellForView(&b) {
                cell.setXPlacement(NSGridCellPlacement::Leading);
            }
        }
        grid.rowAtIndex(1).setTopPadding(-4.0);
        grid.rowAtIndex(2).setTopPadding(12.0);
        grid.rowAtIndex(6).setTopPadding(12.0);
        grid.rowAtIndex(9).setTopPadding(-2.0);
        grid.rowAtIndex(10).setTopPadding(12.0);

        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(420.0, 360.0)),
                NSWindowStyleMask::Titled | NSWindowStyleMask::Closable,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        unsafe { window.setReleasedWhenClosed(false) };
        window.setTitle(&ns("Hotswitch Settings"));
        window.setDelegate(Some(ProtocolObject::from_ref(&*handler)));
        let content = window.contentView().expect("window has a content view");
        grid.setTranslatesAutoresizingMaskIntoConstraints(false);
        content.addSubview(&grid);
        for c in [
            grid.leadingAnchor()
                .constraintEqualToAnchor_constant(&content.leadingAnchor(), 24.0),
            content
                .trailingAnchor()
                .constraintEqualToAnchor_constant(&grid.trailingAnchor(), 24.0),
            grid.topAnchor()
                .constraintEqualToAnchor_constant(&content.topAnchor(), 20.0),
            content
                .bottomAnchor()
                .constraintEqualToAnchor_constant(&grid.bottomAnchor(), 20.0),
        ] {
            c.setActive(true);
        }
        window.setContentSize(content.fittingSize());

        Self {
            mtm,
            window,
            handler,
            receiver,
            receiver_note,
            monitor,
            ps5_input,
            mac_input,
            audio_input,
            hotkeys,
            login,
        }
    }

    /// Fill in the current values and bring the window forward.
    pub fn show(&self, settings: &Settings, start_on_login: bool, pc_status: &str) {
        self.receiver
            .setStringValue(&ns(settings.receiver.as_deref().unwrap_or("")));
        self.set_receiver_note(pc_status, false);

        // Every monitor with its own serial (registry only, nothing is sent). Nothing is
        // chosen until a monitor is saved.
        let mut ids: Vec<Option<String>> = Vec::new();
        let mut titles: Vec<String> = Vec::new();
        if settings.monitor.is_none() {
            ids.push(None);
            titles.push("Choose a monitor".into());
        }
        for m in ddc::Layout::read().targetable() {
            ids.push(Some(m.serial.clone()));
            titles.push(m.label());
        }
        if let Some(saved) = &settings.monitor {
            if !ids.contains(&Some(saved.clone())) {
                ids.push(Some(saved.clone()));
                titles.push(format!("{saved} (not connected)"));
            }
        }
        let selected = match &settings.monitor {
            Some(saved) => ids.iter().position(|id| id.as_deref() == Some(saved)),
            None => Some(0),
        };
        fill(&self.monitor, &titles, selected.unwrap_or(0));
        *self.handler.ivars().monitor_ids.borrow_mut() = ids;

        for (popup, value) in [
            (&self.ps5_input, settings.ps5_input),
            (&self.mac_input, settings.mac_input),
        ] {
            let index = ddc::INPUTS.iter().position(|(v, _)| *v == value);
            popup.selectItemAtIndex(index.unwrap_or(0) as isize);
        }

        // Input devices only, one item per name: a popup drops an item whose title repeats,
        // which shifted every index after it. The saved device stays listed when it's absent.
        let mut names: Vec<String> = passthrough::input_devices()
            .into_iter()
            .map(|(_, name)| name)
            .collect();
        let mut titles = names.clone();
        if !names.contains(&settings.audio_input) {
            names.push(settings.audio_input.clone());
            titles.push(format!("{} (not available)", settings.audio_input));
        }
        let selected = names.iter().position(|n| *n == settings.audio_input);
        fill(&self.audio_input, &titles, selected.unwrap_or(0));
        *self.handler.ivars().audio_names.borrow_mut() = names;

        for mode in Mode::ALL {
            self.set_hotkey_title(mode, settings, false);
        }
        self.login.setState(if start_on_login {
            NSControlStateValueOn
        } else {
            NSControlStateValueOff
        });

        if !self.window.isVisible() {
            self.window.center();
        }
        self.window.makeKeyAndOrderFront(None);
        NSApplication::sharedApplication(self.mtm).activate();
    }

    pub fn set_receiver_note(&self, text: &str, error: bool) {
        self.receiver_note.setStringValue(&ns(text));
        let color = if error {
            NSColor::systemRedColor()
        } else {
            NSColor::secondaryLabelColor()
        };
        self.receiver_note.setTextColor(Some(&color));
    }

    pub fn receiver_text(&self) -> String {
        self.receiver.stringValue().to_string()
    }

    pub fn set_hotkey_title(&self, mode: Mode, settings: &Settings, recording: bool) {
        let title = if recording {
            "Type Shortcut…".to_string()
        } else {
            settings.hotkeys[mode.index()]
                .map(|h| h.label())
                .unwrap_or_else(|| "Record Shortcut".into())
        };
        self.hotkeys[mode.index()].setTitle(&ns(&title));
    }

    pub fn is_visible(&self) -> bool {
        self.window.isVisible()
    }
}

fn fill(popup: &NSPopUpButton, titles: &[String], selected: usize) {
    popup.removeAllItems();
    for t in titles {
        popup.addItemWithTitle(&ns(t));
    }
    popup.selectItemAtIndex(selected as isize);
}
