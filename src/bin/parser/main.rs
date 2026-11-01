use multiload_qubes::metrics;
mod runtime;

use clap::Parser;
use std::path::PathBuf;

/// Validate guest reports and publish metrics for the panel.
#[derive(Parser)]
#[command(name = "multiload-qubes-parser", version)]
struct Cli {
    /// Root-owned directory containing host context and raw guest reports.
    #[arg(long, default_value = "/run/multiload-qubes-host")]
    input: PathBuf,

    /// Path of the combined snapshot read by the panel.
    #[arg(long, default_value = "/run/multiload-qubes/metrics")]
    output: PathBuf,
}

fn main() {
    let cli = Cli::parse();
    if let Err(error) = runtime::run(runtime::Options {
        input: cli.input,
        output: cli.output,
    }) {
        eprintln!("multiload-qubes-parser: {error}");
        std::process::exit(1);
    }
}
