"""Shared utility functions for e2e tests."""

import json
import os
import random
import re
import shutil
import socket
import subprocess
import time


def free_port():
    """Return a free TCP port in the range 10000-49151."""
    while True:
        port = random.randint(10000, 49151)
        with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
            try:
                s.bind(("127.0.0.1", port))
                return port
            except OSError:
                continue


def wait_for_port(port, host="127.0.0.1", timeout=60.0):
    """Block until a TCP port is accepting connections."""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            with socket.create_connection((host, port), timeout=0.5):
                return True
        except OSError:
            time.sleep(0.1)
    raise TimeoutError(f"Port {port} not ready after {timeout}s")


def kill_proc(proc):
    """Terminate a subprocess, escalating to SIGKILL if needed."""
    if proc.poll() is not None:
        return
    proc.terminate()
    try:
        proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait(timeout=5)


def get_pw_ports(pid):
    """Find PipeWire port IDs for a Direwolf process by its PID.

    Returns dict with:
      - 'tx_output': playback output port ID (TX audio out, first channel)
      - 'rx_input': capture input port ID (RX audio in)
      - 'all': list of all port IDs (for disconnecting defaults)
    """
    result = subprocess.run(
        ["pw-dump"], capture_output=True, text=True, timeout=5,
    )
    data = json.loads(result.stdout)

    client_ids = set()
    for obj in data:
        props = obj.get("info", {}).get("props", {})
        if (props.get("application.process.id") == pid
                and obj.get("type") == "PipeWire:Interface:Client"):
            client_ids.add(obj["id"])

    nodes = {}
    for obj in data:
        props = obj.get("info", {}).get("props", {})
        if (props.get("client.id") in client_ids
                and obj.get("type") == "PipeWire:Interface:Node"):
            nodes[obj["id"]] = props.get("node.name", "")

    tx_output = None
    rx_input = None
    all_ports = []
    for obj in data:
        if obj.get("type") != "PipeWire:Interface:Port":
            continue
        props = obj.get("info", {}).get("props", {})
        node_id = props.get("node.id")
        if node_id not in nodes:
            continue
        port_id = obj["id"]
        port_name = props.get("port.name", "")
        node_name = nodes[node_id]
        all_ports.append(port_id)

        if "playback" in node_name and port_name == "output_FL":
            tx_output = port_id
        if "capture" in node_name and port_name == "input_MONO":
            rx_input = port_id

    return {"tx_output": tx_output, "rx_input": rx_input, "all": all_ports}


def pw_disconnect_links(port_ids, pw_data):
    """Disconnect all PipeWire links involving the given port IDs."""
    port_set = set(port_ids)
    for obj in pw_data:
        if obj.get("type") != "PipeWire:Interface:Link":
            continue
        props = obj.get("info", {}).get("props", {})
        out_port = props.get("link.output.port")
        in_port = props.get("link.input.port")
        if out_port in port_set or in_port in port_set:
            link_id = obj["id"]
            subprocess.run(
                ["pw-link", "-d", str(link_id)],
                capture_output=True, timeout=5,
            )


def pw_set_capture_volume(pid, volume, pw_data):
    """Set the capture node volume for a Direwolf process."""
    client_ids = set()
    for obj in pw_data:
        props = obj.get("info", {}).get("props", {})
        if (props.get("application.process.id") == pid
                and obj.get("type") == "PipeWire:Interface:Client"):
            client_ids.add(obj["id"])

    for obj in pw_data:
        if obj.get("type") != "PipeWire:Interface:Node":
            continue
        props = obj.get("info", {}).get("props", {})
        if (props.get("client.id") in client_ids
                and "capture" in props.get("node.name", "")):
            subprocess.run(
                ["pw-cli", "s", str(obj["id"]),
                 "Props", f"{{ channelVolumes: [{volume}] }}"],
                capture_output=True, timeout=5,
            )


def pw_configure_for_test():
    """Configure PipeWire settings for reliable AFSK audio.

    Returns a dict of original settings for restoration.
    """
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

    return original


def pw_restore_settings(original=None):
    """Reset PipeWire clock settings to the system defaults.

    Deletes the runtime overrides applied by pw_configure_for_test() so
    PipeWire falls back to its configured default.clock.* values.  We
    deliberately do NOT replay a captured snapshot: replaying a snapshot
    taken while the graph was already in a bad state would re-break audio.
    ``original`` is accepted for backwards compatibility and ignored.
    """
    for key in ("clock.allowed-rates", "clock.quantum",
                "clock.min-quantum", "clock.max-quantum"):
        subprocess.run(
            ["pw-metadata", "-n", "settings", "-d", "0", key],
            capture_output=True, timeout=5,
        )


def pw_crosslink(pid_a, pid_b):
    """Cross-link two Direwolf instances' audio via PipeWire."""
    ports_a = get_pw_ports(pid_a)
    ports_b = get_pw_ports(pid_b)

    if not ports_a["tx_output"] or not ports_a["rx_input"]:
        raise RuntimeError(
            f"Direwolf PID {pid_a} missing PipeWire ports: {ports_a}"
        )
    if not ports_b["tx_output"] or not ports_b["rx_input"]:
        raise RuntimeError(
            f"Direwolf PID {pid_b} missing PipeWire ports: {ports_b}"
        )

    result = subprocess.run(
        ["pw-dump"], capture_output=True, text=True, timeout=5,
    )
    pw_data = json.loads(result.stdout)

    pw_disconnect_links(ports_a["all"] + ports_b["all"], pw_data)

    subprocess.run(
        ["pw-link", str(ports_a["tx_output"]), str(ports_b["rx_input"])],
        capture_output=True, timeout=5, check=True,
    )

    subprocess.run(
        ["pw-link", str(ports_b["tx_output"]), str(ports_a["rx_input"])],
        capture_output=True, timeout=5, check=True,
    )

    pw_set_capture_volume(pid_a, 0.25, pw_data)
    pw_set_capture_volume(pid_b, 0.25, pw_data)

    return []
