# JackLink - Portable Computing Gateway
# Copyright (c) 2025 Konstantin Zaremski
# Licensed under the MIT License

import sys
import threading
import time
from datetime import datetime
from pathlib import Path
from typing import Optional

import asyncio
import uvicorn
from rich.console import Console
from rich.layout import Layout
from rich.live import Live
from rich.panel import Panel
from rich.table import Table
from rich.text import Text

from .auth import auth
from .config import config, get_network_interfaces, detect_vnc, NetworkInterface
from .app import app, get_connection_count
from .socks import start_socks_server, get_socks_connection_count


def get_battery_percent() -> Optional[int]:
    """Get battery percentage on Linux/Termux, returns None if not available."""
    import json
    import shutil
    import subprocess

    # Try Termux first (Android)
    if shutil.which("termux-battery-status"):
        try:
            result = subprocess.run(
                ["termux-battery-status"],
                capture_output=True,
                text=True,
                timeout=2
            )
            if result.returncode == 0:
                data = json.loads(result.stdout)
                return int(data.get("percentage", 0))
        except Exception:
            pass

    # Fall back to Linux sysfs
    try:
        for bat_path in Path("/sys/class/power_supply").glob("BAT*"):
            capacity_file = bat_path / "capacity"
            if capacity_file.exists():
                return int(capacity_file.read_text().strip())
    except Exception:
        pass
    return None


class JackLinkTUI:
    def __init__(self):
        self.console = Console()
        self.running = False
        self.server_thread: Optional[threading.Thread] = None
        self.socks_thread: Optional[threading.Thread] = None
        self.interfaces = get_network_interfaces()
        self.selected_index = -1  # -1 = all interfaces (0.0.0.0)
        self.server_started = False

    def make_header(self) -> Table:
        now = datetime.now()
        date_str = now.strftime("%Y-%m-%d")
        time_str = now.strftime("%H:%M:%S")
        battery = get_battery_percent()

        header = Table.grid(expand=True)
        header.add_column(justify="left")
        header.add_column(justify="right")

        left = Text()
        left.append("J A C K L I N K", style="bold #e94560")
        left.append("  ", style="")

        # Tagline with sheen effect
        tagline = "Plug in. You're home."
        sheen_pos = int(time.time() * 8) % (len(tagline) + 6)  # 8 chars per second
        for i, char in enumerate(tagline):
            dist = abs(i - sheen_pos)
            if dist == 0:
                left.append(char, style="#ffffff")
            elif dist == 1:
                left.append(char, style="#bbbbbb")
            elif dist == 2:
                left.append(char, style="#999999")
            else:
                left.append(char, style="#666666")

        right = Text()
        right.append(date_str, style="#888888")
        right.append("  ", style="")
        right.append(time_str, style="bold white")
        if battery is not None:
            right.append("  ", style="")
            right.append(f"{battery}%", style="bold green" if battery > 20 else "bold red")

        header.add_row(left, right)
        return header

    def make_interface_table(self) -> Table:
        table = Table(show_header=True, expand=True, box=None)
        table.add_column("#", style="dim", width=4)
        table.add_column("Interface", style="cyan", ratio=1)
        table.add_column("IP Address", style="green", ratio=1)
        table.add_column("Status", style="yellow", ratio=1)

        # All interfaces option (index -1)
        selected = "► " if self.selected_index == -1 else "  "
        status = "[bold green]SELECTED[/]" if self.selected_index == -1 else ""
        table.add_row(f"{selected}0", "All Interfaces", "0.0.0.0", status)

        for i, iface in enumerate(self.interfaces):
            selected = "► " if i == self.selected_index else "  "
            status = "[bold green]SELECTED[/]" if i == self.selected_index else ""
            table.add_row(
                f"{selected}{i+1}",
                iface.name,
                iface.ip,
                status
            )
        return table

    def make_status_panel(self) -> Panel:
        if not self.server_started:
            content = Text("Press [0-9] to select interface, then server will start...", style="dim")
            return Panel(content, title="Status", border_style="#e94560")

        if self.selected_index == -1:
            display_ip = "0.0.0.0"
        else:
            display_ip = self.interfaces[self.selected_index].ip
        vnc_port = detect_vnc()
        connections = get_connection_count()

        status = Table.grid(padding=(0, 2))
        status.add_column(style="bold #e94560")
        status.add_column(style="white")

        status.add_row("Server URL:", f"[bold green]http://{display_ip}:{config.port}[/]")
        status.add_row("WebDAV URL:", f"[bold cyan]http://{display_ip}:{config.port}/webdav[/]")
        status.add_row("SOCKS Proxy:", f"[bold magenta]socks5://{display_ip}:{config.socks_port}[/]")
        status.add_row("", "")
        status.add_row("Username:", f"[bold yellow]{auth.username}[/]")
        status.add_row("Token:", f"[bold white on #e94560]{auth.token}[/]")
        status.add_row("", "")
        socks_conns = get_socks_connection_count()
        status.add_row("VNC Status:", "[bold green]Detected[/]" if vnc_port else "[dim]Not detected[/]")
        status.add_row("Connections:", f"[cyan]{connections} web, {socks_conns} proxy[/]")

        return Panel(status, title="Server Status", border_style="#e94560")

    def make_controls(self) -> Panel:
        controls = Text()
        controls.append("[0-9]", style="bold #e94560")
        controls.append(" Select Interface  ", style="dim")
        controls.append("[R]", style="bold #e94560")
        controls.append(" Regenerate Token  ", style="dim")
        controls.append("[Q]", style="bold #e94560")
        controls.append(" Quit", style="dim")
        return Panel(controls, border_style="#e94560")

    def make_layout(self) -> Layout:
        layout = Layout()
        layout.split_column(
            Layout(name="header", size=1),
            Layout(name="body"),
            Layout(name="controls", size=3),
        )
        layout["body"].split_column(
            Layout(name="status", size=10),  # 8 lines body + 2 for panel borders
            Layout(name="interfaces"),  # grows/shrinks to fill
        )
        layout["header"].update(self.make_header())
        layout["status"].update(self.make_status_panel())
        layout["interfaces"].update(Panel(self.make_interface_table(), title="Network Interfaces", border_style="#e94560"))
        layout["controls"].update(self.make_controls())
        return layout

    def start_server(self):
        if self.selected_index == -1:
            config.selected_interface = NetworkInterface(name="all", ip="0.0.0.0")
        else:
            config.selected_interface = self.interfaces[self.selected_index]

        def run_server():
            uvicorn.run(
                app,
                host="0.0.0.0",
                port=config.port,
                log_level="error",
                access_log=False,
            )

        def run_socks():
            loop = asyncio.new_event_loop()
            asyncio.set_event_loop(loop)
            loop.run_until_complete(start_socks_server("0.0.0.0", config.socks_port))
            loop.run_forever()

        self.server_thread = threading.Thread(target=run_server, daemon=True)
        self.server_thread.start()

        self.socks_thread = threading.Thread(target=run_socks, daemon=True)
        self.socks_thread.start()

        self.server_started = True

    def handle_key(self, key: str) -> bool:
        if key.lower() == 'q':
            return False
        elif key.lower() == 'r':
            auth.regenerate_token()
        elif key == '0':
            self.selected_index = -1
            if not self.server_started:
                self.start_server()
        elif key.isdigit():
            idx = int(key) - 1
            if 0 <= idx < len(self.interfaces):
                self.selected_index = idx
                if not self.server_started:
                    self.start_server()
        return True

    def run(self):
        self.running = True
        self.start_server()  # Start immediately on 0.0.0.0

        import select
        import termios
        import tty

        old_settings = termios.tcgetattr(sys.stdin)
        try:
            tty.setcbreak(sys.stdin.fileno())

            with Live(self.make_layout(), console=self.console, refresh_per_second=4, screen=True) as live:
                while self.running:
                    if select.select([sys.stdin], [], [], 0.25)[0]:
                        key = sys.stdin.read(1)
                        if not self.handle_key(key):
                            break
                    live.update(self.make_layout())
        finally:
            termios.tcsetattr(sys.stdin, termios.TCSADRAIN, old_settings)


def run_headless():
    console = Console()
    interfaces = get_network_interfaces()
    iface = interfaces[0]
    config.selected_interface = iface
    vnc_port = detect_vnc()

    # Start SOCKS proxy in background thread
    def run_socks():
        loop = asyncio.new_event_loop()
        asyncio.set_event_loop(loop)
        loop.run_until_complete(start_socks_server("0.0.0.0", config.socks_port))
        loop.run_forever()

    socks_thread = threading.Thread(target=run_socks, daemon=True)
    socks_thread.start()

    console.print()
    console.print("[bold #e94560]╔═══════════════════════════════════════╗[/]")
    console.print("[bold #e94560]║           [/][bold white]J A C K L I N K[/][bold #e94560]           ║[/]")
    console.print("[bold #e94560]║         [/][italic #888888]Plug in. You're home.[/][bold #e94560]         ║[/]")
    console.print("[bold #e94560]╚═══════════════════════════════════════╝[/]")
    console.print()
    console.print(f"[bold #e94560]Server URL:[/]  [bold green]http://{iface.ip}:{config.port}[/]")
    console.print(f"[bold #e94560]WebDAV URL:[/]  [bold cyan]http://{iface.ip}:{config.port}/webdav[/]")
    console.print(f"[bold #e94560]SOCKS Proxy:[/] [bold magenta]socks5://{iface.ip}:{config.socks_port}[/]")
    console.print()
    console.print(f"[bold #e94560]Username:[/]    [bold yellow]{auth.username}[/]")
    console.print(f"[bold #e94560]Token:[/]       [bold white on #e94560] {auth.token} [/]")
    console.print()
    console.print(f"[bold #e94560]VNC Status:[/]  {'[bold green]Detected[/] (port ' + str(vnc_port) + ')' if vnc_port else '[dim]Not detected[/]'}")
    console.print()
    console.print("[dim]Press Ctrl+C to stop[/]")
    console.print()

    uvicorn.run(
        app,
        host="0.0.0.0",
        port=config.port,
        log_level="info",
    )


def run():
    if not sys.stdin.isatty():
        run_headless()
        return

    tui = JackLinkTUI()
    try:
        tui.run()
    except KeyboardInterrupt:
        pass
    except Exception:
        run_headless()
    finally:
        print("\n[JackLink] Shutting down...")
