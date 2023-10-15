use derive_more::Display;
use serde::Deserialize;

use crate::config::mods::ModContainer;

#[derive(Debug, Clone, Deserialize)]
pub struct PackConfig {
    pub name: String,
    pub description: String,
    pub author: String,
    pub version: String,
    pub minecraft_version: String,
    pub mod_loader: ModLoader,
    pub mods: ModContainer,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ModLoader {
    pub id: ModLoaderType,
    pub version: String,
}

#[derive(Debug, Display, Clone, Eq, PartialEq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModLoaderType {
    #[display(fmt = "forge")]
    Forge,
    #[display(fmt = "neoforge")]
    Neoforge,
    #[display(fmt = "fabric")]
    Fabric,
    #[display(fmt = "quilt")]
    Quilt,
}
