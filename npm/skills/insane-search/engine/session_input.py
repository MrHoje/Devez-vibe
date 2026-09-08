"""Load an explicitly supplied browser cookie export without persisting secrets."""
from __future__ import annotations

import json
from pathlib import Path
from typing import Any
from urllib.parse import urlsplit


_MAX_COOKIE_FILE_BYTES = 1024 * 1024


def _has_control(value: str) -> bool:
    return any(ord(char) < 0x20 or ord(char) == 0x7F for char in value)


def _domain_matches(host: str, domain: str) -> bool:
    clean = domain.lstrip(".").lower().rstrip(".")
    return bool(clean) and (host == clean or host.endswith("." + clean))


def _normalize_cookie(item: dict[str, Any], host: str) -> dict[str, Any] | None:
    name = str(item.get("name") or "")
    value = str(item.get("value") or "")
    domain = str(item.get("domain") or host).lower()
    path = str(item.get("path") or "/")
    if not name or _has_control(name) or _has_control(value):
        raise ValueError("cookie name or value contains control characters")
    if not _domain_matches(host, domain):
        return None
    if not path.startswith("/") or _has_control(path) or _has_control(domain):
        raise ValueError("cookie path or domain is invalid")
    result: dict[str, Any] = {
        "name": name,
        "value": value,
        "domain": domain,
        "path": path,
        "secure": bool(item.get("secure", False)),
    }
    if bool(item.get("httpOnly", False)):
        result["httpOnly"] = True
    same_site = str(item.get("sameSite") or "").capitalize()
    if same_site in ("Strict", "Lax", "None"):
        result["sameSite"] = same_site
    expires = item.get("expires", item.get("expirationDate"))
    if expires not in (None, "", 0, "0"):
        try:
            result["expires"] = int(float(expires))
        except (TypeError, ValueError) as exc:
            raise ValueError("cookie expiry is invalid") from exc
    return result


def _parse_netscape(text: str) -> list[dict[str, Any]]:
    cookies: list[dict[str, Any]] = []
    for line in text.splitlines():
        if not line or (line.startswith("#") and not line.startswith("#HttpOnly_")):
            continue
        http_only = line.startswith("#HttpOnly_")
        if http_only:
            line = line[len("#HttpOnly_"):]
        parts = line.split("\t")
        if len(parts) != 7:
            continue
        domain, _include_subdomains, path, secure, expires, name, value = parts
        cookies.append({
            "domain": domain,
            "path": path,
            "secure": secure.upper() == "TRUE",
            "expires": expires,
            "name": name,
            "value": value,
            "httpOnly": http_only,
        })
    return cookies


def load_cookie_file(path: str, target_url: str) -> list[dict[str, Any]]:
    """Read JSON or Netscape cookies and keep only cookies valid for target_url."""
    target_host = (urlsplit(target_url).hostname or "").lower()
    if not target_host:
        raise ValueError("target URL must include a host")
    cookie_path = Path(path).expanduser()
    if not cookie_path.is_file():
        raise ValueError("cookie file does not exist")
    if cookie_path.stat().st_size > _MAX_COOKIE_FILE_BYTES:
        raise ValueError("cookie file is too large")
    text = cookie_path.read_text(encoding="utf-8", errors="strict")
    stripped = text.lstrip()
    if stripped.startswith(("[", "{")):
        parsed = json.loads(text)
        raw = parsed.get("cookies") if isinstance(parsed, dict) else parsed
        if not isinstance(raw, list):
            raise ValueError("JSON cookie file must contain a list")
    else:
        raw = _parse_netscape(text)
    if len(raw) > 5000:
        raise ValueError("cookie file contains too many entries")
    cookies = []
    for item in raw:
        if not isinstance(item, dict):
            continue
        normalized = _normalize_cookie(item, target_host)
        if normalized is not None:
            cookies.append(normalized)
    if not cookies:
        raise ValueError("cookie file has no cookies for the target host")
    return cookies


def redact_cookie_values(text: str | None, cookies: list[dict] | None) -> str | None:
    """Remove supplied cookie names and values from subprocess diagnostics."""
    if text is None or not cookies:
        return text
    redacted = text
    for cookie in cookies:
        for value in (str(cookie.get("name") or ""), str(cookie.get("value") or "")):
            if value:
                redacted = redacted.replace(value, "***")
    return redacted
