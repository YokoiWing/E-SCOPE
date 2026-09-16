# 外部 Genus 评价

`evaluate_genus.tcl` 是实际使用的原脚本，四份 Liberty 为原 TT/RVT/0.7V/25°C 库。包内报告来自 Genus 23.14-s090_1。保留 BUFx2 PI driver、零 driver input slew、5.76 输出负载、1000ps 虚拟时钟与统一 activity。时延采用含 PI driver adjustment 的 output arrival；1000ps 是评价虚拟时钟，不能替代每条搜索路径的冻结时延约束。

有同版本 Genus 和许可证时，在本目录运行，例如：

```bash
export EGG_USB_GENUS_OUT=/tmp/new_usb_phy_genus
export EGG_USB_GENUS_LIBS="$(pwd)/lib/asap7sc7p5t_AO_RVT_TT_nldm_211120.lib|$(pwd)/lib/asap7sc7p5t_INVBUF_RVT_TT_nldm_220122.lib|$(pwd)/lib/asap7sc7p5t_OA_RVT_TT_nldm_211120.lib|$(pwd)/lib/asap7sc7p5t_SIMPLE_RVT_TT_nldm_211120.lib"
export EGG_USB_GENUS_INPUTS="usb_phy/final|$(realpath ../results/usb_phy/iterative_internal_v3/final.v)|usb_phy"
test ! -e "$EGG_USB_GENUS_OUT" && timeout 1800 genus -no_gui -files evaluate_genus.tcl -log /tmp/new_usb_phy_genus.log
```

脚本只读 mapped 网表，没有综合、重新映射、优化命令。每个报告目录的 `INPUT.json` 绑定实际评价的输入 SHA。新网表必须重新核查 CEC、零 unresolved、前后实例与连线一致；只有新SHA等于证据输入SHA才可直接复用原指标。本次整理未重跑 Genus。
