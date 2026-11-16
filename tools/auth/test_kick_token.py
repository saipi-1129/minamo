import json
import ssl
import unittest
import urllib.parse
import urllib.request
from unittest.mock import patch

import kick_token


class _Response:
    def __init__(self, payload: bytes, status: int = 200):
        self.payload = payload
        self.status = status

    def __enter__(self):
        return self

    def __exit__(self, *_args):
        return False

    def getcode(self):
        return self.status

    def read(self, size: int):
        return self.payload[:size]


class _Opener:
    def __init__(self, response=None, error=None):
        self.response = response
        self.error = error
        self.request = None
        self.timeout = None

    def open(self, request, timeout):
        self.request = request
        self.timeout = timeout
        if self.error is not None:
            raise self.error
        return self.response


class FetchTokenTests(unittest.TestCase):
    def test_posts_form_encoded_credentials_to_fixed_https_endpoint(self):
        opener = _Opener(_Response(b'{"access_token":"opaque","token_type":"Bearer"}'))
        with patch.object(kick_token, "_build_opener", return_value=opener):
            token = kick_token.fetch_app_access_token("client id", "s&cret")

        self.assertEqual(token, "opaque")
        self.assertEqual(opener.request.full_url, kick_token.TOKEN_URL)
        self.assertTrue(opener.request.full_url.startswith("https://"))
        self.assertEqual(
            opener.request.get_header("Content-type"),
            "application/x-www-form-urlencoded",
        )
        self.assertEqual(
            urllib.parse.parse_qs(opener.request.data.decode("ascii")),
            {
                "grant_type": ["client_credentials"],
                "client_id": ["client id"],
                "client_secret": ["s&cret"],
            },
        )
        self.assertEqual(opener.timeout, kick_token.REQUEST_TIMEOUT_SECONDS)

    def test_opener_requires_verified_tls_and_rejects_redirects(self):
        captured_handlers = []

        def capture(*handlers):
            captured_handlers.extend(handlers)
            return object()

        with patch.object(urllib.request, "build_opener", side_effect=capture):
            kick_token._build_opener()

        https = next(
            handler
            for handler in captured_handlers
            if isinstance(handler, urllib.request.HTTPSHandler)
        )
        self.assertIsInstance(https._context, ssl.SSLContext)
        self.assertTrue(https._context.check_hostname)
        self.assertEqual(https._context.verify_mode, ssl.CERT_REQUIRED)
        redirect = next(
            handler
            for handler in captured_handlers
            if isinstance(handler, urllib.request.HTTPRedirectHandler)
        )
        self.assertIsNone(
            redirect.redirect_request(None, None, 302, "Found", {}, "https://other.example/")
        )

    def test_token_endpoint_errors_are_generic_and_do_not_echo_response(self):
        secret = "private-secret-value"
        opener = _Opener(error=OSError(secret))
        with patch.object(kick_token, "_build_opener", return_value=opener):
            with self.assertRaises(kick_token.AuthError) as caught:
                kick_token.fetch_app_access_token("id", secret)
        self.assertNotIn(secret, str(caught.exception))
        self.assertEqual(str(caught.exception), "Kick token request failed.")

    def test_rejects_malformed_or_non_bearer_token_response(self):
        responses = [
            b"not-json",
            b"[]",
            json.dumps({"access_token": "x", "token_type": "MAC"}).encode(),
            json.dumps({"access_token": "", "token_type": "Bearer"}).encode(),
            json.dumps({"access_token": "bad\nvalue", "token_type": "Bearer"}).encode(),
        ]
        for payload in responses:
            with self.subTest(payload=payload):
                opener = _Opener(_Response(payload))
                with patch.object(kick_token, "_build_opener", return_value=opener):
                    with self.assertRaises(kick_token.AuthError):
                        kick_token.fetch_app_access_token("id", "secret")


class ChildProcessTests(unittest.TestCase):
    def test_token_is_only_in_child_environment_not_argv(self):
        token = "private-access-token"
        child_env_snapshots = []

        def fake_run(argv, *, env, **_kwargs):
            child_env_snapshots.append(env.copy())
            self.assertNotIn(token, argv)
            return type("Result", (), {"returncode": 0})()

        with (
            patch.dict(
                "os.environ",
                {"KICK_CLIENT_ID": "do-not-forward-id", "KICK_CLIENT_SECRET": "do-not-forward"},
            ),
            patch.object(kick_token, "REPO_ROOT", kick_token.Path("/workspace")),
            patch.object(kick_token.Path, "is_file", return_value=True),
            patch.object(kick_token.subprocess, "run", side_effect=fake_run) as run,
        ):
            result = kick_token.run_local_cli(token, limit=7, release=False)

        self.assertEqual(result, 0)
        argv = run.call_args.args[0]
        self.assertEqual(argv[-2:], ["--limit", "7"])
        self.assertNotIn(token, argv)
        self.assertEqual(child_env_snapshots[0]["KICK_ACCESS_TOKEN"], token)
        self.assertNotIn("KICK_CLIENT_ID", child_env_snapshots[0])
        self.assertNotIn("KICK_CLIENT_SECRET", child_env_snapshots[0])
        self.assertNotIn("KICK_ACCESS_TOKEN", run.call_args.kwargs["env"])


if __name__ == "__main__":
    unittest.main()
