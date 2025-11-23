use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::fmt::{Debug, Display, Formatter};

use itertools::Itertools;
use once_cell::sync::Lazy;
use thiserror::Error;
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;

use crate::config::mods::{
    compute_env, ConfigMod, ConfigModContainer, EnvRequirement, KnownEnvRequirement,
};
use crate::config::pack::PackConfig;
use crate::mod_site::{
    CurseForge, DependencyId, ModDependencyKind, ModFileInfo, ModFileLoadingResult, ModId,
    ModIdValue, ModInfo, ModLoadingError, ModSite, Modrinth,
};
use crate::uwu_colors::{
    ErrStyle, CONFIG_VAL_STYLE, SITE_NAME_STYLE, SITE_VAL_STYLE, SUCCESS_STYLE,
};

#[derive(Debug, Clone)]
pub struct VerifiedModContainer {
    pub curseforge: HashMap<String, VerifiedMod<CurseForge>>,
    pub modrinth: HashMap<String, VerifiedMod<Modrinth>>,
}

#[derive(Debug, Clone)]
pub struct VerifiedMod<S: ModSite> {
    pub source: ModId<S::Id>,
    pub info: ModFileInfo<S::Id, S::ModHash>,
    pub env_requirements: KnownEnvRequirements,
}

#[derive(Debug, Clone, Copy)]
pub struct KnownEnvRequirements {
    pub client: KnownEnvRequirement,
    pub server: KnownEnvRequirement,
}

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
    #[error("Error loading dependency {0}: {1}")]
    DependencyLoading(String, #[source] ModLoadingError),
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

pub(crate) async fn verify_mods(
    pack_config: PackConfig<ConfigModContainer>,
) -> Result<PackConfig<VerifiedModContainer>, ModsVerificationError> {
    let cf_verify = tokio::spawn(verify_mods_site(
        pack_config.minecraft_version.clone(),
        pack_config.mods.curseforge,
        CurseForge,
    ));

    let modrinth_verify = tokio::spawn(verify_mods_site(
        pack_config.minecraft_version.clone(),
        pack_config.mods.modrinth,
        Modrinth,
    ));

    let cf_result = cf_verify.await.expect("tokio error");
    let modrinth_result = modrinth_verify.await.expect("tokio error");

    let mod_container = match (cf_result, modrinth_result) {
        (Ok(curseforge), Ok(modrinth)) => VerifiedModContainer {
            curseforge,
            modrinth,
        },
        (cf_result, modrinth_result) => {
            let mut failures = HashMap::new();

            if let Err(e) = cf_result {
                failures.extend(e);
            }

            if let Err(e) = modrinth_result {
                failures.extend(e);
            }

            return Err(ModsVerificationError { failures });
        }
    };

    log::info!("{}", "Verified mods successfully.".errstyle(SUCCESS_STYLE));

    Ok(PackConfig {
        name: pack_config.name,
        description: pack_config.description,
        author: pack_config.author,
        version: pack_config.version,
        minecraft_version: pack_config.minecraft_version,
        mod_loader: pack_config.mod_loader,
        mods: mod_container,
    })
}

async fn verify_mods_site<K, S>(
    minecraft_version: String,
    mods: HashMap<String, ConfigMod<K>>,
    site: S,
) -> Result<HashMap<String, VerifiedMod<S>>, HashMap<String, ModVerificationError>>
where
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

        // Register any ids that are given as substitutions
        for dep_id in &m.substitute_for {
            match dep_id {
                DependencyId::Project(project_id) => {
                    mods_by_project_id.insert(project_id.clone());
                }
                DependencyId::Version(version_id) => {
                    mods_by_version_id.insert(version_id.clone());
                }
            }
        }

        let id = m.source.clone();
        verifications.push((k, m, submit_load(id, site)));
    }
    let mut verification_results = HashMap::with_capacity(verifications.len());
    let mut failures = HashMap::new();
    for (cfg_id, m, verification_ftr) in verifications {
        let failure = match verification_ftr.await.expect("tokio failure") {
            Err(e) => Err(e.into()),
            Ok(loaded_mod) => verify_mod(
                &minecraft_version,
                &mods_by_project_id,
                &mods_by_version_id,
                &cfg_id,
                m.ignored_deps.iter().cloned().collect(),
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

                let map_env = |side: &'static str,
                               cfg_env: EnvRequirement,
                               site_env: EnvRequirement|
                 -> KnownEnvRequirement {
                    let (ret, warning) = compute_env(cfg_env, site_env);
                    if let Some(warning) = warning {
                        log::warn!(
                            "Warning about env requirement for {} on side {}: {}",
                            cfg_id.errstyle(CONFIG_VAL_STYLE),
                            side,
                            warning
                        );
                    }
                    ret
                };

                let client = map_env("client", m.client, mod_info.project_info.side_info.client);
                let server = map_env("server", m.server, mod_info.project_info.side_info.server);
                verification_results.insert(
                    cfg_id,
                    VerifiedMod {
                        source: m.source,
                        info: mod_info,
                        env_requirements: KnownEnvRequirements { client, server },
                    },
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
    if failures.is_empty() {
        Ok(verification_results)
    } else {
        Err(failures)
    }
}

async fn verify_mod<K, H, S>(
    minecraft_version: &String,
    mods_by_project_id: &HashSet<K>,
    mods_by_version_id: &HashSet<K>,
    cfg_id: &str,
    ignored_deps: HashSet<DependencyId<K>>,
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
        if ignored_deps.contains(&dep.id) {
            continue;
        }
        match dep.kind {
            ModDependencyKind::Required => {
                match get_dep_meta_if_missing(
                    site,
                    dep.id.clone(),
                    mods_by_project_id,
                    mods_by_version_id,
                )
                .await
                {
                    Ok(Some(v)) => missing_deps
                        .push(format!("{} (Slug: {}, ID: {:?})", v.name, v.slug, dep.id)),
                    Ok(None) => {}
                    Err(e) => {
                        return Err(ModVerificationError::DependencyLoading(
                            format!("{:?}", dep.id),
                            e,
                        ));
                    }
                }
            }
            ModDependencyKind::Optional => {
                match get_dep_meta_if_missing(
                    site,
                    dep.id.clone(),
                    mods_by_project_id,
                    mods_by_version_id,
                )
                .await
                {
                    Ok(Some(v)) => {
                        log::info!(
                            "[{}] [{}] Missing optional dependency for {}: {} (Slug: {}, ID: {:?})",
                            S::NAME.errstyle(SITE_NAME_STYLE),
                            "FYI".errstyle(|s| s.bold().yellow()),
                            cfg_id.errstyle(CONFIG_VAL_STYLE),
                            v.name.errstyle(SITE_VAL_STYLE),
                            v.slug.errstyle(SITE_VAL_STYLE),
                            dep.id.errstyle(CONFIG_VAL_STYLE),
                        );
                    }
                    Ok(None) => {}
                    Err(e) => {
                        log::warn!(
                            "[{}] Error loading optional dependency for {}, dependency ID = {:?}: {}",
                            S::NAME.errstyle(SITE_NAME_STYLE),
                            cfg_id.errstyle(CONFIG_VAL_STYLE),
                            dep.id.errstyle(CONFIG_VAL_STYLE),
                            e,
                        );
                    }
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

struct DepMeta {
    name: String,
    slug: String,
}

async fn get_dep_meta_if_missing<K, S>(
    site: &S,
    id: DependencyId<K>,
    mods_by_project_id: &HashSet<K>,
    mods_by_version_id: &HashSet<K>,
) -> Result<Option<DepMeta>, ModLoadingError>
where
    K: ModIdValue,
    S: ModSite<Id = K>,
{
    let mod_to_meta = |v: ModInfo| {
        Some(DepMeta {
            name: v.name,
            slug: v.slug,
        })
    };
    match id {
        DependencyId::Project(project_id) => {
            if !mods_by_project_id.contains(&project_id) {
                site.load_metadata(project_id).await.map(mod_to_meta)
            } else {
                Ok(None)
            }
        }
        DependencyId::Version(version_id) => {
            if !mods_by_version_id.contains(&version_id) {
                site.load_metadata_by_version(version_id).await
                    .expect("sites that provide only a version in dependencies must allow lookup by version")
                    .map(mod_to_meta)
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
