use std::collections::HashMap;
use std::fmt::Debug;

use serde::Deserialize;

use crate::mod_site::{DependencyId, ModId, ModIdValue};

#[derive(Debug, Clone, Deserialize)]
pub struct ModConfig {
    pub mods: ModContainer,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ModContainer {
    #[serde(default)]
    pub curseforge: HashMap<String, Mod<i32>>,
    #[serde(default)]
    pub modrinth: HashMap<String, Mod<String>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Mod<K: ModIdValue> {
    #[serde(flatten)]
    pub source: ModId<K>,
    #[serde(default)]
    pub side: ModSide,
    /// Dependencies to ignore when validating.
    #[serde(default)]
    pub ignored_deps: Vec<DependencyId<K>>,
}

#[derive(Debug, Copy, Clone, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ModSide {
    Both,
    Client,
    Server,
}

impl ModSide {
    pub fn on_client(self) -> bool {
        self != Self::Server
    }

    pub fn on_server(self) -> bool {
        self != Self::Client
    }
}

impl Default for ModSide {
    fn default() -> Self {
        Self::Both
    }
}
