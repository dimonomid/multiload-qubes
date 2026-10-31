# Binaries are built in the checkout, not in RPM's source directory.
%undefine _debugsource_packages

Name:           multiload-qubes
Version:        0.1.0
Release:        1%{?dist}
Summary:        Whole-machine CPU, RAM, network, swap and disk I/O graphs for the Qubes XFCE panel
License:        GPL-2.0-or-later
Source0:        %{name}-payload.tar.gz
BuildRequires:  systemd-rpm-macros
Requires:       xfce4-panel
Requires:       xen-libs
Requires:       /usr/bin/qvm-run
%{?systemd_requires}

%description
Native XFCE panel plugin displaying scrolling CPU, memory, network, swap and disk I/O graphs for Qubes.
A read-only dom0 collector publishes Xen allocation and guest-reported usage.

%prep
%setup -q -c -T -n %{name}-payload
tar -xzf %{SOURCE0}

%install
mkdir -p %{buildroot}
cp -a usr %{buildroot}/

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
* Sat Oct 31 2026 Dmitry Frank - 0.1.0-1
- Initial release: CPU, RAM, network, swap and disk I/O charts for the Qubes XFCE panel.
