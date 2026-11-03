//! `sdsyncd`: the single Rust daemon that replaces the NAS shell controller,
//! runner and queue (daemon design, Phase 2).
//!
//! Layout (design §2.1): portable, pure modules first, so Windows and macOS CI
//! build and test them; the Linux-only process, signal and socket code is
//! cfg-gated and keeps every `unsafe` call in `sys`.
//!
//! - `window`: daily windows and DST (port of `routine_window_state`).

// TEMPORARY while the modules land one by one; removed once `--daemon` wires them.
#![allow(dead_code)]
#![deny(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub(crate) mod executor;
pub(crate) mod routines;
pub(crate) mod state;
#[cfg(target_os = "linux")]
pub(crate) mod sys;
pub(crate) mod window;
