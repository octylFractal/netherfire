use std::io::{Seek, Write};
use std::path::{Path, PathBuf};

use once_cell::sync::Lazy;
use reflink::reflink_or_copy;
use thiserror::Error;
use walkdir::WalkDir;
use zip::{CompressionMethod, ZipWriter};

use crate::config::mods::ModsDownloadError;
use crate::output::curseforge_manifest::{
    CurseForgeManifest, ManifestFile, ManifestType, Minecraft, ModLoader,
};
use crate::{ModConfig, PackConfig};

mod curseforge_manifest;

const LIT_MODS: &str = "mods";
const LIT_OVERRIDES: &str = "overrides";
const LIT_SERVER: &str = "_SERVER";
const LIT_CLIENT: &str = "_CLIENT";

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
}

static ZIP_OPTIONS: Lazy<zip::write::FileOptions> = Lazy::new(|| {
    zip::write::FileOptions::default().compression_method(CompressionMethod::Deflated)
});

pub fn create_curseforge_zip(
    pack: &PackConfig,
    mods: &ModConfig,
    source_dir: &Path,
    output_dir: PathBuf,
) -> Result<(), CreateCurseForgeZipError> {
    if !mods.mods.modrinth.is_empty() {
        todo!("Download Modrinth mods and add them to the zip")
    }

    std::fs::create_dir_all(&output_dir)?;
    let output_file = output_dir.join(format!("{} ({}).zip", pack.name, pack.version));

    let mut zip = ZipWriter::new(std::fs::File::create(output_file)?);

    zip_dir(
        source_dir.join(LIT_MODS),
        &mut zip,
        &[LIT_OVERRIDES, LIT_MODS].join("/"),
        CreateCurseForgeZipError::ZipDir,
    )?;
    zip_dir(
        source_dir.join(LIT_CLIENT).join(LIT_MODS),
        &mut zip,
        &[LIT_OVERRIDES, LIT_MODS].join("/"),
        CreateCurseForgeZipError::ZipDir,
    )?;
    zip_dir(
        source_dir.join(LIT_OVERRIDES),
        &mut zip,
        LIT_OVERRIDES,
        CreateCurseForgeZipError::ZipDir,
    )?;
    zip_dir(
        source_dir.join(LIT_CLIENT).join(LIT_OVERRIDES),
        &mut zip,
        LIT_OVERRIDES,
        CreateCurseForgeZipError::ZipDir,
    )?;

    let manifest = CurseForgeManifest {
        minecraft: Minecraft {
            version: pack.minecraft_version.clone(),
            mod_loaders: vec![ModLoader {
                id: pack.mod_loader.clone(),
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
                file_id: m.source.file_id,
                required: true,
            })
            .collect(),
        overrides: LIT_OVERRIDES.to_string(),
    };
    zip.start_file("manifest.json", *ZIP_OPTIONS)?;
    serde_json::to_writer(&mut zip, &manifest)?;

    zip.finish()?;

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
    // Wipe the output dir first, so we don't have leftover files
    std::fs::remove_dir_all(&output_dir)?;

    clone_dir(
        source_dir.join(LIT_MODS),
        output_dir.join(LIT_MODS),
        CreateServerBaseError::CloneDir,
    )?;
    clone_dir(
        source_dir.join(LIT_SERVER).join(LIT_MODS),
        output_dir.join(LIT_MODS),
        CreateServerBaseError::CloneDir,
    )?;
    clone_dir(
        source_dir.join(LIT_OVERRIDES),
        &output_dir,
        CreateServerBaseError::CloneDir,
    )?;
    clone_dir(
        source_dir.join(LIT_SERVER).join(LIT_OVERRIDES),
        &output_dir,
        CreateServerBaseError::CloneDir,
    )?;

    mods.download(&output_dir.join(LIT_MODS), |side| side.on_server())
        .await?;

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
    let from = from.as_ref();
    tokio::task::block_in_place(|| zip_dir_impl(from, to, to_prefix))
        .map_err(|e| error_mapper(from.display().to_string(), e))
}

/// Walk [from] and zip its files to [to].
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
