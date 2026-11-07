//! AppKit status item and menu (port of `build_menu` / `set_status` in `status_bar.py`).
//! The UI renders a plain [`MenuModel`] and reports clicks as [`Action`]s; it holds no app state.

use std::cell::RefCell;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, ProtocolObject};
use objc2::{define_class, msg_send, sel, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSControlStateValueOff, NSControlStateValueOn, NSImage, NSMenu, NSMenuDelegate,
    NSMenuItem, NSStatusBar, NSStatusItem, NSVariableStatusItemLength,
};
use objc2_foundation::{NSObjectProtocol, NSString, NSURL};

use crate::session::state_for_mode;
use crate::settings::{ClosedLidPolicy, LedDisplay};
use sidepulse_core::models::AgentMode;

#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    OpenSession(String),
    SetDisplay(String, LedDisplay),
    SetBrightness(String, u8),
    RemoveDevice(String),
    SetLidPolicy(ClosedLidPolicy),
    Quit,
    /// The menu is about to open (`menuNeedsUpdate:`): refresh time-dependent rows.
    MenuOpening,
}

impl Action {
    fn encode(&self) -> String {
        match self {
            Action::OpenSession(id) => format!("open\t{id}"),
            Action::SetDisplay(p, d) => format!("display\t{p}\t{}", d.as_str()),
            Action::SetBrightness(p, b) => format!("brightness\t{p}\t{b}"),
            Action::RemoveDevice(p) => format!("remove\t{p}"),
            Action::SetLidPolicy(l) => format!("lid\t{}", l.as_str()),
            Action::Quit => "quit".into(),
            Action::MenuOpening => "menu-opening".into(),
        }
    }

    fn decode(s: &str) -> Option<Self> {
        let parts: Vec<&str> = s.split('\t').collect();
        Some(match parts.as_slice() {
            ["open", id] => Action::OpenSession((*id).into()),
            ["display", p, "agent"] => Action::SetDisplay((*p).into(), LedDisplay::Agent),
            ["display", p, "battery"] => Action::SetDisplay((*p).into(), LedDisplay::Battery),
            ["brightness", p, b] => Action::SetBrightness((*p).into(), b.parse().ok()?),
            ["remove", p] => Action::RemoveDevice((*p).into()),
            ["lid", l] => Action::SetLidPolicy(ClosedLidPolicy::parse(l)?),
            ["quit"] => Action::Quit,
            ["menu-opening"] => Action::MenuOpening,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct SessionRow {
    pub title: String,
    pub mode: AgentMode,
    pub agent_id: String,
    pub tooltip: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DeviceRow {
    pub path: String,
    pub name: String,
    pub connected: bool,
    pub display: LedDisplay,
    pub brightness: u8,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MenuModel {
    pub sessions: Vec<SessionRow>,
    pub devices: Vec<DeviceRow>,
    pub lid_policy: ClosedLidPolicy,
    pub sleep_warning: Option<String>,
}

type Handler = Box<dyn Fn(Action)>;

thread_local! {
    static HANDLER: RefCell<Option<Handler>> = const { RefCell::new(None) };
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "SidePulseMenuTarget"]
    struct MenuTarget;

    impl MenuTarget {
        #[unsafe(method(perform:))]
        fn perform(&self, sender: &NSMenuItem) {
            let Some(obj) = sender.representedObject() else { return };
            let Ok(text) = obj.downcast::<NSString>() else { return };
            if let Some(action) = Action::decode(&text.to_string()) {
                dispatch(action);
            }
        }
    }

    unsafe impl NSObjectProtocol for MenuTarget {}

    unsafe impl NSMenuDelegate for MenuTarget {
        /// Called just before the menu is shown; items may be replaced here.
        #[unsafe(method(menuNeedsUpdate:))]
        fn menu_needs_update(&self, _menu: &NSMenu) {
            dispatch(Action::MenuOpening);
        }
    }
);

fn dispatch(action: Action) {
    HANDLER.with(|h| {
        if let Some(handler) = h.borrow().as_ref() {
            handler(action);
        }
    });
}

impl MenuTarget {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = mtm.alloc::<Self>().set_ivars(());
        unsafe { msg_send![super(this), init] }
    }
}

pub struct StatusUi {
    mtm: MainThreadMarker,
    item: Retained<NSStatusItem>,
    target: Retained<MenuTarget>,
    /// One menu for the app's lifetime, refilled in place so it can be updated while opening.
    menu: Retained<NSMenu>,
    last_mode: Option<AgentMode>,
    last_model: Option<MenuModel>,
}

fn symbol(name: &str, description: &str) -> Option<Retained<NSImage>> {
    let img = NSImage::imageWithSystemSymbolName_accessibilityDescription(
        &NSString::from_str(name),
        Some(&NSString::from_str(description)),
    )?;
    img.setTemplate(true);
    Some(img)
}

impl StatusUi {
    pub fn new(mtm: MainThreadMarker, handler: Box<dyn Fn(Action)>) -> Self {
        HANDLER.with(|h| *h.borrow_mut() = Some(handler));
        let item = NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);
        let target = MenuTarget::new(mtm);
        let menu = NSMenu::new(mtm);
        menu.setAutoenablesItems(false);
        // The delegate is weak; `target` is kept alive by `StatusUi`.
        menu.setDelegate(Some(ProtocolObject::from_ref(&*target)));
        item.setMenu(Some(&menu));
        Self {
            mtm,
            item,
            target,
            menu,
            last_mode: None,
            last_model: None,
        }
    }

    /// `set_status`: title, template symbol and tooltip; no-op when unchanged.
    pub fn set_mode(&mut self, mode: AgentMode) {
        let state = state_for_mode(mode);
        if self.last_mode.map(|m| state_for_mode(m).label) == Some(state.label) {
            return;
        }
        self.last_mode = Some(mode);
        if let Some(button) = self.item.button(self.mtm) {
            button.setTitle(&NSString::from_str(&format!(" {}", state.label)));
            button.setImage(symbol(state.symbol, state.label).as_deref());
            button.setToolTip(Some(&NSString::from_str(&format!(
                "SidePulse Agent Monitor: {}",
                state.label
            ))));
        }
    }

    fn item(&self, title: &str, action: Option<&Action>) -> Retained<NSMenuItem> {
        let item = NSMenuItem::new(self.mtm);
        item.setTitle(&NSString::from_str(title));
        match action {
            Some(a) => unsafe {
                item.setTarget(Some(&self.target));
                item.setAction(Some(sel!(perform:)));
                let payload = NSString::from_str(&a.encode());
                item.setRepresentedObject(Some(&payload as &AnyObject));
            },
            None => item.setEnabled(false),
        }
        item
    }

    fn checked(&self, title: &str, action: &Action, on: bool) -> Retained<NSMenuItem> {
        let item = self.item(title, Some(action));
        item.setState(if on {
            NSControlStateValueOn
        } else {
            NSControlStateValueOff
        });
        item
    }

    /// `build_menu`: refilled only when the model changed.
    pub fn set_menu(&mut self, model: &MenuModel) {
        if self.last_model.as_ref() == Some(model) {
            return;
        }
        let menu = &self.menu;
        menu.removeAllItems();
        let add = |i: &NSMenuItem| menu.addItem(i);
        add(&self.item("SidePulse", None));
        add(&NSMenuItem::separatorItem(self.mtm));
        add(&self.item("Agents", None));
        if model.sessions.is_empty() {
            add(&self.item("No recent sessions", None));
        }
        for row in &model.sessions {
            let item = self.item(&row.title, Some(&Action::OpenSession(row.agent_id.clone())));
            let state = state_for_mode(row.mode);
            item.setImage(symbol(state.symbol, state.label).as_deref());
            item.setToolTip(Some(&NSString::from_str(&row.tooltip)));
            add(&item);
        }

        add(&NSMenuItem::separatorItem(self.mtm));
        add(&self.item("Devices", None));
        if model.devices.is_empty() {
            add(&self.item("No SidePulse device found", None));
        }
        for dev in &model.devices {
            let item = self.item(
                &dev.name,
                Some(&Action::SetDisplay(dev.path.clone(), dev.display)),
            );
            item.setState(if dev.connected {
                NSControlStateValueOn
            } else {
                NSControlStateValueOff
            });
            let sub = NSMenu::new(self.mtm);
            sub.setAutoenablesItems(false);
            sub.addItem(&self.checked(
                "Agent Status",
                &Action::SetDisplay(dev.path.clone(), LedDisplay::Agent),
                dev.display == LedDisplay::Agent,
            ));
            sub.addItem(&self.checked(
                "Battery Level",
                &Action::SetDisplay(dev.path.clone(), LedDisplay::Battery),
                dev.display == LedDisplay::Battery,
            ));
            sub.addItem(&NSMenuItem::separatorItem(self.mtm));
            let pct = (f64::from(dev.brightness) * 100.0 / 255.0).round() as u32;
            sub.addItem(&self.item(&format!("Brightness {pct}%"), None));
            for level in [25u32, 50, 75, 100] {
                let value = ((level as f64) * 255.0 / 100.0).round() as u8;
                sub.addItem(&self.checked(
                    &format!("    {level}%"),
                    &Action::SetBrightness(dev.path.clone(), value),
                    dev.brightness == value,
                ));
            }
            if !dev.connected {
                sub.addItem(&NSMenuItem::separatorItem(self.mtm));
                sub.addItem(&self.item("Not connected", None));
                sub.addItem(&self.item("Remove", Some(&Action::RemoveDevice(dev.path.clone()))));
            }
            item.setSubmenu(Some(&sub));
            add(&item);
        }

        add(&NSMenuItem::separatorItem(self.mtm));
        add(&self.item("Keep Awake With Lid Closed", None));
        for (title, policy) in [
            ("Never", ClosedLidPolicy::Never),
            ("When Agents Work", ClosedLidPolicy::Agents),
            ("Always", ClosedLidPolicy::Always),
        ] {
            add(&self.checked(
                title,
                &Action::SetLidPolicy(policy),
                model.lid_policy == policy,
            ));
        }
        if let Some(warning) = &model.sleep_warning {
            add(&self.item(&format!("Sleep warning: {warning}"), None));
        }

        add(&NSMenuItem::separatorItem(self.mtm));
        let quit = self.item("Quit SidePulse", Some(&Action::Quit));
        quit.setKeyEquivalent(&NSString::from_str("q"));
        add(&quit);
        self.last_model = Some(model.clone());
    }
}

pub fn open_url(url: &str) {
    if let Some(url) = NSURL::URLWithString(&NSString::from_str(url)) {
        objc2_app_kit::NSWorkspace::sharedWorkspace().openURL(&url);
    }
}

pub fn terminate(mtm: MainThreadMarker) {
    NSApplication::sharedApplication(mtm).terminate(None);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actions_round_trip() {
        for a in [
            Action::OpenSession("claude:session:abc".into()),
            Action::SetDisplay("/Volumes/Side Pulse".into(), LedDisplay::Battery),
            Action::SetBrightness("/Volumes/SidePulse".into(), 191),
            Action::RemoveDevice("/Volumes/X".into()),
            Action::SetLidPolicy(ClosedLidPolicy::Agents),
            Action::Quit,
            Action::MenuOpening,
        ] {
            assert_eq!(Action::decode(&a.encode()), Some(a));
        }
    }
}
