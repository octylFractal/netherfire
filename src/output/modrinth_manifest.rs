use serde::Serialize;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModrinthManifest {
    pub format_version: u32,
    pub game: Game,
    pub version_id: String,
    pub name: String,
    pub summary: Option<String>,
    pub files: Vec<ModFile>,
    pub dependencies: GameDependencies,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Game {
    Minecraft,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModFile {
    pub path: String,
    pub hashes: ModFileHashes,
    pub env: Option<Environment>,
    pub downloads: Vec<String>,
    pub file_size: u64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModFileHashes {
    pub sha1: String,
    pub sha512: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Environment {
    pub client: EnvRequirement,
    pub server: EnvRequirement,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum EnvRequirement {
    Required,
    Unsupported,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "kebab-case")]
pub struct GameDependencies {
    pub minecraft: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub forge: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub neoforge: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fabric_loader: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quilt_loader: Option<String>,
}

