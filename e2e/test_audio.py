"""Audio path validation tests."""

import pytest

from conftest import pytestmark


class TestAudioPath:
    """Validate PipeWire audio path between Direwolf instances."""

    def test_direwolf_pair_starts(self, direwolf_pair):
        """Both Direwolf instances are running after audio cross-link."""
        assert direwolf_pair["proc_a"].poll() is None
        assert direwolf_pair["proc_b"].poll() is None

    def test_agwpe_ports_accept_connections(self, direwolf_pair):
        """Both Direwolf AGWPE ports accept TCP connections."""
        import socket

        for port_name in ["agwpe_port_a", "agwpe_port_b"]:
            port = direwolf_pair[port_name]
            with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
                s.settimeout(2.0)
                result = s.connect_ex(("127.0.0.1", port))
                assert result == 0, f"Could not connect to {port_name} on port {port}"
