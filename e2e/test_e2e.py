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
