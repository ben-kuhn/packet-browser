# Packet Browser

[![Build and Publish](https://github.com/ben-kuhn/packet-browser/actions/workflows/build.yml/badge.svg)](https://github.com/ben-kuhn/packet-browser/actions/workflows/build.yml)

> **Upgrade note (0.2.0):** The AX.25 wire format was extended in v0.2.0 to
> carry per-URL cache directives. **Client and server must upgrade together** —
> a 0.1.x endpoint talking to a 0.2.x endpoint will fail with `Invalid response
> header`. See `docs/superpowers/specs/2026-07-13-local-cache-design.md` for
> details.

A client/server web browser for packet radio. The **server** (behind BPQ) fetches and sanitizes web pages using headless Firefox, compresses them with brotli, and sends them over AX.25 in a printable, telnet-transparent framing. The **client** connects via AGWPE and provides a local web proxy that users browse with their regular browser.

All data crossing the air interface is public, unencrypted, and decodable with published algorithms (RFC 7932 brotli, RFC 4648 base64) — see [Wire Format & Part 97 Compliance](#wire-format--part-97-compliance) below.

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
│  │  (node)       │    │  TCP:63004        │    │  Firefox        │  │
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
6. **Server** shows logging disclaimer, client waits for operator consent, then sends `AGREE`
7. User opens browser to client's web UI (default `http://localhost:8080`)
8. User enters a URL or clicks links
9. **Client** sends a plain-text `GET <url>\n` (or `POST <url>\n<len_be><body>`) request over AX.25 to the **server**
10. **Server** fetches page with headless Firefox, sanitizes HTML, brotli-compresses the result, then base64-encodes it inside a printable `RESP…` frame
11. **Server** sends the framed, base64-encoded response back over AX.25
12. **Client** finds the `RESP` frame, base64-decodes and brotli-decompresses, rewrites URLs to route through the local proxy, and displays in the browser

### BPQ Handshake

After AX.25 connection is established, the client performs an automated handshake. All bytes exchanged in the handshake are printable ASCII:

1. Client sends the BPQ APPLICATION command (e.g., `WEB\n`), unless configured to skip it for a direct-alias connection.
2. BPQ recognizes the command and opens a TCP session to the server.
3. Server sends `Enter your callsign: `; client replies with the operator's configured callsign followed by `\n`.
4. Server sends the logging disclaimer text with a `Type AGREE to proceed: ` prompt. The client surfaces the disclaimer verbatim to the operator via a consent modal in the local web UI and blocks until the operator explicitly agrees.
5. On consent, the client sends `AGREE\n`. The server records the acknowledgement in its access log and switches into the framed request/response protocol.

The AGREE step is the real consent gate: the client does not auto-answer AGREE on the operator's behalf. If the operator declines or times out, the client tears down the AX.25 link. LinBPQ's HOST-0 telnet driver has been observed to duplicate the first line of AX.25 input to the server; the server tolerates this by discarding a second copy of the just-validated callsign before checking for `AGREE`.

### Wire Format & Part 97 Compliance

Everything Packet Browser puts on the air is transmitted in the clear and can be recovered by any observer with the same freely-available tooling. There is no encryption, no proprietary transform, no shared secret. The two transformations applied to the payload — brotli compression and base64 encoding — are both well-known techniques whose specifications are published for public use, satisfying 47 CFR §97.113(a)(4) and §97.309(a)(4).

**Request bytes (client → server).** Plain ASCII, LF-terminated:

- `GET <url>\n`
- `POST <url>\n<body_length_big_endian_u32><body_bytes>`

`<url>` is a UTF-8 URL string. Nothing about the request is compressed or encoded — a listener sees the URL verbatim.

**Response bytes (server → client).** A printable, telnet-transparent text frame:

```
RESP<status> <base64_len>\n<base64_payload>\n
```

- `RESP` — 4-byte ASCII magic (`0x52 0x45 0x53 0x50`) so a receiver can resync past any node banner text prepended by the transport.
- `<status>` — one ASCII digit: `0` = Ok, `1` = server error, `2` = URL blocked by the sysop's filter.
- `<base64_len>` — decimal ASCII length (in bytes) of the base64 payload that follows.
- Header terminator is `\n` or `\r` (LinBPQ's TELNET driver rewrites one to the other in either direction; either is accepted).
- `<base64_payload>` — the response body encoded with **RFC 4648 standard base64** (alphabet `A–Z a–z 0–9 + / =`, padded). Decoding the base64 yields the **RFC 7932 brotli**-compressed HTML (or plain-text error message for statuses `1` and `2`). Decompressing the brotli yields UTF-8 HTML that the operator's browser then renders. **Both algorithms are open, publicly documented, and implemented by widely available open-source libraries** (`brotli` and `base64` crates on the wire; any web browser's built-in brotli decoder and `openssl base64 -d` will recover the same bytes off a monitored packet capture).

**Why the two transformations?**

- **Brotli compression** — bandwidth. A typical modern web page is tens to hundreds of kilobytes; brotli at quality 11 typically produces a 10× reduction, which is the difference between a page arriving in seconds and arriving in minutes over a 1200-baud AX.25 link. Brotli is a lossless, deterministic reversal — the exact bytes the server's Firefox emitted are recoverable byte-for-byte.
- **Base64 encoding** — transport transparency, **not** to hide meaning. LinBPQ's TELNET / HOST-mode driver has been observed to strip NUL bytes (`0x00`) from the byte stream and to rewrite `\n` ↔ `\r` in both directions. The old raw-binary framing lost the leading bytes of every response and any brotli output that happened to contain a `0x00` byte. Base64 restricts the on-air bytes to `A–Z a–z 0–9 + / = \n \r` — all of which the driver leaves alone — so the receiver reconstructs the exact compressed bytes.

**Part 97 rationale.** §97.113(a)(4) prohibits "messages encoded for the purpose of obscuring their meaning." §97.309(a)(4) permits digital codes so long as the technique is "publicly documented" and the receiver has access to the specification. Packet Browser satisfies both:

- No encryption of any kind is applied. There is no key, shared secret, or per-station transformation.
- Both transformations are standardized, non-proprietary, and reversible with off-the-shelf software. RFC 4648 and RFC 7932 are freely readable; base64 decoders ship with virtually every OS; brotli decoders ship in every major web browser and are available as an open-source Rust crate (`brotli`), a C library (`libbrotli`), a Python module (`brotli`), and a command-line tool (`brotli(1)`).
- A third-party operator monitoring the frequency who reads this section can, with those tools alone, recover the exact HTML page that was transmitted. The intent and effect of the encoding is bandwidth reduction and transport-layer safety, not concealment.
- The server logs every URL retrieved (with the requesting operator's callsign and a timestamp) to `/var/log/packet-browser/access.log`, so the operator has a plain-text audit trail of what has been transmitted through their station.

The full protocol source of truth is `shared/src/protocol.rs` (`Response::encode` and `Response::decode_header`). The tests in that file assert that no NUL, CR, or `0xFF` byte ever appears on the wire and that the round trip is byte-exact for every possible payload byte value.

---

## Demo Mode (Off-Air Testing)

Demo mode sets up a complete virtual radio environment for testing without actual radio hardware. It uses Direwolf TNC emulators connected via PipeWire virtual audio to simulate the full AX.25 communication path.

### Prerequisites

- Direwolf (TNC emulator)
- PipeWire (audio routing)
- LinBPQ (BPQ node software)
- Built binaries (`cargo build` or `nix build`)

### Running Demo Mode

```bash
# From the project root
./demo.sh
```

The script will:
1. Start two Direwolf instances (client-side and server-side)
2. Cross-link their audio via PipeWire
3. Start LinBPQ configured to bridge between Direwolf and the server
4. Start packet-browser-server on a random port
5. Start packet-browser-client on a random port
6. Display access URLs and log locations

### What You Get

- **Client web UI**: Accessible at `http://127.0.0.1:<port>` (shown in output)
- **Full AX.25 path**: Client → Direwolf-A → (audio) → Direwolf-B → LinBPQ → Server
- **Live logs**: All component logs in a temporary directory
- **Clean shutdown**: Ctrl+C stops everything and cleans up

### Configuration

The demo script supports the following environment variables:

- **`BPQ_APP_NAME`** (default: `WEB`): The BPQ application command to send. Different sysops may use different application names.
- **`TARGET_CALLSIGN`** (default: `N0CALL-7`): The target callsign to connect to. Can be a node SSID or regular callsign.
- **`SKIP_BPQ_APP`** (default: `false`): Set to `true` to skip sending the BPQ application command. Useful when connecting directly to a node SSID that doesn't require an application command.

Example:
```bash
# Use a custom BPQ application name
BPQ_APP_NAME=BROWSE ./demo.sh

# Connect directly to a node SSID without sending an application command
SKIP_BPQ_APP=true TARGET_CALLSIGN=NODE-7 ./demo.sh
```

### Demo Mode Architecture

```
┌─────────────────────────────────────────────────────────────┐
│  Your Machine (Demo Mode)                                    │
│                                                               │
│  Browser ──▶ Client:8080 ──▶ Direwolf-A ──┐                 │
│                                              │ (audio)       │
│  Server:63004 ◀── LinBPQ ◀── Direwolf-B ───┘                 │
│       │                                                       │
│       └──▶ Firefox (fetches real web pages)                  │
└─────────────────────────────────────────────────────────────┘
```

### Testing in Demo Mode

1. Open the client web UI in your browser (URL shown in demo output)
2. The client automatically connects to AGWPE on startup
3. Click "AX.25 Connect" to establish connection to the BPQ node
4. Browse to any URL - it will be fetched through the virtual radio link!

If the auto-connect fails, check that:
- Your AGWPE modem/server is running
- The configuration in the Configuration tab is correct

### Troubleshooting

- **Audio issues**: Ensure PipeWire is running (`systemctl --user status pipewire`)
- **Port conflicts**: The script uses random free ports, but check if components fail to start
- **LinBPQ errors**: Check `linbpq.log` in the demo directory
- **Permission denied**: Make sure the script is executable (`chmod +x demo.sh`)

### VARA/Mercury Manual Testing

For VARA/Mercury manual testing, see `demo-vara.sh` — it prints the expected topology and required prerequisites.

Mercury/LinBPQ prerequisites:
  - Start Mercury on both ends
  - Configure LinBPQ on the server side with a VARA port pointing at Mercury's cmd/data ports
  - Open the client web UI `/connect` page and select "VARA HF" transport
  - Use the Mercury ports displayed in the script output

---

## Server

The server runs behind a BPQ node and handles web page fetching, sanitization, and compression.

### Running with Docker

#### Quick Start

```bash
# Create directory for logs
mkdir -p packet-browser/logs

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
    image: ghcr.io/ben-kuhn/packet-browser:latest

    ports:
      # Bind to loopback only by default (security)
      - "127.0.0.1:63004:63004"

    volumes:
      # Logs - accessible from host
      - ./logs:/var/log/packet-browser

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

    # Security hardening
    read_only: true
    tmpfs:
      # Larger than for Chromium — Firefox keeps profile + caches under /tmp.
      - /tmp:size=512M,mode=1777
      - /dev/shm:size=128M,mode=1777
    cap_drop:
      - ALL
    # Firefox's content-process sandbox needs unshare(CLONE_NEWUSER) and a
    # few other namespace syscalls that Docker's default profile denies. The
    # bundled packaging/seccomp/firefox.json is the Moby default plus those
    # syscalls -- see the threat-model section.
    security_opt:
      - seccomp=./packaging/seccomp/firefox.json
      - no-new-privileges:true

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
| `BLOCKLIST_ENABLED` | `true` | Enable/disable the in-process domain blocklist |
| `BLOCKLIST_REFRESH_HOURS` | `24` | How often to refresh blocklists from URLs |
| `BLOCKLIST_URLS` | *(empty)* | Comma-separated URLs of hosts-format blocklists |
| `FIREFOX_PATH` | `/bin/firefox` | Path to the Firefox binary (set in container image) |
| `GECKODRIVER_PATH` | `/bin/geckodriver` | Path to the geckodriver binary (set in container image) |

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

- **Content Filtering**: DNS filtering (OpenDNS Family Shield) + in-process domain blocklist enforced by the filtering proxy
- **SSRF Prevention**: Blocks private IP ranges by default (IPv4 + IPv6 reserved
  ranges, including IPv4-mapped, ULA, link-local, and the `0.0.0.0/8` sinkhole)
- **Protocol Filtering**: Only HTTP/HTTPS allowed (no file://, ftp://, etc.)
- **Container Hardening**: Read-only filesystem, no shell, capability dropping, non-root user
- **Session Security**: Idle timeout, callsign validation, logging acknowledgment

#### Threat-model caveats

These limits apply to the current deployment and are worth understanding
before exposing the server:

- **Renderer sandbox.** The headless Firefox content process runs inside
  Firefox's own user-namespace + seccomp-bpf sandbox, which the engine
  initializes from inside the container without requiring `CAP_SYS_ADMIN`.
  Docker's default seccomp profile blocks the namespace syscalls Firefox
  needs (`unshare(CLONE_NEWUSER)`, `clone` with `CLONE_NEW*` flags,
  `pivot_root`, `mount`). The compose file applies a custom profile at
  `packaging/seccomp/firefox.json` that is the Moby default plus targeted
  allows for those syscalls. Unlike `seccomp=unconfined`, this keeps the
  Docker default deny-list intact for every other privilege-escalation
  syscall (`bpf`, `perf_event_open`, `keyctl`, etc.). Net effect: an
  actual renderer sandbox with a syscall surface only marginally wider
  than the Docker default.
- **Subresource loads are filtered.** Firefox is configured to route all
  requests (HTTP and HTTPS) through an in-process forward proxy that runs
  every URL through `BLOCKED_RANGES` before opening a socket. For HTTPS
  the proxy validates the `CONNECT` target, resolves DNS once, and
  bidirectionally splices to the pinned IP; non-web CONNECT ports (80/443
  only) are refused up front. Nothing Firefox loads — stylesheets,
  fonts, `fetch()` from the sanitizer JS, subresources of subresources —
  bypasses this check.
- **DNS rebinding on subresource fetches is closed.** The proxy performs
  a single DNS resolution per URL and uses the same IP for both the
  block check and the outbound `connect()`, eliminating the window where
  filter and browser could resolve to different addresses.

### Running with Nix (without Docker)

Install from [nix-ham-packages](https://github.com/ben-kuhn/nix-ham-packages) overlay:

```bash
# Install the package
nix-env -iA nixos.packet-browser-server

# Or build directly from the overlay
nix build ham-packages#packet-browser-server
./result/bin/packet-browser-server
```

The server requires Firefox **and** geckodriver at runtime. Point both at the
right binaries via env vars (defaults are the in-container paths):

```bash
export FIREFOX_PATH=$(which firefox)
export GECKODRIVER_PATH=$(which geckodriver)
packet-browser-server
```

Or use the NixOS module (see Client section below for module import instructions).

---

## Client

The client runs on your local machine and provides a web proxy interface. It connects to AGWPE (your TNC) for the radio link and serves pages to your browser.

### Installation

#### Pre-built Binaries

Download from [GitHub Releases](https://github.com/ben-kuhn/packet-browser/releases):

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
wget https://github.com/ben-kuhn/packet-browser/releases/latest/download/packet-browser-client_0.2.0_amd64.deb
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
wget https://github.com/ben-kuhn/packet-browser/releases/latest/download/packet-browser-client-0.2.0-1.x86_64.rpm
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

Using the overlay from [nix-ham-packages](https://github.com/ben-kuhn/nix-ham-packages):

```nix
# In your flake.nix inputs
{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-24.11";
    ham-packages.url = "github:ben-kuhn/nix-ham-packages";
  };

  outputs = { self, nixpkgs, ham-packages }: {
    nixosConfigurations.your-hostname = nixpkgs.lib.nixosSystem {
      system = "x86_64-linux";
      modules = [
        {
          nixpkgs.overlays = [ ham-packages.overlays.default ];
        }
        ./configuration.nix
      ];
    };
  };
}
```

**Client NixOS Module:**

```nix
# In configuration.nix
{
  imports = [ "${ham-packages}/packet-browser-client/module.nix" ];

  services.packet-browser-client = {
    enable = true;
    myCallsign = "N0CALL";
    targetCallsign = "NODE1";
    agwpeHost = "127.0.0.1";
    agwpePort = 8000;
    bpqCommand = "WEB";
    listenAddr = "127.0.0.1:8080";
  };
}
```

**Server NixOS Module:**

```nix
# In configuration.nix
{
  imports = [ "${ham-packages}/packet-browser-server/module.nix" ];

  services.packet-browser-server = {
    enable = true;
    listenPort = 63004;
    portalUrl = "https://www.zeroretries.radio";
    idleTimeoutMinutes = 10;
    brotliQuality = 11;
    blocklistEnabled = true;
    blocklistUrls = [
      "https://cdn.jsdelivr.net/gh/hagezi/dns-blocklists@latest/hosts/ultimate.txt"
    ];
    openFirewall = false;
  };
}
```

**Installing packages directly:**

```bash
# Install client
nix-env -iA nixos.packet-browser-client

# Install server
nix-env -iA nixos.packet-browser-server
```

#### macOS

```bash
# Download and extract
curl -L https://github.com/ben-kuhn/packet-browser/releases/latest/download/packet-browser-aarch64-apple-darwin.tar.gz | tar xz

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

### Running with Nix (without Docker)

Install from [nix-ham-packages](https://github.com/ben-kuhn/nix-ham-packages) overlay:

```bash
# Install the package
nix-env -iA nixos.packet-browser-client

# Or build directly from the overlay
nix build ham-packages#packet-browser-client
./result/bin/packet-browser-client
```

Create a config file and run:

```bash
mkdir -p ~/.config/packet-browser
cp $(nix-build ham-packages#packet-browser-client)/share/packet-browser/config.ini.example \
   ~/.config/packet-browser/config.ini
nano ~/.config/packet-browser/config.ini

packet-browser-client
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
git clone https://github.com/ben-kuhn/packet-browser.git
cd packet-browser

# Build with Nix (includes all dependencies)
nix build

# Binaries are in ./result/bin/
./result/bin/packet-browser-server
./result/bin/packet-browser-client
```

### Nix Packages (nix-ham-packages)

For NixOS system installation, use the [nix-ham-packages](https://github.com/ben-kuhn/nix-ham-packages) overlay instead:

```bash
# Install packages via the overlay
nix-env -iA nixos.packet-browser-server
nix-env -iA nixos.packet-browser-client

# Or build directly
nix build github:ben-kuhn/nix-ham-packages#packet-browser-server
nix build github:ben-kuhn/nix-ham-packages#packet-browser-client
```

See the NixOS section above for systemd service module configuration.

### Development

Enter the Nix development shell:

```bash
nix develop

# Now you have Rust toolchain, Firefox, and dependencies
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

### Demo Mode (Off-Air Testing)

Demo mode sets up a complete virtual radio environment for testing without actual radio hardware. It uses Direwolf TNC emulators connected via PipeWire virtual audio to simulate the full AX.25 communication path.

#### Prerequisites

- Direwolf (TNC emulator)
- PipeWire (audio routing)
- LinBPQ (BPQ node software)
- Built binaries (`cargo build` or `nix build`)

#### Running Demo Mode

```bash
# From the project root
./demo.sh
```

The script will:
1. Start two Direwolf instances (client-side and server-side)
2. Cross-link their audio via PipeWire
3. Start LinBPQ configured to bridge between Direwolf and the server
4. Start packet-browser-server on a random port
5. Start packet-browser-client on a random port
6. Display access URLs and log locations

#### What You Get

- **Client web UI**: Accessible at `http://127.0.0.1:<port>` (shown in output)
- **Full AX.25 path**: Client → Direwolf-A → (audio) → Direwolf-B → LinBPQ → Server
- **Live logs**: All component logs in a temporary directory
- **Clean shutdown**: Ctrl+C stops everything and cleans up

#### Demo Mode Architecture

```
┌─────────────────────────────────────────────────────────────┐
│  Your Machine (Demo Mode)                                    │
│                                                               │
│  Browser ──▶ Client:8080 ──▶ Direwolf-A ──┐                 │
│                                              │ (audio)       │
│  Server:63004 ◀── LinBPQ ◀── Direwolf-B ───┘                 │
│       │                                                       │
│       └──▶ Firefox (fetches real web pages)                  │
└─────────────────────────────────────────────────────────────┘
```

#### Testing in Demo Mode

1. Open the client web UI in your browser
2. Click "Connect to AGWPE" to connect to the virtual TNC
3. Enter a target callsign (e.g., `N0CALL-7`)
4. Click "AX.25 Connect"
5. Browse to any URL - it will be fetched through the virtual radio link!

#### Troubleshooting

- **Audio issues**: Ensure PipeWire is running (`systemctl --user status pipewire`)
- **Port conflicts**: The script uses random free ports, but check if components fail to start
- **LinBPQ errors**: Check `linbpq.log` in the demo directory
- **Permission denied**: Make sure the script is executable (`chmod +x demo.sh`)

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

**Layer 2: In-process Domain Blocklist**
- Fetches hosts-format blocklists from URLs on startup
- Holds parsed domains in an in-memory `HashSet` (no `/etc/hosts` writes)
- The filtering proxy in front of Firefox consults the set before every
  DNS lookup, so entries are enforced on every request the browser issues
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

### Adding Custom Blocks

The blocklist is populated exclusively from URLs in `BLOCKLIST_URLS`, so a
custom local list just needs to live somewhere the container can reach.
The simplest path: publish your own hosts-format file (locally via a
`file:///` if the operator has filesystem access to the container, or
served from a small HTTP host you control) and add it to
`BLOCKLIST_URLS`:

```yaml
environment:
  - BLOCKLIST_URLS=https://cdn.jsdelivr.net/gh/hagezi/dns-blocklists@latest/hosts/ultimate.txt,https://internal.example/my-blocks.txt
```

`docker compose restart` picks up the change; the container refetches on
each start and again every `BLOCKLIST_REFRESH_HOURS` hours.

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

Issues and pull requests: https://github.com/ben-kuhn/packet-browser

For BPQ-specific questions, consult the BPQ32 documentation.
