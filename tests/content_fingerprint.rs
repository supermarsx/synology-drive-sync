use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use synology_drive_sync::api::{RemoteEntry, RemoteInventory};
use synology_drive_sync::integrity::ContentFingerprint;
use synology_drive_sync::local::{EntryKind, IgnoreRules, LocalEntry, LocalInventory};
use synology_drive_sync::path::RemoteRoot;
use synology_drive_sync::plan::{ChangeReason, CompareMode, PlanOptions, build_plan};

#[test]
fn public_planner_rejects_equal_md5_and_crc32_when_sha256_differs() {
    let shared_md5 = [0x7a; 16];
    let shared_crc32 = 0x0102_0304;
    let local_fingerprint = ContentFingerprint::from_digests(shared_md5, shared_crc32, [0x11; 32]);
    let remote_fingerprint = ContentFingerprint::from_digests(shared_md5, shared_crc32, [0x22; 32]);
    let local = LocalInventory {
        root: PathBuf::from("/source"),
        entries: BTreeMap::from([(
            "payload.bin".to_owned(),
            LocalEntry {
                relative: "payload.bin".to_owned(),
                full_path: PathBuf::from("/source/payload.bin"),
                kind: EntryKind::File,
                size: 4096,
                mtime_ms: 1_785_769_200_000,
                content_md5: Some(local_fingerprint),
            },
        )]),
    };
    let remote = RemoteInventory {
        root_exists: true,
        entries: BTreeMap::from([(
            "payload.bin".to_owned(),
            RemoteEntry {
                relative: "payload.bin".to_owned(),
                remote_path: "/share/root/payload.bin".to_owned(),
                kind: EntryKind::File,
                size: 4096,
                mtime_seconds: 1_785_769_200,
                mount_point_type: None,
                content_md5: Some(remote_fingerprint),
            },
        )]),
    };
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let rules_root = std::env::temp_dir().join(format!(
        "sdsync-content-fingerprint-integration-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir(&rules_root).unwrap();
    let rules = IgnoreRules::build(&rules_root, &[]).unwrap();

    let plan = build_plan(
        &RemoteRoot::parse("/share/root").unwrap(),
        &local,
        &remote,
        &rules,
        &PlanOptions {
            delete: false,
            allow_empty_source: false,
            max_delete: 100,
            compare: CompareMode::Content,
            server_copy: false,
        },
    )
    .unwrap();

    assert_eq!(plan.uploads.len(), 1);
    assert_eq!(plan.uploads[0].reason, ChangeReason::ContentDiffers);
    assert_eq!(local_fingerprint.crc32(), remote_fingerprint.crc32());
    assert_ne!(local_fingerprint.sha256(), remote_fingerprint.sha256());
    fs::remove_dir_all(rules_root).unwrap();
}
