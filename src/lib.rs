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

/// Doctor report sections in display order, each with the step at which it runs.
///
/// Display order and execution order deliberately differ: File Station capabilities are settled
/// from the cached discovery response *before* authentication, but reading it fourth in the report
/// keeps the transport, session, and destination groups together. The step number is carried so
/// the report can state the real order rather than implying the printed one.
///
/// This lives in the library rather than in the CLI binary because three consumers must agree on
/// it: the CLI that emits the sections, the AppWindow catalogue that the cross-layer test pins,
/// and the DSM progress-record validator, which resolves a job-supplied section id against this
/// list rather than trusting it. Two of those are separate binaries, so a single definition here
/// is what makes the agreement structural instead of a convention. The UI copy is necessarily a
/// duplicate -- it is JavaScript -- and is held in step by that cross-layer test.
pub const DOCTOR_SECTION_SPECS: [(&str, &str, u8); 16] = [
    (
        "network_reachability",
        "Network reachability and connect timing",
        1,
    ),
    ("routing_tls", "Routing and TLS negotiation", 2),
    ("dsm_api_discovery", "DSM API discovery", 3),
    ("capability_enumeration", "DSM capability enumeration", 4),
    (
        "intermediary_transport",
        "Intermediaries and reverse proxies",
        15,
    ),
    ("dsm_session_auth", "DSM session authentication", 6),
    (
        "session_channel_ablation",
        "DSM session channel ablation",
        7,
    ),
    ("session_concurrency", "Concurrent session fan-out", 8),
    ("session_cookie_ledger", "Session cookie permanence", 16),
    ("file_station_capabilities", "File Station capabilities", 5),
    (
        "capability_diagnosis",
        "File Station capability diagnosis",
        9,
    ),
    (
        "destination_path_resolution",
        "Destination path resolution",
        10,
    ),
    ("destination_permissions", "Destination permissions", 11),
    ("destination_inventory", "Destination inventory", 12),
    (
        "disposable_write_verify_cleanup",
        "Disposable write, verify, and cleanup",
        13,
    ),
    ("session_logout", "DSM session logout", 14),
];

/// Resolve a section id to its display label and step, or `None` if unknown.
///
/// Callers handling untrusted input use this as an allow-list: a job that reports progress names a
/// section by id and the label shown to the operator comes from here, so a compromised or buggy
/// job cannot put arbitrary text on an administrator's screen.
#[must_use]
pub fn doctor_section(id: &str) -> Option<(&'static str, u8)> {
    DOCTOR_SECTION_SPECS
        .iter()
        .find(|(section_id, _, _)| *section_id == id)
        .map(|&(_, label, step)| (label, step))
}

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

    /// Every id resolves, and nothing else does.
    ///
    /// The `None` arm is the load-bearing one: it is what makes the resolver an allow-list rather
    /// than a lookup, so a progress record naming an unknown section yields no label at all
    /// instead of passing job-controlled text through to an administrator's screen.
    #[test]
    fn doctor_sections_resolve_by_id_and_reject_anything_else() {
        for (id, label, step) in DOCTOR_SECTION_SPECS {
            assert_eq!(
                doctor_section(id),
                Some((label, step)),
                "id {id} did not resolve"
            );
        }
        for unknown in [
            "",
            "unknown",
            "Network_Reachability",
            "network_reachability ",
            "../etc",
        ] {
            assert_eq!(
                doctor_section(unknown),
                None,
                "{unknown:?} must not resolve"
            );
        }
    }

    /// Ids are unique and the steps are exactly 1..=16.
    ///
    /// A duplicate id would make the resolver's answer depend on ordering, and a gap or repeat in
    /// the steps would let the AppWindow render "step 7 of 16" for two different sections.
    #[test]
    fn doctor_section_ids_are_unique_and_steps_are_a_complete_permutation() {
        let mut ids: Vec<&str> = DOCTOR_SECTION_SPECS.iter().map(|&(id, _, _)| id).collect();
        ids.sort_unstable();
        let count = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), count, "duplicate doctor section id");

        let mut steps: Vec<u8> = DOCTOR_SECTION_SPECS
            .iter()
            .map(|&(_, _, step)| step)
            .collect();
        steps.sort_unstable();
        assert_eq!(
            steps,
            (1..=16).collect::<Vec<u8>>(),
            "steps are not 1..=16 exactly"
        );
    }

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
