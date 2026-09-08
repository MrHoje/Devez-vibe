"""One-shot hidden browser fallback using Scrapling's StealthyFetcher."""
from __future__ import annotations

import json
import sys


def main() -> int:
    args = json.load(sys.stdin)
    from scrapling.fetchers import StealthyFetcher

    options = {
        "headless": bool(args.get("headless", True)),
        "real_chrome": bool(args.get("realChrome", True)),
        "solve_cloudflare": bool(args.get("solveCloudflare", False)),
        "block_ads": bool(args.get("blockAds", True)),
        "timeout": int(args.get("timeout", 90000)),
    }
    if args.get("waitSelector"):
        options["wait_selector"] = args["waitSelector"]
    if args.get("proxy"):
        options["proxy"] = args["proxy"]
    if args.get("cookies"):
        options["cookies"] = args["cookies"]

    page = StealthyFetcher.fetch(args["url"], **options)
    html = getattr(page, "html_content", "") or ""
    payload = {
        "html": html,
        "finalUrl": str(getattr(page, "url", "") or args["url"]),
        "status": int(getattr(page, "status", 0) or 200),
        "cookies": [],
        "userAgent": "",
        "automation": "scrapling-stealthy-fetcher",
        "innerText": "",
    }
    sys.stdout.write(json.dumps(payload, ensure_ascii=False))
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except Exception as exc:
        print(f"{type(exc).__name__}: {exc}", file=sys.stderr)
        sys.exit(1)
