#!/usr/bin/env bash
set -euo pipefail

# Packet Browser Demo Mode
# Sets up a complete off-air test environment with virtual radio

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DEMO_DIR=$(mktemp -d)
trap 'cleanup' EXIT

# Configuration (can be overridden via environment variables)
BPQ_APP_NAME="${BPQ_APP_NAME:-WEB}"
TARGET_CALLSIGN="${TARGET_CALLSIGN:-N0CALL-7}"
SKIP_BPQ_APP="${SKIP_BPQ_APP:-false}"  # Set to "true" to skip sending BPQ app command

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

# Find PipeWire port IDs for a specific process PID.
# Uses the same approach as tncd e2e helpers:
#   1. Find Client objects by application.process.id == PID
#   2. Find Node objects by client.id in those client IDs
#   3. Find Port objects by node.id in those node IDs
#   4. Classify ports as playback (output) or capture (input) by port.name
# Returns numeric port IDs for use with pw-link
get_pw_ports_for_pid() {
    local target_pid=$1
    python3 - "$target_pid" << 'PYTHON'
import json, subprocess, sys

pid = int(sys.argv[1])
result = subprocess.run(['pw-dump'], capture_output=True, text=True, timeout=5)
data = json.loads(result.stdout)

# Step 1: Find Client objects for this PID
client_ids = set()
for obj in data:
    if obj.get('type') != 'PipeWire:Interface:Client':
        continue
    props = obj.get('info', {}).get('props', {})
    if props.get('application.process.id') == pid:
        client_ids.add(obj['id'])

if not client_ids:
    sys.exit(0)

# Step 2: Find Node objects belonging to those clients
node_ids = set()
for obj in data:
    if obj.get('type') != 'PipeWire:Interface:Node':
        continue
    props = obj.get('info', {}).get('props', {})
    if props.get('client.id') in client_ids:
        node_ids.add(obj['id'])

if not node_ids:
    sys.exit(0)

# Step 3: Find Port objects belonging to those nodes
# Classify as playback or capture based on port.name
playback_ports = []
capture_ports = []
for obj in data:
    if obj.get('type') != 'PipeWire:Interface:Port':
        continue
    props = obj.get('info', {}).get('props', {})
    if props.get('node.id') not in node_ids:
        continue
    port_name = props.get('port.name', '')
    port_id = obj['id']
    if 'output' in port_name.lower() or 'FL' in port_name or 'playback' in port_name.lower():
        playback_ports.append(port_id)
    elif 'input' in port_name.lower() or 'MONO' in port_name or 'capture' in port_name.lower():
        capture_ports.append(port_id)

# Output: PLAYBACK:id CAPTURE:id (use first of each)
if playback_ports:
    print(f'PLAYBACK:{playback_ports[0]}')
if capture_ports:
    print(f'CAPTURE:{capture_ports[0]}')
PYTHON
}

start_direwolf() {
    local name="$1"
    local callsign="$2"
    local agwpe_port="$3"
    local config_file="$DEMO_DIR/direwolf-$name.conf"
    local log_file="$DEMO_DIR/direwolf-$name.log"

    cat > "$config_file" <<EOF
ADEVICE default default
ACHANNELS 1
ARATE 44100
MYCALL $callsign
MODEM 1200
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

    local max_attempts=15
    local attempt=0
    local result_a=""
    local result_b=""

    while [[ $attempt -lt $max_attempts ]]; do
        result_a=$(get_pw_ports_for_pid "$pid_a")
        result_b=$(get_pw_ports_for_pid "$pid_b")

        local has_playback_a=$(echo "$result_a" | grep -c '^PLAYBACK:' || true)
        local has_capture_a=$(echo "$result_a" | grep -c '^CAPTURE:' || true)
        local has_playback_b=$(echo "$result_b" | grep -c '^PLAYBACK:' || true)
        local has_capture_b=$(echo "$result_b" | grep -c '^CAPTURE:' || true)

        if [[ $has_playback_a -gt 0 && $has_capture_a -gt 0 && \
              $has_playback_b -gt 0 && $has_capture_b -gt 0 ]]; then
            break
        fi

        attempt=$((attempt + 1))
        log "Attempt $attempt/$max_attempts: waiting for PipeWire ports (A: $has_playback_a/$has_capture_a, B: $has_playback_b/$has_capture_b)"
        sleep 2
    done

    if [[ -z "$result_a" || -z "$result_b" ]]; then
        error "Failed to find PipeWire ports for Direwolf instances"
        error "Is PipeWire running? Check: systemctl --user status pipewire"
        exit 1
    fi

    # Parse port IDs (format: TYPE:id)
    local pb_a_id=$(echo "$result_a" | grep '^PLAYBACK:' | cut -d: -f2)
    local cap_a_id=$(echo "$result_a" | grep '^CAPTURE:' | cut -d: -f2)
    local pb_b_id=$(echo "$result_b" | grep '^PLAYBACK:' | cut -d: -f2)
    local cap_b_id=$(echo "$result_b" | grep '^CAPTURE:' | cut -d: -f2)

    log "A: playback=$pb_a_id capture=$cap_a_id"
    log "B: playback=$pb_b_id capture=$cap_b_id"

    # Cross-link: A's output -> B's input, B's output -> A's input
    pw-link "$pb_a_id" "$cap_b_id" 2>/dev/null || { error "Failed to link A->B"; exit 1; }
    pw-link "$pb_b_id" "$cap_a_id" 2>/dev/null || { error "Failed to link B->A"; exit 1; }

    success "Audio cross-linked: A($pb_a_id)->B($cap_b_id), B($pb_b_id)->A($cap_a_id)"
}

start_linbpq() {
    local work_dir="$DEMO_DIR/linbpq"
    local config_file="$work_dir/bpq32.cfg"
    local agwpe_port="$1"
    local log_file="$DEMO_DIR/linbpq.log"

    mkdir -p "$work_dir"
    
    # Clear the log file to avoid matching old entries
    > "$log_file"

    cat > "$config_file" <<EOF
SIMPLE
NODECALL=N0CALL-7
NODEALIAS=DEMO
LOCATOR=EN43bx
IDINTERVAL=0
BTINTERVAL=0

PORT
 PORTNUM=1
 ID=Radio Port
 DRIVER=UZ7HO
 CHANNEL=A
 PORTCALL=N0CALL-7
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

APPLICATION 1,WEB,C 1 HOST 0 S
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
    echo "  LinBPQ (N0CALL-7)      - BPQ node"
    echo "  packet-browser-server  - Web fetcher (TCP $SERVER_PORT)"
    echo "  packet-browser-client  - Web proxy (HTTP $WEB_PORT)"
    echo ""
    echo "  Open in browser: http://127.0.0.1:$WEB_PORT"
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

    AGWPE_PORT_A=$(find_free_port)
    AGWPE_PORT_B=$(find_free_port)
    SERVER_PORT=$(find_free_port)
    WEB_PORT=$(find_free_port)

    log "Using ports:"
    log "  Direwolf-A AGWPE: $AGWPE_PORT_A"
    log "  Direwolf-B AGWPE: $AGWPE_PORT_B"
    log "  Server TCP: $SERVER_PORT"
    log "  Client web UI: $WEB_PORT"
    echo ""

    PID_A=$(start_direwolf "a" "W1TEST-1" "$AGWPE_PORT_A")
    
    # Small delay to avoid race conditions
    sleep 1
    
    PID_B=$(start_direwolf "b" "N0CALL-2" "$AGWPE_PORT_B")

    crosslink_audio "$PID_A" "$PID_B"

    start_server "$SERVER_PORT" "https://www.zeroretries.radio"
    start_linbpq "$AGWPE_PORT_B"
    start_client "$AGWPE_PORT_A" "$WEB_PORT"

    show_status

    # Wait for Ctrl+C
    while true; do
        sleep 1
    done
}

main "$@"
