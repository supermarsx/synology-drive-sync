//! SC-2: refuse a tree too large for the memory this run may use, by name, before the kernel's
//! out-of-memory killer does it without one.
//!
//! A run holds a local and a remote inventory in full, and a run that executes anything holds a
//! second pair for reconciliation. That is O(entries) with no ceiling: adv3 measured 583 MiB at
//! 500k entries and a SIGKILL during the scan of a 200k tree at `-m 256m`. A SIGKILLed core prints
//! nothing, so the runner recorded `core_failed` with no cause and its retry ladder repeated the
//! kill every interval. On a NAS the OOM killer may pick a DSM service instead.
//!
//! The check runs after the local walk (the entry count is known, the listing has not started)
//! and again after the remote listing (the remote count is known, no digest has been taken). Over
//! budget is a lasting, user-fixable condition: the run does nothing, exits 0 and names
//! `tree_too_large_for_memory` with the numbers and the fix (split the profile).

use std::path::{Path, PathBuf};

/// Heap per inventory entry that does not depend on path length, in bytes.
///
/// Calibrated against `tests/scan_memory.rs`, which measures the real heap of an entry pair with a
/// counting allocator and asserts this model is never below it.
pub const ENTRY_FIXED_BYTES: u64 = 520;
/// Copies of the relative path each inventory entry holds (map key, `relative`, and the derived
/// absolute or remote path).
pub const PATH_COPIES_PER_ENTRY: u64 = 3;
/// What the process needs besides the inventories: code, runtimes, TLS, buffers.
pub const PROCESS_BASELINE_BYTES: u64 = 24 * 1024 * 1024;
/// Generations of both inventories alive at the peak of a run that executes (plan-time plus
/// reconciliation), as a multiplier in tenths.
pub const EXECUTING_GENERATIONS_TENTHS: u64 = 20;
/// The same for a plan-only run: one generation plus digest-pass slack.
pub const PLANNING_GENERATIONS_TENTHS: u64 = 12;
/// Share of the available memory a run may plan to use; the rest is headroom for the page cache,
/// DSM and estimate error.
pub const USABLE_PERCENT: u64 = 80;

const MIB: u64 = 1024 * 1024;

/// Where the available memory figure came from.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BudgetSource {
    /// `--memory-budget`, given by the operator.
    Operator,
    /// The tighter of the cgroup limit's headroom and `MemAvailable`.
    Kernel,
}

/// What this host lets a run use, as read once.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MemoryProbe {
    /// Bytes this process may still allocate before a limit or the kernel says no.
    pub available: u64,
    /// This process's resident set when probed: already allocated, so not needed again.
    pub resident: u64,
    pub source: BudgetSource,
}

/// The outcome of one budget check.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BudgetCheck {
    /// The tree fits, with the numbers that said so.
    Fits(BudgetFigures),
    /// It does not: refuse the run by name.
    TooLarge(BudgetFigures),
    /// No figure could be read (not Linux, or `/proc` unreadable); the run goes ahead unchecked,
    /// and the result says why.
    Unchecked { cause: String },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BudgetFigures {
    pub local_entries: u64,
    pub remote_entries: u64,
    /// Whether the remote count is the listing's, or assumed equal to the local count.
    pub remote_counted: bool,
    pub need_bytes: u64,
    pub available_bytes: u64,
    pub source: BudgetSource,
}

impl BudgetFigures {
    pub fn need_mib(&self) -> u64 {
        self.need_bytes.div_ceil(MIB)
    }

    pub fn available_mib(&self) -> u64 {
        self.available_bytes / MIB
    }

    /// The operator sentence: counts, estimate, availability and the fix.
    pub fn refusal_text(&self) -> String {
        format!(
            "tree_too_large_for_memory: {} local and {} remote entries{} need about {} MiB; {} MiB \
             is available to this run{}; nothing was synced. Split this profile with a scope or \
             into subfolder profiles",
            self.local_entries,
            self.remote_entries,
            if self.remote_counted {
                ""
            } else {
                " (remote assumed equal)"
            },
            self.need_mib(),
            self.available_mib(),
            match self.source {
                BudgetSource::Operator => " (--memory-budget)",
                BudgetSource::Kernel => "",
            },
        )
    }
}

/// Estimated peak heap for `local` and `remote` entries averaging `average_path_bytes`, for a run
/// that executes (`executing`) or only plans.
pub fn estimate_need(local: u64, remote: u64, average_path_bytes: u64, executing: bool) -> u64 {
    let per_entry =
        ENTRY_FIXED_BYTES.saturating_add(PATH_COPIES_PER_ENTRY.saturating_mul(average_path_bytes));
    let one_generation = local.saturating_add(remote).saturating_mul(per_entry);
    let tenths = if executing {
        EXECUTING_GENERATIONS_TENTHS
    } else {
        PLANNING_GENERATIONS_TENTHS
    };
    one_generation
        .saturating_mul(tenths)
        .div_ceil(10)
        .saturating_add(PROCESS_BASELINE_BYTES)
}

/// Decide whether a tree fits `probe`.
///
/// Only what is still to be allocated counts against what is available: the resident set is
/// already paid for, and it includes the inventory the walk just built.
pub fn assess(
    local: u64,
    remote: Option<u64>,
    average_path_bytes: u64,
    executing: bool,
    probe: MemoryProbe,
) -> BudgetCheck {
    let remote_entries = remote.unwrap_or(local);
    let need = estimate_need(local, remote_entries, average_path_bytes, executing);
    let still_needed = need.saturating_sub(probe.resident);
    let usable = probe.available.saturating_mul(USABLE_PERCENT) / 100;
    let figures = BudgetFigures {
        local_entries: local,
        remote_entries,
        remote_counted: remote.is_some(),
        need_bytes: need,
        available_bytes: probe.available.saturating_add(probe.resident),
        source: probe.source,
    };
    if still_needed > usable {
        BudgetCheck::TooLarge(figures)
    } else {
        BudgetCheck::Fits(figures)
    }
}

/// Read what this process may still allocate.
///
/// `operator_mib` (`--memory-budget`) wins everywhere. Otherwise Linux reads the cgroup's limit
/// and usage (v2 `memory.max`/`memory.current`, or v1 `memory.limit_in_bytes`/`usage_in_bytes`)
/// for this process's cgroup and every ancestor, takes the tightest headroom, and the smaller of
/// that and `/proc/meminfo`'s `MemAvailable`. Elsewhere, or when nothing can be read, the reason
/// is returned instead.
pub fn probe(operator_mib: Option<u64>) -> Result<MemoryProbe, String> {
    let resident = resident_bytes(Path::new("/")).unwrap_or(0);
    if let Some(mib) = operator_mib {
        return Ok(MemoryProbe {
            available: mib.saturating_mul(MIB).saturating_sub(resident),
            resident,
            source: BudgetSource::Operator,
        });
    }
    probe_kernel(Path::new("/"), resident)
}

#[cfg(target_os = "linux")]
fn probe_kernel(root: &Path, resident: u64) -> Result<MemoryProbe, String> {
    let available = kernel_available(root)?;
    Ok(MemoryProbe {
        available,
        resident,
        source: BudgetSource::Kernel,
    })
}

#[cfg(not(target_os = "linux"))]
fn probe_kernel(_root: &Path, _resident: u64) -> Result<MemoryProbe, String> {
    Err(
        "memory budget unchecked: only Linux exposes the limits this check reads; pass \
         --memory-budget MiB to check anyway"
            .to_owned(),
    )
}

/// `min(MemAvailable, tightest cgroup headroom)` under `root` (`/` outside tests).
pub fn kernel_available(root: &Path) -> Result<u64, String> {
    let meminfo = std::fs::read_to_string(root.join("proc/meminfo"))
        .map_err(|error| format!("memory budget unchecked: /proc/meminfo: {error}"))?;
    let mem_available = meminfo
        .lines()
        .find_map(|line| line.strip_prefix("MemAvailable:"))
        .and_then(|rest| rest.trim().strip_suffix("kB"))
        .and_then(|kib| kib.trim().parse::<u64>().ok())
        .map(|kib| kib.saturating_mul(1024))
        .ok_or_else(|| "memory budget unchecked: /proc/meminfo has no MemAvailable".to_owned())?;
    Ok(match cgroup_headroom(root) {
        Some(headroom) => headroom.min(mem_available),
        None => mem_available,
    })
}

/// The tightest `limit - usage` over this process's memory cgroup and its ancestors, or `None`
/// when no limit is set anywhere (or none can be read: `MemAvailable` still bounds the run).
pub fn cgroup_headroom(root: &Path) -> Option<u64> {
    let membership = std::fs::read_to_string(root.join("proc/self/cgroup")).ok()?;
    let mut tightest: Option<u64> = None;
    let mut consider = |limit: Option<u64>, usage: Option<u64>| {
        if let (Some(limit), Some(usage)) = (limit, usage) {
            let headroom = limit.saturating_sub(usage);
            tightest = Some(tightest.map_or(headroom, |current| current.min(headroom)));
        }
    };
    for line in membership.lines() {
        let mut fields = line.splitn(3, ':');
        let (Some(_), Some(controllers), Some(path)) =
            (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        if controllers.is_empty() {
            // cgroup v2: one unified hierarchy.
            for directory in ancestors(&root.join("sys/fs/cgroup"), path) {
                consider(
                    read_limit(&directory.join("memory.max")),
                    read_limit(&directory.join("memory.current")),
                );
            }
        } else if controllers
            .split(',')
            .any(|controller| controller == "memory")
        {
            for directory in ancestors(&root.join("sys/fs/cgroup/memory"), path) {
                consider(
                    read_limit(&directory.join("memory.limit_in_bytes")),
                    read_limit(&directory.join("memory.usage_in_bytes")),
                );
            }
        }
    }
    tightest
}

/// `base/path` and every directory above it up to `base`.
fn ancestors(base: &Path, path: &str) -> Vec<PathBuf> {
    let mut directories = vec![base.to_path_buf()];
    let mut current = base.to_path_buf();
    for component in path.split('/').filter(|component| !component.is_empty()) {
        if component == ".." || component == "." {
            continue;
        }
        current = current.join(component);
        directories.push(current.clone());
    }
    directories
}

/// A cgroup byte figure; `max` and v1's "unlimited" (a page-rounded `i64::MAX`) are no limit.
fn read_limit(path: &Path) -> Option<u64> {
    let text = std::fs::read_to_string(path).ok()?;
    let text = text.trim();
    if text == "max" {
        return None;
    }
    let value = text.parse::<u64>().ok()?;
    (value < (1_u64 << 62)).then_some(value)
}

/// This process's resident set from `/proc/self/statm`, where there is one.
fn resident_bytes(root: &Path) -> Option<u64> {
    let statm = std::fs::read_to_string(root.join("proc/self/statm")).ok()?;
    let pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    Some(pages.saturating_mul(4096))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_root(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "sdsync-membudget-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join("proc/self")).unwrap();
        root
    }

    fn write(root: &Path, relative: &str, text: &str) {
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn the_tightest_cgroup_ancestor_and_mem_available_bound_the_run() {
        let root = fake_root("v2");
        write(
            &root,
            "proc/meminfo",
            "MemTotal: 4000000 kB\nMemAvailable:  2000000 kB\n",
        );
        write(
            &root,
            "proc/self/cgroup",
            "0::/system.slice/sdsync.service\n",
        );
        write(&root, "sys/fs/cgroup/memory.max", "max\n");
        write(&root, "sys/fs/cgroup/memory.current", "999\n");
        write(
            &root,
            "sys/fs/cgroup/system.slice/memory.max",
            "1073741824\n",
        );
        write(
            &root,
            "sys/fs/cgroup/system.slice/memory.current",
            "73741824\n",
        );
        write(
            &root,
            "sys/fs/cgroup/system.slice/sdsync.service/memory.max",
            "max\n",
        );
        write(
            &root,
            "sys/fs/cgroup/system.slice/sdsync.service/memory.current",
            "1000\n",
        );
        // The slice's limit (1 GiB - 70.3 MiB used) is tighter than MemAvailable (1.9 GiB).
        assert_eq!(cgroup_headroom(&root), Some(1_000_000_000));
        assert_eq!(kernel_available(&root), Ok(1_000_000_000));
        write(&root, "proc/meminfo", "MemAvailable:  500000 kB\n");
        assert_eq!(kernel_available(&root), Ok(512_000_000));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn cgroup_v1_and_an_unlimited_hierarchy_are_read() {
        let root = fake_root("v1");
        write(&root, "proc/meminfo", "MemAvailable: 1000 kB\n");
        write(
            &root,
            "proc/self/cgroup",
            "12:cpu,cpuacct:/x\n4:memory:/docker/abc\n",
        );
        write(
            &root,
            "sys/fs/cgroup/memory/memory.limit_in_bytes",
            "9223372036854771712\n",
        );
        write(&root, "sys/fs/cgroup/memory/memory.usage_in_bytes", "5\n");
        write(
            &root,
            "sys/fs/cgroup/memory/docker/abc/memory.limit_in_bytes",
            "268435456\n",
        );
        write(
            &root,
            "sys/fs/cgroup/memory/docker/abc/memory.usage_in_bytes",
            "68435456\n",
        );
        assert_eq!(cgroup_headroom(&root), Some(200_000_000));
        write(&root, "proc/self/cgroup", "0::/\n");
        assert_eq!(cgroup_headroom(&root), None, "no limit anywhere");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn unreadable_meminfo_is_named_not_ignored() {
        let root = fake_root("none");
        let error = kernel_available(&root).unwrap_err();
        assert!(error.contains("/proc/meminfo"), "{error}");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_tree_over_budget_is_refused_with_its_numbers_and_the_fix() {
        let probe = MemoryProbe {
            available: 100 * MIB,
            resident: 20 * MIB,
            source: BudgetSource::Kernel,
        };
        // 200k entries at 60-byte paths, executing: about 2 x 400k x 700 B + 24 MiB.
        let check = assess(200_000, None, 60, true, probe);
        let BudgetCheck::TooLarge(figures) = check else {
            panic!("{check:?}");
        };
        assert!(figures.need_mib() > 400, "{figures:?}");
        assert_eq!(figures.available_mib(), 120);
        let text = figures.refusal_text();
        assert!(text.starts_with("tree_too_large_for_memory: 200000 local"));
        assert!(text.contains("(remote assumed equal)"));
        assert!(text.contains("Split this profile"));
        // The same tree fits when only planning, against a roomier host.
        let roomy = MemoryProbe {
            available: 2_048 * MIB,
            ..probe
        };
        assert!(matches!(
            assess(200_000, Some(200_000), 60, false, roomy),
            BudgetCheck::Fits(_)
        ));
    }

    #[test]
    fn an_operator_budget_is_honoured_on_every_platform() {
        let probe = probe(Some(64)).unwrap();
        assert_eq!(probe.source, BudgetSource::Operator);
        assert!(probe.available <= 64 * MIB);
    }
}
