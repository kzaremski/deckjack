# JackLink

**Plug in. You're home.**

JackLink is a portable computing gateway that serves a browser-based environment from your phone or laptop over USB tether. Plug into any machine, open a browser, and access your full environment without installing anything on the host.

## Features

- **Terminal**: Full PTY terminal via xterm.js
- **File Manager**: Browse, upload, download files
- **Code Editor**: Monaco editor with syntax highlighting
- **Image Viewer**: Browse and rotate images
- **VNC Desktop**: Connect to local VNC server via noVNC
- **Browser**: Web proxy for browsing through the server's network connection
- **KeePass**: View KDBX password databases with TOTP support
- **WebDAV**: Mount as network drive in any file manager
- **Settings**: Configurable username and wallpaper

## Requirements

- Python 3.11+
- Linux (uses PTY)

## Installation

```bash
pip install -e .
```

Or install dependencies directly:

```bash
pip install -r requirements.txt
```

## Usage

```bash
python -m jacklink
```

The TUI will display:
- Available network interfaces
- Server URL and WebDAV URL
- Authentication token (regenerate with `R`)
- VNC status
- Active connections

Press number keys to select an interface, `R` to regenerate the token, `Q` to quit.

## Authentication

- **Username**: Configurable in Settings (default: `jacklink`)
- **Password**: Random token displayed in TUI

Use these credentials for:
- Web login form
- WebDAV HTTP Basic Auth

## USB Tethering

1. Enable USB tethering on your phone/laptop
2. Connect to the target machine
3. Run JackLink
4. Select the USB network interface
5. Open the displayed URL in the host's browser

## WebDAV

Mount `http://<ip>:<port>/webdav` in your file manager:
- **Linux**: `gio mount dav://jacklink:<token>@<ip>:<port>/webdav`
- **macOS**: Finder → Go → Connect to Server
- **Windows**: Map Network Drive

## VNC

If a VNC server is running on localhost (ports 5900-5910), JackLink will detect it and enable the Desktop feature, proxying VNC through the WebSocket connection.

## License

MIT License - Copyright (c) 2025 Konstantin Zaremski
