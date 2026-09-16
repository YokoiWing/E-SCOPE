# Iterative 冻结流程最小独立包

唯一运行入口是 `run.py`，Python 只负责冻结策略决策、进程边界与记录；拓扑生成、定价、conditional A2/O1 和接受逻辑在 Rust 中执行。

```bash
python3 run.py --case usb_phy --guide genlib --mode plan --out /tmp/usb_phy_plan
python3 run.py --case usb_phy --guide genlib --mode execute --out /tmp/usb_phy_new_run
```

`--case` 支持 usb_phy、simple_spi、systemcdes、usb_funct、aes_core、RISC；`--guide` 为 genlib 或 internal_v3。输出目录必须不存在。不同路径可使用独立输出目录并行；每条路径原 jobs、native round cap 和环境均保留。Ctrl-C/SIGTERM 只清理本次启动的进程。

## 目录

- `run.py`：薄 Python 在线入口，无原仓库 import、拟合或批量实验依赖。
- `data/policy.json`：逐特征分支的冻结树与13个执行 profile，字节 SHA 不变。
- `data/routes.json`：12份固定执行档、G0/资产定位及只读历史 observer。case 名只定位输入和执行档；树按在线特征发出动作。原 cap 与进程边界属于执行档，不能声称由树自行推导。
- `data/results.json`：本次12个最终结果的 SHA 与指标；只用于结果检查，不用于候选选择。
- `assets/`：11个去重后的 G0、Liberty 与规则资产。
- `source/`：独立 Cargo 构建所需 Rust 编译单元；无其他实验入口、候选目录、图表和批量队列。
- `CHECKSUMS.json`：包内不可变文件的 SHA256；启动前逐项验证。
- `VERIFICATION.json`：本次提取、重定位与实际首轮检查结果。

搜索使用本次生成的 parent。每个 native 阶段连续运行，保留原 cap；portfolio 阶段每轮 cold restart，按冻结的可行性/area/power/delay/ID 规则选本次候选池。observer 不纠正选择；分歧后继续冻结策略。接口恢复逻辑保留。最终 SHA 不同只标记待新评价，不能复用对应 Genus 指标。

## 源码构建

需要 Rust/Cargo 和 C 工具链。发布仓库不分发带本机构建路径的冻结二进制；先从源码构建：

```bash
CARGO_TARGET_DIR=/tmp/iterative_build cargo build --release \
  --manifest-path source/D1-series/Cargo.toml --bin run_generator_union_native
python3 run.py --case usb_phy --guide genlib --mode execute --out /tmp/usb_phy_new_run \
  --binary /tmp/iterative_build/release/run_generator_union_native
```

已有 crates 缓存可加 `--offline`。只保留一个 Cargo binary target。共享编译单元中仍有内嵌 helper/测试和条件分支；没有对核心函数做激进裁剪，以免改变搜索语义。关键源码采用冻结快照，其余依赖来自提取时的工作区，出处见 `SOURCE_PROVENANCE.json`。当前没有完整历史编译环境的证明，**源码构建成功不等于已证明重建相同二进制或相同轨迹**。

## 结果与适用范围

本次旧表对照：7条逐轮精确、3条最终SHA一致、2条Genus原始值略不同。`data/results.json` 采用本次实际结果，包括新 aes_core/RISC Internal 网表。Internal-V3 指导的最终A/D/P来自Genus mapped-as-is；GENLIB那组为独立GENLIB A/D。证据汇总写入 `data/results.json`，Genus工具及历史大目录不随本最小搜索包附带；当前结果的关键外部报告与CEC证据位于相邻 `../results/`。

本包是六个开发电路上事后拟合并冻结的调度，不是未经调参的泛化方法。浮点近并列与原生2秒局部rewrite时限仍可能导致轨迹变化；打包不能保证每次都得到相同SHA。10/12旧目标最终SHA一致的实际证据不能改写成旧表12/12精确复现。

提取后只做了计划/决策一致检查和独立副本的真实首轮 smoke，没有再跑耗时约39小时的全12路径搜索。原始全路径证据留在本仓库 `paper_results_v8/areapmo_sixcase_frozen_flow_repro_v1/`，运行本包不读取该目录。
