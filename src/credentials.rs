use std::env;
use std::fs::File;
use std::io::{self, BufRead, IsTerminal, Read};
use std::path::Path;

use synology_drive_sync::api::ApiClient;
use synology_drive_sync::vault::{CredentialKind, OsVault, generate_totp, parse_totp_secret};
use synology_drive_sync::{Error, Result};
use zeroize::Zeroizing;

use crate::cli::{CredentialAction, CredentialsArgs, OTP_ENV, PASSWORD_ENV, RemoveKind};
use crate::config::{Profile, ResolvedCredentialProfile};

const MAX_STDIN_SECRET_BYTES: u64 = 4098;

pub(crate) fn run(
    credentials: &CredentialsArgs,
    resolved: &ResolvedCredentialProfile,
    selected_profile: Option<&Profile>,
    quiet: bool,
) -> Result<CredentialOutcome> {
    match &credentials.action {
        CredentialAction::SetPassword(arguments) => {
            let vault = open_credential_vault(resolved, quiet)?;
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
            let vault = open_credential_vault(resolved, quiet)?;
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
            let vault = open_credential_vault(resolved, quiet)?;
            let status = vault.status()?;
            Ok(CredentialOutcome::Status {
                password_stored: status.password,
                totp_stored: status.totp,
            })
        }
        CredentialAction::Remove(arguments) => {
            let vault = open_credential_vault(resolved, quiet)?;
            match arguments.kind {
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
            }
        }
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
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

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
