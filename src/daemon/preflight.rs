//! The checks `sdsync-run` made before the core started, for daemon runs:
//! the security policy's gates and each profile's secret contract. A run they
//! refuse never starts, and the refusal names the gate (`policy_destructive`,
//! `policy_http`, `policy_invalid_tls`, `policy_remote_logging`,
//! `policy_empty_source`, `password_missing`, `secret_unsafe`,
//! `policy_unreadable`) and the profile, in the runner's own words; the
//! dashboard already knows every one of these reasons.
//!
//! Without them the daemon would have run what the operator's security policy
//! forbids (a deleting sync, an HTTP target), which the shell never did.

#![forbid(unsafe_code)]

/// The policy gates a run is held to.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct PolicyGates {
    pub(crate) allow_destructive_sync: bool,
    pub(crate) allow_http_targets: bool,
    pub(crate) allow_invalid_tls: bool,
    pub(crate) allow_remote_logging: bool,
    pub(crate) allow_empty_source: bool,
}

/// What a profile fragment asks for, as far as the gates care.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ProfileAsks {
    pub(crate) http: bool,
    pub(crate) delete: bool,
    pub(crate) allow_empty_source: bool,
    pub(crate) invalid_tls: bool,
    pub(crate) remote_logging: bool,
}

/// A refusal: the reason token and one line naming the profile.
pub(crate) type Refusal = (&'static str, String);

/// Read the fragment's relevant keys; a fragment that does not parse is
/// `profile_invalid`, never "asks for nothing".
pub(crate) fn profile_asks(name: &str, fragment: &[u8]) -> Result<ProfileAsks, Refusal> {
    let invalid = |detail: String| ("profile_invalid", format!("profile '{name}' {detail}"));
    let text = std::str::from_utf8(fragment).map_err(|_| invalid("is not UTF-8".into()))?;
    let table: toml::Table =
        toml::from_str(text).map_err(|error| invalid(format!("does not parse: {error}")))?;
    let flag = |key: &str| -> Result<bool, Refusal> {
        match table.get(key) {
            None => Ok(false),
            Some(toml::Value::Boolean(value)) => Ok(*value),
            Some(_) => Err(invalid(format!("has a {key} that is not true or false"))),
        }
    };
    let http = match table.get("url") {
        Some(toml::Value::String(url)) if url.starts_with("https://") => false,
        Some(toml::Value::String(url)) if url.starts_with("http://") => true,
        _ => return Err(invalid("has no http(s) url".into())),
    };
    Ok(ProfileAsks {
        http,
        delete: flag("delete")?,
        allow_empty_source: flag("allow-empty-source")?,
        invalid_tls: flag("danger-accept-invalid-certs")?,
        remote_logging: table.contains_key("remote-log-url"),
    })
}

/// The runner's gates, in its order.
pub(crate) fn decide(
    gates: PolicyGates,
    name: &str,
    asks: &ProfileAsks,
    allow_delete: bool,
) -> Result<(), Refusal> {
    let refuse = |reason, what: &str| {
        Err((
            reason,
            format!("security policy blocks {what} for profile '{name}'"),
        ))
    };
    if !gates.allow_destructive_sync && (asks.delete || allow_delete) {
        return refuse("policy_destructive", "destructive sync");
    }
    if !gates.allow_http_targets && asks.http {
        return refuse("policy_http", "HTTP targets");
    }
    if !gates.allow_invalid_tls && asks.invalid_tls {
        return refuse("policy_invalid_tls", "invalid TLS certificates");
    }
    if !gates.allow_remote_logging && asks.remote_logging {
        return refuse("policy_remote_logging", "remote logging");
    }
    if !gates.allow_empty_source && asks.allow_empty_source {
        return refuse("policy_empty_source", "empty-source synchronization");
    }
    Ok(())
}

#[cfg(target_os = "linux")]
pub(crate) use linux::check;

#[cfg(target_os = "linux")]
mod linux {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::path::Path;

    use super::{PolicyGates, Refusal, decide, profile_asks};

    fn private_secret(path: &Path, uid: u32) -> bool {
        std::fs::symlink_metadata(path).is_ok_and(|metadata| {
            metadata.file_type().is_file()
                && metadata.uid() == uid
                && metadata.permissions().mode() & 0o777 == 0o600
                && metadata.nlink() == 1
        })
    }

    /// Every profile of a run against the policy and its secrets.
    pub(crate) fn check(
        home: &Path,
        profiles: &[String],
        allow_delete: bool,
        uid: u32,
    ) -> Result<(), Refusal> {
        let policy = super::super::super::linux_files::load_security_policy_at(
            &home.join("config/security.conf"),
            uid,
        )
        .map_err(|_| {
            (
                "policy_unreadable",
                "security policy is not a private complete document".to_owned(),
            )
        })?;
        let gates = PolicyGates {
            allow_destructive_sync: policy.allow_destructive_sync,
            allow_http_targets: policy.allow_http_targets,
            allow_invalid_tls: policy.allow_invalid_tls,
            allow_remote_logging: policy.allow_remote_logging,
            allow_empty_source: policy.allow_empty_source,
        };
        for name in profiles {
            let fragment = home.join("config/profiles.d").join(format!("{name}.toml"));
            let bytes = std::fs::read(&fragment).map_err(|error| {
                (
                    "profile_invalid",
                    format!("profile '{name}': {}: {error}", fragment.display()),
                )
            })?;
            let asks = profile_asks(name, &bytes)?;
            decide(gates, name, &asks, allow_delete)?;
            let secrets = home.join("secrets");
            if !private_secret(&secrets.join(format!("{name}.password")), uid) {
                return Err((
                    "password_missing",
                    format!(
                        "profile '{name}' has no protected password; use sdsync-dsm set-password"
                    ),
                ));
            }
            for optional in ["totp", "remote-log-token"] {
                let path = secrets.join(format!("{name}.{optional}"));
                if std::fs::symlink_metadata(&path).is_ok() && !private_secret(&path, uid) {
                    return Err((
                        "secret_unsafe",
                        format!(
                            "optional secret material is not a package-owned 0600 regular file: {}",
                            path.display()
                        ),
                    ));
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HTTPS: &[u8] = b"url = \"https://nas:5001\"\ndelete = false\n";

    #[test]
    fn each_gate_refuses_by_name_and_open_gates_pass() {
        let closed = PolicyGates::default();
        let open = PolicyGates {
            allow_destructive_sync: true,
            allow_http_targets: true,
            allow_invalid_tls: true,
            allow_remote_logging: true,
            allow_empty_source: true,
        };
        for (fragment, allow_delete, reason) in [
            (
                &b"url = \"https://n\"\ndelete = true\n"[..],
                false,
                "policy_destructive",
            ),
            (HTTPS, true, "policy_destructive"),
            (b"url = \"http://n\"\n", false, "policy_http"),
            (
                b"url = \"https://n\"\ndanger-accept-invalid-certs = true\n",
                false,
                "policy_invalid_tls",
            ),
            (
                b"url = \"https://n\"\nremote-log-url = \"https://log\"\n",
                false,
                "policy_remote_logging",
            ),
            (
                b"url = \"https://n\"\nallow-empty-source = true\n",
                false,
                "policy_empty_source",
            ),
        ] {
            let asks = profile_asks("p", fragment).unwrap();
            let (got, detail) = decide(closed, "p", &asks, allow_delete).unwrap_err();
            assert_eq!(got, reason);
            assert!(detail.contains("profile 'p'"), "{detail}");
            assert_eq!(decide(open, "p", &asks, allow_delete), Ok(()));
        }
        let asks = profile_asks("p", HTTPS).unwrap();
        assert_eq!(decide(closed, "p", &asks, false), Ok(()));
    }

    #[test]
    fn a_fragment_that_does_not_say_what_it_asks_is_refused() {
        for bad in [
            &b"delete = true\n"[..],
            b"url = \"ftp://x\"\n",
            b"url = \"https://x\"\ndelete = \"yes\"\n",
            b"= broken",
        ] {
            assert_eq!(profile_asks("p", bad).unwrap_err().0, "profile_invalid");
        }
    }
}
