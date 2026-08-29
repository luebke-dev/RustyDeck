//! YAML configuration from `~/.config/rustydeck/config.yaml`.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub device: DeviceConfig,
    #[serde(default)]
    pub defaults: Style,
    /// Page shown at startup. Without one: `main`, otherwise the first page in
    /// the file.
    #[serde(default)]
    pub start_page: Option<String>,
    pub pages: BTreeMap<String, Page>,

    /// Directory holding the config file — the base for relative icon paths.
    #[serde(skip)]
    pub base_dir: PathBuf,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct DeviceConfig {
    /// Serial number, needed once more than one deck is attached.
    #[serde(default)]
    pub serial: Option<String>,
    #[serde(default = "default_brightness")]
    pub brightness: u8,
}

fn default_brightness() -> u8 {
    60
}

#[derive(Debug, Deserialize, Default, Clone)]
#[serde(deny_unknown_fields)]
pub struct Style {
    /// Background colour: `#rgb`, `#rrggbb`, or a name such as `black`.
    pub background: Option<String>,
    /// Text colour.
    pub color: Option<String>,
    /// Colour of a named `mdi:` icon. Without one, `color` is used.
    pub icon_color: Option<String>,
    /// Path to a TTF/OTF file. Without one, a system font is looked up.
    pub font: Option<String>,
    pub font_size: Option<f32>,
    /// Padding around the icon, in pixels.
    pub padding: Option<u32>,
    /// Brighten the key while it is held down.
    pub press_feedback: Option<bool>,
}

impl Style {
    /// `self` wins; missing values come from `fallback`.
    pub fn merged(&self, fallback: &Style) -> Style {
        Style {
            background: self
                .background
                .clone()
                .or_else(|| fallback.background.clone()),
            color: self.color.clone().or_else(|| fallback.color.clone()),
            icon_color: self
                .icon_color
                .clone()
                .or_else(|| fallback.icon_color.clone()),
            font: self.font.clone().or_else(|| fallback.font.clone()),
            font_size: self.font_size.or(fallback.font_size),
            padding: self.padding.or(fallback.padding),
            press_feedback: self.press_feedback.or(fallback.press_feedback),
        }
    }
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Page {
    #[serde(default, flatten)]
    pub style: Style,
    #[serde(default)]
    pub buttons: BTreeMap<u8, Button>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Button {
    pub label: Option<String>,
    /// Either a named icon (`mdi:volume-high`) or an image path (PNG, JPEG,
    /// GIF, BMP, WebP) relative to the config file.
    pub icon: Option<String>,
    #[serde(default, flatten)]
    pub style: Style,
    #[serde(default)]
    pub action: Option<Action>,
    /// Makes the key follow some state of the system, such as whether audio is
    /// muted.
    #[serde(default)]
    pub state: Option<StateSpec>,
}

/// A command whose output decides how the key looks right now.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateSpec {
    /// Command asked for the current state. A string goes through `sh -c`, a
    /// list is run directly. It should be cheap — it runs on every poll.
    pub run: Command,
    /// Seconds between polls.
    #[serde(default = "default_interval")]
    pub interval: f32,
    /// Checked top to bottom; the first case that matches wins. A case without
    /// any condition always matches and belongs last. Leaving `cases` out
    /// gives a single catch-all case — enough when the state only feeds a
    /// template, as in `label: \"{{ stdout }}\"`.
    #[serde(default = "catch_all_case")]
    pub cases: Vec<StateCase>,
}

fn default_interval() -> f32 {
    5.0
}

fn catch_all_case() -> Vec<StateCase> {
    vec![StateCase::default()]
}

/// One state of a key: a condition plus how the key looks while it holds.
#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct StateCase {
    /// Matches when the command's output contains this text.
    pub contains: Option<String>,
    /// Matches when the trimmed output equals this text.
    pub equals: Option<String>,
    /// Matches when the command exited with this status.
    pub exit: Option<i32>,

    /// Overrides the key's own label, icon, style and action while this case
    /// is the active one.
    pub label: Option<String>,
    pub icon: Option<String>,
    #[serde(default, flatten)]
    pub style: Style,
    #[serde(default)]
    pub action: Option<Action>,
}

impl StateCase {
    /// Does this case match the outcome of the state command?
    pub fn matches(&self, stdout: &str, exit_code: Option<i32>) -> bool {
        if let Some(needle) = &self.contains
            && !stdout.contains(needle.as_str())
        {
            return false;
        }
        if let Some(expected) = &self.equals
            && stdout.trim() != expected.trim()
        {
            return false;
        }
        if let Some(expected) = self.exit
            && exit_code != Some(expected)
        {
            return false;
        }
        true
    }

    /// True for a case that carries no condition and therefore always matches.
    pub fn is_catch_all(&self) -> bool {
        self.contains.is_none() && self.equals.is_none() && self.exit.is_none()
    }
}

/// What a key press does. In the YAML it is a single key each: `run:`,
/// `page:`, `brightness:` — or plainly `back` / `reload`.
#[derive(Debug, Clone)]
pub enum Action {
    /// Run a command: a string goes through `sh -c`, a list is started
    /// directly, without a shell.
    Run(Command),
    /// Switch to another page.
    Page(String),
    /// Go back to the previous page.
    Back,
    /// Set the brightness (`60`) or change it (`"+10"`, `"-10"`).
    Brightness(Brightness),
    /// Re-read the configuration.
    Reload,
}

const ACTION_KEYS: &str = "run, page, brightness, back, reload";

fn action_value<T, E>(key: &str, value: serde_yaml_ng::Value) -> Result<T, E>
where
    T: serde::de::DeserializeOwned,
    E: serde::de::Error,
{
    serde_yaml_ng::from_value(value)
        .map_err(|e| E::custom(format!("value for `{key}` does not fit: {e}")))
}

// Hand-written, because serde would otherwise demand YAML tags (`!run`) for
// enums — in a hand-edited config you want to write `action: {run: ...}`.
impl<'de> Deserialize<'de> for Action {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error as _;
        use serde_yaml_ng::Value;

        let value = Value::deserialize(deserializer)?;
        match value {
            Value::String(word) => match word.as_str() {
                "back" => Ok(Action::Back),
                "reload" => Ok(Action::Reload),
                other => Err(D::Error::custom(format!(
                    "unknown action `{other}` (allowed: {ACTION_KEYS})"
                ))),
            },
            Value::Mapping(map) => {
                if map.len() != 1 {
                    return Err(D::Error::custom(format!(
                        "an action needs exactly one key ({ACTION_KEYS}), found: {}",
                        map.len()
                    )));
                }
                let (key, value) = map.into_iter().next().expect("length is 1");
                let key = key
                    .as_str()
                    .ok_or_else(|| D::Error::custom("action name must be a string"))?
                    .to_string();

                match key.as_str() {
                    "run" => Ok(Action::Run(action_value(&key, value)?)),
                    "page" => Ok(Action::Page(action_value(&key, value)?)),
                    "brightness" => Ok(Action::Brightness(action_value(&key, value)?)),
                    other => Err(D::Error::custom(format!(
                        "unknown action `{other}` (allowed: {ACTION_KEYS})"
                    ))),
                }
            }
            other => Err(D::Error::custom(format!(
                "an action must be a string or a mapping, not {other:?}"
            ))),
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
#[serde(untagged)]
pub enum Command {
    Shell(String),
    Argv(Vec<String>),
}

#[derive(Debug, Deserialize, Clone)]
#[serde(untagged)]
pub enum Brightness {
    Absolute(i16),
    Relative(String),
}

impl Config {
    pub fn path() -> PathBuf {
        if let Ok(dir) = std::env::var("RUSTYDECK_CONFIG") {
            return PathBuf::from(dir);
        }
        config_dir().join("config.yaml")
    }

    pub fn load(path: &Path) -> Result<Config> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read configuration {}", path.display()))?;
        let mut cfg: Config = serde_yaml_ng::from_str(&text)
            .with_context(|| format!("{} is not valid YAML", path.display()))?;
        cfg.base_dir = path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<()> {
        if self.pages.is_empty() {
            bail!("the configuration contains no pages (`pages:`)");
        }
        if let Some(start) = &self.start_page
            && !self.pages.contains_key(start)
        {
            bail!("start_page `{start}` is not a defined page");
        }
        for (name, page) in &self.pages {
            for (idx, button) in &page.buttons {
                let mut actions = vec![button.action.as_ref()];
                if let Some(state) = &button.state {
                    if state.cases.is_empty() {
                        bail!(
                            "page `{name}`, key {idx}: `cases` is empty — leave it out for a \
                             single catch-all case"
                        );
                    }
                    if state.interval <= 0.0 {
                        bail!("page `{name}`, key {idx}: `interval` must be greater than zero");
                    }
                    actions.extend(state.cases.iter().map(|case| case.action.as_ref()));
                }

                for action in actions.into_iter().flatten() {
                    if let Action::Page(target) = action
                        && !self.pages.contains_key(target)
                    {
                        bail!("page `{name}`, key {idx}: `page: {target}` does not exist");
                    }
                }
            }
        }
        Ok(())
    }

    /// Name of the start page.
    pub fn start_page(&self) -> String {
        if let Some(p) = &self.start_page {
            return p.clone();
        }
        if self.pages.contains_key("main") {
            return "main".to_string();
        }
        self.pages.keys().next().cloned().unwrap_or_default()
    }

    /// Resolve an icon path: expand `~`, anchor relative paths at the config
    /// file.
    pub fn resolve_path(&self, raw: &str) -> PathBuf {
        let expanded = expand_tilde(raw);
        if expanded.is_absolute() {
            expanded
        } else {
            self.base_dir.join(expanded)
        }
    }
}

pub fn config_dir() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME")
        && !xdg.is_empty()
    {
        return PathBuf::from(xdg).join("rustydeck");
    }
    home().join(".config/rustydeck")
}

pub fn home() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/"))
}

pub fn expand_tilde(raw: &str) -> PathBuf {
    if raw == "~" {
        return home();
    }
    if let Some(rest) = raw.strip_prefix("~/") {
        return home().join(rest);
    }
    PathBuf::from(raw)
}
