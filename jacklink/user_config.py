# JackLink - Portable Computing Gateway
# Copyright (c) 2025 Konstantin Zaremski
# Licensed under the MIT License

import json
from dataclasses import dataclass, field, asdict
from pathlib import Path
from typing import Optional


CONFIG_DIR = Path.home() / ".config" / "jacklink"
CONFIG_FILE = CONFIG_DIR / "config.json"


@dataclass
class UserConfig:
    username: str = "jacklink"
    wallpaper: Optional[str] = None  # Path to wallpaper image
    wallpaper_color: Optional[str] = None  # Hex color for solid background

    def save(self):
        """Save config to file."""
        CONFIG_DIR.mkdir(parents=True, exist_ok=True)
        CONFIG_FILE.write_text(json.dumps(asdict(self), indent=2))

    @classmethod
    def load(cls) -> "UserConfig":
        """Load config from file, or return defaults."""
        if CONFIG_FILE.exists():
            try:
                data = json.loads(CONFIG_FILE.read_text())
                return cls(
                    username=data.get("username", "jacklink"),
                    wallpaper=data.get("wallpaper"),
                    wallpaper_color=data.get("wallpaper_color"),
                )
            except Exception:
                pass
        return cls()


user_config = UserConfig.load()
