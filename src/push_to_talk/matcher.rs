use std::{collections::HashMap, hash::Hash};

use super::{Bindings, Command, ModifierMask};

#[derive(Clone, Copy)]
pub(super) enum ModifierLayout {
    Evdev,
    WindowsSet1,
}

const KEYS: usize = 768;
const WORDS: usize = KEYS / 64;
type KeySet = [u64; WORDS];

impl ModifierLayout {
    pub(super) fn slot(self, scancode: u32) -> Option<usize> {
        let slot = match self {
            Self::Evdev => scancode as usize,
            Self::WindowsSet1 => {
                let bank = match scancode & !0xff {
                    0 => 0,
                    0xe000 => 1,
                    0xe100 => 2,
                    _ => return None,
                };
                bank * 256 + (scancode & 0xff) as usize
            }
        };
        (slot < KEYS).then_some(slot)
    }

    #[cfg(any(target_os = "windows", test))]
    fn scancode(self, slot: usize) -> u32 {
        match self {
            Self::Evdev => slot as u32,
            Self::WindowsSet1 => [0, 0xe000, 0xe100][slot / 256] | (slot % 256) as u32,
        }
    }
}

fn insert(keys: &mut KeySet, slot: usize) -> bool {
    let mask = 1 << (slot % 64);
    let word = &mut keys[slot / 64];
    let changed = *word & mask == 0;
    *word |= mask;
    changed
}

fn remove(keys: &mut KeySet, slot: usize) -> bool {
    let mask = 1 << (slot % 64);
    let word = &mut keys[slot / 64];
    let changed = *word & mask != 0;
    *word &= !mask;
    changed
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Edges {
    pub talk: Option<bool>,
    commands: [Option<Command>; 2],
    len: usize,
}

impl Edges {
    fn command(&mut self, command: Command) {
        debug_assert!(self.len < self.commands.len());
        self.commands[self.len] = Some(command);
        self.len += 1;
    }

    pub fn commands(&self) -> impl Iterator<Item = Command> + '_ {
        self.commands[..self.len]
            .iter()
            .filter_map(|command| *command)
    }
}

/// Tracks only keys currently down, grouped by their input device. Repeated
/// downs and duplicate downs from another device do not make another edge.
pub(super) struct Matcher<D> {
    bindings: Bindings,
    layout: ModifierLayout,
    devices: HashMap<D, KeySet>,
    counts: [u16; KEYS],
    seeded: KeySet,
    modifiers: [u32; 4],
    talk_held: bool,
}

impl<D: Eq + Hash> Matcher<D> {
    pub fn new(bindings: Bindings, layout: ModifierLayout) -> Self {
        Self {
            bindings,
            layout,
            devices: HashMap::new(),
            counts: [0; KEYS],
            seeded: [0; WORDS],
            modifiers: [0; 4],
            talk_held: false,
        }
    }

    pub fn event(&mut self, device: D, scancode: u32, pressed: bool) -> Edges {
        let Some(slot) = self.layout.slot(scancode) else {
            return Edges::default();
        };
        if pressed {
            if !insert(self.devices.entry(device).or_insert([0; WORDS]), slot) {
                return Edges::default();
            }
            let count = &mut self.counts[slot];
            if *count != 0 {
                *count = count.saturating_add(1);
                return Edges::default();
            }
            *count = 1;
            self.down(scancode)
        } else {
            let device_down = self
                .devices
                .get_mut(&device)
                .is_some_and(|keys| remove(keys, slot));
            let seeded_down = remove(&mut self.seeded, slot);
            if !device_down && !seeded_down {
                return Edges::default();
            }
            let count = &mut self.counts[slot];
            *count = count.saturating_sub(u16::from(device_down) + u16::from(seeded_down));
            if *count != 0 {
                return Edges::default();
            }
            self.up(scancode)
        }
    }

    /// Seeds keys already held when a source is initialized or resynchronized.
    /// They are state only: no command or PTT edge is synthesized for them.
    pub fn seed_down(&mut self, scancodes: impl IntoIterator<Item = u32>) {
        for scancode in scancodes {
            if let Some(slot) = self.layout.slot(scancode)
                && insert(&mut self.seeded, slot)
                && self.counts[slot] == 0
            {
                self.counts[slot] = 1;
                self.set_modifier(scancode, true);
            }
        }
    }

    /// Starts an input source with no keys currently down.
    #[cfg(target_os = "windows")]
    pub fn add_device(&mut self, device: D) {
        self.devices.entry(device).or_insert([0; WORDS]);
    }

    /// Drops all keys attributed to one device, emitting only genuine final
    /// releases after other keyboards' state has been accounted for.
    #[cfg(any(target_os = "windows", test))]
    pub fn remove_device(&mut self, device: &D) -> Edges {
        let Some(keys) = self.devices.remove(device) else {
            return Edges::default();
        };
        let mut edges = Edges::default();
        for (word, mut bits) in keys.into_iter().enumerate() {
            while bits != 0 {
                let bit = bits.trailing_zeros() as usize;
                bits &= bits - 1;
                let slot = word * 64 + bit;
                self.counts[slot] = self.counts[slot].saturating_sub(1);
                if self.counts[slot] == 0 {
                    self.up_into(self.layout.scancode(slot), &mut edges);
                }
            }
        }
        edges
    }

    /// Clears stale input after a disconnect, desktop change, or resume.
    pub fn clear(&mut self) -> Edges {
        let mut edges = Edges::default();
        if self.talk_held {
            self.talk_held = false;
            edges.talk = Some(false);
        }
        self.devices.clear();
        self.counts.fill(0);
        self.seeded.fill(0);
        self.modifiers = [0; 4];
        edges
    }

    fn down(&mut self, scancode: u32) -> Edges {
        self.set_modifier(scancode, true);
        let mut edges = Edges::default();
        if self
            .bindings
            .talk
            .as_ref()
            .is_some_and(|key| key.scancode == scancode)
            && !self.talk_held
        {
            self.talk_held = true;
            edges.talk = Some(true);
        }
        for (chord, command) in [
            (self.bindings.toggle_stream.as_ref(), Command::ToggleStream),
            (self.bindings.leave_voice.as_ref(), Command::LeaveVoice),
        ] {
            if chord.is_some_and(|chord| {
                chord.key.scancode == scancode && chord.modifiers == self.modifier_mask()
            }) {
                edges.command(command);
            }
        }
        edges
    }

    fn up(&mut self, scancode: u32) -> Edges {
        let mut edges = Edges::default();
        self.up_into(scancode, &mut edges);
        edges
    }

    fn up_into(&mut self, scancode: u32, edges: &mut Edges) {
        self.set_modifier(scancode, false);
        if self
            .bindings
            .talk
            .as_ref()
            .is_some_and(|key| key.scancode == scancode)
            && self.talk_held
        {
            self.talk_held = false;
            edges.talk = Some(false);
        }
    }

    fn modifier_mask(&self) -> ModifierMask {
        let mut mask = ModifierMask::default();
        for (index, count) in self.modifiers.iter().enumerate() {
            if *count != 0 {
                mask |= ModifierMask::from_index(index);
            }
        }
        mask
    }

    fn set_modifier(&mut self, scancode: u32, down: bool) {
        let Some(index) = modifier_index(self.layout, scancode) else {
            return;
        };
        if down {
            self.modifiers[index] = self.modifiers[index].saturating_add(1);
        } else {
            self.modifiers[index] = self.modifiers[index].saturating_sub(1);
        }
    }
}

pub(super) fn modifier_index(layout: ModifierLayout, scancode: u32) -> Option<usize> {
    let codes = match layout {
        ModifierLayout::Evdev => [[29, 97], [56, 100], [42, 54], [125, 126]],
        ModifierLayout::WindowsSet1 => [
            [0x001d, 0xe01d],
            [0x0038, 0xe038],
            [0x002a, 0x0036],
            [0xe05b, 0xe05c],
        ],
    };
    codes.iter().position(|pair| pair.contains(&scancode))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::push_to_talk::{Chord, Keybind};

    fn key(scancode: u32) -> Keybind {
        Keybind {
            scancode,
            name: "test".into(),
            trigger: None,
        }
    }

    fn chord(scancode: u32, modifiers: ModifierMask) -> Chord {
        Chord {
            key: key(scancode),
            modifiers,
        }
    }

    fn bindings() -> Bindings {
        Bindings {
            talk: Some(key(47)),
            toggle_stream: Some(chord(47, ModifierMask::CTRL)),
            leave_voice: Some(chord(47, ModifierMask::CTRL | ModifierMask::SHIFT)),
        }
    }

    #[test]
    fn exact_modifiers_and_repeats_produce_one_command_edge() {
        let mut matcher = Matcher::new(bindings(), ModifierLayout::Evdev);
        assert_eq!(matcher.event(1, 29, true), Edges::default());
        assert_eq!(
            matcher.event(1, 47, true).commands().collect::<Vec<_>>(),
            [Command::ToggleStream]
        );
        assert!(matcher.event(1, 47, true).commands().next().is_none());
        assert_eq!(matcher.event(1, 47, false).talk, Some(false));
        assert!(matcher.event(1, 47, false).commands().next().is_none());

        assert_eq!(matcher.event(1, 54, true), Edges::default());
        assert_eq!(
            matcher.event(1, 47, true).commands().collect::<Vec<_>>(),
            [Command::LeaveVoice]
        );
        assert!(matcher.event(1, 47, true).commands().next().is_none());
    }

    #[test]
    fn modifiers_and_primary_keys_aggregate_across_devices() {
        let mut matcher = Matcher::new(bindings(), ModifierLayout::Evdev);
        matcher.event(1, 29, true);
        matcher.event(2, 97, true);
        matcher.event(3, 54, true);
        let edges = matcher.event(4, 47, true);
        assert_eq!(edges.commands().collect::<Vec<_>>(), [Command::LeaveVoice]);
        assert_eq!(edges.talk, Some(true));
        assert_eq!(matcher.event(4, 47, false).talk, Some(false));
        assert!(matcher.event(1, 29, false).talk.is_none());
        assert!(matcher.event(2, 97, false).talk.is_none());
        assert_eq!(matcher.event(3, 54, false), Edges::default());
    }

    #[test]
    fn removing_device_releases_only_its_final_keys() {
        let mut matcher = Matcher::new(bindings(), ModifierLayout::Evdev);
        matcher.event(1, 47, true);
        matcher.event(2, 47, true);
        assert!(matcher.remove_device(&1).talk.is_none());
        assert_eq!(matcher.remove_device(&2).talk, Some(false));
    }

    #[test]
    fn seeded_held_keys_never_synthesize_commands() {
        let mut matcher = Matcher::new(bindings(), ModifierLayout::Evdev);
        matcher.seed_down([29, 47]);
        assert!(matcher.event(1, 47, true).commands().next().is_none());
        assert!(matcher.event(1, 47, false).commands().next().is_none());
        matcher.event(1, 29, false);
        matcher.event(1, 29, true);
        assert_eq!(
            matcher.event(1, 47, true).commands().collect::<Vec<_>>(),
            [Command::ToggleStream]
        );
    }

    #[test]
    fn windows_extended_modifier_codes_are_exact() {
        let bindings = Bindings {
            toggle_stream: Some(chord(0x002f, ModifierMask::CTRL | ModifierMask::ALT)),
            ..Bindings::default()
        };
        let mut matcher = Matcher::new(bindings, ModifierLayout::WindowsSet1);
        matcher.event(1, 0xe01d, true);
        matcher.event(2, 0xe038, true);
        assert_eq!(
            matcher
                .event(3, 0x002f, true)
                .commands()
                .collect::<Vec<_>>(),
            [Command::ToggleStream]
        );
    }
}
