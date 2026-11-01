//! The profile's controller settings (session API 0.20, `input.settings.*`): which button does
//! which action, the keys of keyboard-mode apps, and the right-stick pointer.
//!
//! Kept in `$XDG_STATE_HOME/hinteros/controller.json` (`{"version": 1, "settings": ...}`) as the
//! launcher sends them, only what the user changed ([`ControllerSettings`]).
//! [`Controller::check`] turns them into what the shell uses; a missing file is the defaults, a
//! bad one too (with a warning: it never stops the session). A file of a newer version (after a
//! rollback) is the defaults and stays as it is: nothing is saved over it.
//! `input.settings.set` checks the new settings, saves them atomically and the shell
//! applies them at once.
//!
//! - `general`: action -> button over the defaults; every button has exactly one action. The
//!   shell maps the ten buttons through it before anything else reads them (launcher
//!   navigation, keyboard-mode apps, the on-screen keyboard, the Guide chords). Apps that get
//!   the pad itself (Steam, games) are not affected.
//! - Keys of a keyboard-mode app, lowest to highest: the default keymap, `keyboard` (all apps),
//!   the app's own (`AppConfig::own_keymap`: back_key, the address bar's keys, its keymap),
//!   `apps.<id>`. The launcher computes the same from `input.settings.get`.
//! - `pointer`: speed and swapped sticks go into the InputPlumber profile
//!   ([`crate::input::pointer::profile_yaml`]); the click buttons are physical buttons, read
//!   before the `general` remap.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use hinteros_api::common::check_version;
use hinteros_api::session::{
    CONTROLLER_ACTIONS, ControllerButton, ControllerDefaults, ControllerSettings, KEY_ACTIONS, Key,
    KeyboardApp, Modifier, NavAction, POINTER_SPEED_DEFAULT, POINTER_SPEED_MAX, POINTER_SPEED_MIN,
    POINTER_SPEED_STEP, PointerDefaults, SpeedRange,
};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::config::{AppConfig, nav_action, valid_app_id};
use crate::input::{MouseButton, PointerTuning};
use crate::keys::{KeyLayer, Keymap, apply_layer, binding_name, default_keymap, parse_binding};

/// The settings file, relative to `$XDG_STATE_HOME` (else `~/.local/state`).
pub const SETTINGS_FILE: &str = "hinteros/controller.json";

/// The version of the settings file.
const FILE_VERSION: u32 = 1;

/// The settings file.
#[derive(Debug, Serialize, Deserialize)]
struct SettingsFile<S> {
    version: u32,
    settings: S,
}

/// The default left and right click buttons.
pub const LEFT_CLICK: ControllerButton = ControllerButton::Rt;
pub const RIGHT_CLICK: ControllerButton = ControllerButton::Lt;

/// The settings, checked and ready to use.
#[derive(Debug, Clone, PartialEq)]
pub struct Controller {
    /// Physical button -> action, all ten.
    actions: BTreeMap<ControllerButton, NavAction>,
    /// Every keyboard-mode app.
    keyboard: KeyLayer,
    /// Per keyboard-mode app.
    apps: BTreeMap<String, KeyLayer>,
    pub tuning: PointerTuning,
    pub left_click: ControllerButton,
    pub right_click: ControllerButton,
}

impl Default for Controller {
    fn default() -> Self {
        Self {
            actions: ControllerButton::ALL
                .iter()
                .map(|&b| (b, b.default_action()))
                .collect(),
            keyboard: KeyLayer::new(),
            apps: BTreeMap::new(),
            tuning: PointerTuning::default(),
            left_click: LEFT_CLICK,
            right_click: RIGHT_CLICK,
        }
    }
}

impl Controller {
    /// Check `s`. `known_app` says whether an id in `apps` may be there. The error says what is
    /// wrong, for `invalid_params`.
    pub fn check(
        s: &ControllerSettings,
        known_app: impl Fn(&str) -> bool,
    ) -> Result<Self, Invalid> {
        let mut c = Self::default();
        if let Some(general) = &s.general {
            let mut by_action: BTreeMap<NavAction, ControllerButton> = ControllerButton::ALL
                .iter()
                .map(|&b| (b.default_action(), b))
                .collect();
            for (action, button) in general {
                let a = nav_action(action)
                    .filter(|a| CONTROLLER_ACTIONS.contains(a))
                    .ok_or_else(|| {
                        Invalid::new(
                            format!("general: unknown action '{action}'"),
                            "settings.general".into(),
                            "unknown_action",
                        )
                    })?;
                let b = ControllerButton::from_name(button).ok_or_else(|| {
                    Invalid::new(
                        format!("general: unknown button '{button}'"),
                        format!("settings.general.{action}"),
                        "unknown_button",
                    )
                })?;
                by_action.insert(a, b);
            }
            // Ten actions on ten buttons: none has two exactly when none has none.
            for &b in ControllerButton::ALL {
                let on: Vec<NavAction> = CONTROLLER_ACTIONS
                    .iter()
                    .copied()
                    .filter(|a| by_action[a] == b)
                    .collect();
                if on.len() > 1 {
                    let names: Vec<String> = on.iter().map(|&a| action_name(a)).collect();
                    let count = if on.len() == 2 {
                        "two".to_string()
                    } else {
                        on.len().to_string()
                    };
                    return Err(Invalid::new(
                        format!(
                            "general: {} has {count} actions ({})",
                            b.name(),
                            names.join(", ")
                        ),
                        "settings.general".into(),
                        "not_bijective",
                    ));
                }
            }
            for (a, b) in by_action {
                c.actions.insert(b, a);
            }
        }
        if let Some(keyboard) = &s.keyboard {
            c.keyboard = key_layer("keyboard", keyboard)?;
        }
        for (id, map) in s.apps.iter().flatten() {
            if !valid_app_id(id) || !known_app(id) {
                return Err(Invalid::new(
                    format!("no keyboard app '{id}'"),
                    "settings.apps".into(),
                    "unknown_app",
                ));
            }
            c.apps
                .insert(id.clone(), key_layer(&format!("apps.{id}"), map)?);
        }
        if let Some(p) = &s.pointer {
            if let Some(speed) = p.speed {
                if !(POINTER_SPEED_MIN..=POINTER_SPEED_MAX).contains(&speed)
                    || speed % POINTER_SPEED_STEP != 0
                {
                    return Err(Invalid::new(
                        format!(
                            "pointer: speed must be {POINTER_SPEED_MIN} to {POINTER_SPEED_MAX} in steps of {POINTER_SPEED_STEP}"
                        ),
                        "settings.pointer.speed".into(),
                        "bad_speed",
                    ));
                }
                c.tuning.speed = speed;
            }
            if let Some(swap) = p.swap_sticks {
                c.tuning.swap_sticks = swap;
            }
            let button = |field: &str, name: &Option<String>, default| match name {
                None => Ok(default),
                Some(n) => ControllerButton::from_name(n).ok_or_else(|| {
                    Invalid::new(
                        format!("pointer: unknown button '{n}'"),
                        format!("settings.pointer.{field}"),
                        "unknown_button",
                    )
                }),
            };
            c.left_click = button("left_click", &p.left_click, LEFT_CLICK)?;
            c.right_click = button("right_click", &p.right_click, RIGHT_CLICK)?;
            if c.left_click == c.right_click {
                return Err(Invalid::new(
                    format!(
                        "pointer: left and right click are both {}",
                        c.left_click.name()
                    ),
                    "settings.pointer".into(),
                    "same_buttons",
                ));
            }
        }
        Ok(c)
    }

    /// The action of a physical button.
    pub fn action(&self, button: ControllerButton) -> NavAction {
        self.actions[&button]
    }

    /// The mouse button a physical button is, with the pointer.
    pub fn click(&self, button: ControllerButton) -> Option<MouseButton> {
        if button == self.left_click {
            Some(MouseButton::Left)
        } else if button == self.right_click {
            Some(MouseButton::Right)
        } else {
            None
        }
    }

    /// An app's keymap (`None`: not a keyboard-mode app): the default keymap, `keyboard`, the
    /// app's own keys, `apps.<id>`.
    pub fn keymap(&self, app: &AppConfig) -> Option<Keymap> {
        let own = match app.keymap() {
            Ok(None) => return None,
            Ok(Some(_)) => app.own_keymap().unwrap_or_default(),
            Err(e) => {
                warn!(app = %app.id, "bad keymap, using the defaults: {e:#}");
                KeyLayer::new()
            }
        };
        let mut map = default_keymap(Key::Escape.into());
        apply_layer(&mut map, &self.keyboard);
        apply_layer(&mut map, &own);
        if let Some(user) = self.apps.get(&app.id) {
            apply_layer(&mut map, user);
        }
        Some(map)
    }

    /// The actions that the user's keys for this app (`apps.<id>`) map to a key.
    pub fn app_mapped(&self, id: &str) -> impl Iterator<Item = NavAction> + '_ {
        self.apps
            .get(id)
            .into_iter()
            .flatten()
            .filter(|(_, k)| k.is_some())
            .map(|(a, _)| *a)
    }
}

/// What is wrong with settings: `invalid_params` with `data {field, reason}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invalid {
    pub message: String,
    /// Where (`settings.general`, `settings.keyboard.back`, ...).
    pub field: String,
    pub reason: &'static str,
}

impl Invalid {
    fn new(message: String, field: String, reason: &'static str) -> Self {
        Self {
            message,
            field,
            reason,
        }
    }
}

impl std::fmt::Display for Invalid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// A keymap setting (`what`: where it is, `keyboard` or `apps.<id>`).
fn key_layer(what: &str, map: &BTreeMap<String, String>) -> Result<KeyLayer, Invalid> {
    let mut layer = KeyLayer::new();
    for (action, key) in map {
        let a = nav_action(action)
            .filter(|a| KEY_ACTIONS.contains(a))
            .ok_or_else(|| {
                Invalid::new(
                    format!("{what}: unknown action '{action}'"),
                    format!("settings.{what}"),
                    "unknown_action",
                )
            })?;
        let k = parse_binding(key).map_err(|e| {
            Invalid::new(
                format!("{what}: {e}"),
                format!("settings.{what}.{action}"),
                "unknown_key",
            )
        })?;
        layer.insert(a, k);
    }
    Ok(layer)
}

/// An action's API name.
pub fn action_name(action: NavAction) -> String {
    serde_json::to_value(action)
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_else(|| format!("{action:?}"))
}

/// The key names for `input.settings.get`: the usual keys first, then letters, digits and the
/// rest.
pub fn key_list() -> Vec<Key> {
    use Key::*;
    let mut keys = vec![
        Enter, Escape, Backspace, Space, Tab, Up, Down, Left, Right, PageUp, PageDown, Home, End,
        F1, F2, F3, F4, F5, F6, F7, F8, F9, F10, F11, F12,
    ];
    let letters = Key::ALL.iter().copied().filter(|k| {
        let n = k.name();
        n.len() == 1 && n.as_bytes()[0].is_ascii_lowercase()
    });
    let digits = Key::ALL
        .iter()
        .copied()
        .filter(|k| k.name().len() == 1 && k.name().as_bytes()[0].is_ascii_digit());
    keys.extend(letters);
    keys.extend(digits);
    let rest: Vec<Key> = Key::ALL
        .iter()
        .copied()
        .filter(|k| !keys.contains(k))
        .collect();
    keys.extend(rest);
    keys
}

/// The modifiers for `input.settings.get`, in the order a key is written.
pub const MODIFIERS: [Modifier; 4] = [
    Modifier::Ctrl,
    Modifier::Alt,
    Modifier::Shift,
    Modifier::Meta,
];

/// `input.settings.get`'s `defaults`.
pub fn defaults() -> ControllerDefaults {
    ControllerDefaults {
        general: ControllerButton::ALL
            .iter()
            .map(|&b| (b.default_action(), b))
            .collect(),
        keyboard: default_keymap(Key::Escape.into())
            .iter()
            .map(|(a, k)| (*a, k.to_string()))
            .collect(),
        pointer: PointerDefaults {
            speed: POINTER_SPEED_DEFAULT,
            swap_sticks: false,
            left_click: LEFT_CLICK,
            right_click: RIGHT_CLICK,
        },
    }
}

/// `input.settings.get`'s `speed`.
pub fn speed_range() -> SpeedRange {
    SpeedRange {
        min: POINTER_SPEED_MIN,
        max: POINTER_SPEED_MAX,
        step: POINTER_SPEED_STEP,
    }
}

/// The keyboard-mode apps among `apps`, each with its own keys (`input.settings.get`).
pub fn keyboard_apps(apps: &[AppConfig]) -> Vec<KeyboardApp> {
    apps.iter()
        .filter(|a| a.input == crate::config::InputProfile::Keyboard)
        .map(|a| KeyboardApp {
            id: a.id.clone(),
            name: a.name.clone(),
            keymap: a
                .own_keymap()
                .unwrap_or_default()
                .iter()
                .map(|(action, key)| (*action, binding_name(key.as_ref())))
                .collect(),
        })
        .collect()
}

/// Why `input.settings.set` failed.
#[derive(Debug)]
pub enum SetError {
    /// The settings are wrong (`invalid_params`).
    Invalid(Invalid),
    /// They could not be saved.
    Io(std::io::Error),
}

/// The settings as saved, and checked.
#[derive(Debug)]
pub struct ControllerStore {
    file: Option<PathBuf>,
    saved: ControllerSettings,
    current: Controller,
}

impl Default for ControllerStore {
    /// The defaults, and nowhere to save (tests).
    fn default() -> Self {
        Self::load(None)
    }
}

impl ControllerStore {
    /// From `$XDG_STATE_HOME/hinteros/controller.json`.
    pub fn load_default() -> Self {
        Self::load(crate::displays::state_dir().map(|d| d.join(SETTINGS_FILE)))
    }

    /// From `file`: missing is the defaults; unreadable or wrong is the defaults too, with a
    /// warning (app ids are not checked against the apps here: an app may come back).
    pub fn load(file: Option<PathBuf>) -> Self {
        let mut store = Self {
            file,
            saved: ControllerSettings::default(),
            current: Controller::default(),
        };
        let Some(path) = store.file.as_deref() else {
            return store;
        };
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return store,
            Err(e) => {
                warn!(file = %path.display(), "controller settings ignored, using the defaults: {e}");
                return store;
            }
        };
        if let Err(e) = check_version(path, text.as_bytes(), FILE_VERSION) {
            warn!("controller settings use the defaults: {e}");
            store.file = None;
            return store;
        }
        let checked = serde_json::from_str::<SettingsFile<ControllerSettings>>(&text)
            .map_err(|e| e.to_string())
            .and_then(|f| {
                let s = f.settings;
                Controller::check(&s, |_| true)
                    .map(|c| (s, c))
                    .map_err(|e| e.message)
            });
        match checked {
            Ok((saved, current)) => {
                info!(file = %path.display(), "controller settings loaded");
                store.saved = saved;
                store.current = current;
            }
            Err(e) => {
                warn!(file = %path.display(), "controller settings ignored, using the defaults: {e}")
            }
        }
        store
    }

    /// The settings as saved.
    pub fn saved(&self) -> &ControllerSettings {
        &self.saved
    }

    pub fn get(&self) -> &Controller {
        &self.current
    }

    /// Check `s` (apps: `keyboard_apps`, or an entry saved before and sent back unchanged: its
    /// app may come back), save it and use it.
    pub fn set(&mut self, s: ControllerSettings, keyboard_apps: &[String]) -> Result<(), SetError> {
        let old = self.saved.apps.clone().unwrap_or_default();
        let new = s.apps.clone().unwrap_or_default();
        let known = |id: &str| {
            keyboard_apps.iter().any(|a| a == id)
                || (old.contains_key(id) && old.get(id) == new.get(id))
        };
        let current = Controller::check(&s, known).map_err(SetError::Invalid)?;
        if let Some(path) = &self.file {
            save(path, &s).map_err(SetError::Io)?;
        }
        self.saved = s;
        self.current = current;
        Ok(())
    }
}

/// Write the file atomically (a temporary file in the same directory, then rename).
fn save(path: &Path, s: &ControllerSettings) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    serde_json::to_writer_pretty(
        &mut tmp,
        &SettingsFile {
            version: FILE_VERSION,
            settings: s,
        },
    )?;
    writeln!(tmp)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::config::InputProfile;

    fn settings(v: serde_json::Value) -> ControllerSettings {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn errors_name_the_field() {
        let e = |v| Controller::check(&settings(v), |id| id == "youtube").unwrap_err();
        let got = |i: Invalid| (i.field, i.reason);
        let s = |f: &str, r| (f.to_string(), r);
        assert_eq!(
            got(e(json!({"general": {"accept": "b"}}))),
            s("settings.general", "not_bijective")
        );
        assert_eq!(
            got(e(json!({"general": {"accept": "guide"}}))),
            s("settings.general.accept", "unknown_button")
        );
        assert_eq!(
            got(e(json!({"keyboard": {"back": "hyper+q"}}))),
            s("settings.keyboard.back", "unknown_key")
        );
        assert_eq!(
            got(e(json!({"apps": {"youtube": {"jump": "a"}}}))),
            s("settings.apps.youtube", "unknown_action")
        );
        assert_eq!(
            got(e(json!({"apps": {"nope": {}}}))),
            s("settings.apps", "unknown_app")
        );
        assert_eq!(
            got(e(json!({"pointer": {"speed": 10}}))),
            s("settings.pointer.speed", "bad_speed")
        );
        assert_eq!(
            got(e(json!({"pointer": {"left_click": "lt"}}))),
            s("settings.pointer", "same_buttons")
        );
        // Three actions on one button.
        assert_eq!(
            e(json!({"general": {"accept": "y", "back": "y"}})).message,
            "general: y has 3 actions (accept, back, context)"
        );
    }

    fn check(v: serde_json::Value) -> Result<Controller, String> {
        Controller::check(&settings(v), |id| id == "youtube").map_err(|e| e.message)
    }

    fn combo(s: &str) -> crate::keys::KeyCombo {
        s.parse().unwrap()
    }

    #[test]
    fn defaults_match_the_button_events() {
        let c = Controller::default();
        for &b in ControllerButton::ALL {
            assert_eq!(c.action(b), b.default_action());
        }
        assert_eq!(c.click(ControllerButton::Rt), Some(MouseButton::Left));
        assert_eq!(c.click(ControllerButton::Lt), Some(MouseButton::Right));
        assert_eq!(c.click(ControllerButton::A), None);
        assert_eq!(c.tuning, PointerTuning::default());
        assert_eq!(check(json!({})).unwrap(), c);
        // The defaults are what the session did before settings existed.
        let d = serde_json::to_value(defaults()).unwrap();
        assert_eq!(
            d["general"],
            json!({"accept": "a", "back": "b", "secondary": "x", "context": "y", "page_left": "lb", "page_right": "rb", "trigger_left": "lt", "trigger_right": "rt", "view": "view", "menu": "menu"})
        );
        assert_eq!(
            d["keyboard"],
            json!({"up": "up", "down": "down", "left": "left", "right": "right", "accept": "enter", "back": "escape"})
        );
        // Every key once, the usual ones first.
        let keys = key_list();
        assert_eq!(keys.len(), Key::ALL.len());
        assert_eq!(&keys[..3], &[Key::Enter, Key::Escape, Key::Backspace]);
        assert_eq!(keys[25], Key::A);
        assert_eq!(keys[51], Key::Digit0);
    }

    #[test]
    fn general_is_a_bijection() {
        let c = check(json!({"general": {"accept": "b", "back": "a"}})).unwrap();
        assert_eq!(c.action(ControllerButton::A), NavAction::Back);
        assert_eq!(c.action(ControllerButton::B), NavAction::Accept);
        assert_eq!(c.action(ControllerButton::X), NavAction::Secondary);
        // A three-way rotation.
        let c =
            check(json!({"general": {"accept": "x", "secondary": "y", "context": "a"}})).unwrap();
        assert_eq!(c.action(ControllerButton::A), NavAction::Context);
        assert_eq!(c.action(ControllerButton::X), NavAction::Accept);
        assert_eq!(c.action(ControllerButton::Y), NavAction::Secondary);

        assert_eq!(
            check(json!({"general": {"accept": "b"}})).unwrap_err(),
            "general: b has two actions (accept, back)"
        );
        assert_eq!(
            check(json!({"general": {"jump": "a"}})).unwrap_err(),
            "general: unknown action 'jump'"
        );
        // Directions and the other actions are not in general.
        for action in ["up", "quick", "keyboard"] {
            assert!(
                check(json!({"general": {action: "a"}})).is_err(),
                "{action}"
            );
        }
        assert_eq!(
            check(json!({"general": {"accept": "guide"}})).unwrap_err(),
            "general: unknown button 'guide'"
        );
    }

    #[test]
    fn keys_apps_and_pointer_are_checked() {
        assert_eq!(
            check(json!({"keyboard": {"back": "hyper+x"}})).unwrap_err(),
            "keyboard: unknown modifier 'hyper' in 'hyper+x'"
        );
        assert_eq!(
            check(json!({"keyboard": {"quick": "enter"}})).unwrap_err(),
            "keyboard: unknown action 'quick'"
        );
        assert_eq!(
            check(json!({"apps": {"jellyfin": {"back": "escape"}}})).unwrap_err(),
            "no keyboard app 'jellyfin'"
        );
        assert_eq!(
            check(json!({"apps": {"youtube": {"up": "w", "down": "nope"}}})).unwrap_err(),
            "apps.youtube: unknown key 'nope' in 'nope'"
        );
        let c = check(json!({"keyboard": {"back": "backspace", "view": "none"}})).unwrap();
        assert_eq!(c.keyboard[&NavAction::Back], Some(combo("backspace")));
        assert_eq!(c.keyboard[&NavAction::View], None);

        for speed in [0, 20, 30, 110, 425, 1000] {
            assert_eq!(
                check(json!({"pointer": {"speed": speed}})).unwrap_err(),
                "pointer: speed must be 25 to 400 in steps of 25",
                "{speed}"
            );
        }
        for speed in [25, 100, 375, 400] {
            assert_eq!(
                check(json!({"pointer": {"speed": speed}}))
                    .unwrap()
                    .tuning
                    .speed,
                speed
            );
        }
        assert_eq!(
            check(json!({"pointer": {"left_click": "lt"}})).unwrap_err(),
            "pointer: left and right click are both lt"
        );
        assert_eq!(
            check(json!({"pointer": {"right_click": "home"}})).unwrap_err(),
            "pointer: unknown button 'home'"
        );
        let c = check(
            json!({"pointer": {"left_click": "a", "right_click": "rt", "swap_sticks": true}}),
        )
        .unwrap();
        assert_eq!(c.click(ControllerButton::A), Some(MouseButton::Left));
        assert_eq!(c.click(ControllerButton::Rt), Some(MouseButton::Right));
        assert_eq!(c.click(ControllerButton::Lt), None);
        assert!(c.tuning.swap_sticks);
        // Unknown fields are refused, not dropped.
        assert!(serde_json::from_value::<ControllerSettings>(json!({"mouse": {}})).is_err());
    }

    fn keyboard_app(id: &str) -> AppConfig {
        let mut app: AppConfig = toml::from_str(&format!(
            "id = \"{id}\"\nname = \"{id}\"\ncommand = [\"x\"]\ninput = \"keyboard\""
        ))
        .unwrap();
        app.input = InputProfile::Keyboard;
        app
    }

    #[test]
    fn keymap_layers() {
        let mut app = keyboard_app("youtube");
        app.back_key = Some("backspace".into());
        app.keymap = BTreeMap::from([("context".into(), "f".into())]);
        let mut firefox = keyboard_app("firefox");
        firefox.address_bar = true;

        // Defaults < shared user < the app's own < per-app user.
        let c = check(json!({
            "keyboard": {"back": "escape", "context": "space", "menu": "m", "up": "w"},
            "apps": {"youtube": {"menu": "none", "up": "k"}}
        }))
        .unwrap();
        let map = c.keymap(&app).unwrap();
        assert_eq!(
            map[&NavAction::Back],
            combo("backspace"),
            "the app's own beats shared"
        );
        assert_eq!(map[&NavAction::Context], combo("f"));
        assert_eq!(map[&NavAction::Up], combo("k"), "per-app beats everything");
        assert!(!map.contains_key(&NavAction::Menu), "none unmaps");
        assert_eq!(map[&NavAction::Accept], combo("enter"));
        assert_eq!(c.app_mapped("youtube").collect::<Vec<_>>(), [NavAction::Up]);

        // The address bar's keys are the app's own.
        let map = c.keymap(&firefox).unwrap();
        assert_eq!(map[&NavAction::PageLeft], combo("alt+left"));
        assert_eq!(map[&NavAction::Back], combo("escape"));
        assert_eq!(map[&NavAction::Menu], combo("m"));
        assert_eq!(map[&NavAction::Up], combo("w"));

        // No settings: what the config alone gave.
        let c = Controller::default();
        assert_eq!(c.keymap(&app), app.keymap().unwrap());
        // Gamepad apps have none.
        let mut game = keyboard_app("game");
        game.input = InputProfile::Gamepad;
        assert_eq!(c.keymap(&game), None);

        // input.settings.get lists the app's own keys only.
        game.id = "game".into();
        let listed = keyboard_apps(&[app.clone(), game, firefox]);
        assert_eq!(
            serde_json::to_value(&listed).unwrap(),
            json!([
                {"id": "youtube", "name": "youtube", "keymap": {"back": "backspace", "context": "f"}},
                {"id": "firefox", "name": "firefox", "keymap": {"page_left": "alt+left", "page_right": "alt+right"}}
            ])
        );
    }

    #[test]
    fn store_loads_saves_and_keeps_entries_of_gone_apps() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state/hinteros/controller.json");
        // Nothing saved: the defaults.
        let mut store = ControllerStore::load(Some(path.clone()));
        assert_eq!(store.saved(), &ControllerSettings::default());
        assert_eq!(store.get(), &Controller::default());

        let s = settings(json!({
            "general": {"accept": "b", "back": "a"},
            "apps": {"youtube": {"back": "backspace"}},
            "pointer": {"speed": 150}
        }));
        store.set(s.clone(), &["youtube".into()]).unwrap();
        assert_eq!(store.get().action(ControllerButton::A), NavAction::Back);
        let again = ControllerStore::load(Some(path.clone()));
        assert_eq!(again.saved(), &s);
        assert_eq!(again.get(), store.get());

        // Refused: nothing saved, nothing changed.
        let bad = settings(json!({"general": {"accept": "b"}}));
        assert!(matches!(
            store.set(bad, &["youtube".into()]),
            Err(SetError::Invalid(_))
        ));
        assert_eq!(ControllerStore::load(Some(path.clone())).saved(), &s);
        assert_eq!(store.saved(), &s);

        // youtube was removed: its saved entry may come back unchanged, not changed.
        let mut keep = s.clone();
        keep.pointer = None;
        store.set(keep.clone(), &[]).unwrap();
        let mut changed = keep.clone();
        changed.apps = Some(BTreeMap::from([(
            "youtube".into(),
            BTreeMap::from([("back".into(), "escape".into())]),
        )]));
        assert!(matches!(store.set(changed, &[]), Err(SetError::Invalid(_))));
        // Dropping it is fine.
        store.set(ControllerSettings::default(), &[]).unwrap();
        let saved: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(saved, json!({"version": 1, "settings": {}}));
    }

    #[test]
    fn bad_file_is_the_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("controller.json");
        for text in [
            "not json",
            r#"{"general": {"accept": "a"}}"#,
            r#"{"version": 1, "settings": {"general": {"accept": "b"}}}"#,
            r#"{"version": 1, "settings": {"pointer": {"speed": 7}}}"#,
            r#"{"version": 1, "settings": {"extra": 1}}"#,
        ] {
            std::fs::write(&path, text).unwrap();
            let store = ControllerStore::load(Some(path.clone()));
            assert_eq!(store.saved(), &ControllerSettings::default(), "{text}");
            assert_eq!(store.get(), &Controller::default(), "{text}");
        }
        // An app that is not configured (now) is kept.
        std::fs::write(
            &path,
            r#"{"version": 1, "settings": {"apps": {"gone": {"back": "escape"}}}}"#,
        )
        .unwrap();
        let store = ControllerStore::load(Some(path.clone()));
        assert!(store.saved().apps.as_ref().unwrap().contains_key("gone"));
    }

    #[test]
    fn newer_file_is_the_defaults_and_stays() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("controller.json");
        let text = r#"{"version": 2, "layers": []}"#;
        std::fs::write(&path, text).unwrap();
        let mut store = ControllerStore::load(Some(path.clone()));
        assert_eq!(store.saved(), &ControllerSettings::default());
        let s = settings(json!({"pointer": {"speed": 150}}));
        store.set(s.clone(), &[]).unwrap();
        assert_eq!(store.saved(), &s);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
    }
}
