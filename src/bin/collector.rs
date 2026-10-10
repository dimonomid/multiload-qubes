#[path = "../collector/mod.rs"]
mod collector;

fn options(args: impl IntoIterator<Item = String>) -> Result<Option<collector::Options>, String> {
    let mut options = collector::Options {
        once: false,
        interval: 1.0,
        output: "/run/multiload-qubes/metrics".into(),
        network: Some("sys-net".into()),
        memory: true,
    };
    let mut no_network = false;
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => {
                println!("multiload-qubes-collector\n  --once\n  --interval SECONDS (0.25–10)\n  --output PATH\n  --network-qube NAME\n  --no-network\n  --no-guest-memory");
                return Ok(None);
            }
            "--once" => options.once = true,
            "--no-network" => no_network = true,
            "--no-guest-memory" => options.memory = false,
            "--interval" => {
                options.interval = args
                    .next()
                    .ok_or("missing interval")?
                    .parse()
                    .map_err(|_| "invalid interval")?
            }
            "--output" => options.output = args.next().ok_or("missing output path")?.into(),
            "--network-qube" => {
                let name = args.next().ok_or("missing network qube")?;
                if !collector::protocol::qube_name(&name) {
                    return Err("invalid network qube name".into());
                }
                options.network = Some(name);
            }
            _ => return Err(format!("unknown option {arg}")),
        }
    }
    if !options.interval.is_finite() || !(0.25..=10.0).contains(&options.interval) {
        return Err("interval must be between 0.25 and 10 seconds".into());
    }
    if options.output.as_os_str().is_empty() {
        return Err("empty output path".into());
    }
    if no_network {
        options.network = None;
    }
    Ok(Some(options))
}
fn main() {
    let result = options(std::env::args().skip(1)).and_then(|o| match o {
        Some(o) => collector::run(o),
        None => Ok(()),
    });
    if let Err(error) = result {
        eprintln!("multiload-qubes-collector: {error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cli_defaults_and_validation() {
        assert!(options(Vec::new()).unwrap().unwrap().memory);
        for args in [
            vec!["--interval", "NaN"],
            vec!["--interval", "0"],
            vec!["--output", ""],
            vec!["--network-qube", "x;bad"],
            vec!["--interval"],
            vec!["--bad"],
        ] {
            assert!(options(args.into_iter().map(str::to_string)).is_err());
        }
        let o = options(["--no-network", "--no-guest-memory", "--once"].map(str::to_string))
            .unwrap()
            .unwrap();
        assert!(o.once && !o.memory && o.network.is_none());
    }
}
