# Case-study runners

This directory contains only source, prepared inputs, configuration, and run
drivers. Saved outputs and historical Genus reports are intentionally omitted.

- `areapmo/`: build and run the AreaPMO baseline.
- `iterative/`: build and run the Iterative flow with either guide.
- `evaluation/evaluate_genus.tcl`: mapped-as-is Genus evaluation script; set
  `E_SCOPE_GENUS_LIBERTY` to a locally supplied Liberty file.

Example:

```bash
python3 case_study/areapmo/run.py \
  --case usb_phy --mode plan --out /tmp/areapmo-plan

python3 case_study/iterative/run.py \
  --case usb_phy --guide internal_v3 --mode plan \
  --out /tmp/iterative-plan
```

Use `--mode execute` after building the corresponding source and providing the
external tools described by the component README.
