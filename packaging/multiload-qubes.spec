Name:           multiload-qubes
Version:        0.4.5
Release:        1%{?dist}
Summary:        Whole-machine CPU, RAM and network graphs for the Qubes XFCE panel
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
Native XFCE panel plugin displaying scrolling CPU, memory and network graphs for Qubes.
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
