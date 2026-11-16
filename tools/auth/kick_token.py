#!/usr/bin/env python3
"""Fetch a Kick App Access Token and pass it only to the local kick-live CLI."""

from __future__ import annotations

import argparse
import getpass
import json
import os
import ssl
import subprocess
import sys
import urllib.parse
import urllib.request
import warnings
from pathlib import Path
from typing import Sequence


TOKEN_URL = "https://id.kick.com/oauth/token"
REQUEST_TIMEOUT_SECONDS = 15
MAX_RESPONSE_BYTES = 64 * 1024
MAX_CLIENT_ID_LENGTH = 512
MAX_TOKEN_LENGTH = 16 * 1024
MAX_LIMIT = 25
REPO_ROOT = Path(__file__).resolve().parents[2]


class AuthError(Exception):
    """An error safe to display without exposing request or response details."""


class _NoRedirect(urllib.request.HTTPRedirectHandler):
    """Reject redirects so the client secret cannot be sent to another host."""

    def redirect_request(self, request, file, code, message, headers, new_url):
        return None


def _build_opener():
    tls_context = ssl.create_default_context()
    tls_context.check_hostname = True
    tls_context.verify_mode = ssl.CERT_REQUIRED
    return urllib.request.build_opener(
        urllib.request.HTTPSHandler(context=tls_context), _NoRedirect()
    )


def fetch_app_access_token(client_id: str, client_secret: str) -> str:
    """Request a token using Kick's documented client_credentials grant."""
    if not client_id or len(client_id) > MAX_CLIENT_ID_LENGTH:
        raise AuthError("Kick Client ID is missing or invalid.")
    if not client_secret or not client_secret.strip():
        raise AuthError("Kick Client Secret is missing or invalid.")

    body = urllib.parse.urlencode(
        {
            "grant_type": "client_credentials",
            "client_id": client_id,
            "client_secret": client_secret,
        }
    ).encode("ascii")
    request = urllib.request.Request(
        TOKEN_URL,
        data=body,
        headers={"Content-Type": "application/x-www-form-urlencoded"},
        method="POST",
    )

    try:
        with _build_opener().open(
            request, timeout=REQUEST_TIMEOUT_SECONDS
        ) as response:
            if response.getcode() != 200:
                raise AuthError("Kick token request failed.")
            payload = response.read(MAX_RESPONSE_BYTES + 1)
    except AuthError:
        raise
    except Exception:
        raise AuthError("Kick token request failed.") from None

    if len(payload) > MAX_RESPONSE_BYTES:
        raise AuthError("Kick token response was invalid.")
    try:
        decoded = json.loads(payload)
    except (UnicodeDecodeError, json.JSONDecodeError):
        raise AuthError("Kick token response was invalid.") from None
    if not isinstance(decoded, dict):
        raise AuthError("Kick token response was invalid.")

    token = decoded.get("access_token")
    token_type = decoded.get("token_type")
    if (
        not isinstance(token, str)
        or not token.strip()
        or len(token) > MAX_TOKEN_LENGTH
        or not token.isascii()
        or any(not (0x21 <= ord(character) <= 0x7E) for character in token)
        or not isinstance(token_type, str)
        or token_type.casefold() != "bearer"
    ):
        raise AuthError("Kick token response was invalid.")
    return token


def run_local_cli(access_token: str, *, limit: int, release: bool) -> int:
    """Run the already-built CLI with a short-lived token environment value."""
    build_type = "release" if release else "debug"
    executable = REPO_ROOT / "target" / build_type / "kick-live"
    if not executable.is_file():
        raise AuthError(
            "Local kick-live CLI is not built; build it with Cargo first."
        )

    child_env = os.environ.copy()
    child_env.pop("KICK_CLIENT_ID", None)
    child_env.pop("KICK_CLIENT_SECRET", None)
    child_env["KICK_ACCESS_TOKEN"] = access_token
    try:
        try:
            result = subprocess.run(
                [str(executable), "--limit", str(limit)],
                env=child_env,
                check=False,
                close_fds=True,
            )
        except OSError:
            raise AuthError("Could not start the local kick-live CLI.") from None
        return result.returncode
    finally:
        child_env.pop("KICK_ACCESS_TOKEN", None)


def _parse_args(argv: Sequence[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=(
            "Fetch a Kick App Access Token interactively and run the local CLI."
        ),
        allow_abbrev=False,
    )
    parser.add_argument(
        "--limit",
        type=int,
        default=5,
        choices=range(1, MAX_LIMIT + 1),
        help="maximum number of live streams to request (1–25; default: 5)",
    )
    parser.add_argument(
        "--release",
        action="store_true",
        help="run target/release/kick-live instead of target/debug/kick-live",
    )
    return parser.parse_args(argv)


def main(argv: Sequence[str] | None = None) -> int:
    try:
        args = _parse_args(argv)
        client_id = input("Kick Client ID: ").strip()
        if not client_id or len(client_id) > MAX_CLIENT_ID_LENGTH:
            raise AuthError("Kick Client ID is missing or invalid.")

        # Treat getpass's no-controlling-terminal warning as an error, so its
        # echoing stdin fallback can never reveal the secret.
        with warnings.catch_warnings():
            warnings.simplefilter("error", getpass.GetPassWarning)
            client_secret = getpass.getpass("Kick Client Secret: ")
        token = fetch_app_access_token(client_id, client_secret)
        del client_secret
        del client_id
        return run_local_cli(token, limit=args.limit, release=args.release)
    except AuthError as error:
        print(f"kick-token: {error}", file=sys.stderr)
        return 1
    except (EOFError, getpass.GetPassWarning):
        print("kick-token: Interactive credential input is unavailable.", file=sys.stderr)
        return 1
    except KeyboardInterrupt:
        print("kick-token: Cancelled.", file=sys.stderr)
        return 130


if __name__ == "__main__":
    raise SystemExit(main())
