#!/usr/bin/env python3
"""Document, media-transcript, and archive fallback regressions."""
from __future__ import annotations

import os
import sys
from unittest import mock

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.abspath(os.path.join(HERE, "..", ".."))
sys.path.insert(0, ROOT)

import engine.fetch_chain as fc  # noqa: E402
import engine.phase0 as phase0  # noqa: E402


class _Response:
    def __init__(self, status: int, text: str, payload=None, url=""):
        self.status_code = status
        self.text = text
        self.url = url
        self._payload = payload

    def json(self):
        return self._payload


def t_pdf_table_is_preserved_as_markdown() -> None:
    table = [["Name", "Value|Unit"], ["Alpha", "3\nkg"], [None, "4"]]
    markdown = fc._pdf_table_to_markdown(table)
    assert "| Name | Value\\|Unit |" in markdown
    assert "| Alpha | 3 kg |" in markdown
    assert "|  | 4 |" in markdown


def t_scanned_pdf_ocr_degrades_when_tools_are_missing() -> None:
    with mock.patch.object(fc._shutil, "which", return_value=None):
        text, error = fc._extract_pdf_ocr(b"%PDF-1.4\n")
    assert text == ""
    assert error == "pdf_ocr_tools_missing"


def t_vtt_cleanup_removes_timing_and_duplicate_caption_lines() -> None:
    vtt = """WEBVTT

00:00:00.000 --> 00:00:02.000
<c>Hello world</c>

00:00:01.500 --> 00:00:03.000
Hello world
Next line
"""
    assert phase0._vtt_to_text(vtt) == "Hello world\nNext line"
    segments = phase0._vtt_segments(vtt)
    assert segments[0] == {
        "start": "00:00:00.000", "end": "00:00:02.000", "text": "Hello world"}


def t_subtitle_language_prefers_creator_korean_then_english() -> None:
    metadata = {
        "subtitles": {"en": [{"ext": "vtt"}], "ko": [{"ext": "vtt"}]},
        "automatic_captions": {"ko-orig": [{"ext": "vtt"}]},
    }
    assert phase0._choose_subtitle_language(metadata, ("ko", "en")) == ("ko", "creator")


def t_archive_route_returns_only_a_valid_wayback_snapshot() -> None:
    api = _Response(200, "{}", {
        "archived_snapshots": {"closest": {
            "available": True,
            "status": "200",
            "timestamp": "20240102030405",
            "url": "https://web.archive.org/web/20240102030405/https://example.com/a",
        }}
    })
    page = _Response(200, "archived body " * 100,
                     url="https://web.archive.org/web/20240102030405/https://example.com/a")
    with mock.patch.object(phase0, "_archive_get", side_effect=[api, page]):
        result = phase0.route_archive("https://example.com/a", timeout=2)
    assert result["ok"] is True
    assert result["timestamp"] == "20240102030405"
    assert result["content"].startswith("archived body")


def t_archive_fallback_is_labeled_historical_in_evidence() -> None:
    base = fc.FetchResult(
        ok=False, final_url="https://example.com/a", verdict="not_found",
        trace=[fc.Attempt(
            phase="probe", executor="curl_cffi", url="https://example.com/a",
            url_transform="original", impersonate="safari", referer="self_root",
            verdict="not_found")],
    )
    archived = {
        "ok": True,
        "content": "<html><body>" + ("historical text " * 100) + "</body></html>",
        "final_url": "https://web.archive.org/web/20240102030405/https://example.com/a",
        "timestamp": "20240102030405",
        "attempts": [{
            "route": "wayback-snapshot", "ok": True, "status": 200,
            "bytes": 1600, "note": "historical_snapshot",
        }],
    }
    with mock.patch.object(phase0, "route_archive", return_value=archived):
        result = fc._archive_fallback(
            base, "https://example.com/a", timeout=2, proxy=None,
            enable_extraction=True, enable_markdown=True, enable_maincontent=False)
    assert result.ok is True
    assert result.extraction_source.startswith("archive+")
    assert result.evidence_record()["currentness"] == "historical_snapshot"


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
