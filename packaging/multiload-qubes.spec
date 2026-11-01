# Binaries are built in the checkout, not in RPM's source directory.
%undefine _debugsource_packages

Name:           multiload-qubes
# x-release-please-start-version
Version:        0.2.0
# x-release-please-end
Release:        1%{?dist}
Summary:        Whole-machine CPU, RAM, network, swap and disk I/O graphs for the Qubes XFCE panel
License:        GPL-2.0-or-later
Source0:        %{name}-payload.tar.gz
BuildRequires:  systemd-rpm-macros
Requires(pre):  /usr/bin/systemd-sysusers
Requires:       xfce4-panel
Requires:       xen-libs
Requires:       /usr/bin/qvm-run
%{?systemd_requires}

%description
Native XFCE panel plugin displaying scrolling CPU, memory, network, swap and disk I/O graphs for Qubes.
A privileged collector publishes host statistics and raw guest reports.
A separate restricted service parses guest reports for the panel.

%prep
%setup -q -c -T -n %{name}-payload
tar -xzf %{SOURCE0}

%install
mkdir -p %{buildroot}
cp -a usr %{buildroot}/

%pre
systemd-sysusers - <<'EOF'
u multiload-qubes - "Multiload guest report parser" / -
EOF

%post
%systemd_post multiload-qubes-collector.service multiload-qubes-parser.service

%preun
%systemd_preun multiload-qubes-collector.service multiload-qubes-parser.service

%postun
%systemd_postun_with_restart multiload-qubes-collector.service multiload-qubes-parser.service

%files
%license LICENSE
%doc README.md
%{_libdir}/xfce4/panel/plugins/libmultiload-qubes.so
%{_datadir}/xfce4/panel/plugins/multiload-qubes.desktop
%{_libexecdir}/multiload-qubes-collector
%{_libexecdir}/multiload-qubes-parser
%{_unitdir}/multiload-qubes-collector.service
%{_unitdir}/multiload-qubes-parser.service
%{_sysusersdir}/multiload-qubes.conf

%changelog
* Sat Oct 31 2026 Dmitry Frank - 0.1.0-1
- Initial release: CPU, RAM, network, swap and disk I/O charts for the Qubes XFCE panel.
