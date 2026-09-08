#!/usr/bin/env python3
"""Explicit cookie-session handoff regressions."""
from __future__ import annotations

import json
import os
import sys
import tempfile
import types
from unittest import mock

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.abspath(os.path.join(HERE, "..", ".."))
sys.path.insert(0, ROOT)

from engine.session_input import load_cookie_file, redact_cookie_values  # noqa: E402
from engine.transport import SessionPool  # noqa: E402
import engine.executor as executor  # noqa: E402


def _write(text: str) -> str:
    handle = tempfile.NamedTemporaryFile("w", encoding="utf-8", delete=False)
    try:
        handle.write(text)
        return handle.name
    finally:
        handle.close()


def t_json_cookie_export_is_scoped_to_target_host() -> None:
    path = _write(json.dumps({"cookies": [
        {"name": "sid", "value": "one", "domain": ".example.com", "path": "/"},
        {"name": "other", "value": "two", "domain": "unrelated.test", "path": "/"},
    ]}))
    try:
        cookies = load_cookie_file(path, "https://news.example.com/article")
    finally:
        os.unlink(path)
    assert cookies == [{
        "name": "sid", "value": "one", "domain": ".example.com",
        "path": "/", "secure": False,
    }]


def t_netscape_cookie_file_is_supported() -> None:
    path = _write(
        "# Netscape HTTP Cookie File\n"
        ".example.com\tTRUE\t/\tTRUE\t2147483647\tsid\tsecret\n")
    try:
        cookies = load_cookie_file(path, "https://example.com/")
    finally:
        os.unlink(path)
    assert cookies[0]["name"] == "sid"
    assert cookies[0]["secure"] is True
    assert cookies[0]["expires"] == 2147483647


def t_unrelated_only_cookie_file_is_rejected() -> None:
    path = _write(json.dumps([
        {"name": "sid", "value": "secret", "domain": "unrelated.test"},
    ]))
    try:
        try:
            load_cookie_file(path, "https://example.com/")
        except ValueError as exc:
            assert "target host" in str(exc)
        else:
            raise AssertionError("unrelated cookies must not be accepted")
    finally:
        os.unlink(path)


def t_cookie_value_with_line_break_is_rejected() -> None:
    path = _write(json.dumps([
        {"name": "sid", "value": "secret\nleak", "domain": "example.com"},
    ]))
    try:
        try:
            load_cookie_file(path, "https://example.com/")
        except ValueError as exc:
            assert "control" in str(exc)
        else:
            raise AssertionError("control characters must be rejected")
    finally:
        os.unlink(path)


def t_cookie_seed_applies_to_future_tls_identities_without_disk_persistence() -> None:
    recorded = []

    class _Cookies:
        def set(self, name, value, **kwargs):
            recorded.append((name, value, kwargs))

    class _Session:
        def __init__(self, **_kwargs):
            self.cookies = _Cookies()

    fake = types.SimpleNamespace(requests=types.SimpleNamespace(Session=_Session))
    pool = SessionPool()
    pool.seed_cookies("example.com", [{
        "name": "sid", "value": "secret", "domain": ".example.com", "path": "/",
    }])
    with mock.patch.dict(sys.modules, {"curl_cffi": fake}):
        assert pool.get("example.com", "safari") is not None
    assert recorded == [("sid", "secret", {"domain": ".example.com", "path": "/"})]
    assert pool.stats()["sessions"] == 1
    pool.reset()
    assert pool._seeds == {}


def t_browser_cookie_handoff_uses_an_ephemeral_profile() -> None:
    captured = {}
    cookies = [{
        "name": "sid", "value": "secret", "domain": "example.com", "path": "/",
        "secure": True,
    }]

    def fake_run(_template, args, **_kwargs):
        captured.update(args)
        html = "<html><body>" + ("real article text " * 400) + "</body></html>"
        return 0, json.dumps({
            "html": html, "finalUrl": "https://example.com/", "status": 200,
            "cookies": [], "userAgent": "", "automation": "test", "innerText": "",
        }), ""

    with mock.patch.object(executor, "_resolve_node_deps", return_value=ROOT), \
            mock.patch.object(executor, "_run_node_template", side_effect=fake_run):
        attempt, content = executor.run_playwright_fallback(
            "https://example.com/", profile_id="unknown_challenge",
            force_executor="playwright_real_chrome", cookies=cookies)
    assert attempt.status == 200 and content
    assert captured["cookies"] == cookies
    assert not os.path.exists(captured["profileDir"])


def t_cookie_secrets_are_removed_from_diagnostics() -> None:
    text = redact_cookie_values(
        "failed with sid=super-secret", [{"name": "sid", "value": "super-secret"}])
    assert text == "failed with ***=***"


def main() -> int:
    tests = [v for k, v in sorted(globals().items()) if k.startswith("t_") and callable(v)]
    failed = 0
    for test in tests:
        try:
            test()
        except AssertionError as exc:
            print(f"  x {test.__name__}: {exc}")
            failed += 1
    print(f"\n{'OK' if failed == 0 else 'FAIL'}: {len(tests) - failed}/{len(tests)} passed")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
