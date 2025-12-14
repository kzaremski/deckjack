# JackLink - Portable Computing Gateway
# Copyright (c) 2025 Konstantin Zaremski
# Licensed under the MIT License

import socket
import subprocess
from dataclasses import dataclass, field
from typing import Optional
from pathlib import Path


@dataclass
class NetworkInterface:
    name: str
    ip: str


@dataclass
class JackLinkConfig:
    host: str = "0.0.0.0"
    port: int = 8080
    root_path: Path = field(default_factory=lambda: Path.home())
    selected_interface: Optional[NetworkInterface] = None


def get_network_interfaces() -> list[NetworkInterface]:
    interfaces = []
    try:
        result = subprocess.run(
            ["ip", "-4", "addr", "show"],
            capture_output=True,
            text=True,
            timeout=5
        )
        current_iface = None
        for line in result.stdout.split("\n"):
            if ": " in line and not line.startswith(" "):
                parts = line.split(": ")
                if len(parts) >= 2:
                    current_iface = parts[1].split("@")[0]
            elif "inet " in line and current_iface:
                parts = line.strip().split()
                if len(parts) >= 2:
                    ip = parts[1].split("/")[0]
                    if ip != "127.0.0.1":
                        interfaces.append(NetworkInterface(name=current_iface, ip=ip))
    except Exception:
        try:
            hostname = socket.gethostname()
            ip = socket.gethostbyname(hostname)
            if ip != "127.0.0.1":
                interfaces.append(NetworkInterface(name="default", ip=ip))
        except Exception:
            pass
    if not interfaces:
        interfaces.append(NetworkInterface(name="localhost", ip="127.0.0.1"))
    return interfaces


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


config = JackLinkConfig()
