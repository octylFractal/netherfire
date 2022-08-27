use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::fmt::{Debug, Display, Formatter};
use std::path::{Path, PathBuf};

use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use itertools::Itertools;
use once_cell::sync::Lazy;
use serde::Deserialize;
use thiserror::Error;
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;

use crate::mod_site::{
    CurseForge, DependencyId, ModDependencyKind, ModDownloadError, ModFileInfo,
    ModFileLoadingResult, ModId, ModIdValue, ModLoadingError, ModSite, Modrinth,
};
use crate::progress::{steady_tick_duration, style_bar};

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
        let multi = MultiProgress::new();

        let cf_verify = Self::submit_verify_site(CurseForge, &multi, &self.mods.curseforge);

        let modrinth_verify = Self::submit_verify_site(Modrinth, &multi, &self.mods.modrinth);

        let mut failures = cf_verify.await.expect("tokio error");
        for (k, v) in modrinth_verify.await.expect("tokio error").drain() {
            failures.insert(k, v);
        }

        if !failures.is_empty() {
            return Err(ModsVerificationError { failures });
        }

        Ok(())
    }

    fn submit_verify_site<S>(
        site: S,
        multi: &MultiProgress,
        mods: &HashMap<String, Mod<S::Id>>,
    ) -> JoinHandle<HashMap<String, ModVerificationError>>
    where
        S: ModSite,
    {
        let multi = multi.clone();
        let mods = mods.clone();
        tokio::spawn(async move {
            let mut failures = HashMap::<String, ModVerificationError>::new();
            Self::verify_mods_site(&mut failures, multi, mods, site).await;
            failures
        })
    }

    async fn verify_mods_site<K, S>(
        failures: &mut HashMap<String, ModVerificationError>,
        multi: MultiProgress,
        mods: HashMap<String, Mod<K>>,
        site: S,
    ) where
        K: ModIdValue,
        S: ModSite<Id = K> + Clone + Send + Sync + 'static,
    {
        let mut mods_by_project_id = HashSet::with_capacity(mods.len());
        let mut mods_by_version_id = HashSet::with_capacity(mods.len());
        let mut verifications = Vec::with_capacity(mods.len());
        for (k, m) in mods.into_iter().sorted_by_key(|(k, _)| k.to_string()) {
            mods_by_project_id.insert(m.source.project_id.clone());
            mods_by_version_id.insert(m.source.version_id.clone());
            // Include the ignored mods in the mods_by* tables to skip them.
            for ignored_mod in m.ignored_deps.iter() {
                match ignored_mod.clone() {
                    DependencyId::Project(project_id) => {
                        mods_by_project_id.insert(project_id);
                    }
                    DependencyId::Version(version_id) => {
                        mods_by_version_id.insert(version_id);
                    }
                }
            }

            let progress_bar = multi.add(ProgressBar::new_spinner().with_message(k.clone()));
            verifications.push((
                k,
                submit_load(progress_bar.clone(), m.source.clone(), site),
                progress_bar,
            ));
        }
        for (cfg_id, verification_ftr, progress_bar) in verifications {
            let failure = match verification_ftr.await.expect("tokio failure") {
                Err(e) => Err(e.into()),
                Ok(loaded_mod) => Self::verify_mod(
                    &mods_by_project_id,
                    &mods_by_version_id,
                    &cfg_id,
                    loaded_mod.clone(),
                    &site,
                )
                .await
                .map(|_| loaded_mod),
            };
            progress_bar.disable_steady_tick();
            match failure {
                Ok(mod_info) => {
                    progress_bar.finish_with_message(format!(
                        "[{}] Mod {} (in config: {}) verified.",
                        S::NAME,
                        mod_info.project_info.name,
                        cfg_id
                    ));
                }
                Err(failure) => {
                    progress_bar.finish_with_message(format!(
                        "[{}] Mod (in config: {}) FAILED verification.",
                        S::NAME,
                        cfg_id
                    ));
                    failures.insert(cfg_id, failure);
                }
            }
        }
    }

    async fn verify_mod<K, S>(
        mods_by_project_id: &HashSet<K>,
        mods_by_version_id: &HashSet<K>,
        cfg_id: &str,
        loaded_mod: ModFileInfo<K>,
        site: &S,
    ) -> Result<(), ModVerificationError>
    where
        K: ModIdValue,
        S: ModSite<Id = K>,
    {
        if !loaded_mod.project_info.distribution_allowed {
            return Err(ModVerificationError::DistributionDenied);
        }
        // Verify that all dependencies are specified.
        let mut missing_deps = Vec::new();
        for dep in loaded_mod.dependencies {
            match dep.kind {
                ModDependencyKind::Required => {
                    if let Some(v) = Self::get_dep_name_if_missing(
                        site,
                        dep.id.clone(),
                        mods_by_project_id,
                        mods_by_version_id,
                    )
                    .await?
                    {
                        missing_deps.push(format!("{} ({:?})", v, dep.id));
                    }
                }
                ModDependencyKind::Optional => {
                    if let Some(v) = Self::get_dep_name_if_missing(
                        site,
                        dep.id.clone(),
                        mods_by_project_id,
                        mods_by_version_id,
                    )
                    .await?
                    {
                        log::info!(
                            "[{}] [FYI] Missing optional dependency for {}: {} ({:?})",
                            S::NAME,
                            cfg_id,
                            v,
                            dep.id,
                        );
                    }
                }
                _ => {}
            };
        }
        if !missing_deps.is_empty() {
            return Err(ModVerificationError::MissingRequiredDependencies(
                missing_deps,
            ));
        }

        Ok(())
    }

    async fn get_dep_name_if_missing<K, S>(
        site: &S,
        id: DependencyId<K>,
        mods_by_project_id: &HashSet<K>,
        mods_by_version_id: &HashSet<K>,
    ) -> Result<Option<String>, ModVerificationError>
    where
        K: ModIdValue,
        S: ModSite<Id = K>,
    {
        match id {
            DependencyId::Project(project_id) => {
                if !(mods_by_project_id.contains(&project_id)) {
                    site.load_metadata(project_id)
                        .await
                        .map(|v| Some(v.name))
                        .map_err(Into::into)
                } else {
                    Ok(None)
                }
            }
            DependencyId::Version(version_id) => {
                if !mods_by_version_id.contains(&version_id) {
                    site.load_metadata_by_version(version_id).await
                        .expect("sites that provide only a version in dependencies must allow lookup by version")
                        .map(|v| Some(v.name))
                        .map_err(Into::into)
                } else {
                    Ok(None)
                }
            }
        }
    }

    pub(crate) async fn download<F>(
        &self,
        multi: MultiProgress,
        dest_dir: &Path,
        side_test: F,
    ) -> Result<(), ModsDownloadError>
    where
        F: FnMut(ModSide) -> bool + Clone,
    {
        let mut failures = HashMap::<String, ModDownloadToFileError>::new();

        Self::download_from_site(
            CurseForge,
            multi.clone(),
            dest_dir,
            &mut failures,
            &self.mods.curseforge,
            side_test.clone(),
        )
        .await;
        Self::download_from_site(
            Modrinth,
            multi.clone(),
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
        multi: MultiProgress,
        dest_dir: &Path,
        failures: &mut HashMap<String, ModDownloadToFileError>,
        mods: &HashMap<String, Mod<K>>,
        mut side_test: F,
    ) where
        F: FnMut(ModSide) -> bool,
        K: ModIdValue,
        S: ModSite<Id = K>,
    {
        let downloads = mods
            .iter()
            .filter(|(_, m)| side_test(m.side))
            .sorted_by_key(|(k, _)| k.as_str())
            .map(|(k, m)| {
                (
                    k.clone(),
                    submit_download(multi.clone(), k.clone(), m.source.clone(), site, dest_dir),
                )
            })
            .collect::<Vec<_>>();
        for (cfg_id, dl_ftr) in downloads {
            if let Err(e) = dl_ftr.await.expect("tokio failure") {
                failures.insert(cfg_id.clone(), e);
            }
        }
        multi.clear().expect("bar cleared");
    }
}

fn submit_load<K>(
    progress_bar: ProgressBar,
    mod_id: ModId<K>,
    site: impl ModSite<Id = K>,
) -> JoinHandle<ModFileLoadingResult<K>>
where
    K: ModIdValue,
{
    static CONCURRENCY_LIMITER: Lazy<Semaphore> = Lazy::new(|| Semaphore::new(30));

    tokio::task::spawn(async move {
        let _guard = CONCURRENCY_LIMITER.acquire().await.expect("tokio failure");
        progress_bar.enable_steady_tick(steady_tick_duration());
        site.load_file(mod_id).await
    })
}

fn submit_download<K, S>(
    multi: MultiProgress,
    cfg_id: String,
    mod_id: ModId<K>,
    site: S,
    dest_dir: &Path,
) -> JoinHandle<Result<PathBuf, ModDownloadToFileError>>
where
    K: ModIdValue,
    S: ModSite<Id = K>,
{
    static CONCURRENCY_LIMITER: Lazy<Semaphore> = Lazy::new(|| Semaphore::new(5));

    let dest_dir = dest_dir.to_owned();
    tokio::task::spawn(async move {
        let progress_bar = multi.add(ProgressBar::new_spinner().with_message(cfg_id.to_string()));
        let _guard = CONCURRENCY_LIMITER.acquire().await.expect("tokio failure");
        progress_bar.enable_steady_tick(steady_tick_duration());
        let file_meta = site.load_file(mod_id.clone()).await?;
        let dest_file = dest_dir.join(&file_meta.filename);
        if let Some(hash) = file_meta.hash {
            if dest_file.exists() {
                // Check if we already have the file.
                let content = tokio::fs::read(&dest_file).await?;
                if hash.check(&content) {
                    progress_bar.disable_steady_tick();
                    progress_bar.finish_with_message(format!(
                        "[{}] Found cached {}",
                        S::NAME,
                        file_meta.filename
                    ));
                    multi.remove(&progress_bar);
                    return Ok(dest_file);
                }
            }
        }

        progress_bar.disable_steady_tick();
        progress_bar.set_style(style_bar());
        progress_bar.set_length(file_meta.file_length);
        progress_bar.set_message(format!("[{}] {}", S::NAME, file_meta.filename));

        tokio::io::copy(
            &mut progress_bar.wrap_async_read(site.download(mod_id).await?),
            &mut tokio::fs::File::create(&dest_file).await?,
        )
        .await?;

        progress_bar.reset();
        progress_bar.set_style(ProgressStyle::default_spinner());
        progress_bar.finish_with_message(format!(
            "[{}] Downloaded {}",
            S::NAME,
            file_meta.filename
        ));

        Ok(dest_file)
    })
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
