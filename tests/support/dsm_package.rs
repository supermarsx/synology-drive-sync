//! A DSM-shaped package tree whose configuration is written by the real
//! manager (`packaging/synology/package/bin/sdsync-dsm`), not by the test.
//!
//! Why this exists: a fixture written by the same author as the code encodes the
//! author's assumption, not production's. Plain temp dirs and hand-written
//! `source = ...` fragments hid an exit 78 on every NAS (the daemon refused the
//! symlinked roots) and two reads of the wrong config section. So here:
//!
//! - `home`, `var` and `target` are symlinks to `@apphome`, `@appdata` and
//!   `@appstore`, which is what DSM's `/var/packages/<pkg>` holds and what the
//!   scripts hand the daemon;
//! - a profile is saved by running the shipped manager's `configure-profile`, so
//!   the fragment is the manager's `[profiles.<name>]` table, byte for byte, and
//!   `config.toml` is the manager's merge of the fragments;
//! - the shipped core validates the candidate configuration, as on a NAS.
//!
//! Only two lines of the shipped scripts are relocated (the framework alias
//! `/var/packages/<pkg>` and the DSM volume root `/`), exactly as the packaging
//! suite does. The bridge the manager calls for the package transition and the
//! audit outbox is a small `sh` mock that refuses anything it does not know.
//!
//! Use [`DsmPackage::configure_profile`] for every profile a test needs.

#![cfg(unix)]
#![allow(dead_code)]

use std::fs;
use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// The shipped package, from the repository.
const PACKAGE_SOURCE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/packaging/synology/package");

/// The one framework alias the shipped scripts hard-code.
const COMMON_ALIAS_LINE: &str = "package_base=/var/packages/$package_name\n";
/// The one DSM volume root the shipped manager hard-codes.
const MANAGER_ROOT_LINE: &str = "dsm_system_root=/\n";

/// What the manager's bridge calls need, and nothing else. An unknown call fails
/// by name rather than succeeding, so a manager that learns a new bridge call
/// breaks this fixture visibly.
const BRIDGE_MOCK: &str = r#"#!/bin/sh
case "$*" in
    "--package-transition status") echo open ;;
    "--service-admission status") echo open ;;
    "--audit-transaction create "*) echo mock-transaction ;;
    "--audit-transaction reconcile"|"--audit-transaction begin "*|"--audit-transaction complete "*|"--audit-transaction execute "*) ;;
    *) echo "mock bridge: unexpected call: $*" >&2; exit 64 ;;
esac
exit 0
"#;

pub struct DsmPackage {
    root: PathBuf,
}

fn private_dir(path: &Path) {
    fs::create_dir_all(path).unwrap_or_else(|e| panic!("create {}: {e}", path.display()));
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .unwrap_or_else(|e| panic!("chmod {}: {e}", path.display()));
}

fn copy_tree(from: &Path, to: &Path) -> io::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), &target)?;
            // The shipped scripts are not executable in the repository (the package build
            // sets the bit), so a checkout that keeps modes, or one that does not, both
            // need it set on the copy that is run.
            fs::set_permissions(&target, fs::Permissions::from_mode(0o755))?;
        }
    }
    Ok(())
}

fn patch_once(path: &Path, from: &str, to: &str) {
    let text = fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    assert_eq!(
        text.matches(from).count(),
        1,
        "{} must hold the line {from:?} exactly once; the shipped script changed and this fixture \
         must follow it",
        path.display()
    );
    fs::write(path, text.replacen(from, to, 1))
        .unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
}

impl DsmPackage {
    /// Lay the tree out under `root` (which must not exist yet). `core` is the
    /// shipped core binary, which the manager runs to validate the configuration.
    ///
    /// Panics when run as root: the manager (and the daemon) insist on the
    /// package user's identity, and a root-owned fixture would test something
    /// else. Run the suite as an unprivileged user, as CI does.
    pub fn build(root: &Path, core: &Path) -> Self {
        let _ = fs::remove_dir_all(root);
        private_dir(root);
        assert_ne!(
            fs::metadata(root).unwrap().uid(),
            0,
            "the DSM-shaped fixtures need an unprivileged user (the package identity); this run \
             is root. Re-run as a non-root user, e.g. docker run -u 1000."
        );
        for dir in [
            "@apphome",
            "@apphome/config",
            "@apphome/config/profiles.d",
            "@apphome/config/routines.d",
            "@apphome/secrets",
            "@appdata",
        ] {
            private_dir(&root.join(dir));
        }
        let store = root.join("@appstore");
        for dir in ["bin", "libexec"] {
            copy_tree(&Path::new(PACKAGE_SOURCE).join(dir), &store.join(dir))
                .unwrap_or_else(|e| panic!("copy the shipped package {dir}: {e}"));
        }
        patch_once(
            &store.join("libexec/sdsync-common"),
            COMMON_ALIAS_LINE,
            &format!("package_base={}\n", root.display()),
        );
        let volumes = root.join("sysroot");
        private_dir(&volumes);
        private_dir(&volumes.join("volume1"));
        patch_once(
            &store.join("bin/sdsync-dsm"),
            MANAGER_ROOT_LINE,
            &format!("dsm_system_root={}\n", volumes.display()),
        );
        let bridge = store.join("bin/sdsync-dsm-api");
        fs::write(&bridge, BRIDGE_MOCK).unwrap();
        fs::set_permissions(&bridge, fs::Permissions::from_mode(0o755)).unwrap();
        symlink(core, store.join("bin/synology-drive-sync"))
            .unwrap_or_else(|e| panic!("link the core: {e}"));
        for (alias, real) in [
            ("home", "@apphome"),
            ("var", "@appdata"),
            ("target", "@appstore"),
        ] {
            symlink(root.join(real), root.join(alias))
                .unwrap_or_else(|e| panic!("link {alias}: {e}"));
        }
        Self {
            root: root.to_path_buf(),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// A path under the package root (`home/...`, `var/...`, `target/...`).
    pub fn p(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    /// The directory a profile may sync: the manager admits sources only under a
    /// DSM volume. Created on first use.
    pub fn source_dir(&self, name: &str) -> PathBuf {
        let dir = self.root.join("sysroot/volume1").join(name);
        fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("create {}: {e}", dir.display()));
        dir
    }

    /// The profile's password file, as the manager names it in the fragment.
    pub fn write_password(&self, name: &str) -> PathBuf {
        let path = self
            .root
            .join("home/secrets")
            .join(format!("{name}.password"));
        fs::write(&path, b"secret\n").unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        path
    }

    /// The shipped manager, with the package's own environment and nothing else.
    pub fn manager(&self) -> Command {
        let mut command = Command::new(self.root.join("target/bin/sdsync-dsm"));
        command
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("SYNOPKG_PKGDEST", self.root.join("target"))
            .env("SYNOPKG_PKGHOME", self.root.join("home"))
            .env("SYNOPKG_PKGVAR", self.root.join("var"))
            .env("SYNOPKG_PKGNAME", "synology-drive-sync")
            .env("SYNOPKG_DSM_VERSION_MAJOR", "7");
        command
    }

    /// Save profile `name` through the shipped manager's `configure-profile`
    /// (the fragment under `home/config/profiles.d`, the merged `config.toml`
    /// and the `default-profile` file all come from it), and write its password
    /// file. `overrides` replace or add manager options: `("--timeout", "2")`,
    /// and `("--default", "")` for a bare flag. The defaults are an HTTP target
    /// at `http://127.0.0.1:9` for user `operator`, whose source is
    /// [`DsmPackage::source_dir`].
    ///
    /// Panics with the manager's stdout and stderr when it refuses.
    pub fn configure_profile(&self, name: &str, overrides: &[(&str, &str)]) -> PathBuf {
        let source = self.source_dir(name);
        let remote = format!("/home/Drive/{name}");
        let mut options: Vec<(String, String)> = [
            ("--name", name),
            ("--source", source.to_str().unwrap()),
            ("--url", "http://127.0.0.1:9"),
            ("--username", "operator"),
            ("--remote", remote.as_str()),
            ("--allow-http", ""),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        for (flag, value) in overrides {
            match options.iter_mut().find(|(k, _)| k == flag) {
                Some(slot) => slot.1 = value.to_string(),
                None => options.push((flag.to_string(), value.to_string())),
            }
        }
        let mut command = self.manager();
        command.arg("configure-profile");
        for (flag, value) in &options {
            command.arg(flag);
            if !value.is_empty() {
                command.arg(value);
            }
        }
        let output: Output = command
            .output()
            .unwrap_or_else(|e| panic!("run the manager: {e}"));
        assert!(
            output.status.success(),
            "the manager refused configure-profile {name}: {}\nstdout: {}\nstderr: {}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        self.write_password(name);
        self.p(&format!("home/config/profiles.d/{name}.toml"))
    }
}
