use std::error::Error;
use std::path::PathBuf;

use synology_drive_sync::cancel::CancellationToken;
use synology_drive_sync::sdk::{
    Engine, EventControl, OtpChallenge, PlanDecision, Secret, SecretProvider, SecretProviderError,
    SyncRequest,
};

struct PromptSecrets;

impl SecretProvider for PromptSecrets {
    fn password(&mut self) -> Result<Secret, SecretProviderError> {
        rpassword::prompt_password("DSM password: ")
            .map(Secret::new)
            .map_err(|_| SecretProviderError::Unavailable)
    }

    fn otp(&mut self, challenge: OtpChallenge) -> Result<Option<Secret>, SecretProviderError> {
        eprintln!("DSM OTP challenge: {challenge:?}");
        rpassword::prompt_password("DSM six-digit OTP: ")
            .map(Secret::new)
            .map(Some)
            .map_err(|_| SecretProviderError::Unavailable)
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let mut arguments = std::env::args_os().skip(1);
    let endpoint = arguments.next().ok_or("missing ENDPOINT")?;
    let username = arguments.next().ok_or("missing USERNAME")?;
    let source = arguments.next().ok_or("missing SOURCE")?;
    let remote = arguments.next().ok_or("missing REMOTE")?;
    let apply = arguments.next().is_some_and(|value| value == "--apply");

    let request = SyncRequest::builder(
        endpoint
            .into_string()
            .map_err(|_| "ENDPOINT is not UTF-8")?,
        username
            .into_string()
            .map_err(|_| "USERNAME is not UTF-8")?,
        PathBuf::from(source),
        remote.into_string().map_err(|_| "REMOTE is not UTF-8")?,
    )
    .build()?;

    let mut secrets = PromptSecrets;
    let cancellation = CancellationToken::default();
    let outcome = Engine.run(
        &request,
        &mut secrets,
        &cancellation,
        |plan| {
            eprintln!(
                "plan: {} changes, {} upload bytes",
                plan.changes().len(),
                plan.upload_bytes()
            );
            if apply {
                PlanDecision::Apply
            } else {
                PlanDecision::PreviewOnly
            }
        },
        |event| {
            eprintln!("{event:?}");
            EventControl::Continue
        },
    )?;

    println!(
        "applied={}, reconciled={}, changes={}",
        outcome.applied(),
        outcome.reconciled(),
        outcome.plan().changes().len()
    );
    Ok(())
}
