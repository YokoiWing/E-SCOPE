# AreaPMO 最小复现

`run.py` 使用同一份冻结 native 二进制和同一套参数处理六例。默认从包内已冻结的 high-effort prepared AIG 开始，重新执行作者 GENLIB 的 emap2 映射得到 G0，再进行 AreaPMO 搜索；并非复制旧中间优化网表。原始 ABC high-effort 预处理不重复执行，其输出 AIG 和来源 SHA 已冻结。若要从未经预处理的原始 benchmark 开始，此包未包含该可选步骤。

```bash
python3 run.py --case usb_phy --mode plan --out /tmp/ap_plan
python3 run.py --case usb_phy --mode execute --out /tmp/ap_run
```

实际原参数：seed=5、preserve_depth=true、delay_awareness=true、max_divisors=256，最多100轮，至少10轮且连续3轮无可行面积改善停止。结束后在 G0 和满足 `delay <= G0 + 1e-9` 的本次轮次中取 `(area, round)` 最小者；不把最后一轮自动当作论文结果。不使用 Genus 排名，不按 case 改参数。

运行记录 PID、原命令、状态、日志和结果；默认每个子进程上限3600秒，可用 `--timeout-sec` 调整资源上限。中断仅终止自身进程组。独立 GENLIB 回读和 ABC CEC 检查 prepared AIG→G0→final；输出包含与冻结 G0/final 的 SHA 比较。CEC 按接口顺序匹配（`-n`），与原验证脚本一致。

`source/` 仅提取已使用的编译依赖，保留原头文件布局与许可证；没有作者仓库实验集、其他主程序或历史运行目录。发布仓库不分发预编译的 AreaPMO 或 ABC；构建 AreaPMO 源码，并用 `--abc-bin` 或 `ABC_BIN` 指向用户自行取得的 ABC。Genus 软件不包含在包中。

```bash
cmake -S source -B /tmp/areapmo_build -DCMAKE_BUILD_TYPE=Release
cmake --build /tmp/areapmo_build -j2
python3 run.py --case usb_phy --mode execute --out /tmp/ap_run \
  --areapmo-bin /tmp/areapmo_build/areapmo_native --abc-bin /path/to/abc
```

构建产物不覆盖默认冻结二进制。C++17、Linux x86-64 和相应系统运行库为必要环境。源码依赖取自已编译目标的依赖清单；当前构建与历史二进制是否逐字节相同需单独看验证记录，不能由编译成功推断。

发布整理期间已在隔离的 `/tmp` 构建目录成功完成 CMake Release 构建，并以
`usb_phy` 完成10轮真实 smoke、CEC PASS。原先打包的预编译二进制已删除，因此
本次不再声称源码重建产物与历史二进制逐字节相同。Iterative 源码重建的证据边界见其独立说明。
