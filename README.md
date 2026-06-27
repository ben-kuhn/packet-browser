# Packet Browser

[![Build and Publish](https://github.com/ben-kuhn/docker-packet-browser/actions/workflows/build.yml/badge.svg)](https://github.com/ben-kuhn/docker-packet-browser/actions/workflows/build.yml)

A client/server web browser for packet radio. The **server** (behind BPQ) fetches and sanitizes web pages using headless Chromium, compresses them with brotli, and sends them over AX.25. The **client** connects via AGWPE and provides a local web proxy that users browse with their regular browser.

## Architecture

```
┌─────────────────────────────────────────────────────────────────────┐
│                        Your Local Machine                            │
│                                                                       │
│  ┌──────────────┐    ┌──────────────────┐    ┌──────────────────┐  │
│  │   Browser     │───▶│  Client (proxy)   │───▶│  AGWPE / TNC     │  │
│  │ localhost:8080│    │  Web UI + Proxy   │    │  (Direwolf, etc) │  │
│  └──────────────┘    └──────────────────┘    └──────────────────┘  │
│                                                                       │
└─────────────────────────────────────────────────────────────────────┘
                            │
                            │ AX.25 over radio
                            ▼
┌─────────────────────────────────────────────────────────────────────┐
│                        Remote BBS / Node                             │
│                                                                       │
│  ┌──────────────┐    ┌──────────────────┐    ┌──────────────────┐  │
│  │  LinBPQ       │───▶│  Server           │───▶│  Headless        │  │
│  │  (node)       │    │  TCP:63004        │    │  Chromium        │  │
│  └──────────────┘    └──────────────────┘    └──────────────────┘  │
│                                                                       │
└─────────────────────────────────────────────────────────────────────┘
```

### How It Works

1. **Client** connects to AGWPE (your local TNC like Direwolf)
2. **Client** establishes AX.25 connection to a remote BPQ node
3. **Client** sends BPQ APPLICATION command (e.g., `WEB`) to trigger the browser application
4. **BPQ** connects to the **server** via TCP (port 63004)
5. **Server** prompts for callsign, client sends it
6. **Server** shows logging disclaimer, client sends `AGREE`
7. User opens browser to client's web UI (default `http://localhost:8080`)
8. User enters a URL or clicks links
9. **Client** sends request over AX.25 to **server**
10. **Server** fetches page with headless Chromium, sanitizes HTML, compresses with brotli
11. **Server** sends compressed response back over AX.25
12. **Client** decompresses, rewrites URLs to route through local proxy, displays in browser

### BPQ Handshake

After AX.25 connection is established, the client performs an automated handshake:

1. Client sends the BPQ APPLICATION command (e.g., `WEB\n`)
2. BPQ recognizes the command and connects to the server via TCP
3. Server sends callsign prompt, client sends configured callsign
4. Server sends logging disclaimer with "AGREE" prompt, client sends `AGREE\n`
5. Server sends portal page, handshake complete

This is fully automated - no user interaction required.

---

## Server

The server runs behind a BPQ node and handles web page fetching, sanitization, and compression.

### Running with Docker

#### Quick Start

```bash
# Create directory and required files
mkdir -p packet-browser/logs
touch packet-browser/hosts

# Create docker-compose.yml (see below)
nano docker-compose.yml

# Pull and start
docker compose pull
docker compose up -d
```

#### Docker Compose Configuration

```yaml
services:
  packet-browser:
    image: ghcr.io/ben-kuhn/docker-packet-browser:latest

    ports:
      # Bind to loopback only by default (security)
      - "127.0.0.1:63004:63004"

    volumes:
      # Logs - accessible from host
      - ./logs:/var/log/packet-browser
      # Hosts file for blocklist management
      - ./hosts:/etc/hosts

    environment:
      # Service configuration
      - LISTEN_PORT=63004
      - PORTAL_URL=https://www.zeroretries.radio
      - IDLE_TIMEOUT_MINUTES=10
      - BROTLI_QUALITY=11

      # SSRF prevention - blocked IP ranges
      - BLOCKED_RANGES=127.0.0.0/8,10.0.0.0/8,172.16.0.0/12,192.168.0.0/16,169.254.0.0/16

      # Blocklist settings
      - BLOCKLIST_ENABLED=true
      - BLOCKLIST_REFRESH_HOURS=24
      - BLOCKLIST_URLS=https://cdn.jsdelivr.net/gh/hagezi/dns-blocklists@latest/hosts/ultimate.txt

      # Logging
      - LOG_ROTATE_ENABLED=true
      - LOG_RETAIN_DAYS=30
      - SYSLOG_ENABLED=false

    # Security hardening
    read_only: true
    tmpfs:
      - /tmp:size=128M,mode=1777
      - /dev/shm:size=64M,mode=1777
    cap_drop:
      - ALL

    # DNS filtering - OpenDNS Family Shield
    dns:
      - 208.67.222.123
      - 208.67.220.123

    healthcheck:
      test: ["CMD", "/bin/packet-browser-server", "--healthcheck"]
      interval: 30s
      timeout: 5s
      retries: 3
      start_period: 60s

    restart: unless-stopped
```

#### Server Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `LISTEN_PORT` | `63004` | TCP port the service listens on |
| `PORTAL_URL` | `https://www.zeroretries.radio` | Default home page shown on connect |
| `IDLE_TIMEOUT_MINUTES` | `10` | Session timeout for idle connections |
| `BROTLI_QUALITY` | `11` | Brotli compression level (0-11) |
| `BLOCKED_RANGES` | `127.0.0.0/8,10.0.0.0/8,...` | CIDR ranges blocked for SSRF prevention |
| `BLOCKLIST_ENABLED` | `true` | Enable/disable local hosts-based blocklist |
| `BLOCKLIST_REFRESH_HOURS` | `24` | How often to refresh blocklists from URLs |
| `BLOCKLIST_URLS` | *(empty)* | Comma-separated URLs of hosts-format blocklists |
| `LOG_ROTATE_ENABLED` | `true` | Enable automatic log rotation |
| `LOG_RETAIN_DAYS` | `30` | Number of days to retain rotated logs |
| `SYSLOG_ENABLED` | `false` | Forward logs to external syslog server |
| `SYSLOG_HOST` | *(empty)* | Syslog server hostname or IP |
| `SYSLOG_PORT` | `514` | Syslog server port |

### BPQ Configuration

Add the following to your `bpq32.cfg`:

```
# Add port 63004 to CMDPORT list
# Position in list determines HOST number (0-indexed)
CMDPORT=63001,63002,63003,63004

# Define application
# Syntax: APPLICATION <num>,<command>,C <port> HOST <position> S
APPLICATION 4,WEB,C 10 HOST 3 S
```

**Explanation:**
- `CMDPORT=...` - List of TCP ports, comma-separated
- Port 63004 is at position 3 (0-indexed: 63001=0, 63002=1, 63003=2, 63004=3)
- `APPLICATION 4,WEB,C 10 HOST 3 S`
  - `4` - Application number (1-32, must be unique)
  - `WEB` - Command users type to access browser
  - `C 10` - Use BPQ port 10 (adjust to your telnet port)
  - `HOST 3` - Connect to CMDPORT position 3 (port 63004)
  - `S` - Return user to node menu on disconnect (Stay)

### Server Security Features

- **Content Filtering**: DNS filtering (OpenDNS Family Shield) + hosts-based blocklist
- **SSRF Prevention**: Blocks private IP ranges by default
- **Protocol Filtering**: Only HTTP/HTTPS allowed (no file://, ftp://, etc.)
- **Container Hardening**: Read-only filesystem, no shell, capability dropping, non-root user
- **Session Security**: Idle timeout, callsign validation, logging acknowledgment

---

## Client

The client runs on your local machine and provides a web proxy interface. It connects to AGWPE (your TNC) for the radio link and serves pages to your browser.

### Installation

#### Pre-built Binaries

Download from [GitHub Releases](https://github.com/ben-kuhn/docker-packet-browser/releases):

| Platform | File |
|----------|------|
| Linux x86_64 | `packet-browser-x86_64-unknown-linux-gnu.tar.gz` |
| Linux aarch64 | `packet-browser-aarch64-unknown-linux-gnu.tar.gz` |
| macOS x86_64 | `packet-browser-x86_64-apple-darwin.tar.gz` |
| macOS aarch64 | `packet-browser-aarch64-apple-darwin.tar.gz` |
| Windows x86_64 | `packet-browser-x86_64-pc-windows-msvc.zip` |
| Debian/Ubuntu | `packet-browser-client_0.2.0_amd64.deb` |
| Fedora/RHEL | `packet-browser-client-0.2.0-1.x86_64.rpm` |

#### Linux (Debian/Ubuntu)

```bash
# Download and install
wget https://github.com/ben-kuhn/docker-packet-browser/releases/latest/download/packet-browser-client_0.2.0_amd64.deb
sudo dpkg -i packet-browser-client_0.2.0_amd64.deb

# Copy and edit config
sudo cp /etc/packet-browser/config.ini.example /etc/packet-browser/config.ini
sudo nano /etc/packet-browser/config.ini

# Enable and start service
sudo systemctl enable --now packet-browser-client
```

#### Linux (Fedora/RHEL)

```bash
# Download and install
wget https://github.com/ben-kuhn/docker-packet-browser/releases/latest/download/packet-browser-client-0.2.0-1.x86_64.rpm
sudo rpm -i packet-browser-client-0.2.0-1.x86_64.rpm

# Copy and edit config
sudo cp /etc/packet-browser/config.ini.example /etc/packet-browser/config.ini
sudo nano /etc/packet-browser/config.ini

# Enable and start service
sudo systemctl enable --now packet-browser-client
```

#### Arch Linux

```bash
# Using the PKGBUILD from packaging/arch/
cd packaging/arch
makepkg -si

# Copy and edit config
sudo cp /etc/packet-browser/config.ini.example /etc/packet-browser/config.ini
sudo nano /etc/packet-browser/config.ini

# Enable and start service
sudo systemctl enable --now packet-browser-client
```

#### Gentoo

```bash
# Using the ebuild from packaging/gentoo/
sudo cp packaging/gentoo/packet-browser-client-0.2.0.ebuild /var/db/repos/local/net-misc/packet-browser-client/
sudo ebuild /var/db/repos/local/net-misc/packet-browser-client/packet-browser-client-0.2.0.ebuild merge

# Copy and edit config
sudo cp /etc/packet-browser/config.ini.example /etc/packet-browser/config.ini
sudo nano /etc/packet-browser/config.ini

# Enable and start service
sudo rc-update add packet-browser-client default
sudo rc-service packet-browser-client start
```

#### NixOS

```nix
# In your configuration.nix
{ pkgs, ... }:
{
  services.packet-browser-client = {
    enable = true;
    config = {
      agwpe_host = "127.0.0.1";
      agwpe_port = 8000;
      my_callsign = "N0CALL";
      target_callsign = "NODE1";
      bpq_command = "WEB";
    };
  };
}
```

Or using the package directly:

```bash
nix-env -iA nixos.packet-browser-client
```

#### macOS

```bash
# Download and extract
curl -L https://github.com/ben-kuhn/docker-packet-browser/releases/latest/download/packet-browser-aarch64-apple-darwin.tar.gz | tar xz

# Create config directory
mkdir -p ~/.config/packet-browser
cp config.example.ini ~/.config/packet-browser/config.ini
nano ~/.config/packet-browser/config.ini

# Run
./packet-browser-client
```

#### Windows

```powershell
# Download and extract the zip file
# Create config directory
mkdir %APPDATA%\packet-browser
copy config.example.ini %APPDATA%\packet-browser\config.ini
notepad %APPDATA%\packet-browser\config.ini

# Run
packet-browser-client.exe
```

### Client Configuration

The client uses an INI configuration file. Location (in order of priority):

1. `--config` command-line argument
2. `~/.config/packet-browser/config.ini` (Linux/macOS)
3. `%APPDATA%\packet-browser\config.ini` (Windows)
4. `/etc/packet-browser/config.ini` (system-wide)

#### Configuration File

```ini
[server]
# AGWPE TNC server address and port
agwpe_host = 127.0.0.1
agwpe_port = 8000

[session]
# Your amateur radio callsign
my_callsign = N0CALL

# Target node callsign to connect to
target_callsign = NODE1

# BPQ command to send after connection (must match BPQ APPLICATION command)
bpq_command = WEB
```

#### Configuration Options

| Section | Option | Default | Description |
|---------|--------|---------|-------------|
| `[server]` | `agwpe_host` | `127.0.0.1` | AGWPE TNC server hostname or IP |
| `[server]` | `agwpe_port` | `8000` | AGWPE TNC server port |
| `[session]` | `my_callsign` | *(empty)* | Your amateur radio callsign |
| `[session]` | `target_callsign` | *(empty)* | Target BPQ node callsign |
| `[session]` | `bpq_command` | `WEB` | BPQ APPLICATION command to send |

#### Command-Line Options

```
packet-browser-client [OPTIONS]

Options:
  -c, --config <PATH>        Configuration file (INI format)
      --agwpe-host <HOST>    AGWPE host (overrides config file)
      --agwpe-port <PORT>    AGWPE port (overrides config file)
      --listen-addr <ADDR>   Web proxy listen address [default: 127.0.0.1:8080]
      --bpq-command <CMD>    BPQ APPLICATION command [default: WEB]
  -v, --verbose...           Increase verbosity (-v, -vv, -vvv)
  -h, --help                 Print help
```

### Using the Client

#### Starting the Client

```bash
# Using config file
packet-browser-client

# Using command-line options
packet-browser-client --agwpe-host 192.168.1.100 --agwpe-port 8000 --listen-addr 0.0.0.0:8080

# Using custom config file
packet-browser-client --config /path/to/config.ini
```

#### Web Interface

Open your browser to `http://localhost:8080` (or your configured listen address).

**Connect Page** (`/connect`):
- AGWPE connection status
- Callsign configuration
- Port selection (queries AGWPE for available ports)
- AX.25 connect/disconnect buttons
- Live debug log with SSE updates

**Configuration Page** (`/configuration`):
- AGWPE host/port settings
- Save configuration to file

**Browse Page** (`/browse?url=...`):
- Displays fetched pages with rewritten links
- All links route through the local proxy
- Dark-themed UI optimized for readability

#### Typical Workflow

1. Start the client: `packet-browser-client`
2. Open browser to `http://localhost:8080`
3. On the Connect page, click "Connect to AGWPE"
4. Select your AGWPE port from the dropdown
5. Enter your callsign and target node callsign
6. Click "AX.25 Connect"
7. The client automatically performs the BPQ handshake
8. Enter a URL in the address bar or click links
9. Pages are fetched over the radio link and displayed

### Running as a Service

#### systemd (Linux)

```bash
# Enable and start
sudo systemctl enable --now packet-browser-client

# Check status
sudo systemctl status packet-browser-client

# View logs
sudo journalctl -u packet-browser-client -f
```

#### OpenRC (Gentoo)

```bash
# Add to default runlevel
sudo rc-update add packet-browser-client default

# Start now
sudo rc-service packet-browser-client start

# Check status
sudo rc-service packet-browser-client status
```

#### launchd (macOS)

Create `~/Library/LaunchAgents/com.packet-browser.client.plist`:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.packet-browser.client</string>
    <key>ProgramArguments</key>
    <array>
        <string>/usr/local/bin/packet-browser-client</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
</dict>
</plist>
```

```bash
# Load and start
launchctl load ~/Library/LaunchAgents/com.packet-browser.client.plist
```

#### Windows Service

Use [NSSM](https://nssm.cc/) to install as a Windows service:

```powershell
nssm install PacketBrowserClient "C:\path\to\packet-browser-client.exe"
nssm set PacketBrowserClient AppDirectory "C:\path\to"
nssm start PacketBrowserClient
```

---

## Building from Source

### Prerequisites

- Rust toolchain (rustc, cargo)
- Nix package manager with flakes enabled (for Docker image)
- Docker or Podman (for container deployment)

### Build Process

```bash
# Clone repository
git clone https://github.com/ben-kuhn/docker-packet-browser.git
cd docker-packet-browser

# Build with Nix (includes all dependencies)
nix build

# Binaries are in ./result/bin/
./result/bin/packet-browser-server
./result/bin/packet-browser-client
```

### Development

Enter the Nix development shell:

```bash
nix develop

# Now you have Rust toolchain, Chromium, and dependencies
cargo build
cargo test
cargo run --bin packet-browser-server
cargo run --bin packet-browser-client
```

### Building Docker Image

```bash
# Build with Nix
nix build .#docker-image

# Load into Docker
docker load < result

# Verify
docker images | grep packet-browser
```

### Running Tests

```bash
# Rust unit tests
nix develop -c cargo test --all-features -- --test-threads=1

# Python e2e tests (requires Direwolf, PipeWire, LinBPQ)
cd e2e
pip install -r requirements-test.txt
pytest -v
```

---

## Logging

### Server Logs

All user activity is logged in JSON format to `/var/log/packet-browser/access.log`:

```json
{"ts":"2026-03-19T14:32:01Z","call":"W1ABC","url":"https://example.com","status":"ok"}
{"ts":"2026-03-19T14:32:45Z","call":"W1ABC","url":"https://blocked.com","status":"blocked","reason":"dns_filter"}
```

**Log fields:**
- `ts` - ISO 8601 timestamp
- `call` - User callsign
- `url` - Requested URL
- `status` - Result: `ok`, `blocked`, `error`
- `reason` - Block/error reason (if applicable)

### Client Logs

Client logs are available via:
- systemd journal: `journalctl -u packet-browser-client -f`
- Web UI: Live debug log on the Connect page (SSE-based)
- stdout/stderr when running manually

---

## Security

### Content Filtering

**Layer 1: DNS Filtering**
- Uses OpenDNS Family Shield by default
- Blocks adult content, phishing, malware sites
- Configurable via DNS settings in Docker

**Layer 2: Hosts-based Blocklist**
- Fetches hosts-format blocklists from URLs on startup
- Writes blocked domains to `/etc/hosts` (resolves to 0.0.0.0)
- Refreshes every 24 hours (configurable)

### SSRF Prevention

By default, the following IP ranges are blocked:
- `127.0.0.0/8` - Loopback
- `10.0.0.0/8` - Private network
- `172.16.0.0/12` - Private network
- `192.168.0.0/16` - Private network
- `169.254.0.0/16` - Link-local

### Container Hardening

- Read-only root filesystem
- No shell binaries
- All capabilities dropped except NET_RAW (for DNS)
- Non-root user (UID 1000)
- tmpfs for /tmp (size-limited)
- Network isolation (loopback binding by default)

---

## Managing Blocklists

### Automatic Updates

Blocklists refresh automatically every 24 hours (configurable via `BLOCKLIST_REFRESH_HOURS`).

### Manual Host Blocking

Edit the hosts file volume to add custom blocks:

```bash
# Edit hosts file
nano hosts

# Add custom blocks using hosts format
0.0.0.0 unwanted-site.com
0.0.0.0 another-blocked.com

# Restart container to apply
docker compose restart
```

Custom entries outside the `# BLOCKLIST-MANAGED START/END` markers are preserved during automatic updates.

### Disabling Blocklist

```yaml
environment:
  - BLOCKLIST_ENABLED=false
```

---

## License

GNU General Public License v3 or later

Original browse.sh: Copyright 2019-2023 Red Tuby PE1RRR (SK)
Docker implementation: Copyright 2026 KU0HN

This program is free software: you can redistribute it and/or modify it under the terms of the GNU General Public License as published by the Free Software Foundation, either version 3 of the License, or (at your option) any later version.

This program is distributed in the hope that it will be useful, but WITHOUT ANY WARRANTY; without even the implied warranty of MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the GNU General Public License for more details.

## Credits

- **Red Tuby PE1RRR** (SK) - Original browse.sh implementation (2019-2023)
- **KU0HN** - Docker containerization and Rust port (2026)

## Support

Issues and pull requests: https://github.com/ben-kuhn/docker-packet-browser

For BPQ-specific questions, consult the BPQ32 documentation.
