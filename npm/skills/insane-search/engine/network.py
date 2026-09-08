"""Network-route validation and failure-layer diagnosis."""
from __future__ import annotations

import hashlib
from typing import Any, Optional
from urllib.parse import unquote, urlsplit


_PROXY_SCHEMES = frozenset({"http", "https", "socks4", "socks4a", "socks5", "socks5h"})


def normalize_proxy(value: Optional[str]) -> Optional[str]:
    """Validate an explicitly supplied proxy URL and return its trimmed form."""
    if value is None:
        return None
    value = value.strip()
    if not value:
        return None
    if any(ord(char) < 0x20 for char in value):
        raise ValueError("proxy URL contains control characters")
    try:
        parsed = urlsplit(value)
        port = parsed.port
    except ValueError as exc:
        raise ValueError("proxy URL has an invalid port") from exc
    if parsed.scheme.lower() not in _PROXY_SCHEMES:
        raise ValueError("proxy URL must use HTTP, HTTPS, SOCKS4, or SOCKS5")
    if not parsed.hostname or port is None:
        raise ValueError("proxy URL must include a host and port")
    if parsed.path not in ("", "/") or parsed.query or parsed.fragment:
        raise ValueError("proxy URL cannot contain a path, query, or fragment")
    credentials = "".join(unquote(part or "") for part in (parsed.username, parsed.password))
    if any(ord(char) < 0x20 for char in credentials):
        raise ValueError("proxy credentials contain control characters")
    return value


def proxy_cache_key(proxy: Optional[str]) -> str:
    """Opaque pool key: credentials never appear in diagnostics or statistics."""
    if not proxy:
        return "direct"
    return hashlib.sha256(proxy.encode("utf-8", "surrogatepass")).hexdigest()[:20]


def proxy_for_browser(proxy: Optional[str]) -> Optional[dict[str, str]]:
    """Convert a validated URL to Playwright's proxy object."""
    proxy = normalize_proxy(proxy)
    if not proxy:
        return None
    parsed = urlsplit(proxy)
    host = parsed.hostname or ""
    if ":" in host:
        host = f"[{host}]"
    browser_scheme = {"socks5h": "socks5", "socks4a": "socks4"}.get(
        parsed.scheme.lower(), parsed.scheme.lower())
    result = {"server": f"{browser_scheme}://{host}:{parsed.port}"}
    if parsed.username is not None:
        result["username"] = unquote(parsed.username)
    if parsed.password is not None:
        result["password"] = unquote(parsed.password)
    return result


def redact_proxy(text: Optional[str], proxy: Optional[str]) -> Optional[str]:
    """Remove a proxy URL and either encoded or decoded credentials from errors."""
    if text is None or not proxy:
        return text
    parsed = urlsplit(proxy)
    public = proxy_for_browser(proxy)["server"]
    redacted = text.replace(proxy, public)
    for value in (parsed.username, parsed.password):
        if not value:
            continue
        redacted = redacted.replace(value, "***")
        redacted = redacted.replace(unquote(value), "***")
    return redacted


def diagnose_trace(trace: list[Any]) -> dict[str, Any]:
    """Infer the failed network layer from already collected attempts."""
    errors = [str(getattr(attempt, "error", "") or "").lower() for attempt in trace]
    statuses = [int(getattr(attempt, "status", 0) or 0) for attempt in trace]
    verdicts = [str(getattr(attempt, "verdict", "") or "").lower() for attempt in trace]

    if any(token in error for error in errors for token in (
            "certificateverifyerror", "certificate verify", "ssl certificate", "curl: (60)")):
        return _diagnosis("tls", "certificate_verification", "fix_ca_or_use_system_browser")
    if any(token in error for error in errors for token in (
            "could not resolve", "name or service not known", "nodename nor servname", "gaierror")):
        return _diagnosis("dns", "resolution_failed", "check_dns_or_use_remote_dns_proxy")
    if any(token in error for error in errors for token in (
            "connection refused", "connection reset", "network is unreachable", "no route to host")):
        return _diagnosis("tcp", "connection_failed", "check_firewall_or_use_approved_proxy")
    if any(token in error for error in errors for token in ("timed out", "timeout")):
        return _diagnosis("transport", "timeout", "check_route_or_retry_with_backoff")
    if 429 in statuses or "rate_limited" in verdicts:
        return _diagnosis("http_policy", "rate_limited", "backoff_before_retry")
    if any(status in (401, 407) for status in statuses) or "auth_required" in verdicts:
        return _diagnosis("authentication", "credentials_required", "request_user_authentication")
    if 403 in statuses or any(verdict in ("blocked", "challenge", "suspect_ok") for verdict in verdicts):
        return _diagnosis("http_policy", "waf_or_access_policy", "try_browser_or_approved_proxy")
    if 404 in statuses or "not_found" in verdicts:
        return _diagnosis("http", "not_found", "verify_url")
    return _diagnosis("unknown", "insufficient_signal", "inspect_trace")


def _diagnosis(layer: str, category: str, next_action: str) -> dict[str, str]:
    return {"layer": layer, "category": category, "next_action": next_action}
