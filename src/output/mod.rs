use std::io::{Seek, Write};
use std::ops::DerefMut;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use once_cell::sync::Lazy;
use reflink::reflink_or_copy;
use thiserror::Error;
use tokio::spawn;
use tokio::sync::Mutex;
use tokio_util::io::SyncIoBridge;
use walkdir::WalkDir;
use zip::{CompressionMethod, ZipWriter};

use crate::checks::verify_mods::{VerifiedMod, VerifiedModContainer};
use crate::config::pack::ModLoaderType;
use crate::mod_site::ModSite;
use crate::output::curseforge_manifest::{
    CurseForgeManifest, ManifestFile, ManifestType, Minecraft, ModLoader,
};
use crate::output::mod_download::{
    download_mods, mod_download, ModDownloadError, ModsDownloadError,
};
use crate::output::modrinth_manifest::ModrinthManifest;
use crate::uwu_colors::{ErrStyle, FILE_STYLE, SITE_NAME_STYLE};
use crate::PackConfig;

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

static ZIP_OPTIONS: Lazy<zip::write::FileOptions<()>> = Lazy::new(|| {
    zip::write::FileOptions::default().compression_method(CompressionMethod::Deflated)
});

pub async fn create_curseforge_zip(
    pack: &PackConfig<VerifiedModContainer>,
    source_dir: &Path,
    output_dir: PathBuf,
    include_optional: bool,
) -> Result<(), CreateCurseForgeZipError> {
    let output_file = output_dir.join(format!("{} ({}).zip", pack.name, pack.version));

    log::info!(
        "Creating CurseForge zip at '{}'...",
        output_file.display().errstyle(FILE_STYLE)
    );

    std::fs::create_dir_all(&output_dir)?;

    let zip = ZipWriter::new(std::fs::File::create(&output_file)?);

    log::info!(
        "Downloading {} mods...",
        "Modrinth".errstyle(SITE_NAME_STYLE)
    );

    let zip_arc = Arc::new(Mutex::new(zip));
    let mut zip_dl_tasks = Vec::with_capacity(pack.mods.modrinth.len());
    for (cfg_id, mod_) in &pack.mods.modrinth {
        if !mod_.env_requirements.client.is_needed(include_optional) {
            continue;
        }
        zip_dl_tasks.push((
            cfg_id,
            spawn(add_mod_to_zip(
                mod_.clone(),
                LIT_OVERRIDES,
                Arc::clone(&zip_arc),
            )),
        ));
    }
    for (cfg_id, task) in zip_dl_tasks {
        task.await
            .expect("task panicked")
            .map_err(|e| CreateCurseForgeZipError::ZipMod(cfg_id.clone(), e))?;
    }
    let mut zip = Arc::into_inner(zip_arc)
        .expect("all zip tasks should be finished")
        .into_inner();

    log::info!("Copying overrides...");
    zip_dir(
        source_dir.join(LIT_OVERRIDES),
        &mut zip,
        LIT_OVERRIDES,
        CreateCurseForgeZipError::ZipDir,
    )?;
    log::info!("Copying client-only overrides...");
    zip_dir(
        source_dir.join(LIT_CLIENT_OVERRIDES),
        &mut zip,
        LIT_OVERRIDES,
        CreateCurseForgeZipError::ZipDir,
    )?;

    log::info!("Writing manifest...");
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
        files: pack
            .mods
            .curseforge
            .values()
            .filter(|m| m.env_requirements.client.is_needed(include_optional))
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

    log::info!("Flushing zip...");

    zip.finish()?;

    log::info!(
        "Created CurseForge zip at '{}'.",
        output_file.display().errstyle(FILE_STYLE)
    );

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
}

pub async fn create_modrinth_pack(
    pack: &PackConfig<VerifiedModContainer>,
    source_dir: &Path,
    output_dir: PathBuf,
    include_optional: bool,
) -> Result<(), CreateModrinthPackError> {
    let output_file = output_dir.join(format!("{} ({}).mrpack", pack.name, pack.version));

    log::info!(
        "Creating Modrinth pack at '{}'...",
        output_file.display().errstyle(FILE_STYLE)
    );

    std::fs::create_dir_all(&output_dir)?;

    let mut modrinth_files = Vec::with_capacity(pack.mods.modrinth.len());
    for mod_ in pack.mods.modrinth.values() {
        let mod_info = &mod_.info;
        modrinth_files.push(modrinth_manifest::ModFile {
            path: format!("mods/{}", mod_info.filename),
            hashes: modrinth_manifest::ModFileHashes {
                sha1: format!("{:x}", mod_info.hash.sha1),
                sha512: format!("{:x}", mod_info.hash.sha512),
            },
            env: Some(mod_.env_requirements.into()),
            downloads: vec![mod_info.url.clone().expect("verified earlier")],
            file_size: mod_info.file_length,
        });
    }

    log::info!(
        "Downloading {} mods...",
        "CurseForge".errstyle(SITE_NAME_STYLE)
    );

    let zip = ZipWriter::new(std::fs::File::create(&output_file)?);

    let zip_arc = Arc::new(Mutex::new(zip));
    let mut zip_dl_tasks = Vec::with_capacity(pack.mods.curseforge.len());
    for (cfg_id, mod_) in &pack.mods.curseforge {
        let overrides = match (
            mod_.env_requirements.client.is_needed(include_optional),
            mod_.env_requirements.server.is_needed(include_optional),
        ) {
            (true, true) => LIT_OVERRIDES,
            (true, false) => LIT_CLIENT_OVERRIDES,
            (false, true) => LIT_SERVER_OVERRIDES,
            (false, false) => continue,
        };
        zip_dl_tasks.push((
            cfg_id,
            spawn(add_mod_to_zip(
                mod_.clone(),
                overrides,
                Arc::clone(&zip_arc),
            )),
        ));
    }
    for (cfg_id, task) in zip_dl_tasks {
        task.await
            .expect("task panicked")
            .map_err(|e| CreateModrinthPackError::ZipMod(cfg_id.clone(), e))?;
    }
    let mut zip = Arc::into_inner(zip_arc)
        .expect("all zip tasks should be finished")
        .into_inner();

    log::info!("Copying overrides...");
    zip_dir(
        source_dir.join(LIT_OVERRIDES),
        &mut zip,
        LIT_OVERRIDES,
        CreateModrinthPackError::ZipDir,
    )?;
    log::info!("Copying client-only overrides...");
    zip_dir(
        source_dir.join(LIT_CLIENT_OVERRIDES),
        &mut zip,
        LIT_CLIENT_OVERRIDES,
        CreateModrinthPackError::ZipDir,
    )?;
    log::info!("Copying server-only overrides...");
    zip_dir(
        source_dir.join(LIT_SERVER_OVERRIDES),
        &mut zip,
        LIT_SERVER_OVERRIDES,
        CreateModrinthPackError::ZipDir,
    )?;

    log::info!("Writing manifest...");

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

    log::info!("Flushing zip...");

    zip.finish()?;

    log::info!(
        "Created Modrinth pack at '{}'.",
        output_file.display().errstyle(FILE_STYLE)
    );

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
    pack: &PackConfig<VerifiedModContainer>,
    source_dir: &Path,
    output_dir: PathBuf,
    include_optional: bool,
) -> Result<(), CreateServerBaseError> {
    log::info!(
        "Creating server base at '{}'...",
        output_dir.display().errstyle(FILE_STYLE)
    );

    // Wipe the output dir first, so we don't have leftover files
    // Yes this defeats the hash check for now. TODO: cache files for the user as a whole
    if output_dir.exists() {
        log::info!("Removing existing server base...");
        std::fs::remove_dir_all(&output_dir)?;
    }

    std::fs::create_dir_all(&output_dir)?;
    let mods_folder = output_dir.join(LIT_MODS);
    std::fs::create_dir_all(&mods_folder)?;

    log::info!("Copying overrides...");
    clone_dir(
        source_dir.join(LIT_OVERRIDES),
        &output_dir,
        CreateServerBaseError::CloneDir,
    )?;
    log::info!("Copying server-only overrides...");
    clone_dir(
        source_dir.join(LIT_SERVER_OVERRIDES),
        &output_dir,
        CreateServerBaseError::CloneDir,
    )?;

    download_mods(pack, &mods_folder, |reqs| {
        reqs.server.is_needed(include_optional)
    })
    .await?;

    log::info!(
        "Created server base at '{}'.",
        output_dir.display().errstyle(FILE_STYLE)
    );

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
                to.start_file(&*dest_path, *ZIP_OPTIONS)?;
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
    #[error("Mod download Error: {0}")]
    ModDownload(#[from] ModDownloadError),
    #[error("Zip Error: {0}")]
    Zip(#[from] zip::result::ZipError),
}

async fn add_mod_to_zip<S: ModSite, W>(
    mod_: VerifiedMod<S>,
    dest_overrides: &'static str,
    zip: Arc<Mutex<ZipWriter<W>>>,
) -> Result<(), ZipModError>
where
    W: Write + Seek,
{
    let mod_info = mod_.info;

    let mut zip = zip.lock().await;
    zip.start_file(
        [dest_overrides, LIT_MODS, &mod_info.filename].join("/"),
        *ZIP_OPTIONS,
    )?;

    let mut content = mod_download(mod_info.url.expect("verified earlier")).await?;
    tokio::task::block_in_place(|| {
        std::io::copy(&mut SyncIoBridge::new(&mut content), zip.deref_mut())
    })?;
    drop(zip);

    log::info!(
        "[{}] Mod {} downloaded.",
        S::NAME.errstyle(SITE_NAME_STYLE),
        mod_info.filename.errstyle(FILE_STYLE),
    );

    Ok(())
}
