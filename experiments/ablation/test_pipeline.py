"""Small synthetic checks for ablation selection and CSV export."""

import csv
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

import pipeline as p


class PipelineTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.g0 = self.netlist("g0.v", "assign y = a;")
        self.point = {
            "benchmark": "example", "anchor": "a0", "selected_method": "Conquer",
            "g0_path": str(self.g0), "g0_sha256": p.sha256(self.g0), "round_cap": 3,
        }
        self.manifest = self.root / "manifest.json"
        p.dump(self.manifest, {"points": [self.point]})
        self.override = patch.object(p, "MANIFEST", self.manifest)
        self.override.start()
        self.addCleanup(self.override.stop)

    def netlist(self, name, statement):
        path = self.root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(f"module example(input a, output y); {statement} endmodule\n")
        return path

    def make_full(self, method="Conquer"):
        directory = self.root / "full/example__a0"
        path = self.netlist("full/example__a0/selected/mapped.v", "assign y = a;")
        p.dump(directory / "PLAN.json", self.point)
        p.dump(directory / "status.json", {"status": "complete"})
        p.dump(directory / "selected/receipt.json", {
            "method": method, "candidate_id": "NORMAL_POST", "selected_sha256": p.sha256(path),
        })
        return directory

    def test_full_result_checks_method_and_netlist(self):
        directory = self.make_full("Iterative")
        with self.assertRaisesRegex(RuntimeError, "same method"):
            p.full_result(directory.parent, self.point)
        self.make_full()
        path, receipt = p.full_result(directory.parent, self.point)
        self.assertEqual(receipt["candidate_id"], "NORMAL_POST")
        path.write_text("changed")
        with self.assertRaisesRegex(RuntimeError, "SHA mismatch"):
            p.full_result(directory.parent, self.point)

    def test_conquer_pairs_current_full_candidate(self):
        pre = self.netlist("example/phase1-only/search/points/example/a0/normal/round_00/leader_pre_a2.v",
                           "assign y = ~a;")
        path, name = p.fixed_search_netlist(self.root, self.point, "phase1-only",
                                          {"candidate_id": "NORMAL_POST"})
        self.assertEqual((path, name), (pre, "NORMAL_PRE"))
        atomic = self.netlist("atomic.v", "assign y = a;")
        pool = self.root / "example/phase1-only/search/frozen_finalists.json"
        p.dump(pool, {"finalists": [{"candidate_id": "NEW_ATOMIC", "netlist": str(atomic),
                                   "sha256": p.sha256(atomic)}]})
        self.assertEqual(p.fixed_search_netlist(self.root, self.point, "phase1-only",
                                               {"candidate_id": "NEW_ATOMIC"})[0], atomic)
        with self.assertRaisesRegex(RuntimeError, "no unique counterpart"):
            p.fixed_search_netlist(self.root, self.point, "phase1-only",
                                   {"candidate_id": "MISSING"})

    def evaluated_rows(self, method):
        self.point["selected_method"] = method
        p.dump(self.manifest, {"points": [self.point]})
        p.dump(self.root / "frozen/manifest.json", {"benchmarks": ["example"]})
        rows = []
        def add(kind, mode, candidate, area, round_number=0):
            rows.append({
                "id": candidate, "kind": kind, "benchmark": "example", "mode": mode,
                "candidate_id": candidate, "round": round_number, "sha256": p.sha256(self.g0),
                "netlist": str(self.g0),
                "ppa": {"boundary_delay_ps": 2.0, "area_um2": area, "power_uw": 3.0},
            })
        add("g0", "g0", "G0", 10)
        add("full", "full", "FULL", 5)
        for mode in p.MODES:
            add("candidate", mode, mode + "_R1", 12, 1)
            if method == "Iterative" or mode == "no-pi-drive-expansion":
                add("candidate", mode, mode + "_R2", 11, 2)
        p.dump(self.root / "external/results.json", {"results": rows, "failures": []})
        p.dump(self.root / "formal/results.json", {
            "results": [{"id": row["id"], "receipt": {"formal_equivalence": "PASS"}}
                        for row in rows if row["kind"] != "g0"], "failures": [],
        })

    def test_iterative_uses_best_checkpoint_without_g0_clamping(self):
        self.evaluated_rows("Iterative")
        result = p.write_csv(self.root)
        with (self.root / result["data_csv"]).open() as stream:
            row = next(csv.DictReader(stream))
        self.assertAlmostEqual(float(row["full_over_phase_i_only"]), 5 / 11)
        self.assertAlmostEqual(float(row["phase_i_only_d2ap_over_g0"]), 1.1)
        with (self.root / result["ppa_csv"]).open() as stream:
            rows = list(csv.DictReader(stream))
        self.assertEqual(len(rows), 5)
        self.assertEqual(float(rows[1]["d2ap"]), 60)
        self.assertFalse(list(self.root.rglob("*.png")))

    def test_only_conquer_no_drive_can_fall_back_to_g0(self):
        self.evaluated_rows("Conquer")
        result = p.select_results(self.root)[0]
        self.assertEqual(result["modes"]["no-pi-drive-expansion"]["candidate_id"], "G0")
        self.assertEqual(result["modes"]["phase1-only"]["candidate_id"], "phase1-only_R1")
        status = self.root / "formal/results.json"
        data = p.load(status)
        data["failures"] = [{"id": "phase1-only_R1", "error": "timeout"}]
        p.dump(status, data)
        with self.assertRaisesRegex(RuntimeError, "validation contains failures"):
            p.write_csv(self.root)


if __name__ == "__main__":
    unittest.main()
