# Iterative case study

Run from the repository root:

```bash
CARGO_TARGET_DIR=/tmp/escope-iterative cargo build --release --locked \
  --manifest-path case_study/iterative/source/optimizer/Cargo.toml \
  --bin run_generator_union_native
python3 case_study/iterative/run.py \
  --case usb_phy --guide internal_v3 --mode execute \
  --out /tmp/iterative-usb-phy \
  --binary /tmp/escope-iterative/release/run_generator_union_native
```

Use `--guide genlib` for the GENLIB-guided variant and a separate output
directory. Available cases are `usb_phy`, `simple_spi`, `systemcdes`,
`usb_funct`, `aes_core`, and `RISC`.

The input netlists, rules, and search libraries are included. The runner
selects profiles during search and writes `final.v` and `RESULT.json` in the
output directory. Use the common [evaluation setup](../evaluation/README.md)
for external PPA measurements.
