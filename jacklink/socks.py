# JackLink - Portable Computing Gateway
# Copyright (c) 2025 Konstantin Zaremski
# Licensed under the MIT License

"""
SOCKS5 proxy server for routing browser traffic through JackLink.

This allows users to configure their browser's proxy settings to route
all traffic through the JackLink server's network connection (e.g., Tailscale).
"""

import asyncio
import struct
import socket
from typing import Optional, Tuple

# SOCKS5 constants
SOCKS_VERSION = 0x05

# Authentication methods
AUTH_NONE = 0x00
AUTH_NO_ACCEPTABLE = 0xFF

# Commands
CMD_CONNECT = 0x01

# Address types
ATYP_IPV4 = 0x01
ATYP_DOMAIN = 0x03
ATYP_IPV6 = 0x04

# Reply codes
REP_SUCCESS = 0x00
REP_GENERAL_FAILURE = 0x01
REP_CONNECTION_NOT_ALLOWED = 0x02
REP_NETWORK_UNREACHABLE = 0x03
REP_HOST_UNREACHABLE = 0x04
REP_CONNECTION_REFUSED = 0x05
REP_TTL_EXPIRED = 0x06
REP_COMMAND_NOT_SUPPORTED = 0x07
REP_ADDRESS_TYPE_NOT_SUPPORTED = 0x08


class Socks5Server:
    """Async SOCKS5 proxy server."""

    def __init__(self, host: str = "0.0.0.0", port: int = 1080):
        self.host = host
        self.port = port
        self.server: Optional[asyncio.Server] = None
        self.active_connections = 0

    async def start(self):
        """Start the SOCKS5 server."""
        self.server = await asyncio.start_server(
            self._handle_client,
            self.host,
            self.port,
            reuse_address=True,
        )

    async def stop(self):
        """Stop the SOCKS5 server."""
        if self.server:
            self.server.close()
            await self.server.wait_closed()

    async def _handle_client(
        self,
        reader: asyncio.StreamReader,
        writer: asyncio.StreamWriter,
    ):
        """Handle a SOCKS5 client connection."""
        self.active_connections += 1
        remote_writer: Optional[asyncio.StreamWriter] = None

        try:
            # Authentication negotiation
            if not await self._handle_auth(reader, writer):
                return

            # Connection request
            dest_addr, dest_port = await self._handle_request(reader, writer)
            if dest_addr is None:
                return

            # Connect to destination
            remote_writer = await self._connect_remote(
                writer, dest_addr, dest_port
            )
            if remote_writer is None:
                return

            # Relay data between client and remote
            await self._relay(reader, writer, remote_writer)

        except (asyncio.CancelledError, ConnectionResetError, BrokenPipeError):
            pass
        except Exception:
            pass
        finally:
            self.active_connections -= 1
            writer.close()
            try:
                await writer.wait_closed()
            except Exception:
                pass
            if remote_writer:
                remote_writer.close()
                try:
                    await remote_writer.wait_closed()
                except Exception:
                    pass

    async def _handle_auth(
        self,
        reader: asyncio.StreamReader,
        writer: asyncio.StreamWriter,
    ) -> bool:
        """Handle SOCKS5 authentication negotiation."""
        # Read version and number of methods
        data = await reader.read(2)
        if len(data) < 2:
            return False

        version, nmethods = data[0], data[1]
        if version != SOCKS_VERSION:
            return False

        # Read authentication methods
        methods = await reader.read(nmethods)
        if len(methods) < nmethods:
            return False

        # We only support no authentication
        if AUTH_NONE in methods:
            writer.write(bytes([SOCKS_VERSION, AUTH_NONE]))
            await writer.drain()
            return True
        else:
            writer.write(bytes([SOCKS_VERSION, AUTH_NO_ACCEPTABLE]))
            await writer.drain()
            return False

    async def _handle_request(
        self,
        reader: asyncio.StreamReader,
        writer: asyncio.StreamWriter,
    ) -> Tuple[Optional[str], Optional[int]]:
        """Handle SOCKS5 connection request."""
        # Read request header: VER CMD RSV ATYP
        header = await reader.read(4)
        if len(header) < 4:
            return None, None

        version, cmd, _, atyp = header

        if version != SOCKS_VERSION:
            return None, None

        if cmd != CMD_CONNECT:
            await self._send_reply(writer, REP_COMMAND_NOT_SUPPORTED)
            return None, None

        # Parse destination address
        dest_addr: Optional[str] = None

        if atyp == ATYP_IPV4:
            addr_data = await reader.read(4)
            if len(addr_data) < 4:
                return None, None
            dest_addr = socket.inet_ntoa(addr_data)

        elif atyp == ATYP_DOMAIN:
            length_data = await reader.read(1)
            if len(length_data) < 1:
                return None, None
            length = length_data[0]
            domain_data = await reader.read(length)
            if len(domain_data) < length:
                return None, None
            dest_addr = domain_data.decode("utf-8")

        elif atyp == ATYP_IPV6:
            addr_data = await reader.read(16)
            if len(addr_data) < 16:
                return None, None
            dest_addr = socket.inet_ntop(socket.AF_INET6, addr_data)

        else:
            await self._send_reply(writer, REP_ADDRESS_TYPE_NOT_SUPPORTED)
            return None, None

        # Read destination port
        port_data = await reader.read(2)
        if len(port_data) < 2:
            return None, None
        dest_port = struct.unpack("!H", port_data)[0]

        return dest_addr, dest_port

    async def _connect_remote(
        self,
        writer: asyncio.StreamWriter,
        dest_addr: str,
        dest_port: int,
    ) -> Optional[asyncio.StreamWriter]:
        """Connect to the remote destination."""
        try:
            remote_reader, remote_writer = await asyncio.wait_for(
                asyncio.open_connection(dest_addr, dest_port),
                timeout=30.0,
            )

            # Send success reply
            await self._send_reply(writer, REP_SUCCESS)
            return remote_writer

        except asyncio.TimeoutError:
            await self._send_reply(writer, REP_TTL_EXPIRED)
        except ConnectionRefusedError:
            await self._send_reply(writer, REP_CONNECTION_REFUSED)
        except OSError as e:
            if e.errno == 101:  # Network unreachable
                await self._send_reply(writer, REP_NETWORK_UNREACHABLE)
            elif e.errno == 113:  # No route to host
                await self._send_reply(writer, REP_HOST_UNREACHABLE)
            else:
                await self._send_reply(writer, REP_GENERAL_FAILURE)
        except Exception:
            await self._send_reply(writer, REP_GENERAL_FAILURE)

        return None

    async def _send_reply(
        self,
        writer: asyncio.StreamWriter,
        reply_code: int,
    ):
        """Send a SOCKS5 reply."""
        # Reply format: VER REP RSV ATYP BND.ADDR BND.PORT
        # We use 0.0.0.0:0 as bound address
        reply = bytes([
            SOCKS_VERSION,
            reply_code,
            0x00,  # Reserved
            ATYP_IPV4,
            0, 0, 0, 0,  # Bound address (0.0.0.0)
            0, 0,  # Bound port (0)
        ])
        writer.write(reply)
        await writer.drain()

    async def _relay(
        self,
        client_reader: asyncio.StreamReader,
        client_writer: asyncio.StreamWriter,
        remote_writer: asyncio.StreamWriter,
    ):
        """Relay data between client and remote."""
        # Get the remote reader from the connection
        remote_reader = remote_writer.get_extra_info("reader")
        if remote_reader is None:
            # Fallback: we need to get reader from the transport
            # This shouldn't happen with asyncio.open_connection
            return

        async def client_to_remote():
            try:
                while True:
                    data = await client_reader.read(65536)
                    if not data:
                        break
                    remote_writer.write(data)
                    await remote_writer.drain()
            except Exception:
                pass

        async def remote_to_client():
            try:
                while True:
                    data = await remote_reader.read(65536)
                    if not data:
                        break
                    client_writer.write(data)
                    await client_writer.drain()
            except Exception:
                pass

        # Run both relay tasks concurrently
        task1 = asyncio.create_task(client_to_remote())
        task2 = asyncio.create_task(remote_to_client())

        done, pending = await asyncio.wait(
            [task1, task2],
            return_when=asyncio.FIRST_COMPLETED,
        )

        for task in pending:
            task.cancel()
            try:
                await task
            except asyncio.CancelledError:
                pass


# Store remote reader alongside writer during connection
_original_open_connection = asyncio.open_connection


async def _open_connection_with_reader(*args, **kwargs):
    """Wrapper to store reader in writer's extra info."""
    reader, writer = await _original_open_connection(*args, **kwargs)
    # Store reader reference for relay function
    writer._reader = reader
    return reader, writer


# Patch to get reader from writer
def _get_reader_from_writer(writer: asyncio.StreamWriter):
    """Get the reader associated with a writer."""
    return getattr(writer, "_reader", None)


# Override the relay method's reader access
Socks5Server._get_reader = staticmethod(_get_reader_from_writer)


# Update the relay method to use our reader access
async def _relay_fixed(
    self,
    client_reader: asyncio.StreamReader,
    client_writer: asyncio.StreamWriter,
    remote_writer: asyncio.StreamWriter,
):
    """Relay data between client and remote."""
    remote_reader = getattr(remote_writer, "_reader", None)
    if remote_reader is None:
        return

    async def client_to_remote():
        try:
            while True:
                data = await client_reader.read(65536)
                if not data:
                    break
                remote_writer.write(data)
                await remote_writer.drain()
        except Exception:
            pass

    async def remote_to_client():
        try:
            while True:
                data = await remote_reader.read(65536)
                if not data:
                    break
                client_writer.write(data)
                await client_writer.drain()
        except Exception:
            pass

    task1 = asyncio.create_task(client_to_remote())
    task2 = asyncio.create_task(remote_to_client())

    done, pending = await asyncio.wait(
        [task1, task2],
        return_when=asyncio.FIRST_COMPLETED,
    )

    for task in pending:
        task.cancel()
        try:
            await task
        except asyncio.CancelledError:
            pass


# Global server instance
socks_server: Optional[Socks5Server] = None


async def start_socks_server(host: str = "0.0.0.0", port: int = 1080) -> Socks5Server:
    """Start the global SOCKS5 server."""
    global socks_server
    socks_server = Socks5Server(host, port)

    # Use our patched open_connection
    original_connect = asyncio.open_connection

    async def patched_open_connection(*args, **kwargs):
        reader, writer = await original_connect(*args, **kwargs)
        writer._reader = reader
        return reader, writer

    # Patch the connect method
    socks_server._original_open_connection = original_connect
    socks_server._patched_open_connection = patched_open_connection

    # Override _connect_remote to use patched connection
    original_connect_remote = socks_server._connect_remote

    async def patched_connect_remote(writer, dest_addr, dest_port):
        try:
            remote_reader, remote_writer = await asyncio.wait_for(
                patched_open_connection(dest_addr, dest_port),
                timeout=30.0,
            )
            await socks_server._send_reply(writer, REP_SUCCESS)
            return remote_writer
        except asyncio.TimeoutError:
            await socks_server._send_reply(writer, REP_TTL_EXPIRED)
        except ConnectionRefusedError:
            await socks_server._send_reply(writer, REP_CONNECTION_REFUSED)
        except OSError as e:
            if e.errno == 101:
                await socks_server._send_reply(writer, REP_NETWORK_UNREACHABLE)
            elif e.errno == 113:
                await socks_server._send_reply(writer, REP_HOST_UNREACHABLE)
            else:
                await socks_server._send_reply(writer, REP_GENERAL_FAILURE)
        except Exception:
            await socks_server._send_reply(writer, REP_GENERAL_FAILURE)
        return None

    socks_server._connect_remote = patched_connect_remote
    socks_server._relay = lambda *args: _relay_fixed(socks_server, *args)

    await socks_server.start()
    return socks_server


def get_socks_connection_count() -> int:
    """Get the number of active SOCKS connections."""
    if socks_server:
        return socks_server.active_connections
    return 0
