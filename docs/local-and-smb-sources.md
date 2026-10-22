# Local, mapped-drive, and SMB sources

`SOURCE` is any local path the running identity can read as an ordinary directory: a folder on an
internal disk, a drive mapped from a NAS, or a share mounted over SMB/CIFS/NFS. The scanner
(`src/local.rs`) is portable `std::fs` code with only two platform-specific checks — detecting
links/reparse points and classifying a handful of Windows-only attributes — so the same source rules
apply everywhere. This page documents the mounted/mapped case as a supported way to point
`SOURCE` at NAS-hosted content, not as a workaround.

Mounting the share is the operating system's job, not this client's. The tool never mounts, maps,
or authenticates an SMB/NFS share itself: doing that reliably needs elevated privileges and a
second credential store independent of the DSM login this client already manages. Use the OS's own
mechanism, then point `SOURCE` at the result:

- **Windows**: a UNC path (`\\nas\media\photos`) or a drive letter mapped with `net use` or File
  Explorer (`Z:\photos`).
- **macOS**: `Finder > Go > Connect to Server`, or `mount_smbfs`, publishes the share under
  `/Volumes/...` (`/Volumes/media/photos`).
- **Linux**: `mount -t cifs` (or an `/etc/fstab`/systemd `.mount` unit) publishes the share wherever
  you choose, conventionally under `/mnt/...` (`/mnt/media/photos`).

```bash
# Windows (PowerShell), UNC path
synology-drive-sync.exe plan '\\nas\media\photos' '/team-folder/photos' --profile nas

# Windows (PowerShell), mapped drive
synology-drive-sync.exe plan 'Z:\photos' '/team-folder/photos' --profile nas

# macOS
synology-drive-sync plan /Volumes/media/photos /team-folder/photos --profile nas

# Linux
synology-drive-sync plan /mnt/media/photos /team-folder/photos --profile nas
```

Validate the exact mounted path with `doctor source` before relying on it, especially under a
scheduler identity — a drive letter mapped in an interactive session is frequently invisible to a
scheduled task running under a different logon:

```bash
synology-drive-sync doctor source '\\nas\media\photos' --hash
```

## The share root cannot be the source (use a subdirectory)

Today, pointing `SOURCE` directly at a share's root — `\\nas\media` with nothing after the share
name, or a mapped drive's own root such as `Z:\` — is rejected:

```text
the canonical source root cannot be a filesystem root
```

This comes from `reject_filesystem_root` in `src/local.rs`, which refuses any canonicalized source
whose `parent()` is `None`. On Windows, `fs::canonicalize` resolves both a share root and a mapped
drive root to an extended-length path (`\\?\UNC\nas\media` or `\\?\Z:\`) that has no parent
component, so the same check that correctly rejects `C:\` or `/` also catches a bare share root.

**Workaround**: sync a subdirectory of the share instead of the share itself — `\\nas\media\photos`
rather than `\\nas\media`. A subdirectory canonicalizes to a path that does have a parent, so it is
accepted normally. This is a known limitation, not final behavior; it is tracked for a fix that lets
a share root be selected directly. Until then, create (or point at an existing) subdirectory one
level below the share root.

## Performance: SMB sources work, but they are not fast

A mounted share is exercised through the same filesystem calls as a local disk, so nothing here is
SMB-specific in the code — but every one of those calls becomes a network round trip instead of a
local syscall, and the scanner and hasher are deliberately conservative about re-checking what they
just read. Expect a scan and sync of a large SMB source to be substantially slower than the same
tree on local disk. Where that cost comes from:

- **Scanning.** For every child of every directory, `scan_dir` takes the entry's file type from the
  directory listing the OS already returned, then makes one additional `fs::symlink_metadata` call
  per entry to positively classify symlinks, junctions, reparse points, and Windows attributes
  before trusting it. That is one extra stat-equivalent round trip per file and directory
  on top of whatever the listing itself cost.
- **Hashing.** Under `--compare content`, hashing one file (`hash_file_snapshot`) brackets the
  actual read with four metadata checks — a snapshot check before opening, another right after
  opening, and both again after the read completes — to catch a file that changed out from under
  the scan or during the read (a TOCTOU defense, not incidental overhead). On local disk those
  checks are effectively free; on SMB, each one is a round trip.
- **Re-hashing on upload.** Content comparison hashes every payload file once during the scan. Any
  file that actually needs to be uploaded or server-copied is then hashed again immediately before
  the transfer opens it (the same TOCTOU guard, run a second time right before the mutation), and
  once more after the transfer completes, to prove the source did not change mid-transfer. A changed
  file can therefore have its local bytes read and hashed three times over the SMB connection for
  one sync, before the destination copy is separately streamed from File Station and confirmed with
  its complete MD5/CRC32/SHA-256 fingerprint.

**Recommendation**: for a large SMB source, prefer `--compare metadata` (matches on size and
File Station's whole-second mtime, under the floor-or-round rule in
[how modification times are compared](diagnostics-and-batch.md#how-modification-times-are-compared),
with no content hashing) or `--compare size-only`. Both skip
the repeated local hashing above entirely. The tradeoff is real: metadata comparison can miss a
content change that leaves size and mtime unchanged (including an edit of the same size made within
the same second, which the destination's resolution cannot separate), and size-only is weaker still, and neither
verifies uploaded bytes against a rehash the way `--compare content` does. Use content comparison
when correctness matters more than throughput — a first sync of a large tree, or a source small
enough that the extra round trips do not matter — and metadata or size-only for routine incremental
syncs of a large share where you already trust the source's mtimes.

## What the scan skips, excludes and carries

The scan no longer aborts on the entries below. Each is either excluded up front, or **skipped and
reported**: it is never uploaded, never descended into, and the NAS copy of it is protected, not
deleted. Skips are listed as `SKIPPED` lines in `plan` output and in `plan.skipped`, counted in
`result.skipped`, and logged as one `entry.skipped` warning each; they exit `0` with a warning because
nothing about them changes on the next run. See the
[reason vocabulary](observability.md#partial-runs-deferrals-and-skips).

**Excluded by default** (so they are never uploaded, and any remote copy is protected): `desktop.ini`,
`Thumbs.db` and `ehthumbs.db` (any case), `.DS_Store`, a macOS `Icon\r` file, and Office lock files
(`~$*`). These are ordinary ignore rules applied before yours, so an operator rule such as
`!desktop.ini` in `.sdsyncignore` wins.

**Skipped and reported:**

- symlinks, junctions and volume mount points (`link-or-reparse-point`);
- Windows SYSTEM and TEMPORARY entries the default excludes do not cover (`windows-attributes`);
- dehydrated cloud or HSM placeholders, carrying `RECALL_ON_DATA_ACCESS`, `RECALL_ON_OPEN` or
  `OFFLINE` (`dehydrated-placeholder`);
- names that collide by case or Unicode normalisation (`local-case-collision`), described below.

**Carried as ordinary files:** reparse points that hold their own data, namely deduplicated files,
and hydrated or pinned OneDrive and other cloud files. These are hashed, uploaded and descended into.

**Still refused (the run fails):** a source root that is itself a link or reparse point, an
`.sdsyncignore` that is not ordinary data, special files (FIFO, socket, device), and names the tool
refuses (such as `~foo`, `aux.h` or a trailing dot) unless you exclude them.

### Cloud-tiered and HSM-backed shares

A share backed by cloud-tiering or hierarchical storage management (for example, an SMB re-export of
a OneDrive Files-On-Demand folder, or NAS-side HSM that marks cold files offline) can expose files
whose content has not been hydrated to local or near storage. Such an entry is skipped and reported as
`dehydrated-placeholder` rather than failing the scan: hashing or uploading a file DSM cannot verify
the true bytes of, or one that a tiering layer will change without touching its logical mtime, is
exactly what the content-comparison and TOCTOU guards elsewhere in this document exist to prevent.
A source that is nothing but placeholders therefore syncs nothing and says so, entry by entry, rather
than aborting.

To sync a skipped placeholder, hydrate it first (for example, force a full local copy through the
tiering client) or leave it out with `.sdsyncignore`/`--exclude` to silence the report.

### Case and Unicode-normalisation names

Windows and macOS treat names that differ only by case, or by Unicode normalisation form (NFC and
NFD, as when a Mac and a Windows machine spell `é` differently), as one name; File Station keeps them
apart or together depending on the volume. These now converge instead of failing:

- A name that differs from its remote counterpart only by case or normalisation is matched to it. A
  case-only rename (`Report.pdf` to `report.pdf`) is carried under `--delete` as an upload of the new
  spelling followed by removal of the old one; without `--delete` it is skipped as
  `case-rename-needs-delete` and the old spelling is kept. A normalisation difference is matched to
  the existing remote spelling, so new files under a paired folder go to the remote spelling and
  nothing is uploaded and deleted as a pair.
- A **true collision**, two local names (`local-case-collision`) or two remote names
  (`remote-case-collision`) that fold to the same name, is skipped and reported with both spellings
  named. Nothing under either is deleted. Rename one of them to continue.
- If the upload half of a rename is itself deferred (the file changed while it was read), the old
  spelling is not deleted either; both survive until the next run.

During a case-rename under `--delete` the NAS briefly holds both spellings, because the upload
precedes the removal. No data is lost on the NAS, but a Synology Drive client may see a create and a
delete of case-equal names.
