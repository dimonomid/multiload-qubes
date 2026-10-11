//! Standalone Rust entry point; no XFCE adapter is linked into this binary.
fn main() -> std::process::ExitCode {
    multiload_qubes::run_preview()
}
