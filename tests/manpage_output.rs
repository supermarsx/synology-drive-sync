#![forbid(unsafe_code)]

use std::fs;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

#[test]
fn root_and_recursive_manpage_modes_are_complete() {
    let binary = env!("CARGO_BIN_EXE_synology-drive-sync");

    let root = Command::new(binary)
        .arg("manpage")
        .output()
        .expect("run root manpage generator");
    assert!(root.status.success());
    assert!(root.stderr.is_empty());
    let root_text = String::from_utf8(root.stdout).expect("root manpage is UTF-8 roff");
    assert!(root_text.contains(".SH SYNOPSIS"));
    assert!(root_text.contains(".SH SUBCOMMANDS"));

    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is after Unix epoch")
        .as_nanos();
    let output_directory =
        std::env::temp_dir().join(format!("sdsync-manpages-{}-{nonce}", std::process::id()));

    let recursive = Command::new(binary)
        .args(["manpage", "--all"])
        .arg(&output_directory)
        .output()
        .expect("run recursive manpage generator");
    assert!(recursive.status.success());
    assert!(recursive.stdout.is_empty());
    assert!(recursive.stderr.is_empty());

    let mut generated = fs::read_dir(&output_directory)
        .expect("recursive generator created its output directory")
        .map(|entry| {
            entry
                .expect("read generated manpage entry")
                .file_name()
                .into_string()
                .expect("generated manpage name is UTF-8")
        })
        .collect::<Vec<_>>();
    generated.sort();

    let mut expected = vec![
        "synology-drive-sync-completions.1",
        "synology-drive-sync-config-path.1",
        "synology-drive-sync-config-show.1",
        "synology-drive-sync-config-validate.1",
        "synology-drive-sync-config.1",
        "synology-drive-sync-credentials-remove.1",
        "synology-drive-sync-credentials-set-password.1",
        "synology-drive-sync-credentials-set-totp.1",
        "synology-drive-sync-credentials-status.1",
        "synology-drive-sync-credentials.1",
        "synology-drive-sync-doctor-source.1",
        "synology-drive-sync-doctor-target.1",
        "synology-drive-sync-doctor.1",
        "synology-drive-sync-manpage.1",
        "synology-drive-sync-plan.1",
        "synology-drive-sync-sync.1",
        "synology-drive-sync.1",
    ];
    expected.sort();
    assert_eq!(generated, expected);

    for name in &generated {
        let text = fs::read_to_string(output_directory.join(name))
            .expect("generated manpage is readable UTF-8 roff");
        assert!(text.starts_with(".ie \\n(.g .ds Aq"));
        assert!(text.contains(".SH NAME"));
    }

    let asserted_controls = [
        (
            "synology-drive-sync-sync.1",
            &[
                "\\-\\-profiles",
                "\\-\\-all\\-profiles",
                "\\-\\-max\\-total\\-delete",
                "\\-\\-delete",
                "\\-\\-max\\-delete",
                "\\-\\-password\\-file",
                "\\-\\-totp\\-secret\\-file",
            ][..],
        ),
        (
            "synology-drive-sync-plan.1",
            &[
                "\\-\\-profiles",
                "\\-\\-all\\-profiles",
                "\\-\\-max\\-total\\-delete",
                "\\-\\-exit\\-code",
            ][..],
        ),
        (
            "synology-drive-sync-doctor.1",
            &[
                "\\-\\-profiles",
                "\\-\\-all\\-profiles",
                "\\-\\-routing\\-only",
                "\\-\\-totp\\-secret\\-file",
            ][..],
        ),
        ("synology-drive-sync-doctor-source.1", &["\\-\\-hash"]),
        (
            "synology-drive-sync-doctor-target.1",
            &["\\-\\-write\\-test"],
        ),
    ];
    for (name, controls) in asserted_controls {
        let text = fs::read_to_string(output_directory.join(name))
            .expect("generated control-bearing manpage is readable UTF-8 roff");
        for control in controls {
            assert!(
                text.contains(control),
                "generated {name} omitted the {control} control"
            );
        }
    }

    fs::remove_dir_all(&output_directory).expect("remove isolated manpage test directory");
}
