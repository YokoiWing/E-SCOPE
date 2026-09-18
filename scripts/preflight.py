#!/usr/bin/env python3
"""Report tools available for each reproduction layer without running them."""

import json
import os
import shutil
import sys


def tool(name):
    return shutil.which(name)


def main():
    report = {
        "python": {"status": "PASS" if sys.version_info >= (3, 10) else "FAIL", "version": sys.version.split()[0]},
        "optimization_build": {
            "cmake": tool("cmake") or "MISSING",
            "cxx": tool("c++") or tool("g++") or "MISSING",
            "cargo": tool("cargo") or "MISSING",
            "rustc": tool("rustc") or "MISSING",
            "abc": os.environ.get("ABC_BIN", "MISSING (set ABC_BIN or pass --abc-bin)"),
        },
        "external_validation": {
            "genus": os.environ.get("GENUS_BIN", "MISSING (set GENUS_BIN to a licensed installation)"),
            "status": "AVAILABLE" if os.environ.get("GENUS_BIN") else "SKIPPED",
        },
    }
    print(json.dumps(report, indent=2))
    return 0 if report["python"]["status"] == "PASS" else 1


if __name__ == "__main__":
    raise SystemExit(main())
