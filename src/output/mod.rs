use std::io::{Seek, Write};
use std::ops::DerefMut;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use once_cell::sync::Lazy;
use reflink::reflink_or_copy;
use thiserror::Error;
use tokio::spawn;
use tokio::sync::Mutex;
use tokio_util::io::SyncIoBridge;
use walkdir::WalkDir;
use zip::{CompressionMethod, ZipWriter};

use crate::config::pack::ModLoaderType;
use crate::mod_site::{CurseForge, ModDownloadError, ModId, ModLoadingError, ModSite, Modrinth};
use crate::output::curseforge_manifest::{
    CurseForgeManifest, ManifestFile, ManifestType, Minecraft, ModLoader,
};
use crate::output::mod_download::{download_mods, ModsDownloadError};
use crate::output::modrinth_manifest::ModrinthManifest;
use crate::progress::{steady_tick_duration, style_bar};
use crate::{ModConfig, PackConfig};

mod curseforge_manifest;
mod mod_download;
mod modrinth_manifest;

const LIT_MODS: &str = "mods";
const LIT_OVERRIDES: &str = "overrides";
const LIT_SERVER_OVERRIDES: &str = "server-overrides";
const LIT_CLIENT_OVERRIDES: &str = "client-overrides";

#[derive(Debug, Error)]
pub enum CreateCurseForgeZipError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Json error: {0}")]
    Json(#[from] serde_json::error::Error),
    #[error("ZIP error: {0}")]
    Zip(#[from] zip::result::ZipError),
    #[error("Zipping directory {0} failed: {1}")]
    ZipDir(String, #[source] ZipDirError),
    #[error("Zipping mod {0} failed: {1}")]
    ZipMod(String, #[source] ZipModError),
}

static ZIP_OPTIONS: Lazy<zip::write::FileOptions> = Lazy::new(|| {
    zip::write::FileOptions::default().compression_method(CompressionMethod::Deflated)
});

pub async fn create_curseforge_zip(
    pack: &PackConfig,
    mods: &ModConfig,
    source_dir: &Path,
    output_dir: PathBuf,
) -> Result<(), CreateCurseForgeZipError> {
    let output_file = output_dir.join(format!("{} ({}).zip", pack.name, pack.version));

    let multi = MultiProgress::new();
    let action_pb = multi.add(ProgressBar::new_spinner().with_message(format!(
        "Creating CurseForge zip at '{}'...",
        output_file.display()
    )));
    action_pb.enable_steady_tick(steady_tick_duration());

    std::fs::create_dir_all(&output_dir)?;

    let zip = ZipWriter::new(std::fs::File::create(&output_file)?);

    let progress_bar = multi.add(ProgressBar::new_spinner());
    progress_bar.enable_steady_tick(steady_tick_duration());

    progress_bar.set_message("Downloading Modrinth mods...");
    progress_bar.set_length(mods.mods.modrinth.len() as u64);

    let zip_arc = Arc::new(Mutex::new(zip));
    let mut zip_dl_tasks = Vec::with_capacity(mods.mods.modrinth.len());
    for (cfg_id, mod_) in &mods.mods.modrinth {
        zip_dl_tasks.push((
            cfg_id,
            spawn(add_mod_to_zip(
                multi.clone(),
                Modrinth,
                mod_.source.clone(),
                Arc::clone(&zip_arc),
            )),
        ));
    }
    for (cfg_id, task) in zip_dl_tasks {
        task.await
            .expect("task panicked")
            .map_err(|e| CreateCurseForgeZipError::ZipMod(cfg_id.clone(), e))?;
        progress_bar.inc(1);
    }
    let mut zip = Arc::into_inner(zip_arc)
        .expect("all zip tasks should be finished")
        .into_inner();

    progress_bar.set_style(ProgressStyle::default_spinner());
    progress_bar.set_message("Copying overrides...");
    zip_dir(
        source_dir.join(LIT_OVERRIDES),
        &mut zip,
        LIT_OVERRIDES,
        CreateCurseForgeZipError::ZipDir,
    )?;
    progress_bar.set_message("Copying client-only overrides...");
    zip_dir(
        source_dir.join(LIT_CLIENT_OVERRIDES),
        &mut zip,
        LIT_OVERRIDES,
        CreateCurseForgeZipError::ZipDir,
    )?;

    progress_bar.set_message("Writing manifest...");
    let manifest = CurseForgeManifest {
        minecraft: Minecraft {
            version: pack.minecraft_version.clone(),
            mod_loaders: vec![ModLoader {
                id: format!("{}-{}", pack.mod_loader.id, pack.mod_loader.version),
                primary: true,
            }],
        },
        manifest_type: ManifestType::MinecraftModpack,
        manifest_version: 1,
        name: pack.name.clone(),
        version: pack.version.clone(),
        author: pack.author.clone(),
        files: mods
            .mods
            .curseforge
            .values()
            .filter(|m| m.side.on_client())
            .map(|m| ManifestFile {
                project_id: m.source.project_id,
                file_id: m.source.version_id,
                required: true,
            })
            .collect(),
        overrides: LIT_OVERRIDES.to_string(),
    };
    zip.start_file("manifest.json", *ZIP_OPTIONS)?;
    serde_json::to_writer(&mut zip, &manifest)?;

    progress_bar.set_message("Flushing zip...");

    zip.finish()?;

    multi.remove(&progress_bar);

    action_pb.disable_steady_tick();
    action_pb.finish_with_message(format!(
        "Created CurseForge zip at '{}'.",
        output_file.display()
    ));

    Ok(())
}

#[derive(Debug, Error)]
pub enum CreateModrinthPackError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Json error: {0}")]
    Json(#[from] serde_json::error::Error),
    #[error("ZIP error: {0}")]
    Zip(#[from] zip::result::ZipError),
    #[error("Zipping directory {0} failed: {1}")]
    ZipDir(String, #[source] ZipDirError),
    #[error("Zipping mod {0} failed: {1}")]
    ZipMod(String, #[source] ZipModError),
    #[error("Failed to load mod {0} metadata: {1}")]
    ModrinthModLoad(String, #[source] ModLoadingError),
}

pub async fn create_modrinth_pack(
    pack: &PackConfig,
    mods: &ModConfig,
    source_dir: &Path,
    output_dir: PathBuf,
) -> Result<(), CreateModrinthPackError> {
    let output_file = output_dir.join(format!("{} ({}).mrpack", pack.name, pack.version));

    let multi = MultiProgress::new();
    let action_pb = multi.add(ProgressBar::new_spinner().with_message(format!(
        "Creating Modrinth pack at '{}'...",
        output_file.display()
    )));
    action_pb.enable_steady_tick(steady_tick_duration());

    std::fs::create_dir_all(&output_dir)?;

    let progress_bar = multi.add(ProgressBar::new_spinner());
    progress_bar.enable_steady_tick(steady_tick_duration());

    progress_bar.set_message("Fetching Modrinth metadata...");
    progress_bar.set_length(mods.mods.modrinth.len() as u64);
    let mut modrinth_files = Vec::with_capacity(mods.mods.modrinth.len());
    for (cfg_id, mod_) in &mods.mods.modrinth {
        let mod_info = Modrinth
            .load_file(mod_.source.clone())
            .await
            .map_err(|e| CreateModrinthPackError::ModrinthModLoad(cfg_id.clone(), e))?;
        modrinth_files.push(modrinth_manifest::ModFile {
            path: format!("mods/{}", mod_info.filename),
            hashes: modrinth_manifest::ModFileHashes {
                sha1: format!("{:x}", mod_info.hash.sha1),
                sha512: format!("{:x}", mod_info.hash.sha512),
            },
            env: Some(modrinth_manifest::Environment {
                client: if mod_.side.on_client() {
                    modrinth_manifest::EnvRequirement::Required
                } else {
                    modrinth_manifest::EnvRequirement::Unsupported
                },
                server: if mod_.side.on_server() {
                    modrinth_manifest::EnvRequirement::Required
                } else {
                    modrinth_manifest::EnvRequirement::Unsupported
                },
            }),
            downloads: vec![mod_info.url],
            file_size: mod_info.file_length,
        });
        progress_bar.inc(1);
    }

    progress_bar.set_message("Downloading CurseForge mods...");
    progress_bar.set_length(mods.mods.curseforge.len() as u64);

    let zip = ZipWriter::new(std::fs::File::create(&output_file)?);

    let zip_arc = Arc::new(Mutex::new(zip));
    let mut zip_dl_tasks = Vec::with_capacity(mods.mods.curseforge.len());
    for (cfg_id, mod_) in &mods.mods.curseforge {
        zip_dl_tasks.push((
            cfg_id,
            spawn(add_mod_to_zip(
                multi.clone(),
                CurseForge,
                mod_.source,
                Arc::clone(&zip_arc),
            )),
        ));
    }
    for (cfg_id, task) in zip_dl_tasks {
        task.await
            .expect("task panicked")
            .map_err(|e| CreateModrinthPackError::ZipMod(cfg_id.clone(), e))?;
        progress_bar.inc(1);
    }
    let mut zip = Arc::into_inner(zip_arc)
        .expect("all zip tasks should be finished")
        .into_inner();

    progress_bar.set_style(ProgressStyle::default_spinner());
    progress_bar.set_message("Copying overrides...");
    zip_dir(
        source_dir.join(LIT_OVERRIDES),
        &mut zip,
        LIT_OVERRIDES,
        CreateModrinthPackError::ZipDir,
    )?;
    progress_bar.set_message("Copying client-only overrides...");
    zip_dir(
        source_dir.join(LIT_CLIENT_OVERRIDES),
        &mut zip,
        LIT_CLIENT_OVERRIDES,
        CreateModrinthPackError::ZipDir,
    )?;
    progress_bar.set_message("Copying server-only overrides...");
    zip_dir(
        source_dir.join(LIT_SERVER_OVERRIDES),
        &mut zip,
        LIT_SERVER_OVERRIDES,
        CreateModrinthPackError::ZipDir,
    )?;

    progress_bar.set_message("Writing manifest...");

    let forge =
        (pack.mod_loader.id == ModLoaderType::Forge).then(|| pack.mod_loader.version.clone());
    let neoforge =
        (pack.mod_loader.id == ModLoaderType::Neoforge).then(|| pack.mod_loader.version.clone());
    let fabric_loader =
        (pack.mod_loader.id == ModLoaderType::Fabric).then(|| pack.mod_loader.version.clone());
    let quilt_loader =
        (pack.mod_loader.id == ModLoaderType::Quilt).then(|| pack.mod_loader.version.clone());

    let manifest = ModrinthManifest {
        format_version: 1,
        game: modrinth_manifest::Game::Minecraft,
        version_id: pack.version.clone(),
        name: pack.name.clone(),
        summary: Some(pack.description.clone()),
        files: modrinth_files,
        dependencies: modrinth_manifest::GameDependencies {
            minecraft: pack.minecraft_version.clone(),
            forge,
            neoforge,
            fabric_loader,
            quilt_loader,
        },
    };
    zip.start_file("modrinth.index.json", *ZIP_OPTIONS)?;
    serde_json::to_writer(&mut zip, &manifest)?;

    progress_bar.set_message("Flushing zip...");

    zip.finish()?;

    multi.remove(&progress_bar);

    action_pb.disable_steady_tick();
    action_pb.finish_with_message(format!(
        "Created Modrinth zip at '{}'.",
        output_file.display()
    ));

    Ok(())
}

#[derive(Debug, Error)]
pub enum CreateServerBaseError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Cloning directory {0} failed: {1}")]
    CloneDir(String, #[source] CloneDirError),
    #[error("Error downloading mods: {0}")]
    ModDownload(#[from] ModsDownloadError),
}

pub async fn create_server_base(
    mods: &ModConfig,
    source_dir: &Path,
    output_dir: PathBuf,
) -> Result<(), CreateServerBaseError> {
    let multi = MultiProgress::new();
    let action_pb = multi.add(ProgressBar::new_spinner().with_message(format!(
        "Creating server base at '{}'...",
        output_dir.display()
    )));
    action_pb.enable_steady_tick(steady_tick_duration());

    let progress_bar = multi.add(ProgressBar::new_spinner());
    progress_bar.enable_steady_tick(steady_tick_duration());

    // Wipe the output dir first, so we don't have leftover files
    // Yes this defeats the hash check for now. TODO: cache files for the user as a whole
    if output_dir.exists() {
        progress_bar.set_message("Cleaning output directory");
        std::fs::remove_dir_all(&output_dir)?;
    }

    std::fs::create_dir_all(&output_dir)?;
    let mods_folder = output_dir.join(LIT_MODS);
    std::fs::create_dir_all(&mods_folder)?;

    progress_bar.set_message("Copying overrides");
    clone_dir(
        source_dir.join(LIT_OVERRIDES),
        &output_dir,
        CreateServerBaseError::CloneDir,
    )?;
    progress_bar.set_message("Copying server-specific overrides");
    clone_dir(
        source_dir.join(LIT_SERVER_OVERRIDES),
        &output_dir,
        CreateServerBaseError::CloneDir,
    )?;

    progress_bar.set_message("Downloading remote mods...");
    download_mods(mods, multi.clone(), &mods_folder, |side| side.on_server()).await?;

    multi.remove(&progress_bar);
    action_pb.set_message(format!("Created server base at '{}'", output_dir.display()));

    Ok(())
}

#[derive(Debug, Error)]
pub enum CloneDirError {
    #[error("I/O Error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Walk Error: {0}")]
    Walk(#[from] walkdir::Error),
}

fn clone_dir<F, T, E, EF>(from: F, to: T, error_mapper: EF) -> Result<(), E>
where
    F: AsRef<Path>,
    T: AsRef<Path>,
    EF: FnOnce(String, CloneDirError) -> E,
{
    let from = from.as_ref();
    tokio::task::block_in_place(|| clone_dir_impl(from, to))
        .map_err(|e| error_mapper(from.display().to_string(), e))
}

/// Walk [from] and clone its files to [to].
fn clone_dir_impl<F: AsRef<Path>, T: AsRef<Path>>(from: F, to: T) -> Result<(), CloneDirError> {
    let from = from.as_ref();
    let to = to.as_ref();
    if !from.exists() {
        log::debug!("Skipped cloning {} as it did not exist", from.display());
        return Ok(());
    }
    std::fs::create_dir_all(to)?;
    for entry in WalkDir::new(from) {
        let entry = entry?;
        let ft = entry.file_type();
        let src_path = entry.into_path();
        let dest_path = to.join(
            src_path
                .strip_prefix(from)
                .expect("walked path must contain `from` as prefix"),
        );
        if ft.is_dir() {
            match std::fs::create_dir(&dest_path) {
                Ok(_) => log::debug!("Created directory {}", dest_path.display()),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    log::debug!("Directory {} already exists", dest_path.display())
                }
                Err(e) => return Err(e.into()),
            }
        } else if ft.is_file() {
            let mut done = false;
            while !done {
                if dest_path.exists() {
                    std::fs::remove_file(&dest_path)?;
                }
                match reflink_or_copy(&src_path, &dest_path) {
                    Ok(v) => {
                        done = true;
                        match v {
                            Some(_) => log::debug!(
                                "Copied {} to {}",
                                src_path.display(),
                                dest_path.display()
                            ),
                            None => log::debug!(
                                "Reflinked {} to {}",
                                src_path.display(),
                                dest_path.display()
                            ),
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                        // Loop to try again.
                    }
                    Err(e) => return Err(e.into()),
                }
            }
        } else {
            log::debug!(
                "Skipped {} as it is not a regular file or directory",
                src_path.display()
            );
        }
    }

    Ok(())
}

#[derive(Debug, Error)]
pub enum ZipDirError {
    #[error("I/O Error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Walk Error: {0}")]
    Walk(#[from] walkdir::Error),
    #[error("Zip Error: {0}")]
    Zip(#[from] zip::result::ZipError),
}

/// Walk [from] and zip its files to [to].
fn zip_dir<F, W, E, EF>(
    from: F,
    to: &mut ZipWriter<W>,
    to_prefix: &str,
    error_mapper: EF,
) -> Result<(), E>
where
    F: AsRef<Path>,
    W: Write + Seek,
    EF: FnOnce(String, ZipDirError) -> E,
{
    fn zip_dir_impl<F: AsRef<Path>, W: Write + Seek>(
        from: F,
        to: &mut ZipWriter<W>,
        to_prefix: &str,
    ) -> Result<(), ZipDirError> {
        let from = from.as_ref();
        if !from.exists() {
            log::debug!("Skipped zipping {} as it did not exist", from.display());
            return Ok(());
        }
        for entry in WalkDir::new(from) {
            let entry = entry?;
            let ft = entry.file_type();
            let src_path = entry.into_path();
            let dest_path = [
                to_prefix,
                src_path
                    .strip_prefix(from)
                    .expect("walked path must contain `from` as prefix")
                    .to_str()
                    .expect("must be zip-able path"),
            ]
            .join("/");
            if ft.is_file() {
                to.start_file(&dest_path, *ZIP_OPTIONS)?;
                std::io::copy(&mut std::fs::File::open(&src_path)?, to)?;
                log::debug!("Copied {} to {}", src_path.display(), dest_path);
            } else {
                log::debug!("Skipped {} as it is not a regular file", src_path.display());
            }
        }

        Ok(())
    }

    let from = from.as_ref();
    tokio::task::block_in_place(|| zip_dir_impl(from, to, to_prefix))
        .map_err(|e| error_mapper(from.display().to_string(), e))
}

#[derive(Debug, Error)]
pub enum ZipModError {
    #[error("I/O Error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Mod loading Error: {0}")]
    ModLoading(#[from] ModLoadingError),
    #[error("Mod download Error: {0}")]
    ModDownload(#[from] ModDownloadError),
    #[error("Zip Error: {0}")]
    Zip(#[from] zip::result::ZipError),
}

async fn add_mod_to_zip<M: ModSite, W>(
    multi: MultiProgress,
    mod_site: M,
    mod_id: ModId<M::Id>,
    zip: Arc<Mutex<ZipWriter<W>>>,
) -> Result<(), ZipModError>
where
    W: Write + Seek,
{
    let mod_info = mod_site.load_file(mod_id.clone()).await?;
    let progress_bar = multi.add(
        ProgressBar::new(mod_info.file_length)
            .with_style(style_bar())
            .with_message(mod_info.filename.clone()),
    );

    let mut zip = zip.lock().await;
    zip.start_file(
        [LIT_OVERRIDES, LIT_MODS, &mod_info.filename].join("/"),
        *ZIP_OPTIONS,
    )?;

    let mut content = progress_bar.wrap_async_read(mod_site.download(mod_id).await?);
    tokio::task::block_in_place(|| {
        std::io::copy(&mut SyncIoBridge::new(&mut content), zip.deref_mut())
    })?;
    drop(zip);

    progress_bar.reset();
    progress_bar.set_style(ProgressStyle::default_spinner());
    progress_bar.finish_with_message(format!(
        "[{}] Mod {} downloaded.",
        M::NAME,
        mod_info.filename,
    ));

    Ok(())
}
