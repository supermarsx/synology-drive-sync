#![deny(unsafe_op_in_unsafe_fn)]

#[path = "../dsm_api.rs"]
mod dsm_api;

fn main() -> std::process::ExitCode {
    dsm_api::main_entry()
}
