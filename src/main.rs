use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::process::Termination;

use clap::Parser;
use log::LevelFilter;
use serde::de::DeserializeOwned;
use thiserror::Error;

use crate::checks::verify_mods::{verify_mods, ModsVerificationError};
use crate::config::mods::ModConfig;
use crate::config::pack::PackConfig;
use crate::output::{
    create_curseforge_zip, create_modrinth_pack, create_server_base, CreateCurseForgeZipError,
    CreateModrinthPackError, CreateServerBaseError,
};

mod checks;
mod config;
mod mod_site;
mod output;
mod progress;

/// Handles files for a Minecraft modpack.
///
/// General layout of a `netherfire` modpack source directory:
/// - `config.toml` file for general configuration (mod loader, MC version, etc.)
/// - `mods.toml` file for CurseForge or Modrinth mods
/// - `overrides/` directory for anything that should be added to the base game folder (put other `mods/` here!)
/// - `client-overrides/` directory for client-only `overrides/`
/// - `server-overrides/` directory for server-only `overrides/`
#[derive(Parser)]
#[clap(verbatim_doc_comment)]
pub struct Netherfire {
    /// Modpack source folder.
    pub source: PathBuf,
    /// Write a CurseForge-format client modpack ZIP to the given path.
    /// The path should be a directory, the ZIP will be written under it.
    #[clap(long)]
    pub create_curseforge_zip: Option<PathBuf>,
    /// Write a Modrinth `.mrpack` to the given path.
    /// The path should be a directory, the pack will be written under it.
    #[clap(long)]
    pub create_modrinth_pack: Option<PathBuf>,
    /// Produce a server base folder by downloading mods if needed.
    #[clap(long)]
    pub create_server_base: Option<PathBuf>,
    /// Verbosity level, repeat to increase.
    #[clap(short, long, action = clap::ArgAction::Count)]
    pub verbose: u8,
}

#[derive(Debug, Error)]
enum NetherfireError {
    #[error("Modpack configuration load error: {0}")]
    PackConfigLoad(#[from] PackConfigLoadError),
    #[error("Mod configuration load error: {0}")]
    ModConfigLoad(#[from] ModConfigLoadError),
    #[error("Mod verification errors: {0}")]
    ModVerification(#[from] ModsVerificationError),
    #[error("Create CurseForge ZIP error: {0}")]
    CreateCurseForgeZip(#[from] CreateCurseForgeZipError),
    #[error("Create Modrinth Pack error: {0}")]
    CreateModrinthPack(#[from] CreateModrinthPackError),
    #[error("Create server base error: {0}")]
    CreateServerBase(#[from] CreateServerBaseError),
}

#[derive(Debug, Error)]
enum PackConfigLoadError {
    #[error("I/O Error: {0}")]
    Io(#[from] std::io::Error),
    #[error("TOML Parse Error: {0}")]
    TomlParse(#[from] toml::de::Error),
}

#[derive(Debug, Error)]
enum ModConfigLoadError {
    #[error("I/O Error: {0}")]
    Io(#[from] std::io::Error),
    #[error("TOML Parse Error: {0}")]
    TomlParse(#[from] toml::de::Error),
}

impl Termination for NetherfireError {
    fn report(self) -> ExitCode {
        // Might split this up later.
        ExitCode::FAILURE
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let args: Netherfire = Netherfire::parse();
    env_logger::Builder::new()
        .filter_level(match args.verbose {
            0 => LevelFilter::Info,
            1 => LevelFilter::Debug,
            _ => LevelFilter::Trace,
        })
        .init();

    match main_for_result(args).await {
        Ok(_) => ExitCode::SUCCESS,
        Err(e) => {
            log::error!("{:#}", e);
            e.report()
        }
    }
}

async fn main_for_result(args: Netherfire) -> Result<(), NetherfireError> {
    let pack_config =
        toml_load::<_, PackConfig, PackConfigLoadError>(args.source.join("config.toml"))?;
    let mod_config = toml_load::<_, ModConfig, ModConfigLoadError>(args.source.join("mods.toml"))?;

    verify_mods(&mod_config, &pack_config.minecraft_version).await?;

    if let Some(cf_zip) = args.create_curseforge_zip {
        create_curseforge_zip(&pack_config, &mod_config, &args.source, cf_zip).await?;
    }

    if let Some(mrpack) = args.create_modrinth_pack {
        create_modrinth_pack(&pack_config, &mod_config, &args.source, mrpack).await?;
    }

    if let Some(server_base_dir) = args.create_server_base {
        create_server_base(&mod_config, &args.source, server_base_dir).await?;
    }

    Ok(())
}

fn toml_load<P, C, E>(path: P) -> Result<C, E>
where
    P: AsRef<Path>,
    C: DeserializeOwned,
    E: From<std::io::Error> + From<toml::de::Error>,
{
    let s = std::fs::read_to_string(path)?;
    toml::from_str::<C>(&s).map_err(Into::into)
}
