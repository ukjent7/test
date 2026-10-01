"""Fetch the pinned, unmodified production decoder for the E2E client."""

import argparse
import hashlib
from pathlib import Path
import urllib.request

from common import write_json


REVISION = "2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8"
SOURCE = f"https://raw.githubusercontent.com/xai-org/grok-build/{REVISION}/crates/codegen/xai-grok-sampling-types/src/messages.rs"
ROOT = Path(__file__).resolve().parent


def main(output):
    with urllib.request.urlopen(SOURCE, timeout=60) as response:
        source = response.read()
    digest = hashlib.sha256(source).hexdigest()
    # This hash is calculated from the source already inspected in the workspace.
    expected = (ROOT / "grok-wire.sha256").read_text().strip()
    if digest != expected:
        raise RuntimeError(f"Grok wire source hash mismatch: {digest}")
    vendor = ROOT / "grok-wire/vendor"
    vendor.mkdir(parents=True, exist_ok=True)
    (vendor / "messages.rs").write_bytes(source)
    output.mkdir(parents=True, exist_ok=True)
    write_json(output / "grok-wire-source.json", {
        "revision": REVISION, "url": SOURCE, "sha256": digest,
    })


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, default=Path("artifacts/wire"))
    main(parser.parse_args().output)
