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
