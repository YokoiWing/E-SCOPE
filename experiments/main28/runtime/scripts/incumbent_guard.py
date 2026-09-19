"""Select only frozen, externally better, formal-clean mapped netlists."""
import hashlib
import math
from pathlib import Path


def select_verified_incumbent(incumbent, candidates, frozen_shas, validations):
    """Inputs must share one external environment and source-equivalence contract.

    The caller provides complete external rows and a pre-evaluation SHA allowlist.
    A rejected challenger never displaces the verified incumbent.
    """
    checks = ("mapped_only_legality", "full186_only", "write_reparse_cold_read", "formal_equivalence")

    def valid(row):
        digest = row["sha256"]
        if digest not in frozen_shas or row.get("status") != "PASS":
            return False
        try:
            if hashlib.sha256(Path(row["netlist"]).read_bytes()).hexdigest() != digest:
                return False
            metrics = row["external"]
            if not all(math.isfinite(float(metrics[k])) and float(metrics[k]) > 0
                       for k in ("delay_ps", "area_um2", "power_w", "d2ap")):
                return False
            expected = metrics["delay_ps"] ** 2 * metrics["area_um2"] * metrics["power_w"]
            if not math.isclose(expected, metrics["d2ap"], rel_tol=1e-9):
                return False
        except (OSError, KeyError, TypeError, ValueError):
            return False
        return any(v.get("target_sha256") == digest and all(v.get(k) == "PASS" for k in checks)
                   and v.get("unresolved_cells") == 0 for v in validations)

    if not valid(incumbent):
        raise ValueError("mandatory incumbent is not fully verified; do not promote a challenger")
    winner = incumbent
    for row in candidates:
        if valid(row) and row["external"]["d2ap"] < winner["external"]["d2ap"]:
            winner = row
    return winner
