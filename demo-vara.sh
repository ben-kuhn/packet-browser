#!/usr/bin/env bash
# demo-vara.sh — end-to-end VARA/Mercury manual demo scaffold
#
# The AX.25 demo (demo.sh) can auto-wire Direwolf on both ends because
# Direwolf is open-source and works with looped audio. VARA is
# proprietary and license-gated. Mercury is open-source and speaks the
# VARA-HF API; it's the only in-repo-runnable end-to-end option.
#
# This script does not launch modems for you — it documents the
# expected topology and starts packet-browser-server + packet-browser-client
# configured to point at the modem endpoints below. Run Mercury (or
# your VARA installation) yourself on each side before invoking.

set -euo pipefail

: "${MERCURY_CMD_PORT_CLIENT:=3000}"
: "${MERCURY_DATA_PORT_CLIENT:=3001}"
: "${LINBPQ_HTTP_PORT:=8082}"

cat <<EOF
[demo-vara] Expected topology:

  packet-browser-client
      │
      ├── Mercury (cmd :$MERCURY_CMD_PORT_CLIENT / data :$MERCURY_DATA_PORT_CLIENT)
      │        │
      │        ▼ RF (or loopback audio)
      │
      ├── Mercury on the other end
      │        │
      │        ▼
      ├── LinBPQ (VARA port configured)
      │        │
      │        ▼
      └── packet-browser-server (unchanged)

Before running this demo:
  - Start Mercury on both ends
  - Configure LinBPQ on the server side with a VARA port pointing at
    Mercury's cmd/data ports
  - Open http://127.0.0.1:<client-port>/connect and pick "VARA HF"
    with the Mercury ports above.

This script is a placeholder that intentionally does not spawn Mercury.
When run, it prints this help and exits.
EOF

exit 0
