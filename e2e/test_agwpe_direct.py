#!/usr/bin/env python3
"""Test AGWPE connection to Direwolf."""

import asyncio
import struct
import sys

AGWPE_HEADER_FORMAT = '<BBBBBBBB10s10sII'
AGWPE_HEADER_SIZE = 36

def create_agwpe_frame(port, datakind, call_from, call_to, data=b''):
    """Create an AGWPE frame."""
    return struct.pack(
        AGWPE_HEADER_FORMAT,
        port, 0, 0, 0, datakind, 0, 0, 0,
        call_from.encode().ljust(10, b'\x00'),
        call_to.encode().ljust(10, b'\x00'),
        len(data), 0
    ) + data

async def test_agwpe(direwolf_pair):
    """Test AGWPE connection to Direwolf."""
    # Use the direwolf_pair fixture to get the AGWPE port
    host = "127.0.0.1"
    port = direwolf_pair["agwpe_port_a"]
    
    print(f"Connecting to {host}:{port}...")
    reader, writer = await asyncio.open_connection(host, port)
    print("Connected!")
    
    # Send registration frame
    print("Sending registration frame for W1TEST...")
    frame = create_agwpe_frame(0, ord('X'), 'W1TEST', '', b'')
    print(f"Frame bytes: {frame.hex()}")
    writer.write(frame)
    await writer.drain()
    print("Frame sent!")
    
    # Read response
    print("Waiting for response...")
    header = await asyncio.wait_for(reader.readexactly(AGWPE_HEADER_SIZE), timeout=5.0)
    print(f"Response header: {header.hex()}")
    
    # Parse header
    port, res1, res2, res3, datakind, res4, pid, res5, call_from, call_to, data_len, user = struct.unpack(
        AGWPE_HEADER_FORMAT, header
    )
    print(f"Port: {port}")
    print(f"Datakind: {chr(datakind)} (0x{datakind:02x})")
    print(f"Call from: {call_from.decode().strip()}")
    print(f"Call to: {call_to.decode().strip()}")
    print(f"Data len: {data_len}")
    
    if data_len > 0:
        data = await asyncio.wait_for(reader.readexactly(data_len), timeout=5.0)
        print(f"Data: {data.hex()}")
    
    writer.close()
    await writer.wait_closed()
    print("Done!")
