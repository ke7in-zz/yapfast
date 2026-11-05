//! Watches push-to-talk and command chords globally through the desktop's
//! GlobalShortcuts portal on Wayland, XI raw input on X11, and Winit's existing
//! raw-input receiver on Windows. A Windows low-level keyboard hook carries
//! PTT only. Where global PTT cannot work, the configured key works while
//! Yapfast's window has focus. The focused fallback and global listener own
//! separate holds; a serialized aggregator publishes their logical OR.
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use serde::{Deserialize, Serialize};
use slint::winit_030::winit::{
    event::{ElementState, KeyEvent, WindowEvent},
    keyboard::{Key, KeyLocation, NamedKey, PhysicalKey},
    platform::scancode::PhysicalKeyExtScancode,
};
use yapfast_core::runtime::BackgroundRuntime;

/// A key recorded in settings.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Keybind {
    /// winit's scan code for it: evdev's on Linux, which X11 reports 8
    /// higher, and set 1's on Windows, with 0xE000 for an extended key.
    pub scancode: u32,
    /// As settings show it, such as "V" or "Right Ctrl".
    pub name: String,
    /// Its XKB keysym name, such as "v" or "Control_R", which the
    /// GlobalShortcuts portal takes as the shortcut it would like.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger: Option<String>,
}

impl Keybind {
    /// The key a key press names, unless it has no scan code.
    pub fn recorded(event: &KeyEvent) -> Option<Self> {
        let scancode = event.physical_key.to_scancode()?;
        Some(Self {
            scancode,
            name: name(event),
            trigger: trigger(event),
        })
    }
}
/// Ctrl, Alt, Shift, or Super bits for a command chord.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ModifierMask(pub u8);

impl ModifierMask {
    /// Exact Control modifier.
    pub const CTRL: Self = Self(1 << 0);
    /// Exact Alt modifier.
    pub const ALT: Self = Self(1 << 1);
    /// Exact Shift modifier.
    pub const SHIFT: Self = Self(1 << 2);
    /// Exact Super/Logo modifier.
    pub const SUPER: Self = Self(1 << 3);
    /// Capture the four supported modifiers from a winit event.
    pub fn from_winit(modifiers: slint::winit_030::winit::keyboard::ModifiersState) -> Self {
        let mut mask = Self::default();
        if modifiers.control_key() {
            mask |= Self::CTRL;
        }
        if modifiers.alt_key() {
            mask |= Self::ALT;
        }
        if modifiers.shift_key() {
            mask |= Self::SHIFT;
        }
        if modifiers.super_key() {
            mask |= Self::SUPER;
        }
        mask
    }

    /// The serialized modifier bit set.
    pub const fn bits(self) -> u8 {
        self.0
    }

    pub(super) const fn from_index(index: usize) -> Self {
        Self(1 << index)
    }
}

impl std::ops::BitOr for ModifierMask {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

impl std::ops::BitOrAssign for ModifierMask {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

/// A command key plus the exact modifier set that must be down with it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Chord {
    /// Physical primary key for the chord.
    pub key: Keybind,
    /// The exact modifiers required with `key`.
    #[serde(default)]
    pub modifiers: ModifierMask,
}
impl Chord {
    /// Record a physical key and the modifiers held when it was pressed.
    pub fn recorded(event: &KeyEvent, modifiers: ModifierMask) -> Option<Self> {
        if matches!(
            unmodified(event),
            Key::Named(
                NamedKey::Control
                    | NamedKey::Alt
                    | NamedKey::Shift
                    | NamedKey::Super
                    | NamedKey::Meta
            )
        ) {
            return None;
        }
        Some(Self {
            key: Keybind::recorded(event)?,
            modifiers,
        })
    }

    /// Format the chord as `Ctrl+Shift+V` for settings.
    pub fn name(&self) -> String {
        let mut name = String::new();
        for (modifier, label) in [
            (ModifierMask::CTRL, "Ctrl"),
            (ModifierMask::ALT, "Alt"),
            (ModifierMask::SHIFT, "Shift"),
            (ModifierMask::SUPER, "Super"),
        ] {
            if self.modifiers.bits() & modifier.bits() != 0 {
                if !name.is_empty() {
                    name.push('+');
                }
                name.push_str(label);
            }
        }
        if !name.is_empty() {
            name.push('+');
        }
        name.push_str(&self.key.name);
        name
    }

    /// Reject malformed saved bindings and system-navigation chords.
    pub fn is_valid(&self) -> bool {
        let layout = if cfg!(windows) {
            matcher::ModifierLayout::WindowsSet1
        } else {
            matcher::ModifierLayout::Evdev
        };
        let key = self.key.scancode;
        let bits = self.modifiers.bits();
        let alt = bits & ModifierMask::ALT.bits() != 0;
        let control = bits & ModifierMask::CTRL.bits() != 0;
        let logo = bits & ModifierMask::SUPER.bits() != 0;
        let delete = if cfg!(windows) { 0xe053 } else { 111 };
        bits & !15 == 0
            && key != 0
            && layout.slot(key).is_some()
            && matcher::modifier_index(layout, key).is_none()
            && !(alt && matches!(key, 15 | 62))
            && !(logo && key == 38)
            && !(control && alt && key == delete)
    }

    /// PTT fires for its key even when other modifiers are held.
    pub fn overlaps_talk(&self, talk: &Keybind) -> bool {
        if self.key.scancode == talk.scancode {
            return true;
        }
        let layout = if cfg!(windows) {
            matcher::ModifierLayout::WindowsSet1
        } else {
            matcher::ModifierLayout::Evdev
        };
        matcher::modifier_index(layout, talk.scancode).is_some_and(|index| {
            self.modifiers.bits() & ModifierMask::from_index(index).bits() != 0
        })
    }
}

/// The bindings watched together by one native input listener.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bindings {
    /// Optional single-key push-to-talk binding.
    pub talk: Option<Keybind>,
    /// Optional global stream-toggle command chord.
    pub toggle_stream: Option<Chord>,
    /// Optional global leave-voice command chord.
    pub leave_voice: Option<Chord>,
}

/// Which stored binding a status update describes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Binding {
    /// Push-to-talk.
    Talk,
    /// Toggle the game stream.
    ToggleStream,
    /// Leave the voice channel.
    LeaveVoice,
}

impl Binding {
    const fn index(self) -> usize {
        match self {
            Self::Talk => 0,
            Self::ToggleStream => 1,
            Self::LeaveVoice => 2,
        }
    }

    fn configured(self, bindings: &Bindings) -> bool {
        match self {
            Self::Talk => bindings.talk.is_some(),
            Self::ToggleStream => bindings.toggle_stream.is_some(),
            Self::LeaveVoice => bindings.leave_voice.is_some(),
        }
    }
}

/// A command edge emitted by a global chord press.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    /// Toggle the current game's stream.
    ToggleStream,
    /// Leave the voice channel.
    LeaveVoice,
}

#[derive(Clone)]
pub(super) struct Actions {
    callback: Arc<dyn Fn(Command) + Send + Sync>,
    enabled: Arc<AtomicBool>,
}

impl Actions {
    fn new(command: impl Fn(Command) + Send + Sync + 'static) -> Self {
        Self {
            callback: Arc::new(command),
            enabled: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(super) fn emit(&self, command: Command) {
        if self.enabled.load(Ordering::Acquire) {
            (self.callback)(command);
        }
    }

    fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Release);
    }
}

type StatusCallback = Arc<dyn Fn(Binding, Reach) + Send + Sync>;

struct StatusState {
    reach: [Reach; 3],
    #[cfg(target_os = "linux")]
    configuration_available: bool,
}

#[derive(Clone)]
pub(super) struct StatusReporter {
    values: Arc<Mutex<StatusState>>,
    callback: StatusCallback,
    talk_scancode: Option<u32>,
}

impl StatusReporter {
    fn new(bindings: &Bindings, told: impl Fn(Binding, Reach) + Send + Sync + 'static) -> Self {
        Self {
            values: Arc::new(Mutex::new(StatusState {
                reach: [
                    Reach::NotConfigured,
                    Reach::NotConfigured,
                    Reach::NotConfigured,
                ],
                #[cfg(target_os = "linux")]
                configuration_available: false,
            })),
            callback: Arc::new(told),
            talk_scancode: bindings.talk.as_ref().map(|key| key.scancode),
        }
    }

    pub(super) fn asking(&self, bindings: &Bindings) {
        for binding in [Binding::Talk, Binding::ToggleStream, Binding::LeaveVoice] {
            if binding.configured(bindings) {
                self.set(binding, Reach::Asking);
            }
        }
    }

    pub(super) fn set(&self, binding: Binding, reach: Reach) {
        let changed = {
            let mut values = self
                .values
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let value = &mut values.reach[binding.index()];
            if *value == reach {
                false
            } else {
                *value = reach.clone();
                true
            }
        };
        if binding == Binding::Talk {
            let scancode = if matches!(&reach, Reach::Focused(_)) {
                self.talk_scancode
            } else {
                None
            };
            set_focused_scancode(scancode);
        }
        if changed {
            (self.callback)(binding, reach);
        }
    }

    fn get(&self, binding: Binding) -> Reach {
        self.values
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .reach[binding.index()]
        .clone()
    }

    #[cfg(target_os = "linux")]
    pub(super) fn set_configuration_available(&self, available: bool) {
        self.values
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .configuration_available = available;
    }
}

#[path = "push_to_talk/matcher.rs"]
mod matcher;

#[cfg(target_os = "linux")]
#[path = "push_to_talk/linux.rs"]
mod linux;
#[cfg(target_os = "linux")]
pub(crate) fn wayland_session() -> bool {
    linux::is_wayland_session()
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn wayland_session() -> bool {
    false
}

#[cfg(windows)]
#[path = "push_to_talk/windows.rs"]
mod windows;

#[cfg(windows)]
pub fn windows_custom_application_handler()
-> impl slint::winit_030::CustomApplicationHandler + 'static {
    windows::custom_application_handler()
}

#[cfg(windows)]
/// Restores Winit's original keyboard registration and removes resume hooks.
/// Call before the UI event loop returns; retry if Windows reports `false`.

pub fn windows_teardown() -> bool {
    windows::teardown()
}

/// The key without Shift or Caps Lock, so a letter reads as itself.
fn unmodified(event: &KeyEvent) -> Key {
    #[cfg(any(target_os = "linux", windows))]
    {
        use slint::winit_030::winit::platform::modifier_supplement::KeyEventExtModifierSupplement;
        event.key_without_modifiers()
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        event.logical_key.clone()
    }
}

fn name(event: &KeyEvent) -> String {
    let side = match event.location {
        KeyLocation::Left => "Left ",
        KeyLocation::Right => "Right ",
        KeyLocation::Numpad => "Numpad ",
        KeyLocation::Standard => "",
    };
    match unmodified(event) {
        Key::Character(text) => format!("{side}{}", text.to_uppercase()),
        Key::Named(named) => {
            let named = match named {
                NamedKey::Control => "Ctrl".to_owned(),
                NamedKey::Super | NamedKey::Meta => "Super".to_owned(),
                NamedKey::ArrowUp => "Up".to_owned(),
                NamedKey::ArrowDown => "Down".to_owned(),
                NamedKey::ArrowLeft => "Left".to_owned(),
                NamedKey::ArrowRight => "Right".to_owned(),
                NamedKey::Space => "Space".to_owned(),
                named => format!("{named:?}"),
            };
            format!("{side}{named}")
        }
        _ => match event.physical_key {
            PhysicalKey::Code(code) => format!("{code:?}"),
            PhysicalKey::Unidentified(_) => "Unknown key".to_owned(),
        },
    }
}

/// The key's keysym name, as the XDG shortcuts specification writes a
/// trigger; on Linux, where only the portal needs it.
#[cfg(target_os = "linux")]
fn trigger(event: &KeyEvent) -> Option<String> {
    let right = event.location == KeyLocation::Right;
    let side =
        |left: &str, right_name: &str| Some(if right { right_name } else { left }.to_owned());
    match unmodified(event) {
        Key::Character(text) => {
            let mut chars = text.chars();
            let (Some(char), None) = (chars.next(), chars.next()) else {
                return None;
            };
            let name = xkeysym::Keysym::from_char(char).name()?;
            Some(name.strip_prefix("XK_").unwrap_or(name).to_owned())
        }
        Key::Named(named) => match named {
            NamedKey::Control => side("Control_L", "Control_R"),
            NamedKey::Shift => side("Shift_L", "Shift_R"),
            NamedKey::Alt => side("Alt_L", "Alt_R"),
            NamedKey::Super | NamedKey::Meta => side("Super_L", "Super_R"),
            NamedKey::AltGraph => Some("ISO_Level3_Shift".to_owned()),
            NamedKey::CapsLock => Some("Caps_Lock".to_owned()),
            NamedKey::ScrollLock => Some("Scroll_Lock".to_owned()),
            NamedKey::NumLock => Some("Num_Lock".to_owned()),
            NamedKey::PageUp => Some("Prior".to_owned()),
            NamedKey::PageDown => Some("Next".to_owned()),
            NamedKey::Space => Some("space".to_owned()),
            NamedKey::Enter => Some("Return".to_owned()),
            NamedKey::Backspace => Some("BackSpace".to_owned()),
            NamedKey::ArrowUp => Some("Up".to_owned()),
            NamedKey::ArrowDown => Some("Down".to_owned()),
            NamedKey::ArrowLeft => Some("Left".to_owned()),
            NamedKey::ArrowRight => Some("Right".to_owned()),
            NamedKey::Tab
            | NamedKey::Insert
            | NamedKey::Delete
            | NamedKey::Home
            | NamedKey::End
            | NamedKey::Pause => Some(format!("{named:?}")),
            NamedKey::ContextMenu => Some("Menu".to_owned()),
            named => {
                let name = format!("{named:?}");
                // F1 to F35 are their own keysyms.
                (name.starts_with('F') && name[1..].parse::<u8>().is_ok()).then_some(name)
            }
        },
        _ => None,
    }
}

#[cfg(not(target_os = "linux"))]
fn trigger(_event: &KeyEvent) -> Option<String> {
    None
}

/// Where a binding works.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reach {
    NotConfigured,
    Asking,
    Everywhere(Option<String>),
    Focused(&'static str),
    Unavailable(&'static str),
}

impl Reach {
    pub fn note(&self) -> String {
        match self {
            Reach::NotConfigured => "No shortcut is set.".into(),
            Reach::Asking => {
                "Asking your desktop to let Yapfast use this shortcut globally...".into()
            }
            Reach::Everywhere(None) => "Available in every app.".into(),
            Reach::Everywhere(Some(shortcut)) if shortcut.is_empty() => {
                "Your desktop accepted this shortcut but didn't report its key.".into()
            }
            Reach::Everywhere(Some(shortcut)) => {
                format!("Your desktop assigned {shortcut} for this shortcut.")
            }
            Reach::Focused(why) => {
                format!("{why}, so push to talk works only while Yapfast's window is in front.")
            }
            Reach::Unavailable(why) => why.to_string(),
        }
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct HoldSources {
    global: bool,
    focused: bool,
    focused_scancode: Option<u32>,
}

static HOLD_SOURCES: Mutex<HoldSources> = Mutex::new(HoldSources {
    global: false,
    focused: false,
    focused_scancode: None,
});

fn change_hold_sources(change: impl FnOnce(&mut HoldSources)) {
    let mut sources = HOLD_SOURCES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let previous = *sources;
    change(&mut sources);
    if *sources != previous {
        yapfast_core::push_to_talk::hold(sources.global || sources.focused);
    }
}

pub(super) fn global_hold(held: bool) {
    change_hold_sources(|sources| sources.global = held);
}

fn set_focused_scancode(scancode: Option<u32>) {
    change_hold_sources(|sources| {
        if sources.focused_scancode != scancode {
            sources.focused = false;
            sources.focused_scancode = scancode;
        }
    });
}

fn clear_focused_hold() {
    change_hold_sources(|sources| sources.focused = false);
}

// Winit 0.30.13 keeps `KeyEvent`'s fields private, so tests drive this
// production keyboard handler directly instead of constructing that event.
pub(super) fn focused_key_event(scancode: Option<u32>, state: ElementState, repeat: bool) {
    if repeat {
        return;
    }
    let Some(scancode) = scancode else {
        return;
    };
    change_hold_sources(|sources| {
        if sources.focused_scancode == Some(scancode) {
            sources.focused = state == ElementState::Pressed;
        }
    });
}

pub(super) fn reset_hold_sources() {
    let mut sources = HOLD_SOURCES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *sources = HoldSources::default();
    yapfast_core::push_to_talk::hold(false);
}

/// PTT-only fallback while the global listener cannot watch the key.
pub fn window_event(event: &WindowEvent) {
    match event {
        WindowEvent::KeyboardInput { event, .. } => {
            focused_key_event(event.physical_key.to_scancode(), event.state, event.repeat)
        }
        WindowEvent::Focused(false) => clear_focused_hold(),
        _ => {}
    }
}

/// Watches PTT and both command chords through a single global input source.
pub struct Watcher {
    bindings: Bindings,
    statuses: StatusReporter,
    actions: Actions,
    #[cfg(target_os = "linux")]
    _global: Option<linux::Global>,
    #[cfg(target_os = "linux")]
    configure: Option<linux::Configure>,
    #[cfg(windows)]
    _global: Option<windows::Global>,
}

impl Watcher {
    pub fn start(
        bindings: Bindings,
        runtime: &BackgroundRuntime,
        command: impl Fn(Command) + Send + Sync + 'static,
        told: impl Fn(Binding, Reach) + Send + Sync + 'static,
    ) -> Self {
        reset_hold_sources();
        let statuses = StatusReporter::new(&bindings, told);
        statuses.asking(&bindings);
        let actions = Actions::new(command);
        #[cfg(target_os = "linux")]
        let (global, configure) =
            linux::Global::start(&bindings, runtime, statuses.clone(), actions.clone());
        #[cfg(windows)]
        let _ = runtime;

        #[cfg(windows)]
        let global = windows::Global::start(&bindings, statuses.clone(), actions.clone());
        #[cfg(not(any(target_os = "linux", windows)))]
        {
            let _ = runtime;
            for binding in [Binding::Talk, Binding::ToggleStream, Binding::LeaveVoice] {
                if binding.configured(&bindings) {
                    statuses.set(
                        binding,
                        if binding == Binding::Talk {
                            Reach::Focused(
                                "Yapfast can't watch keys in other apps on this system yet",
                            )
                        } else {
                            Reach::Unavailable(
                                "Yapfast can't watch shortcuts in other apps on this system yet",
                            )
                        },
                    );
                }
            }
        }
        Self {
            bindings,
            statuses,
            actions,
            #[cfg(target_os = "linux")]
            _global: global,
            #[cfg(target_os = "linux")]
            configure,
            #[cfg(windows)]
            _global: Some(global),
        }
    }

    pub fn bindings(&self) -> &Bindings {
        &self.bindings
    }

    pub fn status(&self, binding: Binding) -> Reach {
        self.statuses.get(binding)
    }

    /// Enables or suppresses command delivery. PTT remains independently
    /// active; commands start disabled until the caller establishes ownership.
    pub fn set_commands_enabled(&self, enabled: bool) {
        self.actions.set_enabled(enabled);
    }

    /// Queues the portal v2 shortcut editor when the live session supports it;
    /// `true` means queued, not that the portal opened it.
    pub fn configure_shortcuts(&self) -> bool {
        #[cfg(target_os = "linux")]
        if let Some(configure) = &self.configure {
            let available = self
                .statuses
                .values
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .configuration_available;
            return available && configure.send(()).is_ok();
        }
        false
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        #[cfg(any(target_os = "linux", windows))]
        drop(self._global.take());
        reset_hold_sources();
    }
}
