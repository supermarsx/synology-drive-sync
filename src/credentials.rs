use std::env;
use std::io::{self, BufRead, IsTerminal, Read};

use clap::{Args, Subcommand, ValueEnum};
use synology_drive_sync::api::ApiClient;
use synology_drive_sync::vault::{CredentialKind, OsVault, generate_totp, parse_totp_secret};
use synology_drive_sync::{Error, Result};
use zeroize::Zeroizing;

const PASSWORD_ENV: &str = "SDSYNC_PASSWORD";
const OTP_ENV: &str = "SDSYNC_OTP";
const MAX_STDIN_SECRET_BYTES: u64 = 4098;

#[derive(Debug, Args)]
pub(crate) struct CredentialsArgs {
    #[command(subcommand)]
    action: CredentialAction,
}

#[derive(Debug, Args)]
struct CredentialProfileArgs {
    /// Reverse-proxy base URL that owns this credential profile.
    #[arg(long, env = "SDSYNC_URL")]
    url: String,

    /// DSM account that owns this credential profile.
    #[arg(long, env = "SDSYNC_USERNAME")]
    username: String,

    /// Permit an HTTP profile. Intended only for trusted testing/LAN use.
    #[arg(long)]
    allow_http: bool,
}

#[derive(Debug, Args)]
struct SetPasswordArgs {
    #[command(flatten)]
    profile: CredentialProfileArgs,

    /// Read the password from the first line of standard input.
    #[arg(long)]
    password_stdin: bool,
}

#[derive(Debug, Args)]
struct SetTotpArgs {
    #[command(flatten)]
    profile: CredentialProfileArgs,

    /// Read the TOTP seed/URI from the first line of standard input.
    #[arg(long)]
    secret_stdin: bool,
}

#[derive(Debug, Args)]
struct StatusArgs {
    #[command(flatten)]
    profile: CredentialProfileArgs,
}

#[derive(Debug, Args)]
struct RemoveArgs {
    #[command(flatten)]
    profile: CredentialProfileArgs,

    #[arg(value_enum)]
    kind: RemoveKind,
}

#[derive(Debug, Subcommand)]
enum CredentialAction {
    /// Store or replace the DSM password using masked input.
    SetPassword(SetPasswordArgs),
    /// Import DSM's existing TOTP manual key or provisioning URI.
    SetTotp(SetTotpArgs),
    /// Report only whether each vault entry exists.
    Status(StatusArgs),
    /// Remove one or both entries from the OS vault.
    Remove(RemoveArgs),
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum RemoveKind {
    Password,
    Totp,
    All,
}

pub(crate) fn run(credentials: CredentialsArgs) -> Result<()> {
    match credentials.action {
        CredentialAction::SetPassword(arguments) => {
            let vault = open_credential_vault(&arguments.profile)?;
            let password = read_new_password(arguments.password_stdin)?;
            vault.store_password(&password)?;
            eprintln!("Stored the DSM password in the OS credential vault.");
        }
        CredentialAction::SetTotp(arguments) => {
            let vault = open_credential_vault(&arguments.profile)?;
            eprintln!(
                "warning: storing the TOTP seed enables unattended login but places both factors under the same OS account"
            );
            let provisioning = read_totp_provisioning(arguments.secret_stdin)?;
            let secret = parse_totp_secret(&provisioning)?;
            drop(provisioning);
            vault.store_totp_secret(&secret)?;
            eprintln!("Stored the DSM TOTP seed in the OS credential vault.");
        }
        CredentialAction::Status(arguments) => {
            let vault = open_credential_vault(&arguments.profile)?;
            let status = vault.status()?;
            eprintln!(
                "Password: {}",
                if status.password {
                    "stored"
                } else {
                    "not stored"
                }
            );
            eprintln!(
                "TOTP seed: {}",
                if status.totp { "stored" } else { "not stored" }
            );
        }
        CredentialAction::Remove(arguments) => {
            let vault = open_credential_vault(&arguments.profile)?;
            match arguments.kind {
                RemoveKind::Password => {
                    print_removal("Password", vault.remove(CredentialKind::Password)?)
                }
                RemoveKind::Totp => print_removal("TOTP seed", vault.remove(CredentialKind::Totp)?),
                RemoveKind::All => {
                    print_removal("Password", vault.remove(CredentialKind::Password)?);
                    print_removal("TOTP seed", vault.remove(CredentialKind::Totp)?);
                }
            }
        }
    }
    Ok(())
}

fn open_credential_vault(profile: &CredentialProfileArgs) -> Result<OsVault> {
    if profile.allow_http {
        eprintln!(
            "warning: this vault profile permits HTTP; credentials may be exposed during sync"
        );
    }
    OsVault::new(&profile.url, &profile.username, profile.allow_http)
}

fn print_removal(label: &str, removed: bool) {
    eprintln!(
        "{label}: {}.",
        if removed { "removed" } else { "not stored" }
    );
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

pub(crate) fn authenticate(
    client: &mut ApiClient,
    username: &str,
    password: &str,
    vault: &mut VaultSession<'_>,
) -> Result<()> {
    let otp = secret_from_env(OTP_ENV)?;
    if let Some(code) = &otp {
        validate_otp_code(code, OTP_ENV)?;
    }
    authenticate_with_otp(client, username, password, otp, || vault.generate_totp())
}

fn authenticate_with_otp<C, F>(
    client: &mut C,
    username: &str,
    password: &str,
    mut otp: Option<Zeroizing<String>>,
    mut vault_totp: F,
) -> Result<()>
where
    C: LoginClient,
    F: FnMut() -> Result<Option<Zeroizing<String>>>,
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
                    Ok(None) => {
                        otp = Some(prompt_otp()?);
                        prompted = true;
                    }
                    Err(vault_error) if io::stdin().is_terminal() => {
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
                match (refresh_error, io::stdin().is_terminal()) {
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
            Err(error)
                if error.api_code() == Some(404) && !prompted && io::stdin().is_terminal() =>
            {
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

pub(crate) fn read_password(
    from_stdin: bool,
    vault: &mut VaultSession<'_>,
) -> Result<Zeroizing<String>> {
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

fn read_new_password(from_stdin: bool) -> Result<Zeroizing<String>> {
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

fn read_totp_provisioning(from_stdin: bool) -> Result<Zeroizing<String>> {
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

fn prompt_secret(prompt: &str, label: &str) -> Result<Zeroizing<String>> {
    let value = rpassword::prompt_password(prompt)
        .map(Zeroizing::new)
        .map_err(|error| Error::Message(format!("failed to read {label}: {error}")))?;
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

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;

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
