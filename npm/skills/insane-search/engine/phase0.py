"""Phase 0 — official public-API router (the SANCTIONED exception to No-Site-Name).

Per SKILL.md R5, platforms that publish official no-auth public endpoints get a
deterministic route tried BEFORE the generic WAF grid. This is the *enforced,
in-engine* version of what used to be agent-driven curl snippets in SKILL.md —
so the agent can no longer silently skip it (which is exactly how Reddit/X were
wrongly declared "blocked": the grid 403'd on `.json` and nobody tried `.rss`).

This file is the ONLY engine/ module allowed to name platform hosts; it is
exempted in `bias_check.EXPLICIT_ALLOW_FILES`. Do NOT add per-site logic to any
other engine file — generic WAF handling stays site-agnostic.

Contract:
    route(url) -> Optional[dict]
      None              → url is not a recognised Phase-0 platform; caller runs
                          the generic grid as usual.
      {"platform","ok","route","content","final_url","attempts":[...]}
                        → recognised platform. `ok` says whether an official
                          route succeeded. Even on ok=False the caller should
                          fall through to the grid, but `attempts` is recorded
                          so failure is never silent.

Each attempt dict: {"route","platform","ok","status","bytes","note"}.
"""
from __future__ import annotations

import importlib.util
import html
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path
from typing import Optional
from urllib.parse import quote, urlsplit


# --- low-level helpers -------------------------------------------------------
def _cffi_get(url: str, *, impersonate: str = "safari", timeout: int = 15,
              proxy: Optional[str] = None):
    from curl_cffi import requests as r  # lazy: engine works even if missing
    kwargs = dict(
        impersonate=impersonate,  # type: ignore[arg-type]
        timeout=timeout,
        headers={
            "Accept": "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
            "Accept-Language": "en-US,en;q=0.9,ko;q=0.8",
        },
        allow_redirects=True,
    )
    if proxy:
        kwargs["proxy"] = proxy
    return r.get(url, **kwargs)


def _host(url: str) -> str:
    h = (urlsplit(url).hostname or "").lower()
    return h[4:] if h.startswith("www.") else h  # strip the literal "www." prefix only


def _attempt(platform: str, route: str, ok: bool, status: int, body: str, note: str = "") -> dict:
    return {"platform": platform, "route": route, "ok": ok, "status": status,
            "bytes": len(body or ""), "note": note}


# --- platform detectors ------------------------------------------------------
def _detect(url: str) -> Optional[str]:
    h = _host(url)
    if not h:
        return None
    if "reddit.com" in h or h == "redd.it":
        return "reddit"
    if h in ("x.com", "twitter.com") or h.endswith(".x.com") or h.endswith(".twitter.com"):
        return "x"
    if "youtube.com" in h or h == "youtu.be":
        return "youtube"
    if h in ("threads.com", "threads.net") or h.endswith((".threads.com", ".threads.net")):
        return "threads"
    return None


# --- reddit ------------------------------------------------------------------
def _reddit(url: str, timeout: int, proxy: Optional[str] = None) -> dict:
    attempts: list[dict] = []
    base = url.split("?", 1)[0].rstrip("/")
    # Build an .rss / .json target from the path (works for /r/<sub> and post URLs).
    rss_url = base + ("/.rss" if "/comments/" not in base else ".rss")
    json_url = base + ("/.json" if "/comments/" not in base else ".json")

    # Route 1: RSS (the route that actually survives — Reddit gates the JSON API).
    try:
        x = _cffi_get(rss_url, timeout=timeout, proxy=proxy)
        ok = x.status_code == 200 and ("<rss" in x.text or "<feed" in x.text)
        attempts.append(_attempt("reddit", "rss", ok, x.status_code, x.text,
                                 "feed" if ok else "no-feed-markers"))
        if ok:
            return {"platform": "reddit", "ok": True, "route": "rss",
                    "content": x.text, "final_url": rss_url, "attempts": attempts}
    except Exception as e:
        attempts.append(_attempt("reddit", "rss", False, 0, "", f"{type(e).__name__}"))

    # Route 2: JSON via curl_cffi (often 403 now, but try — cheap).
    try:
        x = _cffi_get(json_url, timeout=timeout, proxy=proxy)
        ok = x.status_code == 200 and x.text.lstrip().startswith(("{", "["))
        attempts.append(_attempt("reddit", "json", ok, x.status_code, x.text,
                                 "json" if ok else f"status={x.status_code}"))
        if ok:
            return {"platform": "reddit", "ok": True, "route": "json",
                    "content": x.text, "final_url": json_url, "attempts": attempts}
    except Exception as e:
        attempts.append(_attempt("reddit", "json", False, 0, "", f"{type(e).__name__}"))

    return {"platform": "reddit", "ok": False, "route": None, "content": "",
            "final_url": url, "attempts": attempts}


# --- x / twitter -------------------------------------------------------------
_TWEET_ID_RE = re.compile(r"/status(?:es)?/(\d+)")


def _x(url: str, timeout: int, proxy: Optional[str] = None) -> dict:
    attempts: list[dict] = []
    m = _TWEET_ID_RE.search(url)

    if m:  # single tweet → tweet-result + oembed (both no-auth, reliable)
        tid = m.group(1)
        try:
            x = _cffi_get(f"https://cdn.syndication.twimg.com/tweet-result?id={tid}&token=a",
                          timeout=timeout, proxy=proxy)
            d = x.json() if x.status_code == 200 else {}
            ok = bool(d.get("text"))
            attempts.append(_attempt("x", "tweet-result", ok, x.status_code, x.text,
                                     "has-text" if ok else f"status={x.status_code}"))
            if ok:
                return {"platform": "x", "ok": True, "route": "tweet-result",
                        "content": x.text, "final_url": url, "attempts": attempts}
        except Exception as e:
            attempts.append(_attempt("x", "tweet-result", False, 0, "", f"{type(e).__name__}"))
        try:
            ourl = f"https://publish.twitter.com/oembed?url=https://twitter.com/i/status/{tid}&omit_script=1"
            x = _cffi_get(ourl, timeout=timeout, proxy=proxy)
            d = x.json() if x.status_code == 200 else {}
            ok = bool(d.get("html"))
            attempts.append(_attempt("x", "oembed", ok, x.status_code, x.text,
                                     "has-html" if ok else f"status={x.status_code}"))
            if ok:
                return {"platform": "x", "ok": True, "route": "oembed",
                        "content": x.text, "final_url": ourl, "attempts": attempts}
        except Exception as e:
            attempts.append(_attempt("x", "oembed", False, 0, "", f"{type(e).__name__}"))
    else:  # profile timeline → syndication (rate-limit-prone; retry once)
        handle = urlsplit(url).path.strip("/").split("/")[0]
        _reserved = {"i", "search", "home", "explore", "messages", "notifications", "settings", "hashtag"}
        if handle and handle.lower() not in _reserved:
            surl = f"https://syndication.twitter.com/srv/timeline-profile/screen-name/{handle}"
            for attempt_no in range(2):
                try:
                    x = _cffi_get(surl, timeout=timeout, proxy=proxy)
                    ok = x.status_code == 200 and "__NEXT_DATA__" in x.text
                    attempts.append(_attempt("x", f"syndication-timeline#{attempt_no+1}", ok,
                                             x.status_code, x.text,
                                             "timeline" if ok else f"status={x.status_code}"))
                    if ok:
                        return {"platform": "x", "ok": True, "route": "syndication-timeline",
                                "content": x.text, "final_url": surl, "attempts": attempts}
                except Exception as e:
                    attempts.append(_attempt("x", f"syndication-timeline#{attempt_no+1}", False, 0, "", f"{type(e).__name__}"))

    return {"platform": "x", "ok": False, "route": None, "content": "",
            "final_url": url, "attempts": attempts}


# --- youtube -----------------------------------------------------------------
def _ytdlp_argv() -> Optional[list[str]]:
    """Best yt-dlp invocation for this environment, or None if unavailable.

    Prefers the ``yt-dlp`` console script on PATH; falls back to
    ``<python> -m yt_dlp`` when the script dir is not on PATH but the module is
    importable — the common case for ``pip install --user`` and Windows / venv
    installs, where the Scripts/bin dir is frequently absent from PATH. Mirrors
    the ``which yt-dlp || python3 -m yt_dlp`` fallback already documented in
    references/media.md."""
    exe = shutil.which("yt-dlp")
    if exe:
        return [exe]
    if importlib.util.find_spec("yt_dlp") is not None:
        return [sys.executable, "-m", "yt_dlp"]
    return None


def _clean_caption_line(line: str) -> str:
    line = re.sub(r"<[^>]+>", "", line)
    line = html.unescape(line).strip()
    return "" if line.startswith(("Kind:", "Language:")) else line


def _vtt_segments(vtt: str) -> list[dict[str, str]]:
    """Parse bounded WebVTT cues so final citations can include timestamps."""
    source = (vtt or "")[:2_000_000].splitlines()
    segments: list[dict[str, str]] = []
    total = 0
    index = 0
    while index < len(source):
        timing = source[index].strip()
        if "-->" not in timing:
            index += 1
            continue
        start, end = [part.strip().split()[0] for part in timing.split("-->", 1)]
        index += 1
        cue: list[str] = []
        while index < len(source) and source[index].strip():
            line = _clean_caption_line(source[index])
            if line and (not cue or cue[-1] != line):
                cue.append(line)
            index += 1
        text = " ".join(cue).strip()
        if text and segments and text.startswith(segments[-1]["text"]):
            text = text[len(segments[-1]["text"]):].strip()
        if text and (not segments or segments[-1]["text"] != text):
            take = text[:max(0, 500_000 - total)]
            if take:
                segments.append({"start": start, "end": end, "text": take})
                total += len(take)
        if total >= 500_000 or len(segments) >= 5000:
            break
        index += 1
    return segments


def _vtt_to_text(vtt: str) -> str:
    lines: list[str] = []
    for segment in _vtt_segments(vtt):
        text = segment["text"]
        if not lines or lines[-1] != text:
            lines.append(text)
    return "\n".join(lines)[:500_000]


def _choose_subtitle_language(metadata: dict, preferred: tuple[str, ...] = ("ko", "en")) -> tuple[str, str] | None:
    """Choose a creator subtitle first, then an automatic caption."""
    for key, source in (("subtitles", "creator"), ("automatic_captions", "automatic")):
        available = metadata.get(key) or {}
        for wanted in preferred:
            if wanted in available:
                return wanted, source
            match = next((lang for lang in available if lang.lower().startswith(wanted.lower() + "-")), None)
            if match:
                return match, source
    return None


_YTDLP_ARGV_CACHE: Optional[list[str]] = None


def _ensure_ytdlp_argv(allow_install: bool) -> Optional[list[str]]:
    """Resolve yt-dlp, optionally installing it into an isolated first-use venv."""
    global _YTDLP_ARGV_CACHE
    available = _ytdlp_argv()
    if available is not None:
        return available
    if not allow_install:
        return None
    if os.environ.get("INSANE_AUTO_INSTALL", "").strip().lower() in ("0", "false", "no"):
        return None
    if _YTDLP_ARGV_CACHE is not None:
        return _YTDLP_ARGV_CACHE or None
    root = os.path.expanduser("~/.insane-search/media-venv")
    python = os.path.join(
        root, "Scripts", "python.exe") if os.name == "nt" else os.path.join(root, "bin", "python")
    try:
        if not os.path.isfile(python):
            subprocess.run(
                [sys.executable, "-m", "venv", root], capture_output=True,
                text=True, timeout=180, check=False)
        probe = subprocess.run(
            [python, "-m", "yt_dlp", "--version"], capture_output=True,
            text=True, timeout=30, check=False)
        if probe.returncode != 0:
            installed = subprocess.run(
                [python, "-m", "pip", "install", "yt-dlp", "-q"],
                capture_output=True, text=True, timeout=300, check=False)
            if installed.returncode != 0:
                _YTDLP_ARGV_CACHE = []
                return None
        _YTDLP_ARGV_CACHE = [python, "-m", "yt_dlp"]
        return list(_YTDLP_ARGV_CACHE)
    except Exception:
        _YTDLP_ARGV_CACHE = []
        return None


def _attach_transcript(metadata: dict, url: str, argv: list[str], timeout: int,
                       proxy: Optional[str], cookie_path: Optional[str] = None) -> dict:
    choice = _choose_subtitle_language(metadata)
    if choice is None:
        metadata["research_transcript"] = {"available": False, "reason": "no_subtitles"}
        return metadata
    language, source = choice
    try:
        with tempfile.TemporaryDirectory(prefix="insane-media-") as root:
            command = argv + [
                "--write-subs", "--write-auto-subs", "--sub-langs", language,
                "--sub-format", "vtt", "--skip-download", "--no-playlist",
                "--paths", root, "-o", "%(id)s.%(ext)s",
            ]
            if proxy:
                command += ["--proxy", proxy]
            if cookie_path:
                command += ["--cookies", cookie_path]
            completed = subprocess.run(
                command + [url], capture_output=True, text=True,
                timeout=max(timeout, 90), check=False)
            files = sorted(Path(root).glob("*.vtt"))
            text = ""
            segments: list[dict[str, str]] = []
            if completed.returncode == 0 and files:
                raw_vtt = files[0].read_text(encoding="utf-8", errors="replace")
                segments = _vtt_segments(raw_vtt)
                text = _vtt_to_text(raw_vtt)
            metadata["research_transcript"] = {
                "available": bool(text),
                "language": language,
                "source": source,
                "text": text,
                "segments": segments,
                "reason": "" if text else "subtitle_download_failed",
            }
    except subprocess.TimeoutExpired:
        metadata["research_transcript"] = {"available": False, "reason": "subtitle_timeout"}
    except Exception as exc:
        metadata["research_transcript"] = {
            "available": False, "reason": f"subtitle_error:{type(exc).__name__}"}
    return metadata


def _compact_media_metadata(metadata: dict) -> dict:
    """Drop signed streams and extractor internals that waste research tokens."""
    keys = (
        "id", "title", "description", "uploader", "uploader_id", "channel",
        "channel_id", "duration", "duration_string", "upload_date", "timestamp",
        "release_date", "webpage_url", "original_url", "availability", "live_status",
        "view_count", "like_count", "comment_count", "categories", "tags", "chapters",
    )
    compact = {key: metadata[key] for key in keys if metadata.get(key) is not None}
    compact["subtitle_languages"] = sorted((metadata.get("subtitles") or {}).keys())[:100]
    compact["automatic_caption_languages"] = sorted(
        (metadata.get("automatic_captions") or {}).keys())[:100]
    if "research_transcript" in metadata:
        compact["research_transcript"] = metadata["research_transcript"]
    return compact


def _write_cookie_jar(cookies: list[dict]) -> str:
    handle = tempfile.NamedTemporaryFile(
        "w", encoding="utf-8", prefix="insane-media-cookies-", suffix=".txt",
        delete=False)
    try:
        handle.write("# Netscape HTTP Cookie File\n")
        for cookie in cookies:
            domain = str(cookie.get("domain") or "")
            include_subdomains = "TRUE" if domain.startswith(".") else "FALSE"
            fields = (
                domain, include_subdomains, str(cookie.get("path") or "/"),
                "TRUE" if cookie.get("secure") else "FALSE",
                str(int(cookie.get("expires") or 0)), str(cookie.get("name") or ""),
                str(cookie.get("value") or ""),
            )
            handle.write("\t".join(fields) + "\n")
        return handle.name
    finally:
        handle.close()


def _media(url: str, timeout: int, proxy: Optional[str] = None,
           include_transcript: bool = False, platform: str = "media",
           cookies: Optional[list[dict]] = None,
           auto_install: bool = False) -> dict:
    attempts: list[dict] = []
    argv = _ensure_ytdlp_argv(auto_install)
    if argv is None:
        attempts.append(_attempt(platform, "yt-dlp", False, 0, "", "yt-dlp not installed"))
        return {"platform": platform, "ok": False, "route": None, "content": "",
                "final_url": url, "attempts": attempts}
    cookie_path = _write_cookie_jar(cookies) if cookies else None
    try:
        command = argv + ["--dump-json", "--skip-download"]
        if proxy:
            command += ["--proxy", proxy]
        if cookie_path:
            command += ["--cookies", cookie_path]
        p = subprocess.run(
            command + [url],
            capture_output=True, text=True, timeout=max(timeout, 60),
        )
        ok = p.returncode == 0 and p.stdout.strip().startswith("{")
        diagnostic = p.stderr or ""
        if cookie_path:
            diagnostic = diagnostic.replace(cookie_path, "<cookie-file>")
        if cookies:
            from .session_input import redact_cookie_values
            diagnostic = redact_cookie_values(diagnostic, cookies) or ""
        note = "json" if ok else diagnostic.strip()[:80]
        attempts.append(_attempt(platform, "yt-dlp", ok, 200 if ok else 0, p.stdout, note))
        if ok:
            metadata = json.loads(p.stdout)
            if include_transcript:
                metadata = _attach_transcript(
                    metadata, url, argv, timeout, proxy, cookie_path)
            if include_transcript or platform == "media":
                metadata = _compact_media_metadata(metadata)
            transcript_available = bool(
                (metadata.get("research_transcript") or {}).get("available"))
            return {"platform": platform, "ok": True, "route": "yt-dlp",
                    "content": json.dumps(metadata, ensure_ascii=False),
                    "final_url": url, "attempts": attempts,
                    "content_kind": ("media+transcript" if transcript_available
                                     else "media_metadata"),
                    "transcript": transcript_available}
    except FileNotFoundError:
        attempts.append(_attempt(platform, "yt-dlp", False, 0, "", "yt-dlp not installed"))
    except Exception as e:
        attempts.append(_attempt(platform, "yt-dlp", False, 0, "", f"{type(e).__name__}"))
    finally:
        if cookie_path:
            try:
                Path(cookie_path).unlink()
            except OSError:
                pass
    return {"platform": platform, "ok": False, "route": None, "content": "",
            "final_url": url, "attempts": attempts}


def _youtube(url: str, timeout: int, proxy: Optional[str] = None,
             include_transcript: bool = False,
             cookies: Optional[list[dict]] = None) -> dict:
    return _media(
        url, timeout, proxy, include_transcript, platform="youtube", cookies=cookies,
        auto_install=include_transcript)


# --- threads -----------------------------------------------------------------
_THREADS_POST_RE = re.compile(r"/post/([A-Za-z0-9_-]+)")


def _threads(url: str, timeout: int, proxy: Optional[str] = None) -> dict:
    """Threads video post → signed CDN URLs from the page's inline JSON.

    yt-dlp has no Threads extractor, but an anonymous GET with a curl_cffi
    fingerprint passes the WAF and the HTML embeds several `video_versions`
    blocks (related posts included) — the block nearest to `"code":"<shortcode>"`
    is the requested post's. The signed URLs expire (`oe` param): download
    promptly, outside the engine (plain curl suffices for the CDN).
    """
    attempts: list[dict] = []
    m = _THREADS_POST_RE.search(url.split("?", 1)[0])
    if not m:  # profile/tag URL — no deterministic media route; run the grid
        attempts.append(_attempt("threads", "inline-json", False, 0, "", "no-post-shortcode"))
        return {"platform": "threads", "ok": False, "route": None, "content": "",
                "final_url": url, "attempts": attempts}
    code = m.group(1)
    try:
        x = _cffi_get(url, timeout=timeout, proxy=proxy)
        raw = x.text if x.status_code == 200 else ""
        code_pos = [c.start() for c in re.finditer(r'"code"\s*:\s*"%s"' % re.escape(code), raw)]
        blocks = list(re.finditer(r'"video_versions"\s*:\s*\[(.*?)\]', raw))
        if not code_pos or not blocks:
            note = (f"status={x.status_code}" if x.status_code != 200
                    else ("no-code-marker" if not code_pos else "no-video_versions"))
            attempts.append(_attempt("threads", "inline-json", False, x.status_code, raw, note))
            return {"platform": "threads", "ok": False, "route": None, "content": "",
                    "final_url": url, "attempts": attempts}
        best = min(blocks, key=lambda b: min(abs(b.start() - c) for c in code_pos))
        urls: list[str] = []
        for u in re.findall(r'"url"\s*:\s*"([^"]+)"', best.group(1)):
            u = u.replace("\\/", "/").encode().decode("unicode_escape")
            if u not in urls:
                urls.append(u)
        if not urls:
            attempts.append(_attempt("threads", "inline-json", False, x.status_code, raw, "empty-video_versions"))
            return {"platform": "threads", "ok": False, "route": None, "content": "",
                    "final_url": url, "attempts": attempts}
        content = json.dumps({"post_code": code, "video_urls": urls}, ensure_ascii=False)
        attempts.append(_attempt("threads", "inline-json", True, x.status_code, content,
                                 f"{len(urls)} video url(s)"))
        return {"platform": "threads", "ok": True, "route": "inline-json",
                "content": content, "final_url": url, "attempts": attempts}
    except Exception as e:
        attempts.append(_attempt("threads", "inline-json", False, 0, "", f"{type(e).__name__}"))
        return {"platform": "threads", "ok": False, "route": None, "content": "",
                "final_url": url, "attempts": attempts}


_ROUTERS = {"reddit": _reddit, "x": _x, "youtube": _youtube, "threads": _threads}


# --- public entrypoint -------------------------------------------------------
def route_media(url: str, *, timeout: int = 15, proxy: Optional[str] = None,
                include_transcript: bool = False,
                cookies: Optional[list[dict]] = None) -> dict:
    return _media(
        url, timeout, proxy, include_transcript, platform="media", cookies=cookies,
        auto_install=True)


class _ArchiveResponse:
    def __init__(self, status: int, text: str, url: str, transport: str):
        self.status_code = status
        self.text = text
        self.url = url
        self.transport = transport

    def json(self):
        return json.loads(self.text)


def _archive_get(url: str, timeout: int, proxy: Optional[str]) -> _ArchiveResponse:
    try:
        response = _cffi_get(url, timeout=timeout, proxy=proxy)
        response.transport = "curl_cffi"
        return response
    except Exception:
        from .executor import run_playwright_fallback
        attempt, body = run_playwright_fallback(
            url, profile_id="unknown_challenge", force_executor="scrapling",
            timeout=max(timeout, 60), proxy=proxy)
        inner = str(getattr(attempt, "_inner_text", "") or "").strip()
        if inner:
            text = inner
        else:
            text = html.unescape(re.sub(r"(?s)<[^>]+>", " ", body or ""))
            text = re.sub(r"\s+", " ", text).strip()
        return _ArchiveResponse(
            int(getattr(attempt, "status", 0) or 0), text if text else body,
            str(getattr(attempt, "url", "") or url), "system_browser")


def route_archive(url: str, *, timeout: int = 15, proxy: Optional[str] = None) -> dict:
    """Retrieve the closest public Wayback snapshot without treating it as current."""
    from .safety import allow_private_default, classify_url

    attempts: list[dict] = []
    safe, reason = classify_url(url, allow_private_default())
    if not safe:
        attempts.append(_attempt("archive", "wayback-available", False, 0, "", reason))
        return {"platform": "archive", "ok": False, "route": None, "content": "",
                "final_url": url, "attempts": attempts, "timestamp": ""}
    try:
        api_url = "https://archive.org/wayback/available?url=" + quote(url, safe="")
        response = _archive_get(api_url, timeout=timeout, proxy=proxy)
        data = response.json() if response.status_code == 200 else {}
        closest = ((data.get("archived_snapshots") or {}).get("closest") or {})
        snapshot = str(closest.get("url") or "")
        timestamp = str(closest.get("timestamp") or "")
        valid_snapshot = (
            closest.get("available") is True
            and str(closest.get("status") or "") == "200"
            and urlsplit(snapshot).hostname == "web.archive.org"
        )
        attempts.append(_attempt(
            "archive", "wayback-available", valid_snapshot, response.status_code,
            response.text,
            (("snapshot:" + getattr(response, "transport", "unknown")) if valid_snapshot
             else (f"status={response.status_code}" if response.status_code != 200
                   else "no_snapshot"))))
        if not valid_snapshot:
            return {"platform": "archive", "ok": False, "route": None, "content": "",
                    "final_url": url, "attempts": attempts, "timestamp": ""}
        page = _archive_get(snapshot, timeout=max(timeout, 30), proxy=proxy)
        ok = page.status_code == 200 and len(page.text or "") >= 200
        attempts.append(_attempt(
            "archive", "wayback-snapshot", ok, page.status_code, page.text,
            (("historical_snapshot:" + getattr(page, "transport", "unknown"))
             if ok else "snapshot_fetch_failed")))
        return {"platform": "archive", "ok": ok,
                "route": "wayback" if ok else None,
                "content": page.text if ok else "", "final_url": snapshot if ok else url,
                "attempts": attempts, "timestamp": timestamp if ok else ""}
    except Exception as exc:
        attempts.append(_attempt(
            "archive", "wayback-available", False, 0, "", f"{type(exc).__name__}"))
        return {"platform": "archive", "ok": False, "route": None, "content": "",
                "final_url": url, "attempts": attempts, "timestamp": ""}


def route(url: str, *, timeout: int = 15, proxy: Optional[str] = None,
          include_media_transcript: bool = False,
          cookies: Optional[list[dict]] = None) -> Optional[dict]:
    platform = _detect(url)
    if platform is None:
        return None
    if platform == "youtube":
        return _youtube(url, timeout, proxy, include_media_transcript, cookies)
    return _ROUTERS[platform](url, timeout, proxy)
