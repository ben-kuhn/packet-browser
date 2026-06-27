# Copyright 2024 Gentoo Authors
# Distributed under the terms of the GNU General Public License v2

EAPI=8

CRATES=""
inherit cargo systemd

DESCRIPTION="Client component for Packet Browser - Web browser over AX.25 packet radio"
HOMEPAGE="https://github.com/yourusername/docker-packet-browser"
SRC_URI="https://github.com/yourusername/docker-packet-browser/archive/v${PV}.tar.gz -> ${P}.tar.gz"

LICENSE="MIT"
SLOT="0"
KEYWORDS="~amd64 ~arm64"

DEPEND="
	dev-libs/openssl:0=
"
RDEPEND="${DEPEND}"
BDEPEND="
	virtual/rust
"

S="${WORKDIR}/docker-packet-browser-${PV}"

src_unpack() {
	default
	cargo_src_unpack
}

src_compile() {
	cargo_src_compile --bin packet-browser-client
}

src_install() {
	dobin target/release/packet-browser-client
	
	# Install example config
	insinto /etc/packet-browser
	doins client/config.example.ini
	
	# Install systemd service
	systemd_dounit packaging/systemd/packet-browser-client.service
	
	# Create log directory
	keepdir /var/log/packet-browser
	fowners packet-browser:packet-browser /var/log/packet-browser
}

pkg_postinst() {
	enewgroup packet-browser
	enewuser packet-browser -1 -1 /var/lib/packet-browser packet-browser
}
