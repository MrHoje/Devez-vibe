#!/usr/bin/env python3
"""Research evidence metadata regression tests."""
from __future__ import annotations

import hashlib
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.abspath(os.path.join(HERE, "..", ".."))
sys.path.insert(0, ROOT)

from engine.fetch_chain import Attempt, FetchResult  # noqa: E402


def _result(**kwargs) -> FetchResult:
    values = dict(
        ok=True,
        content="source body",
        final_url="https://example.com/article?token=secret",
        verdict="strong_ok",
        profile_used="unknown_challenge",
        trace=[Attempt(
            phase="probe", executor="curl_cffi",
            url="https://example.com/article?token=secret",
            url_transform="original", impersonate="safari", referer="self_root",
        )],
        extraction_quality=0.8,
        extraction_source="raw+md",
    )
    values.update(kwargs)
    return FetchResult(**values)


def t_evidence_record_is_reproducible_and_masks_secrets() -> None:
    result = _result()
    evidence = result.evidence_record()
    assert evidence["requested_url"].endswith("token=REDACTED")
    assert evidence["final_url"].endswith("token=REDACTED")
    assert evidence["content_sha256"] == hashlib.sha256(b"source body").hexdigest()
    assert evidence["retrieved_at"].endswith("Z")
    assert evidence["source_kind"] == "web"
    assert evidence["currentness"] == "current"


def t_archive_record_is_labeled_historical() -> None:
    result = _result(
        extraction_source="archive+raw+md",
        extraction_meta={"snapshot_timestamp": "20240102030405"},
    )
    evidence = result.evidence_record()
    assert evidence["source_kind"] == "archive"
    assert evidence["currentness"] == "historical_snapshot"
    assert evidence["source_timestamp"] == "20240102030405"


def t_bundle_never_exposes_raw_content_without_boundary() -> None:
    result = _result()
    bundle = result.to_evidence_bundle()
    assert "content" not in bundle["result"]
    assert "evidence" not in bundle["result"]
    wrapped = bundle["untrusted_content"]
    assert "source body" in wrapped
    assert "BEGIN UNTRUSTED WEB CONTENT" in wrapped
    assert bundle["evidence"] == result.evidence_record()


def t_failed_retrieval_is_not_labeled_current() -> None:
    result = _result(ok=False, content="", final_url="", verdict="blocked")
    evidence = result.evidence_record()
    assert evidence["retrieval_status"] == "failed"
    assert evidence["currentness"] == "unavailable"


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
