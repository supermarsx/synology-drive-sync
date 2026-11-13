//! Control-thread tests with a fake launcher, mutation runner and change source.
//! Portable: they run on every CI platform.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use super::super::changes::FakeChangeSource;
use super::super::events::MemorySink;
use super::super::executor::Ending;
use super::super::window::FixedOffsetClock;
use super::*;

/// How a fake run behaves.
#[derive(Clone, Copy)]
struct Plan {
    /// Exit on its own after this long (`None`: run until stopped).
    finish_after: Option<Duration>,
    exit: i32,
    /// When stopped, take this long before exiting (beyond the grace: forced).
    stop_takes: Duration,
}

#[derive(Default)]
struct World {
    launched: Vec<RunSpec>,
    running: usize,
    max_running: usize,
    /// A 137 exit reports the cgroup's OOM counter as moved.
    oom_proven: bool,
    /// An exit-0 run carries the result record of a sync that left nothing
    /// undone (the daemon's evidence that it converged).
    converged_record: bool,
}

struct FakeLauncher {
    world: Arc<Mutex<World>>,
    plan: Arc<Mutex<Plan>>,
    grace: Duration,
}

struct FakeControl {
    stop: Sender<StopCause>,
}

impl RunControl for FakeControl {
    fn stop(&self, cause: StopCause) {
        let _ = self.stop.send(cause);
    }
}

impl Launcher for FakeLauncher {
    fn launch(
        &self,
        spec: RunSpec,
        sink: Box<dyn Fn(ExecEvent) + Send>,
    ) -> io::Result<Box<dyn RunControl>> {
        let plan = *self.plan.lock().unwrap();
        {
            let mut world = self.world.lock().unwrap();
            world.launched.push(spec);
            world.running += 1;
            world.max_running = world.max_running.max(world.running);
        }
        let (stop, requests) = mpsc::channel();
        let world = Arc::clone(&self.world);
        let grace = self.grace;
        thread::spawn(move || {
            sink(ExecEvent::Started { pid: 4242 });
            let wait = plan.finish_after.unwrap_or(Duration::from_secs(3600));
            let (exit, ending) = match requests.recv_timeout(wait) {
                Ok(cause) => {
                    thread::sleep(plan.stop_takes);
                    let forced = plan.stop_takes >= grace;
                    (
                        if forced { 137 } else { 130 },
                        Ending::Stopped { cause, forced },
                    )
                }
                Err(_) => (plan.exit, Ending::Exited),
            };
            let (oom, converged_record) = {
                let mut world = world.lock().unwrap();
                world.running -= 1;
                (world.oom_proven && exit == 137, world.converged_record)
            };
            sink(ExecEvent::Finished(Box::new(RunReport {
                exit,
                ending,
                stderr_tail: if exit == 0 {
                    Vec::new()
                } else {
                    b"error: fake failure\n".to_vec()
                },
                records: if converged_record && exit == 0 {
                    vec![super::super::executor::JobRecord {
                        profile: "docs".into(),
                        status: "ok".into(),
                        deferred: Some(0),
                        skipped: Some(0),
                        facts: super::super::state::ResultFacts {
                            incomplete_listings: Some(0),
                            link_guard_unchecked: Some(0),
                            ..Default::default()
                        },
                        ..Default::default()
                    }]
                } else {
                    Vec::new()
                },
                record_error: None,
                oom_proven: oom.then_some(true),
                memory_note: None,
                audit_error: None,
                last_phase: None,
            })));
        });
        Ok(Box::new(FakeControl { stop }))
    }
}

struct SlowMutations {
    calls: AtomicUsize,
}

impl MutationRunner for SlowMutations {
    fn run(&self, mutation: &Mutation, _deadline: Duration) -> MutationOutcome {
        self.calls.fetch_add(1, Ordering::SeqCst);
        thread::sleep(Duration::from_millis(50));
        MutationOutcome {
            exit: 0,
            detail: format!("{} saved", mutation.op),
        }
    }
}

struct Harness {
    root: PathBuf,
    var: PathBuf,
    tx: Sender<Msg>,
    handle: Option<thread::JoinHandle<()>>,
    world: Arc<Mutex<World>>,
    plan: Arc<Mutex<Plan>>,
    sink: Arc<MemorySink>,
    wall: Arc<AtomicU64>,
    changes: FakeChangeSource,
    mutations: Arc<SlowMutations>,
    alerts: Arc<RecordingAlerts>,
}

/// Alerts the control thread asked for, in order.
#[derive(Default)]
struct RecordingAlerts {
    calls: Mutex<Vec<String>>,
}

impl super::super::alerts::Alerts for RecordingAlerts {
    fn run_result(&self, scope: &str, failed: bool, exit: i32) {
        self.calls
            .lock()
            .unwrap()
            .push(format!("result {scope} failed={failed} exit={exit}"));
    }
    fn escalate(&self, profile: &str, exit: i32) {
        self.calls
            .lock()
            .unwrap()
            .push(format!("escalate {profile} exit={exit}"));
    }
}

static NEXT: AtomicU64 = AtomicU64::new(0);

const T0: u64 = 1_780_000_000;

/// How long a test that waits on a real debounce (a second of wall clock) or a real
/// run waits for it. A wait only costs this long when it is about to fail, and the
/// control thread of a loaded runner (twelve busy loops beside the whole suite)
/// has been seen not to log its first line within five seconds: the waits in
/// `failed_partial_busy_cancelled_runs_keep_the_change_pending` were that flake.
const PATIENT: Duration = Duration::from_secs(30);
// Why 30 s and not 5: the control thread's own state writes are bounded at
// STATE_WRITE_DEADLINE (10 s) because an fsync can legitimately stall that long on a
// loaded runner (macOS fsync is F_FULLFSYNC), so a harness that gives up on the
// thread after 5 s mistakes the contract's slowest case for a hang. Measured on Linux
// beside twelve busy loops on three cpus, every wait but the deliberate ones (a real
// debounce) ends in under 400 ms, so the budget costs nothing unless a test fails.
// What the budget is NOT for: a wait that polls `launched() == N` can be overshot by a
// second launch (the routine a lifted block releases): count with `>=`.

fn grace() -> Duration {
    Duration::from_millis(300)
}

impl Harness {
    fn new(label: &str, setup: impl FnOnce(&Path, &Path)) -> Self {
        Self::tuned(label, setup, |_| {})
    }

    /// [`Harness::new`] with the settings adjusted before the daemon starts.
    fn tuned(
        label: &str,
        setup: impl FnOnce(&Path, &Path),
        tune: impl FnOnce(&mut Settings),
    ) -> Self {
        let root = std::env::temp_dir().join(format!(
            "sdsyncd-control-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = fs::remove_dir_all(&root);
        let home = root.join("home");
        let var = root.join("var");
        // Shaped like DSM: `home` and `var` are the framework's symlinks to
        // `@apphome` and `@appdata`, which is what the scripts pass the daemon.
        #[cfg(unix)]
        {
            for (alias, real) in [("home", "@apphome"), ("var", "@appdata")] {
                fs::create_dir_all(root.join(real)).unwrap();
                std::os::unix::fs::symlink(root.join(real), root.join(alias)).unwrap();
            }
        }
        for dir in [
            "home/config/profiles.d",
            "home/config/routines.d",
            "var/state/routines",
            "var/state/profiles",
            "var/run",
            "var/log",
        ] {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
        setup(&home, &var);
        let world = Arc::new(Mutex::new(World::default()));
        let plan = Arc::new(Mutex::new(Plan {
            finish_after: Some(Duration::from_millis(30)),
            exit: 0,
            stop_takes: Duration::from_millis(10),
        }));
        let sink = Arc::new(MemorySink::default());
        let wall = Arc::new(AtomicU64::new(T0));
        let changes = FakeChangeSource::default();
        let alerts = Arc::new(RecordingAlerts::default());
        let mutations = Arc::new(SlowMutations {
            calls: AtomicUsize::new(0),
        });
        let mut settings = Settings::default();
        settings.timings.cancel_grace = grace();
        settings.timings.kill_wait = Duration::from_millis(200);
        settings.wake_cap = Duration::from_millis(100);
        settings.publish_run_lock = false;
        // Rows are asserted as soon as they are due; one test sets a grace.
        settings.observe_grace = Duration::ZERO;
        tune(&mut settings);
        let wall_reader = Arc::clone(&wall);
        let parts = Parts {
            paths: Paths::layout(&home, &var, PathBuf::from("/fake/synology-drive-sync")),
            settings,
            launcher: Box::new(FakeLauncher {
                world: Arc::clone(&world),
                plan: Arc::clone(&plan),
                grace: grace(),
            }),
            mutations: Arc::clone(&mutations) as Arc<dyn MutationRunner>,
            audit: Arc::new(super::super::audit::NoAudit),
            alerts: Arc::clone(&alerts) as Arc<dyn super::super::alerts::Alerts>,
            changes: Some({
                let source = changes.clone();
                Box::new(move |_sink| Box::new(source) as Box<dyn ChangeSource>)
            }),
            sink: Box::new(Arc::clone(&sink)),
            clock: Box::new(FixedOffsetClock { offset_seconds: 0 }),
            wall: Box::new(move || Some(wall_reader.load(Ordering::SeqCst))),
            pid: 777,
        };
        let (tx, handle) = spawn(parts).unwrap();
        Self {
            root,
            var,
            tx,
            handle: Some(handle),
            world,
            plan,
            sink,
            wall,
            changes,
            mutations,
            alerts,
        }
    }

    fn ask(&self, request: Request) -> Value {
        ask(&self.tx, request, PATIENT)
    }

    fn timed(&self, request: Request) -> (Value, Duration) {
        let start = Instant::now();
        let value = self.ask(request);
        (value, start.elapsed())
    }

    fn launched(&self) -> usize {
        self.world.lock().unwrap().launched.len()
    }

    fn wait_for(&self, what: &str, within: Duration, done: impl Fn(&Self) -> bool) {
        let deadline = Instant::now() + within;
        while !done(self) {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}; events: {:?}",
                self.logs()
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn idle(&self) -> bool {
        let status = self.ask(Request::Status);
        status["active"].is_null() && status["queue"].as_array().is_some_and(Vec::is_empty)
    }

    fn logs(&self) -> Vec<String> {
        self.sink
            .logs
            .lock()
            .unwrap()
            .iter()
            .map(|e| format!("{} {}", e.event, e.detail))
            .collect()
    }

    fn state(&self, rel: &str) -> String {
        fs::read_to_string(self.var.join("state").join(rel)).unwrap_or_default()
    }

    fn advance(&self, seconds: u64) {
        self.wall.fetch_add(seconds, Ordering::SeqCst);
        let _ = self.tx.send(Msg::Signal(12));
    }

    fn stop(&mut self) -> Value {
        let answer = ask(&self.tx, Request::Stop, Duration::from_secs(10));
        if let Some(handle) = self.handle.take() {
            handle.join().unwrap();
        }
        answer
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        if self.handle.is_some() {
            let _ = self.stop();
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// The shipped manager's `[profiles.<name>]` table, as `configure-profile`
/// renders it, with `@ROOT@`, `@SOURCE@` and `@NAME@` left to fill. The e2e test
/// `the_unit_harness_template_is_what_the_real_manager_renders` re-renders it
/// with the real shell manager and fails when the two drift apart, so the
/// fragments below are the manager's, not this file's idea of them.
const MANAGER_PROFILE: &str = include_str!("testdata/manager-profile.toml");

/// Where a DSM volume source for `name` sits in this harness (the manager admits
/// sources only under a volume; the fake launcher never touches the directory).
fn volume_source(root: &Path, name: &str) -> PathBuf {
    root.join("sysroot/volume1").join(name)
}

/// A path as TOML text and as the daemon reads it back: forward slashes on every
/// platform (a backslash-u in a TOML basic string is not a path).
fn slashed(path: &Path) -> String {
    path.display().to_string().replace('\\', "/")
}

/// The package root a `home` or `var` path hangs off.
fn package_root(dir: &Path) -> &Path {
    dir.parent()
        .expect("home and var sit under the package root")
}

/// The manager's fragment for `name`, with `settings` (`key`, TOML value text)
/// replacing the manager's value for that key. A key the manager does not
/// render is a test bug and panics, so a setting can never be silently dropped.
fn fragment(root: &Path, name: &str, settings: &[(&str, &str)]) -> String {
    let source = volume_source(root, name);
    let mut text = MANAGER_PROFILE
        .replace("@SOURCE@", &slashed(&source))
        .replace("@ROOT@", &slashed(root))
        .replace("@NAME@", name);
    for (key, value) in settings {
        let prefix = format!("{key} = ");
        let mut found = false;
        text = text
            .lines()
            .map(|line| {
                if line.starts_with(&prefix) {
                    found = true;
                    format!("{prefix}{value}")
                } else {
                    line.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        assert!(found, "the manager's fragment has no `{key}` line:\n{text}");
    }
    text
}

fn profile(home: &Path, name: &str) {
    profile_with(home, name, &[]);
}

fn profile_with(home: &Path, name: &str, settings: &[(&str, &str)]) {
    fs::write(
        home.join(format!("config/profiles.d/{name}.toml")),
        fragment(package_root(home), name, settings),
    )
    .unwrap();
}

/// The shipped manager's `routines.d/<name>.conf`, as `configure-routine --profile
/// <name>` renders it: every key it writes, in its order, with its defaults. The e2e
/// test `the_unit_harness_routine_template_is_what_the_real_manager_renders`
/// (tests/routine_template_drift.rs) re-renders it with the real shell manager, in
/// every mode, and fails when the two drift apart.
const MANAGER_ROUTINE: &str = include_str!("testdata/manager-routine.conf");

/// Write `routines.d/<name>.conf` as the manager would: its whole key set, with
/// `conf` (`key=value` lines) replacing the manager's value for each key it names.
/// A key the manager does not write is a test bug and panics, so a setting can
/// never be silently dropped, and a conf of three or four keys that no manager
/// ever produced cannot hide what the daemon does with a real one.
fn routine(home: &Path, name: &str, conf: &str) {
    let mut wanted: Vec<(&str, &str)> = conf
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| {
            line.split_once('=')
                .unwrap_or_else(|| panic!("`{line}` is not a key=value line"))
        })
        .collect();
    let mut text = String::new();
    for line in MANAGER_ROUTINE.lines() {
        let key = line.split_once('=').map_or(line, |(key, _)| key);
        match wanted.iter().position(|(wanted, _)| *wanted == key) {
            Some(at) => {
                let (key, value) = wanted.remove(at);
                text.push_str(&format!("{key}={value}\n"));
            }
            None => {
                text.push_str(line);
                text.push('\n');
            }
        }
    }
    assert!(
        wanted.is_empty(),
        "the manager's routine has no such key (or it is given twice): {wanted:?}\n{MANAGER_ROUTINE}"
    );
    fs::write(home.join(format!("config/routines.d/{name}.conf")), text).unwrap();
}

/// The harness writes the manager's whole routine, overrides applied in place, and
/// refuses a key the manager does not write.
#[test]
fn the_harness_writes_routines_with_the_managers_key_set() {
    let root = std::env::temp_dir().join(format!(
        "sdsyncd-routine-keys-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    ));
    let home = root.join("home");
    fs::create_dir_all(home.join("config/routines.d")).unwrap();
    routine(
        &home,
        "alpha",
        "enabled=false\nmode=realtime\ndebounce_seconds=1\n",
    );
    let written = fs::read_to_string(home.join("config/routines.d/alpha.conf")).unwrap();
    let keys = |text: &str| -> Vec<String> {
        text.lines()
            .map(|l| l.split_once('=').map_or(l, |(k, _)| k).to_owned())
            .collect()
    };
    assert_eq!(keys(&written), keys(MANAGER_ROUTINE), "{written}");
    assert!(
        written.contains("enabled=false\n")
            && written.contains("mode=realtime\n")
            && written.contains("debounce_seconds=1\n")
            && written.contains("retry_count=5\n"),
        "overrides in place, the manager's defaults elsewhere:\n{written}"
    );
    let refused = std::panic::catch_unwind(|| routine(&home, "beta", "enabled=true\nbogus=1\n"));
    fs::remove_dir_all(&root).unwrap();
    assert!(
        refused.is_err(),
        "a key the manager does not write was accepted"
    );
}

/// Replace `path` whole, as the shipped manager does (a private temporary beside
/// it, then a rename). A running daemon reads these documents on its own
/// schedule: a plain `fs::write` truncates first, so a read in between sees a
/// torn document (the cause of a flake in the re-arm test).
fn write_atomic(path: &Path, contents: impl AsRef<[u8]>) {
    let name = path
        .file_name()
        .unwrap_or_else(|| panic!("{} has no file name", path.display()))
        .to_string_lossy();
    let temporary = path.with_file_name(format!(".{name}.tmp{}", std::process::id()));
    fs::write(&temporary, contents)
        .unwrap_or_else(|e| panic!("write {}: {e}", temporary.display()));
    fs::rename(&temporary, path).unwrap_or_else(|e| {
        panic!(
            "rename {} over {}: {e}",
            temporary.display(),
            path.display()
        )
    });
}

/// The property the re-arm flake lacked: a reader that opens the path at any moment sees
/// one whole document, never an empty or half-written file. (With `fs::write`, which
/// truncates and then writes, the same reader sees both within a few hundred rewrites.)
#[test]
fn write_atomic_never_shows_a_reader_a_torn_document() {
    let dir = std::env::temp_dir().join(format!(
        "sdsyncd-atomic-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    ));
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("fragment.toml");
    let documents = [
        format!("# a\n{}", "a = 1\n".repeat(2000)),
        format!("# b\n{}", "b = 2\n".repeat(3000)),
    ];
    write_atomic(&path, &documents[0]);
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let reader = {
        let (path, stop, documents) = (path.clone(), Arc::clone(&stop), documents.clone());
        thread::spawn(move || {
            let mut torn = Vec::new();
            let mut reads = 0_u64;
            while !stop.load(Ordering::SeqCst) {
                let text = fs::read_to_string(&path)
                    .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
                reads += 1;
                if !documents.contains(&text) && torn.len() < 3 {
                    torn.push(format!(
                        "{} bytes starting {:?}",
                        text.len(),
                        &text[..text.len().min(12)]
                    ));
                }
            }
            (reads, torn)
        })
    };
    for round in 0..400 {
        write_atomic(&path, &documents[round % 2]);
    }
    stop.store(true, Ordering::SeqCst);
    let (reads, torn) = reader.join().unwrap();
    fs::remove_dir_all(&dir).unwrap();
    assert!(reads > 0, "the reader never ran");
    assert!(
        torn.is_empty(),
        "{reads} reads, torn documents seen: {torn:?}"
    );
}

fn run(scope: &str) -> Request {
    Request::Run {
        scope: scope.into(),
        action: Action::Sync,
        allow_delete: false,
        max_total_delete: None,
    }
}

/// I-1: routine, schedule and requested runs share one slot and never overlap.
#[test]
fn two_runs_never_overlap_across_routine_dashboard_and_cli() {
    let h = Harness::new("overlap", |home, var| {
        profile(home, "alpha");
        profile(home, "beta");
        routine(
            home,
            "alpha",
            "enabled=true\nmode=interval\ninterval_seconds=60\n",
        );
        // Already due.
        fs::write(
            var.join("state/routines/alpha.state"),
            format!("next_run_epoch={}\n", T0 - 5),
        )
        .unwrap();
        fs::write(
            home.join("config/schedule.conf"),
            "enabled=true\ninterval_seconds=60\n",
        )
        .unwrap();
        fs::write(
            var.join("state/schedule.state"),
            format!("next_run_epoch={}\n", T0 + 1),
        )
        .unwrap();
    });
    *h.plan.lock().unwrap() = Plan {
        finish_after: Some(Duration::from_millis(80)),
        exit: 0,
        stop_takes: Duration::ZERO,
    };
    assert_eq!(h.ask(run("beta"))["ok"], true);
    assert_eq!(h.ask(run("alpha"))["ok"], true);
    h.advance(2);
    h.wait_for("four runs", Duration::from_secs(10), |h| {
        h.launched() >= 4 && h.idle()
    });
    let world = h.world.lock().unwrap();
    assert_eq!(world.max_running, 1, "I-1");
    let argv: Vec<String> = world
        .launched
        .iter()
        .map(|spec| {
            spec.args
                .iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect();
    assert!(
        argv.iter()
            .any(|a| a.contains("--profiles alpha,beta --max-total-delete 100 --no-delete")),
        "{argv:?}"
    );
    // F5: an all run names the profiles run.scope lists; --all-profiles would
    // also sync a profile saved while it ran, outside its published scope.
    assert!(
        argv.iter().all(|a| !a.contains("--all-profiles")),
        "{argv:?}"
    );
    assert!(
        argv.iter()
            .filter(|a| a.contains("--profile alpha"))
            .count()
            >= 2,
        "{argv:?}"
    );
    assert!(argv.iter().all(|a| a.starts_with("--config ")));
}

/// H6: the core publishes its progress record and the watchdog reads it:
/// network IO does not move rchar/wchar, so I/O counters alone call a busy
/// upload stalled.
#[test]
fn runs_publish_a_progress_record_the_watchdog_reads() {
    let h = Harness::new("progress", |home, _| profile(home, "alpha"));
    assert_eq!(h.ask(run("alpha"))["ok"], true);
    h.wait_for("the run", PATIENT, |h| h.launched() >= 1);
    let world = h.world.lock().unwrap();
    let spec = &world.launched[0];
    let record = spec
        .progress_record
        .clone()
        .expect("a progress record for the watchdog");
    let args: Vec<String> = spec
        .args
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let at = args
        .iter()
        .position(|a| a == "--progress-record")
        .expect("the core is told where to write it");
    assert_eq!(args[at + 1], record.display().to_string());
    // The core's own naming rule: <request id>.<job id>.json, lowercase hex.
    let name = record.file_name().unwrap().to_string_lossy().into_owned();
    let stem = name.strip_suffix(".json").unwrap();
    let (request, job) = stem.split_once('.').unwrap();
    for part in [request, job] {
        assert!(
            !part.is_empty()
                && part
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "{name}"
        );
    }
}

/// The hang fix: status answers in under a second and a mutation completes in
/// under two while a run is in flight; only removing the running profile is
/// refused (R6).
#[test]
fn ctl_answers_and_mutations_complete_while_a_sync_runs() {
    let h = Harness::new("busy", |home, _| {
        profile(home, "alpha");
        profile(home, "beta");
    });
    *h.plan.lock().unwrap() = Plan {
        finish_after: None,
        exit: 0,
        stop_takes: Duration::ZERO,
    };
    assert_eq!(h.ask(run("alpha"))["ok"], true);
    h.wait_for("the run", PATIENT, |h| h.launched() >= 1);
    for _ in 0..20 {
        let (status, took) = h.timed(Request::Status);
        assert!(took < Duration::from_secs(1), "{took:?}");
        assert_eq!(status["active"]["scope"], "alpha");
        assert_eq!(status["busy_profiles"], json!(["alpha"]));
    }
    let (saved, took) = h.timed(Request::Mutate(Mutation {
        op: "configure-profile".into(),
        profile: Some("alpha".into()),
        args: vec![],
    }));
    assert_eq!(saved["ok"], true, "{saved}");
    assert!(took < Duration::from_secs(2), "{took:?}");
    let (refused, took) = h.timed(Request::Mutate(Mutation {
        op: "remove-profile".into(),
        profile: Some("alpha".into()),
        args: vec![],
    }));
    assert_eq!(refused["code"], "profile_running");
    assert_eq!(refused["exit"], 75);
    assert!(took < Duration::from_secs(1));
    let (other, _) = h.timed(Request::Mutate(Mutation {
        op: "remove-profile".into(),
        profile: Some("beta".into()),
        args: vec![],
    }));
    assert_eq!(other["ok"], true);
    assert_eq!(h.mutations.calls.load(Ordering::SeqCst), 2);
    assert_eq!(h.ask(Request::Cancel { id: None })["state"], "stopping");
    h.wait_for("the cancel", PATIENT, Harness::idle);
    let run_state = h.state("run.state");
    assert!(
        run_state.contains("state=failed\n")
            && run_state.contains("reason=cancelled\n")
            && run_state.contains("outcome=cancelled\n"),
        "{run_state}"
    );
}

/// Hang test at the loop level: a run that ignores TERM is KILLed after the
/// grace and recorded `cancelled`; stop completes within the budget (I-11).
#[test]
fn stop_within_budget_and_forced_kill_reads_cancelled() {
    let mut h = Harness::new("stop", |home, _| profile(home, "alpha"));
    *h.plan.lock().unwrap() = Plan {
        finish_after: None,
        exit: 0,
        stop_takes: grace() + Duration::from_millis(100),
    };
    assert_eq!(h.ask(run("alpha"))["ok"], true);
    h.wait_for("the run", PATIENT, |h| h.launched() >= 1);
    let start = Instant::now();
    let answer = h.stop();
    let took = start.elapsed();
    assert_eq!(answer["state"], "stopped", "{answer}");
    assert!(
        took < grace() + Duration::from_millis(200) + Duration::from_secs(5),
        "{took:?}"
    );
    let run_state = h.state("run.state");
    assert!(
        run_state.contains("state=failed\nstarted_epoch="),
        "{run_state}"
    );
    assert!(run_state.contains("exit_code=137\n"), "{run_state}");
    assert!(run_state.contains("reason=cancelled\n"), "{run_state}");
    assert!(run_state.contains("detail=the package stopped during the run; the core outlasted the cancel grace and was killed\n"), "{run_state}");
    assert!(h.state("controller.state").starts_with("state=stopped\n"));
}

/// I-12: once stopping, no new work is taken.
#[test]
fn no_new_run_after_term() {
    let h = Harness::new("term", |home, _| profile(home, "alpha"));
    *h.plan.lock().unwrap() = Plan {
        finish_after: None,
        exit: 0,
        stop_takes: Duration::from_millis(200),
    };
    assert_eq!(h.ask(run("alpha"))["ok"], true);
    h.wait_for("the run", PATIENT, |h| h.launched() >= 1);
    let _ = h.tx.send(Msg::Signal(15));
    let refused = h.ask(run("alpha"));
    // Either the refusal, or the control thread already finished stopping.
    assert!(
        refused["code"] == "stopping" || refused["code"] == "daemon_stopped",
        "{refused}"
    );
    thread::sleep(Duration::from_millis(400));
    assert_eq!(h.launched(), 1);
}

fn realtime_harness(label: &str) -> Harness {
    Harness::new(label, |home, _| {
        profile(home, "docs");
        routine(
            home,
            "docs",
            "enabled=true\nmode=realtime\ndebounce_seconds=1\nretry_backoff_seconds=10\n",
        );
    })
}

fn change(h: &Harness, dir: &str) {
    let _ = h.tx.send(Msg::Change(ChangeEvent::Changed {
        routine: "docs".into(),
        dir: dir.into(),
        depth: Depth::Shallow,
    }));
}

/// A realtime change is armed, debounced and dispatched as a scoped run with one
/// element per changed directory (shallow: its direct children).
#[test]
fn a_realtime_change_dispatches_a_scoped_run_after_its_debounce() {
    let h = realtime_harness("scope");
    let source = volume_source(&h.root, "docs");
    h.wait_for("arm", PATIENT, |h| {
        h.changes
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|c| c.starts_with(&format!("arm docs {} gen=", slashed(&source))))
    });
    let _ = h.tx.send(Msg::Change(ChangeEvent::Backend {
        routine: "docs".into(),
        backend: Backend::Inotify,
        reason: BackendReason::Ok,
        detail: "armed".into(),
    }));
    let start = Instant::now();
    change(&h, "Projects/2026/q4");
    change(&h, "Projects/2026/q3");
    h.wait_for("the dispatch", PATIENT, |h| h.launched() == 1);
    assert!(
        start.elapsed() >= Duration::from_millis(900),
        "debounced: {:?}",
        start.elapsed()
    );
    let args = h.world.lock().unwrap().launched[0]
        .args
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    // Two folders under one parent are two elements, not their common ancestor.
    let shallow: Vec<&str> = args
        .windows(2)
        .filter(|w| w[0] == "--scope-shallow")
        .map(|w| w[1].as_str())
        .collect();
    assert_eq!(
        shallow,
        ["Projects/2026/q3", "Projects/2026/q4"],
        "{args:?}"
    );
    assert!(!args.iter().any(|a| a == "--scope"), "{args:?}");
    h.wait_for("idle", PATIENT, Harness::idle);
    let state = h.state("routines/docs.state");
    assert!(
        state.contains("state=succeeded\n") && state.contains("backend=inotify\n"),
        "{state}"
    );
}

/// N-D5: the change source reports a new cause under the same backend and
/// reason (the walk deadline growing is exactly that), and it was logged only when
/// the backend or reason changed, so `walk_deadline_exceeded` never reached the
/// controller log the UI points at.
#[test]
fn a_new_backend_cause_under_the_same_reason_is_logged() {
    let h = realtime_harness("backend-detail");
    h.wait_for("arm", PATIENT, |h| armed_generation(h, "docs").is_some());
    let report = |detail: &str| {
        h.tx.send(Msg::Change(ChangeEvent::Backend {
            routine: "docs".into(),
            backend: Backend::Polling,
            reason: BackendReason::WatchSetupFailed,
            detail: detail.into(),
        }))
        .unwrap();
    };
    report("arm gave up");
    report(
        "arm gave up; walk_deadline_exceeded: the walk gave up after 120s in /x; \
         next_walk_deadline_seconds=240",
    );
    let logged = |h: &Harness, needle: &str| {
        h.logs()
            .iter()
            .any(|l| l.starts_with("routine_backend ") && l.contains(needle))
    };
    h.wait_for("the first cause", PATIENT, |h| logged(h, "arm gave up"));
    h.wait_for("the grown deadline", PATIENT, |h| {
        logged(h, "walk_deadline_exceeded")
    });
    let line = h
        .logs()
        .into_iter()
        .find(|l| l.contains("walk_deadline_exceeded"))
        .unwrap();
    assert!(
        line.contains("profile=docs")
            && line.contains("backend=polling")
            && line.contains("reason=watch_setup_failed")
            && line.contains("next_walk_deadline_seconds=240"),
        "{line}"
    );
}

/// I-3: a failed run keeps its change pending, and a change during a run is run
/// again after it.
#[test]
fn failed_partial_busy_cancelled_runs_keep_the_change_pending() {
    for exit in [1, 3, 75] {
        let h = realtime_harness(&format!("pending-{exit}"));
        *h.plan.lock().unwrap() = Plan {
            finish_after: Some(Duration::from_millis(50)),
            exit,
            stop_takes: Duration::ZERO,
        };
        change(&h, "a");
        h.wait_for("the dispatch", PATIENT, |h| h.launched() >= 1);
        h.wait_for("idle", PATIENT, Harness::idle);
        let status = h.ask(Request::Status);
        assert_eq!(
            status["routines"][0]["pending_change"], true,
            "exit {exit}: {status}"
        );
    }
    // Cancelled.
    let h = realtime_harness("pending-cancel");
    *h.plan.lock().unwrap() = Plan {
        finish_after: None,
        exit: 0,
        stop_takes: Duration::ZERO,
    };
    change(&h, "a");
    h.wait_for("the dispatch", PATIENT, |h| h.launched() >= 1);
    h.ask(Request::Cancel { id: None });
    h.wait_for("idle", PATIENT, Harness::idle);
    assert_eq!(
        h.ask(Request::Status)["routines"][0]["pending_change"],
        true
    );
    let state = h.state("routines/docs.state");
    assert!(
        state.contains("state=failed\n") && state.contains("last_reason=cancelled\n"),
        "{state}"
    );

    // A change during a successful run is run again afterwards.
    let h = realtime_harness("during");
    *h.plan.lock().unwrap() = Plan {
        finish_after: Some(Duration::from_millis(400)),
        exit: 0,
        stop_takes: Duration::ZERO,
    };
    change(&h, "a");
    h.wait_for("the first run", PATIENT, |h| h.launched() >= 1);
    change(&h, "b");
    h.wait_for("the second run", PATIENT, |h| h.launched() == 2);
    let args = h.world.lock().unwrap().launched[1]
        .args
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    assert!(
        args.windows(2)
            .any(|w| w[0] == "--scope-shallow" && w[1] == "b"),
        "{args:?}"
    );
}

/// Overflow means the whole tree: no `--scope`.
#[test]
fn an_overflow_runs_the_whole_tree() {
    let h = realtime_harness("overflow");
    change(&h, "a/b");
    let _ =
        h.tx.send(Msg::Change(ChangeEvent::Overflow { routine: None }));
    h.wait_for("the dispatch", PATIENT, |h| h.launched() == 1);
    let args = h.world.lock().unwrap().launched[0]
        .args
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    assert!(!args.iter().any(|a| a == "--scope"), "{args:?}");
    assert!(
        h.logs()
            .iter()
            .any(|l| l.starts_with("routine_watch_overflow"))
    );
}

/// Every failure is named in the log and in state (I-16): a failed run names its
/// cause in run.state, the profile record and Activity.
#[test]
fn every_failure_has_log_and_state() {
    let h = Harness::new("failure", |home, _| profile(home, "alpha"));
    *h.plan.lock().unwrap() = Plan {
        finish_after: Some(Duration::from_millis(20)),
        exit: 1,
        stop_takes: Duration::ZERO,
    };
    h.ask(run("alpha"));
    h.wait_for("the run", PATIENT, |h| h.launched() == 1 && h.idle());
    let run_state = h.state("run.state");
    assert!(
        run_state.contains("reason=core_failed\ndetail=error: fake failure\n"),
        "{run_state}"
    );
    let profile_state = h.state("profiles/alpha.state");
    assert!(
        profile_state.contains("state=failed\n")
            && profile_state.contains("detail=error: fake failure\n"),
        "{profile_state}"
    );
    let rows = h.sink.rows.lock().unwrap();
    assert!(
        rows.iter()
            .any(|r| r.code == "run.failed" && r.message.contains("fake failure"))
    );
    drop(rows);
    assert!(
        h.logs()
            .iter()
            .any(|l| l.starts_with("run_finished") && l.contains("reason=core_failed"))
    );
}

/// A `running` run.state left by a dead daemon is reconciled at start.
#[test]
fn a_dead_daemons_running_record_is_reconciled() {
    let h = Harness::new("reconcile", |_, var| {
        fs::write(
            var.join("state/run.state"),
            "state=running\nstarted_epoch=5\nfinished_epoch=0\nexit_code=-1\nscope=all\noperation=sync\nreason=none\ndetail=\n",
        )
        .unwrap();
    });
    h.wait_for("reconcile", PATIENT, |h| {
        h.state("run.state").contains("reason=runner_died")
    });
    assert!(
        h.state("run.state")
            .starts_with("state=failed\nstarted_epoch=5\n")
    );
}

#[test]
fn unknown_scopes_and_ids_are_refused_by_name() {
    let h = Harness::new("refuse", |home, _| profile(home, "alpha"));
    assert_eq!(h.ask(run("nope"))["code"], "scope_not_found");
    assert_eq!(h.ask(run("../x"))["code"], "scope_not_found");
    assert_eq!(
        h.ask(Request::Cancel { id: Some(99) })["code"],
        "nothing_running"
    );
    assert_eq!(
        h.ask(Request::RoutineNow {
            profile: "alpha".into()
        })["code"],
        "routine_unknown"
    );
    assert_eq!(h.ask(Request::Ping)["ok"], true);
}

#[test]
fn an_invalid_routine_is_reported_once_per_field() {
    let h = Harness::new("skip", |home, _| {
        profile(home, "alpha");
        routine(home, "alpha", "enabled=true\nmode=hourly\n");
    });
    // Wait for the first report (a loaded runner can take longer than any
    // fixed sleep to reach the first turn), then give later turns the chance
    // to repeat it.
    let reported = |h: &Harness| {
        h.logs()
            .iter()
            .any(|l| l.starts_with("routine_invalid profile=alpha field=mode"))
    };
    h.wait_for("the first report", Duration::from_secs(10), reported);
    // More turns (a reload would rightly report it again, so only wakes).
    let _ = h.ask(Request::Wake);
    let _ = h.ask(Request::Wake);
    thread::sleep(Duration::from_millis(400));
    let skips = h
        .logs()
        .iter()
        .filter(|l| l.starts_with("routine_invalid profile=alpha field=mode"))
        .count();
    assert_eq!(skips, 1);
    let rows = h
        .sink
        .rows
        .lock()
        .unwrap()
        .iter()
        .filter(|r| r.code == "routine.deferred")
        .count();
    assert_eq!(rows, 1);
}

#[test]
fn the_performance_level_sets_the_core_nice_value() {
    assert_eq!(performance_nice(Some(b"level=full\n")), 0);
    assert_eq!(performance_nice(Some(b"level=gentle\n")), 19);
    assert_eq!(performance_nice(Some(b"level=turbo\n")), 10);
    assert_eq!(performance_nice(None), 10);
}

// ------------------------------------------------------------ lasting blocks

/// `alpha`'s fragment as the manager renders it (`max-delete` 100 unless given).
fn alpha_fragment(root: &Path, settings: &[(&str, &str)]) -> String {
    fragment(root, "alpha", settings)
}

/// A block record for `alpha` tied to its current settings.
fn plant_block(var: &Path) {
    let alpha = alpha_fragment(package_root(var), &[]);
    let lasting = super::super::lasting::Lasting {
        reason: "core_oom_killed".into(),
        detail: "the sync core was killed by the out-of-memory killer (exit 137)".into(),
        since: T0 - 100,
        fingerprint: super::super::lasting::fingerprint(Some(alpha.as_bytes()), None),
    };
    fs::create_dir_all(var.join("state/lasting")).unwrap();
    fs::write(var.join("state/lasting/alpha.state"), lasting.render()).unwrap();
}

/// An interval routine of `alpha` that is already due.
fn due_alpha(home: &Path, var: &Path) {
    profile(home, "alpha");
    routine(
        home,
        "alpha",
        "enabled=true\nmode=interval\ninterval_seconds=60\n",
    );
    fs::write(
        var.join("state/routines/alpha.state"),
        format!("next_run_epoch={}\n", T0 - 5),
    )
    .unwrap();
}

fn suppressed_logs(h: &Harness) -> usize {
    h.logs()
        .iter()
        .filter(|l| l.starts_with("routine_suppressed profile=alpha"))
        .count()
}

/// A block survives a restart: the due routine is held, said once (not every
/// turn), and `ctl status` names it with how it clears.
#[test]
fn a_lasting_block_holds_the_routine_across_a_restart() {
    let h = Harness::new("lasting-restart", |home, var| {
        due_alpha(home, var);
        plant_block(var);
    });
    h.wait_for("the hold", PATIENT, |h| suppressed_logs(h) == 1);
    h.advance(1);
    h.advance(1);
    thread::sleep(Duration::from_millis(300));
    assert_eq!(h.launched(), 0, "a held routine ran");
    assert_eq!(suppressed_logs(&h), 1, "held once an hour, not every turn");
    let status = h.ask(Request::Status);
    assert_eq!(
        status["lasting"]["alpha"]["reason"], "core_oom_killed",
        "{status}"
    );
    assert_eq!(
        status["lasting"]["alpha"]["clears_on"],
        "run_now_or_settings_change"
    );
}

/// A save that changes nothing keeps the block; a real change lifts it.
#[test]
fn a_no_op_save_keeps_the_block_and_a_real_change_lifts_it() {
    let h = Harness::new("lasting-save", |home, var| {
        due_alpha(home, var);
        plant_block(var);
    });
    let fragment = h.root.join("home/config/profiles.d/alpha.toml");
    h.wait_for("the hold", PATIENT, |h| suppressed_logs(h) == 1);
    write_atomic(&fragment, alpha_fragment(&h.root, &[]));
    let _ = h.ask(Request::Reload);
    assert_eq!(
        h.ask(Request::Status)["lasting"]["alpha"]["reason"],
        "core_oom_killed"
    );
    assert!(h.var.join("state/lasting/alpha.state").exists());
    write_atomic(&fragment, alpha_fragment(&h.root, &[("max-delete", "50")]));
    let _ = h.ask(Request::Reload);
    h.wait_for("the run", PATIENT, |h| h.launched() >= 1);
    assert!(!h.var.join("state/lasting/alpha.state").exists());
    assert!(
        h.logs()
            .iter()
            .any(|l| l == "lasting_cleared profile=alpha cause=settings_changed"),
        "{:?}",
        h.logs()
    );
}

/// Run now lifts the block and runs; killed again, it records red and blocks
/// again, and the routine is held once more.
#[test]
fn run_now_lifts_the_block_and_a_second_kill_blocks_again() {
    let h = Harness::new("lasting-run-now", |home, var| {
        due_alpha(home, var);
        plant_block(var);
    });
    h.wait_for("the hold", PATIENT, |h| suppressed_logs(h) == 1);
    *h.plan.lock().unwrap() = Plan {
        finish_after: Some(Duration::from_millis(30)),
        exit: 137,
        stop_takes: Duration::ZERO,
    };
    h.world.lock().unwrap().oom_proven = true;
    assert_eq!(h.ask(run("alpha"))["ok"], true);
    h.wait_for("the run", PATIENT, |h| h.launched() >= 1);
    h.wait_for("idle", PATIENT, Harness::idle);
    assert_eq!(h.launched(), 1, "only Run now ran: the routine stayed held");
    let profile = h.state("profiles/alpha.state");
    assert!(
        profile.contains("state=failed\n") && profile.contains("reason=core_oom_killed\n"),
        "{profile}"
    );
    assert!(h.var.join("state/lasting/alpha.state").exists());
    let logs = h.logs();
    assert!(
        logs.iter()
            .any(|l| l == "lasting_cleared profile=alpha cause=run_now")
            && logs
                .iter()
                .any(|l| l.starts_with("lasting_blocked profile=alpha reason=core_oom_killed")),
        "{logs:?}"
    );
    h.advance(120);
    thread::sleep(Duration::from_millis(300));
    assert_eq!(h.launched(), 1, "blocked again after the second kill");
}

/// A removed profile takes its block with it.
#[test]
fn a_removed_profile_takes_its_block_with_it() {
    let h = Harness::new("lasting-removed", |home, var| {
        due_alpha(home, var);
        plant_block(var);
    });
    h.wait_for("the hold", PATIENT, |h| suppressed_logs(h) == 1);
    fs::remove_file(h.root.join("home/config/profiles.d/alpha.toml")).unwrap();
    let _ = h.ask(Request::Reload);
    assert!(!h.var.join("state/lasting/alpha.state").exists());
    assert_eq!(h.ask(Request::Status)["lasting"], json!({}));
    // The block is lifted exactly once, and the cause is the removal: load_lasting
    // re-checks the fragment when its fingerprint differs, so a periodic pass that
    // reads it between the existence check and the fingerprint read cannot name
    // the lift `settings_changed`.
    let cleared: Vec<String> = h
        .logs()
        .into_iter()
        .filter(|l| l.starts_with("lasting_cleared profile=alpha "))
        .collect();
    assert_eq!(
        cleared,
        ["lasting_cleared profile=alpha cause=profile_removed"],
        "one lift, by removal; logs: {:?}",
        h.logs()
    );
}

/// Run now lifts a block and so releases the due routine it held: two runs follow, the
/// requested one and the routine's. A test that polled `launched() == 1` missed the moment
/// between them whenever it was slow to look (CI, macOS aarch64: `timed out waiting for the
/// run` with `run_started id=2 origin=routine` in the events). A slow observer sees both.
#[test]
fn run_now_beside_a_due_routine_runs_twice_for_an_observer_that_looks_late() {
    let h = Harness::new("lasting-late-observer", |home, var| {
        due_alpha(home, var);
        fs::create_dir_all(var.join("state/lasting")).unwrap();
        fs::write(
            var.join("state/lasting/alpha.state"),
            "garbage
",
        )
        .unwrap();
    });
    h.wait_for("the hold", PATIENT, |h| suppressed_logs(h) == 1);
    assert_eq!(h.ask(run("alpha"))["ok"], true);
    // Looking late: both runs have started and finished by now.
    thread::sleep(Duration::from_millis(600));
    h.wait_for("the first run", PATIENT, |h| h.launched() >= 1);
    h.wait_for("idle", PATIENT, Harness::idle);
    assert_eq!(
        h.launched(),
        2,
        "Run now, then the routine it released: {:?}",
        h.logs()
    );
    assert!(!h.var.join("state/lasting/alpha.state").exists());
}

/// A corrupt record is named and still holds: never read as lifted.
#[test]
fn a_corrupt_block_record_is_named_and_still_holds() {
    let h = Harness::new("lasting-corrupt", |home, var| {
        due_alpha(home, var);
        fs::create_dir_all(var.join("state/lasting")).unwrap();
        fs::write(var.join("state/lasting/alpha.state"), "garbage\n").unwrap();
    });
    h.wait_for("the hold", PATIENT, |h| suppressed_logs(h) == 1);
    assert_eq!(h.launched(), 0);
    let status = h.ask(Request::Status);
    assert_eq!(
        status["lasting"]["alpha"]["reason"], "lasting_state_unreadable",
        "{status}"
    );
    assert!(
        status["lasting"]["alpha"]["error"]
            .as_str()
            .is_some_and(|e| e.contains("alpha.state") && e.contains("first line")),
        "{status}"
    );
    let named = h
        .logs()
        .iter()
        .filter(|l| l.starts_with("lasting_state_unreadable profile=alpha"))
        .count();
    assert_eq!(named, 1, "named once, not every turn");
    // Run now still lifts it.
    assert_eq!(h.ask(run("alpha"))["ok"], true);
    h.wait_for("the run", PATIENT, |h| h.launched() >= 1);
    assert!(!h.var.join("state/lasting/alpha.state").exists());
}

/// An OOM kill of a routine run blocks it: no retry is scheduled.
#[test]
fn an_oom_killed_routine_run_blocks_instead_of_retrying() {
    let h = Harness::new("lasting-oom", due_alpha);
    *h.plan.lock().unwrap() = Plan {
        finish_after: Some(Duration::from_millis(30)),
        exit: 137,
        stop_takes: Duration::ZERO,
    };
    h.world.lock().unwrap().oom_proven = true;
    h.wait_for("the run", PATIENT, |h| h.launched() >= 1);
    h.wait_for("the block", PATIENT, |h| {
        h.var.join("state/lasting/alpha.state").exists()
    });
    h.wait_for("the routine record", PATIENT, |h| {
        h.state("routines/alpha.state").contains(
            "last_reason=core_oom_killed
",
        )
    });
    let routine = h.state("routines/alpha.state");
    assert!(
        routine.contains("retry_attempt=0\n") && routine.contains("last_reason=core_oom_killed\n"),
        "{routine}"
    );
    h.advance(3600);
    thread::sleep(Duration::from_millis(300));
    assert_eq!(
        h.launched(),
        1,
        "no automatic re-run walked into the kill again"
    );
}

/// The generation of the newest arming of `routine` the fake source saw.
fn armed_generation(h: &Harness, routine: &str) -> Option<u64> {
    h.changes
        .calls
        .lock()
        .unwrap()
        .iter()
        .rev()
        .find(|c| c.starts_with(&format!("arm {routine} ")))
        .and_then(|c| c.rsplit("gen=").next())
        .and_then(|g| g.parse().ok())
}

/// A realtime profile whose source (or exclude list) is edited is watched again
/// on the new tree; the old watch is disarmed, and its late events are dropped.
#[test]
fn an_edited_realtime_source_is_rearmed_and_old_events_are_dropped() {
    let h = realtime_harness("rearm");
    h.wait_for("arm", PATIENT, |h| armed_generation(h, "docs").is_some());
    let first = armed_generation(&h, "docs").unwrap();
    let moved = h.root.join("sysroot/volume2/docs");
    // Whole or not at all, as the manager replaces a fragment: the daemon reads it
    // on its own schedule, and a plain `fs::write` truncates first, so a read in
    // between parsed a torn fragment (`routine_invalid field=profile_max_delete`).
    write_atomic(
        &h.root.join("home/config/profiles.d/docs.toml"),
        fragment(
            &h.root,
            "docs",
            &[("source", &format!("\"{}\"", slashed(&moved)))],
        ),
    );
    let _ = h.ask(Request::Reload);
    h.wait_for("re-arm", PATIENT, |h| {
        h.changes
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|c| c.starts_with(&format!("arm docs {} gen=", slashed(&moved))))
    });
    let second = armed_generation(&h, "docs").unwrap();
    assert!(second > first, "{first} -> {second}");
    {
        let calls = h.changes.calls.lock().unwrap();
        let disarm = calls.iter().position(|c| c == "disarm docs");
        let rearm = calls
            .iter()
            .position(|c| c.starts_with(&format!("arm docs {}", slashed(&moved))));
        assert!(disarm.is_some() && disarm < rearm, "{calls:?}");
    }
    assert!(
        h.logs()
            .iter()
            .any(|l| l.starts_with("routine_rearmed profile=docs")),
        "{:?}",
        h.logs()
    );
    // A change from the old watch: dropped. From the new one: dispatched.
    let _ = h.tx.send(Msg::Armed(
        first,
        ChangeEvent::Changed {
            routine: "docs".into(),
            dir: "old".into(),
            depth: Depth::Shallow,
        },
    ));
    thread::sleep(Duration::from_millis(1500));
    assert_eq!(h.launched(), 0, "a stale event dispatched a run");
    let _ = h.tx.send(Msg::Armed(
        second,
        ChangeEvent::Changed {
            routine: "docs".into(),
            dir: "new".into(),
            depth: Depth::Shallow,
        },
    ));
    h.wait_for("the dispatch", PATIENT, |h| h.launched() == 1);
    let args = h.world.lock().unwrap().launched[0]
        .args
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    assert!(
        args.windows(2)
            .any(|w| w[0] == "--scope-shallow" && w[1] == "new"),
        "{args:?}"
    );
}

/// An unchanged reload does not re-arm (a re-arm costs a whole-tree walk).
#[test]
fn an_unchanged_reload_keeps_the_watch() {
    let h = realtime_harness("keep");
    h.wait_for("arm", PATIENT, |h| armed_generation(h, "docs").is_some());
    let _ = h.ask(Request::Reload);
    let _ = h.ask(Request::Reload);
    let arms = h
        .changes
        .calls
        .lock()
        .unwrap()
        .iter()
        .filter(|c| c.starts_with("arm docs"))
        .count();
    assert_eq!(arms, 1);
}

/// `ctl status` names each profile's index: the counts, a corrupt index named,
/// a malformed record named rather than read as "no index", and none at all as
/// null.
#[test]
fn status_names_each_profiles_index() {
    let h = Harness::new("index-status", |home, var| {
        for name in ["alpha", "beta", "gamma", "delta"] {
            profile(home, name);
        }
        fs::write(
            var.join("state/profiles/alpha.state"),
            "state=succeeded\nindex_state=warm\nindex_entries=1000\nindex_hits=975\n\
             index_misses=25\nindex_canary_checked=25\nindex_canary_mismatches=0\n",
        )
        .unwrap();
        fs::write(
            var.join("state/profiles/beta.state"),
            "state=succeeded\nindex_state=cold\nindex_entries=60\nindex_hits=0\nindex_misses=60\n\
             index_issue=pair-index-corrupt\nindex_issue_detail=/p/beta.redb: set aside\n",
        )
        .unwrap();
        fs::write(
            var.join("state/profiles/gamma.state"),
            "state=succeeded\nindex_state=lukewarm\n",
        )
        .unwrap();
    });
    let status = h.ask(Request::Status);
    let index = &status["index"];
    assert_eq!(index["alpha"]["state"], "warm", "{status}");
    assert_eq!(index["alpha"]["entries"], 1000);
    assert_eq!(index["alpha"]["hits"], 975);
    assert_eq!(index["alpha"]["misses"], 25);
    assert_eq!(index["alpha"]["refreshed"], false);
    assert_eq!(index["beta"]["issue"], "pair-index-corrupt");
    assert_eq!(index["beta"]["issue_detail"], "/p/beta.redb: set aside");
    assert!(
        index["gamma"]["error"]
            .as_str()
            .is_some_and(|e| e.contains("lukewarm")),
        "{status}"
    );
    assert_eq!(index["delta"], Value::Null);
}

/// I-7's twin on the run side: a configuration change in flight
/// (management.lock held by a live process) is never run beside. The run is
/// deferred as busy and named; once the change is done, a run publishes
/// run.scope with run.lock and removes both after.
#[cfg(target_os = "linux")]
#[test]
fn a_run_never_starts_while_management_lock_is_live() {
    let h = Harness::tuned(
        "management",
        |home, _| {
            profile(home, "alpha");
            profile(home, "beta");
        },
        |settings| settings.publish_run_lock = true,
    );
    let run_dir = h.var.join("run");
    let mut holder = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let pid = holder.id();
    let start = super::super::super::linux_files::process_start(pid).unwrap();
    let boot = fs::read_to_string("/proc/sys/kernel/random/boot_id").unwrap();
    fs::create_dir_all(run_dir.join("management.lock")).unwrap();
    fs::write(
        run_dir.join("management.lock/pid"),
        format!("{pid}\n{start}\n{}\n", boot.trim()),
    )
    .unwrap();
    assert_eq!(h.ask(run("all"))["ok"], true);
    h.wait_for("the deferral", PATIENT, |h| {
        h.logs()
            .iter()
            .any(|l| l.starts_with("run_deferred ") && l.contains(&format!("process {pid}")))
    });
    assert_eq!(h.launched(), 0, "a run started mid-commit");
    assert!(!run_dir.join("run.lock").exists() && !run_dir.join("run.scope").exists());
    let _ = holder.kill();
    let _ = holder.wait();
    fs::remove_dir_all(run_dir.join("management.lock")).unwrap();
    *h.plan.lock().unwrap() = Plan {
        finish_after: Some(Duration::from_millis(400)),
        exit: 0,
        stop_takes: Duration::ZERO,
    };
    assert_eq!(h.ask(run("all"))["ok"], true);
    h.wait_for("the run", PATIENT, |h| h.launched() >= 1);
    let scope = fs::read_to_string(run_dir.join("run.scope")).unwrap();
    let lock = fs::read_to_string(run_dir.join("run.lock/pid")).unwrap();
    assert_eq!(
        scope,
        format!("{lock}profiles=alpha,beta\n"),
        "all is expanded"
    );
    h.wait_for("idle", PATIENT, Harness::idle);
    assert!(!run_dir.join("run.lock").exists() && !run_dir.join("run.scope").exists());
}

/// The runner's alert rules: a sync's result feeds the counter and channels;
/// a cancelled one does not; an exhausted ladder escalates once per the
/// policy's cooldown, the rest named as held by it.
#[test]
fn alerts_follow_the_runners_rules_and_escalations_their_cooldown() {
    let h = Harness::new("alerts", |home, var| {
        profile(home, "alpha");
        routine(
            home,
            "alpha",
            "enabled=true\nmode=interval\ninterval_seconds=60\nretry_count=1\nretry_backoff_seconds=10\n",
        );
        fs::write(
            var.join("state/routines/alpha.state"),
            format!("next_run_epoch={}\n", T0 - 5),
        )
        .unwrap();
        fs::write(home.join("config/alerts.conf"), "cooldown_seconds=600\n").unwrap();
    });
    *h.plan.lock().unwrap() = Plan {
        finish_after: Some(Duration::from_millis(20)),
        exit: 1,
        stop_takes: Duration::ZERO,
    };
    let launched = |n: usize| move |h: &Harness| h.launched() >= n && h.idle();
    h.wait_for("the first failure", PATIENT, launched(1));
    h.advance(11);
    h.wait_for("the first escalation", PATIENT, |h| {
        h.alerts
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|c| c == "escalate alpha exit=1")
    });
    assert!(
        h.alerts
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|c| c == "result alpha failed=true exit=1")
    );
    // The next interval exhausts the ladder again inside the 600 s cooldown:
    // no second escalation, and the hold is said.
    h.wait_for("idle", PATIENT, Harness::idle);
    h.advance(61);
    h.wait_for("the third run", PATIENT, launched(3));
    h.advance(11);
    h.wait_for("the held escalation", PATIENT, |h| {
        h.logs()
            .iter()
            .any(|l| l.starts_with("routine_escalation_cooldown profile=alpha"))
    });
    let escalations = h
        .alerts
        .calls
        .lock()
        .unwrap()
        .iter()
        .filter(|c| c.starts_with("escalate"))
        .count();
    assert_eq!(escalations, 1);
}

/// The daily safety net: an armed realtime routine syncs its whole tree once
/// per period whatever the watcher saw (inotify misses some writes).
#[test]
fn an_armed_realtime_routine_syncs_its_whole_tree_once_a_period() {
    let h = Harness::tuned(
        "full",
        |home, _| {
            profile(home, "docs");
            routine(
                home,
                "docs",
                "enabled=true\nmode=realtime\ndebounce_seconds=1\nretry_backoff_seconds=10\n",
            );
        },
        |settings| settings.full_reconcile_every = Duration::from_secs(2),
    );
    h.wait_for("arm", PATIENT, |h| armed_generation(h, "docs").is_some());
    let _ = h.tx.send(Msg::Change(ChangeEvent::Backend {
        routine: "docs".into(),
        backend: Backend::Inotify,
        reason: BackendReason::Ok,
        detail: "armed".into(),
    }));
    h.wait_for("the safety-net run", Duration::from_secs(10), |h| {
        h.launched() >= 1
    });
    let args = h.world.lock().unwrap().launched[0]
        .args
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    assert!(
        !args.iter().any(|a| a == "--scope"),
        "a whole-tree run: {args:?}"
    );
    assert!(
        h.logs()
            .iter()
            .any(|l| l.starts_with("routine_full_reconcile profile=docs")),
        "{:?}",
        h.logs()
    );
}

/// No run while service admission is closed: a requested run is refused
/// with a named code; a routine is deferred, named, and runs once it opens.
#[test]
fn runs_wait_for_service_admission() {
    let h = Harness::new("admission", |home, var| {
        due_alpha(home, var);
        fs::write(var.join("run/service.closed"), "closed\n").unwrap();
    });
    h.wait_for("the deferral", PATIENT, |h| {
        h.logs()
            .iter()
            .any(|l| l.starts_with("run_deferred ") && l.contains("reason=admission_refused"))
    });
    let refused = h.ask(run("alpha"));
    assert_eq!(refused["code"], "admission_closed", "{refused}");
    assert_eq!(refused["exit"], 75);
    assert_eq!(h.launched(), 0);
    fs::remove_file(h.var.join("run/service.closed")).unwrap();
    // A marker that is not the lifecycle's own reads closed, never open.
    fs::write(h.var.join("run/package.transition"), "junk\n").unwrap();
    assert_eq!(h.ask(run("alpha"))["code"], "admission_closed");
    fs::remove_file(h.var.join("run/package.transition")).unwrap();
    assert_eq!(h.ask(run("alpha"))["ok"], true);
    h.wait_for("the run", PATIENT, |h| h.launched() >= 1);
}

/// `wait` answers with the run's own exit and outcome, or names a timeout.
#[test]
fn wait_answers_with_the_runs_own_ending() {
    let h = Harness::new("wait", |home, _| profile(home, "alpha"));
    *h.plan.lock().unwrap() = Plan {
        finish_after: Some(Duration::from_millis(300)),
        exit: 3,
        stop_takes: Duration::ZERO,
    };
    let queued = h.ask(run("alpha"));
    let id = queued["id"].as_u64().unwrap();
    let done = ask(
        &h.tx,
        Request::Wait {
            id,
            within: Duration::from_secs(5),
        },
        Duration::from_secs(10),
    );
    assert_eq!(done["finished"], true, "{done}");
    assert_eq!(done["exit"], 3);
    assert_eq!(done["outcome"], "partial");
    // Asked again after the end: the same answer.
    let again = h.ask(Request::Wait {
        id,
        within: Duration::from_secs(1),
    });
    assert_eq!(again["exit"], 3);
    // A run still going when the deadline passes: named, exit 75.
    *h.plan.lock().unwrap() = Plan {
        finish_after: Some(Duration::from_secs(5)),
        exit: 0,
        stop_takes: Duration::ZERO,
    };
    let long = h.ask(run("alpha"))["id"].as_u64().unwrap();
    let timed_out = h.ask(Request::Wait {
        id: long,
        within: Duration::from_millis(300),
    });
    assert_eq!(timed_out["code"], "wait_timeout", "{timed_out}");
    assert_eq!(timed_out["exit"], 75);
    assert_eq!(
        h.ask(Request::Wait {
            id: 999,
            within: Duration::from_secs(1)
        })["code"],
        "run_unknown"
    );
}

/// Every plan and sync gets the verified-pair index directory.
#[test]
fn runs_are_given_the_pair_index_directory() {
    let h = Harness::tuned(
        "pair-index",
        |home, _| profile(home, "alpha"),
        |settings| settings.pair_index_dir = Some(PathBuf::from("/var/state/cache/pairs")),
    );
    assert_eq!(h.ask(run("alpha"))["ok"], true);
    h.wait_for("the run", PATIENT, |h| h.launched() >= 1);
    let args = h.world.lock().unwrap().launched[0]
        .args
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    assert!(
        args.windows(2)
            .any(|w| w[0] == "--pair-index" && w[1] == "/var/state/cache/pairs"),
        "{args:?}"
    );
}

/// S2-2: a configuration file that cannot be read is not an absent one. An
/// unreadable routine conf used to read as `enabled=false` (realtime silently
/// disarmed), an unreadable routines.d as no routines. Each is named once per
/// episode with its path and cause, the last good configuration is kept, and
/// the recovery is named.
#[cfg(unix)]
#[test]
fn unreadable_configuration_is_named_and_never_read_as_absent() {
    use std::os::unix::fs::PermissionsExt;
    let h = realtime_harness("unreadable");
    let armed = |h: &Harness| {
        h.changes
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|c| c.starts_with("arm docs"))
    };
    h.wait_for("arm", PATIENT, armed);
    let conf = h.root.join("home/config/routines.d/docs.conf");
    fs::set_permissions(&conf, fs::Permissions::from_mode(0o000)).unwrap();
    if super::super::testkit::permissions_do_not_bind(
        "unreadable_configuration_is_named_and_never_read_as_absent",
        &conf,
    ) {
        return;
    }
    let named = |h: &Harness, needle: &str| {
        h.logs()
            .iter()
            .filter(|l| l.starts_with("config_unreadable") && l.contains(needle))
            .count()
    };
    let _ = h.ask(Request::Reload);
    h.wait_for("the conf named", PATIENT, |h| named(h, "docs.conf") == 1);
    for _ in 0..3 {
        let _ = h.ask(Request::Wake);
    }
    thread::sleep(Duration::from_millis(300));
    assert_eq!(named(&h, "docs.conf"), 1, "once per episode");
    assert!(
        h.logs()
            .iter()
            .any(|l| l.contains("docs.conf") && l.contains("ermission denied")),
        "{:?}",
        h.logs()
    );
    let disarmed = |h: &Harness| {
        h.changes
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|c| c == "disarm docs")
    };
    assert!(
        !disarmed(&h),
        "an unreadable conf is not a disabled routine"
    );
    // The whole directory unreadable: not zero routines.
    let dir = h.root.join("home/config/routines.d");
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o000)).unwrap();
    let _ = h.ask(Request::Reload);
    h.wait_for("the directory named", PATIENT, |h| {
        named(h, "routines.d cause=") == 1
    });
    assert!(!disarmed(&h), "an unreadable directory is not an empty one");
    assert_eq!(
        h.ask(Request::Status)["routines"].as_array().map(Vec::len),
        Some(1)
    );
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
    fs::set_permissions(&conf, fs::Permissions::from_mode(0o600)).unwrap();
    let _ = h.ask(Request::Reload);
    h.wait_for("the recovery named", PATIENT, |h| {
        h.logs()
            .iter()
            .any(|l| l.starts_with("config_recovered") && l.contains("docs.conf"))
    });
    assert!(!disarmed(&h));
}

/// S2-2: an unreadable routine state is held and named, never read as a
/// fresh state (which reset the failure streak and the retry backoff).
#[cfg(unix)]
#[test]
fn an_unreadable_routine_state_is_held_not_reset() {
    use std::os::unix::fs::PermissionsExt;
    let h = Harness::new("state-unreadable", |home, var| {
        profile(home, "alpha");
        routine(
            home,
            "alpha",
            "enabled=true\nmode=interval\ninterval_seconds=60\n",
        );
        let state = var.join("state/routines/alpha.state");
        fs::write(&state, format!("next_run_epoch={}\n", T0 - 5)).unwrap();
        fs::set_permissions(&state, fs::Permissions::from_mode(0o000)).unwrap();
    });
    if super::super::testkit::permissions_do_not_bind(
        "an_unreadable_routine_state_is_held_not_reset",
        &h.var.join("state/routines/alpha.state"),
    ) {
        return;
    }
    h.wait_for("the state named", PATIENT, |h| {
        h.logs()
            .iter()
            .any(|l| l.starts_with("config_unreadable") && l.contains("routines/alpha.state"))
    });
    h.wait_for("the hold named", PATIENT, |h| {
        h.logs()
            .iter()
            .any(|l| l.starts_with("routine_state_invalid profile=alpha field=unreadable"))
    });
    thread::sleep(Duration::from_millis(300));
    assert_eq!(h.launched(), 0, "never run from a state it could not read");
}

// ---------------------------------------------------------------- fix6-core

/// N-D3: an unreadable profile state is never rewritten from nothing. Read as
/// absent, a failed run reset `consecutive_failures` 7 to 1 and
/// `last_success_epoch` to 0, and the log said the last configuration was
/// kept. The record is left as it is, the run says `degraded=profile_state`,
/// and the log names the path, the cause and what was kept.
#[cfg(unix)]
#[test]
fn an_unreadable_profile_state_is_kept_not_reset() {
    use std::os::unix::fs::PermissionsExt;
    const STREAK: &str = "state=failed\nfinished_epoch=1779990000\nexit_code=1\naction=sync\n\
        outcome=failed\nreason=core_failed\nscope=profile\noutcome_epoch=1779990000\n\
        consecutive_failures=7\nlast_success_epoch=1779000000\n";
    let h = Harness::new("profile-state-unreadable", |home, var| {
        profile(home, "alpha");
        let state = var.join("state/profiles/alpha.state");
        fs::write(&state, STREAK).unwrap();
        fs::set_permissions(&state, fs::Permissions::from_mode(0o000)).unwrap();
    });
    let state = h.var.join("state/profiles/alpha.state");
    if super::super::testkit::permissions_do_not_bind(
        "an_unreadable_profile_state_is_kept_not_reset",
        &state,
    ) {
        return;
    }
    *h.plan.lock().unwrap() = Plan {
        finish_after: Some(Duration::from_millis(20)),
        exit: 1,
        stop_takes: Duration::ZERO,
    };
    h.ask(run("alpha"));
    h.wait_for("the run", PATIENT, |h| {
        h.launched() == 1 && h.idle() && h.state("run.state").contains("state=failed\n")
    });
    fs::set_permissions(&state, fs::Permissions::from_mode(0o600)).unwrap();
    let kept = fs::read_to_string(&state).unwrap();
    let run_state = h.state("run.state");
    let logs = h.logs();
    eprintln!("profile state: {kept:?}\nrun.state: {run_state:?}\nlogs: {logs:#?}");
    assert_eq!(kept, STREAK, "the streak is kept, not reset");
    assert!(
        run_state.contains("degraded=profile_state\n"),
        "the run says its profile record was not written: {run_state}"
    );
    let named = logs
        .iter()
        .find(|l| l.starts_with("profile_state_unreadable"))
        .unwrap_or_else(|| panic!("the skipped write is named: {logs:#?}"));
    assert!(
        named.contains("profiles/alpha.state")
            && named.contains("ermission denied")
            && named.contains("kept=the record on disk"),
        "{named}"
    );
    assert!(
        !logs
            .iter()
            .any(|l| l.contains("alpha.state") && l.contains("kept=the last configuration read")),
        "no false claim about a state file: {logs:#?}"
    );
}

/// H2 amber: the queue's audit health reaches controller.state as
/// `degraded=`, `degraded_since=`, `degraded_cause=` while it lasts, each
/// change named, and the keys go when it recovers.
#[test]
fn the_audit_health_is_published_in_controller_state() {
    let h = Harness::new("audit-health", |home, _| profile(home, "alpha"));
    h.wait_for("the first controller.state", PATIENT, |h| {
        h.state("controller.state").starts_with("state=running\n")
    });
    let degraded = super::super::state::ControllerDegraded {
        reason: "audit_unavailable",
        since: T0 - 40,
        cause: "the audit log could not be reconciled (exit=73)".into(),
    };
    h.tx.send(Msg::AuditHealth(Some(degraded.clone()))).unwrap();
    h.wait_for("the degradation written", PATIENT, |h| {
        h.state("controller.state")
            .contains("degraded=audit_unavailable\n")
    });
    let state = h.state("controller.state");
    eprintln!("{state}");
    assert!(
        state.contains(&format!("degraded_since={}\n", T0 - 40))
            && state.contains("degraded_cause=the audit log could not be reconciled (exit=73)\n"),
        "{state}"
    );
    // Every later rewrite keeps it while it lasts (here: a run's end).
    h.advance(1);
    h.ask(run("alpha"));
    h.wait_for("the run", PATIENT, |h| h.launched() == 1 && h.idle());
    h.wait_for("the rewrite keeps it", PATIENT, |h| {
        let state = h.state("controller.state");
        state.contains(&format!("updated_epoch={}\n", T0 + 1))
            && state.contains("degraded=audit_unavailable\n")
    });
    h.tx.send(Msg::AuditHealth(None)).unwrap();
    h.wait_for("the degradation cleared", PATIENT, |h| {
        !h.state("controller.state").contains("degraded")
    });
    let logs = h.logs();
    eprintln!("{logs:#?}");
    assert_eq!(
        logs.iter()
            .filter(|l| l.starts_with("service_degraded reason=audit_unavailable"))
            .count(),
        1,
        "{logs:#?}"
    );
    assert!(
        logs.iter()
            .any(|l| l.starts_with("service_degraded_cleared reason=audit_unavailable")),
        "{logs:#?}"
    );
}

/// S3-2: a degradation that lasts but whose cause moves (the reconcile failed
/// with exit 73, now 69) is republished with the new cause and the same start;
/// controller.state must not keep the cause of the first failure.
#[test]
fn a_degradation_whose_cause_changes_is_republished() {
    let h = Harness::new("audit-cause", |home, _| profile(home, "alpha"));
    h.wait_for("the first controller.state", PATIENT, |h| {
        h.state("controller.state").starts_with("state=running\n")
    });
    let degraded = |cause: &str| super::super::state::ControllerDegraded {
        reason: "audit_unavailable",
        since: T0 - 40,
        cause: cause.into(),
    };
    h.tx.send(Msg::AuditHealth(Some(degraded(
        "the audit log could not be reconciled (exit=73)",
    ))))
    .unwrap();
    h.wait_for("exit=73 written", PATIENT, |h| {
        h.state("controller.state").contains("(exit=73)\n")
    });
    h.tx.send(Msg::AuditHealth(Some(degraded(
        "the audit log could not be reconciled (exit=69)",
    ))))
    .unwrap();
    h.wait_for("exit=69 written", PATIENT, |h| {
        h.state("controller.state").contains("(exit=69)\n")
    });
    let state = h.state("controller.state");
    let logs = h.logs();
    eprintln!("{state}\n{logs:#?}");
    assert!(!state.contains("exit=73"), "the old cause stayed: {state}");
    assert!(
        state.contains(&format!("degraded_since={}\n", T0 - 40)),
        "one episode keeps its start: {state}"
    );
    // Each cause is named in the log with the episode's start.
    for exit in ["exit=73", "exit=69"] {
        assert_eq!(
            logs.iter()
                .filter(
                    |l| l.starts_with("service_degraded reason=audit_unavailable")
                        && l.contains(&format!("since={}", T0 - 40))
                        && l.contains(exit)
                )
                .count(),
            1,
            "{exit}: {logs:#?}"
        );
    }
}

/// N-D5 (status half, asked by fix6-watch): the change source's account of
/// a degraded backend reaches `ctl status` as `backend_detail`, the latest
/// one, even when backend and reason did not change.
#[test]
fn status_carries_the_backends_detail() {
    let h = realtime_harness("backend-detail");
    h.wait_for("arm", PATIENT, |h| armed_generation(h, "docs").is_some());
    let detail = |h: &Harness| {
        h.ask(Request::Status)["routines"]
            .as_array()
            .and_then(|list| list.iter().find(|r| r["profile"] == "docs").cloned())
            .map(|r| r["backend_detail"].clone())
            .unwrap_or(Value::Null)
    };
    for account in [
        "walk_deadline_exceeded seconds=120 path=/volume1/docs",
        "walk_deadline_exceeded seconds=240 path=/volume1/docs",
    ] {
        h.tx.send(Msg::Change(ChangeEvent::Backend {
            routine: "docs".into(),
            backend: Backend::Polling,
            reason: BackendReason::WatchSetupFailed,
            detail: account.into(),
        }))
        .unwrap();
        h.wait_for("the detail in status", PATIENT, |h| detail(h) == account);
    }
}

/// S3-12: `backend_detail` is cut at 240 bytes, and it was cut from the end,
/// which lost `next_walk_deadline_seconds=N`, the one number the dashboard
/// points at. The tail is kept.
#[test]
fn a_long_backend_detail_keeps_its_tail() {
    let h = realtime_harness("backend-tail");
    h.wait_for("arm", PATIENT, |h| armed_generation(h, "docs").is_some());
    let long = format!(
        "inotify unavailable: {}; walk_deadline_exceeded: the walk gave up after 120s in /x; \
         next_walk_deadline_seconds=240",
        "the watcher's own account of why, which goes on and on, ".repeat(8)
    );
    assert!(long.len() > 240);
    h.tx.send(Msg::Change(ChangeEvent::Backend {
        routine: "docs".into(),
        backend: Backend::Polling,
        reason: BackendReason::WatchSetupFailed,
        detail: long,
    }))
    .unwrap();
    let detail = |h: &Harness| {
        h.ask(Request::Status)["routines"]
            .as_array()
            .and_then(|list| list.iter().find(|r| r["profile"] == "docs").cloned())
            .and_then(|r| r["backend_detail"].as_str().map(str::to_owned))
            .unwrap_or_default()
    };
    h.wait_for("the detail in status", PATIENT, |h| !detail(h).is_empty());
    let kept = detail(&h);
    assert!(kept.len() <= 240, "{} bytes: {kept}", kept.len());
    assert!(
        kept.ends_with("next_walk_deadline_seconds=240"),
        "the tail is kept: {kept}"
    );
    assert!(
        kept.starts_with("inotify unavailable"),
        "the head is kept: {kept}"
    );
}

/// S3-12: a polling tree whose skipped-entry count changes on every reading
/// reported a new backend account each time, and each was a `routine_backend`
/// line. One cause is one episode: the first report is logged, the rest are
/// counted, and the report that ends the episode carries the count.
#[test]
fn a_flapping_backend_cause_is_logged_once_per_episode() {
    let h = realtime_harness("backend-episode");
    h.wait_for("arm", PATIENT, |h| armed_generation(h, "docs").is_some());
    let report = |backend: Backend, reason: BackendReason, detail: String| {
        h.tx.send(Msg::Change(ChangeEvent::Backend {
            routine: "docs".into(),
            backend,
            reason,
            detail,
        }))
        .unwrap();
    };
    const FLAPS: usize = 40;
    for skipped in 1..=FLAPS {
        report(
            Backend::Polling,
            BackendReason::WatchSetupFailed,
            format!(
                "inotify watch limit reached; {skipped} entries skipped: a/{skipped} (PermissionDenied)"
            ),
        );
    }
    report(
        Backend::Inotify,
        BackendReason::Ok,
        "12 directories watched".into(),
    );
    h.wait_for("the recovery line", PATIENT, |h| {
        h.logs()
            .iter()
            .any(|l| l.starts_with("routine_backend ") && l.contains("reason=ok"))
    });
    let lines: Vec<String> = h
        .logs()
        .into_iter()
        .filter(|l| l.starts_with("routine_backend "))
        .collect();
    assert_eq!(
        lines.len(),
        2,
        "{FLAPS} reports of one cause are one line, then the recovery: {lines:#?}"
    );
    assert!(
        lines[0].contains("reason=watch_setup_failed") && lines[0].contains("1 entries skipped"),
        "the first report of the episode is logged whole: {}",
        lines[0]
    );
    assert!(
        lines[1].contains("reason=ok")
            && lines[1].contains(&format!("suppressed_reports={}", FLAPS - 1))
            && lines[1].contains("previous_reason=watch_setup_failed"),
        "the recovery line counts what the episode did not log: {}",
        lines[1]
    );
}

/// The paths `ctl status` lists as `config_unreadable`.
fn unreadable_paths(h: &Harness) -> Vec<String> {
    h.ask(Request::Status)["config_unreadable"]
        .as_array()
        .map(|list| {
            list.iter()
                .map(|e| format!("{} {}", e["path"].as_str().unwrap_or(""), e["cause"]))
                .collect()
        })
        .unwrap_or_default()
}

/// N-D8: a dependency's profile state that cannot be read held the routine
/// back without a word (`read_document(..).ok().flatten()`). It is named as
/// `config_unreadable` with its path and cause, and its recovery too.
#[cfg(unix)]
#[test]
fn an_unreadable_dependency_state_is_named() {
    use std::os::unix::fs::PermissionsExt;
    let h = Harness::new("dependency-unreadable", |home, var| {
        profile(home, "beta");
        due_alpha(home, var);
        routine(
            home,
            "alpha",
            "enabled=true\nmode=interval\ninterval_seconds=60\ndepends_on=beta\n",
        );
        let state = var.join("state/profiles/beta.state");
        fs::write(&state, "state=succeeded\n").unwrap();
        fs::set_permissions(&state, fs::Permissions::from_mode(0o000)).unwrap();
    });
    let state = h.var.join("state/profiles/beta.state");
    if super::super::testkit::permissions_do_not_bind(
        "an_unreadable_dependency_state_is_named",
        &state,
    ) {
        return;
    }
    h.wait_for("the dependency named", PATIENT, |h| {
        unreadable_paths(h)
            .iter()
            .any(|p| p.contains("profiles/beta.state") && p.contains("ermission denied"))
    });
    assert!(
        h.logs()
            .iter()
            .any(|l| l.starts_with("config_unreadable") && l.contains("profiles/beta.state")),
        "{:#?}",
        h.logs()
    );
    assert_eq!(
        h.launched(),
        0,
        "a dependency that cannot be read is not met"
    );
    fs::set_permissions(&state, fs::Permissions::from_mode(0o600)).unwrap();
    // The deferred routine is evaluated again at its next check.
    h.advance(3600);
    h.wait_for("the recovery", PATIENT, |h| {
        h.logs()
            .iter()
            .any(|l| l.starts_with("config_recovered") && l.contains("profiles/beta.state"))
    });
    h.wait_for("the run once it can be read", PATIENT, |h| {
        h.launched() == 1
    });
}

/// N-D8: an unreadable record of a routine that was then removed was listed
/// in `config_unreadable` for the life of the daemon. It is forgotten, named,
/// with the routine.
#[cfg(unix)]
#[test]
fn a_removed_routines_unreadable_record_is_forgotten() {
    use std::os::unix::fs::PermissionsExt;
    let h = Harness::new("removed-unreadable", |home, var| {
        due_alpha(home, var);
        let state = var.join("state/routines/alpha.state");
        fs::set_permissions(&state, fs::Permissions::from_mode(0o000)).unwrap();
    });
    let state = h.var.join("state/routines/alpha.state");
    if super::super::testkit::permissions_do_not_bind(
        "a_removed_routines_unreadable_record_is_forgotten",
        &state,
    ) {
        return;
    }
    h.wait_for("the state named", PATIENT, |h| {
        unreadable_paths(h)
            .iter()
            .any(|p| p.contains("routines/alpha.state"))
    });
    fs::remove_file(h.root.join("home/config/routines.d/alpha.conf")).unwrap();
    let _ = h.ask(Request::Reload);
    h.wait_for("the record forgotten", PATIENT, |h| {
        !unreadable_paths(h)
            .iter()
            .any(|p| p.contains("routines/alpha.state"))
    });
    let logs = h.logs();
    eprintln!("{logs:#?}");
    assert!(
        logs.iter().any(|l| l.starts_with("config_forgotten")
            && l.contains("routines/alpha.state")
            && l.contains("routine was removed")),
        "{logs:#?}"
    );
}

/// S3-1: an unreadable dependency state stayed in `config_unreadable` after the
/// routine that depended on it stopped doing so (its `depends_on` edited away),
/// until the daemon restarted, and its `kept=` text kept claiming a dependency
/// that no longer exists. The profile is still there: only the dependency read
/// is gone, so the entry goes with it, named.
#[cfg(unix)]
#[test]
fn a_dependency_state_nobody_depends_on_any_more_is_forgotten() {
    use std::os::unix::fs::PermissionsExt;
    let h = Harness::new("dependency-dropped", |home, var| {
        profile(home, "beta");
        due_alpha(home, var);
        routine(
            home,
            "alpha",
            "enabled=true\nmode=interval\ninterval_seconds=60\ndepends_on=beta\n",
        );
        let state = var.join("state/profiles/beta.state");
        fs::write(&state, "state=succeeded\n").unwrap();
        fs::set_permissions(&state, fs::Permissions::from_mode(0o000)).unwrap();
    });
    let state = h.var.join("state/profiles/beta.state");
    if super::super::testkit::permissions_do_not_bind(
        "a_dependency_state_nobody_depends_on_any_more_is_forgotten",
        &state,
    ) {
        return;
    }
    h.wait_for("the dependency named", PATIENT, |h| {
        unreadable_paths(h)
            .iter()
            .any(|p| p.contains("profiles/beta.state"))
    });
    // The operator drops the dependency (the manager's configure-routine
    // replaces the file whole).
    write_atomic(
        &h.root.join("home/config/routines.d/alpha.conf"),
        b"enabled=true\nmode=interval\ninterval_seconds=60\ndepends_on=\n",
    );
    let _ = h.ask(Request::Reload);
    h.wait_for("the entry forgotten", PATIENT, |h| {
        !unreadable_paths(h)
            .iter()
            .any(|p| p.contains("profiles/beta.state"))
    });
    let logs = h.logs();
    eprintln!("{logs:#?}\n{:#}", h.ask(Request::Status));
    let forgotten = logs
        .iter()
        .find(|l| l.starts_with("config_forgotten") && l.contains("profiles/beta.state"))
        .unwrap_or_else(|| panic!("the forgetting is named with its path: {logs:#?}"));
    assert!(forgotten.contains("depends"), "{forgotten}");
}

/// S3-1: a removed profile's own state record, unreadable when a run tried to
/// update it, stayed listed too.
#[cfg(unix)]
#[test]
fn a_removed_profiles_unreadable_state_is_forgotten() {
    use std::os::unix::fs::PermissionsExt;
    let h = Harness::new("removed-profile-state", |home, var| {
        profile(home, "alpha");
        profile(home, "beta");
        let state = var.join("state/profiles/beta.state");
        fs::write(&state, "state=succeeded\n").unwrap();
        fs::set_permissions(&state, fs::Permissions::from_mode(0o000)).unwrap();
    });
    let state = h.var.join("state/profiles/beta.state");
    if super::super::testkit::permissions_do_not_bind(
        "a_removed_profiles_unreadable_state_is_forgotten",
        &state,
    ) {
        return;
    }
    assert_eq!(h.ask(run("beta"))["ok"], true);
    h.wait_for("the state named", PATIENT, |h| {
        unreadable_paths(h)
            .iter()
            .any(|p| p.contains("profiles/beta.state"))
    });
    // A profile that still exists keeps its entry: the record is still read.
    let _ = h.ask(Request::Reload);
    assert!(
        unreadable_paths(&h)
            .iter()
            .any(|p| p.contains("profiles/beta.state")),
        "{:?}",
        unreadable_paths(&h)
    );
    fs::remove_file(h.root.join("home/config/profiles.d/beta.toml")).unwrap();
    let _ = h.ask(Request::Reload);
    h.wait_for("the entry forgotten", PATIENT, |h| {
        !unreadable_paths(h)
            .iter()
            .any(|p| p.contains("profiles/beta.state"))
    });
    let logs = h.logs();
    eprintln!("{logs:#?}");
    assert!(
        logs.iter().any(|l| l.starts_with("config_forgotten")
            && l.contains("profiles/beta.state")
            && l.contains("profile was removed")),
        "{logs:#?}"
    );
}

/// N-D8: `ctl status` read an unreadable profiles.d as no profiles
/// (`profile_names_in(..).unwrap_or_default()`), so `index` was silently
/// empty. It says the index list is unavailable, with the path and cause.
#[cfg(unix)]
#[test]
fn status_names_an_unreadable_profile_directory() {
    use std::os::unix::fs::PermissionsExt;
    let h = Harness::new("status-profiles-unreadable", |home, _| {
        profile(home, "alpha");
    });
    let dir = h.root.join("home/config/profiles.d");
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o000)).unwrap();
    if super::super::testkit::permissions_do_not_bind(
        "status_names_an_unreadable_profile_directory",
        &dir,
    ) {
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        return;
    }
    let status = h.ask(Request::Status);
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
    eprintln!("{status:#}");
    let unavailable = &status["index_unavailable"];
    assert!(
        unavailable["path"]
            .as_str()
            .is_some_and(|p| p.ends_with("profiles.d"))
            && unavailable["cause"]
                .as_str()
                .is_some_and(|c| c.contains("ermission denied")),
        "{status:#}"
    );
}

// ------------------------------------------------------------ observation (C2, C3)

/// The Activity rows of one code for one profile, as `(state, level, message)`.
fn observation_rows(h: &Harness, code: &str, profile: &str) -> Vec<(String, Level, String)> {
    h.sink
        .rows
        .lock()
        .unwrap()
        .iter()
        .filter(|r| r.code == code && r.profile == profile)
        .map(|r| (r.state.to_owned(), r.level, r.message.clone()))
        .collect()
}

fn report_backend(h: &Harness, backend: Backend, reason: BackendReason, detail: &str) {
    h.tx.send(Msg::Change(ChangeEvent::Backend {
        routine: "docs".into(),
        backend,
        reason,
        detail: detail.into(),
    }))
    .unwrap();
}

fn notice(h: &Harness, detail: &str) {
    h.tx.send(Msg::Change(ChangeEvent::Notice {
        routine: "docs".into(),
        warn: true,
        detail: detail.into(),
    }))
    .unwrap();
}

/// The walk cap leaves nothing watching or polling the tree; before this it was
/// a controller.log line only, so the Activity page read as healthy. One row per
/// episode, however many turns and repeated reports, and one when it clears.
#[test]
fn a_blind_routine_writes_one_activity_row_per_episode() {
    let h = realtime_harness("observe-walk-cap");
    h.wait_for("arm", PATIENT, |h| armed_generation(h, "docs").is_some());
    let capped = format!(
        "walk cap reached; retrying every 30 s; {}",
        super::super::watch_source::WALK_CAP_NOTE
    );
    report_backend(&h, Backend::None, BackendReason::WatchSetupFailed, &capped);
    h.wait_for("the row", PATIENT, |h| {
        observation_rows(h, "routine.unobserved", "docs").len() == 1
    });
    // More reports and turns of the same episode say nothing more.
    report_backend(&h, Backend::None, BackendReason::WatchSetupFailed, &capped);
    let _ = h.ask(Request::Wake);
    let _ = h.ask(Request::Wake);
    thread::sleep(Duration::from_millis(400));
    let rows = observation_rows(&h, "routine.unobserved", "docs");
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].0, "unavailable");
    assert_eq!(rows[0].1, Level::Warn);
    assert!(rows[0].2.contains("too many tree walks"), "{rows:?}");
    let state = h.state("routines/docs.state");
    assert!(
        state.contains("observe_state=blind\n")
            && state.contains("observe_cause=walk_cap\n")
            && state.contains("watching_via=none\n"),
        "{state}"
    );
    let routine = h.ask(Request::Status)["routines"]
        .as_array()
        .and_then(|list| list.iter().find(|r| r["profile"] == "docs").cloned())
        .unwrap();
    assert_eq!(routine["observe"], "blind", "{routine}");
    assert_eq!(routine["observe_cause"], "walk_cap", "{routine}");
    // The episode ends when a watcher reports in.
    report_backend(&h, Backend::Inotify, BackendReason::Ok, "armed");
    h.wait_for("the recovery row", PATIENT, |h| {
        observation_rows(h, "routine.observed", "docs").len() == 1
    });
    let state = h.state("routines/docs.state");
    assert!(
        state.contains("observe_state=ok\n")
            && state.contains("observe_cause=none\n")
            && state.contains("observe_since_epoch=0\n")
            && state.contains("watching_via=native\n"),
        "{state}"
    );
    assert_eq!(observation_rows(&h, "routine.unobserved", "docs").len(), 1);
}

/// A poll floor that cannot finish its walk is named once, degraded not blind
/// (the native watch still looks), and its recovery is said too.
#[test]
fn a_failing_poll_floor_is_one_degraded_episode() {
    let h = realtime_harness("observe-floor");
    h.wait_for("arm", PATIENT, |h| armed_generation(h, "docs").is_some());
    report_backend(&h, Backend::Inotify, BackendReason::Ok, "armed");
    notice(&h, "poll_floor_failed cause=walk_deadline_exceeded");
    h.wait_for("the row", PATIENT, |h| {
        observation_rows(h, "routine.unobserved", "docs").len() == 1
    });
    notice(&h, "poll_floor_failed cause=walk_deadline_exceeded");
    let _ = h.ask(Request::Wake);
    thread::sleep(Duration::from_millis(300));
    let rows = observation_rows(&h, "routine.unobserved", "docs");
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert!(rows[0].2.contains("backstop poll"), "{rows:?}");
    let state = h.state("routines/docs.state");
    assert!(
        state.contains("observe_state=degraded\n")
            && state.contains("observe_cause=poll_floor_failed\n")
            && state.contains("watching_via=native\n"),
        "{state}"
    );
    notice(&h, "poll_floor_recovered");
    h.wait_for("the recovery", PATIENT, |h| {
        observation_rows(h, "routine.observed", "docs").len() == 1
    });
}

/// A routine held by a lasting block, and one the user disabled, each write one
/// row, however many turns pass.
#[test]
fn a_lasting_block_and_a_disabled_routine_each_write_one_row() {
    let h = Harness::new("observe-held", |home, var| {
        due_alpha(home, var);
        plant_block(var);
        profile(home, "beta");
        routine(
            home,
            "beta",
            "enabled=false\nmode=interval\ninterval_seconds=60\n",
        );
    });
    h.wait_for("both rows", PATIENT, |h| {
        observation_rows(h, "routine.unobserved", "alpha").len() == 1
            && observation_rows(h, "routine.unobserved", "beta").len() == 1
    });
    h.advance(1);
    h.advance(1);
    thread::sleep(Duration::from_millis(300));
    let alpha = observation_rows(&h, "routine.unobserved", "alpha");
    let beta = observation_rows(&h, "routine.unobserved", "beta");
    assert_eq!((alpha.len(), beta.len()), (1, 1), "{alpha:?} {beta:?}");
    assert_eq!(alpha[0].1, Level::Warn);
    assert!(alpha[0].2.contains("held"), "{alpha:?}");
    assert_eq!(beta[0].1, Level::Info);
    assert!(beta[0].2.contains("disabled"), "{beta:?}");
    let state = h.state("routines/alpha.state");
    assert!(
        state.contains("observe_state=blind\n") && state.contains("observe_cause=lasting_block\n"),
        "{state}"
    );
    let state = h.state("routines/beta.state");
    assert!(
        state.contains("observe_state=disabled\n") && state.contains("observe_cause=disabled\n"),
        "{state}"
    );
}

/// A record that already holds the episode is not announced again after a restart.
#[test]
fn an_episode_on_disk_is_not_announced_again_after_a_restart() {
    let h = Harness::new("observe-restart", |home, var| {
        profile(home, "beta");
        routine(
            home,
            "beta",
            "enabled=false\nmode=interval\ninterval_seconds=60\n",
        );
        fs::write(
            var.join("state/routines/beta.state"),
            "state=never\nobserve_state=disabled\nobserve_cause=disabled\nobserve_since_epoch=5\n",
        )
        .unwrap();
    });
    h.advance(1);
    thread::sleep(Duration::from_millis(400));
    assert!(observation_rows(&h, "routine.unobserved", "beta").is_empty());
    let state = h.state("routines/beta.state");
    assert!(state.contains("observe_since_epoch=5\n"), "{state}");
}

/// C3: the record says when a change was last seen and when a run last finished.
#[test]
fn the_state_record_carries_the_last_trigger_and_the_last_run() {
    let h = realtime_harness("observe-liveness");
    h.wait_for("arm", PATIENT, |h| armed_generation(h, "docs").is_some());
    report_backend(&h, Backend::Inotify, BackendReason::Ok, "armed");
    change(&h, "Projects");
    h.wait_for("the run", PATIENT, |h| h.launched() == 1);
    h.wait_for("idle", PATIENT, Harness::idle);
    let state = h.state("routines/docs.state");
    assert!(!state.contains("last_trigger_epoch=0\n"), "{state}");
    assert!(!state.contains("last_run_epoch=0\n"), "{state}");
    assert!(state.contains("watching_via=native\n"), "{state}");
}

/// A reading that clears inside the grace (a watcher still starting reads blind
/// for a moment) is never announced; one that holds past it is, once.
#[test]
fn a_reading_that_clears_inside_the_grace_is_never_announced() {
    let h = Harness::tuned(
        "observe-grace",
        |home, _| {
            profile(home, "docs");
            routine(
                home,
                "docs",
                "enabled=true\nmode=realtime\ndebounce_seconds=1\nretry_backoff_seconds=10\n",
            );
        },
        |settings| settings.observe_grace = Duration::from_millis(1500),
    );
    h.wait_for("arm", PATIENT, |h| armed_generation(h, "docs").is_some());
    report_backend(
        &h,
        Backend::None,
        BackendReason::WatchSetupFailed,
        "starting",
    );
    let _ = h.ask(Request::Wake);
    thread::sleep(Duration::from_millis(300));
    report_backend(&h, Backend::Inotify, BackendReason::Ok, "armed");
    thread::sleep(Duration::from_millis(2200));
    let _ = h.ask(Request::Wake);
    thread::sleep(Duration::from_millis(300));
    assert!(observation_rows(&h, "routine.unobserved", "docs").is_empty());
    assert!(observation_rows(&h, "routine.observed", "docs").is_empty());
    // Held past the grace it is one row.
    report_backend(&h, Backend::None, BackendReason::WatchSetupFailed, "stuck");
    h.wait_for("the row", PATIENT, |h| {
        let _ = h.ask(Request::Wake);
        observation_rows(h, "routine.unobserved", "docs").len() == 1
    });
    thread::sleep(Duration::from_millis(400));
    assert_eq!(observation_rows(&h, "routine.unobserved", "docs").len(), 1);
}

// ------------------------------------------------------- P8: converged skips

/// A source with one directory whose files are older than the racy margin, so a
/// run that converges may record it.
#[cfg(unix)]
fn quiet_tree(h: &Harness) -> PathBuf {
    let source = volume_source(&h.root, "docs");
    fs::create_dir_all(source.join("a")).unwrap();
    fs::write(source.join("a/f.txt"), "1").unwrap();
    thread::sleep(Duration::from_millis(3300));
    source
}

#[cfg(unix)]
fn converged_dirs(h: &Harness) -> u64 {
    h.ask(Request::Status)["routines"][0]["converged_dirs"]
        .as_u64()
        .unwrap()
}

#[cfg(unix)]
fn skipped(h: &Harness) -> Value {
    h.ask(Request::Status)["routines"][0].clone()
}

/// A change that leaves the directory as the last converged run saw it is not
/// dispatched: it is logged by name, counted in the status, and its pending
/// change is consumed. A real edit afterwards runs.
#[cfg(unix)]
#[test]
fn an_unchanged_directory_after_a_converged_run_is_skipped_and_named() {
    let h = realtime_harness("converged-skip");
    h.world.lock().unwrap().converged_record = true;
    h.wait_for("arm", PATIENT, |h| armed_generation(h, "docs").is_some());
    let source = quiet_tree(&h);
    change(&h, "a");
    h.wait_for("the first run", PATIENT, |h| h.launched() == 1);
    h.wait_for("idle", PATIENT, Harness::idle);
    assert_eq!(
        converged_dirs(&h),
        1,
        "the converged run recorded its directory"
    );

    change(&h, "a");
    h.wait_for("the skip", PATIENT, |h| {
        h.logs()
            .iter()
            .any(|l| l.starts_with("routine_run_skipped "))
    });
    let line = h
        .logs()
        .into_iter()
        .find(|l| l.starts_with("routine_run_skipped "))
        .unwrap();
    assert!(
        line.contains("profile=docs")
            && line.contains("dirs=1")
            && line.contains("reason=unchanged_since_converged"),
        "{line}"
    );
    assert_eq!(h.launched(), 1, "nothing was dispatched");
    // The user sees it in Activity, not only in the log.
    let rows = observation_rows(&h, "routine.skipped", "docs");
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert!(rows[0].2.contains("skipped"), "{rows:?}");
    let status = skipped(&h);
    assert_eq!(status["skipped_runs"], 1, "{status}");
    assert_eq!(status["last_skip_reason"], "unchanged_since_converged");
    assert_eq!(status["pending_change"], false, "the change was consumed");

    // A real edit is not an unchanged directory.
    fs::write(source.join("a/f.txt"), "22").unwrap();
    change(&h, "a");
    h.wait_for("the second run", PATIENT, |h| h.launched() == 2);
}

/// A manual run that fails leaves the NAS in a state no digest vouches for: the
/// record is forgotten, named, and the next change dispatches.
#[cfg(unix)]
#[test]
fn a_run_that_did_not_converge_forgets_what_was_recorded() {
    let h = realtime_harness("converged-forget");
    h.world.lock().unwrap().converged_record = true;
    h.wait_for("arm", PATIENT, |h| armed_generation(h, "docs").is_some());
    let _source = quiet_tree(&h);
    change(&h, "a");
    h.wait_for("the first run", PATIENT, |h| h.launched() == 1);
    h.wait_for("idle", PATIENT, Harness::idle);
    assert_eq!(converged_dirs(&h), 1);

    *h.plan.lock().unwrap() = Plan {
        finish_after: Some(Duration::from_millis(30)),
        exit: 1,
        stop_takes: Duration::ZERO,
    };
    assert_eq!(h.ask(run("docs"))["ok"], true);
    h.wait_for("the failed run", PATIENT, |h| h.launched() == 2);
    h.wait_for("idle", PATIENT, Harness::idle);
    assert_eq!(converged_dirs(&h), 0);
    assert_eq!(
        observation_rows(&h, "routine.converged_forgotten", "docs").len(),
        1
    );
    assert!(
        h.logs()
            .iter()
            .any(|l| l.starts_with("routine_converged_forgotten ") && l.contains("profile=docs")),
        "{:?}",
        h.logs()
    );
    *h.plan.lock().unwrap() = Plan {
        finish_after: Some(Duration::from_millis(30)),
        exit: 0,
        stop_takes: Duration::ZERO,
    };
    change(&h, "a");
    h.wait_for("the next change dispatches", PATIENT, |h| h.launched() == 3);
}

/// An overflow loses events: it forgets the record and syncs the whole tree.
#[cfg(unix)]
#[test]
fn an_overflow_forgets_the_record_and_is_never_skipped() {
    let h = realtime_harness("converged-overflow");
    h.world.lock().unwrap().converged_record = true;
    h.wait_for("arm", PATIENT, |h| armed_generation(h, "docs").is_some());
    let _source = quiet_tree(&h);
    change(&h, "a");
    h.wait_for("the first run", PATIENT, |h| h.launched() == 1);
    h.wait_for("idle", PATIENT, Harness::idle);
    assert_eq!(converged_dirs(&h), 1);
    let _ = h.tx.send(Msg::Change(ChangeEvent::Overflow {
        routine: Some("docs".into()),
    }));
    h.wait_for("the whole-tree run", PATIENT, |h| h.launched() == 2);
    assert_eq!(
        h.logs()
            .iter()
            .filter(|l| l.starts_with("routine_run_skipped "))
            .count(),
        0
    );
}

/// A directory the check cannot read is not an excuse to skip: the dispatch
/// goes ahead, and the reason is named once.
#[cfg(unix)]
#[test]
fn a_check_that_cannot_be_made_dispatches_and_says_why() {
    let h = realtime_harness("converged-unavailable");
    h.wait_for("arm", PATIENT, |h| armed_generation(h, "docs").is_some());
    let source = volume_source(&h.root, "docs");
    fs::create_dir_all(&source).unwrap();
    std::os::unix::fs::symlink("/", source.join("a")).unwrap();
    change(&h, "a");
    h.wait_for("the dispatch", PATIENT, |h| h.launched() == 1);
    let line = h
        .logs()
        .into_iter()
        .find(|l| l.starts_with("routine_skip_check_unavailable "))
        .expect("the cause is named");
    assert!(
        line.contains("profile=docs") && line.contains("not a directory"),
        "{line}"
    );
    let rows = observation_rows(&h, "routine.skip_check_unavailable", "docs");
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert!(rows[0].2.contains("not a directory"), "{rows:?}");
}

// ------------------------------------------------------- P3: delete-only events

fn deleted(h: &Harness, dir: &str) {
    let _ = h.tx.send(Msg::Change(ChangeEvent::Deleted {
        routine: "docs".into(),
        dir: dir.into(),
        depth: Depth::Shallow,
    }));
}

/// A routine that cannot delete has no work for a removal: the event is
/// dropped, counted in the status and named, and a real change still runs.
#[test]
fn a_delete_only_event_is_ignored_by_a_routine_that_cannot_delete() {
    let h = realtime_harness("delete-ignored");
    h.wait_for("arm", PATIENT, |h| armed_generation(h, "docs").is_some());
    deleted(&h, "a");
    deleted(&h, "b");
    h.wait_for("the first event is named", PATIENT, |h| {
        h.logs()
            .iter()
            .any(|l| l.starts_with("routine_events_ignored "))
    });
    let line = h
        .logs()
        .into_iter()
        .find(|l| l.starts_with("routine_events_ignored "))
        .unwrap();
    assert!(
        line.contains("profile=docs")
            && line.contains("reason=delete_only_no_delete")
            && line.contains("first_dir=a"),
        "{line}"
    );
    // Past the debounce nothing was dispatched, and nothing is pending.
    thread::sleep(Duration::from_millis(1600));
    assert_eq!(h.launched(), 0);
    let status = h.ask(Request::Status);
    assert_eq!(status["routines"][0]["pending_change"], false, "{status}");
    assert_eq!(
        status["routines"][0]["ignored_delete_events"], 2,
        "{status}"
    );
    // Only the first of a burst is logged.
    assert_eq!(
        h.logs()
            .iter()
            .filter(|l| l.starts_with("routine_events_ignored "))
            .count(),
        1
    );
    let rows = observation_rows(&h, "routine.events_ignored", "docs");
    assert_eq!(rows.len(), 1, "one row, not one per event: {rows:?}");
    change(&h, "a");
    h.wait_for("a real change runs", PATIENT, |h| h.launched() == 1);
}

/// A routine that may delete treats a removal as any other change.
#[test]
fn a_delete_event_dispatches_for_a_routine_that_can_delete() {
    let h = Harness::new("delete-allowed", |home, _| {
        profile(home, "docs");
        routine(
            home,
            "docs",
            "enabled=true\nmode=realtime\ndebounce_seconds=1\nallow_delete=true\n",
        );
    });
    h.wait_for("arm", PATIENT, |h| armed_generation(h, "docs").is_some());
    deleted(&h, "a");
    h.wait_for("the dispatch", PATIENT, |h| h.launched() == 1);
    assert!(
        !h.logs()
            .iter()
            .any(|l| l.starts_with("routine_events_ignored ")),
        "{:?}",
        h.logs()
    );
}

/// A routine edited since the last convergence may make a run do what an
/// unchanged tree no longer says it would (deletion allowed, say): what was
/// recorded is forgotten.
#[cfg(unix)]
#[test]
fn an_edited_routine_forgets_what_was_recorded() {
    let h = realtime_harness("converged-edited");
    h.world.lock().unwrap().converged_record = true;
    h.wait_for("arm", PATIENT, |h| armed_generation(h, "docs").is_some());
    let _source = quiet_tree(&h);
    change(&h, "a");
    h.wait_for("the first run", PATIENT, |h| h.launched() == 1);
    h.wait_for("idle", PATIENT, Harness::idle);
    assert_eq!(converged_dirs(&h), 1);
    routine(
        &h.root.join("home"),
        "docs",
        "enabled=true\nmode=realtime\ndebounce_seconds=1\nretry_backoff_seconds=10\nallow_delete=true\n",
    );
    let _ = h.ask(Request::Reload);
    h.wait_for("the record is forgotten", PATIENT, |h| {
        converged_dirs(h) == 0
    });
    change(&h, "a");
    h.wait_for("the change runs", PATIENT, |h| h.launched() == 2);
}

// ------------------------------------------------------- canary of a realtime run

fn launched_args(h: &Harness, index: usize) -> Vec<String> {
    h.world.lock().unwrap().launched[index]
        .args
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect()
}

fn flag_value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    args.windows(2)
        .find(|w| w[0] == flag)
        .map(|w| w[1].as_str())
}

fn realtime_with_index(label: &str) -> Harness {
    Harness::tuned(
        label,
        |home, _| {
            profile(home, "docs");
            routine(
                home,
                "docs",
                "enabled=true\nmode=realtime\ndebounce_seconds=1\nretry_backoff_seconds=10\n",
            );
        },
        |settings| settings.pair_index_dir = Some(PathBuf::from("/var/state/cache/pairs")),
    )
}

/// A realtime run for a dirty set draws 2 canaries, including one whose dirty
/// directory is the source root (no `--scope`, which would draw 25); the
/// whole-tree mark, a manual run and a routine that is not realtime keep the
/// default. The flags only ever travel with `--pair-index`.
#[test]
fn a_realtime_dirty_set_run_pins_two_canaries_and_nothing_else_does() {
    let h = realtime_with_index("argv-two");
    h.wait_for("arm", PATIENT, |h| armed_generation(h, "docs").is_some());

    // The root's direct children: no --scope (the whole tree would draw 25), a
    // shallow root element, and still 2.
    change(&h, "");
    h.wait_for("the root run", PATIENT, |h| h.launched() == 1);
    let args = launched_args(&h, 0);
    assert!(!args.iter().any(|a| a == "--scope"), "{args:?}");
    assert_eq!(flag_value(&args, "--scope-shallow"), Some(""), "{args:?}");
    assert_eq!(
        flag_value(&args, "--pair-index-canary"),
        Some("2"),
        "{args:?}"
    );
    assert_eq!(
        flag_value(&args, "--reconcile-canary"),
        Some("2"),
        "{args:?}"
    );
    assert!(args.iter().any(|a| a == "--pair-index"), "{args:?}");
    h.wait_for("idle", PATIENT, Harness::idle);

    // A directory: scoped, 2.
    change(&h, "a/b");
    h.wait_for("the scoped run", PATIENT, |h| h.launched() == 2);
    let args = launched_args(&h, 1);
    assert_eq!(
        flag_value(&args, "--scope-shallow"),
        Some("a/b"),
        "{args:?}"
    );
    assert!(!args.iter().any(|a| a == "--scope"), "{args:?}");
    assert_eq!(
        flag_value(&args, "--pair-index-canary"),
        Some("2"),
        "{args:?}"
    );
    assert_eq!(
        flag_value(&args, "--reconcile-canary"),
        Some("2"),
        "{args:?}"
    );
    h.wait_for("idle", PATIENT, Harness::idle);

    // The whole-tree mark (an overflow): the core's default.
    let _ = h.tx.send(Msg::Change(ChangeEvent::Overflow {
        routine: Some("docs".into()),
    }));
    h.wait_for("the whole-tree run", PATIENT, |h| h.launched() == 3);
    let args = launched_args(&h, 2);
    assert!(
        !args.iter().any(|a| a.ends_with("-canary")) && !args.iter().any(|a| a == "--scope"),
        "{args:?}"
    );
    h.wait_for("idle", PATIENT, Harness::idle);

    // A manual run: the default too.
    assert_eq!(h.ask(run("docs"))["ok"], true);
    h.wait_for("the manual run", PATIENT, |h| h.launched() == 4);
    let args = launched_args(&h, 3);
    assert!(!args.iter().any(|a| a.ends_with("-canary")), "{args:?}");
}

/// Without an index directory there is nothing to draw a canary from, and the
/// core refuses the flags: none is passed.
#[test]
fn no_canary_flags_without_the_pair_index() {
    let h = realtime_harness("argv-plain");
    h.wait_for("arm", PATIENT, |h| armed_generation(h, "docs").is_some());
    change(&h, "a");
    h.wait_for("the run", PATIENT, |h| h.launched() == 1);
    let args = launched_args(&h, 0);
    assert!(!args.iter().any(|a| a.ends_with("-canary")), "{args:?}");
    assert!(!args.iter().any(|a| a == "--pair-index"), "{args:?}");
}

// ------------------------------------------------------- scope elements

fn appeared(h: &Harness, dir: &str) {
    let _ = h.tx.send(Msg::Change(ChangeEvent::Changed {
        routine: "docs".into(),
        dir: dir.into(),
        depth: Depth::Recursive,
    }));
}

/// A directory that appeared is read whole (`--scope`), a file saved elsewhere
/// by its parent's direct children (`--scope-shallow`), and what a recursive
/// element covers is not sent again.
#[test]
fn recursive_and_shallow_elements_are_sent_as_the_core_takes_them() {
    let h = realtime_harness("elements");
    h.wait_for("arm", PATIENT, |h| armed_generation(h, "docs").is_some());
    appeared(&h, "Projects/new");
    change(&h, "Projects/new/inside");
    change(&h, "Projects");
    change(&h, "Other/2026");
    h.wait_for("the dispatch", PATIENT, |h| h.launched() == 1);
    let args = launched_args(&h, 0);
    let after = |flag: &str| -> Vec<&str> {
        args.windows(2)
            .filter(|w| w[0] == flag)
            .map(|w| w[1].as_str())
            .collect()
    };
    assert_eq!(after("--scope"), ["Projects/new"], "{args:?}");
    assert_eq!(
        after("--scope-shallow"),
        ["Other/2026", "Projects"],
        "{args:?}"
    );
    let line = h
        .logs()
        .into_iter()
        .find(|l| l.starts_with("run_started "))
        .unwrap();
    assert!(
        line.contains("subscope=Other/2026:shallow,Projects:shallow,Projects/new"),
        "{line}"
    );
}

/// A recursive root is the whole tree: no scope flags at all.
#[test]
fn a_recursive_root_is_the_whole_tree() {
    let h = realtime_harness("elements-root");
    h.wait_for("arm", PATIENT, |h| armed_generation(h, "docs").is_some());
    appeared(&h, "");
    change(&h, "a");
    h.wait_for("the dispatch", PATIENT, |h| h.launched() == 1);
    let args = launched_args(&h, 0);
    assert!(
        !args
            .iter()
            .any(|a| a == "--scope" || a == "--scope-shallow"),
        "{args:?}"
    );
}

/// A directory that appeared (a recursive element) is never compared with a
/// manifest of direct children: it dispatches, whatever was recorded.
#[cfg(unix)]
#[test]
fn a_recursive_element_is_never_skipped() {
    let h = realtime_harness("converged-recursive");
    h.world.lock().unwrap().converged_record = true;
    h.wait_for("arm", PATIENT, |h| armed_generation(h, "docs").is_some());
    let _source = quiet_tree(&h);
    change(&h, "a");
    h.wait_for("the first run", PATIENT, |h| h.launched() == 1);
    h.wait_for("idle", PATIENT, Harness::idle);
    assert_eq!(converged_dirs(&h), 1);
    appeared(&h, "a");
    h.wait_for("the run", PATIENT, |h| h.launched() == 2);
    assert_eq!(skipped(&h)["skipped_runs"], 0);
}
