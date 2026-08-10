//! Non-secret TOML profile loading and CLI/profile resolution.
//!
//! This module requires a direct `toml` dependency (the implementation is compatible with
//! the current `toml` 0.9 API). Shell completion generation is intentionally kept outside this
//! module and will additionally require `clap_complete` when wired by the binary.
//!
//! Precedence is: command-line value, Clap-provided environment value, selected profile, then
//! the built-in default. Clap resolves its CLI/environment layers before these functions run.
//! The schema has no password, TOTP-seed, OTP-code, or bearer-token value fields. It can retain
//! only paths to secret files or the *name* of a token-bearing environment variable.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::cli::{
    AuthenticationArgs, CompareArg, ConnectionArgs, CredentialProfileArgs, DoctorArgs, LogFormat,
    LogLevel, NetworkArgs, OutputArgs, OutputFormat, ProgressMode, REMOTE_LOG_TOKEN_ENV,
    RemoteLogMode, SafetyArgs, SyncArgs, SyncBehaviorArgs,
};

const MAX_CONFIG_BYTES: usize = 1024 * 1024;

pub const DEFAULT_JOBS: u8 = 2;
pub const DEFAULT_MAX_DELETE: usize = 100;
pub const DEFAULT_RETRIES: u8 = 2;
pub const DEFAULT_TIMEOUT_SECONDS: u64 = 7200;
pub const DEFAULT_CONNECT_TIMEOUT_SECONDS: u64 = 15;
pub const DEFAULT_MAX_TOTAL_DELETE: usize = 100;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read configuration {path:?}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("configuration {path:?} is not valid UTF-8")]
    Encoding { path: PathBuf },

    #[error("configuration {path:?} exceeds the 1 MiB safety limit")]
    TooLarge { path: PathBuf },

    #[error(
        "failed to parse non-secret configuration {path:?}; TOML details are withheld because the file may contain mistakenly pasted secrets"
    )]
    Parse { path: PathBuf },

    #[error("configuration profile {0:?} does not exist")]
    MissingProfile(String),

    #[error(
        "{0} is required; provide it on the command line, through its SDSYNC_* environment variable, or in the selected profile"
    )]
    MissingValue(&'static str),

    #[error("invalid effective configuration: {0}")]
    Invalid(String),
}

/// Top-level non-secret configuration document.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct ConfigFile {
    /// Profile selected when `--profile`/`SDSYNC_PROFILE` is absent.
    pub default_profile: Option<String>,

    #[serde(default)]
    pub profiles: BTreeMap<String, Profile>,
}

/// Flat profile schema intended to remain easy to audit and hand-edit.
///
/// Every field is optional because a profile may hold only connection defaults and leave the
/// source/destination to each invocation. Secret-bearing fields intentionally do not exist.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Profile {
    pub source: Option<PathBuf>,
    pub remote: Option<String>,
    pub url: Option<String>,
    pub username: Option<String>,

    pub password_file: Option<PathBuf>,
    pub totp_secret_file: Option<PathBuf>,
    pub no_vault: Option<bool>,

    pub compare: Option<CompareArg>,
    pub jobs: Option<u8>,
    #[serde(default)]
    pub excludes: Vec<String>,

    pub delete: Option<bool>,
    pub allow_empty_source: Option<bool>,
    pub max_delete: Option<usize>,

    pub retries: Option<u8>,
    pub timeout: Option<u64>,
    pub connect_timeout: Option<u64>,
    pub max_rate: Option<u64>,
    pub ca_certificate: Option<PathBuf>,
    pub allow_http: Option<bool>,
    pub danger_accept_invalid_certs: Option<bool>,

    pub verbose: Option<u8>,
    pub quiet: Option<bool>,
    pub log_level: Option<LogLevel>,
    pub log_format: Option<LogFormat>,
    pub log_file: Option<PathBuf>,
    pub remote_log_url: Option<String>,
    pub remote_log_token_file: Option<PathBuf>,
    pub remote_log_token_env: Option<String>,
    pub remote_log_mode: Option<RemoteLogMode>,
    pub progress: Option<ProgressMode>,
    pub output: Option<OutputFormat>,
}

impl Profile {
    fn rebase_paths(&mut self, base: &Path) {
        for path in [
            &mut self.source,
            &mut self.password_file,
            &mut self.totp_secret_file,
            &mut self.ca_certificate,
            &mut self.log_file,
            &mut self.remote_log_token_file,
        ] {
            if let Some(value) = path
                && value.is_relative()
            {
                *value = base.join(&*value);
            }
        }
    }
}

/// Parsed configuration with relative paths anchored to the file's directory.
#[derive(Clone, Debug)]
pub struct LoadedConfig {
    pub path: PathBuf,
    pub values: ConfigFile,
}

impl LoadedConfig {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref().to_path_buf();
        let bytes = fs::read(&path).map_err(|source| ConfigError::Read {
            path: path.clone(),
            source,
        })?;
        if bytes.len() > MAX_CONFIG_BYTES {
            return Err(ConfigError::TooLarge { path });
        }
        let text = std::str::from_utf8(&bytes)
            .map_err(|_| ConfigError::Encoding { path: path.clone() })?;
        Self::from_toml(path, text)
    }

    /// Parse supplied TOML. Public primarily to support embedding and deterministic tests.
    pub fn from_toml(path: impl Into<PathBuf>, text: &str) -> Result<Self, ConfigError> {
        let path = path.into();
        if text.len() > MAX_CONFIG_BYTES {
            return Err(ConfigError::TooLarge { path });
        }
        let mut values: ConfigFile =
            toml::from_str(text).map_err(|_| ConfigError::Parse { path: path.clone() })?;
        let base = path.parent().unwrap_or_else(|| Path::new("."));
        for profile in values.profiles.values_mut() {
            profile.rebase_paths(base);
        }
        Ok(Self { path, values })
    }

    pub fn select_profile<'a>(
        &'a self,
        requested: Option<&str>,
    ) -> Result<SelectedProfile<'a>, ConfigError> {
        let selected = requested
            .map(str::to_owned)
            .or_else(|| self.values.default_profile.clone())
            .unwrap_or_else(|| "default".to_owned());
        let values = self.values.profiles.get(&selected);
        let selection_was_explicit = requested.is_some() || self.values.default_profile.is_some();
        if selection_was_explicit && values.is_none() {
            return Err(ConfigError::MissingProfile(selected));
        }
        Ok(SelectedProfile {
            name: selected,
            values,
        })
    }
}

#[derive(Clone, Debug)]
pub struct SelectedProfile<'a> {
    pub name: String,
    pub values: Option<&'a Profile>,
}

impl SelectedProfile<'_> {
    /// Build the payload for `config show`. This type cannot carry secret values.
    pub fn non_secret_view(&self) -> NonSecretProfileView {
        NonSecretProfileView::new(&self.name, self.values)
    }
}

/// Return the conventional per-user configuration location without creating anything.
pub fn default_config_path() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        return std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .map(|base| base.join("synology-drive-sync").join("config.toml"));
    }
    #[cfg(target_os = "macos")]
    {
        return std::env::var_os("HOME").map(PathBuf::from).map(|base| {
            base.join("Library")
                .join("Application Support")
                .join("synology-drive-sync")
                .join("config.toml")
        });
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if let Some(base) = std::env::var_os("XDG_CONFIG_HOME") {
            return Some(
                PathBuf::from(base)
                    .join("synology-drive-sync")
                    .join("config.toml"),
            );
        }
        return std::env::var_os("HOME")
            .map(PathBuf::from)
            .map(|base| base.join(".config/synology-drive-sync/config.toml"));
    }
    #[allow(unreachable_code)]
    None
}

/// Serializable, intentionally non-secret `config show` representation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub struct NonSecretProfileView {
    pub profile: String,
    pub source: Option<PathBuf>,
    pub remote: Option<String>,
    pub url: Option<String>,
    pub username: Option<String>,
    pub password_file: Option<PathBuf>,
    pub totp_secret_file: Option<PathBuf>,
    pub no_vault: bool,
    pub compare: CompareArg,
    pub jobs: u8,
    pub excludes: Vec<String>,
    pub delete: bool,
    pub allow_empty_source: bool,
    pub max_delete: usize,
    pub retries: u8,
    pub timeout: u64,
    pub connect_timeout: u64,
    /// Upload bytes per second shared by every job; absent means unlimited.
    pub max_rate: Option<u64>,
    pub ca_certificate: Option<PathBuf>,
    pub allow_http: bool,
    pub danger_accept_invalid_certs: bool,
    pub verbose: u8,
    pub quiet: bool,
    pub log_level: LogLevel,
    pub log_format: LogFormat,
    pub log_file: Option<PathBuf>,
    pub remote_log_url: Option<String>,
    pub remote_log_token_file: Option<PathBuf>,
    pub remote_log_token_env: Option<String>,
    pub remote_log_mode: RemoteLogMode,
    pub progress: ProgressMode,
    pub output: OutputFormat,
}

impl NonSecretProfileView {
    fn new(name: &str, values: Option<&Profile>) -> Self {
        let fallback = Profile::default();
        let profile = values.unwrap_or(&fallback);
        Self {
            profile: name.to_owned(),
            source: profile.source.clone(),
            remote: profile.remote.clone(),
            url: profile.url.clone(),
            username: profile.username.clone(),
            password_file: profile.password_file.clone(),
            totp_secret_file: profile.totp_secret_file.clone(),
            no_vault: profile.no_vault.unwrap_or(false),
            compare: profile.compare.unwrap_or(CompareArg::Content),
            jobs: profile.jobs.unwrap_or(DEFAULT_JOBS),
            excludes: profile.excludes.clone(),
            delete: profile.delete.unwrap_or(false),
            allow_empty_source: profile.allow_empty_source.unwrap_or(false),
            max_delete: profile.max_delete.unwrap_or(DEFAULT_MAX_DELETE),
            retries: profile.retries.unwrap_or(DEFAULT_RETRIES),
            timeout: profile.timeout.unwrap_or(DEFAULT_TIMEOUT_SECONDS),
            connect_timeout: profile
                .connect_timeout
                .unwrap_or(DEFAULT_CONNECT_TIMEOUT_SECONDS),
            max_rate: profile.max_rate,
            ca_certificate: profile.ca_certificate.clone(),
            allow_http: profile.allow_http.unwrap_or(false),
            danger_accept_invalid_certs: profile.danger_accept_invalid_certs.unwrap_or(false),
            verbose: profile.verbose.unwrap_or(0),
            quiet: profile.quiet.unwrap_or(false),
            log_level: profile.log_level.unwrap_or(LogLevel::Info),
            log_format: profile.log_format.unwrap_or(LogFormat::Human),
            log_file: profile.log_file.clone(),
            remote_log_url: profile.remote_log_url.clone(),
            remote_log_token_file: profile.remote_log_token_file.clone(),
            remote_log_token_env: profile.remote_log_token_env.clone().or_else(|| {
                (profile.remote_log_token_file.is_none() && profile.remote_log_url.is_some())
                    .then(|| REMOTE_LOG_TOKEN_ENV.to_owned())
            }),
            remote_log_mode: profile.remote_log_mode.unwrap_or(RemoteLogMode::BestEffort),
            progress: profile.progress.unwrap_or(ProgressMode::Auto),
            output: profile.output.unwrap_or(OutputFormat::Human),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedConnection {
    pub url: String,
    pub username: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedAuthentication {
    pub password_stdin: bool,
    pub password_file: Option<PathBuf>,
    pub totp_secret_file: Option<PathBuf>,
    pub no_vault: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedSyncBehavior {
    pub compare: CompareArg,
    pub jobs: u8,
    pub excludes: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedSafety {
    pub delete: bool,
    pub allow_empty_source: bool,
    pub max_delete: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedNetwork {
    pub retries: u8,
    pub timeout: u64,
    pub connect_timeout: u64,
    /// Upload bytes per second shared by every job; `None` leaves uploads unlimited.
    pub max_rate: Option<u64>,
    pub ca_certificate: Option<PathBuf>,
    pub allow_http: bool,
    pub danger_accept_invalid_certs: bool,
}

/// A remote logging token locator. It never contains token bytes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RemoteTokenSource {
    File(PathBuf),
    Environment(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedOutput {
    pub verbosity: u8,
    pub quiet: bool,
    pub log_level: LogLevel,
    pub log_format: LogFormat,
    pub log_file: Option<PathBuf>,
    pub remote_log_url: Option<String>,
    pub remote_log_token: Option<RemoteTokenSource>,
    pub remote_log_mode: RemoteLogMode,
    pub progress: ProgressMode,
    pub output: OutputFormat,
}

impl ResolvedOutput {
    /// Resolve `auto` without requiring the renderer to repeat policy logic.
    pub fn terminal_progress_enabled(&self, stderr_is_terminal: bool) -> bool {
        if self.quiet || self.output != OutputFormat::Human {
            return false;
        }
        match self.progress {
            ProgressMode::Always => true,
            ProgressMode::Never => false,
            ProgressMode::Auto => stderr_is_terminal && self.log_format == LogFormat::Human,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedSync {
    pub source: PathBuf,
    pub remote: String,
    pub connection: ResolvedConnection,
    pub authentication: ResolvedAuthentication,
    pub behavior: ResolvedSyncBehavior,
    pub safety: ResolvedSafety,
    pub network: ResolvedNetwork,
    pub output: ResolvedOutput,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedDoctor {
    pub remote: Option<String>,
    pub routing_only: bool,
    pub compare: CompareArg,
    pub delete: bool,
    pub write_test: bool,
    pub url: String,
    pub username: Option<String>,
    pub authentication: ResolvedAuthentication,
    pub network: ResolvedNetwork,
    pub output: ResolvedOutput,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedSourceDoctor {
    pub source: PathBuf,
    pub excludes: Vec<String>,
    pub hash_content: bool,
    pub output: ResolvedOutput,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedCredentialProfile {
    pub url: String,
    pub username: String,
    pub allow_http: bool,
}

/// Overlay already-parsed CLI/environment values on a selected profile.
pub fn resolve_sync(
    profile: Option<&Profile>,
    arguments: &SyncArgs,
    output: &OutputArgs,
) -> Result<ResolvedSync, ConfigError> {
    let fallback = Profile::default();
    let profile = profile.unwrap_or(&fallback);

    let source = arguments
        .source
        .clone()
        .or_else(|| profile.source.clone())
        .ok_or(ConfigError::MissingValue("SOURCE"))?;
    let remote = arguments
        .remote
        .clone()
        .or_else(|| profile.remote.clone())
        .ok_or(ConfigError::MissingValue("REMOTE"))?;

    let network = resolve_network(profile, &arguments.network)?;
    let connection = resolve_connection(profile, &arguments.connection, network.allow_http)?;
    let authentication = resolve_authentication(profile, &arguments.authentication);
    let behavior = resolve_behavior(profile, &arguments.behavior)?;
    let safety = resolve_safety(profile, &arguments.safety)?;
    let output = resolve_output(profile, output)?;

    Ok(ResolvedSync {
        source,
        remote,
        connection,
        authentication,
        behavior,
        safety,
        network,
        output,
    })
}

pub fn resolve_doctor(
    profile: Option<&Profile>,
    arguments: &DoctorArgs,
    output: &OutputArgs,
) -> Result<ResolvedDoctor, ConfigError> {
    let fallback = Profile::default();
    let profile = profile.unwrap_or(&fallback);
    let url = arguments
        .connection
        .url
        .clone()
        .or_else(|| profile.url.clone())
        .ok_or(ConfigError::MissingValue("--url"))?;
    let network = resolve_network(profile, &arguments.network)?;
    validate_dsm_url(&url, network.allow_http)?;
    let username = arguments
        .connection
        .username
        .clone()
        .or_else(|| profile.username.clone());
    if !arguments.routing_only && username.is_none() {
        return Err(ConfigError::MissingValue("--username"));
    }
    let (action_remote, write_test) = match arguments.action.as_ref() {
        Some(crate::cli::DoctorAction::Target(target)) => {
            (target.remote.clone(), target.write_test)
        }
        Some(crate::cli::DoctorAction::Source(_)) => {
            return Err(ConfigError::Invalid(
                "doctor source must be resolved as a local-only diagnostic".to_owned(),
            ));
        }
        None => (None, false),
    };
    if arguments.routing_only && arguments.action.is_some() {
        return Err(ConfigError::Invalid(
            "--routing-only cannot be combined with a doctor source or target subcommand"
                .to_owned(),
        ));
    }
    let remote = action_remote
        .or_else(|| arguments.remote.clone())
        .or_else(|| profile.remote.clone());
    if write_test && remote.is_none() {
        return Err(ConfigError::Invalid(
            "doctor target --write-test requires REMOTE or a profile remote".to_owned(),
        ));
    }
    Ok(ResolvedDoctor {
        remote,
        routing_only: arguments.routing_only,
        compare: profile.compare.unwrap_or(CompareArg::Content),
        delete: profile.delete.unwrap_or(false),
        write_test,
        url,
        username,
        authentication: resolve_authentication(profile, &arguments.authentication),
        network,
        output: resolve_output(profile, output)?,
    })
}

pub fn resolve_source_doctor(
    profile: Option<&Profile>,
    arguments: &crate::cli::DoctorSourceArgs,
    output: &OutputArgs,
) -> Result<ResolvedSourceDoctor, ConfigError> {
    let fallback = Profile::default();
    let profile = profile.unwrap_or(&fallback);
    let source = arguments
        .source
        .clone()
        .or_else(|| profile.source.clone())
        .ok_or(ConfigError::MissingValue("SOURCE"))?;
    let mut excludes = profile.excludes.clone();
    excludes.extend(arguments.excludes.iter().cloned());
    Ok(ResolvedSourceDoctor {
        source,
        excludes,
        hash_content: arguments.hash,
        output: resolve_output(profile, output)?,
    })
}

pub fn resolve_credential_profile(
    profile: Option<&Profile>,
    arguments: &CredentialProfileArgs,
) -> Result<ResolvedCredentialProfile, ConfigError> {
    let fallback = Profile::default();
    let profile = profile.unwrap_or(&fallback);
    let url = arguments
        .url
        .clone()
        .or_else(|| profile.url.clone())
        .ok_or(ConfigError::MissingValue("--url"))?;
    let allow_http = arguments.allow_http || profile.allow_http.unwrap_or(false);
    validate_dsm_url(&url, allow_http)?;
    Ok(ResolvedCredentialProfile {
        url,
        username: arguments
            .username
            .clone()
            .or_else(|| profile.username.clone())
            .ok_or(ConfigError::MissingValue("--username"))?,
        allow_http,
    })
}

/// Validate profile-local constraints without requiring command-specific source/destination
/// values. This is used by `config validate`.
pub fn validate_profile(profile: &Profile) -> Result<(), ConfigError> {
    if let Some(url) = &profile.url {
        validate_dsm_url(url, profile.allow_http.unwrap_or(false))?;
    }
    if let Some(url) = &profile.remote_log_url {
        validate_remote_log_url(url)?;
    }
    if profile.jobs.is_some_and(|jobs| !(1..=16).contains(&jobs)) {
        return Err(ConfigError::Invalid(
            "jobs must be between 1 and 16".to_owned(),
        ));
    }
    if profile.retries.is_some_and(|retries| retries > 5) {
        return Err(ConfigError::Invalid(
            "retries must be between 0 and 5".to_owned(),
        ));
    }
    if profile.timeout == Some(0) || profile.connect_timeout == Some(0) {
        return Err(ConfigError::Invalid(
            "timeout and connect-timeout must be at least 1 second".to_owned(),
        ));
    }
    if profile.max_rate == Some(0) {
        return Err(ConfigError::Invalid(
            "max-rate must be at least 1 byte per second".to_owned(),
        ));
    }
    if profile.allow_empty_source == Some(true) && profile.delete != Some(true) {
        return Err(ConfigError::Invalid(
            "allow-empty-source requires delete=true in the same profile".to_owned(),
        ));
    }
    if profile.remote_log_token_file.is_some() && profile.remote_log_token_env.is_some() {
        return Err(ConfigError::Invalid(
            "remote-log-token-file conflicts with remote-log-token-env".to_owned(),
        ));
    }
    if profile.remote_log_mode == Some(RemoteLogMode::Required) && profile.remote_log_url.is_none()
    {
        return Err(ConfigError::Invalid(
            "remote-log-mode=required requires remote-log-url".to_owned(),
        ));
    }
    if (profile.remote_log_token_file.is_some() || profile.remote_log_token_env.is_some())
        && profile.remote_log_url.is_none()
    {
        return Err(ConfigError::Invalid(
            "a remote-log token source requires remote-log-url".to_owned(),
        ));
    }
    if let Some(name) = &profile.remote_log_token_env {
        validate_environment_name(name)?;
    }
    Ok(())
}

fn validate_dsm_url(value: &str, allow_http: bool) -> Result<(), ConfigError> {
    let url = reqwest::Url::parse(value).map_err(|_| {
        ConfigError::Invalid("url must be an absolute HTTPS reverse-proxy URL".to_owned())
    })?;
    let allowed_scheme = url.scheme() == "https" || (allow_http && url.scheme() == "http");
    if !allowed_scheme
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(ConfigError::Invalid(if allow_http {
            "url must be an absolute HTTP(S) reverse-proxy URL without credentials, query, or fragment"
                .to_owned()
        } else {
            "url must be an absolute HTTPS reverse-proxy URL without credentials, query, or fragment; set allow-http=true only for controlled LAN testing"
                .to_owned()
        }));
    }
    Ok(())
}

fn validate_remote_log_url(value: &str) -> Result<(), ConfigError> {
    let url = reqwest::Url::parse(value).map_err(|_| {
        ConfigError::Invalid("remote-log-url must be an absolute HTTPS URL".to_owned())
    })?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(ConfigError::Invalid(
            "remote-log-url must be an absolute HTTPS URL without credentials, query, or fragment"
                .to_owned(),
        ));
    }
    Ok(())
}

fn resolve_connection(
    profile: &Profile,
    arguments: &ConnectionArgs,
    allow_http: bool,
) -> Result<ResolvedConnection, ConfigError> {
    let url = arguments
        .url
        .clone()
        .or_else(|| profile.url.clone())
        .ok_or(ConfigError::MissingValue("--url"))?;
    validate_dsm_url(&url, allow_http)?;
    Ok(ResolvedConnection {
        url,
        username: arguments
            .username
            .clone()
            .or_else(|| profile.username.clone())
            .ok_or(ConfigError::MissingValue("--username"))?,
    })
}

fn resolve_authentication(
    profile: &Profile,
    arguments: &AuthenticationArgs,
) -> ResolvedAuthentication {
    let mut password_file = arguments
        .password_file
        .clone()
        .or_else(|| profile.password_file.clone());
    if arguments.password_stdin {
        password_file = None;
    }
    ResolvedAuthentication {
        password_stdin: arguments.password_stdin,
        password_file,
        totp_secret_file: arguments
            .totp_secret_file
            .clone()
            .or_else(|| profile.totp_secret_file.clone()),
        no_vault: if arguments.vault {
            false
        } else {
            arguments.no_vault || profile.no_vault.unwrap_or(false)
        },
    }
}

fn resolve_behavior(
    profile: &Profile,
    arguments: &SyncBehaviorArgs,
) -> Result<ResolvedSyncBehavior, ConfigError> {
    let jobs = arguments.jobs.or(profile.jobs).unwrap_or(DEFAULT_JOBS);
    if !(1..=16).contains(&jobs) {
        return Err(ConfigError::Invalid(
            "jobs must be between 1 and 16".to_owned(),
        ));
    }
    let mut excludes = profile.excludes.clone();
    excludes.extend(arguments.excludes.iter().cloned());
    Ok(ResolvedSyncBehavior {
        compare: arguments
            .compare
            .or(profile.compare)
            .unwrap_or(CompareArg::Content),
        jobs,
        excludes,
    })
}

fn resolve_safety(
    profile: &Profile,
    arguments: &SafetyArgs,
) -> Result<ResolvedSafety, ConfigError> {
    let delete = if arguments.no_delete {
        false
    } else {
        arguments.delete || profile.delete.unwrap_or(false)
    };
    let allow_empty_source =
        arguments.allow_empty_source || profile.allow_empty_source.unwrap_or(false);
    if allow_empty_source && !delete {
        return Err(ConfigError::Invalid(
            "allow-empty-source requires delete".to_owned(),
        ));
    }
    Ok(ResolvedSafety {
        delete,
        allow_empty_source,
        max_delete: arguments
            .max_delete
            .or(profile.max_delete)
            .unwrap_or(DEFAULT_MAX_DELETE),
    })
}

fn resolve_network(
    profile: &Profile,
    arguments: &NetworkArgs,
) -> Result<ResolvedNetwork, ConfigError> {
    let retries = arguments
        .retries
        .or(profile.retries)
        .unwrap_or(DEFAULT_RETRIES);
    let timeout = arguments
        .timeout
        .or(profile.timeout)
        .unwrap_or(DEFAULT_TIMEOUT_SECONDS);
    let connect_timeout = arguments
        .connect_timeout
        .or(profile.connect_timeout)
        .unwrap_or(DEFAULT_CONNECT_TIMEOUT_SECONDS);
    // Absent stays absent: there is no default rate, and absent is what means unlimited.
    let max_rate = arguments.max_rate.or(profile.max_rate);
    if retries > 5 {
        return Err(ConfigError::Invalid(
            "retries must be between 0 and 5".to_owned(),
        ));
    }
    if timeout == 0 || connect_timeout == 0 {
        return Err(ConfigError::Invalid(
            "timeout and connect-timeout must be at least 1 second".to_owned(),
        ));
    }
    if max_rate == Some(0) {
        return Err(ConfigError::Invalid(
            "max-rate must be at least 1 byte per second".to_owned(),
        ));
    }
    Ok(ResolvedNetwork {
        retries,
        timeout,
        connect_timeout,
        max_rate,
        ca_certificate: arguments
            .ca_certificate
            .clone()
            .or_else(|| profile.ca_certificate.clone()),
        allow_http: arguments.allow_http || profile.allow_http.unwrap_or(false),
        danger_accept_invalid_certs: arguments.danger_accept_invalid_certs
            || profile.danger_accept_invalid_certs.unwrap_or(false),
    })
}

pub fn resolve_output(
    profile: &Profile,
    arguments: &OutputArgs,
) -> Result<ResolvedOutput, ConfigError> {
    let quiet = if arguments.no_quiet {
        false
    } else {
        arguments.quiet || profile.quiet.unwrap_or(false)
    };
    let verbosity = if arguments.verbose > 0 {
        arguments.verbose
    } else {
        profile.verbose.unwrap_or(0)
    };
    // `quiet` is a stderr/progress sink policy. It must not silently disable file or
    // remote logging, which may be the only durable diagnostics for unattended runs.
    let log_level = if let Some(level) = arguments.log_level {
        level
    } else if arguments.verbose >= 2 {
        LogLevel::Trace
    } else if arguments.verbose == 1 {
        LogLevel::Debug
    } else if let Some(level) = profile.log_level {
        level
    } else if verbosity >= 2 {
        LogLevel::Trace
    } else if verbosity == 1 {
        LogLevel::Debug
    } else {
        LogLevel::Info
    };
    let remote_log_url = arguments
        .remote_log_url
        .clone()
        .or_else(|| profile.remote_log_url.clone());
    if let Some(url) = &remote_log_url {
        validate_remote_log_url(url)?;
    }
    let remote_log_mode = arguments
        .remote_log_mode
        .or(profile.remote_log_mode)
        .unwrap_or(RemoteLogMode::BestEffort);
    if remote_log_mode == RemoteLogMode::Required && remote_log_url.is_none() {
        return Err(ConfigError::Invalid(
            "remote-log-mode=required requires remote-log-url".to_owned(),
        ));
    }

    let (token_file, token_env) = if let Some(path) = &arguments.remote_log_token_file {
        (Some(path.clone()), None)
    } else if let Some(name) = &arguments.remote_log_token_env {
        (None, Some(name.clone()))
    } else {
        (
            profile.remote_log_token_file.clone(),
            profile.remote_log_token_env.clone(),
        )
    };
    if token_file.is_some() && token_env.is_some() {
        return Err(ConfigError::Invalid(
            "remote-log-token-file conflicts with remote-log-token-env".to_owned(),
        ));
    }
    let remote_log_token = if let Some(path) = token_file {
        Some(RemoteTokenSource::File(path))
    } else if let Some(name) = token_env {
        validate_environment_name(&name)?;
        Some(RemoteTokenSource::Environment(name))
    } else if remote_log_url.is_some() {
        Some(RemoteTokenSource::Environment(
            REMOTE_LOG_TOKEN_ENV.to_owned(),
        ))
    } else {
        None
    };
    if remote_log_url.is_none() && remote_log_token.is_some() {
        return Err(ConfigError::Invalid(
            "a remote-log token source requires remote-log-url".to_owned(),
        ));
    }

    Ok(ResolvedOutput {
        verbosity,
        quiet,
        log_level,
        log_format: arguments
            .log_format
            .or(profile.log_format)
            .unwrap_or(LogFormat::Human),
        log_file: arguments
            .log_file
            .clone()
            .or_else(|| profile.log_file.clone()),
        remote_log_url,
        remote_log_token,
        remote_log_mode,
        progress: arguments
            .progress
            .or(profile.progress)
            .unwrap_or(ProgressMode::Auto),
        output: arguments
            .output
            .or(profile.output)
            .unwrap_or(OutputFormat::Human),
    })
}

fn validate_environment_name(name: &str) -> Result<(), ConfigError> {
    if name.is_empty()
        || name.contains('=')
        || name
            .bytes()
            .any(|byte| byte == 0 || byte.is_ascii_whitespace())
    {
        return Err(ConfigError::Invalid(
            "remote-log-token-env must be a non-empty environment variable name".to_owned(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;
    use crate::cli::{Cli, DoctorAction, Invocation};

    const CONFIG: &str = r#"
default-profile = "production"

[profiles.production]
source = "../source"
remote = "/team/export"
url = "https://files.example.test"
username = "mirror-bot"
password-file = "secrets/password"
totp-secret-file = "secrets/totp"
compare = "size-only"
jobs = 3
excludes = ["*.tmp"]
delete = true
max-delete = 20
retries = 1
timeout = 900
connect-timeout = 10
max-rate = 65536
ca-certificate = "pki/root.pem"
log-level = "warn"
log-format = "json"
log-file = "logs/sync.log"
remote-log-url = "https://logs.example.test/ingest"
remote-log-token-file = "secrets/log-token"
remote-log-mode = "best-effort"
progress = "never"
output = "json"
"#;

    #[test]
    fn loads_non_secret_profile_and_rebases_paths() {
        let loaded = LoadedConfig::from_toml("settings/profiles.toml", CONFIG).unwrap();
        let selected = loaded.select_profile(None).unwrap();
        assert_eq!(selected.name, "production");
        let profile = selected.values.unwrap();
        assert_eq!(
            profile.password_file.as_deref(),
            Some(Path::new("settings/secrets/password"))
        );
        assert_eq!(
            profile.ca_certificate.as_deref(),
            Some(Path::new("settings/pki/root.pem"))
        );
        assert_eq!(
            profile.remote_log_token_file.as_deref(),
            Some(Path::new("settings/secrets/log-token"))
        );
    }

    #[test]
    fn cli_and_environment_layer_overrides_profile_then_defaults_apply() {
        let loaded = LoadedConfig::from_toml("settings/profiles.toml", CONFIG).unwrap();
        let selected = loaded.select_profile(None).unwrap();
        let cli = Cli::try_parse_from([
            "synology-drive-sync",
            "sync",
            "./cli-source",
            "/team/override",
            "--jobs",
            "5",
            "--exclude",
            "cache/**",
            "--log-level",
            "debug",
            "--output",
            "ndjson",
        ])
        .unwrap();
        let Invocation::Sync { arguments, .. } = cli.invocation() else {
            panic!("expected sync");
        };
        let resolved = resolve_sync(selected.values, arguments, &cli.global.output).unwrap();

        assert_eq!(resolved.source, PathBuf::from("./cli-source"));
        assert_eq!(resolved.remote, "/team/override");
        assert_eq!(resolved.behavior.jobs, 5);
        assert_eq!(resolved.behavior.compare, CompareArg::SizeOnly);
        assert_eq!(resolved.behavior.excludes, ["*.tmp", "cache/**"]);
        assert_eq!(resolved.network.retries, 1);
        assert_eq!(resolved.output.log_level, LogLevel::Debug);
        assert_eq!(resolved.output.output, OutputFormat::Ndjson);
        assert_eq!(
            resolved.authentication.password_file.as_deref(),
            Some(Path::new("settings/secrets/password"))
        );
    }

    #[test]
    fn schema_rejects_inline_secret_fields() {
        for forbidden in [
            "password = \"plaintext\"",
            "totp-secret = \"JBSWY3DPEHPK3PXP\"",
            "remote-log-token = \"bearer\"",
        ] {
            let text = format!("[profiles.default]\n{forbidden}\n");
            let error = LoadedConfig::from_toml("config.toml", &text).unwrap_err();
            assert!(matches!(error, ConfigError::Parse { .. }));
            assert!(!error.to_string().contains("plaintext"));
            assert!(!error.to_string().contains("JBSWY3DPEHPK3PXP"));
            assert!(!error.to_string().contains("bearer"));
        }
    }

    #[test]
    fn config_show_view_contains_only_non_secret_material() {
        let loaded = LoadedConfig::from_toml("settings/profiles.toml", CONFIG).unwrap();
        let view = loaded.select_profile(None).unwrap().non_secret_view();
        let json = serde_json::to_string(&view).unwrap();
        assert!(json.contains("password-file"));
        assert!(json.contains("totp-secret-file"));
        assert!(json.contains("remote-log-token-file"));
        for forbidden_key in [
            "\"password\"",
            "\"totp-secret\"",
            "\"otp\"",
            "\"remote-log-token\"",
        ] {
            assert!(!json.contains(forbidden_key), "leaking key {forbidden_key}");
        }
    }

    #[test]
    fn password_stdin_suppresses_profile_password_file() {
        let loaded = LoadedConfig::from_toml("settings/profiles.toml", CONFIG).unwrap();
        let selected = loaded.select_profile(None).unwrap();
        let cli = Cli::try_parse_from(["synology-drive-sync", "sync", "--password-stdin"]).unwrap();
        let Invocation::Sync { arguments, .. } = cli.invocation() else {
            panic!("expected sync");
        };
        let resolved = resolve_sync(selected.values, arguments, &cli.global.output).unwrap();
        assert!(resolved.authentication.password_stdin);
        assert!(resolved.authentication.password_file.is_none());
    }

    #[test]
    fn required_remote_logging_requires_an_endpoint() {
        let profile = Profile {
            remote_log_mode: Some(RemoteLogMode::Required),
            ..Profile::default()
        };
        let cli = Cli::try_parse_from([
            "synology-drive-sync",
            "doctor",
            "--url",
            "https://files.example.test",
            "--routing-only",
        ])
        .unwrap();
        let error = resolve_output(&profile, &cli.global.output).unwrap_err();
        assert!(error.to_string().contains("remote-log-url"));
    }

    #[test]
    fn profile_validation_rejects_unsafe_or_ambiguous_urls() {
        for url in [
            "not a url",
            "http://files.example.test",
            "https://alice:secret@files.example.test",
            "https://files.example.test?redirect=elsewhere",
            "https://files.example.test/#fragment",
        ] {
            let profile = Profile {
                url: Some(url.to_owned()),
                ..Profile::default()
            };
            assert!(
                validate_profile(&profile).is_err(),
                "accepted DSM URL {url}"
            );
        }

        let controlled_http = Profile {
            url: Some("http://nas.lan/proxy".to_owned()),
            allow_http: Some(true),
            ..Profile::default()
        };
        validate_profile(&controlled_http).unwrap();

        for url in [
            "http://logs.example.test/ingest",
            "https://token@logs.example.test/ingest",
            "https://logs.example.test/ingest?token=secret",
            "https://logs.example.test/ingest#fragment",
        ] {
            let profile = Profile {
                remote_log_url: Some(url.to_owned()),
                ..Profile::default()
            };
            assert!(
                validate_profile(&profile).is_err(),
                "accepted remote log URL {url}"
            );
        }
    }

    #[test]
    fn effective_cli_urls_are_validated_before_network_access() {
        let doctor = Cli::try_parse_from([
            "synology-drive-sync",
            "doctor",
            "--url",
            "http://files.example.test",
            "--routing-only",
        ])
        .unwrap();
        let Invocation::Doctor(arguments) = doctor.invocation() else {
            panic!("expected doctor");
        };
        assert!(resolve_doctor(None, arguments, &doctor.global.output).is_err());

        let credentials = Cli::try_parse_from([
            "synology-drive-sync",
            "credentials",
            "status",
            "--url",
            "https://alice:secret@files.example.test",
            "--username",
            "alice",
        ])
        .unwrap();
        let Invocation::Credentials(arguments) = credentials.invocation() else {
            panic!("expected credentials");
        };
        assert!(resolve_credential_profile(None, arguments.profile()).is_err());

        let output = Cli::try_parse_from([
            "synology-drive-sync",
            "doctor",
            "--url",
            "https://files.example.test",
            "--routing-only",
            "--remote-log-url",
            "http://logs.example.test/ingest",
        ])
        .unwrap();
        assert!(resolve_output(&Profile::default(), &output.global.output).is_err());
    }

    #[test]
    fn cross_source_cli_values_override_profile_alternatives() {
        let profile = Profile {
            source: Some(PathBuf::from("profile-source")),
            remote: Some("/share/root".to_owned()),
            url: Some("https://files.example.test".to_owned()),
            username: Some("alice".to_owned()),
            delete: Some(true),
            remote_log_url: Some("https://logs.example.test/ingest".to_owned()),
            remote_log_token_file: Some(PathBuf::from("profile-token")),
            verbose: Some(2),
            log_level: Some(LogLevel::Warn),
            ..Profile::default()
        };
        let cli = Cli::try_parse_from([
            "synology-drive-sync",
            "sync",
            "--allow-empty-source",
            "--remote-log-token-env",
            "CI_LOG_TOKEN",
        ])
        .unwrap();
        let Invocation::Sync { arguments, .. } = cli.invocation() else {
            panic!("expected sync");
        };
        let resolved = resolve_sync(Some(&profile), arguments, &cli.global.output).unwrap();

        assert!(resolved.safety.delete);
        assert!(resolved.safety.allow_empty_source);
        assert_eq!(resolved.output.log_level, LogLevel::Warn);
        assert_eq!(
            resolved.output.remote_log_token,
            Some(RemoteTokenSource::Environment("CI_LOG_TOKEN".to_owned()))
        );
    }

    #[test]
    fn destructive_and_vault_profile_defaults_have_explicit_cli_off_switches() {
        let profile = Profile {
            source: Some(PathBuf::from("profile-source")),
            remote: Some("/share/root".to_owned()),
            url: Some("https://files.example.test".to_owned()),
            username: Some("alice".to_owned()),
            delete: Some(true),
            no_vault: Some(true),
            quiet: Some(true),
            ..Profile::default()
        };
        let cli = Cli::try_parse_from([
            "synology-drive-sync",
            "sync",
            "--no-delete",
            "--vault",
            "--no-quiet",
        ])
        .unwrap();
        let Invocation::Sync { arguments, .. } = cli.invocation() else {
            panic!("expected sync");
        };
        let resolved = resolve_sync(Some(&profile), arguments, &cli.global.output).unwrap();
        assert!(!resolved.safety.delete);
        assert!(!resolved.authentication.no_vault);
        assert!(!resolved.output.quiet);
    }

    #[test]
    fn quiet_suppresses_terminal_noise_without_disabling_durable_logs() {
        let profile = Profile::default();
        let cli = Cli::try_parse_from([
            "synology-drive-sync",
            "doctor",
            "--url",
            "https://files.example.test",
            "--routing-only",
            "--quiet",
            "--log-level",
            "debug",
            "--log-file",
            "sync.log",
        ])
        .unwrap();

        let resolved = resolve_output(&profile, &cli.global.output).unwrap();
        assert!(resolved.quiet);
        assert_eq!(resolved.log_level, LogLevel::Debug);
        assert_eq!(resolved.log_file, Some(PathBuf::from("sync.log")));
    }

    #[test]
    fn implicit_empty_default_profile_is_allowed_but_explicit_missing_is_not() {
        let loaded = LoadedConfig::from_toml("config.toml", "").unwrap();
        let implicit = loaded.select_profile(None).unwrap();
        assert_eq!(implicit.name, "default");
        assert!(implicit.values.is_none());
        assert!(matches!(
            loaded.select_profile(Some("missing")),
            Err(ConfigError::MissingProfile(name)) if name == "missing"
        ));
    }

    #[test]
    fn doctor_inherits_selected_profile_integrity_and_delete_capabilities() {
        let profile = Profile {
            compare: Some(CompareArg::Metadata),
            delete: Some(true),
            ..Profile::default()
        };
        let cli = Cli::try_parse_from([
            "synology-drive-sync",
            "doctor",
            "--url",
            "https://files.example.test",
            "--routing-only",
        ])
        .unwrap();
        let Invocation::Doctor(arguments) = cli.invocation() else {
            panic!("expected doctor");
        };
        let resolved = resolve_doctor(Some(&profile), arguments, &cli.global.output).unwrap();
        assert_eq!(resolved.compare, CompareArg::Metadata);
        assert!(resolved.delete);
    }

    #[test]
    fn source_doctor_resolves_cli_overrides_and_profile_defaults() {
        let profile = Profile {
            source: Some(PathBuf::from("profile-source")),
            excludes: vec!["profile-cache/**".to_owned()],
            output: Some(OutputFormat::Ndjson),
            ..Profile::default()
        };
        let cli = Cli::try_parse_from([
            "synology-drive-sync",
            "doctor",
            "source",
            "./cli-source",
            "--hash",
            "--exclude",
            "*.tmp",
            "--output",
            "json",
        ])
        .unwrap();
        let Invocation::Doctor(arguments) = cli.invocation() else {
            panic!("expected doctor invocation");
        };
        let Some(DoctorAction::Source(source)) = arguments.action.as_ref() else {
            panic!("expected doctor source action");
        };
        let resolved = resolve_source_doctor(Some(&profile), source, &cli.global.output).unwrap();
        assert_eq!(resolved.source, PathBuf::from("./cli-source"));
        assert_eq!(resolved.excludes, ["profile-cache/**", "*.tmp"]);
        assert!(resolved.hash_content);
        assert_eq!(resolved.output.output, OutputFormat::Json);

        let fallback_cli =
            Cli::try_parse_from(["synology-drive-sync", "doctor", "source"]).unwrap();
        let Invocation::Doctor(fallback_arguments) = fallback_cli.invocation() else {
            panic!("expected doctor invocation");
        };
        let Some(DoctorAction::Source(fallback_source)) = fallback_arguments.action.as_ref() else {
            panic!("expected doctor source action");
        };
        let fallback =
            resolve_source_doctor(Some(&profile), fallback_source, &fallback_cli.global.output)
                .unwrap();
        assert_eq!(fallback.source, PathBuf::from("profile-source"));
        assert_eq!(fallback.excludes, ["profile-cache/**"]);
        assert!(!fallback.hash_content);
        assert_eq!(fallback.output.output, OutputFormat::Ndjson);
    }

    #[test]
    fn source_doctor_requires_a_cli_or_profile_source() {
        let cli = Cli::try_parse_from(["synology-drive-sync", "doctor", "source"]).unwrap();
        let Invocation::Doctor(arguments) = cli.invocation() else {
            panic!("expected doctor invocation");
        };
        let Some(DoctorAction::Source(source)) = arguments.action.as_ref() else {
            panic!("expected doctor source action");
        };
        assert!(matches!(
            resolve_source_doctor(None, source, &cli.global.output),
            Err(ConfigError::MissingValue("SOURCE"))
        ));
    }

    #[test]
    fn target_doctor_write_test_is_opt_in_and_uses_resolved_remote() {
        let profile = Profile {
            remote: Some("/team/profile".to_owned()),
            url: Some("https://files.example.test/proxy".to_owned()),
            username: Some("mirror-bot".to_owned()),
            ..Profile::default()
        };

        let write_cli =
            Cli::try_parse_from(["synology-drive-sync", "doctor", "target", "--write-test"])
                .unwrap();
        let Invocation::Doctor(write_arguments) = write_cli.invocation() else {
            panic!("expected doctor invocation");
        };
        let write =
            resolve_doctor(Some(&profile), write_arguments, &write_cli.global.output).unwrap();
        assert_eq!(write.remote.as_deref(), Some("/team/profile"));
        assert!(write.write_test);

        let explicit_cli =
            Cli::try_parse_from(["synology-drive-sync", "doctor", "target", "/team/explicit"])
                .unwrap();
        let Invocation::Doctor(explicit_arguments) = explicit_cli.invocation() else {
            panic!("expected doctor invocation");
        };
        let explicit = resolve_doctor(
            Some(&profile),
            explicit_arguments,
            &explicit_cli.global.output,
        )
        .unwrap();
        assert_eq!(explicit.remote.as_deref(), Some("/team/explicit"));
        assert!(!explicit.write_test);

        let default_cli = Cli::try_parse_from(["synology-drive-sync", "doctor"]).unwrap();
        let Invocation::Doctor(default_arguments) = default_cli.invocation() else {
            panic!("expected doctor invocation");
        };
        let default = resolve_doctor(
            Some(&profile),
            default_arguments,
            &default_cli.global.output,
        )
        .unwrap();
        assert_eq!(default.remote.as_deref(), Some("/team/profile"));
        assert!(!default.write_test);
        assert_eq!(default.compare, CompareArg::Content);
        assert!(!default.delete);
    }

    #[test]
    fn target_doctor_write_test_rejects_a_missing_destination() {
        let cli = Cli::try_parse_from([
            "synology-drive-sync",
            "doctor",
            "--url",
            "https://files.example.test",
            "--username",
            "mirror-bot",
            "target",
            "--write-test",
        ])
        .unwrap();
        let Invocation::Doctor(arguments) = cli.invocation() else {
            panic!("expected doctor invocation");
        };
        let error = resolve_doctor(None, arguments, &cli.global.output).unwrap_err();
        assert!(matches!(error, ConfigError::Invalid(_)));
        assert!(error.to_string().contains("write-test requires REMOTE"));
    }

    #[test]
    fn oversized_config_text_is_rejected_before_parsing() {
        let oversized = "a".repeat(MAX_CONFIG_BYTES + 1);
        let error = LoadedConfig::from_toml("config.toml", &oversized).unwrap_err();
        assert!(matches!(error, ConfigError::TooLarge { .. }));
    }

    #[test]
    fn oversized_config_file_is_rejected_before_reading() {
        let path = std::env::temp_dir().join(format!(
            "sdsync-oversized-config-{}.toml",
            std::process::id()
        ));
        fs::write(&path, vec![b'a'; MAX_CONFIG_BYTES + 1]).unwrap();
        let error = LoadedConfig::load(&path).unwrap_err();
        let _ = fs::remove_file(&path);
        assert!(matches!(error, ConfigError::TooLarge { .. }));
    }

    #[test]
    fn resolve_doctor_requires_username_unless_routing_only() {
        let cli = Cli::try_parse_from([
            "synology-drive-sync",
            "doctor",
            "--url",
            "https://files.example.test",
        ])
        .unwrap();
        let Invocation::Doctor(arguments) = cli.invocation() else {
            panic!("expected doctor");
        };
        let error = resolve_doctor(None, arguments, &cli.global.output).unwrap_err();
        assert!(matches!(error, ConfigError::MissingValue("--username")));
    }

    #[test]
    fn resolve_doctor_rejects_source_action_directly() {
        let cli = Cli::try_parse_from([
            "synology-drive-sync",
            "doctor",
            "--url",
            "https://files.example.test",
            "--routing-only",
            "source",
        ])
        .unwrap();
        let Invocation::Doctor(arguments) = cli.invocation() else {
            panic!("expected doctor");
        };
        let error = resolve_doctor(None, arguments, &cli.global.output).unwrap_err();
        assert_eq!(
            error.to_string(),
            "invalid effective configuration: doctor source must be resolved as a local-only diagnostic"
        );
    }

    #[test]
    fn resolve_doctor_rejects_routing_only_combined_with_target_subcommand() {
        let cli = Cli::try_parse_from([
            "synology-drive-sync",
            "doctor",
            "--url",
            "https://files.example.test",
            "--routing-only",
            "target",
        ])
        .unwrap();
        let Invocation::Doctor(arguments) = cli.invocation() else {
            panic!("expected doctor");
        };
        let error = resolve_doctor(None, arguments, &cli.global.output).unwrap_err();
        assert_eq!(
            error.to_string(),
            "invalid effective configuration: --routing-only cannot be combined with a doctor source or target subcommand"
        );
    }

    #[test]
    fn resolve_credential_profile_combines_cli_url_and_profile_username() {
        let profile = Profile {
            username: Some("mirror-bot".to_owned()),
            ..Profile::default()
        };
        let cli = Cli::try_parse_from([
            "synology-drive-sync",
            "credentials",
            "status",
            "--url",
            "https://files.example.test",
            "--allow-http",
        ])
        .unwrap();
        let Invocation::Credentials(arguments) = cli.invocation() else {
            panic!("expected credentials");
        };
        let resolved = resolve_credential_profile(Some(&profile), arguments.profile()).unwrap();
        assert_eq!(resolved.url, "https://files.example.test");
        assert_eq!(resolved.username, "mirror-bot");
        assert!(resolved.allow_http);
    }

    #[test]
    fn resolve_credential_profile_requires_username() {
        let cli = Cli::try_parse_from([
            "synology-drive-sync",
            "credentials",
            "status",
            "--url",
            "https://files.example.test",
        ])
        .unwrap();
        let Invocation::Credentials(arguments) = cli.invocation() else {
            panic!("expected credentials");
        };
        let error = resolve_credential_profile(None, arguments.profile()).unwrap_err();
        assert!(matches!(error, ConfigError::MissingValue("--username")));
    }

    #[test]
    fn validate_profile_rejects_out_of_range_jobs_retries_and_timeouts() {
        let jobs_error = validate_profile(&Profile {
            jobs: Some(0),
            ..Profile::default()
        })
        .unwrap_err();
        assert_eq!(
            jobs_error.to_string(),
            "invalid effective configuration: jobs must be between 1 and 16"
        );

        let retries_error = validate_profile(&Profile {
            retries: Some(6),
            ..Profile::default()
        })
        .unwrap_err();
        assert_eq!(
            retries_error.to_string(),
            "invalid effective configuration: retries must be between 0 and 5"
        );

        let timeout_error = validate_profile(&Profile {
            timeout: Some(0),
            ..Profile::default()
        })
        .unwrap_err();
        assert_eq!(
            timeout_error.to_string(),
            "invalid effective configuration: timeout and connect-timeout must be at least 1 second"
        );

        let connect_timeout_error = validate_profile(&Profile {
            connect_timeout: Some(0),
            ..Profile::default()
        })
        .unwrap_err();
        assert_eq!(
            connect_timeout_error.to_string(),
            "invalid effective configuration: timeout and connect-timeout must be at least 1 second"
        );

        // A zero rate would be an upload that never makes progress, not an unlimited one;
        // unlimited is spelled by leaving the key out.
        let max_rate_error = validate_profile(&Profile {
            max_rate: Some(0),
            ..Profile::default()
        })
        .unwrap_err();
        assert_eq!(
            max_rate_error.to_string(),
            "invalid effective configuration: max-rate must be at least 1 byte per second"
        );
        validate_profile(&Profile {
            max_rate: Some(1),
            ..Profile::default()
        })
        .unwrap();
    }

    /// The rate limit follows the same precedence as every other option, and unlimited has to
    /// survive resolution as `None` -- any default here would silently throttle every user.
    #[test]
    fn max_rate_resolves_from_the_command_line_then_the_profile_then_unlimited() {
        let loaded = LoadedConfig::from_toml("settings/profiles.toml", CONFIG).unwrap();
        let selected = loaded.select_profile(None).unwrap();

        let profile_only =
            Cli::try_parse_from(["synology-drive-sync", "sync", "./source", "/team"]).unwrap();
        let Invocation::Sync { arguments, .. } = profile_only.invocation() else {
            panic!("expected sync");
        };
        let resolved =
            resolve_sync(selected.values, arguments, &profile_only.global.output).unwrap();
        assert_eq!(resolved.network.max_rate, Some(65536));

        let overridden = Cli::try_parse_from([
            "synology-drive-sync",
            "sync",
            "./source",
            "/team",
            "--max-rate",
            "4096",
        ])
        .unwrap();
        let Invocation::Sync { arguments, .. } = overridden.invocation() else {
            panic!("expected sync");
        };
        let resolved = resolve_sync(selected.values, arguments, &overridden.global.output).unwrap();
        assert_eq!(resolved.network.max_rate, Some(4096));

        // No profile value and no flag: uploads stay unlimited.
        let Invocation::Sync { arguments, .. } = profile_only.invocation() else {
            panic!("expected sync");
        };
        let resolved = resolve_sync(
            Some(&Profile {
                source: Some(PathBuf::from("./source")),
                remote: Some("/team".to_owned()),
                url: Some("https://files.example.test".to_owned()),
                username: Some("mirror-bot".to_owned()),
                ..Profile::default()
            }),
            arguments,
            &profile_only.global.output,
        )
        .unwrap();
        assert_eq!(resolved.network.max_rate, None);
    }

    /// A zero rate reaching resolution from a profile is an upload that never progresses, so it
    /// must be refused there too and not only by clap's range on the flag.
    #[test]
    fn resolve_network_rejects_a_zero_profile_max_rate() {
        let cli =
            Cli::try_parse_from(["synology-drive-sync", "sync", "./source", "/team"]).unwrap();
        let Invocation::Sync { arguments, .. } = cli.invocation() else {
            panic!("expected sync");
        };
        let error = resolve_sync(
            Some(&Profile {
                url: Some("https://files.example.test".to_owned()),
                username: Some("mirror-bot".to_owned()),
                max_rate: Some(0),
                ..Profile::default()
            }),
            arguments,
            &cli.global.output,
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "invalid effective configuration: max-rate must be at least 1 byte per second"
        );
    }

    /// An unset rate limit must stay absent rather than materialising as some default, because
    /// absent is what the client reads as "unlimited".
    #[test]
    fn the_non_secret_view_reports_the_profile_rate_limit() {
        let loaded = LoadedConfig::from_toml("settings/profiles.toml", CONFIG).unwrap();
        let view = loaded.select_profile(None).unwrap().non_secret_view();
        assert_eq!(view.max_rate, Some(65536));

        let unset = NonSecretProfileView::new("default", None);
        assert_eq!(unset.max_rate, None);
        assert!(!serde_json::to_string(&unset).unwrap().contains("65536"));
    }

    #[test]
    fn validate_profile_requires_delete_true_for_allow_empty_source() {
        let error = validate_profile(&Profile {
            allow_empty_source: Some(true),
            ..Profile::default()
        })
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "invalid effective configuration: allow-empty-source requires delete=true in the same profile"
        );
    }

    #[test]
    fn validate_profile_rejects_conflicting_remote_log_token_sources() {
        let error = validate_profile(&Profile {
            remote_log_token_file: Some(PathBuf::from("token-file")),
            remote_log_token_env: Some("TOKEN_ENV".to_owned()),
            ..Profile::default()
        })
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "invalid effective configuration: remote-log-token-file conflicts with remote-log-token-env"
        );
    }

    #[test]
    fn validate_profile_requires_remote_log_url_when_mode_required() {
        let error = validate_profile(&Profile {
            remote_log_mode: Some(RemoteLogMode::Required),
            ..Profile::default()
        })
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "invalid effective configuration: remote-log-mode=required requires remote-log-url"
        );
    }

    #[test]
    fn validate_profile_requires_remote_log_url_for_token_source() {
        let error = validate_profile(&Profile {
            remote_log_token_file: Some(PathBuf::from("token-file")),
            ..Profile::default()
        })
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "invalid effective configuration: a remote-log token source requires remote-log-url"
        );
    }

    #[test]
    fn validate_profile_rejects_invalid_remote_log_token_env_name() {
        let error = validate_profile(&Profile {
            remote_log_url: Some("https://logs.example.test/ingest".to_owned()),
            remote_log_token_env: Some("BAD NAME".to_owned()),
            ..Profile::default()
        })
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "invalid effective configuration: remote-log-token-env must be a non-empty environment variable name"
        );
    }

    #[test]
    fn validate_dsm_url_reports_http_wording_when_allow_http_enabled() {
        let error = validate_profile(&Profile {
            url: Some("http://alice:secret@nas.lan/proxy".to_owned()),
            allow_http: Some(true),
            ..Profile::default()
        })
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "invalid effective configuration: url must be an absolute HTTP(S) reverse-proxy URL without credentials, query, or fragment"
        );
    }

    #[test]
    fn validate_remote_log_url_rejects_an_unparseable_url() {
        let error = validate_profile(&Profile {
            remote_log_url: Some("not a url".to_owned()),
            ..Profile::default()
        })
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "invalid effective configuration: remote-log-url must be an absolute HTTPS URL"
        );
    }

    #[test]
    fn resolve_behavior_rejects_out_of_range_profile_jobs() {
        let profile = Profile {
            source: Some(PathBuf::from("profile-source")),
            remote: Some("/team/export".to_owned()),
            url: Some("https://files.example.test".to_owned()),
            username: Some("alice".to_owned()),
            jobs: Some(20),
            ..Profile::default()
        };
        let cli = Cli::try_parse_from(["synology-drive-sync", "sync"]).unwrap();
        let Invocation::Sync { arguments, .. } = cli.invocation() else {
            panic!("expected sync");
        };
        let error = resolve_sync(Some(&profile), arguments, &cli.global.output).unwrap_err();
        assert_eq!(
            error.to_string(),
            "invalid effective configuration: jobs must be between 1 and 16"
        );
    }

    #[test]
    fn resolve_safety_rejects_allow_empty_source_without_delete() {
        let cli = Cli::try_parse_from([
            "synology-drive-sync",
            "sync",
            "./source",
            "/team/export",
            "--url",
            "https://files.example.test",
            "--username",
            "alice",
            "--allow-empty-source",
        ])
        .unwrap();
        let Invocation::Sync { arguments, .. } = cli.invocation() else {
            panic!("expected sync");
        };
        let error = resolve_sync(None, arguments, &cli.global.output).unwrap_err();
        assert_eq!(
            error.to_string(),
            "invalid effective configuration: allow-empty-source requires delete"
        );
    }

    #[test]
    fn resolve_network_rejects_out_of_range_profile_retries_and_timeouts() {
        let base_profile = Profile {
            source: Some(PathBuf::from("profile-source")),
            remote: Some("/team/export".to_owned()),
            url: Some("https://files.example.test".to_owned()),
            username: Some("alice".to_owned()),
            ..Profile::default()
        };
        let cli = Cli::try_parse_from(["synology-drive-sync", "sync"]).unwrap();
        let Invocation::Sync { arguments, .. } = cli.invocation() else {
            panic!("expected sync");
        };

        let retries_profile = Profile {
            retries: Some(6),
            ..base_profile.clone()
        };
        let error =
            resolve_sync(Some(&retries_profile), arguments, &cli.global.output).unwrap_err();
        assert_eq!(
            error.to_string(),
            "invalid effective configuration: retries must be between 0 and 5"
        );

        let timeout_profile = Profile {
            timeout: Some(0),
            ..base_profile
        };
        let error =
            resolve_sync(Some(&timeout_profile), arguments, &cli.global.output).unwrap_err();
        assert_eq!(
            error.to_string(),
            "invalid effective configuration: timeout and connect-timeout must be at least 1 second"
        );
    }

    #[test]
    fn resolve_output_derives_log_level_and_verbosity_from_cli_flags() {
        let two = Cli::try_parse_from([
            "synology-drive-sync",
            "doctor",
            "--url",
            "https://files.example.test",
            "--routing-only",
            "-v",
            "-v",
        ])
        .unwrap();
        let resolved_two = resolve_output(&Profile::default(), &two.global.output).unwrap();
        assert_eq!(resolved_two.verbosity, 2);
        assert_eq!(resolved_two.log_level, LogLevel::Trace);

        let one = Cli::try_parse_from([
            "synology-drive-sync",
            "doctor",
            "--url",
            "https://files.example.test",
            "--routing-only",
            "-v",
        ])
        .unwrap();
        let resolved_one = resolve_output(&Profile::default(), &one.global.output).unwrap();
        assert_eq!(resolved_one.verbosity, 1);
        assert_eq!(resolved_one.log_level, LogLevel::Debug);
    }

    #[test]
    fn resolve_output_derives_log_level_from_profile_verbosity_when_cli_is_silent() {
        let cli = Cli::try_parse_from([
            "synology-drive-sync",
            "doctor",
            "--url",
            "https://files.example.test",
            "--routing-only",
        ])
        .unwrap();

        let trace_profile = Profile {
            verbose: Some(2),
            ..Profile::default()
        };
        let trace = resolve_output(&trace_profile, &cli.global.output).unwrap();
        assert_eq!(trace.verbosity, 2);
        assert_eq!(trace.log_level, LogLevel::Trace);

        let debug_profile = Profile {
            verbose: Some(1),
            ..Profile::default()
        };
        let debug = resolve_output(&debug_profile, &cli.global.output).unwrap();
        assert_eq!(debug.verbosity, 1);
        assert_eq!(debug.log_level, LogLevel::Debug);
    }

    #[test]
    fn resolve_output_uses_cli_remote_log_token_file_over_profile_env() {
        let profile = Profile {
            remote_log_token_env: Some("PROFILE_ENV".to_owned()),
            ..Profile::default()
        };
        let cli = Cli::try_parse_from([
            "synology-drive-sync",
            "doctor",
            "--url",
            "https://files.example.test",
            "--routing-only",
            "--remote-log-url",
            "https://logs.example.test/ingest",
            "--remote-log-token-file",
            "cli-token",
        ])
        .unwrap();
        let resolved = resolve_output(&profile, &cli.global.output).unwrap();
        assert_eq!(
            resolved.remote_log_token,
            Some(RemoteTokenSource::File(PathBuf::from("cli-token")))
        );
    }

    #[test]
    fn resolve_output_rejects_conflicting_profile_remote_log_token_sources() {
        let profile = Profile {
            remote_log_token_file: Some(PathBuf::from("token-file")),
            remote_log_token_env: Some("TOKEN_ENV".to_owned()),
            ..Profile::default()
        };
        let cli = Cli::try_parse_from([
            "synology-drive-sync",
            "doctor",
            "--url",
            "https://files.example.test",
            "--routing-only",
        ])
        .unwrap();
        let error = resolve_output(&profile, &cli.global.output).unwrap_err();
        assert_eq!(
            error.to_string(),
            "invalid effective configuration: remote-log-token-file conflicts with remote-log-token-env"
        );
    }

    #[test]
    fn resolve_output_defaults_remote_log_token_to_env_var_when_unspecified() {
        let profile = Profile {
            remote_log_url: Some("https://logs.example.test/ingest".to_owned()),
            ..Profile::default()
        };
        let cli = Cli::try_parse_from([
            "synology-drive-sync",
            "doctor",
            "--url",
            "https://files.example.test",
            "--routing-only",
        ])
        .unwrap();
        let resolved = resolve_output(&profile, &cli.global.output).unwrap();
        assert_eq!(
            resolved.remote_log_token,
            Some(RemoteTokenSource::Environment(
                REMOTE_LOG_TOKEN_ENV.to_owned()
            ))
        );
    }

    #[test]
    fn resolve_output_rejects_token_source_without_remote_log_url() {
        let cli = Cli::try_parse_from([
            "synology-drive-sync",
            "doctor",
            "--url",
            "https://files.example.test",
            "--routing-only",
            "--remote-log-token-file",
            "cli-token",
        ])
        .unwrap();
        let error = resolve_output(&Profile::default(), &cli.global.output).unwrap_err();
        assert_eq!(
            error.to_string(),
            "invalid effective configuration: a remote-log token source requires remote-log-url"
        );
    }
}
