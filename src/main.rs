use crate::checks::verify_mods::{verify_mods, ModsVerificationError};
use crate::config::mods::{ConfigMod, ConfigModContainer, EnvRequirement};
use crate::config::pack::PackConfig;
use crate::mod_site::{CurseForge, ModIdValue, ModLoadingError, ModSite, Modrinth};
use crate::output::{
    create_curseforge_zip, create_modrinth_pack, create_server_base, CreateCurseForgeZipError,
    CreateModrinthPackError, CreateServerBaseError,
};
use clap::{Args, Parser, Subcommand};
use log::LevelFilter;
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::process::Termination;
use std::str::FromStr;
use thiserror::Error;
use toml_edit::DocumentMut;

mod checks;
mod config;
mod mod_site;
mod output;
mod uwu_colors;

/// Handles files for a Minecraft modpack.
///
/// General layout of a `netherfire` modpack source directory:
/// - `config.toml` file for general configuration (mod loader, MC version, mods, etc.)
/// - `overrides/` directory for anything that should be added to the base game folder (put other `mods/` here!)
/// - `client-overrides/` directory for client-only `overrides/`
/// - `server-overrides/` directory for server-only `overrides/`
#[derive(Parser)]
#[clap(verbatim_doc_comment)]
struct Netherfire {
    /// Verbosity level, repeat to increase.
    #[clap(short, action = clap::ArgAction::Count)]
    pub verbosity: u8,
    #[clap(subcommand)]
    pub subcommand: NetherfireCommand,
}

#[derive(Subcommand)]
enum NetherfireCommand {
    AddMods(AddMods),
    Generate(Generate),
}

/// Add mods to a modpack.
#[derive(Args)]
struct AddMods {
    /// Modpack source folder.
    pub source: PathBuf,
    /// Mod site to load mods from.
    #[clap(subcommand)]
    pub mod_site: AddModsFrom,
}

#[derive(Subcommand)]
enum AddModsFrom {
    #[clap(name = "curseforge")]
    CurseForge(AddModsFromCurseForge),
    Modrinth(AddModsFromModrinth),
}

/// Add mods to a modpack from CurseForge.
#[derive(Args)]
struct AddModsFromCurseForge {
    /// CurseForge project IDs to add. The latest version matching the modpack's Minecraft version
    /// will be added.
    pub project_ids: Vec<i32>,
}

/// Add mods to a modpack from Modrinth.
#[derive(Args)]
struct AddModsFromModrinth {
    /// Modrinth project IDs to add. The latest version matching the modpack's Minecraft version
    /// will be added.
    pub project_ids: Vec<String>,
}

/// Generate modpack artifacts.
#[derive(Args)]
struct Generate {
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
}

#[derive(Debug, Error)]
enum NetherfireError {
    #[error("Add mods error: {0}")]
    AddMods(#[from] AddModsError),
    #[error("Generate modpack error: {0}")]
    GenerateModpack(#[from] GenerateModpackError),
}

#[derive(Debug, Error)]
enum ConfigLoadError {
    #[error("I/O Error on config.toml: {0}")]
    Io(#[from] std::io::Error),
    #[error("TOML serde Parse Error: {0}")]
    TomlSerdeParse(#[from] toml_edit::de::Error),
    #[error("TOML Parse Error: {0}")]
    TomlParse(#[from] toml_edit::TomlError),
}

#[derive(Debug, Error)]
enum ConfigEditError {
    #[error("Mods section {0} must be a table")]
    ModsNotTable(String),
    #[error("I/O Error on config.toml: {0}")]
    Io(#[from] std::io::Error),
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
            write!(buf, "{}", buf.default_level_style(record.level()))?;

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
    match args.subcommand {
        NetherfireCommand::AddMods(add_mods) => add_mods_to_modpack(add_mods).await?,
        NetherfireCommand::Generate(generate) => generate_modpack(generate).await?,
    }
    Ok(())
}

fn load_pack_config(pack_source: &Path) -> Result<PackConfig<ConfigModContainer>, ConfigLoadError> {
    let s = load_pack_config_str(pack_source)?;
    let pack_config = toml_edit::de::from_str::<PackConfig<ConfigModContainer>>(&s)
        .map_err(ConfigLoadError::from)?;
    Ok(pack_config)
}

fn load_pack_config_str(pack_source: &Path) -> Result<String, ConfigLoadError> {
    let path = get_pack_config_path(pack_source);
    let s = std::fs::read_to_string(path).map_err(ConfigLoadError::from)?;
    Ok(s)
}

fn get_pack_config_path(pack_source: &Path) -> PathBuf {
    pack_source.join("config.toml")
}

#[derive(Debug, Error)]
enum AddModsError {
    #[error("Modpack configuration load error: {0}")]
    PackConfigLoad(#[from] ConfigLoadError),
    #[error("Modpack configuration edit error: {0}")]
    PackConfigEdit(#[from] ConfigEditError),
    #[error("Mod loading error: {0}")]
    ModLoadingError(#[from] ModLoadingError),
}

async fn add_mods_to_modpack(args: AddMods) -> Result<(), AddModsError> {
    let pack_config = load_pack_config(&args.source)?;
    let config_str = load_pack_config_str(&args.source)?;
    let mut editable_config = DocumentMut::from_str(&config_str).map_err(ConfigLoadError::from)?;
    match args.mod_site {
        AddModsFrom::CurseForge(cf) => {
            add_mods_from_site(
                CurseForge,
                &pack_config,
                &pack_config.mods.curseforge,
                editable_config["mods"]["curseforge"]
                    .as_table_mut()
                    .ok_or_else(|| ConfigEditError::ModsNotTable("curseforge".to_string()))?,
                cf.project_ids,
            )
            .await?;
        }
        AddModsFrom::Modrinth(mr) => {
            add_mods_from_site(
                Modrinth,
                &pack_config,
                &pack_config.mods.modrinth,
                editable_config["mods"]["modrinth"]
                    .as_table_mut()
                    .ok_or_else(|| ConfigEditError::ModsNotTable("modrinth".to_string()))?,
                mr.project_ids,
            )
            .await?;
        }
    }
    let new_config_str = editable_config.to_string();
    if config_str == new_config_str {
        log::info!("No changes made to config.toml");
        return Ok(());
    }
    // Backup existing config for safety
    let config_path = get_pack_config_path(&args.source);
    std::fs::copy(&config_path, config_path.with_extension("toml.bak"))
        .map_err(ConfigEditError::from)?;
    // Write new config
    std::fs::write(config_path, new_config_str).map_err(ConfigEditError::from)?;

    Ok(())
}

async fn add_mods_from_site<ID: ModIdValue>(
    site: impl ModSite<Id = ID>,
    pack_config: &PackConfig<ConfigModContainer>,
    original_mods_bucket: &HashMap<String, ConfigMod<ID>>,
    mods_bucket: &mut toml_edit::Table,
    project_ids: Vec<ID>,
) -> Result<(), AddModsError> {
    let project_id_to_key_version_index: HashMap<_, _> = original_mods_bucket
        .iter()
        .map(|(key, mod_entry)| {
            (
                mod_entry.source.project_id.clone(),
                (key.clone(), mod_entry.source.version_id.clone()),
            )
        })
        .collect();
    for project_id in project_ids {
        log::info!("Loading metadata for project ID {:?}...", project_id);
        let Some(latest_version) = site
            .get_latest_version_for_pack(pack_config, project_id.clone())
            .await?
        else {
            log::warn!("No valid version found for project ID {:?}", project_id);
            continue;
        };
        if let Some((key_name, version_id)) = project_id_to_key_version_index.get(&project_id) {
            if version_id == &latest_version {
                log::info!(
                    "Mod {} already exists in the modpack with the same version",
                    key_name
                );
                continue;
            }
            log::info!(
                "Mod {} already exists in the modpack with a different version, will update",
                key_name
            );
            mods_bucket[key_name]
                .as_inline_table_mut()
                .unwrap()
                .insert("version_id", latest_version.into_toml_edit_value());
        } else {
            let extra_info = match site.load_metadata_by_version(latest_version.clone()).await {
                Some(info) => info,
                None => site.load_metadata(project_id.clone()).await,
            }?;
            let key_name = extra_info
                .name
                // Just drop apostrophes
                .replace('\'', "")
                .replace(|c: char| !c.is_ascii_alphanumeric(), "_")
                .to_ascii_lowercase();
            // Replace any run of underscores with a single underscore
            let key_name = key_name
                .chars()
                .fold(
                    (String::new(), false),
                    |(mut acc, last_was_underscore), c| {
                        if c == '_' {
                            if last_was_underscore {
                                (acc, true)
                            } else {
                                acc.push(c);
                                (acc, true)
                            }
                        } else {
                            acc.push(c);
                            (acc, false)
                        }
                    },
                )
                .0;
            // Trim underscores to keep the name clean
            let key_name = key_name.trim_matches('_');
            if mods_bucket.contains_key(key_name) {
                log::warn!("Not overwriting existing mod with key name {}", key_name);
                continue;
            }
            log::info!("Adding mod {} to the modpack", key_name);
            let mut new_entry = toml_edit::InlineTable::new();
            new_entry.insert("project_id", project_id.into_toml_edit_value());
            new_entry.insert("version_id", latest_version.into_toml_edit_value());

            fn emit_env_req(req: &EnvRequirement) -> Option<toml_edit::Value> {
                match req {
                    EnvRequirement::Unknown => None,
                    _ => Some(toml_edit::Value::from(req.to_string())),
                }
            }

            if let Some(env_req) = emit_env_req(&extra_info.side_info.client) {
                new_entry.insert("client", env_req);
            }
            if let Some(env_req) = emit_env_req(&extra_info.side_info.server) {
                new_entry.insert("server", env_req);
            }

            new_entry.fmt();

            mods_bucket.insert(key_name, new_entry.into());
        }
    }
    Ok(())
}

#[derive(Debug, Error)]
enum GenerateModpackError {
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

async fn generate_modpack(args: Generate) -> Result<(), GenerateModpackError> {
    let pack_config = load_pack_config(&args.source)?;

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
