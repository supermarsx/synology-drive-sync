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
            world.lock().unwrap().running -= 1;
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
                oom_proven: None,
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
}

static NEXT: AtomicU64 = AtomicU64::new(0);

const T0: u64 = 1_780_000_000;

fn grace() -> Duration {
    Duration::from_millis(300)
}

impl Harness {
    fn new(label: &str, setup: impl FnOnce(&Path, &Path)) -> Self {
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
        let mutations = Arc::new(SlowMutations {
            calls: AtomicUsize::new(0),
        });
        let mut settings = Settings::default();
        settings.timings.cancel_grace = grace();
        settings.timings.kill_wait = Duration::from_millis(200);
        settings.wake_cap = Duration::from_millis(100);
        settings.publish_run_lock = false;
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
            changes: Some(Box::new(changes.clone())),
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
            .any(|a| a.contains("--all-profiles --max-total-delete 100 --no-delete")),
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
            .any(|c| c == "arm docs /volume1/docs")
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
