//! App-wide state shared by the CLI, the GUI and the network sessions.

use std::sync::{Arc, Mutex, OnceLock};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::secure::{Identity, PairedStore, config_dir};

/// When the display keeps its screen from sleeping.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeepAwake {
    /// While a host is streaming to this display (like a playing video).
    #[default]
    WhileConnected,
    /// While the host's picture keeps changing; after `idle_minutes` of a
    /// still picture the display's normal sleep timeout takes over.
    WhileActive,
    /// Always follow the system's power settings.
    Never,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub name: Option<String>,
    pub keep_awake: KeepAwake,
    /// For [`KeepAwake::WhileActive`].
    pub idle_minutes: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            name: None,
            keep_awake: KeepAwake::default(),
            idle_minutes: 10,
        }
    }
}

pub struct Ctx {
    pub identity: Identity,
    pub paired: PairedStore,
    settings: Mutex<Settings>,
    /// `--name` for this run only, not saved.
    name_override: Mutex<Option<String>>,
}

impl Ctx {
    pub fn load() -> Result<Arc<Self>> {
        let settings: Settings = std::fs::read(config_dir().join("settings.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Ok(Arc::new(Ctx {
            identity: Identity::load_or_create()?,
            paired: PairedStore::load(),
            settings: Mutex::new(settings),
            name_override: Mutex::new(None),
        }))
    }

    /// The name other devices see.
    pub fn name(&self) -> String {
        if let Some(n) = self.name_override.lock().unwrap().clone() {
            return n;
        }
        self.settings
            .lock()
            .unwrap()
            .name
            .clone()
            .unwrap_or_else(hostname)
    }

    /// Changes and persists the device name.
    pub fn set_name(&self, name: &str) {
        let name = name.trim();
        if name.is_empty() {
            return;
        }
        *self.name_override.lock().unwrap() = None;
        self.update_settings(|s| s.name = Some(name.to_owned()));
    }

    /// Overrides the name for this run only (CLI `--name`).
    pub fn override_name(&self, name: &str) {
        *self.name_override.lock().unwrap() = Some(name.to_owned());
    }

    pub fn settings(&self) -> Settings {
        self.settings.lock().unwrap().clone()
    }

    /// Changes and persists settings.
    pub fn update_settings(&self, f: impl FnOnce(&mut Settings)) {
        let mut s = self.settings.lock().unwrap();
        f(&mut s);
        let r = std::fs::create_dir_all(config_dir()).and_then(|_| {
            std::fs::write(
                config_dir().join("settings.json"),
                serde_json::to_vec_pretty(&*s).unwrap(),
            )
        });
        if let Err(e) = r {
            tracing::warn!("could not save settings: {e}");
        }
    }
}

static DEFAULT_NAME: OnceLock<String> = OnceLock::new();

/// The name to use until the user picks one (Android passes the device
/// model, since its hostname is just "localhost").
pub fn set_default_name(name: &str) {
    let _ = DEFAULT_NAME.set(name.to_owned());
}

pub fn hostname() -> String {
    if let Some(n) = DEFAULT_NAME.get() {
        return n.clone();
    }
    gethostname::gethostname().to_string_lossy().into_owned()
}

static WAKER: OnceLock<Box<dyn Fn() + Send + Sync>> = OnceLock::new();

/// Lets background threads ask the GUI to redraw (no-op without a GUI).
pub fn set_waker(f: impl Fn() + Send + Sync + 'static) {
    let _ = WAKER.set(Box::new(f));
}

pub fn wake_ui() {
    if let Some(w) = WAKER.get() {
        w();
    }
}
