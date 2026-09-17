# Strict reproducibility audit

Original audit date: 2026-09-16

Original audited commit: `5c47a662c84378bbf3e78ba17687c4c303cce811`

Post-audit closure status updated: 2026-09-17

This audit uses **strict reproduction** to mean that a reader can start from
the packaged experimental inputs, run the documented code, regenerate the
candidate set and selection, perform the stated legality/equivalence and
mapped-as-is PPA evaluation, and derive the paper result. Recomputing a number
from a saved CSV is recorded separately as **saved-evidence replay**.

## Verdict

| Paper experiment | Saved-evidence replay | Fresh search entry | Strict fresh end-to-end reproduction |
|---|---:|---:|---:|
| Table III and Figure 6, 28-point main result | PASS | PASS | READY; 20/28 freshly checked |
| Figure 7, `epfl_adder` Pareto fronts | PASS | PASS | READY; full queue not rerun |
| Figure 8, selected-method ablation | PASS | PASS | READY; full queue not rerun |
| Table IV, six-circuit case study | PASS | PARTIAL PASS | NO |

The original audit found no complete fresh path. Figure 7, Figure 8, and the
main experiment were closed afterward with unified search→freeze→Genus→formal
entries. Their expensive full queues remain intentionally unlaunched during
packaging.

## Checks performed from a clean checkout

The audit used a no-hardlink clone of the pushed tree. The checkout contained
no local source edits or generated experiment files.

- Release inventory: 915/915 files passed `scripts/verify_release.py`.
- Identity/path scan: no absolute local home path, former internal method name,
  author account, or local username was found in tracked release content.
- Python syntax: all Python under `scripts/`, `experiments/`, and `case_study/`
  compiled successfully.
- Saved evidence and plots:
  - Table III/Figure 6: 28 G0 and 28 selected-netlist hashes passed; D²AP
    geometric mean `0.8210095475250041`; runtime ratio
    `1.4583649253561954`.
  - Figure 7: 20 G0 hashes passed; 86/113/64 frozen candidates for D×A,
    D²AP, and D×P²; 283 plotted rows carried formal PASS; plots regenerated.
  - Figure 8: all 28 method labels and G0 hashes passed; exact reductions
    `13.377061053592222%` and `4.645592689697486%`; exact and paper-clipped
    plots regenerated.
  - Table IV: all 24 rows matched; 1,000 packaged case-study checksums and the
    saved Genus report bindings passed.
- Source builds:
  - Main/Pareto/ablation Rust source built all four required Release binaries
    with `--locked` in an isolated target directory.
  - Case-study AreaPMO C++ source built in an isolated CMake directory.
  - The independent case-study Iterative Rust source built offline with
    `--locked`.
- Minimal real executions:
  - Main `epfl_ctrl`, one Iterative round plus one Conquer smoke: PASS,
    22.01 s concurrent wall time; external validation remained pending.
  - Pareto `p0800`, D×A, one Iterative round: PASS, one accepted round,
    140.89 s; external validation remained pending.
  - Ablation `epfl_ctrl`, selected Conquer Phase-II-only round: PASS;
    external validation remained pending.
  - AreaPMO `usb_phy`: 10 rounds, prepared-input→G0 and G0→final CEC PASS;
    G0 and selected-final SHA-256 both matched the package exactly.

The full queues were deliberately not run. Cadence Genus was unavailable in
the clean audit environment, and the commercial program is correctly absent
from the repository.

## Main experiment closure added after the audit

`experiments/main28/run.py` now starts both branches from the same G0, freezes
the Iterative checkpoint at Conquer completion, runs mapped-as-is Genus and
Yosys/ABC validation, applies the frozen policy, and stops or resumes Iterative.
The historical native D2AP mode, Iterative configuration hash, and Table III
data-path-delay objective were restored and guarded during verification.

Twenty points were exercised with isolated branch CPU sets. All twenty
recovered the paper method and selected-netlist SHA. The remaining eight points
were not freshly rerun, so the full 28-point queue remains a ready execution
path rather than a completed independent rerun. The policy is retrospective
and in-sample, and `epfl_adder/p0800` remains a method-only transfer from the
older adder anchor.

The paper says one CPU core, while the public full-run entry defaults to two
internal jobs. Saved runtime evidence reproduces the plotted 1.46× number, but
the partial fresh run does not establish a new one-core runtime measurement.

## Pareto experiment closure added after the audit

`experiments/pareto_adder/run.py reproduce` now executes the resumable
39-trajectory queue, reconstructs the cross-round Internal-NLDM frontier
union, freezes it before external evaluation, runs mapped-as-is Genus and
Yosys/ABC validation, and generates fresh Figure 7 CSV/PNG/SVG outputs.

The freezer was replayed against the complete historical search trees and
selected exactly the same ordered 86/113/64 netlist SHA lists. The Genus parser
matched all 263 historical candidate reports. A real one-round p0800 D×A test
then froze seven candidates; all 20 G0 plus seven candidates completed fresh
Genus, all seven candidates passed legality/cold-read/whole-network CEC, and
the fresh figure was generated. This validates the full code path without
claiming that the expensive 39 trajectories were rerun during packaging.

## Ablation experiment closure added after the audit

`experiments/ablation/run.py reproduce` now runs or resumes all 84 tasks,
freezes Iterative checkpoints and Conquer finalist pools, automatically pairs
the Conquer Phase-I implementation, invokes mapped-as-is Genus and Yosys/ABC
validation, selects formal-clean outputs, and generates fresh Figure 8
CSV/PNG/SVG/PDF files.

Historical complete-tree staging and policy replay selected all 79 nonblank
result SHAs in the saved Figure 8 table. A real fresh c432 three-mode run then
completed 13/13 Genus points and 12/12 non-G0 formal checks; its selected Phase-I-only and
Phase-II-only SHAs exactly matched the saved experiment. The expensive full
84-task queue was not rerun. Figure 8 still intentionally uses the older
`epfl_adder/t0_0.95x` G0/full reference, and the exact/paper plot split records
the `epfl_i2c` display clipping instead of hiding it.

## Why the case study is not strict end-to-end

The case-study package has the strongest fresh path. AreaPMO rebuilds and, on
`usb_phy`, regenerated the exact G0 and final netlist with independent CEC.
The Iterative source and frozen online route also build from the clean tree.

The complete experiment still starts from frozen, high-effort prepared AIGs;
the original ABC preprocessing step is absent. Full Iterative trajectories are
not guaranteed to repeat bit-for-bit because of floating-point ties and the
historical two-second local rewrite limit. Genus evaluation is a separate,
manual licensed-tool step. A new result may reuse packaged PPA only if its
netlist SHA matches the recorded input; otherwise the table cannot be rebuilt
without a fresh external run.

## Work needed for a strict claim

1. Run the remaining eight main points through the fresh online controller and
   reconcile its worker count with the paper's one-core runtime statement.
2. Resolve the Figure 8 adder-anchor version difference or state in the paper
   that the ablation uses the earlier G0.
3. For Table IV, either package the original pre-AIG preparation command and
   input or define the prepared AIG as the experiment's public starting point.
