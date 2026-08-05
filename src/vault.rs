#[cfg(target_os = "windows")]
use std::collections::HashMap;
use std::sync::Arc;

use keyring_core::{CredentialStore, Entry, Error as KeyringError};
use sha2::{Digest, Sha256};
use totp_rs::{Algorithm, Secret, TOTP};
use zeroize::{Zeroize, Zeroizing};

use crate::api::normalize_base_url;
use crate::{Error, Result};

const SERVICE: &str = "io.github.supermarsx.synology-drive-sync";
const PROFILE_DOMAIN: &[u8] = b"synology-drive-sync vault profile v1\0";
const MIN_SECRET_BYTES: usize = 10;
const MAX_SECRET_BYTES: usize = 128;
const MAX_PROVISIONING_BYTES: usize = 4096;

/// Two-factor material kept independently so either credential can be rotated.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CredentialKind {
    Password,
    Totp,
}

/// Presence-only information. Credential values are never exposed by status output.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VaultStatus {
    pub password: bool,
    pub totp: bool,
}

/// An OS-vault profile scoped to one normalized reverse-proxy URL and DSM username.
///
/// The object retains only opaque entry names, not the URL or username.
pub struct OsVault {
    store: Arc<CredentialStore>,
    #[cfg(target_os = "windows")]
    use_platform_modifiers: bool,
    password_account: String,
    totp_account: String,
}

impl OsVault {
    pub fn new(base_url: &str, username: &str, allow_http: bool) -> Result<Self> {
        if username.is_empty() {
            return Err(Error::Message("DSM username must not be empty".to_owned()));
        }
        let base_url = normalize_base_url(base_url, allow_http)?;
        let (password_account, totp_account) = profile_accounts(base_url.as_str(), username);
        Ok(Self {
            store: open_platform_store().map_err(|error| vault_error("initialization", error))?,
            #[cfg(target_os = "windows")]
            use_platform_modifiers: true,
            password_account,
            totp_account,
        })
    }

    pub fn load_password(&self) -> Result<Option<Zeroizing<String>>> {
        let entry = self.entry(CredentialKind::Password, "initialization")?;
        match entry.get_password() {
            Ok(password) => Ok(Some(Zeroizing::new(password))),
            Err(KeyringError::NoEntry) => Ok(None),
            Err(error) => Err(vault_error("password lookup", error)),
        }
    }

    pub fn store_password(&self, password: &str) -> Result<()> {
        if password.is_empty() {
            return Err(Error::Message("DSM password must not be empty".to_owned()));
        }
        self.entry(CredentialKind::Password, "initialization")?
            .set_password(password)
            .map_err(|error| vault_error("password storage", error))
    }

    pub fn load_totp_secret(&self) -> Result<Option<Zeroizing<Vec<u8>>>> {
        let entry = self.entry(CredentialKind::Totp, "initialization")?;
        match entry.get_password() {
            Ok(encoded) => {
                let encoded = Zeroizing::new(encoded);
                let secret = decode_base32(&encoded).map_err(|_| Error::Vault {
                    operation: "TOTP seed lookup",
                    reason: "the credential vault returned malformed TOTP seed data",
                })?;
                validate_secret_length(secret.len())?;
                Ok(Some(secret))
            }
            Err(KeyringError::NoEntry) => Ok(None),
            Err(error) => Err(vault_error("TOTP seed lookup", error)),
        }
    }

    pub fn store_totp_secret(&self, secret: &[u8]) -> Result<()> {
        validate_secret_length(secret.len())?;
        let encoded = Zeroizing::new(synology_totp(secret).get_secret_base32());
        self.entry(CredentialKind::Totp, "initialization")?
            .set_password(&encoded)
            .map_err(|error| vault_error("TOTP seed storage", error))
    }

    pub fn status(&self) -> Result<VaultStatus> {
        let password = self.load_password()?.is_some();
        let totp = self.load_totp_secret()?.is_some();
        Ok(VaultStatus { password, totp })
    }

    pub fn remove(&self, kind: CredentialKind) -> Result<bool> {
        let (entry, operation) = match kind {
            CredentialKind::Password => (self.entry(kind, "initialization")?, "password removal"),
            CredentialKind::Totp => (self.entry(kind, "initialization")?, "TOTP seed removal"),
        };
        match entry.delete_credential() {
            Ok(()) => Ok(true),
            Err(KeyringError::NoEntry) => Ok(false),
            Err(error) => Err(vault_error(operation, error)),
        }
    }

    fn entry(&self, kind: CredentialKind, operation: &'static str) -> Result<Entry> {
        let account = match kind {
            CredentialKind::Password => &self.password_account,
            CredentialKind::Totp => &self.totp_account,
        };
        #[cfg(target_os = "windows")]
        let entry = if self.use_platform_modifiers {
            let modifiers = HashMap::from([("persistence", "Local")]);
            self.store.build(SERVICE, account, Some(&modifiers))
        } else {
            self.store.build(SERVICE, account, None)
        };
        #[cfg(not(target_os = "windows"))]
        let entry = self.store.build(SERVICE, account, None);
        entry.map_err(|error| vault_error(operation, error))
    }

    #[cfg(test)]
    fn with_store(
        base_url: &str,
        username: &str,
        allow_http: bool,
        store: Arc<CredentialStore>,
    ) -> Result<Self> {
        let base_url = normalize_base_url(base_url, allow_http)?;
        let (password_account, totp_account) = profile_accounts(base_url.as_str(), username);
        Ok(Self {
            store,
            #[cfg(target_os = "windows")]
            use_platform_modifiers: false,
            password_account,
            totp_account,
        })
    }
}

/// Decode a DSM manual Base32 key or a standard `otpauth://totp` provisioning URI.
/// Parser failures are deliberately replaced with fixed text because upstream errors can
/// contain the submitted seed.
pub fn parse_totp_secret(input: &str) -> Result<Zeroizing<Vec<u8>>> {
    if input.len() > MAX_PROVISIONING_BYTES {
        return Err(invalid_totp());
    }
    let input = input.trim();
    let secret = if input
        .get(..15)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("otpauth://totp/"))
    {
        parse_totp_uri(input)?
    } else {
        decode_base32(input)?
    };
    validate_secret_length(secret.len())?;
    Ok(secret)
}

/// Generate the current six-digit DSM code. The returned string is erased on drop.
pub fn generate_totp(secret: &[u8]) -> Result<Zeroizing<String>> {
    validate_secret_length(secret.len())?;
    let totp = synology_totp(secret);
    let code = totp.generate_current().map_err(|_| {
        Error::Message("cannot generate TOTP because the system clock is invalid".to_owned())
    })?;
    Ok(Zeroizing::new(code))
}

fn synology_totp(secret: &[u8]) -> TOTP {
    TOTP::new_unchecked(Algorithm::SHA1, 6, 1, 30, secret.to_vec())
}

fn parse_totp_uri(input: &str) -> Result<Zeroizing<Vec<u8>>> {
    if input.contains('#') {
        return Err(invalid_totp());
    }
    let (_, query) = input.split_once('?').ok_or_else(invalid_totp)?;
    let mut encoded = None;
    let mut algorithm_seen = false;
    let mut digits_seen = false;
    let mut period_seen = false;

    for parameter in query.split('&') {
        let Some((key, value)) = parameter.split_once('=') else {
            return Err(invalid_totp());
        };
        match key {
            "secret" if encoded.is_none() => encoded = Some(percent_decode(value)?),
            "secret" => return Err(invalid_totp()),
            "algorithm" if !algorithm_seen => {
                algorithm_seen = true;
                if !value.eq_ignore_ascii_case("SHA1") {
                    return Err(incompatible_totp_uri());
                }
            }
            "algorithm" => return Err(invalid_totp()),
            "digits" if !digits_seen => {
                digits_seen = true;
                if value != "6" {
                    return Err(incompatible_totp_uri());
                }
            }
            "digits" => return Err(invalid_totp()),
            "period" if !period_seen => {
                period_seen = true;
                if value != "30" {
                    return Err(incompatible_totp_uri());
                }
            }
            "period" => return Err(invalid_totp()),
            _ => {}
        }
    }

    let encoded = encoded.ok_or_else(invalid_totp)?;
    decode_base32(&encoded)
}

fn percent_decode(value: &str) -> Result<Zeroizing<String>> {
    let bytes = value.as_bytes();
    let mut decoded = Zeroizing::new(String::with_capacity(bytes.len()));
    let mut index = 0;
    while index < bytes.len() {
        let byte = if bytes[index] == b'%' {
            if index + 2 >= bytes.len() {
                return Err(invalid_totp());
            }
            let high = hex_value(bytes[index + 1]).ok_or_else(invalid_totp)?;
            let low = hex_value(bytes[index + 2]).ok_or_else(invalid_totp)?;
            index += 3;
            (high << 4) | low
        } else {
            let byte = bytes[index];
            index += 1;
            byte
        };
        if !byte.is_ascii() {
            return Err(invalid_totp());
        }
        decoded.push(char::from(byte));
    }
    Ok(decoded)
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn decode_base32(input: &str) -> Result<Zeroizing<Vec<u8>>> {
    let mut encoded = Zeroizing::new(String::with_capacity(input.len()));
    for character in input.chars() {
        if character.is_ascii_whitespace() || character == '-' {
            continue;
        }
        encoded.push(character.to_ascii_uppercase());
    }
    while encoded.ends_with('=') {
        encoded.pop();
    }
    let secret = Secret::Encoded(encoded.to_string());
    Ok(Zeroizing::new(
        secret.to_bytes().map_err(|_| invalid_totp())?,
    ))
}

fn profile_fingerprint(base_url: &str, username: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(PROFILE_DOMAIN);
    digest.update((base_url.len() as u64).to_be_bytes());
    digest.update(base_url.as_bytes());
    digest.update((username.len() as u64).to_be_bytes());
    digest.update(username.as_bytes());
    let digest = digest.finalize();
    let mut encoded = String::with_capacity(32);
    for byte in &digest[..16] {
        use std::fmt::Write as _;
        write!(&mut encoded, "{byte:02x}").expect("writing to String cannot fail");
    }
    encoded
}

fn profile_accounts(base_url: &str, username: &str) -> (String, String) {
    let fingerprint = profile_fingerprint(base_url, username);
    (
        format!("v1:password:{fingerprint}"),
        format!("v1:totp:{fingerprint}"),
    )
}

fn validate_secret_length(length: usize) -> Result<()> {
    if !(MIN_SECRET_BYTES..=MAX_SECRET_BYTES).contains(&length) {
        return Err(Error::Message(
            "TOTP seed must decode to between 10 and 128 bytes".to_owned(),
        ));
    }
    Ok(())
}

fn invalid_totp() -> Error {
    Error::Message(
        "invalid TOTP seed; enter DSM's Base32 manual key or an otpauth://totp URI".to_owned(),
    )
}

fn incompatible_totp_uri() -> Error {
    Error::Message("TOTP URI must use SHA1, 6 digits, and a 30-second period for DSM".to_owned())
}

fn vault_error(operation: &'static str, error: KeyringError) -> Error {
    let reason = match error {
        KeyringError::NoStorageAccess(_) => "the credential vault is locked or unavailable",
        KeyringError::NoDefaultStore => "no supported credential vault is available",
        KeyringError::NotSupportedByStore(_) => {
            "the credential vault does not support this operation"
        }
        KeyringError::TooLong(_, _) => "the credential vault rejected the entry size",
        KeyringError::BadEncoding(mut bytes) => {
            bytes.zeroize();
            "the credential vault returned malformed secret data"
        }
        KeyringError::BadDataFormat(mut bytes, _) => {
            bytes.zeroize();
            "the credential vault returned malformed secret data"
        }
        KeyringError::Ambiguous(_) => "the credential vault contains duplicate entries",
        _ => "the credential vault is unavailable",
    };
    Error::Vault { operation, reason }
}

#[cfg(target_os = "windows")]
fn open_platform_store() -> std::result::Result<Arc<CredentialStore>, KeyringError> {
    let store: Arc<CredentialStore> = windows_native_keyring_store::Store::new()?;
    Ok(store)
}

#[cfg(target_os = "macos")]
fn open_platform_store() -> std::result::Result<Arc<CredentialStore>, KeyringError> {
    let store: Arc<CredentialStore> = apple_native_keyring_store::keychain::Store::new()?;
    Ok(store)
}

#[cfg(target_os = "linux")]
fn open_platform_store() -> std::result::Result<Arc<CredentialStore>, KeyringError> {
    let store: Arc<CredentialStore> = zbus_secret_service_keyring_store::Store::new()?;
    Ok(store)
}

#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
fn open_platform_store() -> std::result::Result<Arc<CredentialStore>, KeyringError> {
    Err(KeyringError::NotSupportedByStore(
        "OS credential vault support is available only on Windows, macOS, and Linux".to_owned(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const RFC_SECRET: &str = "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ";

    #[test]
    fn profile_is_normalized_scoped_and_domain_separated() {
        let normalized = normalize_base_url("https://files.example.test/nas", false).unwrap();
        let slash = normalize_base_url("https://files.example.test/nas/", false).unwrap();
        let one = profile_accounts(normalized.as_str(), "alice");
        let slash = profile_accounts(slash.as_str(), "alice");
        let other_user = profile_accounts(normalized.as_str(), "Alice");
        let other_prefix = profile_accounts("https://files.example.test/other/", "alice");

        assert_eq!(one, slash);
        assert_ne!(one.0, one.1);
        assert_ne!(one.0, other_user.0);
        assert_ne!(one.0, other_prefix.0);
        assert!(!one.0.contains("alice"));
        assert!(!one.0.contains("files.example"));
    }

    #[test]
    fn mock_store_round_trip_and_independent_removal() {
        let store: Arc<CredentialStore> = keyring_core::mock::Store::new().unwrap();
        let vault = OsVault::with_store(
            "https://files.example.test/nas",
            "alice",
            false,
            store.clone(),
        )
        .unwrap();
        assert_eq!(
            vault.status().unwrap(),
            VaultStatus {
                password: false,
                totp: false
            }
        );

        let secret = parse_totp_secret("jbsw-y3dp ehpk-3pxp").unwrap();
        vault.store_password("test-password").unwrap();
        vault.store_totp_secret(&secret).unwrap();
        assert_eq!(
            vault.load_password().unwrap().unwrap().as_str(),
            "test-password"
        );
        assert_eq!(&*vault.load_totp_secret().unwrap().unwrap(), &*secret);
        assert_eq!(
            store
                .build(SERVICE, &vault.totp_account, None)
                .unwrap()
                .get_password()
                .unwrap(),
            "JBSWY3DPEHPK3PXP"
        );
        assert_eq!(
            vault.status().unwrap(),
            VaultStatus {
                password: true,
                totp: true
            }
        );

        assert!(vault.remove(CredentialKind::Password).unwrap());
        assert!(!vault.remove(CredentialKind::Password).unwrap());
        assert!(vault.load_totp_secret().unwrap().is_some());
        assert!(vault.remove(CredentialKind::Totp).unwrap());
        assert_eq!(
            vault.status().unwrap(),
            VaultStatus {
                password: false,
                totp: false
            }
        );
    }

    #[test]
    fn parses_grouped_base32_and_uri_equally() {
        let raw = parse_totp_secret("jbsw-y3dp ehpk-3pxp").unwrap();
        let uri = parse_totp_secret(
            "otpauth://totp/Synology:alice?secret=JBSWY3DPEHPK3PXP&issuer=Synology",
        )
        .unwrap();
        assert_eq!(&*raw, &*uri);
        assert_eq!(raw.len(), 10);

        let encoded_uri = parse_totp_secret(
            "otpauth://totp/alice?secret=%4a%42%53%57%59%33%44%50%45%48%50%4b%33%50%58%50&digits=6&period=30&algorithm=SHA1",
        )
        .unwrap();
        assert_eq!(&*raw, &*encoded_uri);
    }

    #[test]
    fn generates_rfc_6238_sha1_vectors() {
        let secret = parse_totp_secret(RFC_SECRET).unwrap();
        let totp = synology_totp(&secret);
        assert_eq!(totp.generate(59), "287082");
        assert_eq!(totp.generate(1_111_111_109), "081804");
        assert_eq!(totp.generate(1_234_567_890), "005924");
        assert_eq!(totp.generate(20_000_000_000), "353130");
    }

    #[test]
    fn rejects_non_synology_uri_parameters() {
        for uri in [
            "otpauth://totp/Synology:alice?secret=GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ&algorithm=SHA256",
            "otpauth://totp/Synology:alice?secret=GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ&digits=8",
            "otpauth://totp/Synology:alice?secret=GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ&period=60",
        ] {
            assert!(parse_totp_secret(uri).is_err());
        }
    }

    #[test]
    fn malformed_input_errors_never_echo_the_secret() {
        let marker = "LEAK-ME-NOT-123";
        for input in [
            marker.to_owned(),
            format!("otpauth:///{marker}?secret={marker}"),
            format!("otpauth://totp/alice?secret={marker}"),
        ] {
            let error = parse_totp_secret(&input).unwrap_err();
            assert!(!error.to_string().contains(marker));
            assert!(!format!("{error:?}").contains(marker));
        }
    }

    #[test]
    fn rejects_missing_short_and_oversized_seeds() {
        for input in [
            "",
            "MY======",
            "otpauth://totp/alice",
            "A".repeat(5000).as_str(),
        ] {
            assert!(parse_totp_secret(input).is_err());
        }
    }

    #[test]
    fn current_totp_is_six_ascii_digits_and_rejects_short_seeds() {
        let secret = parse_totp_secret(RFC_SECRET).unwrap();
        let code = generate_totp(&secret).unwrap();
        assert_eq!(code.len(), 6);
        assert!(code.bytes().all(|byte| byte.is_ascii_digit()));
        assert!(generate_totp(&[0_u8; 9]).is_err());
    }

    #[test]
    fn provisioning_uri_rejects_ambiguous_and_malformed_parameters() {
        let secret = RFC_SECRET;
        for uri in [
            format!("otpauth://totp/alice?secret={secret}#fragment"),
            "otpauth://totp/alice?issuer".to_owned(),
            format!("otpauth://totp/alice?secret={secret}&secret={secret}"),
            format!("otpauth://totp/alice?secret={secret}&algorithm=SHA1&algorithm=SHA1"),
            format!("otpauth://totp/alice?secret={secret}&digits=6&digits=6"),
            format!("otpauth://totp/alice?secret={secret}&period=30&period=30"),
            "otpauth://totp/alice?issuer=Synology".to_owned(),
            "otpauth://totp/alice?secret=%".to_owned(),
            "otpauth://totp/alice?secret=%GG".to_owned(),
            "otpauth://totp/alice?secret=%FF".to_owned(),
        ] {
            assert!(parse_totp_secret(&uri).is_err(), "accepted {uri}");
        }
        let upper_hex = parse_totp_secret(
            "otpauth://totp/alice?secret=%4A%42%53%57%59%33%44%50%45%48%50%4B%33%50%58%50",
        )
        .unwrap();
        assert_eq!(
            &*upper_hex,
            &*parse_totp_secret("JBSWY3DPEHPK3PXP").unwrap()
        );
    }

    #[test]
    fn vault_rejects_empty_passwords_and_malformed_stored_totp_data() {
        let store: Arc<CredentialStore> = keyring_core::mock::Store::new().unwrap();
        let vault =
            OsVault::with_store("https://files.example.test", "alice", false, store).unwrap();
        assert_eq!(
            vault.store_password("").unwrap_err().to_string(),
            "DSM password must not be empty"
        );
        assert!(vault.store_totp_secret(&[0_u8; 9]).is_err());
        vault
            .entry(CredentialKind::Totp, "test")
            .unwrap()
            .set_password("NOT-BASE32!")
            .unwrap();
        let error = vault.load_totp_secret().unwrap_err();
        assert!(matches!(
            error,
            Error::Vault {
                operation: "TOTP seed lookup",
                ..
            }
        ));
        assert!(!error.to_string().contains("NOT-BASE32"));
    }

    #[test]
    fn keyring_failures_map_to_stable_secret_free_reasons() {
        let platform = || -> keyring_core::error::PlatformError {
            Box::new(std::io::Error::other("sensitive platform detail"))
        };
        let cases = vec![
            (
                KeyringError::NoStorageAccess(platform()),
                "locked or unavailable",
            ),
            (
                KeyringError::NoDefaultStore,
                "no supported credential vault",
            ),
            (
                KeyringError::NotSupportedByStore("detail".to_owned()),
                "does not support",
            ),
            (
                KeyringError::TooLong("secret".to_owned(), 1),
                "rejected the entry size",
            ),
            (
                KeyringError::BadEncoding(b"LEAK-ME".to_vec()),
                "malformed secret data",
            ),
            (
                KeyringError::BadDataFormat(b"LEAK-ME".to_vec(), platform()),
                "malformed secret data",
            ),
            (KeyringError::Ambiguous(Vec::new()), "duplicate entries"),
            (
                KeyringError::PlatformFailure(platform()),
                "vault is unavailable",
            ),
        ];
        for (source, expected) in cases {
            let rendered = vault_error("test operation", source).to_string();
            assert!(rendered.contains(expected), "{rendered}");
            assert!(!rendered.contains("sensitive platform detail"));
            assert!(!rendered.contains("LEAK-ME"));
            assert!(!rendered.contains("detail"));
        }
    }

    #[test]
    fn vault_profile_rejects_empty_user_before_platform_access() {
        let error = match OsVault::new("https://files.example.test", "", false) {
            Err(error) => error,
            Ok(_) => panic!("an empty username must not create a vault profile"),
        };
        assert_eq!(error.to_string(), "DSM username must not be empty");
    }
}
