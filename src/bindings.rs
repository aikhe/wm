//! Configuration is compiled into chords and typed actions before it reaches
//! the input loop. Rebinding registers additions before removing old keys.
use std::collections::BTreeMap;

use windows::Win32::UI::Input::KeyboardAndMouse::{
    HOT_KEY_MODIFIERS, RegisterHotKey, UnregisterHotKey,
};

use crate::layout::{Direction, Mode};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Focus(Direction),
    Move(Direction),
    Workspace(usize),
    Send(usize),
    CycleWidth,
    /// Sign of the configured resize step.
    Resize(i32),
    ToggleFull,
    Toggle(Mode),
    Overview,
    Close,
    Quit,
    Reload,
}

pub const DEFAULTS: [(&str, &str, Action); 37] = [
    ("focus_left", "Alt+H", Action::Focus(Direction::Left)),
    ("focus_right", "Alt+L", Action::Focus(Direction::Right)),
    ("focus_up", "Alt+K", Action::Focus(Direction::Up)),
    ("focus_down", "Alt+J", Action::Focus(Direction::Down)),
    ("move_left", "Alt+Shift+H", Action::Move(Direction::Left)),
    ("move_right", "Alt+Shift+L", Action::Move(Direction::Right)),
    ("move_up", "Alt+Shift+K", Action::Move(Direction::Up)),
    ("move_down", "Alt+Shift+J", Action::Move(Direction::Down)),
    ("workspace_1", "Alt+1", Action::Workspace(0)),
    ("workspace_2", "Alt+2", Action::Workspace(1)),
    ("workspace_3", "Alt+3", Action::Workspace(2)),
    ("workspace_4", "Alt+4", Action::Workspace(3)),
    ("workspace_5", "Alt+5", Action::Workspace(4)),
    ("workspace_6", "Alt+6", Action::Workspace(5)),
    ("workspace_7", "Alt+7", Action::Workspace(6)),
    ("workspace_8", "Alt+8", Action::Workspace(7)),
    ("workspace_9", "Alt+9", Action::Workspace(8)),
    ("send_1", "Alt+Shift+1", Action::Send(0)),
    ("send_2", "Alt+Shift+2", Action::Send(1)),
    ("send_3", "Alt+Shift+3", Action::Send(2)),
    ("send_4", "Alt+Shift+4", Action::Send(3)),
    ("send_5", "Alt+Shift+5", Action::Send(4)),
    ("send_6", "Alt+Shift+6", Action::Send(5)),
    ("send_7", "Alt+Shift+7", Action::Send(6)),
    ("send_8", "Alt+Shift+8", Action::Send(7)),
    ("send_9", "Alt+Shift+9", Action::Send(8)),
    ("cycle_width", "Alt+R", Action::CycleWidth),
    ("resize_narrower", "Alt+Minus", Action::Resize(-1)),
    ("resize_wider", "Alt+Equals", Action::Resize(1)),
    ("toggle_full", "Alt+F", Action::ToggleFull),
    ("toggle_float", "Alt+T", Action::Toggle(Mode::Floating)),
    ("toggle_sticky", "Alt+Shift+T", Action::Toggle(Mode::Sticky)),
    (
        "toggle_top",
        "Ctrl+Alt+Shift+T",
        Action::Toggle(Mode::OnTop),
    ),
    ("overview", "Alt+O", Action::Overview),
    ("close", "Alt+Shift+W", Action::Close),
    ("quit", "Alt+Shift+E", Action::Quit),
    ("reload", "Alt+Shift+R", Action::Reload),
];

pub fn action_index(name: &str) -> Option<usize> {
    DEFAULTS.iter().position(|(known, _, _)| *known == name)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Chord {
    pub modifiers: u32,
    pub key: u16,
}

impl Chord {
    /// Stable across reloads, even if its action changes. All keys fit a byte.
    pub fn id(self) -> i32 {
        (self.modifiers * 256 + u32::from(self.key)) as i32
    }

    pub fn parse(text: &str) -> Result<Self, String> {
        let mut chord = Chord {
            modifiers: 0,
            key: 0,
        };
        for token in text
            .split('+')
            .map(|token| token.trim().to_ascii_lowercase())
        {
            let modifier = match token.as_str() {
                "alt" => 1,
                "ctrl" | "control" => 2,
                "shift" => 4,
                "win" | "super" => 8,
                _ => 0,
            };
            if modifier != 0 {
                if chord.modifiers & modifier != 0 {
                    return Err(format!("repeated modifier in `{text}`"));
                }
                chord.modifiers |= modifier;
            } else {
                let key = parse_key(&token).ok_or_else(|| format!("unknown key in `{text}`"))?;
                if chord.key != 0 {
                    return Err(format!("more than one key in `{text}`"));
                }
                chord.key = key;
            }
        }
        if chord.key == 0 {
            return Err(format!("missing key in `{text}`"));
        }
        Ok(chord)
    }
}

fn parse_key(text: &str) -> Option<u16> {
    if text.len() == 1 {
        let byte = text.as_bytes()[0];
        if byte.is_ascii_alphanumeric() {
            return Some(u16::from(byte.to_ascii_uppercase()));
        }
    }
    if let Some(number) = text.strip_prefix('f').and_then(|n| n.parse::<u16>().ok()) {
        return (1..=24).contains(&number).then_some(0x6f + number);
    }
    Some(match text {
        "left" => 0x25,
        "up" => 0x26,
        "right" => 0x27,
        "down" => 0x28,
        "space" => 0x20,
        "tab" => 9,
        "enter" | "return" => 13,
        "escape" | "esc" => 27,
        "backspace" => 8,
        "delete" => 0x2e,
        "insert" => 0x2d,
        "home" => 0x24,
        "end" => 0x23,
        "pageup" => 0x21,
        "pagedown" => 0x22,
        "minus" | "-" => 0xbd,
        "equals" | "=" => 0xbb,
        "comma" | "," => 0xbc,
        "period" | "." => 0xbe,
        "semicolon" | ";" => 0xba,
        "slash" | "/" => 0xbf,
        "backslash" | "\\" => 0xdc,
        "leftbracket" | "[" => 0xdb,
        "rightbracket" | "]" => 0xdd,
        "quote" | "'" => 0xde,
        "backtick" | "`" => 0xc0,
        _ => return None,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    pub chord: Chord,
    pub action: Action,
    pub name: String,
}

pub fn compile(overrides: &BTreeMap<String, String>) -> Result<Vec<Binding>, String> {
    for name in overrides.keys() {
        if !DEFAULTS.iter().any(|(known, _, _)| known == name) {
            return Err(format!("unknown hotkeys action `{name}`"));
        }
    }
    let mut bindings: Vec<Binding> = Vec::new();
    for (name, default, action) in DEFAULTS {
        let text = overrides.get(name).map_or(default, String::as_str).trim();
        if text.is_empty() {
            if action == Action::Quit {
                return Err("hotkeys.quit must remain bound".into());
            }
            continue;
        }
        let chord = Chord::parse(text).map_err(|error| format!("hotkeys.{name}: {error}"))?;
        if let Some(other) = bindings.iter().find(|binding| binding.chord == chord) {
            return Err(format!(
                "hotkeys.{name} conflicts with hotkeys.{} ({text})",
                other.name
            ));
        }
        bindings.push(Binding {
            chord,
            action,
            name: name.into(),
        });
    }
    Ok(bindings)
}

#[derive(Default)]
pub struct Hotkeys {
    pub bindings: Vec<Binding>,
}

impl Hotkeys {
    pub fn replace(&mut self, bindings: Vec<Binding>) -> Result<(), String> {
        self.replace_with(
            bindings,
            |chord| {
                unsafe {
                    RegisterHotKey(
                        None,
                        chord.id(),
                        HOT_KEY_MODIFIERS(chord.modifiers),
                        u32::from(chord.key),
                    )
                }
                .map_err(|error| error.to_string())
            },
            |chord| {
                let _ = unsafe { UnregisterHotKey(None, chord.id()) };
            },
        )
    }

    fn replace_with(
        &mut self,
        bindings: Vec<Binding>,
        mut register: impl FnMut(Chord) -> Result<(), String>,
        mut unregister: impl FnMut(Chord),
    ) -> Result<(), String> {
        let mut added = Vec::new();
        for binding in &bindings {
            if self.bindings.iter().any(|old| old.chord == binding.chord) {
                continue;
            }
            if let Err(error) = register(binding.chord) {
                for chord in added {
                    unregister(chord);
                }
                return Err(format!(
                    "couldn't register hotkeys.{}: {error}; previous settings kept",
                    binding.name
                ));
            }
            added.push(binding.chord);
        }
        for old in &self.bindings {
            if !bindings.iter().any(|new| new.chord == old.chord) {
                unregister(old.chord);
            }
        }
        self.bindings = bindings;
        Ok(())
    }

    pub fn action(&self, id: usize) -> Option<Action> {
        self.bindings
            .iter()
            .find(|binding| binding.chord.id() as usize == id)
            .map(|binding| binding.action)
    }
}

impl Drop for Hotkeys {
    fn drop(&mut self) {
        for binding in &self.bindings {
            let _ = unsafe { UnregisterHotKey(None, binding.chord.id()) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_names_round_trip() {
        for (index, (name, _, action)) in DEFAULTS.iter().enumerate() {
            assert_eq!(action_index(name), Some(index));
            assert_eq!(DEFAULTS[action_index(name).unwrap()].2, *action);
        }
        assert_eq!(action_index("unknown"), None);
    }

    #[test]
    fn aliases_and_case_cannot_hide_duplicate_bindings() {
        assert_eq!(
            Chord::parse("shift+ALT+Ctrl+equals"),
            Chord::parse("control+alt+SHIFT+=")
        );
        let overrides = BTreeMap::from([("focus_left".into(), "alt+l".into())]);
        assert!(compile(&overrides).unwrap_err().contains("conflicts"));
        assert!(Chord::parse("Alt+Alt+H").is_err());
        assert!(Chord::parse("Alt+H+J").is_err());
        assert!(Chord::parse("F25").is_err());
    }

    #[test]
    fn failed_rebinding_rolls_back_additions_and_preserves_old_actions() {
        let old = compile(&BTreeMap::new()).unwrap();
        let mut keys = Hotkeys::default();
        keys.replace_with(old.clone(), |_| Ok(()), |_| {}).unwrap();
        let overrides = BTreeMap::from([
            ("focus_left".into(), "Alt+U".into()),
            ("focus_right".into(), "Alt+I".into()),
        ]);
        let new = compile(&overrides).unwrap();
        let mut added = Vec::new();
        let mut undone = Vec::new();
        let failed = keys.replace_with(
            new,
            |chord| {
                if chord.key == u16::from(b'I') {
                    return Err("in use by another app".into());
                }
                added.push(chord);
                Ok(())
            },
            |chord| undone.push(chord),
        );
        assert!(failed.is_err());
        assert_eq!(keys.bindings, old);
        assert_eq!(added, undone);
        // This test uses a fake registrar, so Drop must not touch real hotkeys.
        keys.bindings.clear();
    }

    #[test]
    fn swapping_actions_on_existing_chords_does_not_reregister_keys() {
        let mut keys = Hotkeys {
            bindings: compile(&BTreeMap::new()).unwrap(),
        };
        let overrides = BTreeMap::from([
            ("focus_left".into(), "Alt+L".into()),
            ("focus_right".into(), "Alt+H".into()),
        ]);
        keys.replace_with(
            compile(&overrides).unwrap(),
            |_| panic!("already registered"),
            |_| panic!("still needed"),
        )
        .unwrap();
        let id = Chord::parse("Alt+H").unwrap().id() as usize;
        assert_eq!(keys.action(id), Some(Action::Focus(Direction::Right)));
        keys.bindings.clear();
    }
}
