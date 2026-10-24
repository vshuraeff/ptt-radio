# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Fetch immutable upstream weights and verify their SHA-256 before use."""

import argparse
import hashlib
from pathlib import Path
import urllib.request

COMMIT = "9d24e128b4f409db18227b8babb343016625921f"
HASHES = {
    "128_1": "8d241b3a732af8ca140b2e30043e56a6c3c7800c46e22c26f4eea2f70974ad1e",
    "128_2": "350bb01a1152ae3cabe09fe5e868ef2f7d8b988a9f22aae44f140195f6493126",
    "512_1": "569f7c3cfac96b1e093229c3ca10b5d892f5b1906105b644e457f8245b4f7383",
    "512_2": "fb423d867ab25d5f4716bd369c7126b6c84926175c019b832e71bf21de0e9907",
}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--units", choices=["128", "512"], default="128")
    parser.add_argument("--directory", type=Path, default=Path(__file__).parent / "models")
    args = parser.parse_args()
    args.directory.mkdir(parents=True, exist_ok=True)
    for part in [1, 2]:
        key = f"{args.units}_{part}"
        name = f"dtln_aec_{key}.tflite"
        target = args.directory / name
        data = target.read_bytes() if target.exists() else urllib.request.urlopen(
            f"https://raw.githubusercontent.com/breizhn/DTLN-aec/{COMMIT}/pretrained_models/{name}",
            timeout=60,
        ).read()
        if hashlib.sha256(data).hexdigest() != HASHES[key]:
            raise ValueError(f"SHA-256 mismatch: {name}")
        target.write_bytes(data)
        print(f"verified {name}: {len(data)} bytes")


if __name__ == "__main__":
    main()
