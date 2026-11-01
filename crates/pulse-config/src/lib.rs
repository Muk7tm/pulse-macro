//! Pulse configuration: XDG paths, `config.toml`, schema check, validation
//! into a runnable clicker plan, and atomic persistence.
//!
//! There is exactly one set of clicker settings; Pulse has no profiles or
//! presets.

mod plan;

use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use pulse_core::{ActivationMode, BackendKind, Issue, Key, MouseButton, Pressable};
use serde::{Deserialize, Serialize};

pub use plan::{ClickPlan, plan};

/// The only `config.toml` format Pulse reads. Older (unreleased) and newer
/// schemas are refused, never guessed at.
pub const SCHEMA_VERSION: u32 = 2;

/// Largest config file Pulse will read.
pub const MAX_FILE_BYTES: u64 = 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("I/O error on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{path}: invalid TOML: {message}")]
    Parse { path: PathBuf, message: String },
    #[error("{path}: {message}")]
    Schema { path: PathBuf, message: String },
    #[error("{0}")]
    Invalid(String),
    #[error("cannot determine {0}; set the corresponding XDG variable or HOME")]
    NoDirectory(&'static str),
}

impl ConfigError {
    fn io(path: &Path, source: std::io::Error) -> Self {
        ConfigError::Io {
            path: path.to_owned(),
            source,
        }
    }
}

// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

/// Directory name under each XDG base directory. Not `pulse`: that name is
/// already used by PulseAudio/PipeWire (`~/.config/pulse/cookie`,
/// `$XDG_RUNTIME_DIR/pulse/native`), and sharing it would mix files with the
/// audio server (and let a config cleanup delete its cookie).
pub const DIR_NAME: &str = "pulse-macro";

/// XDG-conformant locations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Paths {
    pub config_dir: PathBuf,
    pub state_dir: PathBuf,
    pub runtime_dir: PathBuf,
}

impl Paths {
    pub fn from_env() -> Result<Paths, ConfigError> {
        let home = std::env::var_os("HOME").filter(|h| !h.is_empty()).map(PathBuf::from);
        let xdg = |var: &str| {
            std::env::var_os(var)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
        };
        let config_home = xdg("XDG_CONFIG_HOME")
            .or_else(|| home.as_ref().map(|h| h.join(".config")))
            .ok_or(ConfigError::NoDirectory("the config directory"))?;
        let state_home = xdg("XDG_STATE_HOME")
            .or_else(|| home.as_ref().map(|h| h.join(".local/state")))
            .ok_or(ConfigError::NoDirectory("the state directory"))?;
        // No fallback to /tmp: the control socket must live in a private,
        // per-user runtime directory.
        let runtime = xdg("XDG_RUNTIME_DIR").ok_or(ConfigError::NoDirectory("XDG_RUNTIME_DIR"))?;
        Ok(Paths {
            config_dir: config_home.join(DIR_NAME),
            state_dir: state_home.join(DIR_NAME),
            runtime_dir: runtime.join(DIR_NAME),
        })
    }

    /// Explicit roots (tests, `--config-dir`/`--runtime-dir` overrides).
    pub fn with_roots(config_dir: PathBuf, runtime_dir: PathBuf) -> Paths {
        Paths {
            state_dir: config_dir.join("state"),
            config_dir,
            runtime_dir,
        }
    }

    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }

    pub fn socket(&self) -> PathBuf {
        self.runtime_dir.join("engine.sock")
    }

    pub fn lock_file(&self) -> PathBuf {
        self.runtime_dir.join("engine.lock")
    }
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EngineSection {
    /// Input method; changed only through `select_backend` so a failed
    /// switch never changes the saved preference.
    pub backend: BackendKind,
    /// Permit intervals below 10 ms (down to 1 ms).
    pub allow_advanced_rates: bool,
    pub stop_on_suspend: bool,
}

impl Default for EngineSection {
    fn default() -> Self {
        EngineSection {
            backend: BackendKind::XTest,
            allow_advanced_rates: false,
            stop_on_suspend: true,
        }
    }
}

/// Whether the clicker repeats a mouse button or a keyboard key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputKind {
    Mouse,
    Key,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ClickerSection {
    /// `mouse` repeats `mouse_button`, `key` repeats `key`.
    pub input: InputKind,
    pub mouse_button: MouseButton,
    pub key: Key,
    /// Time between presses.
    pub interval_ms: u64,
    /// Press → release time.
    pub hold_ms: u64,
    pub activation_mode: ActivationMode,
    pub activation_hotkey: Key,
    /// Always active; stops clicking immediately.
    pub emergency_stop_hotkey: Key,
    /// Stop after this many clicks; 0 = until stopped.
    pub count: u64,
    /// Stop after this many milliseconds; 0 = no time limit.
    pub duration_ms: u64,
}

impl Default for ClickerSection {
    fn default() -> Self {
        ClickerSection {
            input: InputKind::Mouse,
            mouse_button: MouseButton::Left,
            key: Key::E,
            interval_ms: 100,
            hold_ms: 20,
            activation_mode: ActivationMode::Toggle,
            activation_hotkey: Key::F6,
            emergency_stop_hotkey: Key::F8,
            count: 0,
            duration_ms: 0,
        }
    }
}

impl ClickerSection {
    /// The input the clicker repeats.
    pub fn target(&self) -> Pressable {
        match self.input {
            InputKind::Mouse => Pressable::Button(self.mouse_button),
            InputKind::Key => Pressable::Key(self.key),
        }
    }

    pub fn set_target(&mut self, target: Pressable) {
        match target {
            Pressable::Button(b) => {
                self.input = InputKind::Mouse;
                self.mouse_button = b;
            }
            Pressable::Key(k) => {
                self.input = InputKind::Key;
                self.key = k;
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TargetSection {
    /// Click only while the selected game window is focused.
    pub require_focus: bool,
    /// Populated by user selection; never hardcoded.
    pub window_class: String,
    pub window_title_pattern: String,
}

impl Default for TargetSection {
    fn default() -> Self {
        TargetSection {
            require_focus: true,
            window_class: String::new(),
            window_title_pattern: String::new(),
        }
    }
}

impl TargetSection {
    pub fn rule(&self) -> pulse_core::TargetRule {
        pulse_core::TargetRule {
            window_class: self.window_class.clone(),
            window_title_pattern: self.window_title_pattern.clone(),
        }
    }

    pub fn set_rule(&mut self, rule: &pulse_core::TargetRule) {
        self.window_class = rule.window_class.clone();
        self.window_title_pattern = rule.window_title_pattern.clone();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Renderer {
    Auto,
    Software,
    Femtovg,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UiSection {
    /// Slint renderer; applies when the window opens.
    pub renderer: Renderer,
}

impl Default for UiSection {
    fn default() -> Self {
        UiSection {
            renderer: Renderer::Auto,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DiagnosticsSection {
    pub log_level: LogLevel,
}

impl Default for DiagnosticsSection {
    fn default() -> Self {
        DiagnosticsSection {
            log_level: LogLevel::Warn,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub schema_version: u32,
    #[serde(default)]
    pub engine: EngineSection,
    #[serde(default)]
    pub clicker: ClickerSection,
    #[serde(default)]
    pub target: TargetSection,
    #[serde(default)]
    pub ui: UiSection,
    #[serde(default)]
    pub diagnostics: DiagnosticsSection,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            schema_version: SCHEMA_VERSION,
            engine: EngineSection::default(),
            clicker: ClickerSection::default(),
            target: TargetSection::default(),
            ui: UiSection::default(),
            diagnostics: DiagnosticsSection::default(),
        }
    }
}

impl Config {
    pub fn from_toml_str(text: &str, path: &Path) -> Result<Config, ConfigError> {
        let table: toml::Table = toml::from_str(text).map_err(|e| ConfigError::Parse {
            path: path.to_owned(),
            message: e.to_string(),
        })?;
        check_schema_version(&table).map_err(|message| ConfigError::Schema {
            path: path.to_owned(),
            message,
        })?;
        toml::Value::Table(table)
            .try_into()
            .map_err(|e: toml::de::Error| ConfigError::Schema {
                path: path.to_owned(),
                message: e.to_string(),
            })
    }

    pub fn to_toml_string(&self) -> Result<String, ConfigError> {
        toml::to_string_pretty(self).map_err(|e| ConfigError::Invalid(format!("cannot serialize config: {e}")))
    }

    /// Load the config file. A missing file yields defaults.
    pub fn load(path: &Path) -> Result<Config, ConfigError> {
        match read_bounded(path) {
            Ok(text) => Config::from_toml_str(&text, path),
            Err(ConfigError::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
                Ok(Config::default())
            }
            Err(e) => Err(e),
        }
    }

    /// Validate fully (structure and clicker timing) and save atomically.
    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        if let Err(e) = plan(self) {
            let first = e.issues.iter().find(|i| i.is_error()).map(ToString::to_string);
            return Err(ConfigError::Invalid(first.unwrap_or_else(|| e.to_string())));
        }
        atomic_write(path, self.to_toml_string()?.as_bytes())
    }

    /// Structural checks. Clicker timing is validated by [`plan`], so a
    /// config with bad timing still loads and can be fixed from the UI.
    pub fn validate(&self) -> Vec<Issue> {
        let mut issues = Vec::new();
        if self.schema_version != SCHEMA_VERSION {
            issues.push(Issue::error("schema_version", format!("expected {SCHEMA_VERSION}")));
        }
        let c = &self.clicker;
        if c.activation_hotkey == c.emergency_stop_hotkey {
            issues.push(Issue::error(
                "clicker.emergency_stop_hotkey",
                "the emergency stop key must differ from the start/stop hotkey",
            ));
        }
        if c.target() == Pressable::Key(c.emergency_stop_hotkey) {
            issues.push(Issue::error(
                "clicker.key",
                format!(
                    "the clicker cannot repeat the emergency stop key {}",
                    c.emergency_stop_hotkey
                ),
            ));
        }
        if let Err(e) = self.target.rule().validate() {
            issues.push(Issue::error("target", e));
        }
        issues
    }
}

// ---------------------------------------------------------------------------
// File helpers
// ---------------------------------------------------------------------------

fn read_bounded(path: &Path) -> Result<String, ConfigError> {
    let meta = fs::metadata(path).map_err(|e| ConfigError::io(path, e))?;
    if meta.len() > MAX_FILE_BYTES {
        return Err(ConfigError::Invalid(format!(
            "{}: file exceeds {} bytes",
            path.display(),
            MAX_FILE_BYTES
        )));
    }
    fs::read_to_string(path).map_err(|e| ConfigError::io(path, e))
}

fn check_schema_version(table: &toml::Table) -> Result<(), String> {
    let current = i64::from(SCHEMA_VERSION);
    match table.get("schema_version") {
        Some(toml::Value::Integer(v)) if *v == current => Ok(()),
        Some(toml::Value::Integer(v)) if *v > current => Err(format!(
            "schema_version {v} was written by a newer Pulse (this build supports {SCHEMA_VERSION}); refusing to load"
        )),
        Some(toml::Value::Integer(v)) => Err(format!(
            "schema_version {v} is not supported (this build reads {SCHEMA_VERSION}); move the file away to start from defaults"
        )),
        Some(other) => Err(format!("schema_version must be an integer, found {other}")),
        None => Err("missing schema_version".into()),
    }
}

/// Create a private directory (0700) if missing.
pub fn ensure_private_dir(dir: &Path) -> Result<(), ConfigError> {
    fs::create_dir_all(dir).map_err(|e| ConfigError::io(dir, e))?;
    let meta = fs::metadata(dir).map_err(|e| ConfigError::io(dir, e))?;
    if meta.permissions().mode() & 0o077 != 0 {
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).map_err(|e| ConfigError::io(dir, e))?;
    }
    Ok(())
}

/// Atomic replace: write a temp file in the same directory, fsync, rename,
/// fsync the directory. Readers never observe a partial file.
pub fn atomic_write(path: &Path, data: &[u8]) -> Result<(), ConfigError> {
    let dir = path
        .parent()
        .ok_or_else(|| ConfigError::Invalid(format!("{} has no parent directory", path.display())))?;
    ensure_private_dir(dir)?;
    let file_name = path
        .file_name()
        .ok_or_else(|| ConfigError::Invalid(format!("{} has no file name", path.display())))?;
    let mut tmp_name = std::ffi::OsString::from(".");
    tmp_name.push(file_name);
    tmp_name.push(format!(".tmp-{}", std::process::id()));
    let tmp = dir.join(tmp_name);
    let result = (|| {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(|e| ConfigError::io(&tmp, e))?;
        f.write_all(data).map_err(|e| ConfigError::io(&tmp, e))?;
        f.sync_all().map_err(|e| ConfigError::io(&tmp, e))?;
        drop(f);
        fs::rename(&tmp, path).map_err(|e| ConfigError::io(path, e))?;
        fs::File::open(dir)
            .and_then(|d| d.sync_all())
            .map_err(|e| ConfigError::io(dir, e))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("pulse-config-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn xdg_directory_name_does_not_collide_with_pulseaudio() {
        let p = Paths::with_roots(PathBuf::from("/c"), PathBuf::from("/r"));
        assert_eq!(p.socket(), PathBuf::from("/r/engine.sock"));
        assert_ne!(DIR_NAME, "pulse");
    }

    #[test]
    fn example_config_parses() {
        let text = include_str!("../../../configs/example.toml");
        let cfg = Config::from_toml_str(text, Path::new("example.toml")).expect("parses");
        assert_eq!(cfg, Config::default(), "the example documents the defaults");
        assert!(cfg.validate().is_empty());
    }

    #[test]
    fn defaults_roundtrip_through_toml() {
        let mut cfg = Config::default();
        cfg.clicker
            .set_target(Pressable::Key(Key::parse("Space").expect("key")));
        let text = cfg.to_toml_string().expect("ser");
        let back = Config::from_toml_str(&text, Path::new("x")).expect("de");
        assert_eq!(cfg, back);
        assert_eq!(back.clicker.target(), Pressable::Key(Key::parse("Space").expect("key")));
    }

    #[test]
    fn unknown_fields_and_versions_are_rejected() {
        let e = Config::from_toml_str("schema_version = 2\n[engine]\nturbo = true\n", Path::new("x"));
        assert!(e.is_err());
        let e = Config::from_toml_str("schema_version = 99\n", Path::new("x")).expect_err("newer");
        assert!(e.to_string().contains("newer"), "{e}");
        for old in ["schema_version = 1\n", "schema_version = 0\n", "schema_version = -2\n"] {
            let e = Config::from_toml_str(old, Path::new("x")).expect_err(old);
            assert!(e.to_string().contains("not supported"), "{e}");
        }
        let e = Config::from_toml_str("schema_version = \"2\"\n", Path::new("x")).expect_err("string");
        assert!(e.to_string().contains("integer"), "{e}");
        let e = Config::from_toml_str("[engine]\n", Path::new("x")).expect_err("missing version");
        assert!(e.to_string().contains("schema_version"), "{e}");
        let e = Config::from_toml_str("schema_version = 2\n[clicker]\ninput = \"joystick\"\n", Path::new("x"));
        assert!(e.is_err());
    }

    #[test]
    fn validation_rejects_unsafe_hotkeys() {
        let mut cfg = Config::default();
        cfg.clicker.emergency_stop_hotkey = cfg.clicker.activation_hotkey;
        assert!(cfg.validate().iter().any(|i| i.path == "clicker.emergency_stop_hotkey"));
        let mut cfg = Config::default();
        cfg.clicker.set_target(Pressable::Key(Key::F8));
        assert!(cfg.validate().iter().any(|i| i.path == "clicker.key"));
        let d = tmpdir("reject");
        assert!(cfg.save(&d.join("config.toml")).is_err());
    }

    #[test]
    fn save_rejects_invalid_timing() {
        let mut cfg = Config::default();
        cfg.clicker.interval_ms = 5;
        cfg.clicker.hold_ms = 2;
        let d = tmpdir("timing");
        assert!(cfg.save(&d.join("config.toml")).is_err(), "below the standard minimum");
        cfg.engine.allow_advanced_rates = true;
        cfg.save(&d.join("config.toml")).expect("allowed with advanced rates");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn atomic_save_and_load() {
        let d = tmpdir("atomic");
        let path = d.join("pulse/config.toml");
        let mut cfg = Config::default();
        cfg.engine.backend = BackendKind::UInput;
        cfg.save(&path).expect("save");
        assert_eq!(Config::load(&path).expect("load").engine.backend, BackendKind::UInput);
        let mode = fs::metadata(&path).expect("meta").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let dmode = fs::metadata(path.parent().expect("parent"))
            .expect("meta")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dmode, 0o700);
        let leftovers: Vec<_> = fs::read_dir(path.parent().expect("parent"))
            .expect("read")
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp-"))
            .collect();
        assert!(leftovers.is_empty());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn missing_file_gives_defaults_and_oversize_is_rejected() {
        let d = tmpdir("missing");
        assert_eq!(Config::load(&d.join("nope.toml")).expect("defaults"), Config::default());
        fs::create_dir_all(&d).expect("mkdir");
        let big = d.join("big.toml");
        fs::write(&big, vec![b'#'; (MAX_FILE_BYTES + 1) as usize]).expect("write");
        assert!(Config::load(&big).is_err());
        let _ = fs::remove_dir_all(&d);
    }
}
