//! Global command bindings live with the application, not its disposable window.

use std::fs::{File, OpenOptions};
use std::hash::{Hash, Hasher};

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
        } else if self.hotkeys.owner.is_none() {
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
            let mut session = std::collections::hash_map::DefaultHasher::new();
            for key in ["XDG_SESSION_ID", "WAYLAND_DISPLAY", "DISPLAY"] {
                std::env::var_os(key).hash(&mut session);
            }
            #[cfg(windows)]
            {
                use windows_sys::Win32::System::{
                    RemoteDesktop::ProcessIdToSessionId, Threading::GetCurrentProcessId,
                };
                let mut id = 0;
                // SAFETY: the output pointer is valid; only this process's session is queried.
                if unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut id) } == 0 {
                    return Err(std::fs::TryLockError::Error(std::io::Error::last_os_error()));
                }
                id.hash(&mut session);
            }
            self.hotkeys.lock_path =
                Some(directory.join(format!("global-hotkeys-{:016x}.lock", session.finish())));
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
            cfg!(target_os = "linux")
                && std::env::var_os("WAYLAND_DISPLAY").is_some_and(|s| !s.is_empty()),
        );
    }

    fn set_command_key(&mut self, window: &YapfastWindow, binding: Binding, chord: Option<Chord>) {
        let other = match binding {
            Binding::ToggleStream => self.layout.leave_voice_key.as_ref(),
            Binding::LeaveVoice => self.layout.toggle_stream_key.as_ref(),
            Binding::Talk => return,
        };
        let conflict = chord.as_ref().and_then(|chord| {
            if !chord.is_valid() {
                Some("That shortcut is invalid or reserved for system navigation.")
            } else if other.is_some_and(|other| same_chord(chord, other)) {
                Some("That shortcut already controls the other voice action.")
            } else if self.layout.input_mode == InputMode::PushToTalk
                && self.layout.push_to_talk_key.as_ref().is_some_and(|talk| chord.overlaps_talk(talk))
            {
                Some("That shortcut overlaps the push-to-talk key. Choose a different key or modifiers.")
            } else {
                None
            }
        });
        self.hotkeys.rejected = conflict.map(|why| (binding, why));
        if conflict.is_none() {
            match binding {
                Binding::ToggleStream => self.layout.toggle_stream_key = chord,
                Binding::LeaveVoice => self.layout.leave_voice_key = chord,
                Binding::Talk => return,
            }
            self.layout.write(&self.root);
            self.watch_hotkeys();
        }
        self.show_hotkeys(window);
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
