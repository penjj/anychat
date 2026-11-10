#!/usr/bin/env python3
"""Write latest.json (and its mirror copies) from the signed bundles.

Tauri signs the update payloads but does not emit an updater manifest: the
`.sig` files are produced and that is all. The merge step that used to live in
release.yml assumed otherwise and failed with "no updater manifests found",
after every build leg had already succeeded.

So the manifest is assembled here, from what the bundler actually leaves on
disk: a tarball per macOS target, an NSIS zip per Windows target, and a `.sig`
beside each.

Usage: build-updater-manifest.py <artifacts-dir> <output-dir> <version> <tag>
"""
import datetime
import json
import os
import sys

# What the updater consumes per platform. macOS gets the .app tarball; Windows
# gets the NSIS installer, either raw or as the .exe.zip the bundler emits. It
# is *not* .nsis.zip -- the bundler never produces that name, and looking for it
# silently dropped both Windows platforms from the first published manifest.
MACOS_SUFFIX = ".app.tar.gz"
WINDOWS_SUFFIXES = (".exe.zip", ".exe", ".msi.zip")

# Directories produced by the matrix legs, keyed by the triple they built for.
# The name carries the platform, so the same file name can appear four times.
LEG_TARGET = {
    "anychat-mac-arm64": "darwin-aarch64",
    "anychat-mac-x64": "darwin-x86_64",
    "anychat-win-x64": "windows-x86_64",
    "anychat-win-arm64": "windows-aarch64",
}


def kind_label(path: str) -> str:
    """Suffix for the uploaded file, derived from the payload itself."""
    if path.endswith(MACOS_SUFFIX):
        return "app.tar.gz"
    for suffix in WINDOWS_SUFFIXES:
        if path.endswith(suffix):
            return suffix.lstrip(".")
    return "bin"


def find_signed_artifact(leg_dir, kind):
    """Return (payload, signature) paths for one leg, or (None, None)."""
    base = os.path.join(leg_dir, kind)
    if not os.path.isdir(base):
        return None, None
    # Prefer the archive form when present: a single zipped installer keeps the
    # signature covering exactly one file, and it is what the bundler signs.
    candidates = []
    for name in sorted(os.listdir(base)):
        if name.endswith(".sig"):
            continue
        if name.endswith(MACOS_SUFFIX) or name.endswith(WINDOWS_SUFFIXES):
            candidates.append(os.path.join(base, name))
    # Longer suffixes first, so .exe.zip wins over a bare .exe in the same dir.
    candidates.sort(key=lambda p: -len(p))
    for payload in candidates:
        sig = payload + ".sig"
        if os.path.exists(sig):
            return payload, sig
    return None, None


def main() -> int:
    if len(sys.argv) != 5:
        print(__doc__.strip().splitlines()[-1], file=sys.stderr)
        return 2

    artifacts, out_dir, version, tag = sys.argv[1:5]
    base_url = (
        f"https://github.com/penjj/anychat/releases/download/{tag}"
    )

    platforms = {}
    for leg, target in LEG_TARGET.items():
        leg_dir = os.path.join(artifacts, leg)
        kind = "macos" if target.startswith("darwin") else "nsis"
        payload, sig_path = find_signed_artifact(leg_dir, kind)
        if not payload:
            # A missing platform must not sink the release: the others still
            # update, and the gap is visible in the summary below.
            print(f"warning: no signed {kind} payload for {target}", file=sys.stderr)
            continue
        with open(sig_path) as fh:
            signature = fh.read().strip()
        # The bundle names are the same on both macOS legs, so two
        # AnyChat.app.tar.gz files would collide in one release and the second
        # upload would overwrite the first. The release upload renames them to
        # include the leg; the URL has to name the renamed file.
        upload_name = f"AnyChat_{version}_{target}.{kind_label(payload)}"
        platforms[target] = {
            "signature": signature,
            "url": f"{base_url}/{upload_name}",
        }

    if not platforms:
        print("error: no signed payloads found", file=sys.stderr)
        return 1

    doc = {
        "version": version,
        "notes": f"AnyChat {version}",
        "pub_date": datetime.datetime.now(datetime.timezone.utc).strftime(
            "%Y-%m-%dT%H:%M:%SZ"
        ),
        "platforms": platforms,
    }

    os.makedirs(out_dir, exist_ok=True)
    out_path = os.path.join(out_dir, "latest.json")
    with open(out_path, "w") as fh:
        json.dump(doc, fh, indent=2)
        fh.write("\n")

    print(f"wrote {out_path} with {len(platforms)} platform(s)")
    for key in sorted(platforms):
        print(f"  {key}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
