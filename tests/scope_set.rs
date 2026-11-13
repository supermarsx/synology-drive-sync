//! A scope set (`--scope` repeated, `--scope-shallow`) end to end, and above all what it must
//! never do: a shallow folder's run lists, plans and deletes its direct children only, never
//! anything beneath a subfolder, and prunes no index record it did not walk. Each test tries to
//! make a shallow scope reach deeper and checks that nothing below was read, written, deleted or
//! pruned.

#[allow(dead_code)]
mod support;

use std::fs;
use std::process::{Command, Output};

use serde_json::Value;
use support::TestDir;
use support::file_station_mock::{CapturedRequest, MockFileStation};

const PASSWORD: &[u8] = b"correct horse battery staple\n";

struct Fixture {
    directory: TestDir,
    server: MockFileStation,
    root: &'static str,
}

impl Fixture {
    fn new(label: &str, root: &'static str) -> Self {
        let directory = TestDir::new(label);
        fs::create_dir_all(directory.child("source")).unwrap();
        directory.write("password", PASSWORD);
        let server = MockFileStation::start();
        server.add_directory(root);
        Self {
            directory,
            server,
            root,
        }
    }

    fn write(&self, relative: &str, contents: &[u8]) -> i64 {
        let path = self.directory.child("source").join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, contents).unwrap();
        fs::metadata(&path)
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
    }

    /// A file on the NAS (and its folders), with the local copy's time when there is one.
    fn nas_file(&self, relative: &str, contents: &[u8], mtime: i64) {
        let mut folder = self.root.to_owned();
        let parts: Vec<&str> = relative.split('/').collect();
        for part in &parts[..parts.len() - 1] {
            folder = format!("{folder}/{part}");
            self.server.add_directory(&folder);
        }
        self.server
            .add_file(&format!("{}/{relative}", self.root), contents, mtime);
    }

    fn run(&self, command: &str, extra: &[&str]) -> Output {
        let source = self.directory.child("source");
        let password = self.directory.child("password");
        let environment = TestDir::new("scope-set-environment");
        for directory in ["home", "appdata", "local-appdata", "xdg"] {
            fs::create_dir_all(environment.child(directory)).unwrap();
        }
        let mut arguments = vec![
            "--output",
            "json",
            command,
            source.to_str().unwrap(),
            self.root,
            "--url",
            self.server.base_url(),
            "--username",
            "e2e-user",
            "--password-file",
            password.to_str().unwrap(),
            "--no-vault",
            "--allow-http",
            "--jobs",
            "1",
        ];
        arguments.extend_from_slice(extra);
        let mut process = Command::new(env!("CARGO_BIN_EXE_synology-drive-sync"));
        process
            .args(&arguments)
            .env("HOME", environment.child("home"))
            .env("USERPROFILE", environment.child("home"))
            .env("APPDATA", environment.child("appdata"))
            .env("LOCALAPPDATA", environment.child("local-appdata"))
            .env("XDG_CONFIG_HOME", environment.child("xdg"));
        for (name, _) in std::env::vars_os() {
            if name.to_string_lossy().starts_with("SDSYNC_") {
                process.env_remove(name);
            }
        }
        process.env("SDSYNC_TEST_PAIR_INDEX_RACY_WINDOW_MS", "1000");
        process.output().expect("run synology-drive-sync")
    }

    fn sync(&self, extra: &[&str]) -> Value {
        let output = self.run("sync", extra);
        assert_eq!(
            output.status.code(),
            Some(0),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).expect("one JSON document")
    }

    fn nas(&self, relative: &str) -> Option<Vec<u8>> {
        self.server
            .file_contents(&format!("{}/{relative}", self.root))
    }
}

fn folder_of(request: &CapturedRequest) -> Option<String> {
    request
        .fields
        .get("folder_path")
        .map(|value| value.trim_matches('"').to_owned())
}

fn listed(requests: &[CapturedRequest]) -> Vec<String> {
    let mut folders: Vec<String> = requests
        .iter()
        .filter(|request| request.operation() == "SYNO.FileStation.List.list")
        .filter_map(folder_of)
        .collect();
    folders.sort();
    folders.dedup();
    folders
}

fn touched_paths(requests: &[CapturedRequest], operation: &str) -> Vec<String> {
    requests
        .iter()
        .filter(|request| request.operation() == operation)
        .map(|request| format!("{:?}", request.fields))
        .collect()
}

/// `--scope-shallow a --delete` with a subfolder gone locally: the stale file directly in `a` is
/// deleted; the subfolder, which the NAS still holds with files a level and two levels down, is
/// neither listed nor deleted.
#[test]
fn a_shallow_mirror_deletes_direct_child_files_and_never_a_subfolder() {
    let fixture = Fixture::new("scope-shallow-delete", "/team/shallow");
    let keep = fixture.write("a/keep.txt", b"keep");
    fixture.nas_file("a/keep.txt", b"keep", keep);
    fixture.nas_file("a/stale.txt", b"stale", 1_700_000_000);
    fixture.nas_file("a/sub/deep.txt", b"deep", 1_700_000_000);
    fixture.nas_file("a/sub/deeper/deepest.txt", b"deepest", 1_700_000_000);

    let document = fixture.sync(&["--scope-shallow", "a", "--delete", "--allow-empty-source"]);
    assert_eq!(document["result"]["deleted"], 1, "{document}");
    assert_eq!(fixture.nas("a/stale.txt"), None);
    assert_eq!(fixture.nas("a/sub/deep.txt"), Some(b"deep".to_vec()));
    assert_eq!(
        fixture.nas("a/sub/deeper/deepest.txt"),
        Some(b"deepest".to_vec())
    );
    let requests = fixture.server.requests();
    assert!(
        listed(&requests)
            .iter()
            .all(|folder| !folder.contains("/a/sub")),
        "a subfolder of a shallow folder was listed: {:?}",
        listed(&requests)
    );
    let deletes = touched_paths(&requests, "SYNO.FileStation.Delete.delete");
    assert!(
        deletes.iter().all(|fields| !fields.contains("a/sub")),
        "{deletes:?}"
    );
}

/// A shallow folder deleted locally with `--delete`: its direct files go, the folder itself and
/// its subfolder stay, because what they hold was never listed.
#[test]
fn a_shallow_folder_gone_locally_is_never_deleted_on_the_nas() {
    let fixture = Fixture::new("scope-shallow-gone", "/team/gone");
    fixture.write("other.txt", b"other");
    fixture.nas_file("a/f.txt", b"f", 1_700_000_000);
    fixture.nas_file("a/sub/deep.txt", b"deep", 1_700_000_000);

    let document = fixture.sync(&["--scope-shallow", "a", "--delete", "--allow-empty-source"]);
    assert_eq!(document["result"]["deleted"], 1, "{document}");
    assert_eq!(fixture.nas("a/f.txt"), None);
    assert_eq!(fixture.nas("a/sub/deep.txt"), Some(b"deep".to_vec()));
    let deletes = touched_paths(&fixture.server.requests(), "SYNO.FileStation.Delete.delete");
    assert!(
        deletes
            .iter()
            .all(|fields| !fields.contains("/team/gone/a\\\"") && !fields.contains("a/sub")),
        "{deletes:?}"
    );
}

/// A local file where the NAS holds a subfolder of a shallow folder: replacing it would delete
/// what the subfolder holds, which nobody listed. Neither side is touched, and the conflict is
/// named as a skip.
#[test]
fn a_shallow_scope_never_replaces_a_subfolder_with_a_file() {
    let fixture = Fixture::new("scope-shallow-clash", "/team/clash");
    fixture.write("a/clash", b"a file now");
    fixture.nas_file("a/clash/inner.txt", b"inner", 1_700_000_000);

    let document = fixture.sync(&["--scope-shallow", "a", "--delete", "--allow-empty-source"]);
    assert_eq!(fixture.nas("a/clash/inner.txt"), Some(b"inner".to_vec()));
    let requests = fixture.server.requests();
    assert!(touched_paths(&requests, "SYNO.FileStation.Delete.delete").is_empty());
    assert!(
        requests
            .iter()
            .all(|request| request.operation() != "SYNO.FileStation.Upload.upload"),
        "nothing is written over the subfolder"
    );
    let text = document.to_string();
    assert!(
        text.contains("type-conflict") && text.contains("a/clash"),
        "the conflict is named: {document}"
    );
}

/// Two dirty folders in separate subtrees, as the daemon sends them: each is listed, nothing
/// else is, and both edits land.
#[test]
fn two_shallow_folders_list_only_themselves() {
    let fixture = Fixture::new("scope-two", "/team/two");
    for month in ["01", "02", "03", "11"] {
        let mtime = fixture.write(&format!("2026/{month}/doc.txt"), b"old");
        fixture.nas_file(&format!("2026/{month}/doc.txt"), b"old", mtime);
    }
    fixture.write("2026/03/doc.txt", b"edit");
    fixture.write("2026/11/new.txt", b"new");

    let document = fixture.sync(&["--scope-shallow", "2026/03", "--scope-shallow", "2026/11"]);
    assert_eq!(document["result"]["uploaded"], 2, "{document}");
    assert_eq!(fixture.nas("2026/03/doc.txt"), Some(b"edit".to_vec()));
    assert_eq!(fixture.nas("2026/11/new.txt"), Some(b"new".to_vec()));
    assert_eq!(
        listed(&fixture.server.requests()),
        ["/team/two/2026/03", "/team/two/2026/11"]
    );
}

/// Overlapping elements are normalised: a recursive folder wins over a shallow one of the same
/// path, and an element inside it adds nothing.
#[test]
fn a_recursive_element_wins_over_a_shallow_one_and_absorbs_nested_ones() {
    let fixture = Fixture::new("scope-overlap", "/team/overlap");
    fixture.write("a/b/c/deep.txt", b"deep");
    fixture.write("a/top.txt", b"top");

    let document = fixture.sync(&[
        "--scope-shallow",
        "a",
        "--scope",
        "a",
        "--scope",
        "a/b",
        "--scope-shallow",
        "a/b/c",
    ]);
    assert_eq!(document["result"]["uploaded"], 2, "{document}");
    assert_eq!(fixture.nas("a/b/c/deep.txt"), Some(b"deep".to_vec()));
}

/// A path outside the source, and a DSM-managed one, are refused by name before anything runs.
#[test]
fn a_scope_element_that_leaves_the_source_is_refused_by_name() {
    let fixture = Fixture::new("scope-escape", "/team/escape");
    fixture.write("a/f.txt", b"f");
    for arguments in [
        vec!["--scope-shallow", "../outside"],
        vec!["--scope", "a", "--scope-shallow", "a/../../outside"],
        vec!["--scope-shallow", "@eaDir"],
    ] {
        let output = fixture.run("sync", &arguments);
        assert_ne!(output.status.code(), Some(0), "{arguments:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("outside") || stderr.contains("@eaDir"),
            "{arguments:?}: {stderr}"
        );
    }
    assert!(
        fixture
            .server
            .requests()
            .iter()
            .all(|request| request.operation() != "SYNO.FileStation.Upload.upload")
    );
}

/// `status` and `resync` take one path; a set is refused by name rather than read as its first
/// element.
#[test]
fn status_refuses_a_scope_set() {
    let fixture = Fixture::new("scope-status", "/team/status");
    fixture.write("a/f.txt", b"f");
    let output = fixture.run("status", &["--scope", "a", "--scope-shallow", "b"]);
    assert_ne!(output.status.code(), Some(0));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("takes one --scope path"), "{stderr}");
}

/// A link on the NAS standing where a shallow folder should be is refused by name: nothing behind
/// it is listed, written or deleted.
#[test]
fn a_shallow_folder_that_is_a_link_on_the_nas_is_refused_by_name() {
    let fixture = Fixture::new("scope-remote-link", "/team/link");
    fixture.write("lnk/f.txt", b"f");
    fixture.server.add_directory("/team/elsewhere");
    fixture
        .server
        .add_file("/team/elsewhere/victim.txt", b"victim", 1_700_000_000);
    fixture.server.add_directory("/team/link/lnk");
    fixture
        .server
        .add_directory_symlink("/team/link/lnk", "/team/elsewhere");

    let output = fixture.run(
        "sync",
        &["--scope-shallow", "lnk", "--delete", "--allow-empty-source"],
    );
    assert_ne!(output.status.code(), Some(0));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("is a NAS link") && stderr.contains("/team/link/lnk"),
        "{stderr}"
    );
    assert_eq!(
        fixture.server.file_contents("/team/elsewhere/victim.txt"),
        Some(b"victim".to_vec())
    );
    let requests = fixture.server.requests();
    assert!(touched_paths(&requests, "SYNO.FileStation.Delete.delete").is_empty());
    assert!(
        requests
            .iter()
            .all(|request| request.operation() != "SYNO.FileStation.Upload.upload")
    );
}

/// A link in the source standing where a shallow folder should be is never followed: what it
/// points at is not uploaded.
#[cfg(unix)]
#[test]
fn a_shallow_folder_that_is_a_link_in_the_source_is_never_followed() {
    let fixture = Fixture::new("scope-local-link", "/team/local-link");
    let outside = fixture.directory.child("outside");
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("secret.txt"), b"secret").unwrap();
    std::os::unix::fs::symlink(&outside, fixture.directory.child("source").join("lnk")).unwrap();
    fixture.write("other.txt", b"other");

    let output = fixture.run("sync", &["--scope-shallow", "lnk"]);
    assert_ne!(
        output.status.code(),
        Some(0),
        "the scope names nothing it may read"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("scope \"lnk\" does not exist"), "{stderr}");
    assert_eq!(fixture.nas("lnk/secret.txt"), None);
    assert!(
        fixture
            .server
            .requests()
            .iter()
            .all(|request| request.operation() != "SYNO.FileStation.Upload.upload")
    );
}

/// The index: a shallow run prunes the record of a file gone directly from its folder, and keeps
/// every record a level down, however absent its file looks from the run.
#[cfg(unix)]
#[test]
fn a_shallow_run_prunes_no_index_record_beneath_its_folder() {
    use synology_drive_sync::pair_index::PairIndex as _;
    use synology_drive_sync::pair_index_redb::{IndexPaths, RedbPairIndex};

    let fixture = Fixture::new("scope-prune", "/team/prune");
    for relative in ["a/gone.txt", "a/kept.txt", "a/sub/deep.txt"] {
        let mtime = fixture.write(relative, relative.as_bytes());
        fixture.nas_file(relative, relative.as_bytes(), mtime);
    }
    let index = fixture.directory.child("index");
    let index_text = index.to_str().unwrap().to_owned();
    std::thread::sleep(std::time::Duration::from_millis(2_200));
    let first = fixture.sync(&["--pair-index", &index_text]);
    assert_eq!(first["plan"]["pair_index"]["stored"], 3, "{first}");

    let source = fixture.directory.child("source");
    fs::remove_file(source.join("a/gone.txt")).unwrap();
    fs::remove_file(source.join("a/sub/deep.txt")).unwrap();
    fixture.sync(&["--pair-index", &index_text, "--scope-shallow", "a"]);

    let database = fs::read_dir(&index)
        .unwrap()
        .flatten()
        .find(|entry| entry.path().extension().is_some_and(|ext| ext == "redb"))
        .expect("the index file");
    let stem = database
        .path()
        .file_stem()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let opened = RedbPairIndex::open(IndexPaths::new(&index, &stem).unwrap(), 8);
    let mut backend = opened.backend.expect("the index opens");
    let mut keys = Vec::new();
    backend
        .for_each_key(&mut |key| keys.push(key.to_owned()))
        .unwrap();
    keys.sort();
    assert_eq!(keys, ["a/kept.txt", "a/sub/deep.txt"]);
}
