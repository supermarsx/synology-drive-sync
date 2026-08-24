# Rust library

The Cargo package contains both the `synology-drive-sync` executable and a Rust library target
imported as `synology_drive_sync`. Calendar releases publish the version-matched source bundle
`synology-drive-sync-YY.N-rust-sdk.tar.gz`, and Rust applications can pin the same release tag
directly from Git.

> [!CAUTION]
> The Cargo package remains pre-1.0. Pin an exact calendar tag, review SDK changes before upgrading,
> and enter through the high-level `sdk::Engine` instead of composing low-level mutation methods.

## Supported embedding surface

The `sdk` module owns one complete synchronous run. Its `Engine` performs source scanning, API
discovery, conditional password/TOTP acquisition, remote inventory and content hashing, plan
construction, the caller's explicit preview/apply decision, guarded execution, final reconciliation,
and logout. It exposes:

- `SyncRequest::builder` with HTTPS/content/additive safe defaults;
- `SecretProvider`, which is queried only when authentication reaches the relevant challenge;
- immutable `PlanSummary`/`PlannedChange` values presented before any mutation;
- `PlanDecision::PreviewOnly` or `PlanDecision::Apply` at that boundary;
- structured `SdkEvent` phase/mutation events and cooperative cancellation;
- `SyncOutcome`, `ExecutionSummary`, `SdkError`, and stable broad `ErrorCode` categories.

## Public modules

| Module | Purpose |
| --- | --- |
| `sdk` | Supported high-level embedding engine, request builder, secret provider, plan decision, events, and outcome. |
| `api` | Lower-level File Station discovery, login/logout, inventory, permission checks, verified upload/copy/delete, and explicit write probe used by the engine. |
| `batch` | Complete job catalogs, deterministic batch selection, overlap checks, and aggregate deletion preflight. |
| `cancel` | Cloneable cooperative cancellation token. |
| `integrity` | Validated content-MD5 representation. |
| `local` | Local scanner, portable entry model, and gitignore-style rules. |
| `observability` | Structured event and redaction-oriented logging primitives. |
| `path` | Logical remote-root parsing, containment, and portability validation. |
| `plan` | Comparison modes, reasoned actions, deletion snapshots/guards, and plan construction. |
| `progress` | Operation counters, snapshots, events, and renderers. |
| `source_diagnostics` | Local-only diagnostic report over the production scanner. |
| `sync` | Plan execution, operation events, upload observers, and execution reports. |
| `vault` | Endpoint/account-scoped OS credential-store access. |

The crate re-exports its shared `Error` and `Result` types.

## Integration boundary

The library is synchronous and blocking. Callers own process/runtime policy, secret-provider
implementation, configuration layering, scheduling, alerting, and final acceptance. High-level CLI
configuration and argument modules are not part of the library's exported surface.

`sdk::Engine` keeps the remote workflow ordered:

1. validate and canonicalize the local source and remote root;
2. discover required APIs with explicit TLS/timeout/retry controls;
3. ask the secret provider for password and, only after DSM challenge, OTP;
4. inventory both sides and populate every digest required by the chosen mode;
5. present one immutable plan to the caller before mutation;
6. apply only after `PlanDecision::Apply` and within the bounded deletion policy;
7. rescan/replan to prove convergence;
8. log out and return a structured outcome.

Start with the [Rust quick start](quick-start.md), then review
[planning and execution](workflows.md) and the [generated API reference](api-reference.md).
