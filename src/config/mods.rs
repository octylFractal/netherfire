use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::fmt::{Display, Formatter};
use std::path::{Path, PathBuf};

use ferinth::structures::project_structs::Project as FerinthProject;
use ferinth::structures::project_structs::ProjectType;
use furse::structures::file_structs::{FileDependency, FileRelationType, HashAlgo};
use furse::structures::mod_structs::Mod as FurseMod;
use futures::TryStreamExt;
use itertools::Itertools;
use once_cell::sync::Lazy;
use serde::Deserialize;
use thiserror::Error;
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;
use tokio_util::compat::FuturesAsyncReadCompatExt;

use crate::config::global::{FERINTH, FURSE};
use crate::PackConfig;

#[derive(Debug, Clone, Deserialize)]
pub struct ModConfig {
    pub mods: ModContainer,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ModContainer {
    #[serde(default)]
    pub curseforge: HashMap<String, Mod<CurseForgeModSource>>,
    #[serde(default)]
    pub modrinth: HashMap<String, Mod<ModrinthModSource>>,
}

#[derive(Debug)]
pub struct ModsVerificationError {
    pub failures: HashMap<String, ModVerificationError>,
}

impl Error for ModsVerificationError {}

impl Display for ModsVerificationError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let mut failures_vec = self.failures.iter().collect::<Vec<_>>();
        failures_vec.sort_by_key(|(k, _)| (*k).clone());
        for (k, error) in failures_vec {
            writeln!(f, "Mod {}: {}", k, error)?;
        }

        Ok(())
    }
}

#[derive(Debug)]
pub struct ModsDownloadError {
    pub failures: HashMap<String, ModDownloadError>,
}

impl Error for ModsDownloadError {}

impl Display for ModsDownloadError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let mut failures_vec = self.failures.iter().collect::<Vec<_>>();
        failures_vec.sort_by_key(|(k, _)| (*k).clone());
        for (k, error) in failures_vec {
            writeln!(f, "Mod {}: {}", k, error)?;
        }

        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum ModVerificationError {
    #[error("The mod does not allow third-party distribution. Add it to `mods/`.")]
    DistributionDenied,
    #[error("The project exists, but is not a mod")]
    NotAMod,
    #[error("Required dependencies are not specified in the mods list: {0:?}")]
    MissingRequiredDependencies(Vec<i32>),
    #[error("CurseForge Error: {0}")]
    Furse(#[from] furse::Error),
    #[error("Modrinth Error: {0}")]
    Ferinth(#[from] ferinth::Error),
}

#[derive(Debug, Error)]
pub enum ModDownloadError {
    #[error("I/O Error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Reqwest Error: {0}")]
    Reqwest(#[from] reqwest::Error),
    #[error("CurseForge Error: {0}")]
    Furse(#[from] furse::Error),
    #[error("Modrinth Error: {0}")]
    Ferinth(#[from] ferinth::Error),
}

impl ModConfig {
    pub(crate) async fn verify(
        &self,
        pack_config: &PackConfig,
    ) -> Result<(), ModsVerificationError> {
        if !self.mods.modrinth.is_empty() {
            todo!("Modrinth can't be processed fully yet")
        }
        let mut failures = HashMap::<String, ModVerificationError>::new();
        let mods_by_id = self
            .mods
            .curseforge
            .values()
            .map(|v| v.source.file_id)
            .collect::<HashSet<_>>();
        let verifications = self
            .mods
            .curseforge
            .iter()
            .sorted_by_key(|(k, _)| k.to_string())
            .map(|(k, m)| (k.to_string(), submit_verify(m.source.clone(), pack_config)))
            .collect::<Vec<_>>();
        for (cfg_id, verification_ftr) in verifications {
            match verification_ftr.await.expect("tokio failure") {
                Err(e) => {
                    failures.insert(cfg_id.clone(), e);
                }
                Ok(loaded_mod) => {
                    // Verify that all dependencies are specified.
                    let mut missing_deps = Vec::new();
                    for dep in loaded_mod.dependencies {
                        match dep.relation_type {
                            FileRelationType::RequiredDependency => {
                                if !mods_by_id.contains(&dep.mod_id) {
                                    missing_deps.push(dep.mod_id);
                                }
                            }
                            FileRelationType::OptionalDependency => {
                                if !mods_by_id.contains(&dep.mod_id) {
                                    log::info!(
                                        "[FYI] Missing optional dependency for {}: {}",
                                        cfg_id,
                                        dep.mod_id
                                    );
                                }
                            }
                            _ => {}
                        };
                    }
                    if !missing_deps.is_empty() {
                        failures.insert(
                            cfg_id.clone(),
                            ModVerificationError::MissingRequiredDependencies(missing_deps),
                        );
                        continue;
                    }
                    log::info!(
                        "Mod {} (in config: {}) verified.",
                        loaded_mod.mod_.name,
                        cfg_id
                    );
                }
            }
        }

        if !failures.is_empty() {
            return Err(ModsVerificationError { failures });
        }

        Ok(())
    }

    pub(crate) async fn download<F>(
        &self,
        dest_dir: &Path,
        mut side_test: F,
    ) -> Result<(), ModsDownloadError>
    where
        F: FnMut(ModSide) -> bool,
    {
        let mut failures = HashMap::<String, ModDownloadError>::new();
        let downloads = self
            .mods
            .curseforge
            .iter()
            .filter(|(_, m)| side_test(m.side))
            .sorted_by_key(|(k, _)| k.to_string())
            .map(|(k, m)| (k.to_string(), submit_download(m.source.clone(), dest_dir)))
            .chain(
                self.mods
                    .modrinth
                    .iter()
                    .filter(|(_, m)| side_test(m.side))
                    .sorted_by_key(|(k, _)| k.to_string())
                    .map(|(k, m)| (k.to_string(), submit_download(m.source.clone(), dest_dir))),
            )
            .collect::<Vec<_>>();
        for (cfg_id, dl_ftr) in downloads {
            match dl_ftr.await.expect("tokio failure") {
                Err(e) => {
                    failures.insert(cfg_id.clone(), e);
                }
                Ok(dest) => {
                    log::info!("Mod {} downloaded to {}.", cfg_id, dest.display());
                }
            }
        }

        if !failures.is_empty() {
            return Err(ModsDownloadError { failures });
        }

        Ok(())
    }
}

fn submit_verify<SOURCE: ModSource + Send + Sync + 'static>(
    source: SOURCE,
    pack_config: &PackConfig,
) -> JoinHandle<ModVerificationResult> {
    static CONCURRENCY_LIMITER: Lazy<Semaphore> = Lazy::new(|| Semaphore::new(30));

    let pack_config = pack_config.clone();
    tokio::task::spawn(async move {
        let _guard = CONCURRENCY_LIMITER.acquire().await.expect("tokio failure");
        source.verify(&pack_config).await
    })
}

fn submit_download<SOURCE: ModSource + Send + Sync + 'static>(
    source: SOURCE,
    dest_dir: &Path,
) -> JoinHandle<ModDownloadResult> {
    static CONCURRENCY_LIMITER: Lazy<Semaphore> = Lazy::new(|| Semaphore::new(10));

    let dest_dir = dest_dir.to_owned();
    tokio::task::spawn(async move {
        let _guard = CONCURRENCY_LIMITER.acquire().await.expect("tokio failure");
        source.download(&dest_dir).await
    })
}

#[derive(Debug, Clone, Deserialize)]
pub struct Mod<SOURCE> {
    #[serde(flatten)]
    pub source: SOURCE,
    #[serde(default)]
    pub side: ModSide,
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

type ModVerificationResult = Result<ModVerificationInfo, ModVerificationError>;
type ModDownloadResult = Result<PathBuf, ModDownloadError>;

#[async_trait::async_trait]
trait ModSource {
    async fn verify(&self, pack_config: &PackConfig) -> ModVerificationResult;
    async fn download(&self, dest_dir: &Path) -> ModDownloadResult;
}

#[derive(Debug, Clone, Deserialize)]
pub struct CurseForgeModSource {
    pub project_id: i32,
    pub file_id: i32,
}

#[async_trait::async_trait]
impl ModSource for CurseForgeModSource {
    async fn verify(&self, _: &PackConfig) -> ModVerificationResult {
        let mod_id = self.project_id;
        let furse_mod = FURSE.get_mod(mod_id).await?;
        if furse_mod.allow_mod_distribution == Some(false) {
            return Err(ModVerificationError::DistributionDenied);
        }
        let file = FURSE.get_mod_file(mod_id, self.file_id).await?;
        Ok(ModVerificationInfo {
            mod_: furse_mod.into(),
            dependencies: file.dependencies,
        })
    }

    async fn download(&self, dest_dir: &Path) -> ModDownloadResult {
        let file_meta = FURSE.get_mod_file(self.project_id, self.file_id).await?;
        let dest_file = dest_dir.join(&file_meta.file_name);
        if !file_meta.hashes.is_empty() && dest_file.exists() {
            // Check if we already have the file.
            let content = tokio::fs::read(&dest_file).await?;
            if let Some(hash) = file_meta.hashes.iter().find(|h| h.algo == HashAlgo::Sha1) {
                if check_hash::<sha1::Sha1>(&content, &hash.value) {
                    log::debug!("Skipping {}, SHA1 hashes matched", file_meta.file_name);
                    return Ok(dest_file);
                }
            } else if let Some(hash) = file_meta.hashes.iter().find(|h| h.algo == HashAlgo::Md5) {
                if check_hash::<md5::Md5>(&content, &hash.value) {
                    log::debug!("Skipping {}, MD5 hashes matched", file_meta.file_name);
                    return Ok(dest_file);
                }
            }
        }

        log::debug!(
            "Downloading {} to {}",
            file_meta.file_name,
            dest_file.display()
        );

        let req = reqwest::get(file_meta.download_url.expect("verified earlier"))
            .await?
            .error_for_status()?;
        tokio::io::copy(
            &mut req
                .bytes_stream()
                .map_err(|e| futures::io::Error::new(futures::io::ErrorKind::Other, e))
                .into_async_read()
                .compat(),
            &mut tokio::fs::File::create(&dest_file).await?,
        )
        .await?;

        Ok(dest_file)
    }
}

fn check_hash<D: digest::Digest>(content: &[u8], expected_hash: &str) -> bool {
    let expected_hash = hex::decode(expected_hash)
        .unwrap_or_else(|e| panic!("hash ({}) problem: {}", expected_hash, e));
    let actual_hash = D::digest(content);
    actual_hash.as_slice() == expected_hash
}

#[derive(Debug, Clone, Deserialize)]
pub struct ModrinthModSource {
    project_id: String,
}

#[async_trait::async_trait]
impl ModSource for ModrinthModSource {
    async fn verify(&self, _: &PackConfig) -> ModVerificationResult {
        let ferinth_mod = FERINTH.get_project(&self.project_id).await?;
        if ferinth_mod.project_type != ProjectType::Mod {
            return Err(ModVerificationError::NotAMod);
        }

        todo!("Dependencies")
    }

    async fn download(&self, _dest_dir: &Path) -> ModDownloadResult {
        todo!("No Modrinth file download support yet")
    }
}

struct ModVerificationInfo {
    mod_: LoadedMod,
    dependencies: Vec<FileDependency>,
}

struct LoadedMod {
    name: String,
}

impl From<FurseMod> for LoadedMod {
    fn from(furse_mod: FurseMod) -> Self {
        Self {
            name: furse_mod.name,
        }
    }
}

impl From<FerinthProject> for LoadedMod {
    fn from(project: FerinthProject) -> Self {
        Self {
            name: project.title,
        }
    }
}
