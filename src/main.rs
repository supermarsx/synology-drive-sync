use std::env;
use std::io::{self, BufRead, IsTerminal};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, ValueEnum};
use synology_drive_sync::api::{ApiClient, ClientOptions};
use synology_drive_sync::local::{self, IgnoreRules};
use synology_drive_sync::path::RemoteRoot;
use synology_drive_sync::plan::{self, CompareMode, PlanOptions, SyncPlan};
use synology_drive_sync::sync::{self, ExecuteOptions};
use synology_drive_sync::{Error, Result};
use zeroize::Zeroizing;

const PASSWORD_ENV: &str = "SDSYNC_PASSWORD";
const OTP_ENV: &str = "SDSYNC_OTP";

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

/// Push a local folder to a Synology Drive-backed folder using only File Station WebAPI.
#[derive(Debug, Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    /// Authoritative local directory. It is never modified.
    source: PathBuf,

    /// File Station path beginning with a shared folder, e.g. /team/project.
    remote: String,

    /// Public HTTPS reverse-proxy base URL. May include a rewritten path prefix.
    #[arg(long, env = "SDSYNC_URL")]
    url: String,

    /// Dedicated DSM account with File Station and destination write access.
    #[arg(long, env = "SDSYNC_USERNAME")]
    username: String,

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
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            print_error(&error);
            ExitCode::from(2)
        }
    }
}

fn run(cli: Cli) -> Result<()> {
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
    let root = RemoteRoot::parse(&cli.remote)?;
    let rules = IgnoreRules::build(&cli.source, &cli.excludes)?;

    eprintln!("Scanning local source {:?} ...", cli.source);
    let local = local::scan(&cli.source, &rules)?;
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

    eprintln!("Discovering File Station WebAPI through {:?} ...", cli.url);
    let mut client = ApiClient::connect(&ClientOptions {
        base_url: cli.url.clone(),
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

    let password = read_password(cli.password_stdin)?;
    authenticate(&mut client, &cli.username, &password)?;
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

fn authenticate(client: &mut ApiClient, username: &str, password: &str) -> Result<()> {
    let mut otp = secret_from_env(OTP_ENV)?;
    let mut prompted = false;
    loop {
        match client.login(username, password, otp.as_deref().map(String::as_str)) {
            Ok(()) => return Ok(()),
            Err(error)
                if matches!(error.api_code(), Some(403 | 406)) && otp.is_none() && !prompted =>
            {
                otp = Some(prompt_otp()?);
                prompted = true;
            }
            Err(error)
                if error.api_code() == Some(404) && !prompted && io::stdin().is_terminal() =>
            {
                eprintln!("The supplied DSM OTP was invalid or expired; enter a fresh code.");
                otp = Some(prompt_otp()?);
                prompted = true;
            }
            Err(error) if matches!(error.api_code(), Some(403 | 406)) => {
                return Err(Error::Message(format!(
                    "DSM requires a TOTP code; set {OTP_ENV} for non-interactive runs or run from a terminal ({error})"
                )));
            }
            Err(error) => return Err(error),
        }
    }
}

fn read_password(from_stdin: bool) -> Result<Zeroizing<String>> {
    if from_stdin {
        let mut line = String::new();
        io::stdin().lock().read_line(&mut line).map_err(|error| {
            Error::Message(format!("failed to read password from stdin: {error}"))
        })?;
        while line.ends_with(['\r', '\n']) {
            line.pop();
        }
        if line.is_empty() {
            return Err(Error::Message(
                "password read from stdin was empty".to_owned(),
            ));
        }
        return Ok(Zeroizing::new(line));
    }
    if let Some(password) = secret_from_env(PASSWORD_ENV)? {
        if password.is_empty() {
            return Err(Error::Message(format!("{PASSWORD_ENV} is empty")));
        }
        return Ok(password);
    }
    if !io::stdin().is_terminal() {
        return Err(Error::Message(format!(
            "no DSM password available; set {PASSWORD_ENV} or pass --password-stdin"
        )));
    }
    let password = rpassword::prompt_password("DSM password: ")
        .map_err(|error| Error::Message(format!("failed to read DSM password: {error}")))?;
    if password.is_empty() {
        return Err(Error::Message("DSM password was empty".to_owned()));
    }
    Ok(Zeroizing::new(password))
}

fn prompt_otp() -> Result<Zeroizing<String>> {
    if !io::stdin().is_terminal() {
        return Err(Error::Message(format!(
            "DSM requires a TOTP code; set {OTP_ENV} for a non-interactive run"
        )));
    }
    let otp = rpassword::prompt_password("DSM TOTP code: ")
        .map_err(|error| Error::Message(format!("failed to read DSM TOTP code: {error}")))?;
    if otp.is_empty() {
        return Err(Error::Message("DSM TOTP code was empty".to_owned()));
    }
    Ok(Zeroizing::new(otp))
}

fn secret_from_env(name: &str) -> Result<Option<Zeroizing<String>>> {
    match env::var(name) {
        Ok(value) => Ok(Some(Zeroizing::new(value))),
        Err(env::VarError::NotPresent) => Ok(None),
        Err(env::VarError::NotUnicode(_)) => Err(Error::Message(format!(
            "environment variable {name} is not valid Unicode"
        ))),
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
}
