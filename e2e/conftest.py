"""Shared fixtures for e2e tests."""

import os
import shutil
import subprocess
import time
from pathlib import Path

import pytest

from helpers import (
    free_port,
    kill_proc,
    pw_configure_for_test,
    pw_crosslink,
    pw_restore_settings,
    wait_for_port,
)


pytestmark = [
    pytest.mark.skipif(
        not shutil.which("direwolf"), reason="direwolf not installed"
    ),
    pytest.mark.skipif(
        not shutil.which("pw-link"), reason="pipewire not available"
    ),
]

needs_linbpq = pytest.mark.skipif(
    not shutil.which("linbpq"), reason="linbpq not installed"
)

def _have_browser():
    ff = os.environ.get("FIREFOX_PATH") or shutil.which("firefox") or os.path.exists("/bin/firefox")
    gd = os.environ.get("GECKODRIVER_PATH") or shutil.which("geckodriver") or os.path.exists("/bin/geckodriver")
    return bool(ff and gd)


needs_browser = pytest.mark.skipif(
    not _have_browser(),
    reason="firefox and geckodriver required"
)
# Alias kept so the existing @needs_chromium decorators in test files still
# resolve; import path unchanged.
needs_chromium = needs_browser


def write_direwolf_config(path, mycall, agwport=0):
    """Write a Direwolf configuration file."""
    lines = [
        "ADEVICE default",
        "ACHANNELS 1",
        "ARATE 44100",
        f"MYCALL {mycall}",
        "MODEM 1200",
        "AGWPORT 0",
    ]
    if agwport:
        lines.append(f"AGWPORT {agwport}")
    lines.extend([
        "KISSPORT 0",
        "FULLDUP ON",
        "TXDELAY 10",
        "TXTAIL 5",
        "SLOTTIME 10",
        "PERSIST 63",
    ])
    Path(path).write_text("\n".join(lines) + "\n")


def write_linbpq_config(path, direwolf_agwpe_port, bridge_port, nodecall="N0CALL-7"):
    """Write a LinBPQ configuration file for testing."""
    config = f"""SIMPLE
NODECALL={nodecall}
NODEALIAS=TEST
LOCATOR=EN43bx
IDINTERVAL=0
BTINTERVAL=0

PORT
 PORTNUM=1
 ID=Radio Port
 DRIVER=UZ7HO
 CHANNEL=A
 PORTCALL={nodecall}
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
  ADDR 127.0.0.1 {direwolf_agwpe_port}
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
  CMDPORT={bridge_port}
  MAXSESSIONS=2
  CloseOnDisconnect=1
ENDPORT

APPLICATION 1,WEB,C 2 HOST 0 S

"""
    Path(path).write_text(config)


@pytest.fixture()
def direwolf_pair(tmp_path):
    """Start a pair of Direwolf instances with audio cross-linked via PipeWire.

    Yields a dict with:
      - agwpe_port_a: AGWPE port for Direwolf-A
      - agwpe_port_b: AGWPE port for Direwolf-B
      - proc_a: Direwolf-A subprocess
      - proc_b: Direwolf-B subprocess
    """
    pw_original = pw_configure_for_test()

    agwpe_port_a = free_port()
    agwpe_port_b = free_port()

    conf_a = tmp_path / "direwolf-a.conf"
    conf_b = tmp_path / "direwolf-b.conf"

    write_direwolf_config(conf_a, "N0CALL-1", agwport=agwpe_port_a)
    write_direwolf_config(conf_b, "N0CALL-2", agwport=agwpe_port_b)

    log_a = open(tmp_path / "direwolf-a.log", "w+b")
    log_b = open(tmp_path / "direwolf-b.log", "w+b")

    proc_a = subprocess.Popen(
        ["direwolf", "-c", str(conf_a), "-t", "0"],
        stdout=log_a,
        stderr=subprocess.STDOUT,
    )

    proc_b = subprocess.Popen(
        ["direwolf", "-c", str(conf_b), "-t", "0"],
        stdout=log_b,
        stderr=subprocess.STDOUT,
    )

    sink_ids = []
    try:
        wait_for_port(agwpe_port_a)
        wait_for_port(agwpe_port_b)

        time.sleep(1.0)

        sink_ids = pw_crosslink(proc_a.pid, proc_b.pid)

        yield {
            "agwpe_port_a": agwpe_port_a,
            "agwpe_port_b": agwpe_port_b,
            "proc_a": proc_a,
            "proc_b": proc_b,
            "log_a_path": tmp_path / "direwolf-a.log",
            "log_b_path": tmp_path / "direwolf-b.log",
        }
    finally:
        kill_proc(proc_a)
        kill_proc(proc_b)
        for lb in sink_ids:
            kill_proc(lb)
        pw_restore_settings(pw_original)
        log_a.close()
        log_b.close()


@pytest.fixture()
def test_http_server(tmp_path):
    """Start a simple HTTP server serving test pages.

    Yields a dict with:
      - url: Base URL of the server
      - port: Port number
      - proc: Server subprocess
    """
    port = free_port()

    html_dir = tmp_path / "html"
    html_dir.mkdir()

    (html_dir / "index.html").write_text("""<!DOCTYPE html>
<html>
<head><title>Test Portal</title></head>
<body>
  <h1>Packet Browser Test Portal</h1>
  <p>Welcome to the test portal.</p>
  <ul>
    <li><a href="/page2.html">Page 2</a></li>
    <li><a href="/form.html">Search Form</a></li>
  </ul>
</body>
</html>
""")

    (html_dir / "page2.html").write_text("""<!DOCTYPE html>
<html>
<head><title>Page 2</title></head>
<body>
  <h1>Page 2</h1>
  <p>This is the second test page.</p>
  <a href="/">Back to portal</a>
</body>
</html>
""")

    (html_dir / "form.html").write_text("""<!DOCTYPE html>
<html>
<head><title>Search</title></head>
<body>
  <h1>Search</h1>
  <form action="/result.html" method="GET">
    <input type="text" name="q" placeholder="Search...">
    <button type="submit">Search</button>
  </form>
</body>
</html>
""")

    (html_dir / "result.html").write_text("""<!DOCTYPE html>
<html>
<head><title>Search Results</title></head>
<body>
  <h1>Search Results</h1>
  <p>You searched for: <span id="query"></span></p>
  <a href="/">Back to portal</a>
</body>
</html>
""")

    proc = subprocess.Popen(
        ["python3", "-m", "http.server", str(port)],
        cwd=str(html_dir),
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
    )

    try:
        wait_for_port(port)
        yield {
            "url": f"http://127.0.0.1:{port}",
            "port": port,
            "proc": proc,
        }
    finally:
        kill_proc(proc)


@pytest.fixture()
@needs_linbpq
def linbpq_instance(tmp_path, direwolf_pair, pb_server):
    """Start LinBPQ with test configuration.

    Yields a dict with:
      - proc: LinBPQ subprocess
      - config_path: Path to config file
      - work_dir: Working directory
      - bridge_proc: Telnet bridge subprocess
    """
    work_dir = tmp_path / "linbpq"
    work_dir.mkdir()

    config_path = work_dir / "bpq32.cfg"

    # Start telnet bridge
    bridge_port = free_port()
    bridge_script = Path(__file__).parent / "telnet_bridge.py"
    bridge_log = tmp_path / "bridge.log"
    bridge_proc = subprocess.Popen(
        ["python3", str(bridge_script), "127.0.0.1", str(bridge_port), "127.0.0.1", str(pb_server["port"])],
        stdout=open(bridge_log, "w"),
        stderr=subprocess.STDOUT,
    )

    # Wait for bridge to start
    time.sleep(0.5)

    write_linbpq_config(
        config_path,
        direwolf_pair["agwpe_port_b"],
        bridge_port,
        nodecall="N0CALL-7",
    )

    proc = subprocess.Popen(
        ["linbpq"],
        cwd=str(work_dir),
        stdout=open(work_dir / "linbpq.log", "w"),
        stderr=subprocess.STDOUT,
    )

    try:
        time.sleep(2.0)
        yield {
            "proc": proc,
            "config_path": config_path,
            "work_dir": work_dir,
            "bridge_proc": bridge_proc,
            "bridge_log": bridge_log,
            "linbpq_log": work_dir / "linbpq.log",
        }
    finally:
        kill_proc(proc)
        kill_proc(bridge_proc)


@pytest.fixture()
@needs_chromium
def pb_server(tmp_path, test_http_server):
    """Start packet-browser-server on port 63004.

    Yields a dict with:
      - port: Server port
      - proc: Server subprocess
    """
    port = 63004

    env = os.environ.copy()
    env["LISTEN_PORT"] = str(port)
    env["PORTAL_URL"] = test_http_server["url"]
    env["IDLE_TIMEOUT_MINUTES"] = "10"
    env["BROTLI_QUALITY"] = "11"
    env["BLOCKLIST_ENABLED"] = "false"

    # Firefox + geckodriver: match what the container image sets.
    if "FIREFOX_PATH" not in env:
        ff = shutil.which("firefox")
        if ff:
            env["FIREFOX_PATH"] = ff
    if "GECKODRIVER_PATH" not in env:
        gd = shutil.which("geckodriver")
        if gd:
            env["GECKODRIVER_PATH"] = gd

    log_dir = tmp_path / "logs"
    log_dir.mkdir()
    env["LOG_DIR"] = str(log_dir)

    binary = shutil.which("packet-browser-server")
    if not binary:
        # Use absolute path relative to project root
        project_root = Path(__file__).parent.parent
        binary = str(project_root / "target" / "debug" / "packet-browser-server")

    proc = subprocess.Popen(
        [binary],
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
    )

    try:
        wait_for_port(port)
        yield {
            "port": port,
            "proc": proc,
        }
    finally:
        kill_proc(proc)


@pytest.fixture()
def pb_client(direwolf_pair, tmp_path):
    """Start packet-browser-client connecting to Direwolf-A.

    Yields a dict with:
      - web_port: Web proxy port
      - proc: Client subprocess
      - config_path: Path to config file
    """
    web_port = free_port()

    config_dir = tmp_path / "client_config"
    config_dir.mkdir()
    config_path = config_dir / "config.ini"

    config_content = f"""[server]
agwpe_host = 127.0.0.1
agwpe_port = {direwolf_pair["agwpe_port_a"]}

[session]
my_callsign = W1TEST
target_callsign = N0CALL-7
bpq_command = WEB
"""
    config_path.write_text(config_content)

    binary = shutil.which("packet-browser-client")
    if not binary:
        # Use absolute path relative to project root
        project_root = Path(__file__).parent.parent
        binary = str(project_root / "target" / "debug" / "packet-browser-client")

    proc = subprocess.Popen(
        [binary, "--config", str(config_path), "--listen-addr", f"127.0.0.1:{web_port}"],
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
    )

    try:
        wait_for_port(web_port)
        yield {
            "web_port": web_port,
            "proc": proc,
            "config_path": config_path,
        }
    finally:
        kill_proc(proc)
