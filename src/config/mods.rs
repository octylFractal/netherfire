use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::fmt::{Display, Formatter};
use std::hash::Hash;
use std::path::{Path, PathBuf};

use itertools::Itertools;
use once_cell::sync::Lazy;
use serde::Deserialize;
use thiserror::Error;
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;

use crate::mod_site::{
    CurseForge, ModDependencyKind, ModDownloadError, ModFileInfo, ModFileLoadingResult, ModId,
    ModLoadingError, ModSite, Modrinth,
};

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

#[derive(Debug, Error)]
pub enum ModVerificationError {
    #[error("Error loading mod: {0}")]
    Loading(#[from] ModLoadingError),
    #[error("The mod does not allow third-party distribution. Add it to `mods/`.")]
    DistributionDenied,
    #[error("Required dependencies are not specified in the mods list: {0:?}")]
    MissingRequiredDependencies(Vec<String>),
}

#[derive(Debug, Error)]
pub enum ModDownloadToFileError {
    #[error("I/O Error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Mod loading Error: {0}")]
    ModLoading(#[from] ModLoadingError),
    #[error("Mod download Error: {0}")]
    ModDownload(#[from] ModDownloadError),
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
    pub failures: HashMap<String, ModDownloadToFileError>,
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

impl ModConfig {
    pub(crate) async fn verify(&self) -> Result<(), ModsVerificationError> {
        let mut failures = HashMap::<String, ModVerificationError>::new();

        self.verify_mods_site(&mut failures, &self.mods.curseforge, CurseForge)
            .await;

        self.verify_mods_site(&mut failures, &self.mods.modrinth, Modrinth)
            .await;

        if !failures.is_empty() {
            return Err(ModsVerificationError { failures });
        }

        Ok(())
    }

    async fn verify_mods_site<K: Display + Eq + Hash + Clone + Send + Sync + 'static>(
        &self,
        failures: &mut HashMap<String, ModVerificationError>,
        mods: &HashMap<String, Mod<K>>,
        site: impl ModSite<Id = K> + Clone + Send + Sync + 'static,
    ) {
        let mut mods_by_id = HashSet::with_capacity(mods.len());
        let mut verifications = Vec::with_capacity(mods.len());
        for (k, m) in mods.iter().sorted_by_key(|(k, _)| k.to_string()) {
            mods_by_id.insert(m.source.project_id.clone());
            verifications.push((k.to_string(), submit_load(m.source.clone(), site.clone())));
        }
        for (cfg_id, verification_ftr) in verifications {
            match verification_ftr.await.expect("tokio failure") {
                Err(e) => {
                    failures.insert(cfg_id.clone(), e.into());
                }
                Ok(loaded_mod) => {
                    self.verify_mod(failures, &mods_by_id, cfg_id, loaded_mod, site.clone())
                        .await
                }
            }
        }
    }

    async fn verify_mod<K: Display + Eq + Hash>(
        &self,
        failures: &mut HashMap<String, ModVerificationError>,
        mods_by_id: &HashSet<K>,
        cfg_id: String,
        loaded_mod: ModFileInfo<K>,
        site: impl ModSite<Id = K>,
    ) {
        if !loaded_mod.project_info.distribution_allowed {
            failures.insert(cfg_id, ModVerificationError::DistributionDenied);
            return;
        }
        // Verify that all dependencies are specified.
        let mut missing_deps = Vec::new();
        for dep in loaded_mod.dependencies {
            match dep.kind {
                ModDependencyKind::Required => {
                    if !mods_by_id.contains(&dep.project_id) {
                        let dep_name = match site.load_metadata(dep.project_id).await {
                            Ok(v) => v.name,
                            Err(e) => {
                                failures.insert(cfg_id, e.into());
                                return;
                            }
                        };
                        missing_deps.push(dep_name);
                    }
                }
                ModDependencyKind::Optional => {
                    if !mods_by_id.contains(&dep.project_id) {
                        log::info!(
                            "[FYI] Missing optional dependency for {}: {}",
                            cfg_id,
                            dep.project_id
                        );
                    }
                }
                _ => {}
            };
        }
        if !missing_deps.is_empty() {
            failures.insert(
                cfg_id,
                ModVerificationError::MissingRequiredDependencies(missing_deps),
            );
            return;
        }
        log::info!(
            "Mod {} (in config: {}) verified.",
            loaded_mod.project_info.name,
            cfg_id
        );
    }

    pub(crate) async fn download<F>(
        &self,
        dest_dir: &Path,
        side_test: F,
    ) -> Result<(), ModsDownloadError>
    where
        F: FnMut(ModSide) -> bool + Clone,
    {
        let mut failures = HashMap::<String, ModDownloadToFileError>::new();

        Self::download_from_site(
            CurseForge,
            dest_dir,
            &mut failures,
            &self.mods.curseforge,
            side_test.clone(),
        )
        .await;
        Self::download_from_site(
            Modrinth,
            dest_dir,
            &mut failures,
            &self.mods.modrinth,
            side_test,
        )
        .await;

        if !failures.is_empty() {
            return Err(ModsDownloadError { failures });
        }

        Ok(())
    }

    async fn download_from_site<K, S, F>(
        site: S,
        dest_dir: &Path,
        failures: &mut HashMap<String, ModDownloadToFileError>,
        mods: &HashMap<String, Mod<K>>,
        mut side_test: F,
    ) where
        F: FnMut(ModSide) -> bool,
        K: Clone + Send + Sync + 'static,
        S: ModSite<Id = K> + Copy + Send + Sync + 'static,
    {
        let downloads = mods
            .iter()
            .filter(|(_, m)| side_test(m.side))
            .sorted_by_key(|(k, _)| k.to_string())
            .map(|(k, m)| {
                (
                    k.to_string(),
                    submit_download(m.source.clone(), site, dest_dir),
                )
            })
            .collect::<Vec<_>>();
        for (cfg_id, dl_ftr) in downloads {
            match dl_ftr.await.expect("tokio failure") {
                Err(e) => {
                    failures.insert(cfg_id.clone(), e);
                }
                Ok(dest) => {
                    log::info!(
                        "[{}] Mod {} downloaded to {}.",
                        S::NAME,
                        cfg_id,
                        dest.display()
                    );
                }
            }
        }
    }
}

fn submit_load<K: Send + Sync + 'static>(
    mod_id: ModId<K>,
    site: impl ModSite<Id = K> + Send + Sync + 'static,
) -> JoinHandle<ModFileLoadingResult<K>> {
    static CONCURRENCY_LIMITER: Lazy<Semaphore> = Lazy::new(|| Semaphore::new(30));

    tokio::task::spawn(async move {
        let _guard = CONCURRENCY_LIMITER.acquire().await.expect("tokio failure");
        site.load_file(mod_id).await
    })
}

fn submit_download<K, S>(
    mod_id: ModId<K>,
    site: S,
    dest_dir: &Path,
) -> JoinHandle<Result<PathBuf, ModDownloadToFileError>>
where
    K: Clone + Send + Sync + 'static,
    S: ModSite<Id = K> + Send + Sync + 'static,
{
    static CONCURRENCY_LIMITER: Lazy<Semaphore> = Lazy::new(|| Semaphore::new(10));

    let dest_dir = dest_dir.to_owned();
    tokio::task::spawn(async move {
        let _guard = CONCURRENCY_LIMITER.acquire().await.expect("tokio failure");
        let file_meta = site.load_file(mod_id.clone()).await?;
        let dest_file = dest_dir.join(&file_meta.filename);
        if let Some(hash) = file_meta.hash {
            if dest_file.exists() {
                // Check if we already have the file.
                let content = tokio::fs::read(&dest_file).await?;
                if hash.check(&content) {
                    log::debug!(
                        "[{}] Skipping {}, {} hashes matched",
                        S::NAME,
                        file_meta.filename,
                        hash.algo
                    );
                    return Ok(dest_file);
                }
            }
        }

        log::debug!(
            "[{}] Downloading {} to {}",
            S::NAME,
            file_meta.filename,
            dest_file.display()
        );

        tokio::io::copy(
            &mut site.download(mod_id).await?,
            &mut tokio::fs::File::create(&dest_file).await?,
        )
        .await?;

        Ok(dest_file)
    })
}

#[derive(Debug, Clone, Deserialize)]
pub struct Mod<K> {
    #[serde(flatten)]
    pub source: ModId<K>,
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
