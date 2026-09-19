# AreaPMO case study

Run from the repository root:

```bash
cmake -S case_study/areapmo/source -B /tmp/escope-areapmo \
  -DCMAKE_BUILD_TYPE=Release
cmake --build /tmp/escope-areapmo -j2
python3 case_study/areapmo/run.py \
  --case usb_phy --mode execute --out /tmp/areapmo-usb-phy \
  --areapmo-bin /tmp/escope-areapmo/areapmo_native --abc-bin /path/to/abc
```

The runner recreates G0 from the prepared AIG, runs AreaPMO with seed 5, and
chooses the minimum-area feasible round under the G0 delay. ABC checks
prepared-input-to-G0 and G0-to-final equivalence. The six case names are
listed by `python3 case_study/areapmo/run.py --help`.

Measure output PPA using the common [Genus setup](../evaluation/README.md).
