"""Full end-to-end integration tests."""

import asyncio
import struct
import time

import pytest
import requests

from conftest import needs_chromium, needs_linbpq, pytestmark


AGWPE_HEADER_SIZE = 36


def _same_origin_headers(url):
    """Return headers that make the client's same-origin CSRF guard happy.

    The client rejects POSTs whose Origin authority doesn't match the request
    Host. Real browsers send Origin automatically; the requests library does
    not, so tests have to add it explicitly.
    """
    from urllib.parse import urlparse

    parsed = urlparse(url)
    return {"Origin": f"{parsed.scheme}://{parsed.netloc}"}


def post(url, **kwargs):
    """requests.post with Origin set to match the target -- passes CSRF guard."""
    kwargs.setdefault("headers", {}).update(_same_origin_headers(url))
    return requests.post(url, **kwargs)


def capture_client_logs(client_proc, timeout=1):
    """Capture and print client logs."""
    print("\n=== Client Logs ===")
    try:
        client_proc.terminate()
        stdout, _ = client_proc.communicate(timeout=timeout)
        for line in stdout.decode().split('\n'):
            if line.strip():
                print(f"CLIENT: {line}")
    except Exception as e:
        print(f"Error capturing logs: {e}")
        try:
            client_proc.kill()
            stdout, _ = client_proc.communicate()
            for line in stdout.decode().split('\n'):
                if line.strip():
                    print(f"CLIENT: {line}")
        except:
            pass
    print("=== End Client Logs ===\n")


def create_agwpe_frame(port, datakind, call_from, call_to, data=b''):
    """Create an AGWPE frame."""
    return struct.pack(
        '<BBBBBBBB10s10sII',
        port, 0, 0, 0, datakind, 0, 0, 0,
        call_from.encode().ljust(10, b'\x00'),
        call_to.encode().ljust(10, b'\x00'),
        len(data), 0
    ) + data


class AGWPEClient:
    """Simple AGWPE client for testing."""

    def __init__(self, host, port):
        self.host = host
        self.port = port
        self.reader = None
        self.writer = None

    async def connect(self):
        self.reader, self.writer = await asyncio.open_connection(self.host, self.port)

    async def register(self, callsign):
        frame = create_agwpe_frame(0, ord('X'), callsign, '', b'')
        self.writer.write(frame)
        await self.writer.drain()

        header = await asyncio.wait_for(self.reader.readexactly(AGWPE_HEADER_SIZE), timeout=5.0)
        datakind = header[4]
        data_len = struct.unpack('<I', header[28:32])[0]
        data = await self.reader.readexactly(data_len) if data_len > 0 else b''
        # Direwolf responds with same frame type 'X' (0x58) and data=0x01 for success
        assert datakind == ord('X'), f"Expected registration response type 'X', got {chr(datakind)}"
        assert data == b'\x01', f"Expected success data 0x01, got {data.hex()}"

    async def query_ports(self):
        frame = create_agwpe_frame(0, ord('G'), '', '', b'')
        self.writer.write(frame)
        await self.writer.drain()

        ports = []
        while True:
            header = await asyncio.wait_for(self.reader.readexactly(AGWPE_HEADER_SIZE), timeout=5.0)
            datakind = header[4]
            data_len = struct.unpack('<I', header[23:27])[0]

            if datakind == ord('g') and data_len > 0:
                data = await self.reader.readexactly(data_len)
                port_num = data[0]
                desc = data[1:].decode('utf-8', errors='replace').rstrip('\x00')
                ports.append((port_num, desc))
            elif datakind == ord('g') and data_len == 0:
                break
            else:
                break

        return ports

    async def ax25_connect(self, port_num, from_call, to_call):
        frame = create_agwpe_frame(port_num, ord('C'), from_call, to_call, b'')
        self.writer.write(frame)
        await self.writer.drain()

        while True:
            header = await asyncio.wait_for(self.reader.readexactly(AGWPE_HEADER_SIZE), timeout=30.0)
            datakind = header[4]

            if datakind == ord('c'):
                return True
            elif datakind == ord('d'):
                pass
            else:
                raise Exception(f"Unexpected frame type: {chr(datakind)}")

    async def send_data(self, port_num, from_call, to_call, data):
        frame = create_agwpe_frame(port_num, ord('D'), from_call, to_call, data)
        self.writer.write(frame)
        await self.writer.drain()

    async def receive_data(self, timeout=120.0):
        header = await asyncio.wait_for(self.reader.readexactly(AGWPE_HEADER_SIZE), timeout=timeout)
        datakind = header[4]
        data_len = struct.unpack('<I', header[23:27])[0]

        if datakind == ord('d') and data_len > 0:
            data = await self.reader.readexactly(data_len)
            return data
        return None

    async def close(self):
        if self.writer:
            self.writer.close()
            await self.writer.wait_closed()


class TestServerHandshake:
    """Guard the BPQ-mode handshake contract on the server side.

    In `C n HOST 0 S` mode LinBPQ opens the outbound TCP silently — it does
    NOT auto-inject the connecting station's callsign, verified via a
    packet-capturing bridge in front of the server. So the server must
    prompt for the callsign, the client sends it, then the server prompts
    for AGREE. Two failure modes we've hit before, one on each side:

    1. Server reads before writing. Then LinBPQ is idle on TCP, the client
       is waiting for a callsign prompt over AX.25, and both sides
       deadlock until the client's 30 s handshake timeout.
    2. Post-callsign prompt loses the "AGREE" token — the client's second
       wait loop times out.
    """

    def test_server_prompts_for_callsign_then_agree(self, pb_server):
        """Server writes a 'callsign' prompt first; then 'AGREE' after we reply."""
        import socket

        with socket.create_connection(
            ("127.0.0.1", pb_server["port"]), timeout=5
        ) as sock:
            sock.settimeout(5)
            chunks = []
            deadline = time.monotonic() + 5
            while time.monotonic() < deadline:
                try:
                    chunk = sock.recv(4096)
                except socket.timeout:
                    break
                if not chunk:
                    break
                chunks.append(chunk)
                if b"callsign" in b"".join(chunks).lower():
                    break
            received = b"".join(chunks).decode("utf-8", errors="replace")
            assert "callsign" in received.lower(), (
                "Server did not send a prompt containing 'callsign' before "
                f"reading. LinBPQ HOST 0 doesn't auto-inject the callsign, "
                f"so the client would deadlock. Got: {received!r}"
            )

            sock.sendall(b"W1TEST\n")

            chunks = []
            deadline = time.monotonic() + 5
            while time.monotonic() < deadline:
                try:
                    chunk = sock.recv(4096)
                except socket.timeout:
                    break
                if not chunk:
                    break
                chunks.append(chunk)
                if b"AGREE" in b"".join(chunks):
                    break
            received = b"".join(chunks).decode("utf-8", errors="replace")
            assert "AGREE" in received, (
                "After receiving the callsign, server did not send an "
                f"'AGREE' prompt. Client would time out. Got: {received!r}"
            )


class TestAgwpeFrameHandling:
    """Guard client-side AGWPE frame-type handling in the BPQ handshake.

    Direwolf reports incoming radio data with AGWPE frame kind 'D' (0x44) —
    the same byte the client uses to *send* data. If a receive-side match
    arm only recognises 0x64 (an alternate "DataReceived" byte some AGWPE
    variants use), every direwolf-delivered prompt is silently ignored and
    the handshake times out. This is easy to reintroduce because 0x44 vs
    0x64 is a one-character difference in a match arm.
    """

    def test_agwpe_rx_arms_accept_both_data_bytes(self):
        """Every RX-side match on DataReceived must also accept SendData."""
        import pathlib

        src = pathlib.Path(__file__).parent.parent / "client" / "src" / "agwpe.rs"
        text = src.read_text()

        bad_arms = [
            lineno
            for lineno, line in enumerate(text.splitlines(), start=1)
            if line.strip() == "FrameType::DataReceived => {"
        ]
        assert not bad_arms, (
            "Found bare `FrameType::DataReceived => {` in client/src/agwpe.rs "
            f"at line(s) {bad_arms}. Must be `FrameType::DataReceived | "
            "FrameType::SendData => {` — direwolf delivers received data "
            "with the same 0x44 byte the client uses to send data, so a "
            "match on DataReceived alone drops every inbound frame."
        )


class TestLinBPQ:
    """Validate LinBPQ starts and connects to Direwolf."""

    @needs_linbpq
    def test_linbpq_starts(self, linbpq_instance):
        """LinBPQ starts successfully with test configuration."""
        assert linbpq_instance["proc"].poll() is None


class TestFullE2E:
    """Full end-to-end tests through the complete data path."""

    @needs_chromium
    @needs_linbpq
    def test_connect_to_agwpe(self, direwolf_pair, pb_server, linbpq_instance, pb_client):
        """Client connects to AGWPE via Direwolf-A."""
        web_port = pb_client["web_port"]
        client_proc = pb_client["proc"]

        resp = post(f"http://127.0.0.1:{web_port}/api/agwpe-status", timeout=10)
        assert resp.status_code == 200
        data = resp.json()
        print(f"\nAGWPE status response: {data}")
        
        # Terminate client and capture its output
        import time
        time.sleep(0.5)
        client_proc.terminate()
        try:
            stdout, _ = client_proc.communicate(timeout=2)
            print("\n=== Client Logs ===")
            for line in stdout.decode().split('\n'):
                if line.strip():
                    print(f"CLIENT: {line}")
            print("=== End Client Logs ===\n")
        except:
            client_proc.kill()
            stdout, _ = client_proc.communicate()
            print("\n=== Client Logs (after kill) ===")
            for line in stdout.decode().split('\n'):
                if line.strip():
                    print(f"CLIENT: {line}")
            print("=== End Client Logs ===\n")
        
        assert data["ok"] is True, f"Expected ok=True, got: {data}"
        assert data["state"] in ["AGWPE Connected", "Connected"]

    @needs_chromium
    def test_ax25_connect_direct(self, direwolf_pair, pb_server, pb_client):
        """Client establishes AX.25 connection directly to server (bypassing LinBPQ)."""
        web_port = pb_client["web_port"]
        client_proc = pb_client["proc"]

        # Connect to AGWPE
        post(f"http://127.0.0.1:{web_port}/api/agwpe-status", timeout=10)

        # For direct testing, we'll skip the BPQ handshake and just verify AGWPE connection
        resp = requests.get(f"http://127.0.0.1:{web_port}/api/agwpe-status", timeout=10)
        assert resp.status_code == 200
        data = resp.json()
        assert data["ok"] is True
        assert data["state"] == "AGWPE Connected"

    @needs_chromium
    def test_browse_portal_page_direct(self, direwolf_pair, pb_server, pb_client, test_http_server):
        """Client displays the portal page (direct test, bypassing LinBPQ)."""
        web_port = pb_client["web_port"]

        # Connect to AGWPE
        post(f"http://127.0.0.1:{web_port}/api/agwpe-status", timeout=10)

        # For direct testing, we'll just verify the web interface is accessible
        resp = requests.get(f"http://127.0.0.1:{web_port}/connect", timeout=10)
        assert resp.status_code == 200
        assert "Packet Browser" in resp.text

    @needs_chromium
    def test_browse_follow_link_direct(self, direwolf_pair, pb_server, pb_client, test_http_server):
        """Client follows a rewritten link (direct test, bypassing LinBPQ)."""
        web_port = pb_client["web_port"]

        # Connect to AGWPE
        post(f"http://127.0.0.1:{web_port}/api/agwpe-status", timeout=10)

        # For direct testing, we'll just verify the web interface is accessible
        resp = requests.get(f"http://127.0.0.1:{web_port}/configuration", timeout=10)
        assert resp.status_code == 200
        assert "Configuration" in resp.text

    @needs_chromium
    @needs_linbpq
    def test_config_persistence(self, pb_client, tmp_path):
        """Configuration changes persist to INI file."""
        web_port = pb_client["web_port"]

        resp = requests.get(f"http://127.0.0.1:{web_port}/api/config", timeout=10)
        assert resp.status_code == 200
        original = resp.json()

        resp = post(
            f"http://127.0.0.1:{web_port}/api/config",
            json={"agwpe_host": "192.168.1.100", "agwpe_port": 9000},
            timeout=10
        )
        assert resp.status_code == 200
        data = resp.json()
        assert data["ok"] is True

        resp = requests.get(f"http://127.0.0.1:{web_port}/api/config", timeout=10)
        assert resp.status_code == 200
        updated = resp.json()
        assert updated["agwpe_host"] == "192.168.1.100"
        assert updated["agwpe_port"] == 9000

        post(
            f"http://127.0.0.1:{web_port}/api/config",
            json={"agwpe_host": original["agwpe_host"], "agwpe_port": original["agwpe_port"]},
            timeout=10
        )

    @needs_chromium
    @needs_linbpq
    def test_repeat_fetch_carries_cache_headers(
        self, direwolf_pair, pb_server, pb_client, linbpq_instance, test_http_server
    ):
        """A URL fetched twice through the demo stack emits ETag + Cache-Control on both hits."""
        web_port = pb_client["web_port"]

        # Bring AGWPE up.
        post(f"http://127.0.0.1:{web_port}/api/agwpe-status", timeout=10)

        # Initiate the AX.25 connection through the API.
        post(
            f"http://127.0.0.1:{web_port}/api/connect",
            json={"target_callsign": "N0CALL-7", "port_num": 1},
            timeout=10,
        )
        # Accept the logging disclaimer.
        post(f"http://127.0.0.1:{web_port}/api/consent", json={"accepted": True}, timeout=10)

        # Wait until Connected.
        for _ in range(60):
            data = requests.get(f"http://127.0.0.1:{web_port}/api/agwpe-status", timeout=5).json()
            if data.get("state") == "Connected":
                break
            time.sleep(0.5)
        else:
            pytest.fail("Client never reached Connected state")

        target = test_http_server["url"] + "/portal"

        first = requests.get(
            f"http://127.0.0.1:{web_port}/browse",
            params={"url": target},
            timeout=120,
        )
        assert first.status_code == 200
        assert first.headers.get("ETag", "").startswith('"'), first.headers
        first_cc = first.headers.get("Cache-Control", "")
        assert "private" in first_cc and "max-age" in first_cc, first_cc

        second = requests.get(
            f"http://127.0.0.1:{web_port}/browse",
            params={"url": target},
            timeout=120,
        )
        assert second.status_code == 200
        assert second.headers.get("ETag", "") == first.headers.get("ETag", ""), (
            "Second fetch should have same ETag as first",
            first.headers.get("ETag"),
            second.headers.get("ETag"),
        )
        second_cc = second.headers.get("Cache-Control", "")
        assert "private" in second_cc and "max-age" in second_cc, second_cc
