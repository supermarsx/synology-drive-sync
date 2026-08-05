//! Deterministic safety validation for running more than one sync job.
//!
//! This module deliberately stops at preflight. It does not decide how jobs are scheduled or
//! whether execution should continue after one job fails. Callers build a [`BatchCatalog`], select
//! the intended jobs, plan every selected job, and pass the resulting deletion counts to
//! [`ValidatedBatch::preflight_deletions`] before performing any remote mutation.

use std::collections::BTreeMap;

use reqwest::Url;
use thiserror::Error;

use crate::path::RemoteRoot;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BatchJob {
    name: String,
    endpoint: String,
    username: String,
    remote: RemoteRoot,
    delete: bool,
    max_delete: usize,
}

impl BatchJob {
    /// Build a job from the same normalized URL and validated remote-root types used by runtime
    /// integration. URL normalization here is defensive and only establishes a stable comparison
    /// key; transport policy such as whether HTTP is allowed remains the caller's responsibility.
    pub fn new(
        name: impl Into<String>,
        endpoint: Url,
        username: impl Into<String>,
        remote: RemoteRoot,
        delete: bool,
        max_delete: usize,
    ) -> Result<Self, BatchError> {
        let name = name.into();
        validate_job_name(&name)?;
        let username = username.into();
        validate_username(&name, &username)?;
        let endpoint = normalize_endpoint(&name, endpoint)?;
        Ok(Self {
            name,
            endpoint,
            username,
            remote,
            delete,
            max_delete,
        })
    }

    /// Convenience constructor for configuration and CLI adapters.
    pub fn parse(
        name: impl Into<String>,
        endpoint: &str,
        username: impl Into<String>,
        remote: &str,
        delete: bool,
        max_delete: usize,
    ) -> Result<Self, BatchError> {
        let name = name.into();
        validate_job_name(&name)?;
        let endpoint = Url::parse(endpoint.trim()).map_err(|_| BatchError::InvalidEndpoint {
            job_name: name.clone(),
            reason: "the endpoint is not a valid URL",
        })?;
        let remote_value = remote.to_owned();
        let remote =
            RemoteRoot::parse(&remote_value).map_err(|error| BatchError::InvalidRemote {
                job_name: name.clone(),
                remote: remote_value,
                reason: error.to_string(),
            })?;
        Self::new(name, endpoint, username, remote, delete, max_delete)
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    pub fn username(&self) -> &str {
        &self.username
    }

    pub fn remote(&self) -> &RemoteRoot {
        &self.remote
    }

    pub fn delete(&self) -> bool {
        self.delete
    }

    pub fn max_delete(&self) -> usize {
        self.max_delete
    }
}

/// A deterministic catalog may contain alternative jobs whose roots overlap. Overlap becomes an
/// error only when those alternatives are selected into the same [`ValidatedBatch`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BatchCatalog {
    jobs: Vec<BatchJob>,
}

impl BatchCatalog {
    pub fn new(jobs: impl IntoIterator<Item = BatchJob>) -> Result<Self, BatchError> {
        let mut jobs = jobs.into_iter().collect::<Vec<_>>();
        if jobs.is_empty() {
            return Err(BatchError::EmptyBatch);
        }
        sort_jobs(&mut jobs);
        for pair in jobs.windows(2) {
            if pair[0].name == pair[1].name {
                return Err(BatchError::DuplicateJobName {
                    job_name: pair[0].name.clone(),
                });
            }
        }
        Ok(Self { jobs })
    }

    pub fn jobs(&self) -> &[BatchJob] {
        &self.jobs
    }

    pub fn select_all(&self) -> Result<ValidatedBatch, BatchError> {
        ValidatedBatch::from_selected(self.jobs.clone())
    }

    /// Select exact, case-sensitive job names. Requested order never controls execution order;
    /// the returned batch is sorted deterministically by job name.
    pub fn select<I, S>(&self, names: I) -> Result<ValidatedBatch, BatchError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut names = names
            .into_iter()
            .map(|name| name.as_ref().to_owned())
            .collect::<Vec<_>>();
        if names.is_empty() {
            return Err(BatchError::EmptySelection);
        }
        names.sort();
        for pair in names.windows(2) {
            if pair[0] == pair[1] {
                return Err(BatchError::DuplicateSelection {
                    job_name: pair[0].clone(),
                });
            }
        }

        let mut selected = Vec::with_capacity(names.len());
        for name in names {
            let index = self
                .jobs
                .binary_search_by(|job| job.name.cmp(&name))
                .map_err(|_| BatchError::UnknownSelection {
                    job_name: name.clone(),
                })?;
            selected.push(self.jobs[index].clone());
        }
        ValidatedBatch::from_selected(selected)
    }
}

/// A non-empty, deterministically ordered set of jobs whose remote roots cannot overlap on one
/// normalized endpoint. Username is intentionally not part of the overlap key: changing accounts
/// must not make two writers to the same NAS path appear independent.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValidatedBatch {
    jobs: Vec<BatchJob>,
}

impl ValidatedBatch {
    /// Validate an already selected collection directly.
    pub fn new(jobs: impl IntoIterator<Item = BatchJob>) -> Result<Self, BatchError> {
        BatchCatalog::new(jobs)?.select_all()
    }

    fn from_selected(mut jobs: Vec<BatchJob>) -> Result<Self, BatchError> {
        if jobs.is_empty() {
            return Err(BatchError::EmptyBatch);
        }
        sort_jobs(&mut jobs);
        for (index, left) in jobs.iter().enumerate() {
            for right in jobs.iter().skip(index + 1) {
                if left.endpoint == right.endpoint && roots_overlap(&left.remote, &right.remote) {
                    return Err(BatchError::OverlappingRemoteRoots {
                        endpoint: left.endpoint.clone(),
                        first_job: left.name.clone(),
                        first_remote: left.remote.as_str().to_owned(),
                        second_job: right.name.clone(),
                        second_remote: right.remote.as_str().to_owned(),
                    });
                }
            }
        }
        Ok(Self { jobs })
    }

    pub fn jobs(&self) -> &[BatchJob] {
        &self.jobs
    }

    pub fn configured_deletion_limit(&self) -> Result<usize, BatchError> {
        self.jobs
            .iter()
            .filter(|job| job.delete)
            .try_fold(0_usize, |total, job| {
                total
                    .checked_add(job.max_delete)
                    .ok_or(BatchError::DeletionCountOverflow)
            })
    }

    /// Check per-job and optional aggregate deletion caps using complete plan results for every
    /// selected job. Requiring an explicit zero for non-destructive jobs prevents a missing plan
    /// from being mistaken for a safe plan.
    pub fn preflight_deletions<I, S>(
        &self,
        planned: I,
        aggregate_limit: Option<usize>,
    ) -> Result<DeletionPreflight, BatchError>
    where
        I: IntoIterator<Item = (S, usize)>,
        S: AsRef<str>,
    {
        let mut planned = planned
            .into_iter()
            .map(|(name, count)| (name.as_ref().to_owned(), count))
            .collect::<Vec<_>>();
        planned.sort_by(|left, right| left.0.cmp(&right.0));
        for pair in planned.windows(2) {
            if pair[0].0 == pair[1].0 {
                return Err(BatchError::DuplicateDeletionPlan {
                    job_name: pair[0].0.clone(),
                });
            }
        }

        let selected_by_name = self
            .jobs
            .iter()
            .map(|job| (job.name.as_str(), job))
            .collect::<BTreeMap<_, _>>();
        for (name, _) in &planned {
            if !selected_by_name.contains_key(name.as_str()) {
                return Err(BatchError::UnknownDeletionPlan {
                    job_name: name.clone(),
                });
            }
        }
        let planned_by_name = planned.into_iter().collect::<BTreeMap<_, _>>();

        let configured_limit = self.configured_deletion_limit()?;
        let mut total_planned = 0_usize;
        let mut per_job = Vec::with_capacity(self.jobs.len());
        for job in &self.jobs {
            let count = planned_by_name.get(&job.name).copied().ok_or_else(|| {
                BatchError::MissingDeletionPlan {
                    job_name: job.name.clone(),
                }
            })?;
            if !job.delete && count != 0 {
                return Err(BatchError::DeletionDisabled {
                    job_name: job.name.clone(),
                    planned: count,
                });
            }
            let job_limit = if job.delete { job.max_delete } else { 0 };
            if count > job_limit {
                return Err(BatchError::JobDeletionLimit {
                    job_name: job.name.clone(),
                    planned: count,
                    maximum: job_limit,
                });
            }
            total_planned = total_planned
                .checked_add(count)
                .ok_or(BatchError::DeletionCountOverflow)?;
            per_job.push(JobDeletionPreflight {
                job_name: job.name.clone(),
                planned: count,
                maximum: job_limit,
            });
        }

        if let Some(maximum) = aggregate_limit
            && total_planned > maximum
        {
            return Err(BatchError::AggregateDeletionLimit {
                planned: total_planned,
                maximum,
            });
        }
        let effective_limit = aggregate_limit
            .map(|limit| limit.min(configured_limit))
            .unwrap_or(configured_limit);
        Ok(DeletionPreflight {
            per_job,
            total_planned,
            configured_limit,
            requested_aggregate_limit: aggregate_limit,
            effective_limit,
            remaining: effective_limit.saturating_sub(total_planned),
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JobDeletionPreflight {
    pub job_name: String,
    pub planned: usize,
    pub maximum: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeletionPreflight {
    pub per_job: Vec<JobDeletionPreflight>,
    pub total_planned: usize,
    pub configured_limit: usize,
    pub requested_aggregate_limit: Option<usize>,
    pub effective_limit: usize,
    pub remaining: usize,
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum BatchError {
    #[error("a batch must contain at least one job")]
    EmptyBatch,

    #[error("a batch selection must name at least one job")]
    EmptySelection,

    #[error("invalid batch job name {job_name:?}: {reason}")]
    InvalidJobName {
        job_name: String,
        reason: &'static str,
    },

    #[error("invalid username for batch job {job_name:?}: {reason}")]
    InvalidUsername {
        job_name: String,
        reason: &'static str,
    },

    #[error("invalid endpoint for batch job {job_name:?}: {reason}")]
    InvalidEndpoint {
        job_name: String,
        reason: &'static str,
    },

    #[error("invalid remote root {remote:?} for batch job {job_name:?}: {reason}")]
    InvalidRemote {
        job_name: String,
        remote: String,
        reason: String,
    },

    #[error("batch job name {job_name:?} is defined more than once")]
    DuplicateJobName { job_name: String },

    #[error("batch job {job_name:?} was selected more than once")]
    DuplicateSelection { job_name: String },

    #[error("selected batch job {job_name:?} does not exist")]
    UnknownSelection { job_name: String },

    #[error(
        "batch jobs {first_job:?} ({first_remote}) and {second_job:?} ({second_remote}) have equal or nested roots on endpoint {endpoint}"
    )]
    OverlappingRemoteRoots {
        endpoint: String,
        first_job: String,
        first_remote: String,
        second_job: String,
        second_remote: String,
    },

    #[error("deletion plan for batch job {job_name:?} was supplied more than once")]
    DuplicateDeletionPlan { job_name: String },

    #[error("deletion plan refers to unselected batch job {job_name:?}")]
    UnknownDeletionPlan { job_name: String },

    #[error("selected batch job {job_name:?} has no deletion-plan result")]
    MissingDeletionPlan { job_name: String },

    #[error("batch job {job_name:?} planned {planned} deletions even though deletion is disabled")]
    DeletionDisabled { job_name: String, planned: usize },

    #[error(
        "batch job {job_name:?} planned {planned} deletions, exceeding its maximum of {maximum}"
    )]
    JobDeletionLimit {
        job_name: String,
        planned: usize,
        maximum: usize,
    },

    #[error("batch planned {planned} deletions, exceeding the aggregate maximum of {maximum}")]
    AggregateDeletionLimit { planned: usize, maximum: usize },

    #[error("aggregate batch deletion counts exceed this platform's numeric range")]
    DeletionCountOverflow,
}

fn normalize_endpoint(job_name: &str, endpoint: Url) -> Result<String, BatchError> {
    if !matches!(endpoint.scheme(), "http" | "https") {
        return Err(BatchError::InvalidEndpoint {
            job_name: job_name.to_owned(),
            reason: "the endpoint scheme must be HTTP or HTTPS",
        });
    }
    if endpoint.host_str().is_none() {
        return Err(BatchError::InvalidEndpoint {
            job_name: job_name.to_owned(),
            reason: "the endpoint must have a host",
        });
    }
    if !endpoint.username().is_empty() || endpoint.password().is_some() {
        return Err(BatchError::InvalidEndpoint {
            job_name: job_name.to_owned(),
            reason: "the endpoint must not contain credentials",
        });
    }
    if endpoint.query().is_some() || endpoint.fragment().is_some() {
        return Err(BatchError::InvalidEndpoint {
            job_name: job_name.to_owned(),
            reason: "the endpoint must not contain a query string or fragment",
        });
    }
    let mut normalized = endpoint.to_string();
    if !normalized.ends_with('/') {
        normalized.push('/');
    }
    Ok(normalized)
}

fn validate_job_name(name: &str) -> Result<(), BatchError> {
    if name.is_empty() || name.trim() != name {
        return Err(BatchError::InvalidJobName {
            job_name: name.to_owned(),
            reason: "the name must be non-empty and have no surrounding whitespace",
        });
    }
    if name.chars().any(char::is_control) {
        return Err(BatchError::InvalidJobName {
            job_name: name.to_owned(),
            reason: "control characters are not allowed",
        });
    }
    Ok(())
}

fn validate_username(job_name: &str, username: &str) -> Result<(), BatchError> {
    if username.is_empty() || username.trim() != username {
        return Err(BatchError::InvalidUsername {
            job_name: job_name.to_owned(),
            reason: "the username must be non-empty and have no surrounding whitespace",
        });
    }
    if username.chars().any(char::is_control) {
        return Err(BatchError::InvalidUsername {
            job_name: job_name.to_owned(),
            reason: "control characters are not allowed",
        });
    }
    Ok(())
}

fn sort_jobs(jobs: &mut [BatchJob]) {
    jobs.sort_by(|left, right| {
        left.name
            .cmp(&right.name)
            .then_with(|| left.endpoint.cmp(&right.endpoint))
            .then_with(|| left.remote.as_str().cmp(right.remote.as_str()))
            .then_with(|| left.username.cmp(&right.username))
    });
}

fn roots_overlap(left: &RemoteRoot, right: &RemoteRoot) -> bool {
    left.as_str() == right.as_str()
        || left.contains_child(right.as_str())
        || right.contains_child(left.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(
        name: &str,
        endpoint: &str,
        username: &str,
        remote: &str,
        delete: bool,
        max_delete: usize,
    ) -> BatchJob {
        BatchJob::parse(name, endpoint, username, remote, delete, max_delete).unwrap()
    }

    #[test]
    fn catalog_and_selection_are_deterministic() {
        let catalog = BatchCatalog::new([
            job("zeta", "https://nas.test", "alice", "/team/z", false, 10),
            job("alpha", "https://nas.test", "alice", "/team/a", false, 10),
            job("middle", "https://nas.test", "alice", "/team/m", false, 10),
        ])
        .unwrap();
        assert_eq!(
            catalog
                .jobs()
                .iter()
                .map(BatchJob::name)
                .collect::<Vec<_>>(),
            ["alpha", "middle", "zeta"]
        );
        let selected = catalog.select(["zeta", "alpha"]).unwrap();
        assert_eq!(
            selected
                .jobs()
                .iter()
                .map(BatchJob::name)
                .collect::<Vec<_>>(),
            ["alpha", "zeta"]
        );
    }

    #[test]
    fn every_input_permutation_produces_the_same_order() {
        let jobs = [
            job("a", "https://nas.test", "alice", "/team/a", false, 0),
            job("b", "https://nas.test", "alice", "/team/b", false, 0),
            job("c", "https://nas.test", "alice", "/team/c", false, 0),
        ];
        for order in [
            [0, 1, 2],
            [0, 2, 1],
            [1, 0, 2],
            [1, 2, 0],
            [2, 0, 1],
            [2, 1, 0],
        ] {
            let batch = ValidatedBatch::new(order.map(|index| jobs[index].clone())).unwrap();
            assert_eq!(
                batch.jobs().iter().map(BatchJob::name).collect::<Vec<_>>(),
                ["a", "b", "c"]
            );
        }
    }

    #[test]
    fn duplicate_definitions_and_selections_fail_closed() {
        assert!(matches!(
            BatchCatalog::new([
                job("same", "https://one.test", "alice", "/team/a", false, 0),
                job("same", "https://two.test", "alice", "/team/b", false, 0),
            ]),
            Err(BatchError::DuplicateJobName { job_name }) if job_name == "same"
        ));

        let catalog = BatchCatalog::new([
            job("a", "https://nas.test", "alice", "/team/a", false, 0),
            job("b", "https://nas.test", "alice", "/team/b", false, 0),
        ])
        .unwrap();
        assert!(matches!(
            catalog.select(["b", "b"]),
            Err(BatchError::DuplicateSelection { job_name }) if job_name == "b"
        ));
        assert!(matches!(
            catalog.select(["missing"]),
            Err(BatchError::UnknownSelection { job_name }) if job_name == "missing"
        ));
        assert!(matches!(
            catalog.select(std::iter::empty::<&str>()),
            Err(BatchError::EmptySelection)
        ));
    }

    #[test]
    fn equal_and_nested_roots_are_rejected_even_across_usernames() {
        for (left, right) in [
            ("/team/root", "/team/root"),
            ("/team/root", "/team/root/child"),
            ("/team/root/child", "/team/root"),
        ] {
            assert!(matches!(
                ValidatedBatch::new([
                    job("a", "https://nas.test", "alice", left, false, 0),
                    job("b", "https://nas.test/", "bob", right, false, 0),
                ]),
                Err(BatchError::OverlappingRemoteRoots { .. })
            ));
        }
    }

    #[test]
    fn component_neighbors_and_different_endpoints_are_independent() {
        ValidatedBatch::new([
            job(
                "neighbor-a",
                "https://nas.test",
                "alice",
                "/team/root",
                true,
                5,
            ),
            job(
                "neighbor-b",
                "https://nas.test",
                "alice",
                "/team/rooted",
                true,
                5,
            ),
        ])
        .unwrap();
        ValidatedBatch::new([
            job(
                "endpoint-a",
                "https://one.test",
                "alice",
                "/team/root",
                true,
                5,
            ),
            job(
                "endpoint-b",
                "https://two.test",
                "alice",
                "/team/root",
                true,
                5,
            ),
        ])
        .unwrap();
    }

    #[test]
    fn endpoint_keys_normalize_host_default_port_and_trailing_slash() {
        let one = job(
            "a",
            "https://NAS.TEST:443/prefix",
            "alice",
            "/team/root",
            false,
            0,
        );
        let two = job(
            "b",
            "https://nas.test/prefix/",
            "bob",
            "/team/root/child",
            false,
            0,
        );
        assert_eq!(one.endpoint(), two.endpoint());
        assert!(matches!(
            ValidatedBatch::new([one, two]),
            Err(BatchError::OverlappingRemoteRoots { .. })
        ));
    }

    #[test]
    fn overlapping_alternatives_are_safe_when_only_one_is_selected() {
        let catalog = BatchCatalog::new([
            job(
                "additive",
                "https://nas.test",
                "alice",
                "/team/root",
                false,
                0,
            ),
            job("mirror", "https://nas.test", "alice", "/team/root", true, 5),
        ])
        .unwrap();
        catalog.select(["additive"]).unwrap();
        catalog.select(["mirror"]).unwrap();
        assert!(matches!(
            catalog.select_all(),
            Err(BatchError::OverlappingRemoteRoots { .. })
        ));
    }

    #[test]
    fn malformed_identity_and_remote_inputs_are_rejected() {
        assert!(matches!(
            BatchJob::parse(" bad ", "https://nas.test", "alice", "/team/a", false, 0),
            Err(BatchError::InvalidJobName { .. })
        ));
        assert!(matches!(
            BatchJob::parse(
                "job",
                "https://user:pass@nas.test",
                "alice",
                "/team/a",
                false,
                0
            ),
            Err(BatchError::InvalidEndpoint { .. })
        ));
        assert!(matches!(
            BatchJob::parse("job", "https://nas.test", "alice", "/team/../a", false, 0),
            Err(BatchError::InvalidRemote { .. })
        ));
        assert!(matches!(
            BatchJob::parse("job", "https://nas.test", "", "/team/a", false, 0),
            Err(BatchError::InvalidUsername { .. })
        ));
    }

    #[test]
    fn deletion_preflight_is_complete_ordered_and_aggregate_bounded() {
        let batch = ValidatedBatch::new([
            job("z", "https://nas.test", "alice", "/team/z", true, 7),
            job("a", "https://nas.test", "alice", "/team/a", false, 99),
            job("m", "https://nas.test", "alice", "/team/m", true, 5),
        ])
        .unwrap();
        let result = batch
            .preflight_deletions([("z", 3), ("m", 2), ("a", 0)], Some(6))
            .unwrap();
        assert_eq!(result.total_planned, 5);
        assert_eq!(result.configured_limit, 12);
        assert_eq!(result.effective_limit, 6);
        assert_eq!(result.remaining, 1);
        assert_eq!(
            result
                .per_job
                .iter()
                .map(|job| (job.job_name.as_str(), job.planned, job.maximum))
                .collect::<Vec<_>>(),
            [("a", 0, 0), ("m", 2, 5), ("z", 3, 7)]
        );
    }

    #[test]
    fn every_deletion_guard_fails_closed() {
        let batch = ValidatedBatch::new([
            job("off", "https://nas.test", "alice", "/team/off", false, 100),
            job("on", "https://nas.test", "alice", "/team/on", true, 2),
        ])
        .unwrap();
        assert!(matches!(
            batch.preflight_deletions([("off", 1), ("on", 0)], None),
            Err(BatchError::DeletionDisabled { .. })
        ));
        assert!(matches!(
            batch.preflight_deletions([("off", 0), ("on", 3)], None),
            Err(BatchError::JobDeletionLimit { .. })
        ));
        assert!(matches!(
            batch.preflight_deletions([("off", 0), ("on", 2)], Some(1)),
            Err(BatchError::AggregateDeletionLimit { .. })
        ));
        assert!(matches!(
            batch.preflight_deletions([("off", 0)], None),
            Err(BatchError::MissingDeletionPlan { job_name }) if job_name == "on"
        ));
        assert!(matches!(
            batch.preflight_deletions([("off", 0), ("off", 0), ("on", 0)], None),
            Err(BatchError::DuplicateDeletionPlan { job_name }) if job_name == "off"
        ));
        assert!(matches!(
            batch.preflight_deletions([("off", 0), ("on", 0), ("other", 0)], None),
            Err(BatchError::UnknownDeletionPlan { job_name }) if job_name == "other"
        ));
    }

    #[test]
    fn deletion_budget_overflow_is_rejected() {
        let batch = ValidatedBatch::new([
            job(
                "a",
                "https://nas.test",
                "alice",
                "/team/a",
                true,
                usize::MAX,
            ),
            job("b", "https://nas.test", "alice", "/team/b", true, 1),
        ])
        .unwrap();
        assert!(matches!(
            batch.configured_deletion_limit(),
            Err(BatchError::DeletionCountOverflow)
        ));
        assert!(matches!(
            batch.preflight_deletions([("a", 0), ("b", 0)], None),
            Err(BatchError::DeletionCountOverflow)
        ));
    }

    #[test]
    fn component_boundary_property_holds_for_many_sibling_names() {
        for index in 0..128 {
            let left = format!("/team/root-{index}");
            let right = format!("/team/root-{index}-sibling");
            ValidatedBatch::new([
                job("a", "https://nas.test", "alice", &left, true, 1),
                job("b", "https://nas.test", "alice", &right, true, 1),
            ])
            .unwrap();
        }
    }
}
