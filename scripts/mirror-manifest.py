#!/usr/bin/env python3
"""Emit mirror variants of latest.json for networks that cannot reach GitHub.

GitHub release downloads are frequently slow or blocked from mainland China.
The app already tries update endpoints in order, but the manifest it fetches
names the *download* URL, so proxying the manifest alone does nothing: the
payload still comes from github.com.

This script rewrites each platform's download URL to a mirror prefix, producing
one manifest per mirror. Publishing those as release assets lets the app fall
back across endpoints without any client-side URL rewriting -- the signature
covers the payload, not the URL, so a rewritten host still verifies.

Usage: mirror-manifest.py <latest.json> <output-dir> [prefix ...]
"""
import json
import sys


def main() -> int:
    if len(sys.argv) < 3:
        print(__doc__.strip().splitlines()[-1], file=sys.stderr)
        return 2

    src, out_dir = sys.argv[1], sys.argv[2]
    prefixes = sys.argv[3:] or [
        "https://ghproxy.net/",
        "https://ghfast.top/",
    ]

    with open(src) as fh:
        base = json.load(fh)

    written = []
    for i, prefix in enumerate(prefixes, 1):
        doc = json.loads(json.dumps(base))
        for platform in doc.get("platforms", {}).values():
            url = platform.get("url", "")
            # Only rewrite hosts that are actually being mirrored; an asset
            # already served from elsewhere keeps its own URL.
            if "github.com/" in url or "githubusercontent.com/" in url:
                platform["url"] = prefix + url
        path = f"{out_dir}/latest-mirror{i}.json"
        with open(path, "w") as fh:
            json.dump(doc, fh, indent=2)
            fh.write("\n")
        written.append(path)

    for path in written:
        print(f"wrote {path}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
