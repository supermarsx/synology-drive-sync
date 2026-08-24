# Planning and execution

`sdk::Engine::run` treats planning and execution as one safety protocol rather than two independent
conveniences.

## Client construction

`SyncRequest::builder` requires endpoint, username, source, and logical remote root. Its defaults are
HTTPS-only, certificate-validating, content comparison, two workers, bounded timeouts/retries, and
additive/update-only behavior. Optional builder methods add a private CA, rate cap, exclusions,
comparison mode, worker count, or explicit `DeletionPolicy`.

The engine is blocking. Place it on an appropriate worker thread when integrating into an async or UI
application, and keep secrets and callbacks away from diagnostic formatting.

## Inventory and digest selection

The engine builds local ignore rules/inventory and remote inventory beneath a validated `RemoteRoot`.
Content-mode planning intentionally selects the minimum remote hashes needed for same-path comparison,
safe server-copy candidates, and deletion guards.

Never replace an unavailable digest with a zero, empty string, or assumed mismatch. The planner and
executor fail closed when the safety decision depends on missing remote metadata.

## Build a plan

The frozen request controls:

- deletion enablement and per-plan cap;
- empty-source permission;
- content, metadata, or size-only comparison;
- availability of verified server-side copy.

The engine presents an immutable `PlanSummary` exactly once after current inventories/hashes and before
mutation. It exposes ordered `PlannedChange` entries, operation, remote path, optional source, bytes,
and reason plus aggregate create/copy/upload/delete/protected/unchanged counts. The plan-decision
callback returns `PreviewOnly` or `Apply`.

## Execute and observe

The event callback receives:

- phase start/completion events for scan, discovery, authentication, inventory, hashing, planning,
  execution, reconciliation, and logout;
- the immutable plan-ready event;
- structured successful mutation events;
- `EventControl::Cancel` for cooperative cancellation before the next safe operation.

Callbacks must be fast, non-panicking, and non-blocking. The separate `CancellationToken` may also be
cancelled from another thread.

## Required caller-owned controls

The engine provides single-run ordering, plan approval, bounded per-run deletion, final reconciliation,
and logout. Integrators remain responsible for:

- credential source order and redaction;
- aggregate deletion authorization across multiple engine runs;
- all-job preflight before a batch mutates;
- single-writer coordination across processes/hosts;
- source quiescence and scheduler deadlines;
- durable result/event storage and alert delivery;
- production acceptance against the exact NAS/reverse-proxy/scheduler path.

Use separate preview runs carefully: a later apply run rebuilds the plan and calls the decision callback
again; do not assume a previously displayed plan is still current.
