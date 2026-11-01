# Changelog

## [0.3.0](https://github.com/dimonomid/multiload-qubes/compare/v0.2.0...v0.3.0) (2026-11-01)


### ⚠ BREAKING CHANGES

* rename --no-guest-memory to --no-qube-metrics

### Features

* Don't enable by default some per-qube charts ([b83cf5e](https://github.com/dimonomid/multiload-qubes/commit/b83cf5eeb2613b0952b257a8cc16a1aedcae264c))
* Draw the charts right when the snapshot is updated ([1fd40c1](https://github.com/dimonomid/multiload-qubes/commit/1fd40c185f1f7d89a5e818c83bf1ea968d2a884f))
* harden reading snapshot in xfce panel plugin ([be18b75](https://github.com/dimonomid/multiload-qubes/commit/be18b75b2aa1c294e197f27ff95caecfb0ccc916))
* initial release ([d7b9fbf](https://github.com/dimonomid/multiload-qubes/commit/d7b9fbfceb97156e9fb0cc2380bdb0c6ba69549c))
* Make guest reporting follow main cadence ([5753e8d](https://github.com/dimonomid/multiload-qubes/commit/5753e8dc3f9e4d43191994b422f3de65d52847bf))
* Make minimal network traffic scale 256 KiB ([29aab2d](https://github.com/dimonomid/multiload-qubes/commit/29aab2d4d77952973cecf104a32d1c36d7c1ddc3))
* Make parser generate output snapshot only once per tick ([093c1e3](https://github.com/dimonomid/multiload-qubes/commit/093c1e3ddc75b39aea0257b48a9efe6dc57199e4))
* rename --no-guest-memory to --no-qube-metrics ([adc5393](https://github.com/dimonomid/multiload-qubes/commit/adc5393108ed9b5ce6fee539f8da8ba5a925cb71))
* Sign github-built binaries ([01415ac](https://github.com/dimonomid/multiload-qubes/commit/01415ac0c44865204f6bf530dda46b8d45f70abb))
* Split collector and parser in separate services ([4b274a9](https://github.com/dimonomid/multiload-qubes/commit/4b274a9e8813cbff84bf6cbfd88b0a7a043a20fb))


### Bug Fixes

* Keep recent guest readings across tick deadlines ([2512eb8](https://github.com/dimonomid/multiload-qubes/commit/2512eb8a2078bef9bb83d9e46f5cdf551e9d8da9))
* kill remaining process-group members before reaping parent ([550873f](https://github.com/dimonomid/multiload-qubes/commit/550873fd41713f69fc77a9eadf23012c5e51e028))
* Limit chart history size to current chart width ([85f697d](https://github.com/dimonomid/multiload-qubes/commit/85f697d6cd6cd907cc8873ea14ab9059f7ad64ed))
* show missing-data marker for unknown CPU samples ([5d50285](https://github.com/dimonomid/multiload-qubes/commit/5d5028591e48ece5782b0e998210e2cac1c3e61c))
* start guest reply deadline after Xen sampling and conn setup ([1d68a05](https://github.com/dimonomid/multiload-qubes/commit/1d68a059b30735034ad1e4016d687b8178831165))
* use monotonic time for guest reply deadlines ([21458ad](https://github.com/dimonomid/multiload-qubes/commit/21458adfd4c3136fde14f087d7aaa440a3152592))


### Performance Improvements

* sleep until the next collector tick ([76ac2a0](https://github.com/dimonomid/multiload-qubes/commit/76ac2a018e773b6cbeec494255b26cd45b53fdf1))
* stop polling idle reporter connections ([8b8aeaa](https://github.com/dimonomid/multiload-qubes/commit/8b8aeaa56ca71e086e68cb19f52b4058da297d48))

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
