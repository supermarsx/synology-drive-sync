#![forbid(unsafe_code)]

//! The status digest cache, driven through the real binary against a mock File Station.
//!
//! Every other test of this cache exercises the module directly. This one exercises the wiring: the
//! command-line flag, the profile key, the two digest populators the cache narrows, the JSON the
//! answer carries, the rollup written beside it, and the round trips actually saved.
//!
//! The round trips are the point. `SYNO.FileStation.MD5` is a task API — a start plus at least one
//! status poll per file — and in the steady state a status query spends nearly all of its wall clock
//! there. The mock records every request, so "the cache removed this work" is asserted against a
//! request log rather than argued.

/// Only the listing, MD5, and login surface is exercised here.
#[allow(dead_code)]
mod support;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;
use support::TestDir;
use support::file_station_mock::MockFileStation;

const PASSWORD: &[u8] = b"correct horse battery staple\n";
const FILES: usize = 6;

fn modified_seconds(path: &Path) -> i64 {
    i64::try_from(
        fs::metadata(path)
            .expect("fixture metadata")
            .modified()
            .expect("fixture modification time")
            .duration_since(UNIX_EPOCH)
            .expect("fixture predates the Unix epoch")
            .as_secs(),
    )
    .expect("fixture timestamp fits i64")
}

fn text(path: &Path) -> &str {
    path.to_str().expect("UTF-8 fixture path")
}

/// Move a fixture file's modification time an hour into the past.
fn backdate(path: &Path) {
    let handle = fs::OpenOptions::new()
        .write(true)
        .open(path)
        .expect("open a fixture file to backdate it");
    let when = SystemTime::now() - Duration::from_secs(3600);
    handle.set_modified(when).expect("backdate a fixture file");
    handle.sync_all().expect("flush the backdated timestamp");
}

struct Fixture {
    directory: TestDir,
    server: MockFileStation,
    source: PathBuf,
    cache: PathBuf,
    password: PathBuf,
}

impl Fixture {
    /// A tree whose every file already matches the remote byte for byte, with the sizes and
    /// modification times agreeing on both sides. That is the steady state the cache exists for,
    /// and the one where the comparison set is the whole tree.
    fn new() -> Self {
        let directory = TestDir::new("status-cache-e2e");
        let source = directory.child("source");
        fs::create_dir(&source).expect("create fixture source");
        let cache = directory.child("cache");
        fs::create_dir(&cache).expect("create fixture cache directory");
        let password = directory.write("password", PASSWORD);

        let server = MockFileStation::start();
        server.add_directory("/team/export");
        for index in 0..FILES {
            let name = format!("payload-{index}.bin");
            let path = source.join(&name);
            let contents = format!("payload number {index}").into_bytes();
            fs::write(&path, &contents).expect("write a fixture source file");
            // Backdate the file. A digest may never be reused for a file modified within a second
            // of the cache being written -- git's racy-index rule -- so a fixture created moments
            // ago would exercise the withholding path rather than the reuse path.
            backdate(&path);
            server.add_file(
                &format!("/team/export/{name}"),
                &contents,
                modified_seconds(&path),
            );
        }

        Self {
            directory,
            server,
            source,
            cache,
            password,
        }
    }

    fn status(&self, trailing: &[&str]) -> Output {
        let mut arguments = vec![
            "--output",
            "json",
            "status",
            text(&self.source),
            "/team/export",
            "--url",
            self.server.base_url(),
            "--username",
            "e2e-user",
            "--password-file",
            text(&self.password),
            "--no-vault",
            "--allow-http",
            "--compare",
            "content",
            "--all",
        ];
        arguments.extend(trailing.iter().copied());

        let environment = self.directory.child("home");
        fs::create_dir_all(&environment).expect("create isolated subprocess home");
        let mut command = Command::new(env!("CARGO_BIN_EXE_synology-drive-sync"));
        command
            .args(&arguments)
            .env("HOME", &environment)
            .env("USERPROFILE", &environment)
            .env("NO_COLOR", "1")
            .stdin(Stdio::null());
        for (name, _) in std::env::vars_os() {
            if name.to_string_lossy().starts_with("SDSYNC_") {
                command.env_remove(name);
            }
        }
        command.output().expect("run synology-drive-sync")
    }

    fn cached_status(&self, trailing: &[&str]) -> Value {
        let mut arguments = vec!["--status-cache", text(&self.cache)];
        arguments.extend(trailing.iter().copied());
        let output = self.status(&arguments);
        assert_eq!(
            output.status.code(),
            Some(0),
            "status failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).expect("status stdout is one JSON document")
    }

    /// How many File Station MD5 operations the server has been asked for so far.
    fn md5_calls(&self) -> usize {
        self.server
            .requests()
            .iter()
            .filter(|request| request.operation().starts_with("SYNO.FileStation.MD5"))
            .count()
    }

    /// The single rollup in the cache directory. Located by suffix rather than by name because the
    /// profile key for a configuration-free invocation is an internal detail, and asserting on it
    /// here would couple this test to something it is not testing.
    fn rollup_path(&self) -> PathBuf {
        let mut found: Vec<PathBuf> = fs::read_dir(&self.cache)
            .expect("cache directory is readable")
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| path.to_string_lossy().ends_with(".rollup.json"))
            .collect();
        assert_eq!(
            found.len(),
            1,
            "expected exactly one rollup, found {found:?}"
        );
        found.remove(0)
    }

    /// Run `status-rollup` over this fixture's cache, in JSON, with nothing else configured.
    fn status_rollup(&self) -> Output {
        let environment = self.directory.child("home");
        fs::create_dir_all(&environment).expect("create isolated subprocess home");
        let mut command = Command::new(env!("CARGO_BIN_EXE_synology-drive-sync"));
        command
            .args([
                "--output",
                "json",
                "status-rollup",
                "--status-cache",
                text(&self.cache),
            ])
            .env("HOME", &environment)
            .env("USERPROFILE", &environment)
            .env("NO_COLOR", "1")
            .stdin(Stdio::null());
        for (name, _) in std::env::vars_os() {
            if name.to_string_lossy().starts_with("SDSYNC_") {
                command.env_remove(name);
            }
        }
        command
            .output()
            .expect("run synology-drive-sync status-rollup")
    }

    /// The state block of the single stored rollup.
    fn rollup_totals(&self) -> Value {
        self.rollup()["state"].clone()
    }

    fn rollup(&self) -> Value {
        serde_json::from_str(&fs::read_to_string(self.rollup_path()).expect("rollup is readable"))
            .expect("rollup is JSON")
    }
}

fn in_sync_files(document: &Value) -> u64 {
    document["stats"]["in_sync_files"]
        .as_u64()
        .expect("in-sync count")
}

/// The whole flow in one test: the allocation of round trips is observed cumulatively, so splitting
/// this across tests sharing one mock would make each one's baseline depend on the others.
#[test]
fn a_warm_status_reuses_digests_removes_round_trips_and_reports_its_own_staleness() {
    let fixture = Fixture::new();

    // --- Cold: nothing stored yet, so every digest is obtained live. ---
    let cold = fixture.cached_status(&[]);
    let cold_calls = fixture.md5_calls();
    assert_eq!(
        cold["cache"]["state"], "cold",
        "the first run has nothing to reuse: {}",
        cold["cache"]
    );
    assert_eq!(cold["cache"]["entries_reused"], 0);
    assert_eq!(in_sync_files(&cold), FILES as u64);
    assert!(
        cold_calls >= FILES,
        "a cold content comparison must ask File Station for every digest, saw {cold_calls}"
    );

    // --- Warm: nothing changed, so no digest need be obtained at all. ---
    let warm = fixture.cached_status(&["--status-cache-canary", "0"]);
    let warm_calls = fixture.md5_calls() - cold_calls;
    assert_eq!(
        warm["cache"]["state"], "warm",
        "the second run should have reused stored evidence: {}",
        warm["cache"]
    );
    assert_eq!(
        warm["cache"]["entries_reused"], FILES as u64,
        "every unchanged file should have been served from the cache"
    );
    assert_eq!(
        warm["cache"]["entries_verified_live"], 0,
        "nothing changed, so nothing needed recomputing"
    );
    assert_eq!(
        warm_calls, 0,
        "a warm pass asked File Station for {warm_calls} MD5 operations; it should ask for none"
    );
    assert!(
        warm["cache"]["oldest_evidence_epoch"].is_i64(),
        "a warm answer must say how old the oldest evidence behind it is: {}",
        warm["cache"]
    );
    println!(
        "measured {cold_calls} File Station MD5 requests for {FILES} files cold, {warm_calls} warm"
    );

    // The answer itself is unchanged. A cache that alters the verdict is a broken cache.
    assert_eq!(
        warm["stats"], cold["stats"],
        "the cached answer disagreed with the computed one"
    );
    assert_eq!(warm["entries"], cold["entries"]);

    // --- Refresh: stored evidence deliberately ignored, and the work happens again. ---
    let before_refresh = fixture.md5_calls();
    let refreshed = fixture.cached_status(&["--status-cache-refresh"]);
    let refresh_calls = fixture.md5_calls() - before_refresh;
    assert_eq!(refreshed["cache"]["state"], "refreshed");
    assert_eq!(refreshed["cache"]["entries_reused"], 0);
    assert!(
        refresh_calls >= FILES,
        "a refresh must recompute every digest, saw {refresh_calls}"
    );
    assert_eq!(in_sync_files(&refreshed), FILES as u64);

    // --- A changed file leaves the cache and is compared live. ---
    let changed = fixture.source.join("payload-0.bin");
    fs::write(&changed, b"rewritten with different bytes entirely").expect("rewrite a source file");
    let after_change = fixture.cached_status(&["--status-cache-canary", "0"]);
    assert_eq!(
        in_sync_files(&after_change),
        (FILES - 1) as u64,
        "the rewritten file must be reported as no longer in sync"
    );
    assert_eq!(
        after_change["cache"]["entries_reused"],
        (FILES - 1) as u64,
        "only the unchanged files should have been served from the cache"
    );

    // --- Deleting the cache changes the answer not at all, only the work. ---
    fs::remove_dir_all(&fixture.cache).expect("remove the cache directory");
    fs::create_dir(&fixture.cache).expect("recreate an empty cache directory");
    let uncached = fixture.cached_status(&[]);
    assert_eq!(uncached["cache"]["state"], "cold");
    assert_eq!(
        uncached["stats"], after_change["stats"],
        "deleting the cache must cost time and nothing else"
    );
}

/// The rollup is written beside the cache, describes the whole profile, and separates what is in
/// sync now from anything a run transferred.
#[test]
fn an_unscoped_pass_writes_a_rollup_and_a_scoped_one_does_not() {
    let fixture = Fixture::new();
    fixture.cached_status(&[]);

    let rollup = fixture.rollup();
    assert_eq!(rollup["schema"], "sdsync.status-rollup.v1");
    // Named, but not asserted to a particular value: which key a configuration-free invocation
    // resolves to is not what this test is about.
    assert!(
        rollup["profile"]
            .as_str()
            .is_some_and(|name| !name.is_empty()),
        "the rollup must say which profile it describes: {rollup}"
    );
    assert_eq!(rollup["compare"], "content");
    assert_eq!(rollup["state"]["in_sync"]["files"], FILES as u64);
    assert_eq!(rollup["state"]["would_transfer"]["files"], 0);
    assert_eq!(rollup["state"]["would_transfer"]["bytes"], 0);
    assert_eq!(rollup["observation"]["complete"], true);
    assert!(rollup["observed_at_epoch"].is_i64());

    // "Synced" means both "in sync now" and "transferred by the last run", so it names neither.
    let text = fs::read_to_string(fixture.rollup_path()).expect("readable");
    assert!(
        !text.contains("synced"),
        "the rollup must not use a word that means two different quantities: {text}"
    );

    // A scoped pass sees only a subtree, so its totals are not the profile's and must not replace
    // them. The rollup keeps the figures from the unscoped pass above.
    let scoped = fixture.cached_status(&["--scope", "payload-1.bin"]);
    assert_eq!(scoped["scope"], "payload-1.bin");
    let after_scoped = fixture.rollup();
    assert_eq!(
        after_scoped["state"]["in_sync"]["files"], FILES as u64,
        "a scoped pass overwrote the whole-profile rollup with a subtree's totals"
    );
}

/// Without the flag the command behaves exactly as it did before the cache existed, and says so.
#[test]
fn status_without_the_flag_stores_nothing_and_reports_the_cache_as_off() {
    let fixture = Fixture::new();
    let output = fixture.status(&[]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "status failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let document: Value = serde_json::from_slice(&output.stdout).expect("one JSON document");
    assert_eq!(document["cache"]["state"], "off");
    assert_eq!(document["cache"]["entries_reused"], 0);
    assert!(document["cache"]["oldest_evidence_epoch"].is_null());
    assert!(
        fs::read_dir(&fixture.cache)
            .expect("cache directory is readable")
            .next()
            .is_none(),
        "no cache directory was configured, so nothing may have been written to one"
    );
}

/// The property the whole feature rests on: rendering the stored totals touches nothing else.
///
/// The user's ask was an instantaneous view of what needs attention. That is only answerable if
/// opening the dashboard reads a few small documents and stops — no local walk, no File Station
/// call, no content digest. A "small" lookup added to this path later would cost a tree walk per
/// dashboard poll and nobody would notice until a NAS got slow, so it is pinned here rather than
/// left as an intention.
///
/// The test proves it by removing the evidence: the entire source tree is deleted and the server is
/// told to expect nothing. A read path that consulted either would fail or change its answer.
#[test]
fn rendering_the_stored_totals_reads_no_source_file_and_makes_no_request() {
    let fixture = Fixture::new();
    fixture.cached_status(&[]);

    let before = fixture.rollup_totals();
    let requests_before = fixture.server.requests().len();

    // Take away everything the read path must not be using.
    fs::remove_dir_all(&fixture.source).expect("remove the source tree");

    let output = fixture.status_rollup();
    assert_eq!(
        output.status.code(),
        Some(0),
        "rendering stored totals must not need the source tree; it failed once the tree was \
         removed, which means the read path is reading something it should not:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let after: Value =
        serde_json::from_slice(&output.stdout).expect("status-rollup stdout is one JSON document");
    let after_totals = after["profiles"][0]["state"].clone();

    assert_eq!(
        after_totals, before,
        "the stored totals changed after the source tree was deleted. The aggregate read path must \
         render what the last pass recorded and consult nothing else -- if it now walks or stats \
         the source, every dashboard poll costs a tree walk."
    );
    assert_eq!(
        fixture.server.requests().len(),
        requests_before,
        "rendering stored totals made a File Station request. This path must be answerable with the \
         NAS unreachable; if it is not, the dashboard cannot open quickly and cannot open at all \
         while the NAS is busy."
    );
    assert_eq!(
        after["schema"], "sdsync.status-rollup-aggregate.v1",
        "the aggregate must identify its own schema"
    );
}

/// The combined figure is withheld when profiles overlap, and the reason is carried so a reader can
/// show it. A missing row would invite the user to add the per-profile numbers by hand and reach the
/// same wrong answer the withholding exists to prevent.
#[test]
fn an_overlapping_pair_of_profiles_withholds_the_combined_total_visibly() {
    let fixture = Fixture::new();
    fixture.cached_status(&[]);

    // A second profile covering a subtree of the first. Written directly because provoking it
    // through two configured profiles would be testing configuration, not composition.
    let first = fixture.rollup_path();
    let mut document: Value =
        serde_json::from_str(&fs::read_to_string(&first).expect("readable")).expect("json");
    let source = document["source"].as_str().expect("source").to_owned();
    let remote = document["remote"].as_str().expect("remote").to_owned();
    document["profile"] = Value::from("overlapping");
    document["source"] = Value::from(format!("{source}/nested"));
    document["remote"] = Value::from(format!("{remote}/nested"));
    fs::write(
        fixture.cache.join("overlapping.rollup.json"),
        serde_json::to_string(&document).expect("serialize"),
    )
    .expect("write the overlapping rollup");

    let output = fixture.status_rollup();
    assert_eq!(output.status.code(), Some(0));
    let aggregate: Value = serde_json::from_slice(&output.stdout).expect("one JSON document");

    assert_eq!(aggregate["overlapping_profiles"], true);
    assert!(
        aggregate["total"].is_null(),
        "overlapping trees would be counted twice, so no combined total may be offered: {aggregate}"
    );
    let reason = aggregate["total_unavailable_reason"]
        .as_str()
        .expect("a withheld total must carry a reason a reader can show");
    assert!(reason.contains("overlapping"), "unhelpful reason: {reason}");
    assert_eq!(
        aggregate["profiles"].as_array().map(Vec::len),
        Some(2),
        "the per-profile rows survive; only their combination is unsound"
    );
}
