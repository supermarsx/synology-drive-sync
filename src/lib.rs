#![forbid(unsafe_code)]

pub mod api;
pub mod batch;
pub mod cancel;
pub mod error;
pub mod integrity;
pub mod local;
pub mod observability;
pub mod path;
pub mod plan;
pub mod progress;
pub mod sdk;
pub mod source_diagnostics;
pub mod sync;
pub mod transport_diagnostics;
pub mod vault;

pub use error::{Error, Result};

/// The key exchange groups the `ring` provider offers, in its own order.
///
/// This is what identifies the installed provider without depending on a name
/// string: `aws-lc-rs` offers these three plus `X25519MLKEM768`, so the list
/// differs the moment a dependency change swaps the provider back.
const RING_KX_GROUPS: [rustls::NamedGroup; 3] = [
    rustls::NamedGroup::X25519,
    rustls::NamedGroup::secp256r1,
    rustls::NamedGroup::secp384r1,
];

/// Memoised result of the one installation attempt this process makes.
static CRYPTO_PROVIDER: std::sync::OnceLock<core::result::Result<(), &'static str>> =
    std::sync::OnceLock::new();

/// Build a blocking reqwest client.
///
/// **This and [`async_client_builder`] are the only places a reqwest client may
/// be constructed in this workspace.** Installing the provider here rather than
/// at each entry point makes the guarantee structural: a new call site cannot
/// forget it, because there is nowhere else to obtain a builder.
///
/// That matters more than a convention would. `reqwest` built with
/// `rustls-no-provider` does not return an error when no provider is installed —
/// it panics on an internal runtime thread, which reaches the caller as
/// "event loop thread panicked" with no actionable cause.
pub fn blocking_client_builder() -> Result<reqwest::blocking::ClientBuilder> {
    install_crypto_provider()?;
    Ok(reqwest::blocking::Client::builder())
}

/// Build an asynchronous reqwest client. See [`blocking_client_builder`].
pub fn async_client_builder() -> Result<reqwest::ClientBuilder> {
    install_crypto_provider()?;
    Ok(reqwest::Client::builder())
}

/// Install the process-wide rustls crypto provider.
///
/// `reqwest` is built with `rustls-no-provider`, so nothing installs one for us.
/// Every entry point calls this before doing any work, and
/// `ApiClient::connect_with_requirements` calls it again as a safety net, so a
/// path that forgets cannot silently reach TLS without one.
///
/// Failing here has to be loud. Without a provider the failure would otherwise
/// surface inside TLS client construction, which is the one call in this crate
/// that no request deadline covers — exactly the shape of the entropy stall this
/// provider change exists to remove.
///
/// Idempotent: a provider installed by an earlier call, another entry point, or
/// a test harness is accepted, but only if it is the one we asked for.
pub fn install_crypto_provider() -> Result<()> {
    match CRYPTO_PROVIDER.get_or_init(install_ring_provider_once) {
        Ok(()) => Ok(()),
        Err(message) => Err(Error::Message((*message).to_owned())),
    }
}

fn install_ring_provider_once() -> core::result::Result<(), &'static str> {
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        // A concurrent installer may win this race; the check below accepts
        // whichever call succeeded, and rejects a provider we did not choose.
        let _ = rustls::crypto::ring::default_provider().install_default();
    }
    let Some(installed) = rustls::crypto::CryptoProvider::get_default() else {
        return Err("no rustls crypto provider could be installed; TLS is unavailable");
    };
    if installed
        .kx_groups
        .iter()
        .map(|group| group.name())
        .eq(RING_KX_GROUPS)
    {
        return Ok(());
    }
    Err(
        "an unexpected rustls crypto provider is installed; this build requires ring, because \
         other providers can block for minutes on kernels without getrandom(2)",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The provider must be ring, and saying so must not depend on a name
    /// string — `aws-lc-rs` would satisfy any "a provider is installed" check
    /// while reintroducing the entropy stall this build exists to avoid.
    #[test]
    fn installed_provider_is_ring_and_installing_twice_is_accepted() {
        install_crypto_provider().expect("ring installs");
        // Idempotent: entry points, the client-construction safety net and test
        // harnesses all call this, and only the first one wins the race.
        install_crypto_provider().expect("a second install is accepted");

        let installed =
            rustls::crypto::CryptoProvider::get_default().expect("a provider is installed");
        let groups = installed
            .kx_groups
            .iter()
            .map(|group| group.name())
            .collect::<Vec<_>>();
        assert_eq!(groups, RING_KX_GROUPS.to_vec());
        // The distinguishing group: aws-lc-rs offers it, ring does not. If this
        // ever appears, a dependency change has swapped the provider back.
        assert!(
            !groups.contains(&rustls::NamedGroup::X25519MLKEM768),
            "aws-lc-rs is installed; its RNG blocks on kernels without getrandom(2)"
        );
    }

    /// The guarantee has to be structural, not a rule people remember.
    ///
    /// `reqwest` with `rustls-no-provider` does not fail cleanly when no
    /// provider is installed: it panics on an internal runtime thread. A single
    /// missed construction site is therefore a crash with an unactionable
    /// message, and one was missed exactly this way — `query_dsm_user_service`
    /// built its own client and never touched the entry points. So assert that
    /// nothing anywhere constructs a client except the two builders here.
    #[test]
    fn no_reqwest_client_is_built_outside_the_workspace_choke_point() {
        const FORBIDDEN: [&str; 4] = [
            "Client::builder()",
            "Client::new()",
            "blocking::Client::builder",
            "blocking::Client::new",
        ];
        for (name, source) in [
            ("src/api.rs", include_str!("api.rs")),
            ("src/dsm_api.rs", include_str!("dsm_api.rs")),
            ("src/main.rs", include_str!("main.rs")),
            ("src/observability.rs", include_str!("observability.rs")),
            ("src/sdk.rs", include_str!("sdk.rs")),
            ("src/sync.rs", include_str!("sync.rs")),
            ("src/batch.rs", include_str!("batch.rs")),
            (
                "crates/synology-drive-sync-ffi/src/lib.rs",
                include_str!("../crates/synology-drive-sync-ffi/src/lib.rs"),
            ),
        ] {
            for needle in FORBIDDEN {
                assert!(
                    !source.contains(needle),
                    "{name} constructs a reqwest client directly ({needle});                      use blocking_client_builder() or async_client_builder() so                      the crypto provider cannot be missed"
                );
            }
        }
        // ...and the entry points still install eagerly, so a failure is a
        // clear startup error rather than a panic deep inside a queued job.
        assert!(include_str!("main.rs").contains("install_crypto_provider()"));
        assert!(include_str!("dsm_api.rs").contains("install_crypto_provider()"));
    }
}
