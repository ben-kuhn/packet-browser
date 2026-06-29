#!/usr/bin/env bash
set -euo pipefail

# Packet Browser Demo Mode
# Sets up a complete off-air test environment with virtual radio

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DEMO_DIR=$(mktemp -d)
trap 'cleanup' EXIT

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

log() {
    echo -e "${BLUE}[DEMO]${NC} $1"
}

success() {
    echo -e "${GREEN}[✓]${NC} $1"
}

warn() {
    echo -e "${YELLOW}[!]${NC} $1"
}

error() {
    echo -e "${RED}[✗]${NC} $1"
}

cleanup() {
    log "Cleaning up..."
    
    # Kill all demo processes
    if [[ -f "$DEMO_DIR/pids" ]]; then
        while read -r pid; do
            if kill -0 "$pid" 2>/dev/null; then
                kill "$pid" 2>/dev/null || true
                wait "$pid" 2>/dev/null || true
            fi
        done < "$DEMO_DIR/pids"
    fi
    
    # Remove demo directory
    rm -rf "$DEMO_DIR"
    
    success "Cleanup complete"
}

save_pid() {
    echo "$1" >> "$DEMO_DIR/pids"
}

check_dependencies() {
    log "Checking dependencies..."
    
    local missing=()
    
    # Check for required commands
    for cmd in direwolf linbpq pw-link pw-dump python3; do
        if ! command -v "$cmd" &> /dev/null; then
            missing+=("$cmd")
        fi
    done
    
    # Check for packet-browser binaries
    if [[ ! -f "$SCRIPT_DIR/target/debug/packet-browser-server" ]]; then
        missing+=("packet-browser-server")
    fi
    
    if [[ ! -f "$SCRIPT_DIR/target/debug/packet-browser-client" ]]; then
        missing+=("packet-browser-client")
    fi
    
    if [[ ${#missing[@]} -gt 0 ]]; then
        error "Missing dependencies: ${missing[*]}"
        error "Please ensure all dependencies are installed"
        exit 1
    fi
    
    success "All dependencies found"
}

find_free_port() {
    python3 -c 'import socket; s=socket.socket(); s.bind(("", 0)); print(s.getsockname()[1]); s.close()'
}

get_pw_ports() {
    local pid=$1
    local script_file="$DEMO_DIR/get_ports.py"
    
    cat > "$script_file" << 'PYTHON'
import json
import subprocess
import sys

try:
    result = subprocess.run(['pw-dump'], capture_output=True, text=True, timeout=5)
    data = json.loads(result.stdout)
    
    # Direwolf creates nodes with names like "alsa_capture.direwolf" and "alsa_playback.direwolf"
    # We need to find ports belonging to these nodes
    ports = []
    for obj in data:
        if obj.get('type') != 'PipeWire:Interface:Port':
            continue
        props = obj.get('info', {}).get('props', {})
        node_id = props.get('node.id')
        
        # Find the parent node
        for node_obj in data:
            if node_obj.get('type') == 'PipeWire:Interface:Node' and node_obj.get('id') == node_id:
                node_props = node_obj.get('info', {}).get('props', {})
                node_name = node_props.get('node.name', '')
                # Match Direwolf nodes
                if 'direwolf' in node_name.lower():
                    ports.append(obj['id'])
                break
    
    # Return unique ports
    for port in sorted(set(ports)):
        print(port)
except Exception as e:
    pass
PYTHON
    
    python3 "$script_file" "$pid"
}

start_direwolf() {
    local name="$1"
    local callsign="$2"
    local agwpe_port="$3"
    local config_file="$DEMO_DIR/direwolf-$name.conf"
    local log_file="$DEMO_DIR/direwolf-$name.log"
    
    cat > "$config_file" <<EOF
# Use PipeWire for audio
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
    sleep 3
    
    if ! kill -0 "$pid" 2>/dev/null; then
        error "Direwolf $name failed to start"
        cat "$log_file"
        exit 1
    fi
    
    success "Direwolf $name started (PID: $pid)"
    echo "$pid"
}

crosslink_audio() {
    local pid_a="$1"
    local pid_b="$2"
    
    log "Cross-linking audio between Direwolf instances"
    
    # Wait for PipeWire to register the ports
    log "Waiting for PipeWire ports to register..."
    sleep 5
    
    # Get PipeWire port IDs with retry
    local max_attempts=10
    local attempt=0
    local ports_a=""
    local ports_b=""
    
    while [[ $attempt -lt $max_attempts ]]; do
        ports_a=$(get_pw_ports "$pid_a")
        ports_b=$(get_pw_ports "$pid_b")
        
        # We expect at least 2 ports per instance (input and output)
        local count_a=$(echo "$ports_a" | wc -l)
        local count_b=$(echo "$ports_b" | wc -l)
        
        if [[ $count_a -ge 2 && $count_b -ge 2 ]]; then
            break
        fi
        
        attempt=$((attempt + 1))
        log "Attempt $attempt/$max_attempts: Found $count_a ports for A, $count_b ports for B"
        sleep 2
    done
    
    if [[ -z "$ports_a" || -z "$ports_b" ]]; then
        warn "Failed to find PipeWire ports after $max_attempts attempts"
        warn "Audio cross-linking skipped - demo may not work correctly"
        warn "Check that PipeWire is running: systemctl --user status pipewire"
        return 0
    fi
    
    log "Found ports for A: $(echo $ports_a | tr '\n' ' ')"
    log "Found ports for B: $(echo $ports_b | tr '\n' ' ')"
    
    # For each Direwolf instance, we have capture and playback ports
    # We need to link:
    #   A's playback -> B's capture
    #   B's playback -> A's capture
    
    # Simple approach: use first two ports from each
    local ports_a_array=($ports_a)
    local ports_b_array=($ports_b)
    
    if [[ ${#ports_a_array[@]} -lt 2 || ${#ports_b_array[@]} -lt 2 ]]; then
        warn "Not enough ports found for cross-linking"
        return 0
    fi
    
    # Link A's first port to B's second port, and vice versa
    # (This is a heuristic - in production you'd parse port names to be sure)
    pw-link "${ports_a_array[0]}" "${ports_b_array[1]}" 2>/dev/null || warn "Failed to link A[0]->B[1]"
    pw-link "${ports_b_array[0]}" "${ports_a_array[1]}" 2>/dev/null || warn "Failed to link B[0]->A[1]"
    
    success "Audio cross-linked"
}

start_linbpq() {
    local work_dir="$DEMO_DIR/linbpq"
    local config_file="$work_dir/bpq32.cfg"
    local agwpe_port="$1"
    local server_port="$2"
    
    mkdir -p "$work_dir"
    
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

PORT
 PORTNUM=2
 ID=Telnet Server
 DRIVER=TELNET
 CONFIG
  LOGGING=1
  LOCALNET=127.0.0.1/32
  HTTPPORT=0
  TCPPORT=0
  FBBPORT=0
  CMDPORT=0
  MAXSESSIONS=2
  CloseOnDisconnect=1
ENDPORT

APPLICATION 1,WEB,C 2 HOST 0 S
EOF
    
    log "Starting LinBPQ"
    cd "$work_dir"
    linbpq > "$DEMO_DIR/linbpq.log" 2>&1 &
    local pid=$!
    save_pid "$pid"
    cd "$SCRIPT_DIR"
    
    sleep 3
    
    if ! kill -0 "$pid" 2>/dev/null; then
        error "LinBPQ failed to start"
        cat "$DEMO_DIR/linbpq.log"
        exit 1
    fi
    
    success "LinBPQ started (PID: $pid)"
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
    
    sleep 2
    
    if ! kill -0 "$pid" 2>/dev/null; then
        error "Server failed to start"
        cat "$DEMO_DIR/server.log"
        exit 1
    fi
    
    success "Server started (PID: $pid)"
}

start_client() {
    local agwpe_port="$1"
    local web_port="$2"
    local config_file="$DEMO_DIR/client.ini"
    
    cat > "$config_file" <<EOF
[server]
agwpe_host = 127.0.0.1
agwpe_port = $agwpe_port

[session]
my_callsign = W1TEST
target_callsign = N0CALL-7
bpq_command = WEB
EOF
    
    log "Starting packet-browser client on port $web_port"
    
    "$SCRIPT_DIR/target/debug/packet-browser-client" \
        --config "$config_file" \
        --listen-addr "127.0.0.1:$web_port" \
        > "$DEMO_DIR/client.log" 2>&1 &
    local pid=$!
    save_pid "$pid"
    
    sleep 2
    
    if ! kill -0 "$pid" 2>/dev/null; then
        error "Client failed to start"
        cat "$DEMO_DIR/client.log"
        exit 1
    fi
    
    success "Client started (PID: $pid)"
}

show_status() {
    echo ""
    echo "=========================================="
    echo "  Packet Browser Demo Mode Active"
    echo "=========================================="
    echo ""
    echo "Components:"
    echo "  • Direwolf-A (W1TEST-1) - Client side"
    echo "  • Direwolf-B (N0CALL-2) - Server side"
    echo "  • LinBPQ (N0CALL-7) - BPQ node"
    echo "  • packet-browser-server - Web fetcher"
    echo "  • packet-browser-client - Web proxy"
    echo ""
    echo "Access URLs:"
    echo "  • Client web UI: http://127.0.0.1:$WEB_PORT"
    echo "  • Server status: http://127.0.0.1:$SERVER_PORT"
    echo ""
    echo "Log files in: $DEMO_DIR"
    echo "  • direwolf-a.log"
    echo "  • direwolf-b.log"
    echo "  • linbpq.log"
    echo "  • server.log"
    echo "  • client.log"
    echo ""
    echo "Press Ctrl+C to stop demo mode"
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
    
    # Clean up any existing Direwolf processes
    log "Cleaning up existing Direwolf processes..."
    pkill -9 direwolf 2>/dev/null || true
    sleep 1
    
    check_dependencies
    
    # Find free ports
    AGWPE_PORT_A=$(find_free_port)
    AGWPE_PORT_B=$(find_free_port)
    SERVER_PORT=$(find_free_port)
    WEB_PORT=$(find_free_port)
    
    log "Using ports:"
    log "  Direwolf-A AGWPE: $AGWPE_PORT_A"
    log "  Direwolf-B AGWPE: $AGWPE_PORT_B"
    log "  Server: $SERVER_PORT"
    log "  Client web UI: $WEB_PORT"
    echo ""
    
    # Start components
    PID_A=$(start_direwolf "a" "W1TEST-1" "$AGWPE_PORT_A")
    PID_B=$(start_direwolf "b" "N0CALL-2" "$AGWPE_PORT_B")
    
    crosslink_audio "$PID_A" "$PID_B"
    
    start_linbpq "$AGWPE_PORT_B" "$SERVER_PORT"
    start_server "$SERVER_PORT" "https://www.zeroretries.radio"
    start_client "$AGWPE_PORT_A" "$WEB_PORT"
    
    show_status
    
    # Wait for Ctrl+C
    while true; do
        sleep 1
    done
}

main "$@"
