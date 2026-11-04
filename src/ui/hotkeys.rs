//! Global command bindings live with the application, not its disposable window.

use std::fs::{File, OpenOptions};

use slint::winit_030::winit::{
    event::{ElementState, WindowEvent},
    keyboard::{Key, ModifiersState, NamedKey},
    platform::scancode::PhysicalKeyExtScancode,
};

use super::*;
use crate::push_to_talk::{Binding, Bindings, Chord, ModifierMask, Watcher};

#[derive(Default)]
pub(super) struct Keys {
    watcher: Option<Watcher>,
    generation: u64,
    recording: bool,
    enabled: bool,
    owner: Option<File>,
    lock_path: Option<PathBuf>,
    retry: slint::Timer,
    notice: Option<&'static str>,
    rejected: Option<(Binding, &'static str)>,
    invalid: [Option<&'static str>; 2],
}

pub(super) fn bind(window: &YapfastWindow, adapter: &Rc<RefCell<Adapter>>) {
    let state = adapter.clone();
    let weak = window.as_weak();
    window.on_record_hotkey(move |which| {
        let Some(window) = weak.upgrade() else { return };
        let mut adapter = state.borrow_mut();
        window.set_talk_recording(false);
        window.set_hotkey_recording(which.clamp(0, 2));
        adapter.hotkeys.rejected = None;
        adapter.recording_hotkey(which != 0);
        adapter.show_hotkeys(&window);
    });
    let state = adapter.clone();
    let weak = window.as_weak();
    window.on_clear_hotkey(move |which| {
        let Some(window) = weak.upgrade() else { return };
        let binding = if which == 1 {
            Binding::ToggleStream
        } else {
            Binding::LeaveVoice
        };
        window.set_hotkey_recording(0);
        window.set_talk_recording(false);
        let mut adapter = state.borrow_mut();
        adapter.recording_hotkey(false);
        adapter.set_command_key(&window, binding, None);
    });
    let state = adapter.clone();
    let weak = window.as_weak();
    window.on_configure_hotkeys(move || {
        let Some(window) = weak.upgrade() else { return };
        let mut adapter = state.borrow_mut();
        let queued = adapter.hotkeys.watcher.as_ref().is_some_and(Watcher::configure_shortcuts);
        if !queued {
            adapter.hotkeys.rejected = Some((Binding::ToggleStream,
                "This desktop has no live shortcut editor. Record a preferred keybind or use the desktop's settings."));
            adapter.show_hotkeys(&window);
        }
    });
}

impl Adapter {
    pub(super) fn watch_hotkeys(&mut self) {
        let mut wanted = Bindings {
            talk: self
                .layout
                .push_to_talk_key
                .clone()
                .filter(|_| self.layout.input_mode == InputMode::PushToTalk),
            toggle_stream: self.layout.toggle_stream_key.clone(),
            leave_voice: self.layout.leave_voice_key.clone(),
        };
        self.hotkeys.invalid = validate_commands(&mut wanted);
        if self.hotkeys.watcher.as_ref().map(Watcher::bindings) != Some(&wanted) {
            self.hotkeys.watcher = None;
            self.hotkeys.enabled = false;
            self.hotkeys.generation = self.hotkeys.generation.wrapping_add(1);
            let generation = self.hotkeys.generation;
            if wanted != Bindings::default() {
                self.hotkeys.watcher = Some(Watcher::start(
                    wanted,
                    &self.runtime,
                    move |command| {
                        let _ = slint::invoke_from_event_loop(move || {
                            shell::hotkey(command, generation)
                        });
                    },
                    move |_, _| {
                        let _ =
                            slint::invoke_from_event_loop(move || shell::hotkey_status(generation));
                    },
                ));
            }
        }
        self.update_hotkey_owner();
    }

    pub(super) fn recording_hotkey(&mut self, recording: bool) {
        self.hotkeys.recording = recording;
        self.update_hotkey_owner();
    }

    pub(super) fn hotkey_current(&self, generation: u64) -> bool {
        self.hotkeys.generation == generation && self.hotkeys.enabled
    }

    pub(super) fn hotkey_generation(&self) -> u64 {
        self.hotkeys.generation
    }

    /// A second in-call instance must not execute the same global command.
    /// Lock ownership is independent of --data-dir and ends before relaunch.
    pub(super) fn update_hotkey_owner(&mut self) {
        let eligible = !self.app.state.preview
            && self.app.state.voice.active()
            && self.app.state.voice.phase != Phase::Leaving
            && self.hotkeys.watcher.as_ref().is_some_and(|watcher| {
                watcher.bindings().toggle_stream.is_some()
                    || watcher.bindings().leave_voice.is_some()
            });
        let previous_notice = self.hotkeys.notice;
        if !eligible {
            self.hotkeys.owner = None;
            self.hotkeys.retry.stop();
            self.hotkeys.notice = None;
        } else if self.hotkeys.owner.is_none() && !self.hotkeys.retry.running() {
            match self.claim_hotkeys() {
                Ok(file) => {
                    self.hotkeys.owner = Some(file);
                    self.hotkeys.notice = None;
                    self.hotkeys.retry.stop();
                }
                Err(std::fs::TryLockError::WouldBlock) => {
                    self.hotkeys.notice = Some(
                        "Global commands are controlled by another Yapfast instance in a call.",
                    );
                }
                Err(std::fs::TryLockError::Error(_)) => {
                    self.hotkeys.notice = Some(
                        "Yapfast couldn't claim global commands. Check access to its data directory.",
                    );
                }
            }
            if self.hotkeys.owner.is_none() && !self.hotkeys.retry.running() {
                let weak = self.weak.clone();
                self.hotkeys.retry.start(
                    slint::TimerMode::Repeated,
                    Duration::from_secs(1),
                    move || {
                        if let Some(adapter) = weak.upgrade()
                            && let Ok(mut adapter) = adapter.try_borrow_mut()
                        {
                            adapter.hotkeys.retry.stop();
                            adapter.update_hotkey_owner();
                        }
                    },
                );
            }
        }
        let enabled = eligible && self.hotkeys.owner.is_some() && !self.hotkeys.recording;
        let changed = self.hotkeys.enabled != enabled || previous_notice != self.hotkeys.notice;
        if self.hotkeys.enabled != enabled {
            self.hotkeys.enabled = enabled;
            if let Some(watcher) = &self.hotkeys.watcher {
                watcher.set_commands_enabled(enabled);
            }
        }
        if changed && let Some(window) = self.window.upgrade() {
            self.show_hotkeys(&window);
        }
    }

    fn claim_hotkeys(&mut self) -> std::result::Result<File, std::fs::TryLockError> {
        if self.hotkeys.lock_path.is_none() {
            let directory = yapfast_core::default_directory().map_err(|_| {
                std::fs::TryLockError::Error(std::io::Error::other("no per-user data directory"))
            })?;
            std::fs::create_dir_all(&directory).map_err(std::fs::TryLockError::Error)?;
            let identity = lock_identity().map_err(std::fs::TryLockError::Error)?;
            let hash = stable_lock_hash(&identity);
            self.hotkeys.lock_path =
                Some(directory.join(format!("global-hotkeys-{hash:016x}.lock")));
        }
        let path = self.hotkeys.lock_path.as_ref().unwrap();
        // No contents are read or overwritten; the OS releases this lock on exit.
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)
            .map_err(std::fs::TryLockError::Error)?;
        file.try_lock()?;
        Ok(file)
    }

    pub(super) fn stop_hotkeys(&mut self) {
        self.hotkeys.retry.stop();
        if let Some(watcher) = self.hotkeys.watcher.take() {
            watcher.set_commands_enabled(false);
        }
        self.hotkeys.owner = None;
        self.hotkeys.enabled = false;
        self.hotkeys.generation = self.hotkeys.generation.wrapping_add(1);
    }

    pub(super) fn show_hotkeys(&self, window: &YapfastWindow) {
        window.set_toggle_stream_key(
            self.layout
                .toggle_stream_key
                .as_ref()
                .map(Chord::name)
                .unwrap_or_default()
                .into(),
        );
        window.set_leave_voice_key(
            self.layout
                .leave_voice_key
                .as_ref()
                .map(Chord::name)
                .unwrap_or_default()
                .into(),
        );
        let status = |binding| {
            let mut note = self
                .hotkeys
                .watcher
                .as_ref()
                .map(|watcher| watcher.status(binding).note())
                .unwrap_or_else(|| "No shortcut is set.".into());
            if binding != Binding::Talk {
                let index = if binding == Binding::ToggleStream {
                    0
                } else {
                    1
                };
                if let Some(why) = self.hotkeys.invalid[index] {
                    note = why.into();
                }
                if let Some(why) = self.hotkeys.notice {
                    note = why.into();
                }
                if let Some((rejected, why)) = self.hotkeys.rejected
                    && rejected == binding
                {
                    note = why.into();
                }
            }
            note
        };
        window.set_talk_note(status(Binding::Talk).into());
        let mut stream = status(Binding::ToggleStream);
        #[cfg(feature = "discord")]
        if self.layout.toggle_stream_key.is_some()
            && let Err(why) = yapfast_core::stream_worker::game_capture()
        {
            stream.push(' ');
            stream.push_str(why);
            stream.push_str(" Stopping an existing stream still works.");
        }
        #[cfg(not(feature = "discord"))]
        if self.layout.toggle_stream_key.is_some() {
            stream.push_str(" Game streaming is available only in Discord calls.");
        }
        window.set_toggle_stream_note(stream.into());
        window.set_leave_voice_note(status(Binding::LeaveVoice).into());
        window.set_hotkeys_desktop(
            cfg!(target_os = "linux") && crate::push_to_talk::wayland_session(),
        );
    }

    fn set_command_key(&mut self, window: &YapfastWindow, binding: Binding, chord: Option<Chord>) {
        if binding == Binding::Talk {
            return;
        }
        let conflict = self.store_command_key(binding, chord);
        self.hotkeys.rejected = conflict.map(|why| (binding, why));
        if conflict.is_none() {
            self.watch_hotkeys();
        }
        self.show_hotkeys(window);
    }

    fn store_command_key(
        &mut self,
        binding: Binding,
        chord: Option<Chord>,
    ) -> Option<&'static str> {
        let other = match binding {
            Binding::ToggleStream => self.layout.leave_voice_key.as_ref(),
            Binding::LeaveVoice => self.layout.toggle_stream_key.as_ref(),
            Binding::Talk => return None,
        };
        let conflict = chord.as_ref().and_then(|chord| {
            if !chord.is_valid() {
                Some("That shortcut is invalid or reserved for system navigation.")
            } else if other.is_some_and(|other| same_chord(chord, other)) {
                Some("That shortcut already controls the other voice action.")
            } else if self.layout.input_mode == InputMode::PushToTalk
                && self
                    .layout
                    .push_to_talk_key
                    .as_ref()
                    .is_some_and(|talk| chord.overlaps_talk(talk))
            {
                Some("That shortcut overlaps the push-to-talk key. Choose a different key or modifiers.")
            } else {
                None
            }
        });
        if conflict.is_some() {
            return conflict;
        }
        match binding {
            Binding::ToggleStream => self.layout.toggle_stream_key = chord,
            Binding::LeaveVoice => self.layout.leave_voice_key = chord,
            Binding::Talk => return None,
        }
        self.layout.write(&self.root);
        None
    }
}

fn same_chord(a: &Chord, b: &Chord) -> bool {
    a.key.scancode == b.key.scancode && a.modifiers == b.modifiers
}

fn validate_commands(bindings: &mut Bindings) -> [Option<&'static str>; 2] {
    let duplicate = bindings
        .toggle_stream
        .as_ref()
        .zip(bindings.leave_voice.as_ref())
        .is_some_and(|(a, b)| same_chord(a, b));
    let mut invalid = [None; 2];
    for (index, slot) in [&mut bindings.toggle_stream, &mut bindings.leave_voice]
        .into_iter()
        .enumerate()
    {
        let Some(chord) = slot.as_ref() else { continue };
        let why = if !chord.is_valid() {
            Some("This saved shortcut is invalid or reserved. Record another keybind.")
        } else if duplicate {
            Some("Both voice actions have the same shortcut. Record different keybinds.")
        } else if bindings
            .talk
            .as_ref()
            .is_some_and(|talk| chord.overlaps_talk(talk))
        {
            Some("This shortcut overlaps active push to talk. Choose another keybind.")
        } else {
            None
        };
        if why.is_some() {
            invalid[index] = why;
            *slot = None;
        }
    }
    invalid
}

/// A window-local recorder; commands keep using native input, never this path.
#[derive(Default)]
pub(super) struct Recorder {
    modifiers: ModifiersState,
    pending: Option<(Binding, Chord)>,
}

impl Recorder {
    pub(super) fn event(
        &mut self,
        event: &WindowEvent,
        window: &YapfastWindow,
        adapter: &std::rc::Weak<RefCell<Adapter>>,
    ) -> bool {
        if self.pending.is_some() && window.get_hotkey_recording() == 0 {
            self.pending = None;
        }
        match event {
            WindowEvent::ModifiersChanged(modifiers) => self.modifiers = modifiers.state(),
            WindowEvent::Focused(false) => {
                self.modifiers = ModifiersState::empty();
                self.pending = None;
                window.set_hotkey_recording(0);
                window.set_talk_recording(false);
                let adapter = adapter.clone();
                slint::Timer::single_shot(Duration::ZERO, move || {
                    if let Some(adapter) = adapter.upgrade() {
                        adapter.borrow_mut().recording_hotkey(false);
                    }
                });
            }
            WindowEvent::KeyboardInput { event: key, .. } if window.get_hotkey_recording() != 0 => {
                if key.state == ElementState::Pressed {
                    if !key.repeat {
                        if key.logical_key == Key::Named(NamedKey::Escape) {
                            self.cancel(window, adapter);
                        } else if !system_navigation(&key.logical_key, self.modifiers)
                            && let Some(chord) =
                                Chord::recorded(key, ModifierMask::from_winit(self.modifiers))
                        {
                            let binding = if window.get_hotkey_recording() == 1 {
                                Binding::ToggleStream
                            } else {
                                Binding::LeaveVoice
                            };
                            self.pending = Some((binding, chord));
                        }
                    }
                } else if self.pending.as_ref().is_some_and(|(_, chord)| {
                    key.physical_key.to_scancode() == Some(chord.key.scancode)
                }) {
                    let (binding, chord) = self.pending.take().unwrap();
                    self.cancel(window, adapter);
                    let adapter = adapter.clone();
                    let weak = window.as_weak();
                    slint::Timer::single_shot(Duration::ZERO, move || {
                        if let (Some(adapter), Some(window)) = (adapter.upgrade(), weak.upgrade()) {
                            adapter
                                .borrow_mut()
                                .set_command_key(&window, binding, Some(chord));
                        }
                    });
                }
                return true;
            }
            _ => {}
        }
        false
    }

    fn cancel(&mut self, window: &YapfastWindow, adapter: &std::rc::Weak<RefCell<Adapter>>) {
        self.pending = None;
        window.set_hotkey_recording(0);
        let adapter = adapter.clone();
        slint::Timer::single_shot(Duration::ZERO, move || {
            if let Some(adapter) = adapter.upgrade() {
                adapter.borrow_mut().recording_hotkey(false);
            }
        });
    }
}

fn system_navigation(key: &Key, modifiers: ModifiersState) -> bool {
    (modifiers.alt_key() && matches!(key, Key::Named(NamedKey::Tab | NamedKey::F4)))
        || (modifiers.super_key()
            && matches!(key, Key::Character(text) if text.eq_ignore_ascii_case("l")))
        || (modifiers.control_key() && modifiers.alt_key() && *key == Key::Named(NamedKey::Delete))
}
fn stable_lock_hash(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
    })
}

#[cfg(target_os = "linux")]
fn lock_identity() -> std::io::Result<Vec<u8>> {
    Ok(linux_lock_identity(
        crate::push_to_talk::wayland_session(),
        std::env::var_os("XDG_RUNTIME_DIR").as_deref(),
        std::env::var_os("WAYLAND_DISPLAY").as_deref(),
        std::env::var_os("DISPLAY").as_deref(),
        std::env::var_os("XDG_SESSION_ID").as_deref(),
    ))
}

#[cfg(target_os = "linux")]
fn linux_lock_identity(
    wayland: bool,
    runtime: Option<&std::ffi::OsStr>,
    wayland_display: Option<&std::ffi::OsStr>,
    x11_display: Option<&std::ffi::OsStr>,
    session: Option<&std::ffi::OsStr>,
) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;

    let mut identity = Vec::with_capacity(64);
    if wayland {
        identity.extend_from_slice(b"wayland\0");
        if let Some(runtime) = runtime {
            let runtime = std::path::PathBuf::from(runtime);
            let runtime = std::fs::canonicalize(&runtime).unwrap_or(runtime);
            identity.extend_from_slice(runtime.as_os_str().as_bytes());
        } else if let Some(display) = wayland_display {
            let display = std::path::PathBuf::from(display);
            let display = if display.is_absolute() {
                std::fs::canonicalize(&display).unwrap_or(display)
            } else {
                display
            };
            identity.extend_from_slice(display.as_os_str().as_bytes());
        } else if let Some(session) = session {
            identity.extend_from_slice(session.as_bytes());
        } else {
            identity.extend_from_slice(b"session");
        }
    } else if let Some(display) = x11_display {
        identity.extend_from_slice(b"x11\0");
        identity.extend_from_slice(&x11_display_identity(display));
    } else if let Some(session) = session {
        identity.extend_from_slice(b"session\0");
        identity.extend_from_slice(session.as_bytes());
    } else {
        identity.extend_from_slice(b"no-display");
    }
    identity
}

#[cfg(target_os = "linux")]
fn x11_display_identity(display: &std::ffi::OsStr) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;

    let raw = display.as_bytes();
    let mut normalized = if let Some(local) = raw.strip_prefix(b"unix:") {
        let mut normalized = Vec::with_capacity(local.len() + 1);
        normalized.push(b':');
        normalized.extend_from_slice(local);
        normalized
    } else {
        raw.to_vec()
    };
    if let Some(colon) = normalized.iter().rposition(|byte| *byte == b':')
        && let Some(dot) = normalized[colon + 1..]
            .iter()
            .position(|byte| *byte == b'.')
        && normalized[colon + dot + 2..].iter().all(u8::is_ascii_digit)
    {
        normalized.truncate(colon + dot + 1);
    }
    normalized
}

#[cfg(windows)]
fn lock_identity() -> std::io::Result<Vec<u8>> {
    use windows_sys::Win32::System::{
        RemoteDesktop::ProcessIdToSessionId, Threading::GetCurrentProcessId,
    };

    let mut id = 0;
    // SAFETY: The output pointer is valid; query only this process's session.
    if unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut id) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut identity = b"windows\0".to_vec();
    identity.extend_from_slice(&id.to_le_bytes());
    Ok(identity)
}

#[cfg(not(any(target_os = "linux", windows)))]
fn lock_identity() -> std::io::Result<Vec<u8>> {
    Ok(b"unsupported".to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(target_os = "linux")]
    #[test]
    fn wayland_and_xwayland_environment_variants_share_the_session_lock() {
        use std::ffi::OsStr;

        let runtime = OsStr::new("/run/user/1000");
        let wayland = super::linux_lock_identity(
            true,
            Some(runtime),
            Some(OsStr::new("wayland-0")),
            None,
            Some(OsStr::new("2")),
        );
        let xwayland =
            super::linux_lock_identity(true, Some(runtime), None, Some(OsStr::new(":0.0")), None);
        assert_eq!(wayland, xwayland);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn x11_screen_and_local_socket_aliases_share_the_lock() {
        use std::ffi::OsStr;

        let display = |value| super::x11_display_identity(OsStr::new(value));
        assert_eq!(display(":0"), display(":0.0"));
        assert_eq!(display(":0"), display("unix:0"));
    }

    fn fixture() -> (tempfile::TempDir, YapfastWindow, Rc<RefCell<Adapter>>) {
        i_slint_backend_testing::init_no_event_loop();
        let directory = tempfile::tempdir().unwrap();
        let window = YapfastWindow::new().unwrap();
        let adapter = Adapter::new(
            Arc::new(BackgroundRuntime::new().unwrap()),
            directory.path().into(),
            &window,
        )
        .unwrap();
        bind(&window, &adapter);
        (directory, window, adapter)
    }

    fn chord(scancode: u32, modifiers: ModifierMask) -> Chord {
        Chord {
            key: crate::push_to_talk::Keybind {
                scancode,
                name: format!("key {scancode}"),
                trigger: None,
            },
            modifiers,
        }
    }

    #[test]
    fn command_bindings_persist_and_conflicts_preserve_both_keys() {
        let (directory, _, adapter) = fixture();
        let stored = chord(47, ModifierMask::CTRL | ModifierMask::SHIFT);
        let occupied = chord(45, ModifierMask::ALT);
        {
            let mut adapter = adapter.borrow_mut();
            adapter.layout.leave_voice_key = Some(occupied.clone());
            adapter.layout.write(directory.path());
            assert_eq!(
                adapter.store_command_key(Binding::ToggleStream, Some(stored.clone())),
                None
            );
        }
        let persisted = Layout::read(directory.path());
        assert_eq!(persisted.toggle_stream_key, Some(stored.clone()));
        assert_eq!(persisted.leave_voice_key, Some(occupied.clone()));

        assert_eq!(
            adapter
                .borrow_mut()
                .store_command_key(Binding::ToggleStream, Some(occupied.clone())),
            Some("That shortcut already controls the other voice action.")
        );
        let persisted = Layout::read(directory.path());
        assert_eq!(persisted.toggle_stream_key, Some(stored));
        assert_eq!(persisted.leave_voice_key, Some(occupied));
    }

    #[test]
    fn leaving_settings_cancels_an_in_progress_hotkey_recording() {
        let (directory, window, adapter) = fixture();
        window.show().unwrap();
        let original = chord(47, ModifierMask::CTRL);
        adapter.borrow_mut().layout.toggle_stream_key = Some(original.clone());
        adapter.borrow().layout.write(directory.path());

        window.set_settings_open(true);
        window.set_settings_section(1);
        window.invoke_record_hotkey(1);
        assert_eq!(window.get_hotkey_recording(), 1);
        assert!(adapter.borrow().hotkeys.recording);

        let mut recorder = Recorder {
            pending: Some((Binding::ToggleStream, chord(45, ModifierMask::ALT))),
            ..Recorder::default()
        };
        window
            .window()
            .dispatch_event(slint::platform::WindowEvent::KeyPressed {
                text: slint::platform::Key::Escape.into(),
            });

        assert!(!window.get_settings_open());
        assert_eq!(window.get_hotkey_recording(), 0);
        assert!(!adapter.borrow().hotkeys.recording);
        assert!(!recorder.event(
            &WindowEvent::RedrawRequested,
            &window,
            &Rc::downgrade(&adapter)
        ));
        assert!(recorder.pending.is_none());
        assert_eq!(
            Layout::read(directory.path()).toggle_stream_key,
            Some(original)
        );
    }

    #[test]
    fn stopping_replaced_hotkeys_invalidates_queued_commands() {
        let (_, _, adapter) = fixture();
        {
            let mut adapter = adapter.borrow_mut();
            adapter.hotkeys.generation = 41;
            adapter.hotkeys.enabled = true;
        }
        assert!(adapter.borrow().hotkey_current(41));

        adapter.borrow_mut().stop_hotkeys();
        adapter.borrow_mut().hotkeys.enabled = true;
        assert!(!adapter.borrow().hotkey_current(41));
        assert!(adapter.borrow().hotkey_current(42));
    }
}
