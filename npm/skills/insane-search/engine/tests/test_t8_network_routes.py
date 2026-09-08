#!/usr/bin/env python3
"""Regression tests for network diagnosis, proxy routing, and local stealth fallback."""
from __future__ import annotations

import os
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.abspath(os.path.join(HERE, "..", ".."))
sys.path.insert(0, ROOT)

import engine.executor as ex  # noqa: E402
import engine.fetch_chain as fc  # noqa: E402
from engine.fetch_chain import Attempt, FetchResult  # noqa: E402
from engine.network import (  # noqa: E402
    diagnose_trace,
    normalize_proxy,
    proxy_cache_key,
    proxy_for_browser,
    redact_proxy,
)
from engine.transport import SessionPool  # noqa: E402
from engine.validators import Verdict  # noqa: E402
from engine.waf_detector import _load_profiles  # noqa: E402


def t_proxy_validation_and_redaction() -> None:
    proxy = normalize_proxy("socks5h://user:p%40ss@proxy.example:1080")
    assert proxy == "socks5h://user:p%40ss@proxy.example:1080"
    browser = proxy_for_browser(proxy)
    assert browser == {
        "server": "socks5://proxy.example:1080",
        "username": "user",
        "password": "p@ss",
    }
    assert "user" not in proxy_cache_key(proxy)
    assert "p%40ss" not in proxy_cache_key(proxy)
    assert redact_proxy(f"failed via {proxy}", proxy) == "failed via socks5://proxy.example:1080"

    for bad in (
        "ftp://proxy.example:21",
        "https://proxy.example/path",
        "https://proxy.example:bad",
        "proxy.example:8080",
    ):
        try:
            normalize_proxy(bad)
        except ValueError:
            pass
        else:
            raise AssertionError(f"invalid proxy accepted: {bad}")


def t_session_pool_separates_egress_routes() -> None:
    pool = SessionPool()
    direct = pool.get("www.example.com", "safari")
    proxied = pool.get("www.example.com", "safari", "http://proxy.example:8080")
    if direct is None or proxied is None:
        return
    assert direct is not proxied
    assert pool.get("www.example.com", "safari", "http://proxy.example:8080") is proxied


def t_trace_diagnosis_finds_the_failed_layer() -> None:
    tls = Attempt(
        phase="probe", executor="curl_cffi", url="https://www.example.com/",
        url_transform="original", impersonate="safari", referer="self_root",
        error="CertificateVerifyError: certificate verify failed",
    )
    report = diagnose_trace([tls])
    assert report["layer"] == "tls"
    assert report["category"] == "certificate_verification"

    dns = Attempt(
        phase="probe", executor="curl_cffi", url="https://www.example.com/",
        url_transform="original", impersonate="safari", referer="self_root",
        error="Could not resolve host",
    )
    assert diagnose_trace([dns])["layer"] == "dns"

    blocked = Attempt(
        phase="probe", executor="curl_cffi", url="https://www.example.com/",
        url_transform="original", impersonate="safari", referer="self_root",
        status=403, verdict=Verdict.BLOCKED.value,
    )
    assert diagnose_trace([blocked])["layer"] == "http_policy"


def t_fetch_passes_proxy_without_exposing_it() -> None:
    saved = fc._fetch_core
    seen = {}

    def fake_core(url, **kwargs):
        seen.update(kwargs)
        return FetchResult(ok=False, final_url=url, stop_reason="exhausted")

    fc._fetch_core = fake_core
    previous_observations = os.environ.get("INSANE_OBSERVATIONS_DIR")
    try:
        with tempfile.TemporaryDirectory() as temp_dir:
            os.environ["INSANE_OBSERVATIONS_DIR"] = temp_dir
            result = fc.fetch(
                "https://www.example.com/",
                proxy="http://user:secret@proxy.example:8080",
                enable_learning=False,
            )
    finally:
        fc._fetch_core = saved
        if previous_observations is None:
            os.environ.pop("INSANE_OBSERVATIONS_DIR", None)
        else:
            os.environ["INSANE_OBSERVATIONS_DIR"] = previous_observations

    assert seen["proxy"] == "http://user:secret@proxy.example:8080"
    assert result.proxy_used is True
    assert "secret" not in str(result.to_dict())


def t_local_stealth_firefox_receives_browser_proxy() -> None:
    saved_available = ex._module_available
    saved_template = ex._run_python_template
    captured = {}

    ex._module_available = lambda name: name == "invisible_playwright"

    def fake_template(template, args, timeout=90):
        captured.update(args)
        html = "<html><body><article>" + ("x" * 4000) + "</article></body></html>"
        return 0, html, ""

    ex._run_python_template = fake_template
    try:
        attempt, content = ex.run_playwright_fallback(
            "https://www.example.com/",
            profile_id="unknown_challenge",
            force_executor="stealth_firefox",
            proxy="http://user:secret@proxy.example:8080",
        )
    finally:
        ex._module_available = saved_available
        ex._run_python_template = saved_template

    assert attempt.verdict == Verdict.WEAK_OK.value
    assert content
    assert attempt.executor == "stealth_firefox:invisible_playwright"
    assert captured["proxy"] == {
        "server": "http://proxy.example:8080",
        "username": "user",
        "password": "secret",
    }
    assert captured["headless"] is True


def t_scrapling_is_the_primary_hidden_cloudflare_fallback() -> None:
    profile = _load_profiles()["cloudflare_turnstile"]
    assert profile["fallback_when_challenge"][:2] == ["scrapling", "stealth_firefox"]

    saved_available = ex._module_available
    saved_template = ex._run_python_template
    captured = {}
    ex._module_available = lambda name: name == "scrapling"

    def fake_template(template, args, timeout=90):
        captured.update(args)
        html = "<html><body><main>" + ("x" * 4000) + "</main></body></html>"
        return 0, html, ""

    ex._run_python_template = fake_template
    try:
        attempt, content = ex.run_playwright_fallback(
            "https://www.example.com/",
            profile_id="cloudflare_turnstile",
            force_executor="scrapling",
            proxy="http://user:secret@proxy.example:8080",
        )
    finally:
        ex._module_available = saved_available
        ex._run_python_template = saved_template

    assert attempt.verdict == Verdict.WEAK_OK.value
    assert content
    assert attempt.executor == "scrapling:stealthy_fetcher"
    assert captured["headless"] is True
    assert captured["realChrome"] is True
    assert captured["solveCloudflare"] is True
    assert captured["blockAds"] is True
    assert captured["proxy"]["server"] == "http://proxy.example:8080"


def main() -> int:
    tests = [value for name, value in sorted(globals().items())
             if name.startswith("t_") and callable(value)]
    failed = 0
    for test in tests:
        try:
            test()
            print(f"  PASS {test.__name__}")
        except Exception as exc:
            failed += 1
            print(f"  FAIL {test.__name__}: {type(exc).__name__}: {exc}")
    print(f"\n{'OK' if failed == 0 else 'FAIL'}: {len(tests) - failed}/{len(tests)} passed")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
