Name:           multiload-qubes
Version:        0.1.0
Release:        1%{?dist}
Summary:        Whole-machine CPU and RAM graphs for the Qubes XFCE panel
License:        GPL-2.0-or-later
Source0:        %{name}-%{version}.tar.gz
BuildRequires:  gcc
BuildRequires:  cargo
BuildRequires:  rust
BuildRequires:  make
BuildRequires:  pkgconfig(gtk+-3.0)
BuildRequires:  pkgconfig(libxfce4panel-2.0)
BuildRequires:  python3
BuildRequires:  systemd-rpm-macros
Requires:       xfce4-panel
Requires:       python3
Requires:       python3-xen
%{?systemd_requires}

%description
Native XFCE panel plugin displaying scrolling CPU and memory graphs for Qubes.
A read-only dom0 collector publishes Xen allocation and guest-reported usage.

%prep
%autosetup

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
* Wed Oct 28 2026 Multiload for Qubes contributors - 0.1.0-1
- Initial CPU and memory panel plugin
