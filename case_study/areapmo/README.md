# AreaPMO case study

This directory contains the prepared AIG inputs, GENLIB data, the source subset
needed to build AreaPMO, and one runner. It contains no saved optimization
outputs.

Build and run:

```bash
cmake -S source -B /tmp/escope-areapmo -DCMAKE_BUILD_TYPE=Release
cmake --build /tmp/escope-areapmo -j2
python3 run.py --case usb_phy --mode execute --out /tmp/areapmo-usb-phy \
  --areapmo-bin /tmp/escope-areapmo/areapmo_native --abc-bin /path/to/abc
```

Use `--mode plan` to inspect the command without running it. The six case names
are listed by `python3 run.py --help`.

The runner recreates G0 from the prepared AIG, runs AreaPMO with seed 5, and
chooses the minimum-area feasible round under the G0 delay. It then runs ABC CEC
for prepared-input-to-G0 and G0-to-final equivalence. External PPA evaluation is
separate and requires user-supplied Genus and Liberty files.
