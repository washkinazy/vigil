%global debug_package %{nil}

Name:           vigil
# x-release-please-start-version
Version:        0.5.0
# x-release-please-end
Release:        1%{?dist}
Summary:        Session idle manager for Wayland
License:        MIT
URL:            https://github.com/washkinazy/vigil
Source0:        https://github.com/washkinazy/vigil/archive/refs/tags/v%{version}.tar.gz#/%{name}-%{version}.tar.gz

BuildRequires:  rust >= 1.89
BuildRequires:  cargo
BuildRequires:  gcc
BuildRequires:  systemd-rpm-macros
Requires:       systemd
Requires:       pipewire-utils
Requires:       niri

%description
Vigil coordinates idle timers, activity inhibition, display power, locking,
and sleep for Wayland sessions. Includes vigild, vigilctl, and a tray indicator.

%prep
%autosetup

%build
cargo build --workspace --release --locked

%check
cargo test --workspace --release --locked --offline

%install
install -Dm755 target/release/vigild %{buildroot}%{_bindir}/vigild
install -Dm755 target/release/vigilctl %{buildroot}%{_bindir}/vigilctl
install -Dm644 packaging/vigil.service %{buildroot}%{_userunitdir}/vigil.service
install -Dm644 packaging/vigil-tray.service %{buildroot}%{_userunitdir}/vigil-tray.service
sed -i 's|%%h/.local/bin/|%{_bindir}/|g' %{buildroot}%{_userunitdir}/vigil*.service

%post
%systemd_user_post vigil.service vigil-tray.service

%preun
%systemd_user_preun vigil.service vigil-tray.service

%postun
%systemd_user_postun vigil.service vigil-tray.service

%files
%license LICENSE
%doc README.md CHANGELOG.md config.example.toml
%{_bindir}/vigild
%{_bindir}/vigilctl
%{_userunitdir}/vigil.service
%{_userunitdir}/vigil-tray.service

%changelog
* Fri Nov 06 2026 Washkinazy <noreply@github.com> - 0.1.0-1
- Initial package
