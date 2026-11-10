#!/usr/bin/env python3
"""Assemble the standalone server installer from ordered source fragments."""
import argparse
from pathlib import Path

ROOT = Path(__file__).resolve().parent
PARTS = (
    "00-bootstrap.sh",
    "10-management.sh",
    "20-upgrade.sh",
    "30-access.sh",
    "40-network.sh",
    "50-dependencies.sh",
    "60-install.sh",
    "70-config.sh",
    "80-services.sh",
)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true", help="fail if the shipped installer is stale")
    args = parser.parse_args()
    generated = b"".join((ROOT / "server-installer" / name).read_bytes() for name in PARTS)
    destination = ROOT / "install-tako-server.sh"
    if args.check:
        if destination.read_bytes() != generated:
            parser.exit(1, "Server installer is stale; run python3 scripts/build-server-installer.py\n")
    else:
        destination.write_bytes(generated)


if __name__ == "__main__":
    main()
