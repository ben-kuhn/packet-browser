#!/usr/bin/env bash
set -euo pipefail

# Packet Browser Demo Mode
# Sets up a complete off-air test environment with virtual radio

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DEMO_DIR=$(mktemp -d)
trap 'cleanup' EXIT

# Configuration (can be overridden via environment variables)
BPQ_APP_NAME="${BPQ_APP_NAME:-WEB}"
# Node callsign — the "front door" of the BPQ node.
NODE_CALLSIGN="${NODE_CALLSIGN:-N0CALL-7}"
# Optional alias callsign for the WEB app. When set, LinBPQ registers this
# callsign as a shortcut that invokes the app directly on AX.25 connect,
# with no typed "WEB\n" needed. Sysops who don't want to spare an extra
# SSID can set WEB_APP_ALIAS= (empty) to disable it, in which case the
# demo connects to NODE_CALLSIGN and types the app name at the prompt.
WEB_APP_ALIAS="${WEB_APP_ALIAS:-N0CALL-8}"
# If an alias is configured the client dials it directly and skips the
# node prompt; otherwise it dials the node callsign and lets the client's
# BPQ handshake send the app command.
if [[ -n "$WEB_APP_ALIAS" ]]; then
    TARGET_CALLSIGN="${TARGET_CALLSIGN:-$WEB_APP_ALIAS}"
    SKIP_BPQ_APP="${SKIP_BPQ_APP:-true}"
else
    TARGET_CALLSIGN="${TARGET_CALLSIGN:-$NODE_CALLSIGN}"
    SKIP_BPQ_APP="${SKIP_BPQ_APP:-false}"
fi

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

log()     { echo -e "${BLUE}[DEMO]${NC} $1" >&2; }
success() { echo -e "${GREEN}[✓]${NC} $1" >&2; }
warn()    { echo -e "${YELLOW}[!]${NC} $1" >&2; }
error()   { echo -e "${RED}[✗]${NC} $1" >&2; }

cleanup() {
    log "Cleaning up..."
    if [[ -f "$DEMO_DIR/pids" ]]; then
        while read -r pid; do
            kill "$pid" 2>/dev/null || true
        done < "$DEMO_DIR/pids"
    fi
    restore_pipewire
    rm -rf "$DEMO_DIR"
    success "Cleanup complete"
}

save_pid() { echo "$1" >> "$DEMO_DIR/pids"; }

check_dependencies() {
    log "Checking dependencies..."
    local missing=()
    for cmd in direwolf linbpq pw-link pw-dump python3; do
        command -v "$cmd" &>/dev/null || missing+=("$cmd")
    done
    if [[ ! -f "$SCRIPT_DIR/target/debug/packet-browser-server" ]]; then
        missing+=("packet-browser-server (run: cargo build)")
    fi
    if [[ ! -f "$SCRIPT_DIR/target/debug/packet-browser-client" ]]; then
        missing+=("packet-browser-client (run: cargo build)")
    fi
    if [[ ${#missing[@]} -gt 0 ]]; then
        error "Missing dependencies: ${missing[*]}"
        exit 1
    fi
    success "All dependencies found"
}

find_free_port() {
    # Use ports in 10000-20000 range to avoid Direwolf port validation issues
    python3 -c 'import socket, random; port = random.randint(10000, 20000); s=socket.socket(); s.bind(("", port)); print(port); s.close()' 2>/dev/null || \
    python3 -c 'import socket; s=socket.socket(); s.bind(("", 0)); print(s.getsockname()[1]); s.close()'
}

# Tune PipeWire clock settings for reliable AFSK audio and disconnect
# the direwolfs from real hardware. Original settings are saved to
# $DEMO_DIR/pw-original.json so restore_pipewire can put them back.
# Mirrors pw_configure_for_test() in e2e/helpers.py.
configure_pipewire() {
    log "Tuning PipeWire clock settings for AFSK audio"
    python3 - "$DEMO_DIR/pw-original.json" << 'PYTHON'
import json, re, subprocess, sys

result = subprocess.run(
    ["pw-metadata", "-n", "settings"],
    capture_output=True, text=True, timeout=5,
)
original = {}
for line in result.stdout.splitlines():
    for key in ("clock.allowed-rates", "clock.quantum",
                "clock.min-quantum", "clock.max-quantum"):
        if f"key:'{key}'" in line:
            m = re.search(r"value:'(.+?)'\s+type:", line)
            if m:
                original[key] = m.group(1)
with open(sys.argv[1], "w") as f:
    json.dump(original, f)

settings = {
    "clock.allowed-rates": "[ 44100, 48000, 192000 ]",
    "clock.quantum": "1024",
    "clock.min-quantum": "256",
    "clock.max-quantum": "8192",
}
for key, value in settings.items():
    subprocess.run(
        ["pw-metadata", "-n", "settings", "0", key, value],
        capture_output=True, timeout=5,
    )
PYTHON
}

restore_pipewire() {
    [[ -f "$DEMO_DIR/pw-original.json" ]] || return 0
    python3 - "$DEMO_DIR/pw-original.json" << 'PYTHON'
import json, subprocess, sys
with open(sys.argv[1]) as f:
    original = json.load(f)
for key, value in original.items():
    subprocess.run(
        ["pw-metadata", "-n", "settings", "0", key, value],
        capture_output=True, timeout=5,
    )
PYTHON
}

start_direwolf() {
    local name="$1"
    local callsign="$2"
    local agwpe_port="$3"
    local config_file="$DEMO_DIR/direwolf-$name.conf"
    local log_file="$DEMO_DIR/direwolf-$name.log"

    cat > "$config_file" <<EOF
ADEVICE default
ACHANNELS 1
ARATE 44100
MYCALL $callsign
MODEM 1200
AGWPORT 0
AGWPORT $agwpe_port
KISSPORT 0
FULLDUP ON
TXDELAY 10
TXTAIL 5
SLOTTIME 10
PERSIST 63
EOF

    log "Starting Direwolf $name ($callsign) on AGWPE port $agwpe_port"
    direwolf -c "$config_file" -t 0 > "$log_file" 2>&1 &
    local pid=$!
    save_pid "$pid"

    # Wait for AGWPE port to be ready
    local waited=0
    while ! python3 -c "import socket; s=socket.create_connection(('127.0.0.1', $agwpe_port), timeout=0.5); s.close()" 2>/dev/null; do
        sleep 0.5
        waited=$((waited + 1))
        if [[ $waited -ge 20 ]]; then
            error "Direwolf $name AGWPE port $agwpe_port not ready after 10s"
            error "Process PID $pid status:"
            if kill -0 "$pid" 2>/dev/null; then
                error "  Process is still running"
            else
                error "  Process has exited"
            fi
            error "Log output:"
            cat "$log_file" >&2
            exit 1
        fi
    done

    success "Direwolf $name started (PID: $pid, AGWPE: $agwpe_port)"
    echo "$pid"
}

crosslink_audio() {
    local pid_a="$1"
    local pid_b="$2"

    log "Cross-linking audio between Direwolf instances"

    # Mirrors e2e/helpers.py:pw_crosslink():
    #   1. Find each direwolf's tx (playback node, output_FL) and rx
    #      (capture node, input_MONO) ports via the Client->Node->Port graph.
    #   2. Disconnect any auto-links (default source->rx, playback->default
    #      sink). Without this real off-air traffic bleeds in and the
    #      return path never dominates.
    #   3. Link A_tx->B_rx and B_tx->A_rx.
    #   4. Set both capture nodes to 0.25 so the received audio level lands
    #      in the ~50 range Direwolf's AFSK demod expects.
    if ! python3 - "$pid_a" "$pid_b" << 'PYTHON'
import json, subprocess, sys, time

pid_a = int(sys.argv[1])
pid_b = int(sys.argv[2])

def dump():
    r = subprocess.run(["pw-dump"], capture_output=True, text=True, timeout=5)
    return json.loads(r.stdout)

def get_ports(data, pid):
    client_ids = {
        obj["id"] for obj in data
        if obj.get("type") == "PipeWire:Interface:Client"
        and obj.get("info", {}).get("props", {}).get("application.process.id") == pid
    }
    nodes = {
        obj["id"]: obj.get("info", {}).get("props", {}).get("node.name", "")
        for obj in data
        if obj.get("type") == "PipeWire:Interface:Node"
        and obj.get("info", {}).get("props", {}).get("client.id") in client_ids
    }
    tx_output = rx_input = None
    all_ports = []
    for obj in data:
        if obj.get("type") != "PipeWire:Interface:Port":
            continue
        props = obj.get("info", {}).get("props", {})
        node_id = props.get("node.id")
        if node_id not in nodes:
            continue
        all_ports.append(obj["id"])
        node_name = nodes[node_id]
        port_name = props.get("port.name", "")
        if "playback" in node_name and port_name == "output_FL":
            tx_output = obj["id"]
        if "capture" in node_name and port_name == "input_MONO":
            rx_input = obj["id"]
    return {"tx_output": tx_output, "rx_input": rx_input, "all": all_ports}

ports_a = ports_b = None
data = None
for attempt in range(30):
    data = dump()
    ports_a = get_ports(data, pid_a)
    ports_b = get_ports(data, pid_b)
    if (ports_a["tx_output"] and ports_a["rx_input"]
            and ports_b["tx_output"] and ports_b["rx_input"]):
        break
    print(
        f"Attempt {attempt+1}/30: waiting for direwolf PipeWire ports "
        f"(A tx/rx={ports_a['tx_output']}/{ports_a['rx_input']}, "
        f"B tx/rx={ports_b['tx_output']}/{ports_b['rx_input']})",
        file=sys.stderr,
    )
    time.sleep(1)
else:
    print(f"FAILED to find direwolf PipeWire ports. A={ports_a} B={ports_b}",
          file=sys.stderr)
    sys.exit(1)

# Disconnect any pre-existing links touching either direwolf's ports.
# These are the session-manager auto-links to the default source/sink.
port_set = set(ports_a["all"] + ports_b["all"])
for obj in data:
    if obj.get("type") != "PipeWire:Interface:Link":
        continue
    props = obj.get("info", {}).get("props", {})
    if (props.get("link.output.port") in port_set
            or props.get("link.input.port") in port_set):
        subprocess.run(
            ["pw-link", "-d", str(obj["id"])],
            capture_output=True, timeout=5,
        )

subprocess.run(
    ["pw-link", str(ports_a["tx_output"]), str(ports_b["rx_input"])],
    capture_output=True, timeout=5, check=True,
)
subprocess.run(
    ["pw-link", str(ports_b["tx_output"]), str(ports_a["rx_input"])],
    capture_output=True, timeout=5, check=True,
)

for pid in (pid_a, pid_b):
    client_ids = {
        obj["id"] for obj in data
        if obj.get("type") == "PipeWire:Interface:Client"
        and obj.get("info", {}).get("props", {}).get("application.process.id") == pid
    }
    for obj in data:
        if obj.get("type") != "PipeWire:Interface:Node":
            continue
        props = obj.get("info", {}).get("props", {})
        if (props.get("client.id") in client_ids
                and "capture" in props.get("node.name", "")):
            subprocess.run(
                ["pw-cli", "s", str(obj["id"]),
                 "Props", "{ channelVolumes: [0.25] }"],
                capture_output=True, timeout=5,
            )

print(f"A: tx={ports_a['tx_output']} rx={ports_a['rx_input']}")
print(f"B: tx={ports_b['tx_output']} rx={ports_b['rx_input']}")
PYTHON
    then
        error "Failed to cross-link Direwolf audio via PipeWire"
        error "Is PipeWire running? Check: systemctl --user status pipewire"
        exit 1
    fi

    success "Audio cross-linked (A tx -> B rx, B tx -> A rx, defaults disconnected)"
}

start_bridge() {
    local bridge_port="$1"
    local server_port="$2"
    local bridge_script="$SCRIPT_DIR/e2e/telnet_bridge.py"
    local log_file="$DEMO_DIR/bridge.log"

    if [[ ! -f "$bridge_script" ]]; then
        error "telnet_bridge.py not found at $bridge_script"
        exit 1
    fi

    log "Starting telnet bridge (LinBPQ CMDPORT $bridge_port -> server $server_port)"
    python3 "$bridge_script" 127.0.0.1 "$bridge_port" 127.0.0.1 "$server_port" \
        > "$log_file" 2>&1 &
    local pid=$!
    save_pid "$pid"

    local waited=0
    while ! python3 -c "import socket; s=socket.create_connection(('127.0.0.1', $bridge_port), timeout=0.5); s.close()" 2>/dev/null; do
        sleep 0.5
        waited=$((waited + 1))
        if [[ $waited -ge 20 ]]; then
            error "Telnet bridge not ready after 10s"
            cat "$log_file"
            exit 1
        fi
    done
    success "Telnet bridge started (PID: $pid)"
}

start_linbpq() {
    local work_dir="$DEMO_DIR/linbpq"
    local config_file="$work_dir/bpq32.cfg"
    local agwpe_port="$1"
    local bridge_port="$2"
    local telnet_port="$3"
    local fbb_port="$4"
    local http_port="$5"
    local log_file="$DEMO_DIR/linbpq.log"

    mkdir -p "$work_dir"

    # Clear the log file to avoid matching old entries
    > "$log_file"

    # Two ports:
    #   PORT 1 (UZ7HO) — the radio side, talking to Direwolf-B.
    #   PORT 2 (TELNET) — CMDPORT is where LinBPQ makes outgoing "C 2 ..."
    #     connections. The telnet bridge listens there and forwards to
    #     packet-browser-server, giving the WEB app somewhere to send data.
    # Matches e2e/conftest.py:72 write_linbpq_config().
    # Only emit an alias field when one is configured. LinBPQ treats the
    # trailing comma as "no alias" for this app slot.
    local app_alias_suffix=""
    if [[ -n "$WEB_APP_ALIAS" ]]; then
        app_alias_suffix=",$WEB_APP_ALIAS"
    fi

    cat > "$config_file" <<EOF
SIMPLE
NODECALL=$NODE_CALLSIGN
NODEALIAS=DEMO
LOCATOR=EN43bx
IDINTERVAL=0
BTINTERVAL=0

PORT
 PORTNUM=1
 ID=Radio Port
 DRIVER=UZ7HO
 CHANNEL=A
 PORTCALL=$NODE_CALLSIGN
 PERSIST=255
 SLOTTIME=100
 TXDELAY=100
 TXTAIL=50
 MAXFRAME=1
 FRACK=5000
 RESPTIME=100
 RETRIES=10
 PACLEN=128
 CONFIG
  ADDR 127.0.0.1 $agwpe_port
ENDPORT

PORT
 PORTNUM=2
 ID=Telnet Server
 DRIVER=TELNET
 CONFIG
  LOGGING=1
  LOCALNET=127.0.0.1/32
  HTTPPORT=$http_port
  TCPPORT=$telnet_port
  FBBPORT=$fbb_port
  CMDPORT=$bridge_port
  MAXSESSIONS=10
  CloseOnDisconnect=1
  USER=demo,demo,DEMOUSR,,SYSOP
ENDPORT

APPLICATION 1,$BPQ_APP_NAME,C 2 HOST 0 S$app_alias_suffix
EOF

    log "Starting LinBPQ in $work_dir"
    (cd "$work_dir" && linbpq >> "$log_file" 2>&1) &
    local pid=$!
    save_pid "$pid"

    # Wait for LinBPQ to start and connect to Direwolf
    local waited=0
    local max_wait=30
    while [[ $waited -lt $max_wait ]]; do
        sleep 1
        waited=$((waited + 1))
        
        if ! kill -0 "$pid" 2>/dev/null; then
            error "LinBPQ failed to start"
            cat "$log_file"
            exit 1
        fi
        
        # Check if connection failed
        if grep -q "Connect Failed for UZ7HO socket" "$log_file" 2>/dev/null; then
            warn "LinBPQ waiting for Direwolf-B to be ready (attempt $waited/$max_wait)..."
            # Kill and restart LinBPQ
            kill "$pid" 2>/dev/null
            wait "$pid" 2>/dev/null
            sleep 2
            # Clear log and restart
            > "$log_file"
            (cd "$work_dir" && linbpq >> "$log_file" 2>&1) &
            pid=$!
            # Update the PID in the pids file
            sed -i "/^${pid}$/d" "$DEMO_DIR/pids" 2>/dev/null || true
            save_pid "$pid"
            continue
        fi
        
        # Check if LinBPQ has initialized the port successfully
        # If we see "Initialising Port 01" but no "Connect Failed", we're good
        if grep -q "Initialising Port 01" "$log_file" 2>/dev/null && [[ $waited -ge 3 ]]; then
            success "LinBPQ started and connected to Direwolf (PID: $pid)"
            return 0
        fi
    done
    
    error "LinBPQ failed to connect to Direwolf-B after ${max_wait}s"
    error "Check $log_file for details"
    exit 1
}

start_server() {
    local port="$1"
    local portal_url="$2"

    log "Starting packet-browser server on port $port"

    export LISTEN_PORT="$port"
    export PORTAL_URL="$portal_url"
    export IDLE_TIMEOUT_MINUTES=10
    export BROTLI_QUALITY=11
    export BLOCKLIST_ENABLED=false
    export CHROMIUM_PATH=$(which chromium 2>/dev/null || echo "/usr/bin/chromium")
    # Off-air test callsigns. These wouldn't pass the ITU-shape regex the
    # server enforces on production ("DEMOUSR" has no digit in position 4),
    # so they're whitelisted here so LinBPQ's auto-injected identifier can
    # authenticate against the demo server. Real deployments should leave
    # ALLOWED_CALLSIGNS unset.
    export ALLOWED_CALLSIGNS="W1TEST,DEMOUSR,N0CALL"

    "$SCRIPT_DIR/target/debug/packet-browser-server" > "$DEMO_DIR/server.log" 2>&1 &
    local pid=$!
    save_pid "$pid"

    # Wait for server to be ready
    local waited=0
    while ! python3 -c "import socket; s=socket.create_connection(('127.0.0.1', $port), timeout=0.5); s.close()" 2>/dev/null; do
        sleep 0.5
        waited=$((waited + 1))
        if [[ $waited -ge 20 ]]; then
            error "Server not ready after 10s"
            cat "$DEMO_DIR/server.log"
            exit 1
        fi
    done

    success "Server started (PID: $pid, port: $port)"
}

start_client() {
    local agwpe_port="$1"
    local web_port="$2"
    local config_file="$DEMO_DIR/client.ini"

    # Configure BPQ command based on SKIP_BPQ_APP setting
    local bpq_cmd="$BPQ_APP_NAME"
    if [[ "$SKIP_BPQ_APP" == "true" ]]; then
        bpq_cmd=""
        log "Configuring client to connect directly to $TARGET_CALLSIGN (no BPQ app command)"
    else
        log "Configuring client to use BPQ application: $BPQ_APP_NAME"
    fi

    cat > "$config_file" <<EOF
[server]
agwpe_host = 127.0.0.1
agwpe_port = $agwpe_port

[session]
my_callsign = W1TEST
target_callsign = $TARGET_CALLSIGN
bpq_command = $bpq_cmd
skip_bpq_app = $SKIP_BPQ_APP
EOF

    log "Starting packet-browser client on port $web_port"

    "$SCRIPT_DIR/target/debug/packet-browser-client" \
        --config "$config_file" \
        --listen-addr "127.0.0.1:$web_port" \
        > "$DEMO_DIR/client.log" 2>&1 &
    local pid=$!
    save_pid "$pid"

    # Wait for client web UI to be ready
    local waited=0
    while ! python3 -c "import socket; s=socket.create_connection(('127.0.0.1', $web_port), timeout=0.5); s.close()" 2>/dev/null; do
        sleep 0.5
        waited=$((waited + 1))
        if [[ $waited -ge 20 ]]; then
            error "Client not ready after 10s"
            cat "$DEMO_DIR/client.log"
            exit 1
        fi
    done

    success "Client started (PID: $pid, web UI: http://127.0.0.1:$web_port)"
}

show_status() {
    echo ""
    echo "=========================================="
    echo "  Packet Browser Demo Mode Active"
    echo "=========================================="
    echo ""
    echo "Configuration:"
    echo "  Target callsign: $TARGET_CALLSIGN"
    if [[ "$SKIP_BPQ_APP" == "true" ]]; then
        echo "  BPQ app command: (none - direct connection)"
    else
        echo "  BPQ app command: $BPQ_APP_NAME"
    fi
    echo ""
    echo "Components:"
    echo "  Direwolf-A (W1TEST-1)  - Client-side TNC"
    echo "  Direwolf-B (N0CALL-2)  - Server-side TNC"
    if [[ -n "$WEB_APP_ALIAS" ]]; then
        echo "  LinBPQ ($NODE_CALLSIGN, $BPQ_APP_NAME→$WEB_APP_ALIAS) - BPQ node"
    else
        echo "  LinBPQ ($NODE_CALLSIGN, $BPQ_APP_NAME via node prompt) - BPQ node"
    fi
    echo "  packet-browser-server  - Web fetcher (TCP $SERVER_PORT)"
    echo "  packet-browser-client  - Web proxy (HTTP $WEB_PORT)"
    echo ""
    echo "  Open in browser: http://127.0.0.1:$WEB_PORT"
    echo ""
    echo "Diagnostic access to the LinBPQ node:"
    echo "  Telnet (login demo/demo):  nc 127.0.0.1 $BPQ_TELNET_PORT"
    echo "  FBB / BPQTermTCP:          127.0.0.1:$BPQ_FBB_PORT"
    echo "  Web management:            http://127.0.0.1:$BPQ_HTTP_PORT"
    echo ""
    echo "Log files: $DEMO_DIR/"
    echo "  direwolf-a.log  direwolf-b.log"
    echo "  linbpq.log  server.log  client.log"
    echo ""
    echo "Press Ctrl+C to stop"
    echo "=========================================="
    echo ""
}

main() {
    echo ""
    echo "=========================================="
    echo "  Packet Browser Demo Mode"
    echo "  Off-Air Testing Environment"
    echo "=========================================="
    echo ""

    # Display configuration
    log "Configuration:"
    log "  Target callsign: $TARGET_CALLSIGN"
    if [[ "$SKIP_BPQ_APP" == "true" ]]; then
        log "  BPQ app command: (none - direct connection)"
    else
        log "  BPQ app command: $BPQ_APP_NAME"
    fi
    echo ""

    # Aggressive cleanup of any existing Direwolf processes
    log "Cleaning up existing Direwolf processes..."
    pkill -9 direwolf 2>/dev/null || true
    pkill -9 linbpq 2>/dev/null || true
    pkill -9 packet-browser-server 2>/dev/null || true
    pkill -9 packet-browser-client 2>/dev/null || true
    
    # Wait for processes to die and ports to be released
    sleep 3
    
    # Double-check cleanup
    if pgrep -f direwolf >/dev/null 2>&1; then
        warn "Some Direwolf processes still running, force killing..."
        pkill -KILL direwolf 2>/dev/null || true
        sleep 2
    fi

    check_dependencies

    configure_pipewire

    AGWPE_PORT_A=$(find_free_port)
    AGWPE_PORT_B=$(find_free_port)
    SERVER_PORT=$(find_free_port)
    WEB_PORT=$(find_free_port)
    BRIDGE_PORT=$(find_free_port)
    BPQ_TELNET_PORT=$(find_free_port)
    BPQ_FBB_PORT=$(find_free_port)
    BPQ_HTTP_PORT=$(find_free_port)

    log "Using ports:"
    log "  Direwolf-A AGWPE: $AGWPE_PORT_A"
    log "  Direwolf-B AGWPE: $AGWPE_PORT_B"
    log "  Server TCP: $SERVER_PORT"
    log "  Client web UI: $WEB_PORT"
    log "  Telnet bridge:   $BRIDGE_PORT"
    log "  LinBPQ telnet:   $BPQ_TELNET_PORT (login demo/demo — 'nc 127.0.0.1 $BPQ_TELNET_PORT')"
    log "  LinBPQ FBB:      $BPQ_FBB_PORT (BPQTermTCP)"
    log "  LinBPQ HTTP:     $BPQ_HTTP_PORT (http://127.0.0.1:$BPQ_HTTP_PORT)"
    echo ""

    PID_A=$(start_direwolf "a" "W1TEST-1" "$AGWPE_PORT_A")
    
    # Small delay to avoid race conditions
    sleep 1
    
    PID_B=$(start_direwolf "b" "N0CALL-2" "$AGWPE_PORT_B")

    crosslink_audio "$PID_A" "$PID_B"

    start_server "$SERVER_PORT" "https://www.zeroretries.radio"
    start_bridge "$BRIDGE_PORT" "$SERVER_PORT"
    start_linbpq "$AGWPE_PORT_B" "$BRIDGE_PORT" "$BPQ_TELNET_PORT" "$BPQ_FBB_PORT" "$BPQ_HTTP_PORT"
    start_client "$AGWPE_PORT_A" "$WEB_PORT"

    show_status

    # Wait for Ctrl+C
    while true; do
        sleep 1
    done
}

main "$@"
