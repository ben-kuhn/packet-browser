#!/usr/bin/env python3
"""Simple Telnet to TCP bridge for testing.

Accepts Telnet connections from LinBPQ and bridges to packet-browser server.
"""

import asyncio
import sys


async def bridge_connection(reader_in, writer_in, server_host, server_port):
    """Bridge a single connection."""
    print(f"[BRIDGE] New connection from {writer_in.get_extra_info('peername')}")
    
    try:
        # Connect to packet-browser server
        reader_out, writer_out = await asyncio.open_connection(server_host, server_port)
        print(f"[BRIDGE] Connected to server at {server_host}:{server_port}")
    except Exception as e:
        print(f"[BRIDGE] Failed to connect to server: {e}")
        writer_in.close()
        return
    
    async def forward_in_to_out():
        """Forward data from LinBPQ to server."""
        try:
            while True:
                data = await reader_in.read(4096)
                if not data:
                    break
                print(f"[BRIDGE] LinBPQ -> Server: {len(data)} bytes")
                writer_out.write(data)
                await writer_out.drain()
        except Exception as e:
            print(f"[BRIDGE] Error forwarding LinBPQ -> Server: {e}")
        finally:
            writer_out.close()
    
    async def forward_out_to_in():
        """Forward data from server to LinBPQ."""
        try:
            while True:
                data = await reader_out.read(4096)
                if not data:
                    break
                print(f"[BRIDGE] Server -> LinBPQ: {len(data)} bytes")
                writer_in.write(data)
                await writer_in.drain()
        except Exception as e:
            print(f"[BRIDGE] Error forwarding Server -> LinBPQ: {e}")
        finally:
            writer_in.close()
    
    # Run both directions concurrently
    await asyncio.gather(forward_in_to_out(), forward_out_to_in())
    print("[BRIDGE] Connection closed")


async def run_bridge(listen_host, listen_port, server_host, server_port):
    """Run the bridge server."""
    async def handle_connection(reader, writer):
        await bridge_connection(reader, writer, server_host, server_port)
    
    server = await asyncio.start_server(handle_connection, listen_host, listen_port)
    print(f"[BRIDGE] Listening on {listen_host}:{listen_port}")
    print(f"[BRIDGE] Forwarding to {server_host}:{server_port}")
    
    async with server:
        await server.serve_forever()


if __name__ == '__main__':
    if len(sys.argv) != 5:
        print(f"Usage: {sys.argv[0]} <listen_host> <listen_port> <server_host> <server_port>")
        sys.exit(1)
    
    listen_host = sys.argv[1]
    listen_port = int(sys.argv[2])
    server_host = sys.argv[3]
    server_port = int(sys.argv[4])
    
    asyncio.run(run_bridge(listen_host, listen_port, server_host, server_port))
