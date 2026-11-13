use std::env;
use std::fs::File;
use std::io::{self, BufRead, IsTerminal, Read};
use std::path::Path;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::UNIX_EPOCH;

use sha2::{Digest, Sha256};
use synology_drive_sync::api::{ApiClient, ClientOptions, SessionHandover};
use synology_drive_sync::vault::{CredentialKind, OsVault, generate_totp, parse_totp_secret};
use synology_drive_sync::{Error, Result};
use zeroize::Zeroizing;

use crate::cli::{CredentialAction, CredentialsArgs, OTP_ENV, PASSWORD_ENV, RemoveKind};
use crate::config::{Profile, ResolvedCredentialProfile, ResolvedSync};

const MAX_STDIN_SECRET_BYTES: u64 = 4098;

pub(crate) fn run(
    credentials: &CredentialsArgs,
    resolved: &ResolvedCredentialProfile,
    selected_profile: Option<&Profile>,
    quiet: bool,
) -> Result<CredentialOutcome> {
    let vault = open_credential_vault(resolved, quiet)?;
    run_with_vault(&credentials.action, &vault, selected_profile, quiet)
}

/// Dispatch one credential action against an already-opened vault.
///
/// Split out from [`run`] so tests can exercise every arm; production always reaches this via
/// [`run`], which opens the real OS vault first.
fn run_with_vault(
    action: &CredentialAction,
    vault: &OsVault,
    selected_profile: Option<&Profile>,
    quiet: bool,
) -> Result<CredentialOutcome> {
    match action {
        CredentialAction::SetPassword(arguments) => {
            let password_file = if arguments.password_stdin {
                None
            } else {
                arguments.password_file.as_deref().or_else(|| {
                    selected_profile.and_then(|profile| profile.password_file.as_deref())
                })
            };
            let password = read_new_password(arguments.password_stdin, password_file)?;
            vault.store_password(&password)?;
            Ok(CredentialOutcome::StoredPassword)
        }
        CredentialAction::SetTotp(arguments) => {
            if !quiet {
                eprintln!(
                    "warning: storing the TOTP seed enables unattended login but places both factors under the same OS account"
                );
            }
            let secret_file = if arguments.secret_stdin {
                None
            } else {
                arguments.totp_secret_file.as_deref().or_else(|| {
                    selected_profile.and_then(|profile| profile.totp_secret_file.as_deref())
                })
            };
            let provisioning = read_totp_provisioning(arguments.secret_stdin, secret_file)?;
            let secret = parse_totp_secret(&provisioning)?;
            drop(provisioning);
            vault.store_totp_secret(&secret)?;
            Ok(CredentialOutcome::StoredTotp)
        }
        CredentialAction::Status(_) => {
            let status = vault.status()?;
            Ok(CredentialOutcome::Status {
                password_stored: status.password,
                totp_stored: status.totp,
            })
        }
        CredentialAction::Remove(arguments) => match arguments.kind {
            RemoveKind::Password => Ok(CredentialOutcome::Removed {
                password_removed: Some(vault.remove(CredentialKind::Password)?),
                totp_removed: None,
            }),
            RemoveKind::Totp => Ok(CredentialOutcome::Removed {
                password_removed: None,
                totp_removed: Some(vault.remove(CredentialKind::Totp)?),
            }),
            RemoveKind::All => Ok(CredentialOutcome::Removed {
                password_removed: Some(vault.remove(CredentialKind::Password)?),
                totp_removed: Some(vault.remove(CredentialKind::Totp)?),
            }),
        },
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CredentialOutcome {
    StoredPassword,
    StoredTotp,
    Status {
        password_stored: bool,
        totp_stored: bool,
    },
    Removed {
        password_removed: Option<bool>,
        totp_removed: Option<bool>,
    },
}

fn open_credential_vault(profile: &ResolvedCredentialProfile, quiet: bool) -> Result<OsVault> {
    if profile.allow_http && !quiet {
        eprintln!(
            "warning: this vault profile permits HTTP; credentials may be exposed during sync"
        );
    }
    OsVault::new(&profile.url, &profile.username, profile.allow_http)
}

#[derive(Clone, Copy)]
struct VaultProfile<'a> {
    url: &'a str,
    username: &'a str,
    allow_http: bool,
}

impl VaultProfile<'_> {
    fn open(self) -> Result<OsVault> {
        OsVault::new(self.url, self.username, self.allow_http)
    }
}

pub(crate) struct VaultSession<'a> {
    profile: Option<VaultProfile<'a>>,
    vault: Option<OsVault>,
    failed: bool,
}

impl<'a> VaultSession<'a> {
    pub(crate) fn new(enabled: bool, url: &'a str, username: &'a str, allow_http: bool) -> Self {
        Self {
            profile: enabled.then_some(VaultProfile {
                url,
                username,
                allow_http,
            }),
            vault: None,
            failed: false,
        }
    }

    fn load_password(&mut self) -> Result<Option<Zeroizing<String>>> {
        match self.open()? {
            Some(vault) => vault.load_password(),
            None => Ok(None),
        }
    }

    fn generate_totp(&mut self) -> Result<Option<Zeroizing<String>>> {
        let Some(vault) = self.open()? else {
            return Ok(None);
        };
        let Some(secret) = vault.load_totp_secret()? else {
            return Ok(None);
        };
        let code = generate_totp(&secret)?;
        drop(secret);
        Ok(Some(code))
    }

    fn open(&mut self) -> Result<Option<&OsVault>> {
        if self.failed {
            return Ok(None);
        }
        let Some(profile) = self.profile else {
            return Ok(None);
        };
        if self.vault.is_none() {
            match profile.open() {
                Ok(vault) => self.vault = Some(vault),
                Err(error) => {
                    self.failed = true;
                    return Err(error);
                }
            }
        }
        Ok(self.vault.as_ref())
    }
}

trait LoginClient {
    fn login(&mut self, username: &str, password: &str, otp: Option<&str>) -> Result<()>;
}

impl LoginClient for ApiClient {
    fn login(&mut self, username: &str, password: &str, otp: Option<&str>) -> Result<()> {
        ApiClient::login(self, username, password, otp)
    }
}

pub(crate) fn authenticate_with_sources(
    client: &mut ApiClient,
    username: &str,
    password: &str,
    vault: &mut VaultSession<'_>,
    totp_secret_file: Option<&Path>,
) -> Result<()> {
    let otp = secret_from_env(OTP_ENV)?;
    if let Some(code) = &otp {
        validate_otp_code(code, OTP_ENV)?;
    }
    authenticate_with_otp(client, username, password, otp, || {
        if let Some(path) = totp_secret_file {
            let provisioning = read_secret_file(path, "TOTP seed")?;
            let secret = parse_totp_secret(&provisioning)?;
            drop(provisioning);
            let code = generate_totp(&secret)?;
            drop(secret);
            Ok(Some(code))
        } else {
            vault.generate_totp()
        }
    })
}

fn authenticate_with_otp<C, F>(
    client: &mut C,
    username: &str,
    password: &str,
    otp: Option<Zeroizing<String>>,
    vault_totp: F,
) -> Result<()>
where
    C: LoginClient,
    F: FnMut() -> Result<Option<Zeroizing<String>>>,
{
    authenticate_with_otp_interaction(
        client,
        username,
        password,
        otp,
        vault_totp,
        || io::stdin().is_terminal(),
        prompt_otp,
    )
}

fn authenticate_with_otp_interaction<C, F, T, P>(
    client: &mut C,
    username: &str,
    password: &str,
    mut otp: Option<Zeroizing<String>>,
    mut vault_totp: F,
    mut is_terminal: T,
    mut prompt_otp: P,
) -> Result<()>
where
    C: LoginClient,
    F: FnMut() -> Result<Option<Zeroizing<String>>>,
    T: FnMut() -> bool,
    P: FnMut() -> Result<Zeroizing<String>>,
{
    let mut prompted = false;
    let mut generated = false;
    let mut regenerated = false;
    loop {
        match client.login(username, password, otp.as_deref().map(String::as_str)) {
            Ok(()) => return Ok(()),
            Err(error)
                if matches!(error.api_code(), Some(403 | 406)) && otp.is_none() && !prompted =>
            {
                match vault_totp() {
                    Ok(Some(code)) => {
                        otp = Some(code);
                        generated = true;
                        regenerated = false;
                    }
                    Ok(None) if is_terminal() => {
                        otp = Some(prompt_otp()?);
                        prompted = true;
                    }
                    Ok(None) => {
                        return Err(Error::Message(format!(
                            "DSM requires a TOTP code; set {OTP_ENV} for non-interactive runs or run from a terminal"
                        )));
                    }
                    Err(vault_error) if is_terminal() => {
                        eprintln!("warning: {vault_error}; falling back to a one-time code prompt");
                        otp = Some(prompt_otp()?);
                        prompted = true;
                    }
                    Err(vault_error) => {
                        return Err(Error::Message(format!(
                            "DSM requires a TOTP code, but {vault_error}; set {OTP_ENV} for this run"
                        )));
                    }
                }
            }
            Err(error) if error.api_code() == Some(404) && generated && !regenerated => {
                let refresh_error = match vault_totp() {
                    Ok(Some(fresh)) => {
                        if replace_if_changed(&mut otp, fresh) {
                            regenerated = true;
                            continue;
                        }
                        None
                    }
                    Ok(None) => None,
                    Err(error) => Some(error),
                };
                match (refresh_error, is_terminal()) {
                    (None, true) => {
                        eprintln!(
                            "The vault-generated DSM code was rejected; verify clock sync and enter a fresh code."
                        );
                        otp = Some(prompt_otp()?);
                        prompted = true;
                        generated = false;
                    }
                    (None, false) => return Err(generated_totp_rejected()),
                    (Some(vault_error), true) => {
                        eprintln!(
                            "warning: failed to refresh the vault TOTP code ({vault_error}); enter a fresh code"
                        );
                        otp = Some(prompt_otp()?);
                        prompted = true;
                        generated = false;
                    }
                    (Some(vault_error), false) => {
                        return Err(Error::Message(format!(
                            "the vault-generated DSM TOTP code was rejected and could not be refreshed because {vault_error}; verify clock sync or set {OTP_ENV} for one run"
                        )));
                    }
                }
            }
            Err(error) if error.api_code() == Some(404) && !prompted && is_terminal() => {
                if generated {
                    eprintln!(
                        "The vault-generated DSM code was rejected; verify clock sync and enter a fresh code."
                    );
                } else {
                    eprintln!("The supplied DSM OTP was invalid or expired; enter a fresh code.");
                }
                otp = Some(prompt_otp()?);
                prompted = true;
                generated = false;
            }
            Err(error) if error.api_code() == Some(404) && generated => {
                return Err(generated_totp_rejected());
            }
            Err(error) if error.api_code() == Some(404) && otp.is_some() => {
                return Err(Error::Message(format!(
                    "the supplied DSM TOTP code was rejected; set a fresh {OTP_ENV} value or run from a terminal"
                )));
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

fn replace_if_changed(current: &mut Option<Zeroizing<String>>, fresh: Zeroizing<String>) -> bool {
    if current.as_deref().map(String::as_str) == Some(fresh.as_str()) {
        return false;
    }
    *current = Some(fresh);
    true
}

fn generated_totp_rejected() -> Error {
    Error::Message(format!(
        "the vault-generated DSM TOTP code was rejected; verify the stored seed and synchronize the client and NAS clocks, or set {OTP_ENV} for one run"
    ))
}

pub(crate) fn read_password_with_file(
    from_stdin: bool,
    from_file: Option<&Path>,
    vault: &mut VaultSession<'_>,
) -> Result<Zeroizing<String>> {
    if let Some(path) = from_file {
        return read_secret_file(path, "password");
    }
    if from_stdin {
        return read_secret_line("password");
    }
    if let Some(password) = secret_from_env(PASSWORD_ENV)? {
        if password.is_empty() {
            return Err(Error::Message(format!("{PASSWORD_ENV} is empty")));
        }
        return Ok(password);
    }
    match vault.load_password() {
        Ok(Some(password)) => return Ok(password),
        Ok(None) => {}
        Err(error) if io::stdin().is_terminal() => {
            eprintln!("warning: {error}; falling back to a password prompt");
        }
        Err(error) => return Err(error),
    }
    if !io::stdin().is_terminal() {
        return Err(Error::Message(format!(
            "no DSM password available; store one with `credentials set-password`, set {PASSWORD_ENV}, or pass --password-stdin"
        )));
    }
    prompt_secret("DSM password: ", "DSM password")
}

fn read_new_password(from_stdin: bool, from_file: Option<&Path>) -> Result<Zeroizing<String>> {
    if let Some(path) = from_file {
        return read_secret_file(path, "password");
    }
    if from_stdin {
        return read_secret_line("password");
    }
    if let Some(password) = secret_from_env(PASSWORD_ENV)? {
        if password.is_empty() {
            return Err(Error::Message(format!("{PASSWORD_ENV} is empty")));
        }
        return Ok(password);
    }
    if !io::stdin().is_terminal() {
        return Err(Error::Message(format!(
            "no DSM password available; set {PASSWORD_ENV}, pass --password-stdin, or run from a terminal"
        )));
    }
    let password = prompt_secret("New DSM password: ", "DSM password")?;
    let confirmation = prompt_secret("Confirm DSM password: ", "DSM password confirmation")?;
    if password.as_str() != confirmation.as_str() {
        return Err(Error::Message("DSM passwords did not match".to_owned()));
    }
    Ok(password)
}

fn read_totp_provisioning(from_stdin: bool, from_file: Option<&Path>) -> Result<Zeroizing<String>> {
    if let Some(path) = from_file {
        return read_secret_file(path, "TOTP seed");
    }
    if from_stdin {
        return read_secret_line("TOTP seed");
    }
    if !io::stdin().is_terminal() {
        return Err(Error::Message(
            "no TOTP seed available; pass --secret-stdin or run from a terminal".to_owned(),
        ));
    }
    prompt_secret("DSM TOTP manual key or otpauth URI: ", "DSM TOTP seed")
}

/// Read one secret line from piped stdin.
///
/// This blocks until the pipe delivers a line or closes, and std restarts the read across
/// signals, so a first Ctrl-C is observed but does not unblock it. That is left as-is
/// deliberately: the path is only reachable when the operator passed `--password-stdin` or
/// `--secret-stdin`, and the alternative -- reading on a detached thread the main thread stops
/// waiting for -- would strand a live secret in a thread that never drops its `Zeroizing`
/// buffer. A second Ctrl-C exits the process instead. The masked terminal prompt below has no
/// such gap: there Ctrl-C is itself the byte that ends the read.
fn read_secret_line(label: &str) -> Result<Zeroizing<String>> {
    let mut line = Zeroizing::new(String::new());
    let mut input = io::stdin().lock().take(MAX_STDIN_SECRET_BYTES);
    input
        .read_line(&mut line)
        .map_err(|error| Error::Message(format!("failed to read {label} from stdin: {error}")))?;
    while line.ends_with(['\r', '\n']) {
        line.pop();
    }
    validate_secret_input(&line, label)?;
    Ok(line)
}

fn read_secret_file(path: &Path, label: &str) -> Result<Zeroizing<String>> {
    let file = File::open(path).map_err(|source| Error::FileIo {
        path: path.to_owned(),
        source,
    })?;
    let mut line = Zeroizing::new(String::new());
    let mut input = io::BufReader::new(file).take(MAX_STDIN_SECRET_BYTES);
    input.read_line(&mut line).map_err(|source| Error::FileIo {
        path: path.to_owned(),
        source,
    })?;
    while line.ends_with(['\r', '\n']) {
        line.pop();
    }
    validate_secret_input(&line, label)?;
    Ok(line)
}

const INTERACTIVE_SECRET_MASK: char = '*';

fn prompt_secret(prompt: &str, label: &str) -> Result<Zeroizing<String>> {
    let config = rpassword::ConfigBuilder::new()
        .password_feedback_mask(INTERACTIVE_SECRET_MASK)
        .build();
    // Masked entry puts the terminal in raw mode, which suppresses ISIG: Ctrl-C arrives as an
    // ETX byte instead of a signal. rpassword restores the terminal, re-raises SIGINT so the
    // installed handler still sets the cancellation token, and reports `Interrupted`. Mapping
    // that to `Cancelled` keeps the documented exit code 130 instead of a generic failure.
    let value = rpassword::prompt_password_with_config(prompt, config)
        .map(Zeroizing::new)
        .map_err(|error| match error.kind() {
            io::ErrorKind::Interrupted => Error::Cancelled,
            _ => Error::Message(format!("failed to read {label}: {error}")),
        })?;
    validate_secret_input(&value, label)?;
    Ok(value)
}

fn validate_secret_input(value: &str, label: &str) -> Result<()> {
    if value.is_empty() {
        return Err(Error::Message(format!("{label} was empty")));
    }
    if value.len() > 4096 {
        return Err(Error::Message(format!("{label} exceeds 4096 bytes")));
    }
    Ok(())
}

fn prompt_otp() -> Result<Zeroizing<String>> {
    if !io::stdin().is_terminal() {
        return Err(Error::Message(format!(
            "DSM requires a TOTP code; set {OTP_ENV} for a non-interactive run"
        )));
    }
    let otp = prompt_secret("DSM TOTP code: ", "DSM TOTP code")?;
    validate_otp_code(&otp, "DSM TOTP code")?;
    Ok(otp)
}

fn validate_otp_code(code: &str, source: &str) -> Result<()> {
    if code.len() != 6 || !code.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(Error::Message(format!(
            "{source} must contain exactly 6 ASCII digits"
        )));
    }
    Ok(())
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

// ---------------------------------------------------------------- a session lent by the daemon

/// What became of the session the daemon lent, as the handback's status line names it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) enum Borrowed {
    /// Nothing was lent.
    None,
    /// The lent session served the run.
    Used,
    /// DSM refused the lent session; the run logged in once, afresh, and owns that session.
    Rejected,
    /// Lent but not used: bound to other credentials, unreadable, or not usable by this run.
    Ignored,
}

impl Borrowed {
    #[cfg_attr(not(unix), allow(dead_code))]
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Used => "used",
            Self::Rejected => "rejected",
            Self::Ignored => "ignored",
        }
    }
}

/// `--session-stdin`: the socket the daemon passed as standard input, what it lent, and what
/// became of it. Process-wide, because exactly one run of one profile consumes it.
struct SessionChannel {
    #[cfg(unix)]
    socket: Option<std::os::unix::net::UnixStream>,
    lent: Option<SessionHandover>,
    /// Why the lent document was not usable, when it was not.
    unreadable: Option<String>,
    borrowed: Borrowed,
}

static SESSION_CHANNEL: Mutex<Option<SessionChannel>> = Mutex::new(None);
/// Why `--session-stdin` was given and no channel could be opened.
static SESSION_CHANNEL_FAILURE: Mutex<Option<String>> = Mutex::new(None);

fn session_channel() -> MutexGuard<'static, Option<SessionChannel>> {
    SESSION_CHANNEL
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

/// Read what the daemon lent on standard input, once, before any job runs. Never fatal: a channel
/// that cannot be used is kept as the reason, the run logs in and out as it always did, and the
/// run names it ([`Lent::Unusable`]).
pub(crate) fn open_session_channel() {
    match read_session_channel() {
        Ok(channel) => *session_channel() = Some(channel),
        Err(cause) => {
            *SESSION_CHANNEL_FAILURE
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = Some(cause);
        }
    }
}

/// Whether this process reads its standard input as the session channel.
pub(crate) fn session_channel_claims_stdin() -> bool {
    session_channel().is_some()
        || SESSION_CHANNEL_FAILURE
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_some()
}

#[cfg(unix)]
fn read_session_channel() -> std::result::Result<SessionChannel, String> {
    use std::os::fd::AsFd;
    use std::os::unix::fs::FileTypeExt;
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    use synology_drive_sync::api::{LENT_NOTHING, MAX_HANDOVER_BYTES};

    let fd = io::stdin()
        .as_fd()
        .try_clone_to_owned()
        .map_err(|error| format!("standard input cannot be duplicated: {error}"))?;
    let file = File::from(fd);
    let is_socket = file
        .metadata()
        .map(|metadata| metadata.file_type().is_socket())
        .map_err(|error| format!("standard input cannot be inspected: {error}"))?;
    if !is_socket {
        // A pipe or a file could carry a session in but never take one back.
        return Err("standard input is not a socket, so a session could not be handed back".into());
    }
    let mut socket = UnixStream::from(std::os::fd::OwnedFd::from(file));
    // The daemon writes before the run starts and closes its side, so this read never waits for
    // it; the bound covers a caller that never closes.
    socket
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|error| format!("the session channel cannot be bounded: {error}"))?;
    let mut lent = Zeroizing::new(Vec::with_capacity(MAX_HANDOVER_BYTES + 1));
    (&mut socket)
        .take(MAX_HANDOVER_BYTES as u64 + 1)
        .read_to_end(&mut lent)
        .map_err(|error| format!("the session channel cannot be read: {error}"))?;
    if lent.is_empty() {
        return Err("the daemon wrote nothing on the session channel".into());
    }
    let mut channel = SessionChannel {
        socket: Some(socket),
        lent: None,
        unreadable: None,
        borrowed: Borrowed::None,
    };
    if lent.as_slice() != LENT_NOTHING {
        match SessionHandover::decode(&lent) {
            Ok(handover) => channel.lent = Some(handover),
            Err(error) => {
                channel.unreadable = Some(error.to_string());
                channel.borrowed = Borrowed::Ignored;
            }
        }
    }
    Ok(channel)
}

#[cfg(not(unix))]
fn read_session_channel() -> std::result::Result<SessionChannel, String> {
    Err("a lent session needs a Unix socket on standard input".into())
}

/// The binding of a run's credentials: what a lent session must have been established under for
/// this run to use it. A digest of the URL, the account and the identity (device, inode, size,
/// modification time) of the password and TOTP files, so a changed secret ends the reuse. A file
/// that is absent is an identity of its own (the package names a TOTP file whether or not the
/// account has one), so adding it ends the reuse too. `None` when a secret file cannot be
/// inspected: nothing is lent then, and the login names the cause.
pub(crate) fn session_binding(settings: &ResolvedSync) -> Option<String> {
    fn identity(path: Option<&Path>) -> Option<String> {
        let Some(path) = path else {
            return Some("-".to_owned());
        };
        let metadata = match std::fs::metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Some("absent".to_owned());
            }
            Err(_) => return None,
        };
        let modified = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |since| since.as_nanos());
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Some(format!(
                "{}:{}:{}:{modified}",
                metadata.dev(),
                metadata.ino(),
                metadata.len()
            ))
        }
        #[cfg(not(unix))]
        {
            Some(format!("{}:{modified}", metadata.len()))
        }
    }

    let authentication = &settings.authentication;
    let password = identity(authentication.password_file.as_deref())?;
    let totp = identity(authentication.totp_secret_file.as_deref())?;
    let mut digest = Sha256::new();
    for part in [
        "sdsync.session-binding.v1",
        settings.connection.url.as_str(),
        settings.connection.username.as_str(),
        password.as_str(),
        totp.as_str(),
    ] {
        digest.update(part.as_bytes());
        digest.update([0]);
    }
    Some(
        digest
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect(),
    )
}

/// What a run may do with the channel.
pub(crate) enum Lent {
    /// No `--session-stdin`: log in and out as always.
    NoChannel,
    /// `--session-stdin`, and the channel could not be opened, and why: log in and out as always.
    Unusable(String),
    /// A channel, nothing lent on it: log in, and hand the session back at the end.
    Nothing,
    /// A session this run may use, bound to its own credentials.
    Session(SessionHandover),
    /// A session was lent and is not used, and why; the run logs in and hands its own back.
    Ignored(String),
}

/// Take the lent session for the one run that may use it. A second caller gets `Nothing`.
pub(crate) fn take_lent_session(binding: Option<&str>) -> Lent {
    let mut guard = session_channel();
    let Some(channel) = guard.as_mut() else {
        let failure = SESSION_CHANNEL_FAILURE
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        return failure.map_or(Lent::NoChannel, Lent::Unusable);
    };
    if let Some(cause) = channel.unreadable.take() {
        return Lent::Ignored(cause);
    }
    let Some(lent) = channel.lent.take() else {
        return Lent::Nothing;
    };
    match binding {
        Some(binding) if lent.binding() == binding => {
            channel.borrowed = Borrowed::Used;
            Lent::Session(lent)
        }
        Some(_) => {
            channel.borrowed = Borrowed::Ignored;
            Lent::Ignored("it was established under other credentials or another endpoint".into())
        }
        None => {
            channel.borrowed = Borrowed::Ignored;
            Lent::Ignored("this run's secret files cannot be inspected".into())
        }
    }
}

/// A lent session that turned out unusable after it was taken (its API map does not serve this
/// run): the handback says it was not used, so the daemon ends it.
pub(crate) fn note_lent_session_ignored() {
    if let Some(channel) = session_channel().as_mut() {
        channel.borrowed = Borrowed::Ignored;
    }
}

/// Record that DSM refused the lent session, so the handback says so and the daemon does not
/// log out a session DSM already ended. True the first time only, so the caller names it once.
pub(crate) fn note_lent_session_rejected() -> bool {
    match session_channel().as_mut() {
        Some(channel) if channel.borrowed == Borrowed::Used => {
            channel.borrowed = Borrowed::Rejected;
            true
        }
        _ => false,
    }
}

/// How a run's session ended.
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum SessionEnd {
    /// Handed back to the daemon, which now owns it; not logged out.
    HandedBack,
    /// No channel, or nothing to hand back: logged out (or attempted) as always.
    LoggedOut,
    /// The channel failed; the session was logged out instead, and why the handback failed.
    HandbackFailed(String),
}

/// End a run's session: hand it back on the channel when there is one, otherwise log out. A
/// session DSM refused (`dead`) is never handed back.
pub(crate) fn hand_back_or_log_out(
    client: &mut ApiClient,
    binding: Option<&str>,
    options: &ClientOptions,
    dead: bool,
) -> (SessionEnd, Result<()>) {
    let mut guard = session_channel();
    let Some(channel) = guard.as_mut() else {
        drop(guard);
        return (SessionEnd::LoggedOut, client.logout());
    };
    let handover = match binding {
        Some(binding) if !dead => client.hand_over_session(binding, options),
        _ => None,
    };
    let written = write_handback(channel, handover.as_ref());
    drop(guard);
    match written {
        Ok(()) if handover.is_some() => {
            client.forget_session();
            (SessionEnd::HandedBack, Ok(()))
        }
        Ok(()) => (SessionEnd::LoggedOut, client.logout()),
        Err(cause) => (SessionEnd::HandbackFailed(cause), client.logout()),
    }
}

#[cfg(unix)]
fn write_handback(
    channel: &mut SessionChannel,
    handover: Option<&SessionHandover>,
) -> std::result::Result<(), String> {
    use std::io::Write;
    use std::time::Duration;

    use synology_drive_sync::api::HANDBACK_SCHEMA;

    let Some(mut socket) = channel.socket.take() else {
        return Err("the session channel was already used".into());
    };
    let status = format!("{HANDBACK_SCHEMA} {}\n", channel.borrowed.as_str());
    let encoded = handover
        .map(SessionHandover::encode)
        .transpose()
        .map_err(|error| error.to_string())?;
    socket
        .set_write_timeout(Some(Duration::from_secs(5)))
        .and_then(|()| socket.write_all(status.as_bytes()))
        .and_then(|()| match &encoded {
            Some(encoded) => socket.write_all(encoded),
            None => Ok(()),
        })
        .and_then(|()| socket.shutdown(std::net::Shutdown::Write))
        .map_err(|error| format!("the session could not be handed back: {error}"))
}

#[cfg(not(unix))]
fn write_handback(
    _channel: &mut SessionChannel,
    _handover: Option<&SessionHandover>,
) -> std::result::Result<(), String> {
    Err("a session can only be handed back over a Unix socket".into())
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    use keyring_core::CredentialStore;

    use super::*;
    use crate::cli::{
        CredentialProfileArgs, CredentialRemoveArgs, CredentialStatusArgs, SetPasswordArgs,
        SetTotpArgs,
    };

    const TEST_SEED: &str = "JBSWY3DPEHPK3PXP";

    fn mock_vault() -> OsVault {
        let store: Arc<CredentialStore> = keyring_core::mock::Store::new().unwrap();
        OsVault::with_store("https://files.example.test/nas", "alice", false, store).unwrap()
    }

    fn scratch_dir(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("sdsync-{label}-{nonce}"));
        fs::create_dir_all(&directory).unwrap();
        directory
    }

    fn secret_file(directory: &Path, name: &str, contents: &str) -> PathBuf {
        let path = directory.join(name);
        fs::write(&path, contents).unwrap();
        path
    }

    fn profile_args() -> CredentialProfileArgs {
        CredentialProfileArgs {
            url: None,
            username: None,
            allow_http: false,
        }
    }

    fn set_password_action(password_file: Option<PathBuf>) -> CredentialAction {
        CredentialAction::SetPassword(SetPasswordArgs {
            profile: profile_args(),
            password_stdin: false,
            password_file,
        })
    }

    fn set_totp_action(totp_secret_file: Option<PathBuf>) -> CredentialAction {
        CredentialAction::SetTotp(SetTotpArgs {
            profile: profile_args(),
            secret_stdin: false,
            totp_secret_file,
        })
    }

    fn status_action() -> CredentialAction {
        CredentialAction::Status(CredentialStatusArgs {
            profile: profile_args(),
        })
    }

    fn remove_action(kind: RemoveKind) -> CredentialAction {
        CredentialAction::Remove(CredentialRemoveArgs {
            profile: profile_args(),
            kind,
        })
    }

    fn dispatch(action: &CredentialAction, vault: &OsVault) -> Result<CredentialOutcome> {
        run_with_vault(action, vault, None, true)
    }

    #[test]
    fn credential_actions_store_report_and_remove_both_entries() {
        let directory = scratch_dir("credential-lifecycle");
        let password_path = secret_file(&directory, "password.txt", "stored-password\nignored\n");
        let totp_path = secret_file(&directory, "totp.txt", "jbsw-y3dp ehpk-3pxp\nignored\n");
        let vault = mock_vault();

        assert_eq!(
            dispatch(&status_action(), &vault).unwrap(),
            CredentialOutcome::Status {
                password_stored: false,
                totp_stored: false,
            }
        );

        assert_eq!(
            dispatch(&set_password_action(Some(password_path)), &vault).unwrap(),
            CredentialOutcome::StoredPassword
        );
        assert_eq!(
            vault.load_password().unwrap().unwrap().as_str(),
            "stored-password"
        );

        // `quiet: false` also exercises the shared-OS-account warning on the set-totp path.
        assert_eq!(
            run_with_vault(&set_totp_action(Some(totp_path)), &vault, None, false).unwrap(),
            CredentialOutcome::StoredTotp
        );
        assert_eq!(
            &*vault.load_totp_secret().unwrap().unwrap(),
            &*parse_totp_secret(TEST_SEED).unwrap()
        );

        assert_eq!(
            dispatch(&status_action(), &vault).unwrap(),
            CredentialOutcome::Status {
                password_stored: true,
                totp_stored: true,
            }
        );

        assert_eq!(
            dispatch(&remove_action(RemoveKind::All), &vault).unwrap(),
            CredentialOutcome::Removed {
                password_removed: Some(true),
                totp_removed: Some(true),
            }
        );
        assert_eq!(
            dispatch(&status_action(), &vault).unwrap(),
            CredentialOutcome::Status {
                password_stored: false,
                totp_stored: false,
            }
        );
        assert_eq!(
            dispatch(&remove_action(RemoveKind::All), &vault).unwrap(),
            CredentialOutcome::Removed {
                password_removed: Some(false),
                totp_removed: Some(false),
            }
        );

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn removing_one_credential_kind_leaves_the_other_factor_intact() {
        let directory = scratch_dir("credential-remove-kinds");
        let password_path = secret_file(&directory, "password.txt", "stored-password\n");
        let totp_path = secret_file(&directory, "totp.txt", &format!("{TEST_SEED}\n"));
        let vault = mock_vault();
        dispatch(&set_password_action(Some(password_path.clone())), &vault).unwrap();
        dispatch(&set_totp_action(Some(totp_path)), &vault).unwrap();

        assert_eq!(
            dispatch(&remove_action(RemoveKind::Password), &vault).unwrap(),
            CredentialOutcome::Removed {
                password_removed: Some(true),
                totp_removed: None,
            }
        );
        assert_eq!(
            dispatch(&status_action(), &vault).unwrap(),
            CredentialOutcome::Status {
                password_stored: false,
                totp_stored: true,
            }
        );
        assert_eq!(
            dispatch(&remove_action(RemoveKind::Password), &vault).unwrap(),
            CredentialOutcome::Removed {
                password_removed: Some(false),
                totp_removed: None,
            }
        );

        dispatch(&set_password_action(Some(password_path)), &vault).unwrap();
        assert_eq!(
            dispatch(&remove_action(RemoveKind::Totp), &vault).unwrap(),
            CredentialOutcome::Removed {
                password_removed: None,
                totp_removed: Some(true),
            }
        );
        assert_eq!(
            dispatch(&status_action(), &vault).unwrap(),
            CredentialOutcome::Status {
                password_stored: true,
                totp_stored: false,
            }
        );
        assert_eq!(
            dispatch(&remove_action(RemoveKind::Totp), &vault).unwrap(),
            CredentialOutcome::Removed {
                password_removed: None,
                totp_removed: Some(false),
            }
        );

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn profile_secret_files_are_a_fallback_that_command_line_files_override() {
        let directory = scratch_dir("credential-profile-files");
        let profile = Profile {
            password_file: Some(secret_file(
                &directory,
                "profile-password.txt",
                "profile-password\n",
            )),
            totp_secret_file: Some(secret_file(&directory, "profile-totp.txt", TEST_SEED)),
            ..Profile::default()
        };
        let vault = mock_vault();

        assert_eq!(
            run_with_vault(&set_password_action(None), &vault, Some(&profile), true).unwrap(),
            CredentialOutcome::StoredPassword
        );
        assert_eq!(
            run_with_vault(&set_totp_action(None), &vault, Some(&profile), true).unwrap(),
            CredentialOutcome::StoredTotp
        );
        assert_eq!(
            vault.load_password().unwrap().unwrap().as_str(),
            "profile-password"
        );
        assert_eq!(
            &*vault.load_totp_secret().unwrap().unwrap(),
            &*parse_totp_secret(TEST_SEED).unwrap()
        );

        let explicit = secret_file(&directory, "cli-password.txt", "cli-password\n");
        run_with_vault(
            &set_password_action(Some(explicit)),
            &vault,
            Some(&profile),
            true,
        )
        .unwrap();
        assert_eq!(
            vault.load_password().unwrap().unwrap().as_str(),
            "cli-password"
        );

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn rejected_secrets_leave_the_vault_unchanged_without_echoing_the_value() {
        let directory = scratch_dir("credential-rejections");
        let vault = mock_vault();

        let marker = "REJECT-ME-NOT-BASE32";
        let bad_totp = secret_file(&directory, "bad-totp.txt", &format!("{marker}!\n"));
        let error = dispatch(&set_totp_action(Some(bad_totp)), &vault).unwrap_err();
        assert!(!error.to_string().contains(marker), "{error}");

        let empty_password = secret_file(&directory, "empty-password.txt", "\n");
        let error = dispatch(&set_password_action(Some(empty_password)), &vault).unwrap_err();
        assert_eq!(error.to_string(), "password was empty");

        let missing = directory.join("absent.txt");
        assert!(matches!(
            dispatch(&set_password_action(Some(missing.clone())), &vault),
            Err(Error::FileIo { path, .. }) if path == missing
        ));

        assert_eq!(
            dispatch(&status_action(), &vault).unwrap(),
            CredentialOutcome::Status {
                password_stored: false,
                totp_stored: false,
            }
        );

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn secret_files_use_the_first_line_without_echoing_values() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("sdsync-secret-{nonce}.txt"));
        fs::write(&path, b"first-secret\r\nignored\n").unwrap();
        let secret = read_secret_file(&path, "test secret").unwrap();
        fs::remove_file(path).unwrap();
        assert_eq!(secret.as_str(), "first-secret");
    }

    #[test]
    fn interactive_secret_feedback_uses_an_opaque_star_per_character() {
        assert_eq!(INTERACTIVE_SECRET_MASK, '*');
    }

    #[test]
    fn secret_file_failures_are_bounded_and_do_not_echo_secret_material() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("sdsync-secret-errors-{nonce}"));
        fs::create_dir_all(&directory).unwrap();

        let empty = directory.join("empty.txt");
        fs::write(&empty, b"\r\n").unwrap();
        let error = read_secret_file(&empty, "test secret").unwrap_err();
        assert_eq!(error.to_string(), "test secret was empty");

        let marker = "DO-NOT-ECHO-THIS-SECRET";
        let oversized = directory.join("oversized.txt");
        fs::write(&oversized, marker.repeat(200)).unwrap();
        let error = read_secret_file(&oversized, "test secret").unwrap_err();
        assert_eq!(error.to_string(), "test secret exceeds 4096 bytes");
        assert!(!error.to_string().contains(marker));

        let missing = directory.join("missing.txt");
        assert!(matches!(
            read_secret_file(&missing, "test secret"),
            Err(Error::FileIo { path, .. }) if path == missing
        ));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn password_file_precedes_stdin_and_a_disabled_vault_is_never_opened() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("sdsync-password-precedence-{nonce}.txt"));
        fs::write(&path, b"from-file\n").unwrap();
        let mut vault = VaultSession::new(false, "not a URL", "alice", false);

        let password = read_password_with_file(true, Some(&path), &mut vault).unwrap();
        assert_eq!(password.as_str(), "from-file");
        assert!(vault.load_password().unwrap().is_none());
        assert!(vault.generate_totp().unwrap().is_none());

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn invalid_vault_profile_fails_once_then_remains_disabled() {
        let mut vault = VaultSession::new(true, "not a URL", "alice", false);
        assert!(matches!(vault.load_password(), Err(Error::InvalidUrl(_))));
        assert!(vault.load_password().unwrap().is_none());
        assert!(vault.generate_totp().unwrap().is_none());

        let profile = ResolvedCredentialProfile {
            url: "still not a URL".to_owned(),
            username: "alice".to_owned(),
            allow_http: false,
        };
        assert!(matches!(
            open_credential_vault(&profile, true),
            Err(Error::InvalidUrl(_))
        ));
    }

    #[test]
    fn credential_setters_honor_explicit_files_without_consulting_stdin() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("sdsync-set-files-{nonce}"));
        fs::create_dir_all(&directory).unwrap();
        let password_path = directory.join("password.txt");
        let totp_path = directory.join("totp.txt");
        fs::write(&password_path, b"file-password\nignored").unwrap();
        fs::write(&totp_path, b"JBSWY3DPEHPK3PXP\nignored").unwrap();

        assert_eq!(
            read_new_password(true, Some(&password_path))
                .unwrap()
                .as_str(),
            "file-password"
        );
        assert_eq!(
            read_totp_provisioning(true, Some(&totp_path))
                .unwrap()
                .as_str(),
            "JBSWY3DPEHPK3PXP"
        );

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn absent_environment_secret_is_distinct_from_an_empty_secret() {
        let name = format!(
            "SDSYNC_TEST_MISSING_{}_{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        assert!(secret_from_env(&name).unwrap().is_none());
    }

    struct FakeLoginClient {
        replies: VecDeque<Result<()>>,
        otp_attempts: Vec<Option<String>>,
    }

    impl LoginClient for FakeLoginClient {
        fn login(&mut self, _username: &str, _password: &str, otp: Option<&str>) -> Result<()> {
            self.otp_attempts.push(otp.map(str::to_owned));
            self.replies.pop_front().expect("unexpected login attempt")
        }
    }

    #[test]
    fn totp_seed_is_used_only_after_dsm_challenges() {
        for challenge in [403, 406] {
            let mut client = FakeLoginClient {
                replies: VecDeque::from([Err(auth_error(challenge)), Ok(())]),
                otp_attempts: Vec::new(),
            };
            let mut vault_reads = 0;

            authenticate_with_otp(&mut client, "alice", "password", None, || {
                vault_reads += 1;
                Ok(Some(Zeroizing::new("123456".to_owned())))
            })
            .unwrap();

            assert_eq!(vault_reads, 1);
            assert_eq!(client.otp_attempts, [None, Some("123456".to_owned())]);
        }
    }

    #[test]
    fn explicit_otp_bypasses_the_vault() {
        let mut client = FakeLoginClient {
            replies: VecDeque::from([Ok(())]),
            otp_attempts: Vec::new(),
        };

        authenticate_with_otp(
            &mut client,
            "alice",
            "password",
            Some(Zeroizing::new("654321".to_owned())),
            || -> Result<Option<Zeroizing<String>>> {
                panic!("the vault must not be read when an explicit OTP is supplied")
            },
        )
        .unwrap();

        assert_eq!(client.otp_attempts, [Some("654321".to_owned())]);
    }

    #[test]
    fn rejected_explicit_otp_is_redacted_and_never_reads_the_vault() {
        let mut client = FakeLoginClient {
            replies: VecDeque::from([Err(auth_error(403))]),
            otp_attempts: Vec::new(),
        };
        let supplied = "654321";

        let error = authenticate_with_otp(
            &mut client,
            "alice",
            "password",
            Some(Zeroizing::new(supplied.to_owned())),
            || -> Result<Option<Zeroizing<String>>> {
                panic!("the vault must not be read when an explicit OTP is supplied")
            },
        )
        .unwrap_err();

        assert!(error.to_string().contains(OTP_ENV));
        assert!(!error.to_string().contains(supplied));
        assert_eq!(client.otp_attempts, [Some(supplied.to_owned())]);
    }

    #[test]
    fn unrelated_login_errors_are_returned_without_an_otp_retry() {
        let mut client = FakeLoginClient {
            replies: VecDeque::from([Err(Error::Cancelled)]),
            otp_attempts: Vec::new(),
        };

        let result = authenticate_with_otp(&mut client, "alice", "password", None, || {
            panic!("the vault must not be read for an unrelated login error")
        });

        assert!(matches!(result, Err(Error::Cancelled)));
        assert_eq!(client.otp_attempts, [None]);
    }

    #[test]
    fn vault_totp_refreshes_once_across_a_time_step_boundary() {
        let mut client = FakeLoginClient {
            replies: VecDeque::from([Err(auth_error(403)), Err(auth_error(404)), Ok(())]),
            otp_attempts: Vec::new(),
        };
        let mut codes = VecDeque::from(["111111", "222222"]);

        authenticate_with_otp(&mut client, "alice", "password", None, || {
            Ok(codes
                .pop_front()
                .map(|code| Zeroizing::new(code.to_owned())))
        })
        .unwrap();

        assert!(codes.is_empty());
        assert_eq!(
            client.otp_attempts,
            [None, Some("111111".to_owned()), Some("222222".to_owned())]
        );
    }

    #[test]
    fn identical_totp_is_not_treated_as_a_refresh() {
        let mut current = Some(Zeroizing::new("111111".to_owned()));
        assert!(!replace_if_changed(
            &mut current,
            Zeroizing::new("111111".to_owned())
        ));
        assert!(replace_if_changed(
            &mut current,
            Zeroizing::new("222222".to_owned())
        ));
        assert_eq!(current.as_deref().map(String::as_str), Some("222222"));
    }

    #[test]
    fn otp_validation_requires_exact_ascii_digits_without_echoing_the_code() {
        assert!(validate_otp_code("012345", "test source").is_ok());
        for invalid in ["12345", "1234567", "12345x", "１２３４５６"] {
            let error = validate_otp_code(invalid, "test source").unwrap_err();
            assert_eq!(
                error.to_string(),
                "test source must contain exactly 6 ASCII digits"
            );
            assert!(!error.to_string().contains(invalid));
        }
    }

    #[test]
    fn missing_or_unavailable_vault_totp_uses_a_terminal_prompt_only_when_safe() {
        for vault_error in [false, true] {
            let mut client = FakeLoginClient {
                replies: VecDeque::from([Err(auth_error(403)), Ok(())]),
                otp_attempts: Vec::new(),
            };
            let mut prompted = 0;
            authenticate_with_otp_interaction(
                &mut client,
                "alice",
                "password",
                None,
                || {
                    if vault_error {
                        Err(Error::Vault {
                            operation: "TOTP lookup",
                            reason: "vault unavailable",
                        })
                    } else {
                        Ok(None)
                    }
                },
                || true,
                || {
                    prompted += 1;
                    Ok(Zeroizing::new("123456".to_owned()))
                },
            )
            .unwrap();
            assert_eq!(prompted, 1);
            assert_eq!(client.otp_attempts, [None, Some("123456".to_owned())]);
        }

        let mut client = FakeLoginClient {
            replies: VecDeque::from([Err(auth_error(406))]),
            otp_attempts: Vec::new(),
        };
        let error = authenticate_with_otp_interaction(
            &mut client,
            "alice",
            "password",
            None,
            || {
                Err(Error::Vault {
                    operation: "TOTP lookup",
                    reason: "vault unavailable",
                })
            },
            || false,
            || panic!("non-interactive authentication must not prompt"),
        )
        .unwrap_err();
        assert!(error.to_string().contains(OTP_ENV));
        assert_eq!(client.otp_attempts, [None]);

        let mut client = FakeLoginClient {
            replies: VecDeque::from([Err(auth_error(403))]),
            otp_attempts: Vec::new(),
        };
        let error = authenticate_with_otp_interaction(
            &mut client,
            "alice",
            "password",
            None,
            || Ok(None),
            || false,
            || panic!("non-interactive authentication must not prompt"),
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "DSM requires a TOTP code; set SDSYNC_OTP for non-interactive runs or run from a terminal"
        );
        assert_eq!(client.otp_attempts, [None]);
    }

    #[test]
    fn rejected_generated_totp_has_deterministic_refresh_and_prompt_fallbacks() {
        for refresh_error in [false, true] {
            let mut client = FakeLoginClient {
                replies: VecDeque::from([Err(auth_error(403)), Err(auth_error(404)), Ok(())]),
                otp_attempts: Vec::new(),
            };
            let mut reads = 0;
            authenticate_with_otp_interaction(
                &mut client,
                "alice",
                "password",
                None,
                || {
                    reads += 1;
                    if reads == 1 {
                        Ok(Some(Zeroizing::new("111111".to_owned())))
                    } else if refresh_error {
                        Err(Error::Vault {
                            operation: "TOTP lookup",
                            reason: "vault unavailable",
                        })
                    } else {
                        Ok(None)
                    }
                },
                || true,
                || Ok(Zeroizing::new("222222".to_owned())),
            )
            .unwrap();
            assert_eq!(reads, 2);
            assert_eq!(
                client.otp_attempts,
                [None, Some("111111".to_owned()), Some("222222".to_owned())]
            );
        }

        for refresh_error in [false, true] {
            let mut client = FakeLoginClient {
                replies: VecDeque::from([Err(auth_error(403)), Err(auth_error(404))]),
                otp_attempts: Vec::new(),
            };
            let mut reads = 0;
            let error = authenticate_with_otp_interaction(
                &mut client,
                "alice",
                "password",
                None,
                || {
                    reads += 1;
                    if reads == 1 {
                        Ok(Some(Zeroizing::new("111111".to_owned())))
                    } else if refresh_error {
                        Err(Error::Vault {
                            operation: "TOTP lookup",
                            reason: "vault unavailable",
                        })
                    } else {
                        Ok(None)
                    }
                },
                || false,
                || panic!("non-interactive authentication must not prompt"),
            )
            .unwrap_err();
            assert!(error.to_string().contains("clock") || error.to_string().contains("refreshed"));
            assert_eq!(reads, 2);
        }
    }

    #[test]
    fn rejected_explicit_or_prompted_codes_do_not_loop_indefinitely() {
        let mut client = FakeLoginClient {
            replies: VecDeque::from([Err(auth_error(404)), Ok(())]),
            otp_attempts: Vec::new(),
        };
        authenticate_with_otp_interaction(
            &mut client,
            "alice",
            "password",
            Some(Zeroizing::new("111111".to_owned())),
            || panic!("an explicit code must not consult the vault"),
            || true,
            || Ok(Zeroizing::new("222222".to_owned())),
        )
        .unwrap();
        assert_eq!(
            client.otp_attempts,
            [Some("111111".to_owned()), Some("222222".to_owned())]
        );

        let mut client = FakeLoginClient {
            replies: VecDeque::from([Err(auth_error(403)), Err(auth_error(403))]),
            otp_attempts: Vec::new(),
        };
        let error = authenticate_with_otp_interaction(
            &mut client,
            "alice",
            "password",
            None,
            || Ok(None),
            || true,
            || Ok(Zeroizing::new("333333".to_owned())),
        )
        .unwrap_err();
        assert!(error.to_string().contains("requires a TOTP code"));
        assert_eq!(client.otp_attempts, [None, Some("333333".to_owned())]);

        let mut client = FakeLoginClient {
            replies: VecDeque::from([Err(auth_error(404))]),
            otp_attempts: Vec::new(),
        };
        let error = authenticate_with_otp_interaction(
            &mut client,
            "alice",
            "password",
            Some(Zeroizing::new("444444".to_owned())),
            || panic!("an explicit code must not consult the vault"),
            || false,
            || panic!("non-interactive authentication must not prompt"),
        )
        .unwrap_err();
        assert!(error.to_string().contains(OTP_ENV));
    }

    #[test]
    fn a_second_rejected_generated_code_stops_after_one_refresh() {
        let mut client = FakeLoginClient {
            replies: VecDeque::from([
                Err(auth_error(403)),
                Err(auth_error(404)),
                Err(auth_error(404)),
            ]),
            otp_attempts: Vec::new(),
        };
        let mut codes = VecDeque::from(["111111", "222222"]);
        let error = authenticate_with_otp_interaction(
            &mut client,
            "alice",
            "password",
            None,
            || {
                Ok(codes
                    .pop_front()
                    .map(|code| Zeroizing::new(code.to_owned())))
            },
            || false,
            || panic!("non-interactive authentication must not prompt"),
        )
        .unwrap_err();
        assert!(error.to_string().contains("clock"));
        assert!(codes.is_empty());
        assert_eq!(client.otp_attempts.len(), 3);
    }

    #[test]
    fn generated_totp_rejection_message_is_actionable_and_secret_free() {
        let message = generated_totp_rejected().to_string();
        assert!(message.contains("synchronize the client and NAS clocks"));
        assert!(message.contains(OTP_ENV));
        assert!(!message.contains("123456"));
    }

    fn auth_error(code: i64) -> Error {
        Error::Api {
            api: "SYNO.API.Auth".to_owned(),
            operation: "login".to_owned(),
            code,
            description: String::new(),
            details: Vec::new(),
        }
    }
}
