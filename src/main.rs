use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand, ValueEnum};
use synology_drive_sync::api::{ApiClient, ClientOptions};
use synology_drive_sync::local::{self, IgnoreRules};
use synology_drive_sync::path::RemoteRoot;
use synology_drive_sync::plan::{self, CompareMode, PlanOptions, SyncPlan};
use synology_drive_sync::sync::{self, ExecuteOptions};
use synology_drive_sync::{Error, Result};

mod credentials;

#[derive(Clone, Copy, Debug, ValueEnum)]
enum CompareArg {
    /// Compare byte length and modification time (recommended).
    Metadata,
    /// Compare byte length only; faster but can miss same-size changes.
    SizeOnly,
}

impl From<CompareArg> for CompareMode {
    fn from(value: CompareArg) -> Self {
        match value {
            CompareArg::Metadata => Self::Metadata,
            CompareArg::SizeOnly => Self::SizeOnly,
        }
    }
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Store, inspect, or remove credentials in the current user's OS vault.
    Credentials(credentials::CredentialsArgs),
}

/// Push a local folder to a Synology Drive-backed folder using only File Station WebAPI.
#[derive(Debug, Parser)]
#[command(
    version,
    about,
    long_about = None,
    subcommand_negates_reqs = true,
    args_conflicts_with_subcommands = true,
    after_help = "Use `synology-drive-sync credentials --help` to manage OS-vault authentication."
)]
struct Cli {
    /// Authoritative local directory. It is never modified.
    #[arg(required = true)]
    source: Option<PathBuf>,

    /// File Station path beginning with a shared folder, e.g. /team/project.
    #[arg(required = true)]
    remote: Option<String>,

    /// Public HTTPS reverse-proxy base URL. May include a rewritten path prefix.
    #[arg(long, env = "SDSYNC_URL", required = true)]
    url: Option<String>,

    /// Dedicated DSM account with File Station and destination write access.
    #[arg(long, env = "SDSYNC_USERNAME", required = true)]
    username: Option<String>,

    /// Delete remote-only entries and permit remote type replacement.
    #[arg(long)]
    delete: bool,

    /// Show the complete plan without changing the NAS.
    #[arg(long)]
    dry_run: bool,

    /// Permit --delete when the scanned local source contains no payload files.
    #[arg(long, requires = "delete")]
    allow_empty_source: bool,

    /// Refuse a plan with more remote deletions than this number.
    #[arg(long, default_value_t = 100)]
    max_delete: usize,

    /// File comparison strategy.
    #[arg(long, value_enum, default_value_t = CompareArg::Metadata)]
    compare: CompareArg,

    /// Maximum parallel uploads.
    #[arg(long, default_value_t = 2)]
    jobs: usize,

    /// Additional gitignore-style exclusion; repeat as needed.
    #[arg(long = "exclude", value_name = "PATTERN")]
    excludes: Vec<String>,

    /// Read the DSM password from the first line of standard input.
    #[arg(long)]
    password_stdin: bool,

    /// Do not read the password or TOTP seed from the OS credential vault.
    #[arg(long)]
    no_vault: bool,

    /// Retry transient metadata/upload failures this many times.
    #[arg(long, default_value_t = 2)]
    retries: u32,

    /// Per-request timeout in seconds; it must cover the largest upload.
    #[arg(long, default_value_t = 7200)]
    timeout: u64,

    /// TCP/TLS connection timeout in seconds.
    #[arg(long, default_value_t = 15)]
    connect_timeout: u64,

    /// Add a PEM CA certificate for a private reverse-proxy PKI.
    #[arg(long, value_name = "PEM")]
    ca_certificate: Option<PathBuf>,

    /// Allow an HTTP endpoint. Intended only for trusted testing/LAN use.
    #[arg(long)]
    allow_http: bool,

    /// Disable TLS certificate verification. Dangerous; prefer --ca-certificate.
    #[arg(long)]
    danger_accept_invalid_certs: bool,

    /// Print each completed operation.
    #[arg(short, long)]
    verbose: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

fn main() -> ExitCode {
    let mut cli = Cli::parse();
    let result = match cli.command.take() {
        Some(Command::Credentials(arguments)) => credentials::run(arguments),
        None => run(cli),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            print_error(&error);
            ExitCode::from(2)
        }
    }
}

fn run(cli: Cli) -> Result<()> {
    let source = cli
        .source
        .as_deref()
        .ok_or_else(|| Error::Message("local source is required".to_owned()))?;
    let remote = cli
        .remote
        .as_deref()
        .ok_or_else(|| Error::Message("remote destination is required".to_owned()))?;
    let url = cli
        .url
        .as_deref()
        .ok_or_else(|| Error::Message("--url is required".to_owned()))?;
    let username = cli
        .username
        .as_deref()
        .ok_or_else(|| Error::Message("--username is required".to_owned()))?;
    if !(1..=16).contains(&cli.jobs) {
        return Err(Error::Message("--jobs must be between 1 and 16".to_owned()));
    }
    if cli.retries > 5 {
        return Err(Error::Message(
            "--retries must be between 0 and 5".to_owned(),
        ));
    }
    if cli.timeout == 0 || cli.connect_timeout == 0 {
        return Err(Error::Message(
            "--timeout and --connect-timeout must be at least 1 second".to_owned(),
        ));
    }
    let root = RemoteRoot::parse(remote)?;
    let rules = IgnoreRules::build(source, &cli.excludes)?;

    eprintln!("Scanning local source {source:?} ...");
    let local = local::scan(source, &rules)?;
    eprintln!(
        "Found {} files and {} directories locally.",
        local.files(),
        local.entries.len().saturating_sub(local.files())
    );

    if cli.allow_http {
        eprintln!(
            "warning: HTTP is enabled; DSM credentials and file data may be exposed in transit"
        );
    }
    if cli.danger_accept_invalid_certs {
        eprintln!("warning: TLS certificate verification is disabled");
    }

    eprintln!("Discovering File Station WebAPI through {url:?} ...");
    let mut client = ApiClient::connect(&ClientOptions {
        base_url: url.to_owned(),
        allow_http: cli.allow_http,
        accept_invalid_certs: cli.danger_accept_invalid_certs,
        ca_certificate: cli.ca_certificate.clone(),
        connect_timeout: Duration::from_secs(cli.connect_timeout),
        request_timeout: Duration::from_secs(cli.timeout),
        retries: cli.retries,
    })?;
    if cli.delete {
        client.require_delete_api()?;
    }

    let mut vault = credentials::VaultSession::new(!cli.no_vault, url, username, cli.allow_http);
    let password = credentials::read_password(cli.password_stdin, &mut vault)?;
    credentials::authenticate(&mut client, username, &password, &mut vault)?;
    drop(password);

    let operation_result = (|| {
        client.verify_share_writable(&root)?;
        eprintln!("Scanning remote destination {} ...", root.as_str());
        let remote = client.remote_inventory(&root)?;
        eprintln!(
            "Found {} entries remotely{}.",
            remote.entries.len(),
            if remote.root_exists {
                ""
            } else {
                " (destination will be created)"
            }
        );

        let plan = plan::build_plan(
            &root,
            &local,
            &remote,
            &rules,
            &PlanOptions {
                delete: cli.delete,
                allow_empty_source: cli.allow_empty_source,
                max_delete: cli.max_delete,
                compare: cli.compare.into(),
            },
        )?;
        print_plan(&plan, cli.dry_run || cli.verbose);

        if cli.dry_run {
            eprintln!("Dry run complete; no remote changes were made.");
            return Ok(());
        }
        if plan.is_empty() {
            eprintln!("Already in sync; no remote changes were needed.");
            return Ok(());
        }

        let report = sync::execute(
            &client,
            &root,
            &plan,
            ExecuteOptions {
                jobs: cli.jobs,
                dry_run: false,
            },
            |message| {
                if cli.verbose {
                    eprintln!("  {message}");
                }
            },
        )?;
        eprintln!(
            "Sync complete: {} uploaded, {} directories created, {} remote entries deleted.",
            report.uploaded, report.created, report.deleted
        );
        Ok(())
    })();

    let logout_result = client.logout();
    match (operation_result, logout_result) {
        (Err(error), Err(logout_error)) => {
            eprintln!("warning: File Station logout also failed: {logout_error}");
            Err(error)
        }
        (Err(error), _) => Err(error),
        (Ok(()), Err(error)) => Err(error),
        (Ok(()), Ok(())) => Ok(()),
    }
}

fn print_plan(plan: &SyncPlan, detailed: bool) {
    eprintln!(
        "Plan: {} uploads ({}), {} directories, {} deletions, {} unchanged files, {} protected remote entries.",
        plan.uploads.len(),
        format_bytes(plan.upload_bytes),
        plan.creates.len(),
        plan.delete_count(),
        plan.unchanged_files,
        plan.protected_entries
    );
    if !detailed {
        return;
    }
    for action in &plan.pre_deletes {
        eprintln!("  DELETE-CONFLICT {}", action.remote_path);
    }
    for action in &plan.creates {
        eprintln!("  MKDIR  {}", action.remote_path);
    }
    for action in &plan.uploads {
        eprintln!(
            "  UPLOAD {} -> {}",
            action.local.relative, action.remote_path
        );
    }
    for action in &plan.post_deletes {
        eprintln!("  DELETE {}", action.remote_path);
    }
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} {}", bytes, UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn print_error(error: &Error) {
    eprintln!("error: {error}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_human_sizes() {
        assert_eq!(format_bytes(12), "12 B");
        assert_eq!(format_bytes(1536), "1.5 KiB");
    }

    #[test]
    fn legacy_sync_invocation_remains_required_and_compatible() {
        let cli = Cli::try_parse_from([
            "synology-drive-sync",
            "./source",
            "/team/project",
            "--url",
            "https://files.example.test/nas",
            "--username",
            "alice",
            "--no-vault",
        ])
        .unwrap();

        assert!(cli.command.is_none());
        assert_eq!(cli.source, Some(PathBuf::from("./source")));
        assert_eq!(cli.remote.as_deref(), Some("/team/project"));
        assert!(cli.no_vault);

        let reserved_source = Cli::try_parse_from([
            "synology-drive-sync",
            "--url",
            "https://files.example.test",
            "--username",
            "alice",
            "--",
            "credentials",
            "/team/project",
        ])
        .unwrap();
        assert!(reserved_source.command.is_none());
        assert_eq!(reserved_source.source, Some(PathBuf::from("credentials")));
    }

    #[test]
    fn credential_command_has_no_secret_valued_argument() {
        let cli = Cli::try_parse_from([
            "synology-drive-sync",
            "credentials",
            "set-totp",
            "--url",
            "https://files.example.test/nas",
            "--username",
            "alice",
            "--secret-stdin",
        ])
        .unwrap();

        assert!(matches!(cli.command, Some(Command::Credentials(_))));

        assert!(
            Cli::try_parse_from([
                "synology-drive-sync",
                "credentials",
                "set-totp",
                "--url",
                "https://files.example.test",
                "--username",
                "alice",
                "--secret",
                "must-not-be-accepted",
            ])
            .is_err()
        );
    }
}
