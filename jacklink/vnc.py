# JackLink - Portable Computing Gateway
# Copyright (c) 2025 Konstantin Zaremski
# Licensed under the MIT License

import socket
import asyncio
from typing import Optional


def detect_vnc() -> Optional[int]:
    for port in range(5900, 5911):
        try:
            sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
            sock.settimeout(0.5)
            result = sock.connect_ex(("127.0.0.1", port))
            sock.close()
            if result == 0:
                return port
        except Exception:
            pass
    return None


async def vnc_proxy(websocket, vnc_port: int):
    reader: Optional[asyncio.StreamReader] = None
    writer: Optional[asyncio.StreamWriter] = None

    try:
        reader, writer = await asyncio.open_connection("127.0.0.1", vnc_port)

        async def vnc_to_ws():
            try:
                while True:
                    data = await reader.read(65536)
                    if not data:
                        break
                    await websocket.send(data)
            except Exception:
                pass

        async def ws_to_vnc():
            try:
                async for message in websocket:
                    if isinstance(message, bytes):
                        writer.write(message)
                        await writer.drain()
            except Exception:
                pass

        vnc_task = asyncio.create_task(vnc_to_ws())
        ws_task = asyncio.create_task(ws_to_vnc())

        done, pending = await asyncio.wait(
            [vnc_task, ws_task],
            return_when=asyncio.FIRST_COMPLETED
        )

        for task in pending:
            task.cancel()
            try:
                await task
            except asyncio.CancelledError:
                pass

    except Exception:
        pass
    finally:
        if writer:
            writer.close()
            try:
                await writer.wait_closed()
            except Exception:
                pass
