# JackLink - Portable Computing Gateway
# Copyright (c) 2025 Konstantin Zaremski
# Licensed under the MIT License

import secrets
import base64
from typing import Optional

from .user_config import user_config


class JackLinkAuth:
    def __init__(self):
        self.token = self.generate_token()
        self.sessions: set[str] = set()

    @property
    def username(self) -> str:
        return user_config.username

    def generate_token(self) -> str:
        return secrets.token_urlsafe(6)

    def regenerate_token(self) -> str:
        self.token = self.generate_token()
        self.sessions.clear()
        return self.token

    def validate_token(self, token: str) -> bool:
        return secrets.compare_digest(token, self.token)

    def create_session(self) -> str:
        session_id = secrets.token_urlsafe(32)
        self.sessions.add(session_id)
        return session_id

    def check_session(self, session_id: Optional[str]) -> bool:
        if session_id is None:
            return False
        return session_id in self.sessions

    def check_basic_auth(self, authorization: Optional[str]) -> bool:
        if authorization is None:
            return False
        if not authorization.startswith("Basic "):
            return False
        try:
            encoded = authorization[6:]
            decoded = base64.b64decode(encoded).decode("utf-8")
            username, password = decoded.split(":", 1)
            return username == self.username and self.validate_token(password)
        except Exception:
            return False


auth = JackLinkAuth()
