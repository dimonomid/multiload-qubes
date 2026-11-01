# Changelog

## [0.2.0](https://github.com/dimonomid/multiload-qubes/compare/v0.1.0...v0.2.0) (2026-11-01)

### Security improvements

- Guest reports are now parsed by a separate service running under a restricted
  account. The privileged collector only collects host data and passes through
  bounded guest reports.
- Stricter limits on input from qubes and the parser, safer snapshot file
  reads, and better cleanup of reporter processes.
- RPMs and checksums built on GitHub now come with Sigstore signatures.

### Other improvements

- Collection and drawing now follow one cadence. Qubes report when asked, the
  parser publishes once per tick, and charts redraw when new data arrives.
- Lower memory and CPU use: chart history follows the chart width, and idle
  collection threads sleep until there is work to do.
- The network scale now starts at 256 KiB/s, so tiny transfers don't fill the
  chart.
- Per-qube CPU and RAM charts are disabled by default on new installations.
  They can still be enabled in Properties.
- Unknown CPU usage now gets a missing-data marker instead of looking like zero
  usage.

### Breaking change

- Renamed `--no-guest-memory` to `--no-qube-metrics`. This is fine since there
  are no users of this app yet, except myself.

## 0.1.0 (2026-10-31)


### Features

* initial release ([d7b9fbf](https://github.com/dimonomid/multiload-qubes/commit/d7b9fbfceb97156e9fb0cc2380bdb0c6ba69549c))
