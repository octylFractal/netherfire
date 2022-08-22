use digest::Digest;
use std::fmt::{Debug, Display, Formatter};
use std::pin::Pin;

use ferinth::structures::project_structs::ProjectType;
use ferinth::structures::version_structs::DependencyType;
use furse::structures::file_structs::{FileRelationType, HashAlgo};
use futures::TryStreamExt;
use reqwest::Url;
use serde::Deserialize;
use thiserror::Error;
use tokio::io::AsyncRead;
use tokio_util::compat::FuturesAsyncReadCompatExt;

use crate::config::global::{FERINTH, FURSE};

#[derive(Debug, Copy, Clone, Eq, PartialEq, Deserialize)]
pub struct ModId<K> {
    pub project_id: K,
    pub version_id: K,
}

#[async_trait::async_trait]
pub trait ModSite {
    const NAME: &'static str;

    type Id;

    async fn load_metadata(&self, project_id: Self::Id) -> ModLoadingResult;

    async fn load_file(&self, id: ModId<Self::Id>) -> ModFileLoadingResult<Self::Id>;

    async fn download(&self, id: ModId<Self::Id>) -> ModDownloadResult;
}

#[derive(Copy, Clone)]
pub struct CurseForge;

#[async_trait::async_trait]
impl ModSite for CurseForge {
    const NAME: &'static str = "CurseForge";

    type Id = i32;

    async fn load_metadata(&self, project_id: Self::Id) -> ModLoadingResult {
        let furse_mod = FURSE.get_mod(project_id).await?;

        Ok(ModInfo {
            name: furse_mod.name,
            distribution_allowed: furse_mod.allow_mod_distribution.unwrap_or(true),
        })
    }

    async fn load_file(&self, id: ModId<Self::Id>) -> ModFileLoadingResult<Self::Id> {
        let project_info = self.load_metadata(id.project_id).await?;
        let file = FURSE.get_mod_file(id.project_id, id.version_id).await?;

        let mut sha1 = None;
        let mut md5 = None;
        for hash in file.hashes {
            if hash.algo == HashAlgo::Sha1 {
                sha1 = Some(hash.value);
            } else if hash.algo == HashAlgo::Md5 {
                md5 = Some(hash.value);
            }
        }

        Ok(ModFileInfo {
            project_info,
            filename: file.file_name,
            dependencies: file
                .dependencies
                .into_iter()
                .map(|d| ModDependency {
                    project_id: d.mod_id,
                    kind: match d.relation_type {
                        FileRelationType::RequiredDependency => ModDependencyKind::Required,
                        FileRelationType::OptionalDependency => ModDependencyKind::Optional,
                        _ => ModDependencyKind::Other,
                    },
                })
                .collect(),
            hash: sha1
                .map(|v| Hash {
                    algo: HashAlgorithm::Sha1,
                    value: v,
                })
                .or_else(|| {
                    md5.map(|v| Hash {
                        algo: HashAlgorithm::Md5,
                        value: v,
                    })
                }),
        })
    }

    async fn download(&self, id: ModId<Self::Id>) -> ModDownloadResult {
        let file_meta = FURSE.get_mod_file(id.project_id, id.version_id).await?;

        let url = file_meta.download_url.expect("verified earlier");
        reqwest_async_read(url).await
    }
}

#[derive(Copy, Clone)]
pub struct Modrinth;

#[async_trait::async_trait]
impl ModSite for Modrinth {
    const NAME: &'static str = "Modrinth";

    type Id = String;

    async fn load_metadata(&self, project_id: Self::Id) -> ModLoadingResult {
        let ferinth_mod = FERINTH.get_project(&project_id).await?;
        if ferinth_mod.project_type != ProjectType::Mod {
            return Err(ModLoadingError::NotAMod);
        }

        Ok(ModInfo {
            name: ferinth_mod.title,
            distribution_allowed: true,
        })
    }

    async fn load_file(&self, id: ModId<Self::Id>) -> ModFileLoadingResult<Self::Id> {
        let project_info = self.load_metadata(id.project_id).await?;
        let version = FERINTH.get_version(&id.version_id).await?;
        let file_meta = version
            .files
            .into_iter()
            .find(|f| f.primary)
            .expect("no primary file");

        Ok(ModFileInfo {
            project_info,
            filename: file_meta.filename,
            dependencies: version
                .dependencies
                .into_iter()
                .map(|d| ModDependency {
                    project_id: d.project_id.expect("no project ID for dependency"),
                    kind: match d.dependency_type {
                        DependencyType::Required => ModDependencyKind::Required,
                        DependencyType::Optional => ModDependencyKind::Optional,
                        _ => ModDependencyKind::Other,
                    },
                })
                .collect(),
            hash: Some(Hash {
                algo: HashAlgorithm::Sha512,
                value: file_meta.hashes.sha512,
            }),
        })
    }

    async fn download(&self, id: ModId<Self::Id>) -> ModDownloadResult {
        let file_meta = FERINTH
            .get_version(&id.version_id)
            .await?
            .files
            .into_iter()
            .find(|f| f.primary)
            .expect("no primary file");

        reqwest_async_read(file_meta.url).await
    }
}

async fn reqwest_async_read(url: Url) -> Result<BoxAsyncRead, ModDownloadError> {
    let req = reqwest::get(url).await?.error_for_status()?;
    Ok(Box::pin(
        req.bytes_stream()
            .map_err(|e| futures::io::Error::new(futures::io::ErrorKind::Other, e))
            .into_async_read()
            .compat(),
    ))
}

#[derive(Debug, Error)]
pub enum ModLoadingError {
    #[error("The project exists, but is not a mod")]
    NotAMod,
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

pub type ModLoadingResult = Result<ModInfo, ModLoadingError>;
pub type ModFileLoadingResult<K> = Result<ModFileInfo<K>, ModLoadingError>;

#[derive(Debug)]
pub struct ModFileInfo<K> {
    pub project_info: ModInfo,
    pub filename: String,
    pub dependencies: Vec<ModDependency<K>>,
    pub hash: Option<Hash>,
}

#[derive(Debug)]
pub struct Hash {
    pub algo: HashAlgorithm,
    pub value: String,
}

impl Hash {
    pub fn check(&self, content: &[u8]) -> bool {
        let expected_hash = hex::decode(&self.value)
            .unwrap_or_else(|e| panic!("hash ({}) problem: {}", &self.value, e));
        (match self.algo {
            HashAlgorithm::Md5 => md5::Md5::digest(content).to_vec(),
            HashAlgorithm::Sha1 => sha1::Sha1::digest(content).to_vec(),
            HashAlgorithm::Sha512 => sha2::Sha512::digest(content).to_vec(),
        }) == expected_hash
    }
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum HashAlgorithm {
    Md5,
    Sha1,
    Sha512,
}

impl Display for HashAlgorithm {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                HashAlgorithm::Md5 => "md5",
                HashAlgorithm::Sha1 => "sha1",
                HashAlgorithm::Sha512 => "sha512",
            }
        )
    }
}

#[derive(Debug)]
pub struct ModInfo {
    pub name: String,
    pub distribution_allowed: bool,
}

#[derive(Debug)]
pub struct ModDependency<K> {
    pub project_id: K,
    pub kind: ModDependencyKind,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum ModDependencyKind {
    Required,
    Optional,
    Other,
}

type BoxAsyncRead = Pin<Box<dyn AsyncRead + Send + Sync>>;

pub type ModDownloadResult = Result<BoxAsyncRead, ModDownloadError>;
