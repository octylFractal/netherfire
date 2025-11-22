use directories::ProjectDirs;
use ferinth::Ferinth;
use furse::Furse;
use once_cell::sync::Lazy;
use serde::Deserialize;

pub static DIRS: Lazy<ProjectDirs> = Lazy::new(|| {
    ProjectDirs::from("net.octyl", "Octavia Togami", "netherfire")
        .expect("Couldn't load project directories")
});

pub static CONFIG: Lazy<GlobalConfig> = Lazy::new(|| {
    let config_file = DIRS.config_dir().join("config.toml");
    let config_text = std::fs::read_to_string(&config_file)
        .unwrap_or_else(|e| panic!("Failed to read {}: {}", config_file.display(), e));
    toml_edit::de::from_str(&config_text)
        .unwrap_or_else(|e| panic!("Failed to parse {}: {}", config_file.display(), e))
});

pub static FURSE: Lazy<Furse> = Lazy::new(|| Furse::new(&CONFIG.curse_forge_api_key));
pub static FERINTH: Lazy<Ferinth<()>> = Lazy::new(|| {
    Ferinth::<()>::new(
        env!("CARGO_CRATE_NAME"),
        Some(env!("CARGO_PKG_VERSION")),
        Some("Octavia Togami"),
    )
});

#[derive(Debug, Clone, Deserialize)]
pub struct GlobalConfig {
    pub curse_forge_api_key: String,
}
