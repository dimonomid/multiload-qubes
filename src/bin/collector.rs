#[path = "../collector/mod.rs"]
mod collector;

use clap::Parser;
use std::path::PathBuf;

/// Collect Qubes CPU, RAM, network, swap and physical-disk activity.
#[derive(Parser, Debug)]
#[command(name = "multiload-qubes-collector", version)]
struct Cli {
    /// Print one snapshot without starting guest sampling connections.
    #[arg(long)]
    once: bool,

    /// Collection interval in seconds (0.25–10).
    #[arg(long, default_value_t = 0.5, value_parser = parse_interval)]
    interval: f64,

    /// Path of the atomic snapshot read by the panel.
    #[arg(long, default_value = "/run/multiload-qubes/metrics")]
    output: PathBuf,

    /// Qube owning the physical network interfaces.
    #[arg(long, default_value = "sys-net", value_parser = parse_qube)]
    network_qube: String,

    /// Disable network collection, regardless of --network-qube.
    #[arg(long)]
    no_network: bool,

    /// Disable guest RAM/swap collection; their RAM allocation remains unknown.
    #[arg(long)]
    no_guest_memory: bool,
}

fn parse_interval(value: &str) -> Result<f64, String> {
    let interval: f64 = value.parse().map_err(|_| "expected a number of seconds")?;
    if !interval.is_finite() || !(0.25..=10.0).contains(&interval) {
        return Err("interval must be between 0.25 and 10 seconds".into());
    }
    Ok(interval)
}

fn parse_qube(value: &str) -> Result<String, String> {
    if !collector::protocol::qube_name(value) {
        return Err("expected a valid Qubes name (1–31 ASCII letters, digits, '-' or '_', starting with a letter)".into());
    }
    Ok(value.into())
}

impl From<Cli> for collector::Options {
    fn from(cli: Cli) -> Self {
        Self {
            once: cli.once,
            interval: cli.interval,
            output: cli.output,
            network: (!cli.no_network).then_some(cli.network_qube),
            memory: !cli.no_guest_memory,
        }
    }
}

fn main() {
    if let Err(error) = collector::run(Cli::parse().into()) {
        eprintln!("multiload-qubes-collector: {error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::error::ErrorKind;

    fn parse(args: &[&str]) -> Result<collector::Options, clap::Error> {
        Cli::try_parse_from(std::iter::once("collector").chain(args.iter().copied()))
            .map(Into::into)
    }

    #[test]
    fn defaults_and_existing_options() {
        let options = parse(&[]).unwrap();
        assert!(!options.once && options.memory);
        assert_eq!(options.interval, 0.5);
        assert_eq!(
            options.output,
            PathBuf::from("/run/multiload-qubes/metrics")
        );
        assert_eq!(options.network.as_deref(), Some("sys-net"));
        let options = parse(&[
            "--once",
            "--interval",
            "0.25",
            "--output",
            "/tmp/metrics",
            "--network-qube",
            "other-net",
            "--no-guest-memory",
        ])
        .unwrap();
        assert!(options.once && !options.memory);
        assert_eq!(options.interval, 0.25);
        assert_eq!(options.output, PathBuf::from("/tmp/metrics"));
        assert_eq!(options.network.as_deref(), Some("other-net"));
        assert_eq!(parse(&["--interval=10"]).unwrap().interval, 10.0);
    }

    #[test]
    fn rejects_invalid_values_and_missing_arguments() {
        for args in [
            vec!["--interval", "NaN"],
            vec!["--interval=inf"],
            vec!["--interval=0"],
            vec!["--interval=10.1"],
            vec!["--interval=not-a-number"],
            vec!["--output", ""],
            vec!["--network-qube", "x;bad"],
            vec!["--network-qube=-name"],
            vec!["--interval"],
            vec!["--output"],
            vec!["--bad"],
        ] {
            assert!(parse(&args).is_err(), "accepted {args:?}");
        }
    }

    #[test]
    fn disabling_network_is_independent_of_argument_order() {
        for args in [
            vec!["--no-network", "--network-qube", "other-net"],
            vec!["--network-qube=other-net", "--no-network"],
        ] {
            assert!(parse(&args).unwrap().network.is_none());
        }
    }

    #[test]
    fn help_and_version_exit_before_xen_access() {
        assert_eq!(
            Cli::try_parse_from(["collector", "--help"])
                .unwrap_err()
                .kind(),
            ErrorKind::DisplayHelp
        );
        assert_eq!(
            Cli::try_parse_from(["collector", "--version"])
                .unwrap_err()
                .kind(),
            ErrorKind::DisplayVersion
        );
    }
}
