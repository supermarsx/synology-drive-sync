# What it does and what it protects

`synology-drive-sync` is a finite, one-way synchronization engine. Each run scans one local source,
authenticates to Synology DSM through the configured File Station reverse-proxy URL, inventories one
logical remote destination, builds a plan, applies allowed operations, verifies the result, logs out,
and exits.

```text
ordinary local folder
        |
        | scan, compare, plan
        v
HTTPS reverse proxy -> DSM WebAPI -> chosen File Station path
```

The destination is a File Station logical path such as `/home/Drive/NAS-A Backup` or
`/TeamShare/Project`. It is never a DSM filesystem path such as `/volume1/...`. DSM must already
provide the user-home or shared-folder root and grant the dedicated account access. The client may
create a missing destination below an existing writable share.

## Guarantees within one run

- The local source is opened read-only and is never intentionally modified.
- Remote paths are normalized and kept beneath the configured destination root.
- Missing directories are created shallowest-first, including empty directories.
- Default content comparison requires size, MD5, IEEE CRC32, SHA-256, and one-second-resolution modification time.
- Uploads are verified against the local bytes and then checked again by a final rescan and replan.
- Remote deletion is disabled by default and guarded by explicit per-profile and aggregate limits.
- Cancellation requests cooperative shutdown and prevents the later deletion phase from starting.

## Deliberate boundaries

The client does not:

- mount or authenticate an SMB/NFS source share;
- discover NAS devices or bypass the one configured reverse-proxy URL;
- create DSM users, shared folders, home services, Team Folders, or ACLs;
- preserve ACLs, ownership, modes, xattrs, directory mtimes, or other filesystem metadata;
- provide a resumable upload protocol or a transactional snapshot of a changing source tree;
- replace independent backup, snapshots, or Synology Drive version history.

An additive sync may overwrite a remote file whose local counterpart changed. Keep the source
quiescent, use a dedicated non-administrator DSM account, and test recovery before production use.

## Supported source shapes

The source may be an internal disk directory, UNC path, mapped drive, or mounted SMB/CIFS/NFS
directory when the operating system presents it as an ordinary readable folder to the actual run
identity. See [local, mapped-drive, and SMB sources](../local-and-smb-sources.md) before scheduling a
network-backed source.

## The safe adoption sequence

1. [Install and verify](../installation.md) the correct artifact.
2. Create a non-secret profile and store credentials outside TOML.
3. Run `doctor source --hash` under the eventual scheduler identity.
4. Run a non-mutating target diagnostic.
5. Review `plan` with deletion disabled.
6. Perform an additive sync against disposable data.
7. Complete the [live-NAS acceptance and recovery runbook](../production-acceptance.md).
8. Only then consider mirror deletion with small explicit limits.
