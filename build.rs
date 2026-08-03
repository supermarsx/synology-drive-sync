use std::env;

fn main() {
    println!("cargo:rerun-if-env-changed=SDSYNC_BUILD_VERSION");

    let version = env::var("SDSYNC_BUILD_VERSION")
        .or_else(|_| env::var("CARGO_PKG_VERSION"))
        .expect("Cargo always provides CARGO_PKG_VERSION");

    assert!(
        !version.is_empty()
            && version.len() <= 64
            && version
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'+')),
        "SDSYNC_BUILD_VERSION must be a short release-safe version such as 26.1"
    );

    println!("cargo:rustc-env=SDSYNC_VERSION={version}");
}
