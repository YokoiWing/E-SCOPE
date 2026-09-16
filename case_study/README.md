# AreaPMO 与 Iterative 六例复现包

本目录统一保存六个开发电路的 AreaPMO 基线、Iterative 冻结执行代码和最新论文结果证据。相关运行材料位于 `iterative/`。

- [areapmo/](areapmo/README.md)：AreaPMO 源码、实际编译依赖和统一入口。
- [iterative/](iterative/README.md)：Iterative 六例双指导模型的冻结在线入口；原策略、二进制、jobs、阶段 cap 不变。
- [evaluation/](evaluation/README.md)：实际使用的 Genus mapped-as-is Tcl 与四份冻结 Liberty。
- [results/TABLE.csv](results/TABLE.csv)：相对同一 G0 的论文数据；[TABLE.json](results/TABLE.json) 保存原始数值、网表 SHA 和评价口径，[table.tex](results/table.tex) 为六例表。
- `results/<case>/`：G0、AreaPMO 和两份 Iterative 最终网表，搜索选择证据、CEC、Genus 报告。大文本以 `.gz` 无损保存。
- [VERIFICATION.json](VERIFICATION.json)：本次整理实际做过的验证；[SOURCE_INDEX.json](SOURCE_INDEX.json) 记录证据来源。

本次提取后 AreaPMO 六例均完成真实运行：全部轮次 binding、G0与最终SHA一致，CEC通过；独立源码构建也重建出相同二进制。Iterative完成12份搬迁计划检查，使用此前完整12路径验证证据。

在仓库根目录运行：

```bash
python3 case_study/verify.py
python3 case_study/areapmo/run.py --case usb_phy --mode execute --out /tmp/new_areapmo_usb_phy
python3 case_study/iterative/run.py --case usb_phy --guide genlib --mode execute --out /tmp/new_iterative_usb_phy_genlib
python3 case_study/iterative/run.py --case usb_phy --guide internal_v3 --mode execute --out /tmp/new_iterative_usb_phy_internal
```

两个执行器均支持 `--mode plan`，不调用优化或 EDA；输出目录必须不存在。整目录可搬迁。包内不包含 Genus 软件与许可证；同 SHA 的新结果可复用本包历史外部评价，SHA 改变必须重新评价，不能直接套用表中数字。

## 论文当前口径

百分比为 `100 × (final / G0 − 1)`，负数表示下降。GENLIB-guided 那列是作者 GENLIB A/D，没有 power。Internal-V3-guided 那列是 **Genus mapped-as-is A/D/P**，不是 Internal 估算；Genus 不反馈搜索。

本版采用已完成新运行的实际网表。AES 的 Genus A/D/P 为 `1073.001 / 1261.5 ps / 0.00200470 W`，下降 `1.76% / 12.14% / 4.64%`；RISC 为 `3069.95 / 9347.7 ps / 0.00804435 W`，下降 `1.85% / 0.50% / 1.62%`。其余十条 Iterative 最终 SHA 与旧表一致。systemcdes GENLIB 面积按作者浮点向上取整为132.22。

12条 Iterative 实际运行已完成；相对旧目标是7条逐轮一致、3条最终SHA一致、2条不同SHA且通过新的cold/CEC/Genus验证。采用这12份实际结果更新论文，不等于证明以后每次运行都逐位相同，也不等于旧目标12/12精确复现。事后拟合树仅在这六个开发电路验证，不能宣称未经调参的泛化。

优先使用包内冻结配置与源码构建入口；浮点近并列和2秒局部rewrite上限仍可能造成分歧。独立源码构建成功不证明重建相同二进制。此整理没有再次运行耗时数十小时的Iterative全部12路径。

这里只覆盖 usb_phy、simple_spi、systemcdes、usb_funct、aes_core、RISC。上级十例论文表的其他四例未纳入本包，也未改动其数据。旧单例 case study 与公开作者 artifact 审计保留在 [压缩历史归档](../../artifacts/areapmo_flow_audit_20260915.tar.gz)（包内 `final_paper/areapmo_flow_audit/legacy_case_study_20260915/`），不参与当前结果选择；本地 AreaPMO 基线复现不能声称精确恢复了作者外部论文的全部原始 Table 1。
