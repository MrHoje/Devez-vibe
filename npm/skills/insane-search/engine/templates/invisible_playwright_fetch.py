"""Python-only stealth Firefox fallback using invisible-playwright."""
from __future__ import annotations

import json
import sys


def main() -> int:
    args = json.load(sys.stdin)
    from invisible_playwright import InvisiblePlaywright

    launch = {
        "headless": bool(args.get("headless", False)),
        "profile_dir": args.get("profileDir"),
    }
    if args.get("proxy"):
        launch["proxy"] = args["proxy"]

    timeout_ms = int(args.get("timeout", 60000))
    with InvisiblePlaywright(**launch) as browser:
        page = browser.new_page()
        page.set_default_timeout(timeout_ms)
        response = page.goto(args["url"], wait_until="domcontentloaded", timeout=timeout_ms)
        page.wait_for_timeout(3500)
        selector = args.get("waitSelector")
        if selector:
            try:
                page.wait_for_selector(selector, timeout=min(timeout_ms, 15000))
            except Exception as exc:
                print(f"best-effort waitSelector failed: {exc}", file=sys.stderr)

        context = page.context
        cookies = [
            {"name": item["name"], "value": item["value"], "domain": item.get("domain", "")}
            for item in context.cookies()
        ]
        payload = {
            "html": page.content() or "",
            "finalUrl": page.url,
            "status": response.status if response else 200,
            "cookies": cookies,
            "userAgent": page.evaluate("() => navigator.userAgent"),
            "automation": "invisible-playwright",
            "innerText": page.evaluate("() => document.body && document.body.innerText || ''"),
        }
        sys.stdout.write(json.dumps(payload, ensure_ascii=False))
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except Exception as exc:
        print(f"{type(exc).__name__}: {exc}", file=sys.stderr)
        sys.exit(1)
