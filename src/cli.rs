//! Command-line surface shared by the binary and configuration resolver.
//!
//! Secret *values* deliberately have no command-line argument. Passwords, DSM TOTP seeds,
//! and remote-log bearer tokens may be supplied through a masked prompt, standard input, an
//! OS vault, a referenced file, or a dedicated environment variable at execution time.

use std::path::PathBuf;

use clap::{ArgAction, Args, CommandFactory, FromArgMatches, Parser, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};

pub const PASSWORD_ENV: &str = "SDSYNC_PASSWORD";
pub const OTP_ENV: &str = "SDSYNC_OTP";
pub const REMOTE_LOG_TOKEN_ENV: &str = "SDSYNC_REMOTE_LOG_TOKEN";
pub const PLAN_CHANGES_EXIT_CODE: u8 = 10;

const ROOT_LONG_ABOUT: &str = "Push one local folder into a Synology Drive-backed folder through the documented File Station WebAPI.\n\nThe explicit `sync` and `plan` commands are preferred. For compatibility, the former positional form (`synology-drive-sync SOURCE REMOTE ...`) remains representable and is interpreted as `sync`. Local data is authoritative and is never modified. Remote-only data is preserved unless --delete is selected.\n\nConnection and profile files are non-secret. Passwords, TOTP seeds, and logging bearer tokens are accepted only from masked/standard input, an OS vault, a referenced file, or a dedicated environment variable.";

const ROOT_EXAMPLES: &str = "Examples:\n  synology-drive-sync sync ./export /team/export --url https://files.example.com --username mirror-bot\n  synology-drive-sync plan ./export /team/export --profile production --delete --output json\n  synology-drive-sync doctor --profile production\n  synology-drive-sync config validate --profile production\n  synology-drive-sync credentials set-password --profile production\n  synology-drive-sync completions powershell\n  synology-drive-sync manpage > synology-drive-sync.1\n  synology-drive-sync manpage --all ./man\n\nLegacy compatibility:\n  synology-drive-sync ./export /team/export --url https://files.example.com --username mirror-bot\n\nSecrets are never accepted as command-line or TOML values. Configuration may contain only paths to secret files. See `credentials --help` and the Authentication options.";

const SYNC_LONG_ABOUT: &str = "Make the remote File Station folder converge toward the local source. Missing and changed files are uploaded, and empty local directories are created. Remote-only entries are preserved by default. With --delete, the command plans an exact one-way mirror subject to deletion guards.\n\nSOURCE and REMOTE may come from positional arguments or the selected non-secret profile. CLI values override environment values, which override profile values. The local source is never modified. Select multiple complete profile jobs with --profiles or --all-profiles; every job is preflighted before sequential mutation.";

const SYNC_EXAMPLES: &str = "Examples:\n  synology-drive-sync sync ./build /team/releases --profile nas\n  synology-drive-sync sync ./photos /home/Drive/photos --url https://files.example.com --username alice --jobs 4\n  synology-drive-sync sync ./export /team/export --delete --max-delete 25 --progress always\n  printf '%s\\n' \"$DSM_PASSWORD\" | synology-drive-sync sync ./export /team/export --password-stdin --no-vault";

const PLAN_LONG_ABOUT: &str = "Discover and authenticate to File Station, scan both sides, and print the operations a sync would perform without changing the NAS. All comparison, exclusion, and deletion-safety options are identical to `sync`, so a reviewed plan can be repeated as a sync invocation.\n\nWith --exit-code, an empty plan exits 0, a plan with pending changes exits the stable code 10, and operational/configuration failures retain their normal nonzero failure code. This makes plan suitable for CI and service health checks.";

const PLAN_EXAMPLES: &str = "Examples:\n  synology-drive-sync plan ./export /team/export --profile nas\n  synology-drive-sync plan --profile production --delete --output json\n  synology-drive-sync plan ./export /team/export --compare size-only --exclude '*.tmp' --output ndjson\n  synology-drive-sync plan --profile production --exit-code || test $? -eq 10";

const DOCTOR_LONG_ABOUT: &str = "Validate local sources, selected profiles, reverse-proxy routing, required DSM/File Station APIs, authentication, and remote destinations. The default and `target` checks are non-mutating. `target --write-test` is an explicit opt-in disposable create/upload/copy/verify/cleanup probe and must be used only on a prepared non-critical destination.\n\nUse `source` for a local-only scan, --routing-only when credentials are intentionally unavailable, or `target` for an exact destination check. Without --routing-only, normal password and TOTP resolution applies.";

const DOCTOR_EXAMPLES: &str = "Examples:\n  synology-drive-sync doctor source ./export --hash --output json\n  synology-drive-sync doctor --url https://files.example.com --username mirror-bot --routing-only\n  synology-drive-sync doctor --profile production target /team/export --output json\n  synology-drive-sync doctor --profile acceptance target --write-test\n  synology-drive-sync doctor --config ./config.toml --profiles nas-a,nas-b target --output ndjson";

/// Full CLI parser. An absent subcommand represents the legacy sync form.
#[derive(Debug, Parser)]
#[command(
    name = "synology-drive-sync",
    version = env!("SDSYNC_VERSION"),
    about = "One-way File Station folder sync through a reverse proxy",
    long_about = ROOT_LONG_ABOUT,
    after_help = ROOT_EXAMPLES,
    subcommand_negates_reqs = true,
    subcommand_precedence_over_arg = true,
    disable_help_subcommand = true
)]
pub struct Cli {
    #[command(flatten)]
    pub global: GlobalArgs,

    #[command(subcommand)]
    pub command: Option<Command>,

    /// Legacy positional sync invocation retained for compatibility.
    #[command(flatten)]
    pub legacy_sync: SyncArgs,
}

impl Cli {
    /// Parse the process arguments and render any Clap error before exiting.
    pub fn parse_checked() -> Self {
        match Self::try_parse_checked_from(std::env::args_os()) {
            Ok(arguments) => arguments,
            Err(error) => error.exit(),
        }
    }

    /// Parse arguments while rejecting command-local legacy options placed before an explicit
    /// subcommand. Clap otherwise stores those values in the root legacy invocation and silently
    /// leaves the selected subcommand unset.
    pub fn try_parse_checked_from<I, T>(arguments: I) -> std::result::Result<Self, clap::Error>
    where
        I: IntoIterator<Item = T>,
        T: Into<std::ffi::OsString> + Clone,
    {
        let mut command = Self::command();
        let mut matches = command.clone().try_get_matches_from(arguments)?;

        if let Some(subcommand) = matches.subcommand_name().map(str::to_owned) {
            let misplaced = command
                .get_arguments()
                .find(|argument| {
                    !argument.is_global_set()
                        && matches.value_source(argument.get_id().as_str())
                            == Some(clap::parser::ValueSource::CommandLine)
                })
                .map(argument_label);
            if let Some(argument) = misplaced {
                return Err(command.error(
                    clap::error::ErrorKind::ArgumentConflict,
                    format!(
                        "command-specific argument {argument} was provided before the explicit \
                         `{subcommand}` subcommand; place it after the subcommand"
                    ),
                ));
            }
        }

        Self::from_arg_matches_mut(&mut matches).map_err(|error| error.format(&mut command))
    }

    /// Return a uniform view while retaining whether the legacy spelling was used.
    pub fn invocation(&self) -> Invocation<'_> {
        match &self.command {
            Some(Command::Sync(arguments)) => Invocation::Sync {
                arguments,
                legacy: false,
            },
            Some(Command::Plan(arguments)) => Invocation::Plan(arguments),
            Some(Command::Doctor(arguments)) => Invocation::Doctor(arguments),
            Some(Command::Config(arguments)) => Invocation::Config(arguments),
            Some(Command::Credentials(arguments)) => Invocation::Credentials(arguments),
            Some(Command::Completions(arguments)) => Invocation::Completions(arguments),
            Some(Command::Manpage(arguments)) => Invocation::Manpage(arguments),
            None => Invocation::Sync {
                arguments: &self.legacy_sync,
                legacy: true,
            },
        }
    }
}

fn argument_label(argument: &clap::Arg) -> String {
    if let Some(long) = argument.get_long() {
        format!("`--{long}`")
    } else if let Some(short) = argument.get_short() {
        format!("`-{short}`")
    } else if let Some(name) = argument.get_value_names().and_then(|names| names.first()) {
        format!("`<{name}>`")
    } else {
        format!("`{}`", argument.get_id())
    }
}

#[derive(Clone, Copy, Debug)]
pub enum Invocation<'a> {
    Sync {
        arguments: &'a SyncArgs,
        legacy: bool,
    },
    Plan(&'a PlanArgs),
    Doctor(&'a DoctorArgs),
    Config(&'a ConfigArgs),
    Credentials(&'a CredentialsArgs),
    Completions(&'a CompletionArgs),
    Manpage(&'a ManpageArgs),
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Execute a one-way push into File Station.
    #[command(long_about = SYNC_LONG_ABOUT, after_help = SYNC_EXAMPLES)]
    Sync(SyncArgs),

    /// Show the complete sync plan without modifying the NAS.
    #[command(long_about = PLAN_LONG_ABOUT, after_help = PLAN_EXAMPLES)]
    Plan(PlanArgs),

    /// Diagnose profile, proxy, API discovery, and authentication.
    #[command(long_about = DOCTOR_LONG_ABOUT, after_help = DOCTOR_EXAMPLES)]
    Doctor(DoctorArgs),

    /// Inspect and validate non-secret TOML configuration.
    #[command(
        long_about = "Inspect the active configuration path, validate TOML profiles, or print a non-secret effective profile. Passwords, OTP codes, TOTP seeds, and bearer-token values are never accepted in TOML; profiles may reference secret-file paths or a token environment-variable name only."
    )]
    Config(ConfigArgs),

    /// Store, inspect, or remove OS-vault authentication material.
    Credentials(CredentialsArgs),

    /// Write shell completion source to standard output.
    #[command(alias = "completion")]
    Completions(CompletionArgs),

    /// Write one or every roff manual page.
    #[command(
        long_about = "Write the root roff manual page to standard output. With --all DIRECTORY, create DIRECTORY when needed and write the root page plus a page for every nested subcommand there. Existing generated page names in DIRECTORY are replaced."
    )]
    Manpage(ManpageArgs),
}

#[derive(Debug, Args)]
pub struct PlanArgs {
    #[command(flatten)]
    pub sync: SyncArgs,

    /// Exit 10 when the plan contains pending changes; an empty plan exits 0.
    #[arg(long, help_heading = "Output/Logging")]
    pub exit_code: bool,
}

#[derive(Debug, Args)]
pub struct GlobalArgs {
    /// Read non-secret settings from this TOML file.
    #[arg(
        long,
        global = true,
        env = "SDSYNC_CONFIG",
        value_name = "FILE",
        help_heading = "Connection"
    )]
    pub config: Option<PathBuf>,

    /// Select a profile from --config (defaults to the file's default-profile).
    #[arg(
        long,
        global = true,
        env = "SDSYNC_PROFILE",
        value_name = "NAME",
        help_heading = "Connection"
    )]
    pub profile: Option<String>,

    #[command(flatten)]
    pub output: OutputArgs,
}

/// Arguments common to `sync`, `plan`, and the legacy sync spelling.
#[derive(Debug, Args)]
pub struct SyncArgs {
    /// Authoritative local folder; may instead be set by the selected profile.
    #[arg(value_name = "SOURCE", help_heading = "Sync")]
    pub source: Option<PathBuf>,

    /// File Station logical path, e.g. /team/export; may come from the profile.
    #[arg(value_name = "REMOTE", help_heading = "Sync")]
    pub remote: Option<String>,

    /// Compatibility spelling for `plan`; inspect changes without modifying the NAS.
    #[arg(long, help_heading = "Safety")]
    pub dry_run: bool,

    #[command(flatten)]
    pub connection: ConnectionArgs,

    #[command(flatten)]
    pub authentication: AuthenticationArgs,

    #[command(flatten)]
    pub behavior: SyncBehaviorArgs,

    #[command(flatten)]
    pub safety: SafetyArgs,

    #[command(flatten)]
    pub network: NetworkArgs,

    #[command(flatten)]
    pub batch: BatchArgs,
}

/// Select complete named profile jobs for one preflighted sequential batch.
#[derive(Debug, Args, Default)]
pub struct BatchArgs {
    /// Select comma-separated profile jobs. May be repeated; execution is deterministic by profile name.
    #[arg(
        long,
        value_name = "NAME[,NAME...]",
        value_delimiter = ',',
        action = ArgAction::Append,
        conflicts_with = "all_profiles",
        help_heading = "Batch"
    )]
    pub profiles: Vec<String>,

    /// Select every named profile in deterministic configuration order.
    #[arg(long, conflicts_with = "profiles", help_heading = "Batch")]
    pub all_profiles: bool,

    /// Refuse a batch whose combined planned deletions exceed N.
    #[arg(
        long,
        env = "SDSYNC_MAX_TOTAL_DELETE",
        value_name = "N",
        help_heading = "Batch"
    )]
    pub max_total_delete: Option<usize>,
}

impl BatchArgs {
    pub fn requested(&self) -> bool {
        self.all_profiles || !self.profiles.is_empty()
    }
}

#[derive(Debug, Args)]
pub struct ConnectionArgs {
    /// Public reverse-proxy base URL; HTTPS is required unless --allow-http is set.
    #[arg(
        long,
        env = "SDSYNC_URL",
        value_name = "URL",
        help_heading = "Connection"
    )]
    pub url: Option<String>,

    /// Dedicated DSM account with File Station and destination access.
    #[arg(
        long,
        env = "SDSYNC_USERNAME",
        value_name = "USER",
        help_heading = "Connection"
    )]
    pub username: Option<String>,
}

#[derive(Debug, Args)]
pub struct AuthenticationArgs {
    /// Read the DSM password from the first line of standard input.
    #[arg(
        long,
        env = "SDSYNC_PASSWORD_STDIN",
        conflicts_with = "password_file",
        help_heading = "Authentication"
    )]
    pub password_stdin: bool,

    /// Read the DSM password from FILE; the file contents are never stored in config.
    #[arg(
        long,
        env = "SDSYNC_PASSWORD_FILE",
        value_name = "FILE",
        conflicts_with = "password_stdin",
        help_heading = "Authentication"
    )]
    pub password_file: Option<PathBuf>,

    /// Read DSM's Base32 TOTP seed or otpauth URI from FILE.
    #[arg(
        long,
        env = "SDSYNC_TOTP_SECRET_FILE",
        value_name = "FILE",
        help_heading = "Authentication"
    )]
    pub totp_secret_file: Option<PathBuf>,

    /// Do not read the password or TOTP seed from the OS credential vault.
    #[arg(
        long,
        env = "SDSYNC_NO_VAULT",
        conflicts_with = "vault",
        help_heading = "Authentication"
    )]
    pub no_vault: bool,

    /// Enable OS-vault lookup even when the selected profile has no-vault=true.
    #[arg(long, conflicts_with = "no_vault", help_heading = "Authentication")]
    pub vault: bool,
}

#[derive(Debug, Args)]
pub struct SyncBehaviorArgs {
    /// File comparison strategy.
    #[arg(long, env = "SDSYNC_COMPARE", value_enum, help_heading = "Sync")]
    pub compare: Option<CompareArg>,

    /// Maximum number of concurrent uploads (1 through 16).
    #[arg(
        long,
        env = "SDSYNC_JOBS",
        value_name = "N",
        value_parser = clap::value_parser!(u8).range(1..=16),
        help_heading = "Sync"
    )]
    pub jobs: Option<u8>,

    /// Add a gitignore-style exclusion. May be repeated.
    #[arg(
        long = "exclude",
        value_name = "PATTERN",
        action = ArgAction::Append,
        help_heading = "Sync",
        long_help = "Add a gitignore-style exclusion; may be repeated. Patterns are matched in the order given, and a pattern beginning with `!` negates a prior match instead of excluding it, exactly like a .gitignore line. This makes it possible to exclude everything and then re-include a narrow subset. For example, `--exclude '*' --exclude '!*.pdf'` excludes every file except PDFs."
    )]
    pub excludes: Vec<String>,
}

#[derive(Debug, Args)]
pub struct SafetyArgs {
    /// Delete remote-only entries and permit remote type replacement.
    #[arg(long, env = "SDSYNC_DELETE", help_heading = "Safety")]
    pub delete: bool,

    /// Preserve remote-only entries even when the selected profile has delete=true.
    #[arg(long, conflicts_with = "delete", help_heading = "Safety")]
    pub no_delete: bool,

    /// Permit --delete when the local scan contains no payload files.
    #[arg(long, env = "SDSYNC_ALLOW_EMPTY_SOURCE", help_heading = "Safety")]
    pub allow_empty_source: bool,

    /// Refuse a plan containing more than N remote deletions.
    #[arg(
        long,
        env = "SDSYNC_MAX_DELETE",
        value_name = "N",
        help_heading = "Safety"
    )]
    pub max_delete: Option<usize>,
}

#[derive(Debug, Args)]
pub struct NetworkArgs {
    /// Retry transient metadata and upload failures this many times (0 through 5).
    #[arg(
        long,
        env = "SDSYNC_RETRIES",
        value_name = "N",
        value_parser = clap::value_parser!(u8).range(0..=5),
        help_heading = "Network"
    )]
    pub retries: Option<u8>,

    /// Upload/background-operation timeout in seconds; it must cover the largest upload. Control-plane requests are capped at 10 seconds.
    #[arg(
        long,
        env = "SDSYNC_TIMEOUT",
        value_name = "SECONDS",
        value_parser = clap::value_parser!(u64).range(1..),
        help_heading = "Network"
    )]
    pub timeout: Option<u64>,

    /// TCP/TLS connection timeout in seconds.
    #[arg(
        long,
        env = "SDSYNC_CONNECT_TIMEOUT",
        value_name = "SECONDS",
        value_parser = clap::value_parser!(u64).range(1..),
        help_heading = "Network"
    )]
    pub connect_timeout: Option<u64>,

    /// Add a PEM CA certificate for a private reverse-proxy PKI.
    #[arg(
        long,
        env = "SDSYNC_CA_CERTIFICATE",
        value_name = "PEM",
        help_heading = "Network"
    )]
    pub ca_certificate: Option<PathBuf>,

    /// Allow HTTP. Intended only for controlled LAN testing.
    #[arg(long, env = "SDSYNC_ALLOW_HTTP", help_heading = "Network")]
    pub allow_http: bool,

    /// Disable TLS certificate verification. Prefer --ca-certificate.
    #[arg(
        long,
        env = "SDSYNC_DANGER_ACCEPT_INVALID_CERTS",
        help_heading = "Network"
    )]
    pub danger_accept_invalid_certs: bool,
}

#[derive(Debug, Args)]
pub struct OutputArgs {
    /// Increase diagnostic detail; repeat as -vv. Conflicts with --quiet.
    #[arg(
        short = 'v',
        long,
        global = true,
        action = ArgAction::Count,
        conflicts_with = "quiet",
        help_heading = "Output/Logging"
    )]
    pub verbose: u8,

    /// Suppress non-error human diagnostics and progress output.
    #[arg(
        short = 'q',
        long = "quiet",
        global = true,
        env = "SDSYNC_QUIET",
        conflicts_with = "verbose",
        help_heading = "Output/Logging"
    )]
    pub quiet: bool,

    /// Re-enable terminal diagnostics when the selected profile has quiet=true.
    #[arg(
        long = "no-quiet",
        global = true,
        conflicts_with = "quiet",
        help_heading = "Output/Logging"
    )]
    pub no_quiet: bool,

    /// Set the minimum log severity; overrides -v and profile verbosity.
    #[arg(
        long,
        global = true,
        env = "SDSYNC_LOG_LEVEL",
        value_enum,
        help_heading = "Output/Logging"
    )]
    pub log_level: Option<LogLevel>,

    /// Choose human-readable or JSON structured logs.
    #[arg(
        long,
        global = true,
        env = "SDSYNC_LOG_FORMAT",
        value_enum,
        help_heading = "Output/Logging"
    )]
    pub log_format: Option<LogFormat>,

    /// Append logs to FILE in addition to standard error.
    #[arg(
        long,
        global = true,
        env = "SDSYNC_LOG_FILE",
        value_name = "FILE",
        help_heading = "Output/Logging"
    )]
    pub log_file: Option<PathBuf>,

    /// Send structured log events to this HTTPS collector endpoint.
    #[arg(
        long,
        global = true,
        env = "SDSYNC_REMOTE_LOG_URL",
        value_name = "URL",
        help_heading = "Output/Logging"
    )]
    pub remote_log_url: Option<String>,

    /// Read the remote-log bearer token from FILE.
    #[arg(
        long,
        global = true,
        env = "SDSYNC_REMOTE_LOG_TOKEN_FILE",
        value_name = "FILE",
        conflicts_with = "remote_log_token_env",
        help_heading = "Output/Logging"
    )]
    pub remote_log_token_file: Option<PathBuf>,

    /// Read the remote-log bearer token from environment variable NAME.
    #[arg(
        long,
        global = true,
        env = "SDSYNC_REMOTE_LOG_TOKEN_ENV",
        value_name = "NAME",
        conflicts_with = "remote_log_token_file",
        help_heading = "Output/Logging",
        long_help = "Read the remote-log bearer token from environment variable NAME. If neither token-source option is present, SDSYNC_REMOTE_LOG_TOKEN is used. This argument carries a variable name, never the token value."
    )]
    pub remote_log_token_env: Option<String>,

    /// Decide whether remote logging failure is tolerated.
    #[arg(
        long,
        global = true,
        env = "SDSYNC_REMOTE_LOG_MODE",
        value_enum,
        help_heading = "Output/Logging"
    )]
    pub remote_log_mode: Option<RemoteLogMode>,

    /// Control terminal progress rendering.
    #[arg(
        long,
        global = true,
        env = "SDSYNC_PROGRESS",
        value_enum,
        help_heading = "Output/Logging"
    )]
    pub progress: Option<ProgressMode>,

    /// Select command-result output independently from diagnostic logs.
    #[arg(
        long,
        global = true,
        env = "SDSYNC_OUTPUT",
        value_enum,
        help_heading = "Output/Logging"
    )]
    pub output: Option<OutputFormat>,
}

#[derive(Debug, Args)]
pub struct DoctorArgs {
    #[command(flatten)]
    pub connection: ConnectionArgs,

    #[command(flatten)]
    pub authentication: AuthenticationArgs,

    #[command(flatten)]
    pub network: NetworkArgs,

    #[command(flatten)]
    pub batch: BatchArgs,

    /// Optionally validate access to this File Station logical path.
    #[arg(
        long,
        env = "SDSYNC_REMOTE",
        value_name = "REMOTE",
        help_heading = "Sync"
    )]
    pub remote: Option<String>,

    /// Stop after TLS, reverse-proxy, and API-discovery checks; do not authenticate.
    #[arg(long, help_heading = "Safety")]
    pub routing_only: bool,

    #[command(subcommand)]
    pub action: Option<DoctorAction>,
}

#[derive(Debug, Subcommand)]
pub enum DoctorAction {
    /// Validate a local source without contacting DSM or modifying any file.
    Source(DoctorSourceArgs),
    /// Diagnose one File Station destination; optionally run a disposable write probe.
    Target(DoctorTargetArgs),
}

#[derive(Debug, Args)]
pub struct DoctorSourceArgs {
    /// Local source folder; may instead come from each selected profile.
    #[arg(value_name = "SOURCE", help_heading = "Source diagnostic")]
    pub source: Option<PathBuf>,

    /// Read every payload file and verify a stable MD5 snapshot.
    #[arg(long, help_heading = "Source diagnostic")]
    pub hash: bool,

    /// Add a gitignore-style exclusion for this diagnostic. May be repeated.
    #[arg(
        long = "exclude",
        value_name = "PATTERN",
        action = ArgAction::Append,
        help_heading = "Source diagnostic"
    )]
    pub excludes: Vec<String>,
}

#[derive(Debug, Args)]
pub struct DoctorTargetArgs {
    /// File Station logical destination; may instead come from each selected profile.
    #[arg(value_name = "REMOTE", help_heading = "Target diagnostic")]
    pub remote: Option<String>,

    /// Create, upload, verify, optionally server-copy, and remove a unique disposable probe.
    #[arg(long, help_heading = "Target diagnostic")]
    pub write_test: bool,
}

#[derive(Debug, Args)]
pub struct ConfigArgs {
    #[command(subcommand)]
    pub action: ConfigAction,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Subcommand)]
pub enum ConfigAction {
    /// Print the selected or platform-default configuration path.
    Path,
    /// Write a commented starter configuration to the selected or platform-default path.
    #[command(
        long_about = "Write the documented starter configuration to --config, or to the platform-default path when --config is absent. Missing parent directories are created. An existing file is never replaced without --force, and the starter file contains placeholder values that must be edited before use. It is non-secret by construction: it configures only secret-file paths and a token environment-variable name, never a secret value."
    )]
    Init {
        /// Replace an existing configuration file. Its previous contents are lost.
        #[arg(long)]
        force: bool,
    },
    /// Parse and validate the configuration and selected profile without contacting DSM.
    Validate,
    /// Print effective non-secret values for the selected profile.
    #[command(
        long_about = "Print the selected profile after path resolution and built-in defaults are applied. Secret values can never appear: password, TOTP, OTP, and bearer-token material are excluded by the TOML schema. Secret-file paths and token environment-variable names may be shown."
    )]
    Show,
}

#[derive(Debug, Args)]
pub struct CredentialsArgs {
    #[command(subcommand)]
    pub action: CredentialAction,
}

impl CredentialsArgs {
    pub fn profile(&self) -> &CredentialProfileArgs {
        match &self.action {
            CredentialAction::SetPassword(arguments) => &arguments.profile,
            CredentialAction::SetTotp(arguments) => &arguments.profile,
            CredentialAction::Status(arguments) => &arguments.profile,
            CredentialAction::Remove(arguments) => &arguments.profile,
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum CredentialAction {
    /// Store or replace a DSM password using a prompt, stdin, or a referenced file.
    SetPassword(SetPasswordArgs),
    /// Import an existing DSM TOTP manual key or provisioning URI.
    SetTotp(SetTotpArgs),
    /// Report only whether each OS-vault entry exists.
    Status(CredentialStatusArgs),
    /// Remove one or both entries from the current user's OS vault.
    Remove(CredentialRemoveArgs),
}

#[derive(Debug, Args)]
pub struct CredentialProfileArgs {
    /// Public reverse-proxy URL that owns this credential profile.
    #[arg(
        long,
        env = "SDSYNC_URL",
        value_name = "URL",
        help_heading = "Connection"
    )]
    pub url: Option<String>,

    /// DSM account that owns this credential profile.
    #[arg(
        long,
        env = "SDSYNC_USERNAME",
        value_name = "USER",
        help_heading = "Connection"
    )]
    pub username: Option<String>,

    /// Permit an HTTP credential profile for controlled LAN testing.
    #[arg(long, env = "SDSYNC_ALLOW_HTTP", help_heading = "Network")]
    pub allow_http: bool,
}

#[derive(Debug, Args)]
pub struct SetPasswordArgs {
    #[command(flatten)]
    pub profile: CredentialProfileArgs,

    /// Read the password from the first line of standard input.
    #[arg(
        long,
        conflicts_with = "password_file",
        help_heading = "Authentication"
    )]
    pub password_stdin: bool,

    /// Read the password from FILE. There is intentionally no --password option.
    #[arg(
        long,
        env = "SDSYNC_PASSWORD_FILE",
        value_name = "FILE",
        conflicts_with = "password_stdin",
        help_heading = "Authentication"
    )]
    pub password_file: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct SetTotpArgs {
    #[command(flatten)]
    pub profile: CredentialProfileArgs,

    /// Read the seed or otpauth URI from the first line of standard input.
    #[arg(
        long = "secret-stdin",
        conflicts_with = "totp_secret_file",
        help_heading = "Authentication"
    )]
    pub secret_stdin: bool,

    /// Read the seed or otpauth URI from FILE. There is no secret-valued option.
    #[arg(
        long,
        env = "SDSYNC_TOTP_SECRET_FILE",
        value_name = "FILE",
        conflicts_with = "secret_stdin",
        help_heading = "Authentication"
    )]
    pub totp_secret_file: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct CredentialStatusArgs {
    #[command(flatten)]
    pub profile: CredentialProfileArgs,
}

#[derive(Debug, Args)]
pub struct CredentialRemoveArgs {
    #[command(flatten)]
    pub profile: CredentialProfileArgs,

    #[arg(value_enum, value_name = "KIND", help_heading = "Authentication")]
    pub kind: RemoveKind,
}

#[derive(Debug, Args)]
pub struct CompletionArgs {
    /// Shell whose completion source should be written to stdout.
    #[arg(value_enum)]
    pub shell: CompletionShell,
}

#[derive(Debug, Args)]
pub struct ManpageArgs {
    /// Write root and nested-subcommand pages into DIRECTORY instead of stdout.
    #[arg(long, value_name = "DIRECTORY")]
    pub all: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CompareArg {
    Content,
    Metadata,
    SizeOnly,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
    Off,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LogFormat {
    Human,
    Json,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RemoteLogMode {
    BestEffort,
    Required,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProgressMode {
    Auto,
    Always,
    Never,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OutputFormat {
    Human,
    Json,
    Ndjson,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum RemoveKind {
    Password,
    Totp,
    All,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum CompletionShell {
    Bash,
    Zsh,
    Fish,
    #[value(name = "powershell")]
    PowerShell,
    Elvish,
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;

    #[test]
    fn parses_explicit_sync_with_structured_output_and_logging() {
        let cli = Cli::try_parse_checked_from([
            "synology-drive-sync",
            "sync",
            "./source",
            "/team/export",
            "--url",
            "https://files.example.test",
            "--username",
            "mirror-bot",
            "--delete",
            "--max-delete",
            "12",
            "-vv",
            "--log-format",
            "json",
            "--output",
            "ndjson",
            "--remote-log-url",
            "https://logs.example.test/ingest",
            "--remote-log-token-file",
            "./token",
            "--remote-log-mode",
            "required",
        ])
        .unwrap();

        let Invocation::Sync { arguments, legacy } = cli.invocation() else {
            panic!("expected sync invocation");
        };
        assert!(!legacy);
        assert_eq!(arguments.remote.as_deref(), Some("/team/export"));
        assert!(arguments.safety.delete);
        assert_eq!(arguments.safety.max_delete, Some(12));
        assert_eq!(cli.global.output.verbose, 2);
        assert_eq!(cli.global.output.log_format, Some(LogFormat::Json));
        assert_eq!(cli.global.output.output, Some(OutputFormat::Ndjson));
    }

    #[test]
    fn legacy_sync_form_remains_representable() {
        let cli = Cli::try_parse_checked_from([
            "synology-drive-sync",
            "./source",
            "/team/export",
            "--url",
            "https://files.example.test",
            "--username",
            "alice",
            "--dry-run",
        ])
        .unwrap();

        let Invocation::Sync { arguments, legacy } = cli.invocation() else {
            panic!("expected legacy sync invocation");
        };
        assert!(legacy);
        assert_eq!(
            arguments.source.as_deref(),
            Some(std::path::Path::new("./source"))
        );
        assert_eq!(arguments.remote.as_deref(), Some("/team/export"));
        assert!(arguments.dry_run);
    }

    #[test]
    fn secret_values_are_not_command_line_arguments() {
        for arguments in [
            vec![
                "synology-drive-sync",
                "sync",
                "./source",
                "/share/root",
                "--password",
                "do-not-accept",
            ],
            vec![
                "synology-drive-sync",
                "credentials",
                "set-totp",
                "--totp-secret",
                "do-not-accept",
            ],
            vec![
                "synology-drive-sync",
                "doctor",
                "--remote-log-token",
                "do-not-accept",
            ],
        ] {
            assert!(Cli::try_parse_from(arguments).is_err());
        }
    }

    #[test]
    fn credentials_accept_secret_file_references() {
        let cli = Cli::try_parse_from([
            "synology-drive-sync",
            "credentials",
            "set-totp",
            "--profile",
            "production",
            "--totp-secret-file",
            "./totp.seed",
        ])
        .unwrap();

        let Invocation::Credentials(CredentialsArgs {
            action: CredentialAction::SetTotp(arguments),
        }) = cli.invocation()
        else {
            panic!("expected set-totp invocation");
        };
        assert_eq!(
            arguments.totp_secret_file.as_deref(),
            Some(std::path::Path::new("./totp.seed"))
        );
    }

    #[test]
    fn plan_exit_code_and_packaging_commands_have_stable_forms() {
        let plan = Cli::try_parse_from([
            "synology-drive-sync",
            "plan",
            "./source",
            "/share/root",
            "--exit-code",
        ])
        .unwrap();
        let Invocation::Plan(arguments) = plan.invocation() else {
            panic!("expected plan invocation");
        };
        assert!(arguments.exit_code);
        assert_eq!(PLAN_CHANGES_EXIT_CODE, 10);

        let completions =
            Cli::try_parse_from(["synology-drive-sync", "completions", "powershell"]).unwrap();
        assert!(matches!(
            completions.invocation(),
            Invocation::Completions(CompletionArgs {
                shell: CompletionShell::PowerShell
            })
        ));
        let root_manpage = Cli::try_parse_from(["synology-drive-sync", "manpage"]).unwrap();
        assert!(matches!(
            root_manpage.invocation(),
            Invocation::Manpage(ManpageArgs { all: None })
        ));

        let all_manpages =
            Cli::try_parse_from(["synology-drive-sync", "manpage", "--all", "./man"]).unwrap();
        let Invocation::Manpage(arguments) = all_manpages.invocation() else {
            panic!("expected recursive manpage invocation");
        };
        assert_eq!(
            arguments.all.as_deref(),
            Some(std::path::Path::new("./man"))
        );
    }

    #[test]
    fn config_commands_accept_global_profile_after_the_action() {
        for action in ["path", "init", "validate", "show"] {
            let cli = Cli::try_parse_from([
                "synology-drive-sync",
                "config",
                action,
                "--profile",
                "production",
            ])
            .unwrap();
            assert_eq!(cli.global.profile.as_deref(), Some("production"));
            assert!(matches!(cli.invocation(), Invocation::Config(_)));
        }
    }

    #[test]
    fn config_init_replacement_is_opt_in_and_belongs_only_to_init() {
        let default = Cli::try_parse_from(["synology-drive-sync", "config", "init"]).unwrap();
        assert!(matches!(
            default.invocation(),
            Invocation::Config(ConfigArgs {
                action: ConfigAction::Init { force: false }
            })
        ));

        let forced =
            Cli::try_parse_from(["synology-drive-sync", "config", "init", "--force"]).unwrap();
        assert!(matches!(
            forced.invocation(),
            Invocation::Config(ConfigArgs {
                action: ConfigAction::Init { force: true }
            })
        ));

        for action in ["path", "validate", "show"] {
            assert!(
                Cli::try_parse_from(["synology-drive-sync", "config", action, "--force"]).is_err(),
                "--force must not be accepted by config {action}"
            );
        }
    }

    #[test]
    fn global_options_work_before_and_after_every_subcommand() {
        let cases = [
            (
                vec!["--quiet", "sync", "./source", "/share/root"],
                vec!["sync", "./source", "/share/root", "--quiet"],
            ),
            (
                vec!["--quiet", "plan", "./source", "/share/root"],
                vec!["plan", "./source", "/share/root", "--quiet"],
            ),
            (
                vec!["--quiet", "doctor", "--routing-only"],
                vec!["doctor", "--routing-only", "--quiet"],
            ),
            (
                vec!["--quiet", "config", "validate"],
                vec!["config", "validate", "--quiet"],
            ),
            (
                vec!["--quiet", "credentials", "status"],
                vec!["credentials", "status", "--quiet"],
            ),
            (
                vec!["--quiet", "completions", "bash"],
                vec!["completions", "bash", "--quiet"],
            ),
            (vec!["--quiet", "manpage"], vec!["manpage", "--quiet"]),
        ];
        for (before, after) in cases {
            for arguments in [before, after] {
                let cli = Cli::try_parse_checked_from(
                    std::iter::once("synology-drive-sync").chain(arguments),
                )
                .unwrap();
                assert!(cli.global.output.quiet);
                assert!(cli.command.is_some());
            }
        }

        let config = Cli::try_parse_checked_from([
            "synology-drive-sync",
            "--config",
            "settings.toml",
            "config",
            "validate",
        ])
        .unwrap();
        assert!(matches!(config.invocation(), Invocation::Config(_)));

        let escaped =
            Cli::try_parse_checked_from(["synology-drive-sync", "--", "config", "/share/root"])
                .unwrap();
        let Invocation::Sync { arguments, legacy } = escaped.invocation() else {
            panic!("escaped reserved word should remain a legacy source");
        };
        assert!(legacy);
        assert_eq!(
            arguments.source.as_deref(),
            Some(std::path::Path::new("config"))
        );
    }

    #[test]
    fn checked_parser_rejects_command_specific_options_before_explicit_subcommands() {
        for arguments in [
            vec![
                "synology-drive-sync",
                "--url",
                "https://files.example.test",
                "sync",
                "./source",
                "/share/root",
            ],
            vec![
                "synology-drive-sync",
                "--delete",
                "plan",
                "./source",
                "/share/root",
            ],
        ] {
            let error = Cli::try_parse_checked_from(arguments).unwrap_err();
            assert_eq!(error.kind(), clap::error::ErrorKind::ArgumentConflict);
            let rendered = error.to_string();
            assert!(rendered.contains("place it after the subcommand"));
            assert!(rendered.contains("Usage:"));
        }
    }

    #[test]
    fn help_exposes_requested_docker_style_sections_and_examples() {
        let mut command = Cli::command();
        let mut bytes = Vec::new();
        command.write_long_help(&mut bytes).unwrap();
        let help = String::from_utf8(bytes).unwrap();

        for heading in [
            "Connection:",
            "Authentication:",
            "Sync:",
            "Safety:",
            "Network:",
            "Output/Logging:",
            "Examples:",
        ] {
            assert!(help.contains(heading), "missing {heading:?} in:\n{help}");
        }
        for subcommand in [
            "sync",
            "plan",
            "doctor",
            "config",
            "credentials",
            "completions",
            "manpage",
        ] {
            assert!(help.contains(subcommand));
        }
    }

    #[test]
    fn parses_doctor_source_and_target_diagnostics() {
        let source_cli = Cli::try_parse_checked_from([
            "synology-drive-sync",
            "doctor",
            "source",
            "./payload",
            "--hash",
            "--exclude",
            "*.tmp",
            "--exclude",
            "cache/**",
            "--output",
            "json",
        ])
        .unwrap();
        let Invocation::Doctor(source_doctor) = source_cli.invocation() else {
            panic!("expected doctor invocation");
        };
        let Some(DoctorAction::Source(source)) = source_doctor.action.as_ref() else {
            panic!("expected doctor source action");
        };
        assert_eq!(
            source.source.as_deref(),
            Some(std::path::Path::new("./payload"))
        );
        assert!(source.hash);
        assert_eq!(source.excludes, ["*.tmp", "cache/**"]);
        assert_eq!(source_cli.global.output.output, Some(OutputFormat::Json));

        let target_cli = Cli::try_parse_checked_from([
            "synology-drive-sync",
            "doctor",
            "--url",
            "https://files.example.test",
            "--username",
            "mirror-bot",
            "target",
            "/team/export",
            "--write-test",
        ])
        .unwrap();
        let Invocation::Doctor(target_doctor) = target_cli.invocation() else {
            panic!("expected doctor invocation");
        };
        let Some(DoctorAction::Target(target)) = target_doctor.action.as_ref() else {
            panic!("expected doctor target action");
        };
        assert_eq!(target.remote.as_deref(), Some("/team/export"));
        assert!(target.write_test);
    }

    #[test]
    fn batch_profile_selection_parses_csv_repetition_and_total_delete_cap() {
        let cli = Cli::try_parse_checked_from([
            "synology-drive-sync",
            "sync",
            "--profiles",
            "alpha,beta",
            "--profiles",
            "gamma",
            "--max-total-delete",
            "17",
        ])
        .unwrap();
        let Invocation::Sync { arguments, .. } = cli.invocation() else {
            panic!("expected sync invocation");
        };
        assert_eq!(arguments.batch.profiles, ["alpha", "beta", "gamma"]);
        assert!(!arguments.batch.all_profiles);
        assert_eq!(arguments.batch.max_total_delete, Some(17));
        assert!(arguments.batch.requested());

        let doctor_cli = Cli::try_parse_checked_from([
            "synology-drive-sync",
            "doctor",
            "--profiles",
            "alpha,beta",
            "--profiles",
            "gamma",
            "target",
        ])
        .unwrap();
        let Invocation::Doctor(doctor) = doctor_cli.invocation() else {
            panic!("expected doctor invocation");
        };
        assert_eq!(doctor.batch.profiles, ["alpha", "beta", "gamma"]);
        assert!(matches!(
            doctor.action.as_ref(),
            Some(DoctorAction::Target(_))
        ));
    }

    #[test]
    fn all_profiles_and_total_delete_cap_parse_for_plan() {
        let cli = Cli::try_parse_checked_from([
            "synology-drive-sync",
            "plan",
            "--all-profiles",
            "--max-total-delete",
            "23",
        ])
        .unwrap();
        let Invocation::Plan(arguments) = cli.invocation() else {
            panic!("expected plan invocation");
        };
        assert!(arguments.sync.batch.all_profiles);
        assert!(arguments.sync.batch.profiles.is_empty());
        assert_eq!(arguments.sync.batch.max_total_delete, Some(23));
        assert!(arguments.sync.batch.requested());
    }

    #[test]
    fn batch_conflicts_and_invalid_doctor_positionals_are_rejected() {
        let conflict = Cli::try_parse_checked_from([
            "synology-drive-sync",
            "sync",
            "--profiles",
            "alpha",
            "--all-profiles",
        ])
        .unwrap_err();
        assert_eq!(conflict.kind(), clap::error::ErrorKind::ArgumentConflict);

        for arguments in [
            vec!["synology-drive-sync", "doctor", "/team/export"],
            vec!["synology-drive-sync", "doctor", "source", "./one", "./two"],
            vec!["synology-drive-sync", "doctor", "target", "/one", "/two"],
            vec!["synology-drive-sync", "doctor", "source", "--write-test"],
            vec!["synology-drive-sync", "doctor", "target", "--hash"],
        ] {
            let parsed = Cli::try_parse_checked_from(arguments.iter().copied());
            assert!(parsed.is_err(), "accepted invalid arguments {arguments:?}");
        }
    }

    #[test]
    fn credentials_profile_accessor_covers_every_action_variant() {
        let set_password = CredentialsArgs {
            action: CredentialAction::SetPassword(SetPasswordArgs {
                profile: CredentialProfileArgs {
                    url: Some("https://files.example.test".to_owned()),
                    username: None,
                    allow_http: false,
                },
                password_stdin: false,
                password_file: None,
            }),
        };
        assert_eq!(
            set_password.profile().url.as_deref(),
            Some("https://files.example.test")
        );

        let set_totp = CredentialsArgs {
            action: CredentialAction::SetTotp(SetTotpArgs {
                profile: CredentialProfileArgs {
                    url: None,
                    username: Some("alice".to_owned()),
                    allow_http: false,
                },
                secret_stdin: false,
                totp_secret_file: None,
            }),
        };
        assert_eq!(set_totp.profile().username.as_deref(), Some("alice"));

        let status = CredentialsArgs {
            action: CredentialAction::Status(CredentialStatusArgs {
                profile: CredentialProfileArgs {
                    url: None,
                    username: None,
                    allow_http: true,
                },
            }),
        };
        assert!(status.profile().allow_http);

        let remove = CredentialsArgs {
            action: CredentialAction::Remove(CredentialRemoveArgs {
                profile: CredentialProfileArgs {
                    url: Some("https://remove.example.test".to_owned()),
                    username: None,
                    allow_http: false,
                },
                kind: RemoveKind::All,
            }),
        };
        assert_eq!(
            remove.profile().url.as_deref(),
            Some("https://remove.example.test")
        );
    }

    #[test]
    fn checked_parser_rejects_a_misplaced_positional_legacy_argument() {
        let error = Cli::try_parse_checked_from([
            "synology-drive-sync",
            "./source",
            "sync",
            "./other",
            "/team/export",
        ])
        .unwrap_err();
        assert_eq!(error.kind(), clap::error::ErrorKind::ArgumentConflict);
        let rendered = error.to_string();
        assert!(
            rendered.contains("<SOURCE>"),
            "unexpected message: {rendered}"
        );
        assert!(rendered.contains("place it after the subcommand"));
    }

    #[test]
    fn diagnostic_and_batch_help_exposes_safety_controls() {
        let mut command = Cli::command();
        let doctor = command.find_subcommand_mut("doctor").unwrap();
        let mut doctor_bytes = Vec::new();
        doctor.write_long_help(&mut doctor_bytes).unwrap();
        let doctor_help = String::from_utf8(doctor_bytes).unwrap();
        assert!(doctor_help.contains("source"));
        assert!(doctor_help.contains("target"));

        let source = doctor.find_subcommand_mut("source").unwrap();
        let mut source_bytes = Vec::new();
        source.write_long_help(&mut source_bytes).unwrap();
        let source_help = String::from_utf8(source_bytes).unwrap();
        assert!(source_help.contains("--hash"));
        assert!(source_help.contains("--exclude"));

        let target = doctor.find_subcommand_mut("target").unwrap();
        let mut target_bytes = Vec::new();
        target.write_long_help(&mut target_bytes).unwrap();
        let target_help = String::from_utf8(target_bytes).unwrap();
        assert!(target_help.contains("--write-test"));
        assert!(target_help.contains("disposable"));

        let sync = command.find_subcommand_mut("sync").unwrap();
        let mut sync_bytes = Vec::new();
        sync.write_long_help(&mut sync_bytes).unwrap();
        let sync_help = String::from_utf8(sync_bytes).unwrap();
        for option in ["--profiles", "--all-profiles", "--max-total-delete"] {
            assert!(
                sync_help.contains(option),
                "missing {option:?} in:\n{sync_help}"
            );
        }
        assert!(sync_help.contains("deterministic by profile name"));
    }
}
