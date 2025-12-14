# JackLink - Portable Computing Gateway
# Copyright (c) 2025 Konstantin Zaremski
# Licensed under the MIT License

from pathlib import Path
from wsgidav.wsgidav_app import WsgiDAVApp
from wsgidav.dc.simple_dc import SimpleDomainController


class JackLinkDomainController(SimpleDomainController):
    def __init__(self, auth_instance):
        self.auth = auth_instance
        super().__init__({"simple_dc": {"user_mapping": {"*": True}}})

    def get_domain_realm(self, path_info, environ):
        return "JackLink"

    def require_authentication(self, realm, environ):
        return True

    def basic_auth_user(self, realm, user_name, password, environ):
        if user_name == self.auth.username and self.auth.validate_token(password):
            return True
        return False

    def supports_http_digest_auth(self):
        return False


def create_webdav_app(root_path: Path, auth_instance) -> WsgiDAVApp:
    config = {
        "provider_mapping": {
            "/": str(root_path),
        },
        "simple_dc": {
            "user_mapping": {
                "*": True,
            },
        },
        "verbose": 0,
        "logging": {
            "enable": False,
        },
        "http_authenticator": {
            "domain_controller": lambda: JackLinkDomainController(auth_instance),
            "accept_basic": True,
            "accept_digest": False,
            "default_to_digest": False,
        },
        "dir_browser": {
            "enable": True,
            "response_trailer": "",
        },
    }
    return WsgiDAVApp(config)
