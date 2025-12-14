# JackLink - Portable Computing Gateway
# Copyright (c) 2025 Konstantin Zaremski
# Licensed under the MIT License

import asyncio
import json
from pathlib import Path
from typing import Optional

from fastapi import FastAPI, Request, Response, WebSocket, WebSocketDisconnect, HTTPException
from fastapi.responses import HTMLResponse, JSONResponse, RedirectResponse, StreamingResponse
from fastapi.staticfiles import StaticFiles

from .auth import auth
from .config import config, detect_vnc
from .files import list_directory, read_file, read_file_bytes, write_file, save_upload, get_mime_type, is_image, is_text
from .terminal import TerminalSession
from .user_config import user_config
from .vnc import vnc_proxy
from .webdav import create_webdav_app

app = FastAPI(docs_url=None, redoc_url=None)

STATIC_DIR = Path(__file__).parent.parent / "static"

active_connections: set = set()


def get_session_id(request: Request) -> Optional[str]:
    return request.cookies.get("jacklink_session")


def require_auth(request: Request) -> bool:
    session_id = get_session_id(request)
    return auth.check_session(session_id)


@app.get("/", response_class=HTMLResponse)
async def index(request: Request):
    if not require_auth(request):
        return RedirectResponse(url="/login", status_code=302)
    return HTMLResponse(content=(STATIC_DIR / "index.html").read_text())


@app.get("/login", response_class=HTMLResponse)
async def login_page():
    return HTMLResponse(content=(STATIC_DIR / "login.html").read_text())


@app.post("/login")
async def login(request: Request):
    form = await request.form()
    token = form.get("token", "")
    if auth.validate_token(str(token)):
        session_id = auth.create_session()
        response = RedirectResponse(url="/", status_code=302)
        response.set_cookie(
            key="jacklink_session",
            value=session_id,
            httponly=True,
            samesite="lax",
            max_age=86400
        )
        return response
    return RedirectResponse(url="/login?error=1", status_code=302)


@app.get("/logout")
async def logout():
    response = RedirectResponse(url="/login", status_code=302)
    response.delete_cookie("jacklink_session")
    return response


@app.get("/api/files/list")
async def api_list_files(request: Request, path: str = ""):
    if not require_auth(request):
        return JSONResponse({"error": "Unauthorized"}, status_code=401)
    try:
        target_path = Path(path) if path else config.root_path
        if not target_path.is_absolute():
            target_path = config.root_path / target_path
        target_path = target_path.resolve()
        items = await list_directory(target_path)
        return JSONResponse({
            "path": str(target_path),
            "parent": str(target_path.parent) if target_path != target_path.parent else None,
            "items": items,
        })
    except PermissionError as e:
        return JSONResponse({"error": str(e)}, status_code=403)
    except Exception as e:
        return JSONResponse({"error": str(e)}, status_code=500)


@app.get("/api/files/read")
async def api_read_file(request: Request, path: str):
    if not require_auth(request):
        return JSONResponse({"error": "Unauthorized"}, status_code=401)
    try:
        target_path = Path(path).resolve()
        if is_image(target_path):
            return JSONResponse({
                "path": str(target_path),
                "is_image": True,
                "url": f"/api/files/download/{target_path}",
            })
        content = await read_file(target_path)
        return JSONResponse({
            "path": str(target_path),
            "content": content,
            "is_text": is_text(target_path),
        })
    except PermissionError as e:
        return JSONResponse({"error": str(e)}, status_code=403)
    except Exception as e:
        return JSONResponse({"error": str(e)}, status_code=500)


@app.post("/api/files/write")
async def api_write_file(request: Request):
    if not require_auth(request):
        return JSONResponse({"error": "Unauthorized"}, status_code=401)
    try:
        body = await request.json()
        path = body.get("path")
        content = body.get("content", "")
        if not path:
            return JSONResponse({"error": "Path required"}, status_code=400)
        target_path = Path(path).resolve()
        await write_file(target_path, content)
        return JSONResponse({"success": True, "path": str(target_path)})
    except PermissionError as e:
        return JSONResponse({"error": str(e)}, status_code=403)
    except Exception as e:
        return JSONResponse({"error": str(e)}, status_code=500)


@app.get("/api/files/download/{path:path}")
async def api_download_file(request: Request, path: str):
    if not require_auth(request):
        return JSONResponse({"error": "Unauthorized"}, status_code=401)
    try:
        target_path = Path("/" + path).resolve()
        if not target_path.is_file():
            raise HTTPException(status_code=404, detail="File not found")
        content = await read_file_bytes(target_path)
        mime_type = get_mime_type(target_path)
        return Response(
            content=content,
            media_type=mime_type,
            headers={"Content-Disposition": f"inline; filename=\"{target_path.name}\""}
        )
    except PermissionError as e:
        return JSONResponse({"error": str(e)}, status_code=403)


@app.post("/api/files/upload")
async def api_upload_file(request: Request):
    if not require_auth(request):
        return JSONResponse({"error": "Unauthorized"}, status_code=401)
    try:
        form = await request.form()
        directory = form.get("directory", str(config.root_path))
        file = form.get("file")
        if not file:
            return JSONResponse({"error": "No file provided"}, status_code=400)
        target_path = Path(directory) / file.filename
        content = await file.read()
        await save_upload(target_path.resolve(), content)
        return JSONResponse({"success": True, "path": str(target_path)})
    except PermissionError as e:
        return JSONResponse({"error": str(e)}, status_code=403)
    except Exception as e:
        return JSONResponse({"error": str(e)}, status_code=500)


@app.get("/api/vnc/status")
async def api_vnc_status(request: Request):
    if not require_auth(request):
        return JSONResponse({"error": "Unauthorized"}, status_code=401)
    vnc_port = detect_vnc()
    return JSONResponse({"available": vnc_port is not None, "port": vnc_port})


@app.get("/api/system/info")
async def api_system_info(request: Request):
    if not require_auth(request):
        return JSONResponse({"error": "Unauthorized"}, status_code=401)
    import socket
    hostname = socket.gethostname()
    interface = None
    ip = None
    if config.selected_interface:
        interface = config.selected_interface.name
        ip = config.selected_interface.ip
    return JSONResponse({
        "hostname": hostname,
        "interface": interface,
        "ip": ip
    })


@app.get("/settings", response_class=HTMLResponse)
async def settings_page(request: Request):
    if not require_auth(request):
        return RedirectResponse(url="/login", status_code=302)
    return HTMLResponse(content=(STATIC_DIR / "settings.html").read_text())


@app.get("/api/settings")
async def api_get_settings(request: Request):
    if not require_auth(request):
        return JSONResponse({"error": "Unauthorized"}, status_code=401)
    return JSONResponse({
        "username": user_config.username,
        "wallpaper": user_config.wallpaper,
        "wallpaper_color": user_config.wallpaper_color,
    })


@app.post("/api/settings")
async def api_update_settings(request: Request):
    if not require_auth(request):
        return JSONResponse({"error": "Unauthorized"}, status_code=401)
    try:
        body = await request.json()
        if "username" in body:
            user_config.username = str(body["username"]).strip() or "jacklink"
        if "wallpaper" in body:
            user_config.wallpaper = body["wallpaper"]
        if "wallpaper_color" in body:
            user_config.wallpaper_color = body["wallpaper_color"]
        user_config.save()
        return JSONResponse({"success": True})
    except Exception as e:
        return JSONResponse({"error": str(e)}, status_code=500)


@app.post("/api/settings/wallpaper")
async def api_upload_wallpaper(request: Request):
    if not require_auth(request):
        return JSONResponse({"error": "Unauthorized"}, status_code=401)
    try:
        form = await request.form()
        file = form.get("file")
        if not file:
            return JSONResponse({"error": "No file provided"}, status_code=400)

        from .user_config import CONFIG_DIR
        wallpapers_dir = CONFIG_DIR / "wallpapers"
        wallpapers_dir.mkdir(parents=True, exist_ok=True)

        filename = Path(file.filename).name
        target_path = wallpapers_dir / filename
        content = await file.read()
        target_path.write_bytes(content)

        user_config.wallpaper = str(target_path)
        user_config.save()
        return JSONResponse({"success": True, "path": str(target_path)})
    except Exception as e:
        return JSONResponse({"error": str(e)}, status_code=500)


@app.get("/api/wallpaper")
async def api_get_wallpaper(request: Request):
    if not require_auth(request):
        return JSONResponse({"error": "Unauthorized"}, status_code=401)
    if not user_config.wallpaper:
        raise HTTPException(status_code=404, detail="No wallpaper set")
    wallpaper_path = Path(user_config.wallpaper)
    if not wallpaper_path.is_file():
        raise HTTPException(status_code=404, detail="Wallpaper file not found")
    return Response(
        content=wallpaper_path.read_bytes(),
        media_type=get_mime_type(wallpaper_path),
    )


# Browser proxy endpoints
from .proxy import fetch_url, rewrite_html, rewrite_css, get_content_type, should_rewrite
from urllib.parse import unquote


@app.get("/api/proxy")
async def api_proxy_get(request: Request, url: Optional[str] = None):
    if not require_auth(request):
        return JSONResponse({"error": "Unauthorized"}, status_code=401)

    if not url:
        return JSONResponse({"error": "Missing URL parameter"}, status_code=400)

    url = unquote(url)

    # Basic URL validation
    if not url.startswith(('http://', 'https://')):
        return JSONResponse({"error": "Invalid URL - must start with http:// or https://"}, status_code=400)

    try:
        status_code, headers, content, final_url = await fetch_url(url)

        content_type = get_content_type(headers)

        # Rewrite HTML and CSS content
        if content_type == 'text/html' or content_type == 'application/xhtml+xml':
            try:
                charset = 'utf-8'
                ct_header = headers.get('content-type', '')
                if 'charset=' in ct_header:
                    charset = ct_header.split('charset=')[1].split(';')[0].strip()
                text_content = content.decode(charset, errors='replace')
                rewritten = rewrite_html(text_content, final_url)
                content = rewritten.encode('utf-8')
                headers['content-type'] = 'text/html; charset=utf-8'
            except Exception as e:
                pass  # Return original content on error

        elif content_type == 'text/css':
            try:
                text_content = content.decode('utf-8', errors='replace')
                rewritten = rewrite_css(text_content, final_url)
                content = rewritten.encode('utf-8')
            except Exception:
                pass

        # Remove headers that cause issues
        for h in ['content-encoding', 'transfer-encoding', 'content-length',
                  'content-security-policy', 'x-frame-options']:
            headers.pop(h, None)
            headers.pop(h.title(), None)

        return Response(
            content=content,
            status_code=status_code,
            headers={'Content-Type': headers.get('content-type', 'application/octet-stream')},
        )

    except Exception as e:
        return JSONResponse({"error": f"Failed to fetch URL: {str(e)}"}, status_code=502)


@app.post("/api/proxy")
async def api_proxy_post(request: Request, url: Optional[str] = None):
    if not require_auth(request):
        return JSONResponse({"error": "Unauthorized"}, status_code=401)

    if not url:
        return JSONResponse({"error": "Missing URL parameter"}, status_code=400)

    url = unquote(url)

    if not url.startswith(('http://', 'https://')):
        return JSONResponse({"error": "Invalid URL"}, status_code=400)

    try:
        body = await request.body()
        content_type = request.headers.get('content-type', 'application/x-www-form-urlencoded')

        status_code, headers, content, final_url = await fetch_url(
            url,
            method='POST',
            headers={'Content-Type': content_type},
            body=body,
        )

        resp_content_type = get_content_type(headers)

        if resp_content_type == 'text/html' or resp_content_type == 'application/xhtml+xml':
            try:
                text_content = content.decode('utf-8', errors='replace')
                rewritten = rewrite_html(text_content, final_url)
                content = rewritten.encode('utf-8')
                headers['content-type'] = 'text/html; charset=utf-8'
            except Exception:
                pass

        for h in ['content-encoding', 'transfer-encoding', 'content-length',
                  'content-security-policy', 'x-frame-options']:
            headers.pop(h, None)
            headers.pop(h.title(), None)

        return Response(
            content=content,
            status_code=status_code,
            headers={'Content-Type': headers.get('content-type', 'application/octet-stream')},
        )

    except Exception as e:
        return JSONResponse({"error": f"Failed to fetch URL: {str(e)}"}, status_code=502)


@app.websocket("/ws/terminal")
async def ws_terminal(websocket: WebSocket):
    session_id = websocket.cookies.get("jacklink_session")
    if not auth.check_session(session_id):
        await websocket.close(code=4001)
        return

    await websocket.accept()
    active_connections.add(websocket)
    session = TerminalSession()

    try:
        session.spawn()

        async def read_pty():
            loop = asyncio.get_event_loop()
            while session.running:
                try:
                    data = await loop.run_in_executor(None, session.read)
                    if data:
                        await websocket.send_bytes(data)
                except Exception:
                    break
                await asyncio.sleep(0.01)

        read_task = asyncio.create_task(read_pty())

        while True:
            try:
                message = await websocket.receive()
                if message["type"] == "websocket.disconnect":
                    break
                if "bytes" in message:
                    session.write(message["bytes"])
                elif "text" in message:
                    try:
                        msg = json.loads(message["text"])
                        if msg.get("type") == "resize":
                            cols = msg.get("cols", 80)
                            rows = msg.get("rows", 24)
                            session.resize(cols, rows)
                        elif msg.get("type") == "input":
                            data = msg.get("data", "")
                            session.write(data.encode())
                    except json.JSONDecodeError:
                        session.write(message["text"].encode())
            except WebSocketDisconnect:
                break

        read_task.cancel()
        try:
            await read_task
        except asyncio.CancelledError:
            pass
    finally:
        session.terminate()
        active_connections.discard(websocket)


@app.websocket("/ws/vnc")
async def ws_vnc(websocket: WebSocket):
    session_id = websocket.cookies.get("jacklink_session")
    if not auth.check_session(session_id):
        await websocket.close(code=4001)
        return

    vnc_port = detect_vnc()
    if not vnc_port:
        await websocket.close(code=4002)
        return

    await websocket.accept()
    active_connections.add(websocket)

    reader = None
    writer = None

    try:
        reader, writer = await asyncio.open_connection("127.0.0.1", vnc_port)

        async def vnc_to_ws():
            try:
                while True:
                    data = await reader.read(65536)
                    if not data:
                        break
                    await websocket.send_bytes(data)
            except Exception:
                pass

        async def ws_to_vnc():
            try:
                while True:
                    message = await websocket.receive()
                    if message["type"] == "websocket.disconnect":
                        break
                    if "bytes" in message:
                        writer.write(message["bytes"])
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
        active_connections.discard(websocket)


@app.get("/static/{path:path}")
async def serve_static(request: Request, path: str):
    file_path = STATIC_DIR / path
    if not file_path.is_file():
        raise HTTPException(status_code=404)
    return Response(
        content=file_path.read_bytes(),
        media_type=get_mime_type(file_path)
    )


def get_connection_count() -> int:
    return len(active_connections)
