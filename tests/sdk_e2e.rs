#![allow(dead_code)]

mod support;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use support::TestDir;
use support::file_station_mock::MockFileStation;
use synology_drive_sync::cancel::CancellationToken;
use synology_drive_sync::sdk::{
    Engine, ErrorCode, EventControl, OtpChallenge, PlanDecision, SdkEvent, Secret, SecretProvider,
    SecretProviderError, SyncRequest,
};

struct FixedSecrets {
    password_calls: usize,
    otp_challenges: Vec<OtpChallenge>,
}

impl FixedSecrets {
    fn new() -> Self {
        Self {
            password_calls: 0,
            otp_challenges: Vec::new(),
        }
    }
}

impl SecretProvider for FixedSecrets {
    fn password(&mut self) -> Result<Secret, SecretProviderError> {
        self.password_calls += 1;
        Ok(Secret::new("correct horse battery staple"))
    }

    fn otp(&mut self, challenge: OtpChallenge) -> Result<Option<Secret>, SecretProviderError> {
        self.otp_challenges.push(challenge);
        Ok(Some(Secret::new("654321")))
    }
}

fn request(server: &MockFileStation, source: &TestDir) -> SyncRequest {
    SyncRequest::builder(server.base_url(), "e2e-user", source.path(), "/team/sdk")
        .allow_http(true)
        .build()
        .expect("build SDK request")
}

#[test]
fn preview_exposes_the_real_plan_without_remote_mutation() {
    let server = MockFileStation::start();
    let source = TestDir::new("sdk-preview");
    source.write("hello.txt", b"hello from the SDK");
    let request = request(&server, &source);
    let mut secrets = FixedSecrets::new();
    let cancellation = CancellationToken::default();
    let observed_plan_size = Arc::new(Mutex::new(None));
    let observed_plan_size_for_callback = Arc::clone(&observed_plan_size);

    let outcome = Engine
        .run(
            &request,
            &mut secrets,
            &cancellation,
            move |plan| {
                *observed_plan_size_for_callback.lock().expect("plan lock") =
                    Some(plan.changes().len());
                PlanDecision::PreviewOnly
            },
            |_| EventControl::Continue,
        )
        .expect("preview succeeds");

    assert!(!outcome.applied());
    assert!(!outcome.reconciled());
    assert!(outcome.execution().is_none());
    assert_eq!(outcome.plan().creates(), 1);
    assert_eq!(outcome.plan().uploads(), 1);
    assert_eq!(
        *observed_plan_size.lock().expect("plan lock"),
        Some(outcome.plan().changes().len())
    );
    assert!(server.file_contents("/team/sdk/hello.txt").is_none());
    assert_eq!(secrets.password_calls, 1);
    assert!(secrets.otp_challenges.is_empty());
    let operations: Vec<_> = server
        .requests()
        .into_iter()
        .map(|request| request.operation())
        .collect();
    assert!(!operations.iter().any(|operation| {
        operation == "SYNO.FileStation.CreateFolder.create"
            || operation == "SYNO.FileStation.Upload.upload"
            || operation == "SYNO.FileStation.Delete.delete"
    }));
    assert!(
        operations
            .iter()
            .any(|operation| operation == "SYNO.API.Auth.logout")
    );
}

#[test]
fn apply_uses_the_engine_plan_executes_and_reconciles() {
    let server = MockFileStation::start();
    let source = TestDir::new("sdk-apply");
    source.write("nested/hello.txt", b"applied through SDK engine");
    let request = request(&server, &source);
    let mut secrets = FixedSecrets::new();
    let cancellation = CancellationToken::default();
    let events = Arc::new(Mutex::new(Vec::new()));
    let events_for_callback = Arc::clone(&events);

    let outcome = Engine
        .run(
            &request,
            &mut secrets,
            &cancellation,
            |_| PlanDecision::Apply,
            move |event| {
                events_for_callback
                    .lock()
                    .expect("event lock")
                    .push(event.clone());
                EventControl::Continue
            },
        )
        .expect("apply and reconciliation succeed");

    assert!(outcome.applied());
    assert!(outcome.reconciled());
    let execution = outcome.execution().expect("execution summary");
    assert_eq!(execution.uploaded(), 1);
    assert_eq!(execution.created(), 2);
    assert_eq!(
        server.file_contents("/team/sdk/nested/hello.txt"),
        Some(b"applied through SDK engine".to_vec())
    );
    assert!(
        events
            .lock()
            .expect("event lock")
            .iter()
            .any(|event| { matches!(event, SdkEvent::Mutation { .. }) })
    );
}

#[test]
fn otp_is_requested_only_after_the_server_challenge() {
    let server = MockFileStation::start();
    server.require_totp();
    let source = TestDir::new("sdk-otp");
    source.write("hello.txt", b"OTP preview");
    let request = request(&server, &source);
    let mut secrets = FixedSecrets::new();

    Engine
        .run(
            &request,
            &mut secrets,
            &CancellationToken::default(),
            |_| PlanDecision::PreviewOnly,
            |_| EventControl::Continue,
        )
        .expect("challenge-aware login succeeds");

    assert_eq!(secrets.password_calls, 1);
    assert_eq!(secrets.otp_challenges, vec![OtpChallenge::Required]);
    let logins: Vec<_> = server
        .requests()
        .into_iter()
        .filter(|request| request.operation() == "SYNO.API.Auth.login")
        .collect();
    assert_eq!(logins.len(), 2);
    assert!(!logins[0].fields.contains_key("otp_code"));
    assert_eq!(
        logins[1].fields.get("otp_code").map(String::as_str),
        Some("654321")
    );
}

#[test]
fn a_rejected_otp_requests_exactly_one_fresh_value_and_retries() {
    let server = MockFileStation::start();
    server.require_totp();
    server.reject_next_valid_otp();
    let source = TestDir::new("sdk-otp-retry");
    source.write("hello.txt", b"OTP retry preview");
    let request = request(&server, &source);
    let mut secrets = FixedSecrets::new();

    Engine
        .run(
            &request,
            &mut secrets,
            &CancellationToken::default(),
            |_| PlanDecision::PreviewOnly,
            |_| EventControl::Continue,
        )
        .expect("one fresh OTP retry succeeds");

    assert_eq!(
        secrets.otp_challenges,
        vec![OtpChallenge::Required, OtpChallenge::Rejected]
    );
    let logins: Vec<_> = server
        .requests()
        .into_iter()
        .filter(|request| request.operation() == "SYNO.API.Auth.login")
        .collect();
    assert_eq!(logins.len(), 3);
    assert!(!logins[0].fields.contains_key("otp_code"));
    assert!(logins[1].fields.contains_key("otp_code"));
    assert!(logins[2].fields.contains_key("otp_code"));
}

#[test]
fn event_cancellation_before_authentication_never_requests_password() {
    let server = MockFileStation::start();
    let source = TestDir::new("sdk-cancel");
    source.write("hello.txt", b"cancel before credentials");
    let request = request(&server, &source);
    let mut secrets = FixedSecrets::new();

    let error = Engine
        .run(
            &request,
            &mut secrets,
            &CancellationToken::default(),
            |_| PlanDecision::PreviewOnly,
            |event| {
                if matches!(
                    event,
                    SdkEvent::PhaseCompleted {
                        phase: synology_drive_sync::sdk::Phase::ApiDiscovery
                    }
                ) {
                    EventControl::Cancel
                } else {
                    EventControl::Continue
                }
            },
        )
        .expect_err("observer cancellation must stop the run");
    assert_eq!(error.code(), ErrorCode::Cancelled);
    assert_eq!(secrets.password_calls, 0);
}

#[test]
fn cancellation_after_authentication_still_logs_out_the_live_session() {
    let server = MockFileStation::start();
    let source = TestDir::new("sdk-cancel-after-auth");
    source.write("hello.txt", b"cancel after credentials");
    let request = request(&server, &source);
    let mut secrets = FixedSecrets::new();

    let error = Engine
        .run(
            &request,
            &mut secrets,
            &CancellationToken::default(),
            |_| PlanDecision::PreviewOnly,
            |event| {
                if matches!(
                    event,
                    SdkEvent::PhaseCompleted {
                        phase: synology_drive_sync::sdk::Phase::Authentication
                    }
                ) {
                    EventControl::Cancel
                } else {
                    EventControl::Continue
                }
            },
        )
        .expect_err("post-login cancellation must stop after cleanup");

    assert_eq!(error.code(), ErrorCode::Cancelled);
    assert_eq!(secrets.password_calls, 1);
    let operations: Vec<_> = server
        .requests()
        .into_iter()
        .map(|request| request.operation())
        .collect();
    assert!(
        operations
            .iter()
            .any(|operation| operation == "SYNO.API.Auth.logout")
    );
    assert!(!operations.iter().any(|operation| {
        operation == "SYNO.FileStation.List.list_share" || operation == "SYNO.FileStation.List.list"
    }));
}

#[test]
fn panic_after_authentication_logs_out_without_reinvoking_the_observer() {
    let server = MockFileStation::start();
    let source = TestDir::new("sdk-panic-after-auth");
    source.write("hello.txt", b"panic after credentials");
    let request = request(&server, &source);
    let mut secrets = FixedSecrets::new();
    let observer_panicked = Arc::new(AtomicBool::new(false));
    let calls_after_panic = Arc::new(AtomicUsize::new(0));
    let observer_panicked_for_callback = Arc::clone(&observer_panicked);
    let calls_after_panic_for_callback = Arc::clone(&calls_after_panic);

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = Engine.run(
            &request,
            &mut secrets,
            &CancellationToken::default(),
            |_| PlanDecision::PreviewOnly,
            |event| {
                if observer_panicked_for_callback.load(Ordering::SeqCst) {
                    calls_after_panic_for_callback.fetch_add(1, Ordering::SeqCst);
                    return EventControl::Continue;
                }
                if matches!(
                    event,
                    SdkEvent::PhaseCompleted {
                        phase: synology_drive_sync::sdk::Phase::Authentication
                    }
                ) {
                    observer_panicked_for_callback.store(true, Ordering::SeqCst);
                    panic!("observer panic after authentication");
                }
                EventControl::Continue
            },
        );
    }));

    assert!(result.is_err(), "the original observer panic must resume");
    assert!(observer_panicked.load(Ordering::SeqCst));
    assert_eq!(calls_after_panic.load(Ordering::SeqCst), 0);
    assert_eq!(secrets.password_calls, 1);
    let operations: Vec<_> = server
        .requests()
        .into_iter()
        .map(|request| request.operation())
        .collect();
    assert!(
        operations
            .iter()
            .any(|operation| operation == "SYNO.API.Auth.logout")
    );
}

#[test]
fn panic_when_logout_starts_logs_out_without_reinvoking_the_observer() {
    let server = MockFileStation::start();
    let source = TestDir::new("sdk-panic-at-logout");
    source.write("hello.txt", b"panic at logout");
    let request = request(&server, &source);
    let mut secrets = FixedSecrets::new();
    let observer_panicked = Arc::new(AtomicBool::new(false));
    let calls_after_panic = Arc::new(AtomicUsize::new(0));
    let observer_panicked_for_callback = Arc::clone(&observer_panicked);
    let calls_after_panic_for_callback = Arc::clone(&calls_after_panic);

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = Engine.run(
            &request,
            &mut secrets,
            &CancellationToken::default(),
            |_| PlanDecision::PreviewOnly,
            |event| {
                if observer_panicked_for_callback.load(Ordering::SeqCst) {
                    calls_after_panic_for_callback.fetch_add(1, Ordering::SeqCst);
                    return EventControl::Continue;
                }
                if matches!(
                    event,
                    SdkEvent::PhaseStarted {
                        phase: synology_drive_sync::sdk::Phase::Logout
                    }
                ) {
                    observer_panicked_for_callback.store(true, Ordering::SeqCst);
                    panic!("observer panic when logout starts");
                }
                EventControl::Continue
            },
        );
    }));

    assert!(result.is_err(), "the logout observer panic must resume");
    assert!(observer_panicked.load(Ordering::SeqCst));
    assert_eq!(calls_after_panic.load(Ordering::SeqCst), 0);
    assert_eq!(secrets.password_calls, 1);
    assert!(
        server
            .requests()
            .iter()
            .any(|request| request.operation() == "SYNO.API.Auth.logout")
    );
}
