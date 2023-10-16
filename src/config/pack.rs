use derive_more::Display;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackConfig<MC> {
    pub name: String,
    pub description: String,
    pub author: String,
    pub version: String,
    pub minecraft_version: String,
    pub mod_loader: ModLoader,
    pub mods: MC,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
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
