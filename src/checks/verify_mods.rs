use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::fmt::{Debug, Display, Formatter};

use itertools::Itertools;
use once_cell::sync::Lazy;
use thiserror::Error;
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;

use crate::config::mods::Mod;
use crate::config::pack::PackConfig;
use crate::mod_site::{
    CurseForge, DependencyId, ModDependencyKind, ModFileInfo, ModFileLoadingResult, ModId,
    ModIdValue, ModLoadingError, ModSite, Modrinth,
};
use crate::uwu_colors::{
    ErrStyle, CONFIG_VAL_STYLE, SITE_NAME_STYLE, SITE_VAL_STYLE, SUCCESS_STYLE,
};

#[derive(Debug, Error)]
pub enum ModVerificationError {
    #[error("Error loading mod: {0}")]
    Loading(#[from] ModLoadingError),
    #[error("The mod does not allow third-party distribution. Add it to `mods/`.")]
    DistributionDenied,
    #[error("Required dependencies are not specified in the mods list: {0:?}")]
    MissingRequiredDependencies(Vec<String>),
    #[error("Expected Minecraft version {expected}, but got {actual:?}")]
    MinecraftVersionMismatch {
        expected: String,
        actual: Vec<String>,
    },
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

pub(crate) async fn verify_mods(pack_config: &PackConfig) -> Result<(), ModsVerificationError> {
    let cf_verify = submit_verify_site(
        &pack_config.minecraft_version,
        CurseForge,
        &pack_config.mods.curseforge,
    );

    let modrinth_verify = submit_verify_site(
        &pack_config.minecraft_version,
        Modrinth,
        &pack_config.mods.modrinth,
    );

    let mut failures = cf_verify.await.expect("tokio error");
    for (k, v) in modrinth_verify.await.expect("tokio error").drain() {
        failures.insert(k, v);
    }

    if !failures.is_empty() {
        return Err(ModsVerificationError { failures });
    }

    log::info!("{}", "Verified mods successfully.".errstyle(SUCCESS_STYLE));

    Ok(())
}

fn submit_verify_site<S>(
    minecraft_version: &str,
    site: S,
    mods: &HashMap<String, Mod<S::Id>>,
) -> JoinHandle<HashMap<String, ModVerificationError>>
where
    S: ModSite,
    S::ModHash: Clone + Send + Sync + 'static,
{
    let mods = mods.clone();
    let minecraft_version = minecraft_version.to_string();
    tokio::spawn(async move {
        let mut failures = HashMap::<String, ModVerificationError>::new();
        verify_mods_site(&minecraft_version, &mut failures, mods, site).await;
        failures
    })
}

async fn verify_mods_site<K, S>(
    minecraft_version: &String,
    failures: &mut HashMap<String, ModVerificationError>,
    mods: HashMap<String, Mod<K>>,
    site: S,
) where
    K: ModIdValue,
    S: ModSite<Id = K>,
    S::ModHash: Clone + Send + Sync + 'static,
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

        verifications.push((k, submit_load(m.source.clone(), site)));
    }
    for (cfg_id, verification_ftr) in verifications {
        let failure = match verification_ftr.await.expect("tokio failure") {
            Err(e) => Err(e.into()),
            Ok(loaded_mod) => verify_mod(
                minecraft_version,
                &mods_by_project_id,
                &mods_by_version_id,
                &cfg_id,
                loaded_mod.clone(),
                &site,
            )
            .await
            .map(|_| loaded_mod),
        };
        match failure {
            Ok(mod_info) => {
                log::info!(
                    "[{}] Mod {} (in config: {}) verified.",
                    S::NAME.errstyle(SITE_NAME_STYLE),
                    mod_info.project_info.name.errstyle(SITE_VAL_STYLE),
                    cfg_id.errstyle(CONFIG_VAL_STYLE)
                );
            }
            Err(failure) => {
                log::info!(
                    "[{}] Mod (in config: {}) FAILED verification.",
                    S::NAME.errstyle(SITE_NAME_STYLE),
                    cfg_id.errstyle(CONFIG_VAL_STYLE)
                );
                failures.insert(cfg_id, failure);
            }
        }
    }
}

async fn verify_mod<K, H, S>(
    minecraft_version: &String,
    mods_by_project_id: &HashSet<K>,
    mods_by_version_id: &HashSet<K>,
    cfg_id: &str,
    loaded_mod: ModFileInfo<K, H>,
    site: &S,
) -> Result<(), ModVerificationError>
where
    K: ModIdValue,
    S: ModSite<Id = K>,
{
    if !loaded_mod.project_info.distribution_allowed {
        return Err(ModVerificationError::DistributionDenied);
    }
    // Verify that the MC version matches
    if !loaded_mod.minecraft_versions.contains(minecraft_version) {
        return Err(ModVerificationError::MinecraftVersionMismatch {
            expected: minecraft_version.clone(),
            actual: loaded_mod.minecraft_versions,
        });
    }
    // Verify that all dependencies are specified.
    let mut missing_deps = Vec::new();
    for dep in loaded_mod.dependencies {
        match dep.kind {
            ModDependencyKind::Required => {
                if let Some(v) = get_dep_name_if_missing(
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
                if let Some(v) = get_dep_name_if_missing(
                    site,
                    dep.id.clone(),
                    mods_by_project_id,
                    mods_by_version_id,
                )
                .await?
                {
                    log::info!(
                        "[{}] [{}] Missing optional dependency for {}: {} (ID: {:?})",
                        S::NAME.errstyle(SITE_NAME_STYLE),
                        "FYI".errstyle(|s| s.bold().yellow()),
                        cfg_id.errstyle(CONFIG_VAL_STYLE),
                        v.errstyle(SITE_VAL_STYLE),
                        dep.id.errstyle(CONFIG_VAL_STYLE),
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

fn submit_load<K, H>(
    mod_id: ModId<K>,
    site: impl ModSite<Id = K, ModHash = H>,
) -> JoinHandle<ModFileLoadingResult<K, H>>
where
    K: ModIdValue,
    H: Send + Sync + 'static,
{
    static CONCURRENCY_LIMITER: Lazy<Semaphore> = Lazy::new(|| Semaphore::new(5));

    tokio::task::spawn(async move {
        let _guard = CONCURRENCY_LIMITER.acquire().await.expect("tokio failure");
        site.load_file(mod_id).await
    })
}
