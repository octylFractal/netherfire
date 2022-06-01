use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct PackConfig {
    pub name: String,
    pub description: String,
    pub author: String,
    pub version: String,
    pub minecraft_version: String,
    pub mod_loader: String,
}
