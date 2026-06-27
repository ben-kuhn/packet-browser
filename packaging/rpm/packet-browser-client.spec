Name: packet-browser-client
Version: 0.2.0
Release: 1%{?dist}
Summary: Client component for Packet Browser - Web browser over AX.25 packet radio
License: MIT
URL: https://github.com/yourusername/docker-packet-browser
Source0: %{url}/archive/v%{version}/%{name}-%{version}.tar.gz

BuildRequires: rust-toolset
BuildRequires: openssl-devel
BuildRequires: pkg-config

%description
Packet Browser Client provides a web-based interface for browsing web pages
over AX.25 packet radio connections via AGWPE.

%prep
%setup -q

%build
cargo build --release --bin packet-browser-client

%install
install -D -m 0755 target/release/packet-browser-client %{buildroot}%{_bindir}/packet-browser-client
install -D -m 0644 client/config.example.ini %{buildroot}%{_sysconfdir}/packet-browser/config.ini.example
install -D -m 0644 packaging/systemd/packet-browser-client.service %{buildroot}%{_unitdir}/packet-browser-client.service

%files
%{_bindir}/packet-browser-client
%config(noreplace) %{_sysconfdir}/packet-browser/config.ini.example
%{_unitdir}/packet-browser-client.service

%changelog
* Thu Jun 26 2026 Your Name <your.email@example.com> - 0.2.0-1
- Initial package
