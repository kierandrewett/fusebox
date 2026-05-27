use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};
use tapoctl::discovery_scan_targets_with_auto;

pub(crate) const DEFAULT_ENERGY_PRICE_PENCE_PER_KWH: f64 = 27.03;

#[derive(Debug, Clone)]
pub(crate) struct Settings {
    pub(crate) bind_address: SocketAddr,
    pub(crate) username: String,
    pub(crate) password: String,
    pub(crate) refresh_seconds: u64,
    pub(crate) scan_seconds: u64,
    pub(crate) discovery_timeout_seconds: u64,
    pub(crate) discovery_targets: Vec<String>,
    pub(crate) energy_price_pence_per_kwh: f64,
    pub(crate) state_path: PathBuf,
}

impl Settings {
    pub(crate) fn from_env() -> Result<Self> {
        let bind_address = std::env::var("FUSEBOX_BIND")
            .unwrap_or_else(|_| "127.0.0.1:8787".to_string())
            .parse::<SocketAddr>()
            .context("FUSEBOX_BIND must be a socket address, for example 127.0.0.1:8787")?;
        let username = required_env("TAPO_USERNAME")?;
        let password = required_env("TAPO_PASSWORD")?;
        let refresh_seconds = optional_u64_env("FUSEBOX_REFRESH_SECONDS", 10)?;
        let scan_seconds = optional_u64_env("FUSEBOX_SCAN_SECONDS", 60)?;
        let discovery_timeout_seconds = optional_u64_env("FUSEBOX_DISCOVERY_TIMEOUT_SECONDS", 5)?;
        let discovery_targets = optional_string_list_env("FUSEBOX_DISCOVERY_TARGETS")?;
        let energy_price_pence_per_kwh = optional_f64_env(
            "FUSEBOX_ENERGY_PRICE_PENCE_PER_KWH",
            DEFAULT_ENERGY_PRICE_PENCE_PER_KWH,
        )?;
        let state_path = optional_path_env("FUSEBOX_STATE_PATH")
            .unwrap_or(default_state_path().context("failed to resolve default state path")?);

        if !(1..=60).contains(&discovery_timeout_seconds) {
            return Err(anyhow!(
                "FUSEBOX_DISCOVERY_TIMEOUT_SECONDS must be between 1 and 60"
            ));
        }

        if !discovery_targets.is_empty() {
            discovery_scan_targets_with_auto(&discovery_targets, &[], Vec::new())
                .context("FUSEBOX_DISCOVERY_TARGETS must contain IPv4 addresses or CIDRs")?;
        }

        if !(10..=3600).contains(&scan_seconds) {
            return Err(anyhow!("FUSEBOX_SCAN_SECONDS must be between 10 and 3600"));
        }

        if !energy_price_pence_per_kwh.is_finite()
            || !(0.0..=1000.0).contains(&energy_price_pence_per_kwh)
        {
            return Err(anyhow!(
                "FUSEBOX_ENERGY_PRICE_PENCE_PER_KWH must be between 0 and 1000",
            ));
        }

        Ok(Self {
            bind_address,
            username,
            password,
            refresh_seconds,
            scan_seconds,
            discovery_timeout_seconds,
            discovery_targets,
            energy_price_pence_per_kwh,
            state_path,
        })
    }
}

fn required_env(name: &str) -> Result<String> {
    std::env::var(name).with_context(|| format!("{name} must be set"))
}

pub(crate) fn optional_u64_env(name: &str, default: u64) -> Result<u64> {
    match std::env::var(name) {
        Ok(value) => value
            .parse::<u64>()
            .with_context(|| format!("{name} must be an integer")),
        Err(std::env::VarError::NotPresent) => Ok(default),
        Err(error) => Err(error).with_context(|| format!("failed to read {name}")),
    }
}

fn optional_f64_env(name: &str, default: f64) -> Result<f64> {
    match std::env::var(name) {
        Ok(value) => value
            .parse::<f64>()
            .with_context(|| format!("{name} must be a number")),
        Err(std::env::VarError::NotPresent) => Ok(default),
        Err(error) => Err(error).with_context(|| format!("failed to read {name}")),
    }
}

fn optional_string_list_env(name: &str) -> Result<Vec<String>> {
    match std::env::var(name) {
        Ok(value) => Ok(parse_string_list(&value)),
        Err(std::env::VarError::NotPresent) => Ok(Vec::new()),
        Err(error) => Err(error).with_context(|| format!("failed to read {name}")),
    }
}

pub(crate) fn parse_string_list(value: &str) -> Vec<String> {
    value
        .split(|character: char| character == ',' || character.is_whitespace())
        .filter(|part| !part.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn optional_path_env(name: &str) -> Option<PathBuf> {
    match std::env::var_os(name) {
        Some(value) if value.is_empty() => None,
        Some(value) => Some(PathBuf::from(value)),
        None => None,
    }
}

fn default_state_path() -> Result<PathBuf> {
    if let Some(config_home) = std::env::var_os("XDG_CONFIG_HOME").filter(|value| !value.is_empty())
    {
        return Ok(PathBuf::from(config_home)
            .join("fusebox")
            .join("state.json"));
    }

    if let Some(home) = std::env::var_os("HOME").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(home)
            .join(".config")
            .join("fusebox")
            .join("state.json"));
    }

    Err(anyhow!(
        "HOME or XDG_CONFIG_HOME must be set, or set FUSEBOX_STATE_PATH explicitly",
    ))
}
