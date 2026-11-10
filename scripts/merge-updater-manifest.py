#!/usr/bin/env python3
"""Merge Tauri's per-target updater manifests into one latest.json.

Each matrix leg builds on its own platform and emits a latest.json describing
only that platform, so four separate files arrive as artefacts. The updater
fetches a single document from a fixed URL, so they have to be combined here.

Usage: merge-updater-manifest.py <artifacts-dir> <output-path>
"""
import datetime
import glob
import json
import sys


def main() -> int:
    if len(sys.argv) != 3:
        print(__doc__.strip().splitlines()[-1], file=sys.stderr)
        return 2

    root, out_path = sys.argv[1], sys.argv[2]

    platforms: dict = {}
    version = None
    notes = ""
    found = []

    for path in sorted(glob.glob(f"{root}/**/latest.json", recursive=True)):
        try:
            with open(path) as fh:
                doc = json.load(fh)
        except (OSError, json.JSONDecodeError) as why:
            # One unreadable manifest must not sink the release: the other
            # platforms can still update, and the warning names the culprit.
            print(f"warning: skipping {path}: {why}", file=sys.stderr)
            continue

        found.append(path)
        version = version or doc.get("version")
        notes = notes or doc.get("notes", "")
        platforms.update(doc.get("platforms", {}))

    if not platforms:
        print(
            "error: no updater manifests found. Is bundle.createUpdaterArtifacts "
            "enabled and TAURI_SIGNING_PRIVATE_KEY set?",
            file=sys.stderr,
        )
        return 1

    merged = {
        "version": version,
        "notes": notes,
        # The updater compares against the version string, but a timestamp makes
        # it possible to tell two builds of the same version apart when debugging.
        "pub_date": datetime.datetime.now(datetime.timezone.utc).strftime(
            "%Y-%m-%dT%H:%M:%SZ"
        ),
        "platforms": platforms,
    }

    with open(out_path, "w") as fh:
        json.dump(merged, fh, indent=2)
        fh.write("\n")

    print(f"merged {len(found)} manifest(s) into {out_path}")
    for key in sorted(platforms):
        print(f"  {key}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
