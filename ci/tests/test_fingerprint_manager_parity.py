"""The shipped FingerprintManager must match the tested one (#1650).

`python/zccache/fingerprint/_manager.py` is what the `zccache` wheel ships,
while `crates/zccache-fingerprint/python/tests` exercises the crate copy
without needing the native extension. The two drifted before #1650, so a fix
landed in one copy could leave the published API with the false-green bug.
"""

from __future__ import annotations

from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SHIPPED = ROOT / "python" / "zccache" / "fingerprint" / "_manager.py"
TESTED = (
    ROOT
    / "crates"
    / "zccache-fingerprint"
    / "python"
    / "zccache"
    / "fingerprint"
    / "_manager.py"
)


def test_shipped_fingerprint_manager_matches_tested_copy() -> None:
    assert SHIPPED.read_bytes() == TESTED.read_bytes(), (
        f"{SHIPPED.relative_to(ROOT)} and {TESTED.relative_to(ROOT)} must be "
        "identical; edit one and copy it over the other"
    )
