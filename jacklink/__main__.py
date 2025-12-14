# JackLink - Portable Computing Gateway
# Copyright (c) 2025 Konstantin Zaremski
# Licensed under the MIT License

import sys
import os
import subprocess
from pathlib import Path

REQUIRED_PACKAGES = [
    "fastapi",
    "uvicorn",
    "wsgidav",
    "python-multipart",
    "aiofiles",
    "rich",
    "websockets",
    "httpx",
]

VENV_DIR = Path.home() / ".jacklink-venv"


def get_package_dir() -> Path:
    return Path(__file__).parent.parent


def is_in_venv() -> bool:
    return sys.prefix != sys.base_prefix


def check_dependencies() -> list[str]:
    missing = []
    for package in REQUIRED_PACKAGES:
        pkg_name = package.replace("-", "_")
        try:
            __import__(pkg_name)
        except ImportError:
            missing.append(package)
    return missing


def create_venv() -> bool:
    print(f"[JackLink] Creating virtual environment at {VENV_DIR}...", flush=True)
    try:
        subprocess.run(
            [sys.executable, "-m", "venv", str(VENV_DIR)],
            check=True,
            capture_output=True
        )
        return True
    except subprocess.CalledProcessError as e:
        print(f"[JackLink] Failed to create venv: {e}")
        return False


def install_dependencies(pip_path: Path = None) -> bool:
    if pip_path is None:
        pip_path = VENV_DIR / "bin" / "pip"
    requirements_file = get_package_dir() / "requirements.txt"

    print("[JackLink] Installing dependencies...", flush=True)
    try:
        subprocess.run(
            [str(pip_path), "install", "-q", "-r", str(requirements_file)],
            check=True
        )
        return True
    except subprocess.CalledProcessError as e:
        print(f"[JackLink] Failed to install dependencies: {e}")
        return False


def install_missing_in_venv(missing: list[str]) -> bool:
    """Install missing packages in the current venv."""
    print(f"[JackLink] Installing missing packages: {', '.join(missing)}", flush=True)
    try:
        subprocess.run(
            [sys.executable, "-m", "pip", "install", "-q"] + missing,
            check=True
        )
        return True
    except subprocess.CalledProcessError as e:
        print(f"[JackLink] Failed to install packages: {e}")
        return False


def reexec_in_venv():
    python_path = VENV_DIR / "bin" / "python"
    os.execv(str(python_path), [str(python_path), "-m", "jacklink"] + sys.argv[1:])


def prompt_install() -> bool:
    try:
        response = input("[JackLink] Install missing dependencies? [Y/n]: ").strip().lower()
        return response in ("", "y", "yes")
    except (EOFError, KeyboardInterrupt):
        return False


def bootstrap():
    if is_in_venv():
        missing = check_dependencies()
        if missing:
            print(f"[JackLink] Missing packages: {', '.join(missing)}")
            if not install_missing_in_venv(missing):
                print("[JackLink] Please install dependencies manually: pip install -r requirements.txt")
                sys.exit(1)
            print("[JackLink] Packages installed successfully.", flush=True)
        from .tui import run
        run()
        return

    if VENV_DIR.exists():
        python_path = VENV_DIR / "bin" / "python"
        if python_path.exists():
            print(f"[JackLink] Using venv at {VENV_DIR}", flush=True)
            reexec_in_venv()

    missing = check_dependencies()
    if not missing:
        from .tui import run
        run()
        return

    print("[JackLink] Missing dependencies detected:", flush=True)
    for pkg in missing:
        print(f"  - {pkg}", flush=True)
    print(flush=True)

    if not sys.stdin.isatty():
        print("[JackLink] Non-interactive mode. Creating venv and installing...", flush=True)
        should_install = True
    else:
        should_install = prompt_install()

    if not should_install:
        print("[JackLink] Aborted. Install dependencies manually:")
        print(f"  pip install -r {get_package_dir() / 'requirements.txt'}")
        sys.exit(1)

    if not VENV_DIR.exists():
        if not create_venv():
            sys.exit(1)

    if not install_dependencies():
        sys.exit(1)

    print("[JackLink] Dependencies installed. Starting...", flush=True)
    reexec_in_venv()


if __name__ == "__main__":
    bootstrap()
