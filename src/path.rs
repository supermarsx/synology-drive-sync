use std::path::Path;

use crate::{Error, Result};

const DSM_MANAGED_NAMES: &[&str] = &[
    "#recycle",
    "#snapshot",
    "@eaDir",
    "@tmp",
    "@sharebin",
    ".SynologyWorkingDirectory",
];

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteRoot(String);

impl RemoteRoot {
    pub fn parse(value: &str) -> Result<Self> {
        if !value.starts_with('/') {
            return Err(unsafe_path(
                value,
                "it must begin with a shared-folder slash",
            ));
        }
        if value == "/" {
            return Err(unsafe_path(
                value,
                "DSM root is never a valid sync destination",
            ));
        }
        if value.ends_with('/') {
            return Err(unsafe_path(value, "trailing slashes are not allowed"));
        }
        if value.contains('\\') {
            return Err(unsafe_path(value, "use forward slashes, not backslashes"));
        }
        if value.chars().any(char::is_control) {
            return Err(unsafe_path(value, "control characters are not allowed"));
        }

        let components: Vec<_> = value[1..].split('/').collect();
        if components.iter().any(|part| part.is_empty()) {
            return Err(unsafe_path(value, "empty path components are not allowed"));
        }
        if components.iter().any(|part| *part == "." || *part == "..") {
            return Err(unsafe_path(value, ". and .. components are not allowed"));
        }
        if components.iter().any(|part| is_dsm_managed_name(part)) {
            return Err(unsafe_path(
                value,
                "DSM-managed directories cannot be sync roots",
            ));
        }

        Ok(Self(value.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn share_name(&self) -> &str {
        self.0[1..]
            .split('/')
            .next()
            .expect("validated remote root")
    }

    pub fn share_path(&self) -> String {
        format!("/{}", self.share_name())
    }

    pub fn join(&self, relative: &str) -> Result<String> {
        validate_relative(relative)?;
        if relative.is_empty() {
            Ok(self.0.clone())
        } else {
            Ok(format!("{}/{}", self.0, relative))
        }
    }

    pub fn relative(&self, remote: &str) -> Result<String> {
        if remote == self.0 {
            return Ok(String::new());
        }
        let prefix = format!("{}/", self.0);
        let relative = remote
            .strip_prefix(&prefix)
            .ok_or_else(|| Error::RemoteEscape(remote.to_owned()))?;
        validate_relative(relative)?;
        Ok(relative.to_owned())
    }

    pub fn contains_child(&self, remote: &str) -> bool {
        remote.starts_with(&format!("{}/", self.0))
    }
}

pub fn validate_relative(value: &str) -> Result<()> {
    if value.is_empty() {
        return Ok(());
    }
    if value.starts_with('/') || value.ends_with('/') || value.contains('\\') {
        return Err(unsafe_path(value, "relative path is not normalized"));
    }
    if value.chars().any(char::is_control) {
        return Err(unsafe_path(value, "control characters are not allowed"));
    }
    for part in value.split('/') {
        if part.is_empty() || part == "." || part == ".." || part.contains('/') {
            return Err(unsafe_path(
                value,
                "relative path contains an unsafe component",
            ));
        }
    }
    Ok(())
}

pub fn parent_and_name(remote: &str) -> Result<(&str, &str)> {
    let (parent, name) = remote
        .rsplit_once('/')
        .ok_or_else(|| unsafe_path(remote, "remote path has no parent"))?;
    if parent.is_empty() || name.is_empty() || name == "." || name == ".." {
        return Err(unsafe_path(
            remote,
            "remote path has an invalid final component",
        ));
    }
    Ok((parent, name))
}

pub fn depth(relative: &str) -> usize {
    if relative.is_empty() {
        0
    } else {
        relative.bytes().filter(|byte| *byte == b'/').count() + 1
    }
}

pub fn is_dsm_managed(relative: &str) -> bool {
    relative.split('/').any(is_dsm_managed_name)
}

pub fn path_for_match(relative: &str) -> &Path {
    Path::new(relative)
}

fn is_dsm_managed_name(name: &str) -> bool {
    DSM_MANAGED_NAMES.contains(&name)
}

fn unsafe_path(path: &str, reason: &str) -> Error {
    Error::UnsafeRemotePath {
        path: path.to_owned(),
        reason: reason.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_and_maps_remote_root() {
        let root = RemoteRoot::parse("/team/project").unwrap();
        assert_eq!(root.share_name(), "team");
        assert_eq!(root.join("a/b.txt").unwrap(), "/team/project/a/b.txt");
        assert_eq!(root.relative("/team/project/a/b.txt").unwrap(), "a/b.txt");
        assert!(!root.contains_child("/team/project-two/a"));
    }

    #[test]
    fn rejects_dangerous_roots_and_traversal() {
        for value in [
            "/",
            "relative",
            "/share/",
            "/share//x",
            "/share/../x",
            "/share/@eaDir",
            "/share/escape\u{1b}",
        ] {
            assert!(RemoteRoot::parse(value).is_err(), "{value}");
        }
        let root = RemoteRoot::parse("/share/target").unwrap();
        assert!(root.join("../outside").is_err());
        assert!(root.join("line\nbreak").is_err());
        assert!(root.relative("/share/targeted/file").is_err());
    }
}
