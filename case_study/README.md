# Case study

The case study compares AreaPMO with Iterative using GENLIB and internal PPA
guides on six circuits: `usb_phy`, `simple_spi`, `systemcdes`, `usb_funct`,
`aes_core`, and `RISC`.

1. Build and run [AreaPMO](areapmo/README.md).
2. Build and run [Iterative](iterative/README.md), once for each guide.
3. Measure the output netlists with the common
   [Genus evaluation setup](evaluation/README.md).

The runners include their inputs and search configurations. Use `--mode plan`
in place of `--mode execute` to inspect a run before starting it.
