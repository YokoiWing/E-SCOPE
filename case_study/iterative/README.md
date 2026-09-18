# Iterative case study

This directory contains six prepared G0 inputs, the required rules and Liberty
assets, a compact Rust source tree, and the online policy runner. It contains no
saved final netlists or PPA reports.

Build and run:

```bash
CARGO_TARGET_DIR=/tmp/escope-iterative cargo build --release --locked \
  --manifest-path source/D1-series/Cargo.toml --bin run_generator_union_native
python3 run.py --case usb_phy --guide internal_v3 --mode execute \
  --out /tmp/iterative-usb-phy \
  --binary /tmp/escope-iterative/release/run_generator_union_native
```

`--case` accepts `usb_phy`, `simple_spi`, `systemcdes`, `usb_funct`, `aes_core`,
and `RISC`; `--guide` accepts `genlib` and `internal_v3`. Use `--mode plan` to
write the resolved plan without starting a search.

The Python layer selects profiles online and records process state. Rust performs
topology generation, pricing, conditional A2/O1, and acceptance. A new output
must receive fresh legality, CEC, and mapped-as-is PPA evaluation before it is
used in a paper table.
