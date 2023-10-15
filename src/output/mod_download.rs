use std::collections::HashMap;
use std::error::Error;
use std::fmt::{Debug, Display, Formatter};
use std::path::{Path, PathBuf};

use itertools::Itertools;
use once_cell::sync::Lazy;
use thiserror::Error;
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;

use crate::config::mods::{Mod, ModSide};
use crate::config::pack::PackConfig;
use crate::mod_site::{
    CurseForge, ModDownloadError, ModHash, ModId, ModIdValue, ModLoadingError, ModSite, Modrinth,
};
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
    pack_config: &PackConfig,
    dest_dir: &Path,
    side_test: F,
) -> Result<(), ModsDownloadError>
where
    F: FnMut(ModSide) -> bool + Clone,
{
    let mut failures = HashMap::<String, ModDownloadToFileError>::new();

    download_from_site(
        CurseForge,
        dest_dir,
        &mut failures,
        &pack_config.mods.curseforge,
        side_test.clone(),
    )
    .await;
    download_from_site(
        Modrinth,
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

async fn download_from_site<K, S, F>(
    site: S,
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
                submit_download(k.clone(), m.source.clone(), site, dest_dir),
            )
        })
        .collect::<Vec<_>>();
    for (cfg_id, dl_ftr) in downloads {
        if let Err(e) = dl_ftr.await.expect("tokio failure") {
            failures.insert(cfg_id.clone(), e);
        }
    }
}

fn submit_download<K, S>(
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
        let _guard = CONCURRENCY_LIMITER.acquire().await.expect("tokio failure");
        let file_meta = site.load_file(mod_id.clone()).await?;
        let dest_file = dest_dir.join(&file_meta.filename);
        if dest_file.exists() {
            // Check if we already have the file.
            let content = tokio::fs::read(&dest_file).await?;
            if file_meta
                .hash
                .check_hash_if_possible(&content)
                .is_some_and(|valid| valid)
            {
                log::info!(
                    "[{}] Found cached {} for {}",
                    S::NAME.errstyle(SITE_NAME_STYLE),
                    file_meta.filename.errstyle(FILE_STYLE),
                    cfg_id.errstyle(CONFIG_VAL_STYLE),
                );
                return Ok(dest_file);
            }
        }

        tokio::io::copy(
            &mut site.download(mod_id).await?,
            &mut tokio::fs::File::create(&dest_file).await?,
        )
        .await?;

        log::info!(
            "[{}] Downloaded {} for {}",
            S::NAME.errstyle(SITE_NAME_STYLE),
            file_meta.filename.errstyle(FILE_STYLE),
            cfg_id.errstyle(CONFIG_VAL_STYLE),
        );

        Ok(dest_file)
    })
}
