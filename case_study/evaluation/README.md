# External Genus evaluation

`evaluate_genus.tcl` evaluates mapped netlists without resynthesis. It uses a
BUFx2 PI driver, zero input slew, 5.76 output load, a 1000 ps virtual clock, and
uniform activity.

Cadence Genus and its license are not distributed. The required ASAP7 Genus
Liberty files are also omitted; provide their paths through
`EGG_USB_GENUS_LIBS`. Provide evaluation jobs through `EGG_USB_GENUS_INPUTS` and
an unused output directory through `EGG_USB_GENUS_OUT`.

```bash
export EGG_USB_GENUS_LIBS="/path/AO.lib|/path/INVBUF.lib|/path/OA.lib|/path/SIMPLE.lib"
export EGG_USB_GENUS_INPUTS="usb_phy/final|/tmp/iterative-usb-phy/final.v|usb_phy"
export EGG_USB_GENUS_OUT=/tmp/usb-phy-genus
genus -no_gui -files evaluate_genus.tcl -log /tmp/usb-phy-genus.log
```

Check CEC, unresolved cells, and the reported input SHA for every new netlist.
