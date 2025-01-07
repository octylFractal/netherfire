use std::fmt::Debug;
use std::future::Future;

use digest::Digest;
use ferinth::structures::project::{ProjectSupportRange, ProjectType};
use ferinth::structures::version::DependencyType;
use furse::structures::file_structs::{FileRelationType, HashAlgo};
use itertools::Itertools;
use serde::Deserialize;
use thiserror::Error;

use crate::config::global::{FERINTH, FURSE};
use crate::config::mods::EnvRequirement;
use crate::config::pack::{ModLoaderType, PackConfig};

pub trait ModIdValue: Clone + Debug + Eq + std::hash::Hash + Send + Sync + 'static {
    fn into_toml_edit_value(self) -> toml_edit::Value;
}

impl ModIdValue for i32 {
    fn into_toml_edit_value(self) -> toml_edit::Value {
        toml_edit::Value::from(self as i64)
    }
}

impl ModIdValue for String {
    fn into_toml_edit_value(self) -> toml_edit::Value {
        toml_edit::Value::from(self)
    }
}

pub trait ModHash: Clone + Send + Sync + 'static {
    /// Use the strongest available hash to check the content, if possible.
    /// Returns `None` if no hash is available.
    fn check_hash_if_possible(&self, content: &[u8]) -> Option<bool>;
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash, Deserialize)]
pub struct ModId<K: ModIdValue> {
    pub project_id: K,
    pub version_id: K,
}

#[async_trait::async_trait]
pub trait ModSite: Copy + Clone + Send + Sync + 'static {
    const NAME: &'static str;

    type Id: ModIdValue;

    type ModHash: ModHash;

    async fn load_metadata(&self, project_id: Self::Id) -> ModLoadingResult;

    async fn load_metadata_by_version(&self, version_id: Self::Id) -> Option<ModLoadingResult>;

    async fn load_file(&self, id: ModId<Self::Id>)
        -> ModFileLoadingResult<Self::Id, Self::ModHash>;

    async fn get_latest_version_for_pack<MC: Sync>(
        &self,
        pack: &PackConfig<MC>,
        project_id: Self::Id,
        ignore_mod_loader: bool,
    ) -> Result<Option<Self::Id>, ModLoadingError>;
}

#[derive(Debug, Copy, Clone)]
pub struct CurseForge;

#[async_trait::async_trait]
impl ModSite for CurseForge {
    const NAME: &'static str = "CurseForge";

    type Id = i32;

    type ModHash = CFHash;

    async fn load_metadata(&self, project_id: Self::Id) -> ModLoadingResult {
        let furse_mod = FURSE.get_mod(project_id).await?;

        Ok(ModInfo {
            name: furse_mod.name,
            distribution_allowed: furse_mod.allow_mod_distribution.unwrap_or(true),
            side_info: SideInfo {
                client: EnvRequirement::Unknown,
                server: EnvRequirement::Unknown,
            },
        })
    }

    async fn load_metadata_by_version(&self, _: Self::Id) -> Option<ModLoadingResult> {
        None
    }

    async fn load_file(
        &self,
        id: ModId<Self::Id>,
    ) -> ModFileLoadingResult<Self::Id, Self::ModHash> {
        let project_info = self.load_metadata(id.project_id).await?;
        let file = FURSE.get_mod_file(id.project_id, id.version_id).await?;

        let mut sha1 = None;
        let mut md5 = None;
        for hash in file.hashes {
            if hash.algo == HashAlgo::Sha1 {
                sha1 = hex_to_hash_output::<sha1::Sha1>(&hash.value);
            } else if hash.algo == HashAlgo::Md5 {
                md5 = hex_to_hash_output::<md5::Md5>(&hash.value);
            }
        }

        Ok(ModFileInfo {
            project_info,
            filename: file.file_name,
            url: file.download_url.map(|u| u.to_string()),
            file_length: file.file_length as u64,
            minecraft_versions: file.game_versions,
            dependencies: file
                .dependencies
                .into_iter()
                .map(|d| ModDependency {
                    id: DependencyId::Project(d.mod_id),
                    kind: match d.relation_type {
                        FileRelationType::RequiredDependency => ModDependencyKind::Required,
                        FileRelationType::OptionalDependency => ModDependencyKind::Optional,
                        _ => ModDependencyKind::Other,
                    },
                })
                .collect(),
            hash: CFHash { sha1, md5 },
        })
    }

    async fn get_latest_version_for_pack<MC: Sync>(
        &self,
        pack: &PackConfig<MC>,
        project_id: Self::Id,
        ignore_mod_loader: bool,
    ) -> Result<Option<Self::Id>, ModLoadingError> {
        let furse_mod = FURSE.get_mod(project_id).await?;

        let mod_loader_type = match pack.mod_loader.id {
            ModLoaderType::Forge => furse::structures::common_structs::ModLoaderType::Forge,
            ModLoaderType::Neoforge => furse::structures::common_structs::ModLoaderType::NeoForge,
            ModLoaderType::Fabric => furse::structures::common_structs::ModLoaderType::Fabric,
            ModLoaderType::Quilt => furse::structures::common_structs::ModLoaderType::Quilt,
        };

        Ok(furse_mod
            .latest_files_indexes
            .iter()
            .find(|fi| {
                fi.game_version == pack.minecraft_version
                    && fi
                        .mod_loader
                        .as_ref()
                        .is_some_and(|ml| ignore_mod_loader || ml == &mod_loader_type)
            })
            .map(|fi| fi.file_id))
    }
}

#[derive(Debug, Clone)]
pub struct CFHash {
    pub sha1: Option<digest::Output<sha1::Sha1>>,
    pub md5: Option<digest::Output<md5::Md5>>,
}

impl ModHash for CFHash {
    fn check_hash_if_possible(&self, content: &[u8]) -> Option<bool> {
        if let Some(sha1) = self.sha1 {
            return Some(check_hash::<sha1::Sha1>(&sha1, content));
        }
        if let Some(md5) = self.md5 {
            return Some(check_hash::<md5::Md5>(&md5, content));
        }
        None
    }
}

#[derive(Debug, Copy, Clone)]
pub struct Modrinth;

#[async_trait::async_trait]
impl ModSite for Modrinth {
    const NAME: &'static str = "Modrinth";

    type Id = String;

    type ModHash = ModrinthHash;

    async fn load_metadata(&self, project_id: Self::Id) -> ModLoadingResult {
        let ferinth_mod = ferinth_with_retry(|| FERINTH.get_project(&project_id)).await?;
        if ferinth_mod.project_type != ProjectType::Mod {
            return Err(ModLoadingError::NotAMod);
        }

        Ok(ModInfo {
            name: ferinth_mod.title,
            distribution_allowed: true,
            side_info: SideInfo {
                client: ferinth_mod.client_side.into(),
                server: ferinth_mod.server_side.into(),
            },
        })
    }

    async fn load_metadata_by_version(&self, version_id: Self::Id) -> Option<ModLoadingResult> {
        let version_info = match ferinth_with_retry(|| FERINTH.get_version(&version_id)).await {
            Ok(v) => v,
            Err(e) => return Some(Err(e.into())),
        };

        Some(self.load_metadata(version_info.project_id).await)
    }

    async fn load_file(
        &self,
        id: ModId<Self::Id>,
    ) -> ModFileLoadingResult<Self::Id, Self::ModHash> {
        let project_info = self.load_metadata(id.project_id).await?;
        let version = ferinth_with_retry(|| FERINTH.get_version(&id.version_id)).await?;
        let file_meta = version
            .files
            .into_iter()
            .find_or_first(|f| f.primary)
            .ok_or(ModLoadingError::NoFiles)?;

        let dependencies = version
            .dependencies
            .into_iter()
            .map(|d| {
                let id = d.project_id.clone().map(DependencyId::Project)
                    .or_else(|| d.version_id.clone().map(DependencyId::Version))
                    .unwrap_or_else(|| panic!(
                        "one of either project_id or version_id must be set; dependency {:?} from {}",
                        d,
                        project_info.name,
                    ));
                ModDependency {
                    id,
                    kind: match d.dependency_type {
                        DependencyType::Required => ModDependencyKind::Required,
                        DependencyType::Optional => ModDependencyKind::Optional,
                        _ => ModDependencyKind::Other,
                    },
                }
            })
            .collect();
        Ok(ModFileInfo {
            project_info,
            filename: file_meta.filename,
            url: Some(file_meta.url.to_string()),
            file_length: file_meta.size as u64,
            minecraft_versions: version.game_versions,
            dependencies,
            hash: ModrinthHash {
                sha1: hex_to_hash_output::<sha1::Sha1>(&file_meta.hashes.sha1)
                    .expect("invalid sha1 hash"),
                sha512: hex_to_hash_output::<sha2::Sha512>(&file_meta.hashes.sha512)
                    .expect("invalid sha512 hash"),
            },
        })
    }

    async fn get_latest_version_for_pack<MC: Sync>(
        &self,
        pack: &PackConfig<MC>,
        project_id: Self::Id,
        ignore_mod_loader: bool,
    ) -> Result<Option<Self::Id>, ModLoadingError> {
        let ferinth_mod = ferinth_with_retry(|| FERINTH.get_project(&project_id)).await?;
        if ferinth_mod.project_type != ProjectType::Mod {
            return Err(ModLoadingError::NotAMod);
        }

        let mod_loader = pack.mod_loader.id.to_string();
        let mut version_infos = Vec::new();
        for v in ferinth_mod.versions {
            let version_info = ferinth_with_retry(|| FERINTH.get_version(&v)).await?;
            if !version_info.game_versions.contains(&pack.minecraft_version) {
                continue;
            }
            if !ignore_mod_loader && !version_info.loaders.contains(&mod_loader) {
                continue;
            }
            version_infos.push(version_info);
        }
        version_infos.sort_by_key(|v| v.date_published);
        Ok(version_infos.into_iter().last().map(|v| v.id))
    }
}

impl From<ProjectSupportRange> for EnvRequirement {
    fn from(range: ProjectSupportRange) -> Self {
        match range {
            ProjectSupportRange::Unknown => EnvRequirement::Unknown,
            ProjectSupportRange::Required => EnvRequirement::Required,
            ProjectSupportRange::Optional => EnvRequirement::Optional,
            ProjectSupportRange::Unsupported => EnvRequirement::Unsupported,
        }
    }
}

async fn ferinth_with_retry<T, Fut>(request: impl Fn() -> Fut) -> ferinth::Result<T>
where
    Fut: Future<Output = ferinth::Result<T>>,
{
    let mut retries = 0;
    loop {
        match request().await {
            Ok(v) => return Ok(v),
            Err(ferinth::Error::RateLimitExceeded(delay_sec)) => {
                let adjusted_delay = std::cmp::max(delay_sec as u64 + 1, retries + 1);
                if retries >= 5 {
                    return Err(ferinth::Error::RateLimitExceeded(delay_sec));
                }
                log::warn!(
                    "Retrying request in {} (+ {}) sec due to rate limit",
                    delay_sec,
                    adjusted_delay - delay_sec as u64
                );
                tokio::time::sleep(tokio::time::Duration::from_secs(adjusted_delay)).await;
                retries += 1;
            }
            Err(e) => return Err(e),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ModrinthHash {
    pub sha1: digest::Output<sha1::Sha1>,
    pub sha512: digest::Output<sha2::Sha512>,
}

impl ModHash for ModrinthHash {
    fn check_hash_if_possible(&self, content: &[u8]) -> Option<bool> {
        Some(check_hash::<sha2::Sha512>(&self.sha512, content))
    }
}

#[derive(Debug, Error)]
pub enum ModLoadingError {
    #[error("The project exists, but is not a mod")]
    NotAMod,
    #[error("The project and version exist, but they have no files")]
    NoFiles,
    #[error("CurseForge Error: {0}")]
    Furse(#[from] furse::Error),
    #[error("Modrinth Error: {0}")]
    Ferinth(#[from] ferinth::Error),
}

pub type ModLoadingResult = Result<ModInfo, ModLoadingError>;
pub type ModFileLoadingResult<K, H> = Result<ModFileInfo<K, H>, ModLoadingError>;

#[derive(Debug, Clone)]
pub struct ModFileInfo<K, H> {
    pub project_info: ModInfo,
    pub filename: String,
    pub url: Option<String>,
    pub file_length: u64,
    pub minecraft_versions: Vec<String>,
    pub dependencies: Vec<ModDependency<K>>,
    pub hash: H,
}

/// Tries to convert a hex representation of a hash into a hash output.
/// Returns `None` if the hex string is invalid.
pub fn hex_to_hash_output<D: Digest>(s: &str) -> Option<digest::Output<D>> {
    let mut array = digest::Output::<D>::default();
    hex::decode_to_slice(s, &mut array)
        .map_err(|e| {
            log::debug!("invalid hex string: {}", e);
        })
        .ok()?;
    Some(array)
}

pub fn check_hash<D: Digest + Default>(value: &digest::Output<D>, content: &[u8]) -> bool {
    let mut hasher = D::default();
    hasher.update(content);
    &hasher.finalize() == value
}

#[derive(Debug, Clone)]
pub struct ModInfo {
    pub name: String,
    pub distribution_allowed: bool,
    pub side_info: SideInfo,
}

#[derive(Debug, Clone, Copy)]
pub struct SideInfo {
    pub client: EnvRequirement,
    pub server: EnvRequirement,
}

#[derive(Debug, Clone)]
pub struct ModDependency<K> {
    pub id: DependencyId<K>,
    pub kind: ModDependencyKind,
}

#[derive(Debug, Clone, Eq, PartialEq, Hash, Deserialize)]
#[serde(from = "ExplicitDependencyId<K>")]
pub enum DependencyId<K> {
    Project(K),
    Version(K),
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ExplicitDependencyId<K> {
    Project { project_id: K },
    Version { version_id: K },
}

impl<K> From<ExplicitDependencyId<K>> for DependencyId<K> {
    fn from(id: ExplicitDependencyId<K>) -> Self {
        match id {
            ExplicitDependencyId::Project { project_id } => DependencyId::Project(project_id),
            ExplicitDependencyId::Version { version_id } => DependencyId::Version(version_id),
        }
    }
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum ModDependencyKind {
    Required,
    Optional,
    Other,
}
