# Cancellation, progress, and errors

## Cooperative cancellation

`cancel::CancellationToken` is cloneable. Calling `cancel()` makes future `check()` calls return the
shared `Error::Cancelled`; long reads and uploads observe the token at bounded safe boundaries.

Cancellation is not thread termination. Keep the token alive, request cancellation, and wait for the
worker to return so locks, network sessions, and caller-owned resources remain authoritative until
the operation actually ends.

## Progress

The supported SDK emits secret-free `SdkEvent` values for phase boundaries, plan readiness, and
completed mutations. Returning `EventControl::Cancel` requests cancellation. Lower-level upload byte
tracking remains available in the `progress`/`sync` modules for specialist integrations.

An observer or renderer must never expose passwords, OTP material, bearer tokens, raw DSM responses,
or secret file contents. Remote paths and filenames can still be sensitive operational data; apply
your application's privacy policy before exporting them.

## Partial outcomes, deferrals, and skips

A successful `SyncOutcome` is not necessarily a complete one. A path whose check fails for its own
reason (a file that changed or vanished while it was read, a destination that moved after planning,
a refused server-side copy or deletion) is deferred rather than failing the run: it is neither uploaded
nor deleted, and `SyncOutcome::deferred()` lists each as a `DeferredPath` with a `DeferredReason`
(kebab-case, the same eleven tokens as the CLI result). `SyncOutcome::partial()` is true when any
deferral is transient, which is the case where the CLI exits `3`; check it before treating the run as
converged. Lasting deferrals (`unrepresentable-name`, `unsupported-local-entry`, `remote-unreadable`, `listing-incomplete`)
are listed but do not make the outcome partial. A deferral also raises `MutationEvent::PathDeferred`
(`"path-deferred"`).

`PlanSummary::skipped()` returns the paths the plan declines to carry (links and reparse points,
SYSTEM files, dehydrated placeholders, case and normalisation collisions) as `SkippedPath` values
with `relative`, a reason tag, `detail`, and `is_lasting()`. They no longer fail the run, so an
embedding application must read them to know what was left out. `SyncOutcome::recovered()` reports the
retries, re-authentications, keepalive failures and unconfirmed server tasks the engine absorbed; the
SDK does not install a re-authenticator, so an expired session is not re-established there (the
keepalive and the pre-mutation touch still run). A server-side copy that finishes with an error
surfaces as `ErrorCode::Remote`.

## Error model

`SdkError::code()` returns the non-exhaustive broad `ErrorCode` categories `InvalidRequest`,
`CredentialUnavailable`, `OtpRequired`, `Authentication`, `LocalFilesystem`, `Network`, `Remote`,
`Safety`, `Cancelled`, `Reconciliation`, or `Internal`. `message()` returns bounded secret-free operator
text. Match the code for program logic; do not parse messages.

Treat missing metadata, verification failure, stale snapshots, deletion-cap breach, required-log
delivery failure, and cancellation as unsuccessful operations. A prior mutation is not automatically
rolled back, so report partial completion accurately and stop later jobs when required.

## Panic and thread policy

Public callbacks should not panic across worker boundaries. The release profile aborts on panic for
the shipped binary; a Rust library consumer's final panic strategy is determined by its build graph.
Use explicit error propagation and ensure logout/cleanup is attempted without hiding the original
failure.
