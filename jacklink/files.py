# JackLink - Portable Computing Gateway
# Copyright (c) 2025 Konstantin Zaremski
# Licensed under the MIT License

import os
import aiofiles
import aiofiles.os
from pathlib import Path
from typing import Any


async def list_directory(path: Path) -> list[dict[str, Any]]:
    items = []
    try:
        entries = await aiofiles.os.listdir(path)
        for entry in sorted(entries):
            entry_path = path / entry
            try:
                stat = await aiofiles.os.stat(entry_path)
                is_dir = await aiofiles.os.path.isdir(entry_path)
                items.append({
                    "name": entry,
                    "path": str(entry_path),
                    "is_dir": is_dir,
                    "size": stat.st_size if not is_dir else 0,
                    "modified": stat.st_mtime,
                })
            except (PermissionError, OSError):
                items.append({
                    "name": entry,
                    "path": str(entry_path),
                    "is_dir": False,
                    "size": 0,
                    "modified": 0,
                    "error": True,
                })
    except (PermissionError, OSError) as e:
        raise PermissionError(f"Cannot access directory: {e}")
    return items


async def read_file(path: Path) -> str:
    try:
        async with aiofiles.open(path, "r", encoding="utf-8", errors="replace") as f:
            return await f.read()
    except (PermissionError, OSError) as e:
        raise PermissionError(f"Cannot read file: {e}")


async def read_file_bytes(path: Path) -> bytes:
    try:
        async with aiofiles.open(path, "rb") as f:
            return await f.read()
    except (PermissionError, OSError) as e:
        raise PermissionError(f"Cannot read file: {e}")


async def write_file(path: Path, content: str) -> None:
    try:
        async with aiofiles.open(path, "w", encoding="utf-8") as f:
            await f.write(content)
    except (PermissionError, OSError) as e:
        raise PermissionError(f"Cannot write file: {e}")


async def save_upload(path: Path, content: bytes) -> None:
    try:
        async with aiofiles.open(path, "wb") as f:
            await f.write(content)
    except (PermissionError, OSError) as e:
        raise PermissionError(f"Cannot save file: {e}")


def get_mime_type(path: Path) -> str:
    ext = path.suffix.lower()
    mime_types = {
        ".html": "text/html",
        ".css": "text/css",
        ".js": "application/javascript",
        ".json": "application/json",
        ".png": "image/png",
        ".jpg": "image/jpeg",
        ".jpeg": "image/jpeg",
        ".gif": "image/gif",
        ".svg": "image/svg+xml",
        ".webp": "image/webp",
        ".ico": "image/x-icon",
        ".txt": "text/plain",
        ".md": "text/markdown",
        ".py": "text/x-python",
        ".sh": "text/x-shellscript",
        ".pdf": "application/pdf",
        ".zip": "application/zip",
        ".tar": "application/x-tar",
        ".gz": "application/gzip",
    }
    return mime_types.get(ext, "application/octet-stream")


def is_image(path: Path) -> bool:
    return path.suffix.lower() in {".png", ".jpg", ".jpeg", ".gif", ".svg", ".webp", ".bmp", ".ico"}


def is_text(path: Path) -> bool:
    text_extensions = {
        ".txt", ".md", ".py", ".js", ".ts", ".jsx", ".tsx", ".html", ".css",
        ".json", ".xml", ".yaml", ".yml", ".toml", ".ini", ".cfg", ".conf",
        ".sh", ".bash", ".zsh", ".fish", ".c", ".cpp", ".h", ".hpp", ".java",
        ".go", ".rs", ".rb", ".php", ".sql", ".lua", ".vim", ".el", ".clj",
        ".hs", ".ml", ".scala", ".kt", ".swift", ".r", ".jl", ".ex", ".exs",
        ".erl", ".hrl", ".lisp", ".scm", ".rkt", ".pl", ".pm", ".t", ".awk",
        ".sed", ".makefile", ".cmake", ".dockerfile", ".gitignore", ".env",
        ".log", ".csv", ".tsv",
    }
    return path.suffix.lower() in text_extensions or path.name.lower() in {
        "makefile", "dockerfile", "vagrantfile", "gemfile", "rakefile",
        "cmakelists.txt", ".gitignore", ".dockerignore", ".editorconfig",
    }
