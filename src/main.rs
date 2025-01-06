use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::process::Termination;

use clap::Parser;
use log::LevelFilter;
use thiserror::Error;

use crate::checks::verify_mods::{verify_mods, ModsVerificationError};
use crate::config::mods::ConfigModContainer;
use crate::config::pack::PackConfig;
use crate::output::{
    create_curseforge_zip, create_modrinth_pack, create_server_base, CreateCurseForgeZipError,
    CreateModrinthPackError, CreateServerBaseError,
};

mod checks;
mod config;
mod mod_site;
mod output;
mod uwu_colors;

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
    ///
    /// The CurseForge modpack format does not support optional mods, so all optional mods will be
    /// marked as required or included in the ZIP by default. To disable this, pass
    /// `--no-cf-zip-include-optional`.
    #[clap(long)]
    pub create_curseforge_zip: Option<PathBuf>,
    /// Should clientside-optional mods be included in the CurseForge ZIP?
    #[clap(long, requires("create_curseforge_zip"))]
    pub no_cf_zip_include_optional: bool,
    /// Write a Modrinth `.mrpack` to the given path.
    /// The path should be a directory, the pack will be written under it.
    ///
    /// Modrinth supports optional mods, so optional mods will be marked as such in the pack.
    /// However, CurseForge mods cannot be marked as optional, so they will be included in the ZIP.
    /// To disable this, pass `--no-mrpack-include-optional`.
    #[clap(long)]
    pub create_modrinth_pack: Option<PathBuf>,
    /// Should CurseForge optional mods be included in the Modrinth pack?
    #[clap(long, requires("create_modrinth_pack"))]
    pub no_mrpack_include_optional: bool,
    /// Produce a server base folder by downloading mods if needed.
    ///
    /// Optional mods will be included by default. To disable this, pass
    /// `--no-server-base-include-optional`.
    #[clap(long)]
    pub create_server_base: Option<PathBuf>,
    /// Should optional mods be included in the server base?
    #[clap(long, requires("create_server_base"))]
    pub no_server_base_include_optional: bool,
    /// Verbosity level, repeat to increase.
    #[clap(short, action = clap::ArgAction::Count)]
    pub verbosity: u8,
}

#[derive(Debug, Error)]
enum NetherfireError {
    #[error("Modpack configuration load error: {0}")]
    PackConfigLoad(#[from] ConfigLoadError),
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
enum ConfigLoadError {
    #[error("I/O Error on config.toml: {0}")]
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
    let verbosity = args.verbosity;
    env_logger::Builder::new()
        .filter_level(match verbosity {
            0 => LevelFilter::Info,
            1 => LevelFilter::Debug,
            _ => LevelFilter::Trace,
        })
        .format(move |buf, record| {
            write!(buf, "[{}] ", buf.default_level_style(record.level()))?;

            if verbosity > 0 {
                // Include the location of the log message if verbose.
                if let Some(p) = record.module_path() {
                    write!(buf, "[{}] ", p)?;
                } else {
                    write!(buf, "[unknown] ")?;
                }
            }

            writeln!(buf, "{}", record.args())
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
    let path = args.source.join("config.toml");
    let s = std::fs::read_to_string(path).map_err(ConfigLoadError::from)?;
    let pack_config =
        toml::from_str::<PackConfig<ConfigModContainer>>(&s).map_err(ConfigLoadError::from)?;

    let pack_config = verify_mods(pack_config).await?;

    if let Some(cf_zip) = args.create_curseforge_zip {
        create_curseforge_zip(
            &pack_config,
            &args.source,
            cf_zip,
            !args.no_cf_zip_include_optional,
        )
        .await?;
    }

    if let Some(mrpack) = args.create_modrinth_pack {
        create_modrinth_pack(
            &pack_config,
            &args.source,
            mrpack,
            !args.no_mrpack_include_optional,
        )
        .await?;
    }

    if let Some(server_base_dir) = args.create_server_base {
        create_server_base(
            &pack_config,
            &args.source,
            server_base_dir,
            !args.no_server_base_include_optional,
        )
        .await?;
    }

    Ok(())
}
