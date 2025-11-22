use std::collections::HashMap;
use std::fmt::Debug;
use derive_more::Display;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::mod_site::{DependencyId, ModId, ModIdValue};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigModContainer {
    #[serde(default)]
    pub curseforge: HashMap<String, ConfigMod<i32>>,
    #[serde(default)]
    pub modrinth: HashMap<String, ConfigMod<String>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigMod<K: ModIdValue> {
    #[serde(flatten)]
    pub source: ModId<K>,
    #[serde(default)]
    pub client: EnvRequirement,
    #[serde(default)]
    pub server: EnvRequirement,
    /// Dependencies to ignore when validating.
    #[serde(default)]
    pub ignored_deps: Vec<DependencyId<K>>,
}

#[derive(Debug, Copy, Clone, Deserialize, Eq, PartialEq, Display)]
#[display("{}", format!("{:?}", self).to_lowercase())]
#[serde(rename_all = "lowercase")]
pub enum EnvRequirement {
    /// Inherit from the state defined by the mod site or [`Required`].
    Unknown,
    Required,
    Optional,
    Unsupported,
}

impl Default for EnvRequirement {
    fn default() -> Self {
        Self::Unknown
    }
}

// Warning -- this type is explicitly compatible with the Modrinth pack format, and should not be
// changed incompatibly without adding a different type for the format.
#[derive(Debug, Copy, Clone, Serialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum KnownEnvRequirement {
    Required,
    Optional,
    Unsupported,
}

impl KnownEnvRequirement {
    pub fn is_needed(&self, need_optional: bool) -> bool {
        match self {
            KnownEnvRequirement::Required => true,
            KnownEnvRequirement::Optional => need_optional,
            KnownEnvRequirement::Unsupported => false,
        }
    }
}

#[derive(Debug, Error)]
pub enum ComputeEnvWarning {
    #[error(
        "The config allowed the mod on this side, but it was marked as unsupported on the site."
    )]
    ConfigAllowedButSiteUnsupported,
    #[error(
        "The site allowed the mod on this side, but it was marked as unsupported in the config."
    )]
    SiteAllowedButConfigUnsupported,
}

/// Given the env from the config and the site, compute the actual env.
pub fn compute_env(
    cfg_env: EnvRequirement,
    site_env: EnvRequirement,
) -> (KnownEnvRequirement, Option<ComputeEnvWarning>) {
    match cfg_env {
        EnvRequirement::Unknown => match site_env {
            EnvRequirement::Required => (KnownEnvRequirement::Required, None),
            EnvRequirement::Optional => (KnownEnvRequirement::Optional, None),
            EnvRequirement::Unsupported => (KnownEnvRequirement::Unsupported, None),
            EnvRequirement::Unknown => (KnownEnvRequirement::Required, None),
        },
        EnvRequirement::Required | EnvRequirement::Optional => {
            let warning = (site_env == EnvRequirement::Unsupported)
                .then_some(ComputeEnvWarning::ConfigAllowedButSiteUnsupported);
            if cfg_env == EnvRequirement::Required {
                (KnownEnvRequirement::Required, warning)
            } else {
                (KnownEnvRequirement::Optional, warning)
            }
        }
        EnvRequirement::Unsupported => {
            let warning = (site_env == EnvRequirement::Required
                || site_env == EnvRequirement::Optional)
                .then_some(ComputeEnvWarning::SiteAllowedButConfigUnsupported);
            (KnownEnvRequirement::Unsupported, warning)
        }
    }
}
