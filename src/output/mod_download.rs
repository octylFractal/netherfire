use std::collections::HashMap;
use std::error::Error;
use std::fmt::{Debug, Display, Formatter};
use std::path::{Path, PathBuf};

use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use itertools::Itertools;
use once_cell::sync::Lazy;
use thiserror::Error;
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;

use crate::config::mods::{Mod, ModSide};
use crate::mod_site::{
    CurseForge, ModDownloadError, ModId, ModIdValue, ModLoadingError, ModSite, Modrinth,
};
use crate::progress::{steady_tick_duration, style_bar};
use crate::ModConfig;

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
    mod_config: &ModConfig,
    multi: MultiProgress,
    dest_dir: &Path,
    side_test: F,
) -> Result<(), ModsDownloadError>
where
    F: FnMut(ModSide) -> bool + Clone,
{
    let mut failures = HashMap::<String, ModDownloadToFileError>::new();

    download_from_site(
        CurseForge,
        multi.clone(),
        dest_dir,
        &mut failures,
        &mod_config.mods.curseforge,
        side_test.clone(),
    )
    .await;
    download_from_site(
        Modrinth,
        multi.clone(),
        dest_dir,
        &mut failures,
        &mod_config.mods.modrinth,
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
