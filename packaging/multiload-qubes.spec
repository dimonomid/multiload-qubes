Name:           multiload-qubes
Version:        0.11.1
Release:        1%{?dist}
Summary:        Whole-machine CPU, RAM, network, swap and disk I/O graphs for the Qubes XFCE panel
License:        GPL-2.0-or-later
Source0:        %{name}-%{version}.tar.gz
Source1:        %{name}-%{version}-vendor.tar.gz
BuildRequires:  gcc
BuildRequires:  cargo
BuildRequires:  rust
BuildRequires:  make
BuildRequires:  pkgconfig(gtk+-3.0)
BuildRequires:  pkgconfig(libxfce4panel-2.0)
BuildRequires:  xen-devel
BuildRequires:  python3
BuildRequires:  systemd-rpm-macros
Requires:       xfce4-panel
Requires:       xen-libs
Requires:       /usr/bin/qvm-run
%{?systemd_requires}

%description
Native XFCE panel plugin displaying scrolling CPU, memory, network, swap and disk I/O graphs for Qubes.
A read-only dom0 collector publishes Xen allocation and guest-reported usage.

%prep
%autosetup
tar -xzf %{SOURCE1}
mkdir -p .cargo
cp packaging/cargo-vendor.toml .cargo/config.toml

%build
%make_build CFLAGS='%{optflags}'

%check
make test

%install
%make_install LIBDIR=%{_libdir} UNITDIR=%{_unitdir}

%post
%systemd_post multiload-qubes-collector.service

%preun
%systemd_preun multiload-qubes-collector.service

%postun
%systemd_postun_with_restart multiload-qubes-collector.service

%files
%license LICENSE
%doc README.md
%{_libdir}/xfce4/panel/plugins/libmultiload-qubes.so
%{_datadir}/xfce4/panel/plugins/multiload-qubes.desktop
%{_libexecdir}/multiload-qubes-collector
%{_unitdir}/multiload-qubes-collector.service

%changelog
* Thu Oct 29 2026 Multiload for Qubes contributors - 0.11.1-1
- Restore chart button-press events so the XFCE right-click menu opens

* Thu Oct 29 2026 Multiload for Qubes contributors - 0.11.0-1
- Move GTK preferences, theme storage, settings and panel lifecycle into Rust
- Keep a minimal XFCE C adapter and allow standalone collector builds without GTK

* Thu Oct 29 2026 Multiload for Qubes contributors - 0.10.0-1
- Add named custom theme files, draft protection and save/rename/delete controls
- Restore flat Solarized Light and add Solarized Light Gradient as a separate preset

* Thu Oct 29 2026 Multiload for Qubes contributors - 0.9.2-1
- Use the custom CPU background gradient for all Solarized Light charts

* Thu Oct 29 2026 Multiload for Qubes contributors - 0.9.1-1
- Disable borders in every built-in appearance preset

* Thu Oct 29 2026 Multiload for Qubes contributors - 0.9.0-1
- Add Appearance preferences with presets, RGBA colors, transparency and border widths

* Wed Oct 28 2026 Multiload for Qubes contributors - 0.8.1-1
- Label individual-domain charts Per-qube CPU, Per-qube RAM and Per-qube swap in Properties

* Wed Oct 28 2026 Multiload for Qubes contributors - 0.8.0-1
- Add per-qube CPU lines using online vCPU counts and show host contributions

* Wed Oct 28 2026 Multiload for Qubes contributors - 0.7.2-1
- Make the per-qube RAM fill more visible while retaining bright pixel-sharp lines

* Wed Oct 28 2026 Multiload for Qubes contributors - 0.7.1-1
- Draw per-qube RAM and swap with pixel-aligned points and no antialiasing

* Wed Oct 28 2026 Multiload for Qubes contributors - 0.7.0-1
- Add a per-qube RAM percentage chart immediately after the host RAM chart

* Wed Oct 28 2026 Multiload for Qubes contributors - 0.6.3-1
- Use concise scale labels and Physical disk activity wording in tooltips

* Wed Oct 28 2026 Multiload for Qubes contributors - 0.6.2-1
- Clarify that host disk activity includes qube I/O and dom0

* Wed Oct 28 2026 Multiload for Qubes contributors - 0.6.1-1
- Show the current full-chart speed in network and disk tooltips

* Wed Oct 28 2026 Multiload for Qubes contributors - 0.6.0-1
- Add physical-disk read/write activity with multiload-ng colors and per-chart tooltips

* Wed Oct 28 2026 Multiload for Qubes contributors - 0.5.1-1
- Show chart-specific tooltips for horizontal and vertical panel layouts

* Wed Oct 28 2026 Multiload for Qubes contributors - 0.5.0-1
- Add per-domain swap lines, maximum fill and reporting coverage over existing RAM streams

* Wed Oct 28 2026 Multiload for Qubes contributors - 0.4.6-1
- Share snapshot fields, validation and MQ1/MQ2 encoding between collector and panel

* Wed Oct 28 2026 Multiload for Qubes contributors - 0.4.5-1
- Include command exit status and bounded sanitized stderr in failure diagnostics

* Wed Oct 28 2026 Multiload for Qubes contributors - 0.4.4-1
- Track waiting/reporting/retrying states and log guest failures and recoveries

* Wed Oct 28 2026 Multiload for Qubes contributors - 0.4.3-1
- Use libloading for Xen library ownership, symbol checks and fallback diagnostics

* Wed Oct 28 2026 Multiload for Qubes contributors - 0.4.2-1
- Use nix and signal-hook for Unix operations and scoped shutdown handling

* Wed Oct 28 2026 Multiload for Qubes contributors - 0.4.1-1
- Replace custom CLI parsing with clap; bundle dependencies for offline RPM builds

* Wed Oct 28 2026 Multiload for Qubes contributors - 0.4.0-1
- Replace Python dom0 collector with Rust; preserve guest scripts and protocols

* Wed Oct 28 2026 Multiload for Qubes contributors - 0.3.0-1
- Sample guest RAM directly over persistent Qubes connections

* Wed Oct 28 2026 Multiload for Qubes contributors - 0.2.0-1
- Add network chart using a persistent sys-net counter stream

* Wed Oct 28 2026 Multiload for Qubes contributors - 0.1.0-1
- Initial CPU and memory panel plugin
