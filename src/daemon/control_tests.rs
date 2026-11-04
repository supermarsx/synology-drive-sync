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
            let oom = {
                let mut world = world.lock().unwrap();
                world.running -= 1;
                world.oom_proven && exit == 137
            };
            sink(ExecEvent::Finished(Box::new(RunReport {
                exit,
                ending,
                stderr_tail: if exit == 0 {
                    Vec::new()
                } else {
                    b"error: fake failure\n".to_vec()
                },
                records: Vec::new(),
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
        ask(&self.tx, request, Duration::from_secs(5))
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

fn profile(home: &Path, name: &str) {
    fs::write(
        home.join(format!("config/profiles.d/{name}.toml")),
        format!("source = \"/volume1/{name}\"\nmax-delete = 100\n"),
    )
    .unwrap();
}

fn routine(home: &Path, name: &str, conf: &str) {
    fs::write(home.join(format!("config/routines.d/{name}.conf")), conf).unwrap();
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
    h.wait_for("the run", Duration::from_secs(5), |h| h.launched() == 1);
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
    h.wait_for("the run", Duration::from_secs(5), |h| h.launched() == 1);
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
    h.wait_for("the cancel", Duration::from_secs(5), Harness::idle);
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
    h.wait_for("the run", Duration::from_secs(5), |h| h.launched() == 1);
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
    h.wait_for("the run", Duration::from_secs(5), |h| h.launched() == 1);
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
    }));
}

/// A realtime change is armed, debounced and dispatched as a scoped run of the
/// lowest common ancestor.
#[test]
fn a_realtime_change_dispatches_a_scoped_run_after_its_debounce() {
    let h = realtime_harness("scope");
    h.wait_for("arm", Duration::from_secs(5), |h| {
        h.changes
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|c| c.starts_with("arm docs /volume1/docs gen="))
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
    h.wait_for("the dispatch", Duration::from_secs(5), |h| {
        h.launched() == 1
    });
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
    assert!(
        args.windows(2)
            .any(|w| w[0] == "--scope" && w[1] == "Projects/2026"),
        "{args:?}"
    );
    h.wait_for("idle", Duration::from_secs(5), Harness::idle);
    let state = h.state("routines/docs.state");
    assert!(
        state.contains("state=succeeded\n") && state.contains("backend=inotify\n"),
        "{state}"
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
        h.wait_for("the dispatch", Duration::from_secs(5), |h| {
            h.launched() == 1
        });
        h.wait_for("idle", Duration::from_secs(5), Harness::idle);
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
    h.wait_for("the dispatch", Duration::from_secs(5), |h| {
        h.launched() == 1
    });
    h.ask(Request::Cancel { id: None });
    h.wait_for("idle", Duration::from_secs(5), Harness::idle);
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
    h.wait_for("the first run", Duration::from_secs(5), |h| {
        h.launched() == 1
    });
    change(&h, "b");
    h.wait_for("the second run", Duration::from_secs(8), |h| {
        h.launched() == 2
    });
    let args = h.world.lock().unwrap().launched[1]
        .args
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    assert!(
        args.windows(2).any(|w| w[0] == "--scope" && w[1] == "b"),
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
    h.wait_for("the dispatch", Duration::from_secs(5), |h| {
        h.launched() == 1
    });
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
    h.wait_for("the run", Duration::from_secs(5), |h| {
        h.launched() == 1 && h.idle()
    });
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
    h.wait_for("reconcile", Duration::from_secs(5), |h| {
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

const ALPHA_FRAGMENT: &str = "source = \"/volume1/alpha\"\nmax-delete = 100\n";

/// A block record for `alpha` tied to its current settings.
fn plant_block(var: &Path) {
    let lasting = super::super::lasting::Lasting {
        reason: "core_oom_killed".into(),
        detail: "the sync core was killed by the out-of-memory killer (exit 137)".into(),
        since: T0 - 100,
        fingerprint: super::super::lasting::fingerprint(Some(ALPHA_FRAGMENT.as_bytes()), None),
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
    h.wait_for("the hold", Duration::from_secs(5), |h| {
        suppressed_logs(h) == 1
    });
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
    h.wait_for("the hold", Duration::from_secs(5), |h| {
        suppressed_logs(h) == 1
    });
    fs::write(&fragment, ALPHA_FRAGMENT).unwrap();
    let _ = h.ask(Request::Reload);
    assert_eq!(
        h.ask(Request::Status)["lasting"]["alpha"]["reason"],
        "core_oom_killed"
    );
    assert!(h.var.join("state/lasting/alpha.state").exists());
    fs::write(&fragment, "source = \"/volume1/alpha\"\nmax-delete = 50\n").unwrap();
    let _ = h.ask(Request::Reload);
    h.wait_for("the run", Duration::from_secs(5), |h| h.launched() == 1);
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
    h.wait_for("the hold", Duration::from_secs(5), |h| {
        suppressed_logs(h) == 1
    });
    *h.plan.lock().unwrap() = Plan {
        finish_after: Some(Duration::from_millis(30)),
        exit: 137,
        stop_takes: Duration::ZERO,
    };
    h.world.lock().unwrap().oom_proven = true;
    assert_eq!(h.ask(run("alpha"))["ok"], true);
    h.wait_for("the run", Duration::from_secs(5), |h| h.launched() >= 1);
    h.wait_for("idle", Duration::from_secs(5), Harness::idle);
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
    h.wait_for("the hold", Duration::from_secs(5), |h| {
        suppressed_logs(h) == 1
    });
    fs::remove_file(h.root.join("home/config/profiles.d/alpha.toml")).unwrap();
    let _ = h.ask(Request::Reload);
    assert!(!h.var.join("state/lasting/alpha.state").exists());
    assert_eq!(h.ask(Request::Status)["lasting"], json!({}));
    assert!(
        h.logs()
            .iter()
            .any(|l| l == "lasting_cleared profile=alpha cause=profile_removed")
    );
}

/// A corrupt record is named and still holds: never read as lifted.
#[test]
fn a_corrupt_block_record_is_named_and_still_holds() {
    let h = Harness::new("lasting-corrupt", |home, var| {
        due_alpha(home, var);
        fs::create_dir_all(var.join("state/lasting")).unwrap();
        fs::write(var.join("state/lasting/alpha.state"), "garbage\n").unwrap();
    });
    h.wait_for("the hold", Duration::from_secs(5), |h| {
        suppressed_logs(h) == 1
    });
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
    h.wait_for("the run", Duration::from_secs(5), |h| h.launched() == 1);
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
    h.wait_for("the run", Duration::from_secs(5), |h| h.launched() == 1);
    h.wait_for("the block", Duration::from_secs(5), |h| {
        h.var.join("state/lasting/alpha.state").exists()
    });
    h.wait_for("the routine record", Duration::from_secs(5), |h| {
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
    h.wait_for("arm", Duration::from_secs(5), |h| {
        armed_generation(h, "docs").is_some()
    });
    let first = armed_generation(&h, "docs").unwrap();
    fs::write(
        h.root.join("home/config/profiles.d/docs.toml"),
        "source = \"/volume2/docs\"\nmax-delete = 100\n",
    )
    .unwrap();
    let _ = h.ask(Request::Reload);
    h.wait_for("re-arm", Duration::from_secs(5), |h| {
        h.changes
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|c| c.starts_with("arm docs /volume2/docs gen="))
    });
    let second = armed_generation(&h, "docs").unwrap();
    assert!(second > first, "{first} -> {second}");
    {
        let calls = h.changes.calls.lock().unwrap();
        let disarm = calls.iter().position(|c| c == "disarm docs");
        let rearm = calls
            .iter()
            .position(|c| c.starts_with("arm docs /volume2/docs"));
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
        },
    ));
    thread::sleep(Duration::from_millis(1500));
    assert_eq!(h.launched(), 0, "a stale event dispatched a run");
    let _ = h.tx.send(Msg::Armed(
        second,
        ChangeEvent::Changed {
            routine: "docs".into(),
            dir: "new".into(),
        },
    ));
    h.wait_for("the dispatch", Duration::from_secs(5), |h| {
        h.launched() == 1
    });
    let args = h.world.lock().unwrap().launched[0]
        .args
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    assert!(
        args.windows(2).any(|w| w[0] == "--scope" && w[1] == "new"),
        "{args:?}"
    );
}

/// An unchanged reload does not re-arm (a re-arm costs a whole-tree walk).
#[test]
fn an_unchanged_reload_keeps_the_watch() {
    let h = realtime_harness("keep");
    h.wait_for("arm", Duration::from_secs(5), |h| {
        armed_generation(h, "docs").is_some()
    });
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
    h.wait_for("the deferral", Duration::from_secs(5), |h| {
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
    h.wait_for("the run", Duration::from_secs(5), |h| h.launched() == 1);
    let scope = fs::read_to_string(run_dir.join("run.scope")).unwrap();
    let lock = fs::read_to_string(run_dir.join("run.lock/pid")).unwrap();
    assert_eq!(
        scope,
        format!("{lock}profiles=alpha,beta\n"),
        "all is expanded"
    );
    h.wait_for("idle", Duration::from_secs(5), Harness::idle);
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
    h.wait_for("the first failure", Duration::from_secs(5), launched(1));
    h.advance(11);
    h.wait_for("the first escalation", Duration::from_secs(5), |h| {
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
    h.wait_for("idle", Duration::from_secs(5), Harness::idle);
    h.advance(61);
    h.wait_for("the third run", Duration::from_secs(5), launched(3));
    h.advance(11);
    h.wait_for("the held escalation", Duration::from_secs(5), |h| {
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
    h.wait_for("arm", Duration::from_secs(5), |h| {
        armed_generation(h, "docs").is_some()
    });
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
    h.wait_for("the deferral", Duration::from_secs(5), |h| {
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
    h.wait_for("the run", Duration::from_secs(5), |h| h.launched() >= 1);
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
    h.wait_for("the run", Duration::from_secs(5), |h| h.launched() == 1);
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
    h.wait_for("arm", Duration::from_secs(5), armed);
    let conf = h.root.join("home/config/routines.d/docs.conf");
    fs::set_permissions(&conf, fs::Permissions::from_mode(0o000)).unwrap();
    if fs::read(&conf).is_ok() {
        // Running as root: permissions do not bind; nothing to prove here.
        return;
    }
    let named = |h: &Harness, needle: &str| {
        h.logs()
            .iter()
            .filter(|l| l.starts_with("config_unreadable") && l.contains(needle))
            .count()
    };
    let _ = h.ask(Request::Reload);
    h.wait_for("the conf named", Duration::from_secs(5), |h| {
        named(h, "docs.conf") == 1
    });
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
    h.wait_for("the directory named", Duration::from_secs(5), |h| {
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
    h.wait_for("the recovery named", Duration::from_secs(5), |h| {
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
    if fs::read(h.var.join("state/routines/alpha.state")).is_ok() {
        return;
    }
    h.wait_for("the state named", Duration::from_secs(5), |h| {
        h.logs()
            .iter()
            .any(|l| l.starts_with("config_unreadable") && l.contains("routines/alpha.state"))
    });
    h.wait_for("the hold named", Duration::from_secs(5), |h| {
        h.logs()
            .iter()
            .any(|l| l.starts_with("routine_state_invalid profile=alpha field=unreadable"))
    });
    thread::sleep(Duration::from_millis(300));
    assert_eq!(h.launched(), 0, "never run from a state it could not read");
}

// ---------------------------------------------------------------- fix6-core

/// Whether permissions bind for this process. As root they do not, and a test
/// that needs an unreadable file says so on stderr instead of passing quietly.
#[cfg(unix)]
fn permissions_bind(path: &Path, test: &str) -> bool {
    if fs::read(path).is_ok() {
        eprintln!(
            "SKIP {test}: {} is readable at mode 000 (running as root); nothing is proved",
            path.display()
        );
        return false;
    }
    true
}

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
    if !permissions_bind(&state, "an_unreadable_profile_state_is_kept_not_reset") {
        return;
    }
    *h.plan.lock().unwrap() = Plan {
        finish_after: Some(Duration::from_millis(20)),
        exit: 1,
        stop_takes: Duration::ZERO,
    };
    h.ask(run("alpha"));
    h.wait_for("the run", Duration::from_secs(5), |h| {
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
    h.wait_for("the first controller.state", Duration::from_secs(5), |h| {
        h.state("controller.state").starts_with("state=running\n")
    });
    let degraded = super::super::state::ControllerDegraded {
        reason: "audit_unavailable",
        since: T0 - 40,
        cause: "the audit log could not be reconciled (exit=73)".into(),
    };
    h.tx.send(Msg::AuditHealth(Some(degraded.clone()))).unwrap();
    h.wait_for("the degradation written", Duration::from_secs(5), |h| {
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
    h.wait_for("the run", Duration::from_secs(5), |h| {
        h.launched() == 1 && h.idle()
    });
    h.wait_for("the rewrite keeps it", Duration::from_secs(5), |h| {
        let state = h.state("controller.state");
        state.contains(&format!("updated_epoch={}\n", T0 + 1))
            && state.contains("degraded=audit_unavailable\n")
    });
    h.tx.send(Msg::AuditHealth(None)).unwrap();
    h.wait_for("the degradation cleared", Duration::from_secs(5), |h| {
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

/// N-D5 (status half, asked by fix6-watch): the change source's account of
/// a degraded backend reaches `ctl status` as `backend_detail`, the latest
/// one, even when backend and reason did not change.
#[test]
fn status_carries_the_backends_detail() {
    let h = realtime_harness("backend-detail");
    h.wait_for("arm", Duration::from_secs(5), |h| {
        armed_generation(h, "docs").is_some()
    });
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
        h.wait_for("the detail in status", Duration::from_secs(5), |h| {
            detail(h) == account
        });
    }
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
    if !permissions_bind(&state, "an_unreadable_dependency_state_is_named") {
        return;
    }
    h.wait_for("the dependency named", Duration::from_secs(5), |h| {
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
    h.wait_for("the recovery", Duration::from_secs(5), |h| {
        h.logs()
            .iter()
            .any(|l| l.starts_with("config_recovered") && l.contains("profiles/beta.state"))
    });
    h.wait_for("the run once it can be read", Duration::from_secs(5), |h| {
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
    if !permissions_bind(&state, "a_removed_routines_unreadable_record_is_forgotten") {
        return;
    }
    h.wait_for("the state named", Duration::from_secs(5), |h| {
        unreadable_paths(h)
            .iter()
            .any(|p| p.contains("routines/alpha.state"))
    });
    fs::remove_file(h.root.join("home/config/routines.d/alpha.conf")).unwrap();
    let _ = h.ask(Request::Reload);
    h.wait_for("the record forgotten", Duration::from_secs(5), |h| {
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
    if fs::read_dir(&dir).is_ok() {
        eprintln!(
            "SKIP status_names_an_unreadable_profile_directory: {} is readable at mode 000 (running as root)",
            dir.display()
        );
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
