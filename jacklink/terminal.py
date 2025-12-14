# JackLink - Portable Computing Gateway
# Copyright (c) 2025 Konstantin Zaremski
# Licensed under the MIT License

import os
import pty
import fcntl
import struct
import termios
import asyncio
import signal
import json
from typing import Optional


class TerminalSession:
    def __init__(self):
        self.master_fd: Optional[int] = None
        self.pid: Optional[int] = None
        self.running = False

    def spawn(self, cols: int = 80, rows: int = 24) -> None:
        pid, master_fd = pty.fork()
        if pid == 0:
            os.environ["TERM"] = "xterm-256color"
            os.environ["COLORTERM"] = "truecolor"
            shell = os.environ.get("SHELL", "/bin/bash")
            os.execvp(shell, [shell])
        else:
            self.pid = pid
            self.master_fd = master_fd
            self.running = True
            self.resize(cols, rows)
            flags = fcntl.fcntl(master_fd, fcntl.F_GETFL)
            fcntl.fcntl(master_fd, fcntl.F_SETFL, flags | os.O_NONBLOCK)

    def resize(self, cols: int, rows: int) -> None:
        if self.master_fd is not None:
            winsize = struct.pack("HHHH", rows, cols, 0, 0)
            fcntl.ioctl(self.master_fd, termios.TIOCSWINSZ, winsize)

    def write(self, data: bytes) -> None:
        if self.master_fd is not None and self.running:
            os.write(self.master_fd, data)

    def read(self) -> Optional[bytes]:
        if self.master_fd is None or not self.running:
            return None
        try:
            return os.read(self.master_fd, 65536)
        except (OSError, BlockingIOError):
            return None

    def terminate(self) -> None:
        self.running = False
        if self.pid is not None:
            try:
                os.kill(self.pid, signal.SIGTERM)
                os.waitpid(self.pid, os.WNOHANG)
            except (OSError, ChildProcessError):
                pass
        if self.master_fd is not None:
            try:
                os.close(self.master_fd)
            except OSError:
                pass
        self.pid = None
        self.master_fd = None


async def terminal_handler(websocket, path=None):
    session = TerminalSession()
    try:
        session.spawn()

        async def read_pty():
            loop = asyncio.get_event_loop()
            while session.running:
                try:
                    data = await loop.run_in_executor(None, session.read)
                    if data:
                        await websocket.send(data)
                except Exception:
                    break
                await asyncio.sleep(0.01)

        read_task = asyncio.create_task(read_pty())

        async for message in websocket:
            if isinstance(message, bytes):
                session.write(message)
            else:
                try:
                    msg = json.loads(message)
                    if msg.get("type") == "resize":
                        cols = msg.get("cols", 80)
                        rows = msg.get("rows", 24)
                        session.resize(cols, rows)
                    elif msg.get("type") == "input":
                        data = msg.get("data", "")
                        session.write(data.encode())
                except json.JSONDecodeError:
                    session.write(message.encode())

        read_task.cancel()
        try:
            await read_task
        except asyncio.CancelledError:
            pass
    finally:
        session.terminate()
