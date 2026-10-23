#![allow(dead_code)]

mod support;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use support::TestDir;
use support::file_station_mock::MockFileStation;
use synology_drive_sync::cancel::CancellationToken;
use synology_drive_sync::sdk::{
    Comparison, DeferredReason, DeletionPolicy, Engine, ErrorCode, EventControl, MutationEvent,
    OtpChallenge, PlanDecision, PlanOperation, SdkError, SdkEvent, Secret, SecretProvider,
    SecretProviderError, SyncOutcome, SyncRequest,
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

/// N4/N-6: the SDK reaches the command line's verdict. A file that changes while it is hashed is
/// deferred by name and the run is partial, not failed; before, the SDK failed any run whose
/// reconciliation plan was not empty, and the CLI did not.
#[test]
fn a_file_changing_during_hashing_makes_an_sdk_run_partial_not_failed() {
    let server = MockFileStation::start();
    // Present, so the remote walk lists it after the local scan and before hashing.
    server.add_directory("/team/sdk");
    let source = TestDir::new("sdk-deferral");
    source.write("steady.txt", b"steady");
    source.write("busy.log", b"first line\n");
    let busy = source.path().join("busy.log");
    server.on_next_request("SYNO.FileStation.List.list", move || {
        std::fs::write(&busy, b"first line\nsecond line\n").expect("append to busy file");
    });
    let request = request(&server, &source);
    let mut secrets = FixedSecrets::new();
    let cancellation = CancellationToken::default();
    let deferred_events = Arc::new(AtomicUsize::new(0));
    let deferred_events_for_callback = Arc::clone(&deferred_events);

    let outcome = Engine
        .run(
            &request,
            &mut secrets,
            &cancellation,
            |_| PlanDecision::Apply,
            move |event| {
                if matches!(
                    event,
                    SdkEvent::Mutation {
                        mutation: MutationEvent::PathDeferred { .. }
                    }
                ) {
                    deferred_events_for_callback.fetch_add(1, Ordering::AcqRel);
                }
                EventControl::Continue
            },
        )
        .expect("one busy file is not a failed run");

    assert!(outcome.applied());
    assert!(outcome.reconciled());
    assert!(outcome.partial());
    assert_eq!(outcome.deferred().len(), 1, "{:?}", outcome.deferred());
    assert_eq!(outcome.deferred()[0].relative(), "busy.log");
    assert_eq!(
        outcome.deferred()[0].reason(),
        DeferredReason::SourceChanged
    );
    // The plan an embedding application is shown names it too, as `plan` does on the command line.
    let skipped = outcome.plan().skipped();
    assert_eq!(skipped.len(), 1);
    assert_eq!(skipped[0].relative(), "busy.log");
    assert_eq!(skipped[0].reason(), "deferred");
    assert!(!skipped[0].is_lasting());
    assert_eq!(
        outcome.execution().expect("execution summary").uploaded(),
        1
    );
    assert_eq!(
        server.file_contents("/team/sdk/steady.txt"),
        Some(b"steady".to_vec())
    );
    assert!(server.file_contents("/team/sdk/busy.log").is_none());
    // Deferred during planning, so it is reported in the outcome rather than as an execution event.
    assert_eq!(deferred_events.load(Ordering::Acquire), 0);
}

fn run_applying(request: &SyncRequest) -> Result<SyncOutcome, SdkError> {
    let mut secrets = FixedSecrets::new();
    Engine.run(
        request,
        &mut secrets,
        &CancellationToken::default(),
        |_| PlanDecision::Apply,
        |_| EventControl::Continue,
    )
}

/// ADV-R-4: a DSM that ignores the upload `mtime` makes every run re-upload the whole tree. The
/// command line warns and counts it; the SDK outcome said nothing at all.
#[test]
fn an_ignored_upload_mtime_is_counted_in_the_sdk_outcome() {
    let server = MockFileStation::start();
    server.stamp_uploads_with(1_800_000_000);
    let source = TestDir::new("sdk-mtime");
    source.write("a.txt", b"a");
    source.write("b.txt", b"b");
    let request = SyncRequest::builder(server.base_url(), "e2e-user", source.path(), "/team/sdk")
        .allow_http(true)
        .comparison(Comparison::Metadata)
        .build()
        .expect("build SDK request");

    for run in 1..=2 {
        let outcome = run_applying(&request).expect("SDK run succeeds");
        let execution = outcome.execution().expect("executed");
        assert_eq!(execution.uploaded(), 2, "run {run}");
        assert_eq!(
            execution.mtime_not_kept(),
            2,
            "run {run}: the reason every file is re-uploaded is reported"
        );
    }
}

/// ADV-R-4: `recovered` reaches the serialised outcome, which is what the FFI returns.
#[test]
fn the_serialised_outcome_carries_what_the_run_recovered_from() {
    let server = MockFileStation::start();
    server.fail_next_http_operation("SYNO.FileStation.Upload.upload", 502);
    let source = TestDir::new("sdk-recovered");
    source.write("a.txt", b"a");
    let request = SyncRequest::builder(server.base_url(), "e2e-user", source.path(), "/team/sdk")
        .allow_http(true)
        .build()
        .expect("build SDK request");

    let outcome = run_applying(&request).expect("SDK run succeeds");

    assert_eq!(outcome.recovered().retries, 1);
    let document = serde_json::to_value(&outcome).expect("serialise outcome");
    assert_eq!(document["recovered"]["retries"], 1, "{document}");
    for key in [
        "reauthentications",
        "reauthentication_failures",
        "keepalive_failures",
        "server_tasks_not_stopped",
    ] {
        assert_eq!(document["recovered"][key], 0, "{key}: {document}");
    }
    for key in [
        "mtime_not_kept",
        "copy_fallbacks",
        "deletes_skipped",
        "kept_deletes_uploaded",
        "renamed",
    ] {
        assert_eq!(document["execution"][key], 0, "{key}: {document}");
    }
}

/// ADV-R-4: a folder whose listing DSM returned incomplete is named even when no deletion was
/// planned there, as the command line names it.
#[test]
fn an_incomplete_remote_listing_is_named_in_the_sdk_outcome() {
    let server = MockFileStation::start();
    server.add_directory("/team/sdk");
    server.add_directory("/team/sdk/abe");
    server.add_file("/team/sdk/abe/visible.txt", b"v", 1_700_000_000);
    server.hide_list_entries("/team/sdk/abe", 1);
    let source = TestDir::new("sdk-incomplete");
    source.write("abe/visible.txt", b"v");
    let request = SyncRequest::builder(server.base_url(), "e2e-user", source.path(), "/team/sdk")
        .allow_http(true)
        .build()
        .expect("build SDK request");

    let outcome = run_applying(&request).expect("SDK run succeeds");

    assert_eq!(outcome.incomplete_listings(), ["abe".to_owned()]);
    let document = serde_json::to_value(&outcome).expect("serialise outcome");
    assert_eq!(document["incomplete_listings"][0], "abe");
}

/// ADV-R-4: a run that fails part-way still says what it completed first. Before, the error
/// carried only a message, and the folder this run had already created was reported nowhere.
#[test]
fn a_run_failing_mid_execution_reports_what_it_completed() {
    let server = MockFileStation::start();
    server.add_directory("/team/sdk");
    server.fail_next_api_operation("SYNO.FileStation.Upload.upload", 415);
    let source = TestDir::new("sdk-partial-failure");
    source.write("dir/x.txt", b"x");
    let request = SyncRequest::builder(server.base_url(), "e2e-user", source.path(), "/team/sdk")
        .allow_http(true)
        .build()
        .expect("build SDK request");

    let error = run_applying(&request).expect_err("quota exceeded fails the run");

    assert!(
        error.message().contains("/team/sdk/dir/x.txt"),
        "{}",
        error.message()
    );
    let completed = error.completed().expect("execution had started");
    assert_eq!(
        completed.created(),
        1,
        "the folder was created before the failure"
    );
    assert_eq!(completed.uploaded(), 0);
}

/// ADV-R-4: a planned server-side copy DSM refuses to start is uploaded instead, and counted.
#[test]
fn a_copy_fallback_is_counted_in_the_sdk_outcome() {
    let server = MockFileStation::start();
    server.add_directory("/team/sdk");
    server.add_directory("/team/sdk/old");
    let source = TestDir::new("sdk-copy-fallback");
    let local = source.write("new/item.bin", b"copy-me-on-nas");
    let mtime = std::fs::metadata(&local)
        .unwrap()
        .modified()
        .unwrap()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    server.add_file("/team/sdk/old/item.bin", b"copy-me-on-nas", mtime);
    server.fail_next_api_operation("SYNO.FileStation.CopyMove.start", 407);
    let request = SyncRequest::builder(server.base_url(), "e2e-user", source.path(), "/team/sdk")
        .allow_http(true)
        .comparison(Comparison::Content)
        .build()
        .expect("build SDK request");

    let outcome = run_applying(&request).expect("SDK run succeeds");

    let execution = outcome.execution().expect("executed");
    assert_eq!(execution.copy_fallbacks(), 1, "{execution:?}");
    assert_eq!(
        server.file_contents("/team/sdk/new/item.bin"),
        Some(b"copy-me-on-nas".to_vec())
    );
}

/// ADV-R-2 through the SDK: a remote name this tool cannot address no longer fails the run, is
/// never deleted, and its folder is kept and named.
#[test]
fn an_unaddressable_remote_name_is_named_and_its_folder_kept_by_the_sdk() {
    let server = MockFileStation::start();
    server.add_directory("/team/sdk");
    server.add_directory("/team/sdk/photos");
    server.add_file("/team/sdk/photos/Icon\r", b"", 1_700_000_000);
    let source = TestDir::new("sdk-icon");
    source.write("a.txt", b"a");
    let request = SyncRequest::builder(server.base_url(), "e2e-user", source.path(), "/team/sdk")
        .allow_http(true)
        .deletion(DeletionPolicy::bounded(100).expect("deletion policy"))
        .build()
        .expect("build SDK request");

    let outcome = run_applying(&request).expect("an odd remote name does not fail the run");

    assert!(server.file_contents("/team/sdk/photos/Icon\r").is_some());
    assert!(
        server
            .directories()
            .contains(&"/team/sdk/photos".to_owned())
    );
    assert!(
        outcome
            .deferred()
            .iter()
            .any(|record| record.reason() == DeferredReason::RemoteSkipped),
        "{:?}",
        outcome.deferred()
    );
    // A lasting, named warning: only the user can fix the name, so the run is complete rather
    // than partial, which would re-run forever under a realtime trigger (main's decision).
    assert!(DeferredReason::RemoteSkipped.is_lasting());
    assert!(
        !outcome.partial(),
        "a skipped NAS entry is a lasting warning, not a partial run"
    );
}

/// A case-only rename is in the plan an SDK caller previews, before it is applied: counted, listed
/// among the changes, and enough on its own to make the plan non-empty.
#[test]
fn a_case_only_rename_is_visible_in_the_previewed_plan() {
    let server = MockFileStation::start();
    server.add_directory("/team/sdk");
    server.add_directory("/team/sdk/Photos");
    server.add_file("/team/sdk/Photos/a.jpg", b"jpeg-a", 1_700_000_000);
    let source = TestDir::new("sdk-rename-preview");
    source.write("photos/a.jpg", b"jpeg-a");
    let request = SyncRequest::builder(server.base_url(), "e2e-user", source.path(), "/team/sdk")
        .allow_http(true)
        .comparison(Comparison::Content)
        .deletion(DeletionPolicy::bounded(100).expect("deletion policy"))
        .build()
        .expect("build SDK request");
    let mut secrets = FixedSecrets::new();

    let outcome = Engine
        .run(
            &request,
            &mut secrets,
            &CancellationToken::default(),
            |_| PlanDecision::PreviewOnly,
            |_| EventControl::Continue,
        )
        .expect("preview succeeds");

    let plan = outcome.plan();
    assert_eq!(plan.renames(), 1, "{plan:?}");
    assert!(!plan.is_empty());
    let rename = plan
        .changes()
        .iter()
        .find(|change| change.operation() == PlanOperation::RenameCaseOnly)
        .expect("the rename is listed among the changes");
    assert_eq!(rename.remote_path(), "/team/sdk/photos");
    assert_eq!(rename.source(), Some("/team/sdk/Photos"));
    let document = serde_json::to_value(plan).expect("serialise plan");
    assert_eq!(document["renames"], 1, "{document}");
    assert!(
        server
            .requests()
            .iter()
            .all(|request| request.operation() != "SYNO.FileStation.Rename.rename"),
        "a preview renames nothing"
    );
}
