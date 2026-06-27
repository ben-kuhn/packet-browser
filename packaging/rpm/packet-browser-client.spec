Name: packet-browser-client
Version: 0.2.0
Release: 1%{?dist}
Summary: Client component for Packet Browser - Web browser over AX.25 packet radio
License: MIT
URL: https://github.com/yourusername/docker-packet-browser
Source0: packet-browser-x86_64-unknown-linux-gnu.tar.gz

# No build dependencies needed - packaging pre-built binaries
AutoReqProv: no

%description
Packet Browser Client provides a web-based interface for browsing web pages
over AX.25 packet radio connections via AGWPE.

%prep
# No prep needed - we're packaging pre-built binaries

%build
# No build needed - binaries are pre-built

%install
mkdir -p %{buildroot}%{_bindir}
mkdir -p %{buildroot}%{_sysconfdir}/packet-browser
mkdir -p %{buildroot}%{_unitdir}

# Install pre-built binaries from the tarball
tar -xzf %{SOURCE0} -C %{buildroot}%{_bindir} --strip-components=1 packet-browser-client

# Install config example
cp config.example.ini %{buildroot}%{_sysconfdir}/packet-browser/config.ini.example

# Install systemd service
cp packet-browser-client.service %{buildroot}%{_unitdir}/packet-browser-client.service

%files
%{_bindir}/packet-browser-client
%config(noreplace) %{_sysconfdir}/packet-browser/config.ini.example
%{_unitdir}/packet-browser-client.service

%changelog
* Thu Jun 26 2026 Your Name <your.email@example.com> - 0.2.0-1
- Initial package
