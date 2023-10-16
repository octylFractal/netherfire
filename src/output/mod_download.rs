use std::collections::HashMap;
use std::error::Error;
use std::fmt::{Debug, Display, Formatter};
use std::path::{Path, PathBuf};
use std::pin::Pin;

use futures::TryStreamExt;
use itertools::Itertools;
use once_cell::sync::Lazy;
use thiserror::Error;
use tokio::io::AsyncRead;
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;
use tokio_util::compat::FuturesAsyncReadCompatExt;

use crate::checks::verify_mods::{KnownEnvRequirements, VerifiedMod, VerifiedModContainer};
use crate::config::pack::PackConfig;
use crate::mod_site::{ModHash, ModLoadingError, ModSite};
use crate::uwu_colors::{ErrStyle, CONFIG_VAL_STYLE, FILE_STYLE, SITE_NAME_STYLE};

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

pub(crate) async fn download_mods<F>(
    pack_config: &PackConfig<VerifiedModContainer>,
    dest_dir: &Path,
    side_test: F,
) -> Result<(), ModsDownloadError>
where
    F: FnMut(KnownEnvRequirements) -> bool + Clone,
{
    let mut failures = HashMap::<String, ModDownloadToFileError>::new();

    download_from_site(
        dest_dir,
        &mut failures,
        &pack_config.mods.curseforge,
        side_test.clone(),
    )
    .await;
    download_from_site(
        dest_dir,
        &mut failures,
        &pack_config.mods.modrinth,
        side_test,
    )
    .await;

    if !failures.is_empty() {
        return Err(ModsDownloadError { failures });
    }

    Ok(())
}

async fn download_from_site<S, F>(
    dest_dir: &Path,
    failures: &mut HashMap<String, ModDownloadToFileError>,
    mods: &HashMap<String, VerifiedMod<S>>,
    mut side_test: F,
) where
    F: FnMut(KnownEnvRequirements) -> bool,
    S: ModSite,
{
    let downloads = mods
        .iter()
        .filter(|(_, m)| side_test(m.env_requirements))
        .sorted_by_key(|(k, _)| k.as_str())
        .map(|(k, m)| (k.clone(), submit_download(k.clone(), m.clone(), dest_dir)))
        .collect::<Vec<_>>();
    for (cfg_id, dl_ftr) in downloads {
        if let Err(e) = dl_ftr.await.expect("tokio failure") {
            failures.insert(cfg_id.clone(), e);
        }
    }
}

fn submit_download<S>(
    cfg_id: String,
    mod_: VerifiedMod<S>,
    dest_dir: &Path,
) -> JoinHandle<Result<PathBuf, ModDownloadToFileError>>
where
    S: ModSite,
{
    static CONCURRENCY_LIMITER: Lazy<Semaphore> = Lazy::new(|| Semaphore::new(5));

    let dest_dir = dest_dir.to_owned();
    tokio::task::spawn(async move {
        let _guard = CONCURRENCY_LIMITER.acquire().await.expect("tokio failure");
        let mod_info = mod_.info;
        let dest_file = dest_dir.join(&mod_info.filename);
        if dest_file.exists() {
            // Check if we already have the file.
            let content = tokio::fs::read(&dest_file).await?;
            if mod_info
                .hash
                .check_hash_if_possible(&content)
                .is_some_and(|valid| valid)
            {
                log::info!(
                    "[{}] Found cached {} for {}",
                    S::NAME.errstyle(SITE_NAME_STYLE),
                    mod_info.filename.errstyle(FILE_STYLE),
                    cfg_id.errstyle(CONFIG_VAL_STYLE),
                );
                return Ok(dest_file);
            }
        }

        tokio::io::copy(
            &mut mod_download(mod_info.url).await?,
            &mut tokio::fs::File::create(&dest_file).await?,
        )
        .await?;

        log::info!(
            "[{}] Downloaded {} for {}",
            S::NAME.errstyle(SITE_NAME_STYLE),
            mod_info.filename.errstyle(FILE_STYLE),
            cfg_id.errstyle(CONFIG_VAL_STYLE),
        );

        Ok(dest_file)
    })
}

type BoxAsyncRead = Pin<Box<dyn AsyncRead + Send + Sync>>;

#[derive(Debug, Error)]
pub enum ModDownloadError {
    #[error("I/O Error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Reqwest Error: {0}")]
    Reqwest(#[from] reqwest::Error),
}

pub async fn mod_download(url: String) -> Result<BoxAsyncRead, ModDownloadError> {
    let req = reqwest::get(url).await?.error_for_status()?;
    Ok(Box::pin(
        req.bytes_stream()
            .map_err(|e| futures::io::Error::new(futures::io::ErrorKind::Other, e))
            .into_async_read()
            .compat(),
    ))
}
