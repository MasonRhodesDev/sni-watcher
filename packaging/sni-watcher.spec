# RPM spec for sni-watcher. Built in COPR from a local SRPM produced by
# packaging/build-srpm.sh (source tarball from the git tag + vendored cargo
# deps as Source1 — no rust-*-devel packages needed).
%bcond_without check

Name:           sni-watcher
Version:        0.1.0
Release:        1%{?dist}
Summary:        Standalone StatusNotifierWatcher daemon for a persistent system tray
License:        MIT
URL:            https://github.com/MasonRhodesDev/sni-watcher
Source0:        %{url}/archive/v%{version}/%{name}-%{version}.tar.gz
Source1:        %{name}-%{version}-vendor.tar.xz

BuildRequires:  cargo-rpm-macros >= 24
BuildRequires:  systemd-rpm-macros
Requires:       systemd
%{?systemd_requires}

%description
A standalone org.kde.StatusNotifierWatcher D-Bus daemon. Hosting the system-tray
registry in a separate, headless process decouples it from the status bar's
lifecycle, so restarting (or freezing) the bar no longer drops tray items.
Applications that register only once stay in the tray across bar restarts; the
bar attaches as a host-only client and re-reads the intact registry.

%prep
# -a1 unpacks the vendor tarball (vendor/ at its root) into the source dir.
%autosetup -p1 -a1
%cargo_prep -v vendor

%build
%cargo_build
%{cargo_license_summary}
%{cargo_license} > LICENSE.dependencies

%install
%cargo_install
install -Dpm0644 dist/sni-watcher.service %{buildroot}%{_userunitdir}/sni-watcher.service
install -Dpm0644 dist/90-sni-watcher.user.preset %{buildroot}%{_userpresetdir}/90-sni-watcher.preset

%if %{with check}
%check
%cargo_test
%endif

%post
%systemd_user_post sni-watcher.service

%preun
%systemd_user_preun sni-watcher.service

%postun
%systemd_user_postun_with_restart sni-watcher.service

%files
%license LICENSE LICENSE.dependencies
%doc README.md
%{_bindir}/sni-watcher
%{_userunitdir}/sni-watcher.service
%{_userpresetdir}/90-sni-watcher.preset

%changelog
* Mon Jun 29 2026 Mason Rhodes <mrhodesdev@gmail.com> - 0.1.0-1
- Initial release: standalone StatusNotifierWatcher daemon so the tray registry
  survives Waybar restarts (fixes Slack vanishing after hyprctl reload)
