# Multiload for Qubes

A native GTK3 XFCE panel plugin with scrolling charts for whole-machine CPU,
RAM, network, swap and disk I/O.

Inspired by good old [indicator-multiload](https://github.com/mh21/indicator-multiload)
by Michael Hofmann, and the more modern [multiload-ng](https://github.com/udda/multiload-ng) by Mario Cianciolo.

Implemented primarily in Rust, with a small C adapter for the XFCE plugin
registration.

## Graphs

### Whole-machine CPU

A single filled chart, 100% means the full machine's CPU capacity.

### Per-qube CPU

Multiple independent lines chart, where every separate line corresponds to a
particular qube, and a line going to 100% means that single qube is at full CPU
capacity.

### Whole-machine RAM

A filled chart with 3 stacked layers:

- Used (reported as actually used by individual linux qubes, summed up)
- Allocated but unknown (e.g. allocated to non-linux qubes where we don't
  collect actual usage)
- Allocated but spare (reported as unused by individual linux qubes, summed up)

Where 100% is full machine RAM.

### Per-qube RAM

Multiple independent lines chart, where every separate line corresponds to a
particular qube, and a line going to 100% means that single qube is at full RAM
capacity.

## Building

```bash
sudo dnf install rust cargo gcc make pkgconf-pkg-config gtk3-devel xfce4-panel-devel xen-devel
cargo fetch --locked
make
make test
```

GTK3 bindings are enabled by the optional Cargo `panel` feature. A standalone
collector build does not enable it:

```bash
cargo build --release --offline --no-default-features --bin multiload-qubes-collector
```

`make` builds the collector first, then the panel library with `--features panel`,
and keeps a separate panel archive in `build/` for the C adapter to link. This
prevents standalone Cargo builds from replacing the panel archive with a library
that has GTK disabled. The standalone preview is a Cargo binary (`src/bin/preview.rs`); it links no
XFCE adapter. The UI modules live in `src/panel/`: preferences, config,
theme files, runtime and preview/integration checks. Graph history, Rust-string
tooltips and typed Cairo drawing live in `src/graphs/`. The panel calls that Rust
API directly; there is no separate graph C API or raw Cairo-pointer interface.
Only `native/plugin.c` uses the XFCE C API. Its Rust boundary is isolated in
`src/panel/ffi.rs`: callback declarations, raw handle ownership and a safe Host
wrapper. The panel runtime uses typed methods, and shutdown disconnects the host
so retained modal callbacks cannot access a destroyed native plugin.

The `panel` build uses cbindgen (a pinned Cargo build dependency) to generate
`build/panel-api.h` directly from `src/panel/ffi.rs`. The C adapter includes this
header instead of maintaining its own callback table and Rust function
prototypes. The generated file is ignored by Git, regenerated when the Rust API
or `cbindgen.toml` changes, and recreated if deleted. No separately installed
cbindgen tool is required; `cargo fetch --locked` includes it for
offline builds. Standalone collector builds do not enable this build dependency.

Optional lint tools: `sudo dnf install rustfmt clippy`, then `make check`.
Optional GUI tests: `sudo dnf install xorg-x11-server-Xvfb`, then `make gui-test`.
These test the shared plugin entry point and widget lifecycle, as well as settings
and persistence, using an isolated virtual display and temporary configuration.
Rust/Cargo and the development dependencies have already been installed on this
development machine. The plugin is not installed in your panel automatically.

Run `build/multiload-qubes-preview` for a clearly labelled **simulated** preview.
Add `--preferences` to inspect the Rust preferences dialog.
It does not require Xen or alter the panel. For an image without a display:

```bash
build/multiload-qubes-preview --render build/preview.png
```

## Build an RPM for dom0

```bash
sudo dnf install rpm-build systemd-rpm-macros
make rpm
```

`make rpm` runs the regular incremental build and tests in this checkout, then
packages the resulting binaries. It reuses Cargo's normal `target/` directory;
RPM packaging does not compile the project again or vendor dependencies.
Binary RPMs are written under `build/rpm/RPMS/`. The initial checkout needs
`cargo fetch --locked` once before the offline Make targets. The same command
works in CI from a fresh checkout after fetching dependencies.
