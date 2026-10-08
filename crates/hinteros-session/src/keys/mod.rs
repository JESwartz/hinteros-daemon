//! Typing into the focused app: the daemon's virtual keyboard and gamescope's input method.
//!
//! Two backends, chosen per character (hinteros-docs/spikes/05-on-screen-keyboard.md):
//! - [`uinput`]: a virtual US keyboard. Works in any compositor and for any client, X11 or
//!   Wayland; the compositor's layout decides which character a key code makes, so this only
//!   types what is on a US keyboard (gamescope's XKB layout must be US).
//! - [`ime`]: gamescope's private `gamescope_input_method` protocol, which types any Unicode
//!   character (one per commit) but only exists under gamescope.
//!
//! [`plan_text`] and friends turn API requests into [`KeyOp`]s; a [`KeyInjector`] performs them
//! in order, in the background ([`worker`]), so a long text never blocks the daemon.

pub mod ime;
pub mod uinput;
pub mod worker;

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use hinteros_api::session::{Key, KeyAction, Modifier, NavAction};

/// Linux input event codes (`linux/input-event-codes.h`) used here.
pub mod code {
    pub const KEY_ESC: u16 = 1;
    pub const KEY_1: u16 = 2;
    pub const KEY_0: u16 = 11;
    pub const KEY_MINUS: u16 = 12;
    pub const KEY_EQUAL: u16 = 13;
    pub const KEY_BACKSPACE: u16 = 14;
    pub const KEY_TAB: u16 = 15;
    pub const KEY_LEFTBRACE: u16 = 26;
    pub const KEY_RIGHTBRACE: u16 = 27;
    pub const KEY_ENTER: u16 = 28;
    pub const KEY_LEFTCTRL: u16 = 29;
    pub const KEY_SEMICOLON: u16 = 39;
    pub const KEY_APOSTROPHE: u16 = 40;
    pub const KEY_GRAVE: u16 = 41;
    pub const KEY_LEFTSHIFT: u16 = 42;
    pub const KEY_BACKSLASH: u16 = 43;
    pub const KEY_COMMA: u16 = 51;
    pub const KEY_DOT: u16 = 52;
    pub const KEY_SLASH: u16 = 53;
    pub const KEY_LEFTALT: u16 = 56;
    pub const KEY_SPACE: u16 = 57;
    pub const KEY_CAPSLOCK: u16 = 58;
    pub const KEY_F1: u16 = 59;
    pub const KEY_F11: u16 = 87;
    pub const KEY_F12: u16 = 88;
    pub const KEY_HOME: u16 = 102;
    pub const KEY_UP: u16 = 103;
    pub const KEY_PAGEUP: u16 = 104;
    pub const KEY_LEFT: u16 = 105;
    pub const KEY_RIGHT: u16 = 106;
    pub const KEY_END: u16 = 107;
    pub const KEY_DOWN: u16 = 108;
    pub const KEY_PAGEDOWN: u16 = 109;
    pub const KEY_INSERT: u16 = 110;
    pub const KEY_DELETE: u16 = 111;
    pub const KEY_MUTE: u16 = 113;
    pub const KEY_VOLUMEDOWN: u16 = 114;
    pub const KEY_VOLUMEUP: u16 = 115;
    pub const KEY_LEFTMETA: u16 = 125;
    pub const KEY_COMPOSE: u16 = 127;
    pub const KEY_BACK: u16 = 158;
    pub const KEY_FORWARD: u16 = 159;
    pub const KEY_NEXTSONG: u16 = 163;
    pub const KEY_PLAYPAUSE: u16 = 164;
    pub const KEY_PREVIOUSSONG: u16 = 165;
    pub const KEY_STOPCD: u16 = 166;
    pub const KEY_REFRESH: u16 = 173;
}

/// The letter keys in alphabetical order (their codes follow the keyboard rows, not the alphabet).
const LETTERS: [u16; 26] = [
    30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38, 50, 49, 24, 25, 16, 19, 31, 20, 22, 47, 17, 45,
    21, 44,
];

/// The evdev key code of a [`Key`].
pub fn key_code(key: Key) -> u16 {
    use code::*;
    let letter = |i: usize| LETTERS[i];
    match key {
        Key::A => letter(0),
        Key::B => letter(1),
        Key::C => letter(2),
        Key::D => letter(3),
        Key::E => letter(4),
        Key::F => letter(5),
        Key::G => letter(6),
        Key::H => letter(7),
        Key::I => letter(8),
        Key::J => letter(9),
        Key::K => letter(10),
        Key::L => letter(11),
        Key::M => letter(12),
        Key::N => letter(13),
        Key::O => letter(14),
        Key::P => letter(15),
        Key::Q => letter(16),
        Key::R => letter(17),
        Key::S => letter(18),
        Key::T => letter(19),
        Key::U => letter(20),
        Key::V => letter(21),
        Key::W => letter(22),
        Key::X => letter(23),
        Key::Y => letter(24),
        Key::Z => letter(25),
        Key::Digit0 => KEY_0,
        Key::Digit1 => KEY_1,
        Key::Digit2 => KEY_1 + 1,
        Key::Digit3 => KEY_1 + 2,
        Key::Digit4 => KEY_1 + 3,
        Key::Digit5 => KEY_1 + 4,
        Key::Digit6 => KEY_1 + 5,
        Key::Digit7 => KEY_1 + 6,
        Key::Digit8 => KEY_1 + 7,
        Key::Digit9 => KEY_1 + 8,
        Key::F1 => KEY_F1,
        Key::F2 => KEY_F1 + 1,
        Key::F3 => KEY_F1 + 2,
        Key::F4 => KEY_F1 + 3,
        Key::F5 => KEY_F1 + 4,
        Key::F6 => KEY_F1 + 5,
        Key::F7 => KEY_F1 + 6,
        Key::F8 => KEY_F1 + 7,
        Key::F9 => KEY_F1 + 8,
        Key::F10 => KEY_F1 + 9,
        Key::F11 => KEY_F11,
        Key::F12 => KEY_F12,
        Key::Enter => KEY_ENTER,
        Key::Escape => KEY_ESC,
        Key::Backspace => KEY_BACKSPACE,
        Key::Tab => KEY_TAB,
        Key::Space => KEY_SPACE,
        Key::Delete => KEY_DELETE,
        Key::Insert => KEY_INSERT,
        Key::Home => KEY_HOME,
        Key::End => KEY_END,
        Key::PageUp => KEY_PAGEUP,
        Key::PageDown => KEY_PAGEDOWN,
        Key::Up => KEY_UP,
        Key::Down => KEY_DOWN,
        Key::Left => KEY_LEFT,
        Key::Right => KEY_RIGHT,
        Key::Minus => KEY_MINUS,
        Key::Equal => KEY_EQUAL,
        Key::LeftBracket => KEY_LEFTBRACE,
        Key::RightBracket => KEY_RIGHTBRACE,
        Key::Backslash => KEY_BACKSLASH,
        Key::Semicolon => KEY_SEMICOLON,
        Key::Apostrophe => KEY_APOSTROPHE,
        Key::Grave => KEY_GRAVE,
        Key::Comma => KEY_COMMA,
        Key::Period => KEY_DOT,
        Key::Slash => KEY_SLASH,
        Key::CapsLock => KEY_CAPSLOCK,
        Key::ContextMenu => KEY_COMPOSE,
        Key::Back => KEY_BACK,
        Key::Forward => KEY_FORWARD,
        Key::Refresh => KEY_REFRESH,
        Key::PlayPause => KEY_PLAYPAUSE,
        Key::Stop => KEY_STOPCD,
        Key::NextTrack => KEY_NEXTSONG,
        Key::PreviousTrack => KEY_PREVIOUSSONG,
        Key::VolumeUp => KEY_VOLUMEUP,
        Key::VolumeDown => KEY_VOLUMEDOWN,
        Key::Mute => KEY_MUTE,
    }
}

pub fn modifier_code(m: Modifier) -> u16 {
    match m {
        Modifier::Shift => code::KEY_LEFTSHIFT,
        Modifier::Ctrl => code::KEY_LEFTCTRL,
        Modifier::Alt => code::KEY_LEFTALT,
        Modifier::Meta => code::KEY_LEFTMETA,
    }
}

/// Every key code the virtual keyboard can send.
pub fn all_codes() -> Vec<u16> {
    let mut codes: Vec<u16> = Key::ALL.iter().map(|&k| key_code(k)).collect();
    codes.extend(Modifier::ALL.iter().map(|&m| modifier_code(m)));
    codes.sort_unstable();
    codes.dedup();
    codes
}

/// US layout: the unshifted and shifted character of each non-letter key (port of the spike's
/// `kbd.py` table).
const PAIRS: [(Key, char, char); 21] = [
    (Key::Grave, '`', '~'),
    (Key::Digit1, '1', '!'),
    (Key::Digit2, '2', '@'),
    (Key::Digit3, '3', '#'),
    (Key::Digit4, '4', '$'),
    (Key::Digit5, '5', '%'),
    (Key::Digit6, '6', '^'),
    (Key::Digit7, '7', '&'),
    (Key::Digit8, '8', '*'),
    (Key::Digit9, '9', '('),
    (Key::Digit0, '0', ')'),
    (Key::Minus, '-', '_'),
    (Key::Equal, '=', '+'),
    (Key::LeftBracket, '[', '{'),
    (Key::RightBracket, ']', '}'),
    (Key::Backslash, '\\', '|'),
    (Key::Semicolon, ';', ':'),
    (Key::Apostrophe, '\'', '"'),
    (Key::Comma, ',', '<'),
    (Key::Period, '.', '>'),
    (Key::Slash, '/', '?'),
];

/// The key (and whether Shift is needed) that types `c` on a US keyboard.
pub fn us_key(c: char) -> Option<(Key, bool)> {
    match c {
        'a'..='z' => Some((Key::ALL[c as usize - 'a' as usize], false)),
        'A'..='Z' => Some((Key::ALL[c as usize - 'A' as usize], true)),
        ' ' => Some((Key::Space, false)),
        '\n' => Some((Key::Enter, false)),
        '\t' => Some((Key::Tab, false)),
        _ => PAIRS.iter().find_map(|&(key, plain, shifted)| {
            (c == plain)
                .then_some((key, false))
                .or((c == shifted).then_some((key, true)))
        }),
    }
}

/// gamescope input method actions (`gamescope_input_method.action`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum ImeAction {
    Submit = 1,
    DeleteLeft = 2,
    DeleteRight = 3,
    MoveLeft = 4,
    MoveRight = 5,
    MoveUp = 6,
    MoveDown = 7,
}

/// The input method action that stands in for `action` when there is no virtual keyboard.
pub fn ime_action(action: KeyAction) -> Option<ImeAction> {
    Some(match action {
        KeyAction::Backspace => ImeAction::DeleteLeft,
        KeyAction::Delete => ImeAction::DeleteRight,
        KeyAction::Enter => ImeAction::Submit,
        KeyAction::Left => ImeAction::MoveLeft,
        KeyAction::Right => ImeAction::MoveRight,
        KeyAction::Up => ImeAction::MoveUp,
        KeyAction::Down => ImeAction::MoveDown,
        KeyAction::Tab
        | KeyAction::Escape
        | KeyAction::Space
        | KeyAction::Home
        | KeyAction::End => {
            return None;
        }
    })
}

/// One thing to send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyOp {
    /// Press the modifiers (in order), press and release the key, release the modifiers.
    Tap {
        code: u16,
        mods: Vec<u16>,
    },
    /// One character through the input method.
    Ime(String),
    ImeAction(ImeAction),
}

impl KeyOp {
    pub fn tap(combo: &KeyCombo) -> Self {
        KeyOp::Tap {
            code: key_code(combo.key),
            mods: combo.modifiers.iter().map(|&m| modifier_code(m)).collect(),
        }
    }
}

/// Which backends are there.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct KeyCaps {
    /// The virtual keyboard (uinput).
    pub keys: bool,
    /// gamescope's input method.
    pub ime: bool,
}

impl KeyCaps {
    pub fn any(self) -> bool {
        self.keys || self.ime
    }
}

/// What to send for `text`, and the characters that cannot be typed. Per character: the US
/// keyboard if there is one and the character is on it, else the input method.
pub fn plan_text(text: &str, caps: KeyCaps) -> (Vec<KeyOp>, Vec<String>) {
    let mut ops = Vec::new();
    let mut untypeable = Vec::new();
    let text = text.replace("\r\n", "\n");
    for c in text.chars() {
        match us_key(c) {
            Some((key, shift)) if caps.keys => ops.push(KeyOp::tap(&KeyCombo {
                key,
                modifiers: if shift { vec![Modifier::Shift] } else { vec![] },
            })),
            _ if caps.ime && c == '\n' => ops.push(KeyOp::ImeAction(ImeAction::Submit)),
            _ if caps.ime && !c.is_control() => ops.push(KeyOp::Ime(c.to_string())),
            _ => untypeable.push(c.to_string()),
        }
    }
    (ops, untypeable)
}

/// What to send for `keys.action`, if anything can.
pub fn plan_action(action: KeyAction, caps: KeyCaps) -> Option<KeyOp> {
    if caps.keys {
        Some(KeyOp::tap(&KeyCombo::from(action.key())))
    } else if caps.ime {
        ime_action(action).map(KeyOp::ImeAction)
    } else {
        None
    }
}

/// A key with modifiers, written `ctrl+shift+t` in the config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyCombo {
    pub key: Key,
    pub modifiers: Vec<Modifier>,
}

impl From<Key> for KeyCombo {
    fn from(key: Key) -> Self {
        Self {
            key,
            modifiers: Vec::new(),
        }
    }
}

impl FromStr for KeyCombo {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        let lower = s.trim().to_ascii_lowercase();
        let mut parts: Vec<&str> = lower.split('+').map(str::trim).collect();
        let key = parts.pop().unwrap_or_default();
        // A few common spellings besides the API names.
        let key = match key {
            "esc" => "escape",
            "return" => "enter",
            "del" => "delete",
            "pageup" => "page_up",
            "pagedown" => "page_down",
            other => other,
        };
        let key = Key::from_name(key).ok_or_else(|| format!("unknown key '{key}' in '{s}'"))?;
        let mut modifiers = Vec::new();
        for m in parts {
            let m = match m {
                "control" => "ctrl",
                "super" | "win" => "meta",
                other => other,
            };
            let m =
                Modifier::from_name(m).ok_or_else(|| format!("unknown modifier '{m}' in '{s}'"))?;
            if !modifiers.contains(&m) {
                modifiers.push(m);
            }
        }
        Ok(Self { key, modifiers })
    }
}

impl fmt::Display for KeyCombo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for m in &self.modifiers {
            write!(f, "{}+", m.name())?;
        }
        f.write_str(self.key.name())
    }
}

/// What controller actions mean for an app with the keyboard input profile.
pub type Keymap = BTreeMap<NavAction, KeyCombo>;

/// The default keymap: directions to arrows, accept to Enter, back to `back` (Escape unless the
/// app says otherwise).
pub fn default_keymap(back: KeyCombo) -> Keymap {
    BTreeMap::from([
        (NavAction::Up, Key::Up.into()),
        (NavAction::Down, Key::Down.into()),
        (NavAction::Left, Key::Left.into()),
        (NavAction::Right, Key::Right.into()),
        (NavAction::Accept, Key::Enter.into()),
        (NavAction::Back, back),
    ])
}

/// Performs [`KeyOp`]s, in order, without blocking the caller.
pub trait KeyInjector: Send {
    fn caps(&self) -> KeyCaps;
    /// Perform `ops` in order; never dropped (typed text, Enter, shortcuts).
    fn send(&mut self, ops: Vec<KeyOp>);
    /// One tap for a controller direction (a held or repeating D-pad/stick). Never queued
    /// behind other navigation: dropped if a newer navigation tap is waiting or if it waited
    /// too long ([`worker::STALE_NAV`]).
    fn send_nav(&mut self, op: KeyOp) {
        self.send(vec![op]);
    }
}

/// No keyboard at all.
pub struct NoKeys;

impl KeyInjector for NoKeys {
    fn caps(&self) -> KeyCaps {
        KeyCaps::default()
    }
    fn send(&mut self, _ops: Vec<KeyOp>) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tap(code: u16, mods: &[u16]) -> KeyOp {
        KeyOp::Tap {
            code,
            mods: mods.to_vec(),
        }
    }

    const SHIFT: u16 = code::KEY_LEFTSHIFT;
    const BOTH: KeyCaps = KeyCaps {
        keys: true,
        ime: true,
    };

    #[test]
    fn us_table() {
        // Every printable ASCII character is on the US keyboard.
        for c in ' '..='~' {
            assert!(us_key(c).is_some(), "{c:?}");
        }
        assert_eq!(us_key('a'), Some((Key::A, false)));
        assert_eq!(us_key('Q'), Some((Key::Q, true)));
        assert_eq!(us_key('@'), Some((Key::Digit2, true)));
        assert_eq!(us_key('/'), Some((Key::Slash, false)));
        assert_eq!(us_key('?'), Some((Key::Slash, true)));
        assert_eq!(us_key('"'), Some((Key::Apostrophe, true)));
        assert_eq!(us_key('é'), None);
        assert_eq!(key_code(Key::A), 30);
        assert_eq!(key_code(Key::Q), 16);
        assert_eq!(key_code(Key::M), 50);
        assert_eq!(key_code(Key::Digit9), 10);
        assert_eq!(key_code(Key::F10), 68);
        // Key codes are distinct.
        let codes: Vec<u16> = Key::ALL.iter().map(|&k| key_code(k)).collect();
        let mut unique = codes.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), codes.len());
    }

    #[test]
    fn plans_text_per_character() {
        let (ops, bad) = plan_text("P@ss-w0rd!", BOTH);
        assert!(bad.is_empty());
        assert_eq!(
            ops,
            [
                tap(25, &[SHIFT]),
                tap(3, &[SHIFT]),
                tap(31, &[]),
                tap(31, &[]),
                tap(12, &[]),
                tap(17, &[]),
                tap(11, &[]),
                tap(19, &[]),
                tap(32, &[]),
                tap(2, &[SHIFT]),
            ]
        );

        // Non-US characters go through the input method, one per op.
        let (ops, bad) = plan_text("é☃x\r\n", BOTH);
        assert!(bad.is_empty());
        assert_eq!(
            ops,
            [
                KeyOp::Ime("é".into()),
                KeyOp::Ime("☃".into()),
                tap(45, &[]),
                tap(code::KEY_ENTER, &[]),
            ]
        );

        // Without an input method they are reported.
        let keys_only = KeyCaps {
            keys: true,
            ime: false,
        };
        let (ops, bad) = plan_text("café\u{7}", keys_only);
        assert_eq!(ops.len(), 3);
        assert_eq!(bad, ["é", "\u{7}"]);

        // Without uinput, everything goes through the input method.
        let ime_only = KeyCaps {
            keys: false,
            ime: true,
        };
        let (ops, bad) = plan_text("a\n\t", ime_only);
        assert_eq!(
            ops,
            [KeyOp::Ime("a".into()), KeyOp::ImeAction(ImeAction::Submit)]
        );
        assert_eq!(bad, ["\t"]);
    }

    #[test]
    fn plans_actions() {
        assert_eq!(
            plan_action(KeyAction::Backspace, BOTH),
            Some(tap(code::KEY_BACKSPACE, &[]))
        );
        let ime_only = KeyCaps {
            keys: false,
            ime: true,
        };
        assert_eq!(
            plan_action(KeyAction::Backspace, ime_only),
            Some(KeyOp::ImeAction(ImeAction::DeleteLeft))
        );
        assert_eq!(plan_action(KeyAction::Tab, ime_only), None);
        assert_eq!(plan_action(KeyAction::Enter, KeyCaps::default()), None);
    }

    #[test]
    fn key_combos() {
        let c: KeyCombo = "Ctrl+Shift+T".parse().unwrap();
        assert_eq!(c.key, Key::T);
        assert_eq!(c.modifiers, [Modifier::Ctrl, Modifier::Shift]);
        assert_eq!(c.to_string(), "ctrl+shift+t");
        assert_eq!("esc".parse::<KeyCombo>().unwrap(), Key::Escape.into());
        assert_eq!(
            "alt+left".parse::<KeyCombo>().unwrap(),
            KeyCombo {
                key: Key::Left,
                modifiers: vec![Modifier::Alt]
            }
        );
        assert_eq!(
            "backspace".parse::<KeyCombo>().unwrap(),
            Key::Backspace.into()
        );
        for bad in ["", "ctrl+", "hyper+a", "é", "ctrl+shift"] {
            assert!(bad.parse::<KeyCombo>().is_err(), "{bad}");
        }
        assert_eq!(
            KeyOp::tap(&"ctrl+l".parse().unwrap()),
            tap(38, &[code::KEY_LEFTCTRL])
        );
    }

    #[test]
    fn default_keymap_maps_directions_accept_and_back() {
        let map = default_keymap(Key::Backspace.into());
        assert_eq!(map[&NavAction::Up], Key::Up.into());
        assert_eq!(map[&NavAction::Accept], Key::Enter.into());
        assert_eq!(map[&NavAction::Back], Key::Backspace.into());
        assert!(!map.contains_key(&NavAction::Context));
        assert!(all_codes().contains(&code::KEY_LEFTMETA));
    }
}
