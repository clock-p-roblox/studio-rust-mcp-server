use super::{sanitize_identifier, trim, DEFAULT_DOMAIN_SUFFIX, DEFAULT_HELPER_PORT};
use clap::Parser;
use color_eyre::eyre::{eyre, Result, WrapErr};
use std::collections::HashSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, Notify};
use uuid::Uuid;

#[derive(Parser, Debug, Clone)]
pub(super) struct Args {
    #[arg(long, default_value_t = DEFAULT_HELPER_PORT)]
    pub(super) port: u16,
    #[arg(long)]
    pub(super) user_name: Option<String>,
    #[arg(long)]
    pub(super) bearer_token: Option<String>,
    #[arg(long)]
    pub(super) bearer_token_file: Option<PathBuf>,
    #[arg(long, default_value = DEFAULT_DOMAIN_SUFFIX)]
    pub(super) domain_suffix: String,
    #[arg(long)]
    pub(super) helper_id: Option<String>,
    #[arg(long, required = true)]
    pub(super) hub_base_url: Option<String>,
    #[arg(long)]
    pub(super) studio_path: Option<PathBuf>,
    #[arg(long, default_value_t = 4)]
    pub(super) capacity: usize,
    #[arg(long, default_value_t = false)]
    pub(super) skip_claim_studio_launch: bool,
}

#[derive(Clone)]
pub(super) struct HelperConfig {
    pub(super) port: u16,
    pub(super) helper_id: String,
    pub(super) capacity: usize,
    pub(super) user_name: String,
    pub(super) bearer_token: Arc<Mutex<String>>,
    pub(super) bearer_token_source: Arc<Mutex<String>>,
    pub(super) bearer_token_candidates: Arc<Vec<ResolvedToken>>,
    pub(super) domain_suffix: String,
    pub(super) hub_base_url: Option<String>,
    pub(super) studio_path: Option<PathBuf>,
    pub(super) skip_claim_studio_launch: bool,
    pub(super) client: reqwest::Client,
    pub(super) hub_heartbeat_notify: Arc<Notify>,
}

#[derive(Clone)]
pub(super) struct ResolvedToken {
    pub(super) value: String,
    pub(super) source: String,
}

pub(super) fn normalize_bearer_token(value: &str) -> Option<String> {
    let trimmed = value.trim().trim_start_matches('\u{feff}').trim();
    if trimmed.is_empty() {
        return None;
    }

    let raw = trimmed.strip_prefix("Bearer ").unwrap_or(trimmed).trim();
    if raw.is_empty() {
        return None;
    }

    Some(raw.to_owned())
}

fn home_dir() -> Option<PathBuf> {
    env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

fn windows_appdata_dir() -> Option<PathBuf> {
    env::var_os("APPDATA").map(PathBuf::from)
}

fn user_name_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(appdata) = windows_appdata_dir() {
        candidates.push(appdata.join("dev.clock-p.com").join("feishu-user_name"));
    }
    if let Some(home) = home_dir() {
        candidates.push(home.join(".dev.clock-p.com").join("feishu-user_name"));
    }
    candidates
}

fn token_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(appdata) = windows_appdata_dir() {
        candidates.push(appdata.join("dev.clock-p.com").join("feishu-token"));
    }
    if let Some(home) = home_dir() {
        candidates.push(home.join(".dev.clock-p.com").join("feishu-token"));
    }
    candidates
}

fn read_trimmed_file(path: &Path) -> Result<String> {
    Ok(trim(
        &fs::read_to_string(path)
            .wrap_err_with(|| format!("failed to read {}", path.display()))?
            .replace(['\r', '\n'], ""),
    ))
}

pub(super) fn resolve_user_name(args: &Args) -> Result<String> {
    if let Some(value) = args.user_name.as_ref() {
        let trimmed = trim(value).to_lowercase();
        if !trimmed.is_empty() {
            return Ok(trimmed);
        }
    }

    for candidate in user_name_candidates() {
        if candidate.is_file() {
            let value = read_trimmed_file(&candidate)?.to_lowercase();
            if !value.is_empty() {
                return Ok(value);
            }
        }
    }

    Err(eyre!("cannot resolve feishu-user_name for helper"))
}

pub(super) fn resolve_bearer_token(args: &Args) -> Result<String> {
    Ok(resolve_bearer_token_candidates(args)?
        .into_iter()
        .next()
        .ok_or_else(|| eyre!("cannot resolve feishu-token for helper"))?
        .value)
}

pub(super) fn resolve_bearer_token_candidates(args: &Args) -> Result<Vec<ResolvedToken>> {
    let mut resolved = Vec::new();

    if let Some(value) = args.bearer_token.as_ref() {
        if let Some(normalized) = normalize_bearer_token(value) {
            resolved.push(ResolvedToken {
                value: normalized,
                source: "cli --bearer-token".to_owned(),
            });
        }
    }

    if let Some(path) = args.bearer_token_file.as_ref() {
        if let Some(normalized) = normalize_bearer_token(&read_trimmed_file(path)?) {
            resolved.push(ResolvedToken {
                value: normalized,
                source: path.display().to_string(),
            });
        }
    }

    for candidate in token_candidates() {
        if candidate.is_file() {
            if let Some(normalized) = normalize_bearer_token(&read_trimmed_file(&candidate)?) {
                resolved.push(ResolvedToken {
                    value: normalized,
                    source: candidate.display().to_string(),
                });
            }
        }
    }

    let mut unique = Vec::new();
    let mut seen = HashSet::new();
    for item in resolved {
        if seen.insert(item.value.clone()) {
            unique.push(item);
        }
    }

    if unique.is_empty() {
        return Err(eyre!("cannot resolve feishu-token for helper"));
    }

    Ok(unique)
}

pub(super) fn helper_data_dir() -> Result<PathBuf> {
    if let Some(appdata) = windows_appdata_dir() {
        return Ok(appdata.join("dev.clock-p.com").join("studio-helper"));
    }
    if let Some(home) = home_dir() {
        return Ok(home.join(".dev.clock-p.com").join("studio-helper"));
    }
    Err(eyre!("cannot resolve helper data directory"))
}

pub(super) fn helper_id_from_machine_guid(machine_guid: &str) -> Result<String> {
    let normalized = trim(machine_guid).to_ascii_lowercase();
    if normalized.is_empty() {
        return Err(eyre!("MachineGuid must not be empty"));
    }
    let source = format!("windows-machine-guid:{normalized}");
    let derived = Uuid::new_v5(&Uuid::NAMESPACE_OID, source.as_bytes())
        .simple()
        .to_string();
    sanitize_identifier("helper_id", &format!("h_{}", &derived[..12]))
}

pub(super) fn parse_machine_guid_reg_query_output(output: &str) -> Result<String> {
    for line in output.lines() {
        let trimmed = trim(line);
        if !trimmed.starts_with("MachineGuid") {
            continue;
        }
        let mut parts = trimmed.split_whitespace();
        let name = parts.next();
        let value_type = parts.next();
        let value = parts.collect::<Vec<_>>().join(" ");
        if name == Some("MachineGuid") && value_type == Some("REG_SZ") && !value.is_empty() {
            return Ok(trim(&value).to_owned());
        }
    }
    Err(eyre!("reg query did not return MachineGuid"))
}

#[cfg(target_os = "windows")]
fn resolve_default_helper_id() -> Result<String> {
    let output = std::process::Command::new("reg")
        .args([
            "query",
            r"HKEY_LOCAL_MACHINE\SOFTWARE\Microsoft\Cryptography",
            "/v",
            "MachineGuid",
        ])
        .output()
        .wrap_err("failed to query Windows MachineGuid with reg.exe")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(eyre!(
            "failed to query MachineGuid: status={}, stderr={}",
            output.status,
            trim(&stderr)
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let machine_guid = parse_machine_guid_reg_query_output(&stdout)?;
    helper_id_from_machine_guid(&machine_guid)
}

#[cfg(not(target_os = "windows"))]
fn resolve_default_helper_id() -> Result<String> {
    let data_dir = helper_data_dir()?;
    fs::create_dir_all(&data_dir)?;
    let path = data_dir.join("helper-id");
    if path.exists() {
        let existing = trim(&fs::read_to_string(&path)?);
        return sanitize_identifier("helper_id", &existing);
    }
    let generated = format!("h_{}", &Uuid::new_v4().simple().to_string()[..10]);
    fs::write(&path, format!("{generated}\n"))?;
    Ok(generated)
}

pub(super) fn resolve_helper_id(args: &Args) -> Result<String> {
    if let Some(value) = args.helper_id.as_ref() {
        return sanitize_identifier("helper_id", value);
    }
    resolve_default_helper_id()
}

pub(super) fn build_http_client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()?)
}
