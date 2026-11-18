# 许可证与模块边界

Morion OS 采用**按模块许可**（per-module licensing），不是全仓库单一协议。目标是：

- 核心保持 MIT，允许闭源应用、商业集成和再分发；
- 飞地模块用 Apache-2.0，拿到明示专利授权与专利诉讼终止条款；
- 第三方驱动/固件保留原协议，不把 GPL 传染进主仓库。

> 本文是工程约定，不构成法律意见；涉及专利布局或商业授权时请咨询知识产权律师。

## 1. 模块 → 协议

| 路径 | 协议 | 说明 |
|------|------|------|
| `boot/`、`kernel/`、`user/` | **MIT** | 核心可信基、用户态服务与库；见根目录 [LICENSE](../LICENSE) |
| `user/apps/enclave/` | **MIT** | 性能飞地示例（申请设备窗口 → 读寄存器）；飞地 ABI 目前定义在内核与 `libdevice`（`SYS_ENCLAVE_MAP` / `SYS_ENCLAVE_DETACH`） |
| 飞地实现（未来） | **Apache-2.0** | 独立服务 / 独立二进制时启用，含 §3 明示专利授权与专利诉讼终止；见 [LICENSES/Apache-2.0.txt](../LICENSES/Apache-2.0.txt) |
| `user/drivers/wlan/` | **MIT** | 无线驱动抽象层（station 状态机 / 固件契约 / 芯片表；`wifi_srv` 的 L2 与控制面契约） |
| 无线厂商后端（未来） | 随上游 | 优先 BSD-3-Clause / ISC / Apache-2.0；每个后端保留来源与原始版权头 |
| 厂商固件（未来） | 厂商固件许可 | 不入主仓库，按需下载或单独打包；逐文件保留 LICENSE，不并入 MIT 代码 |
| 第三方代码 | 原协议 | 保留 LICENSE、版权头、来源 commit；不得“洗掉”后改协议 |

对外统一表述：

> Morion OS core is MIT-licensed. The enclave module is Apache-2.0. Third-party drivers and firmware retain their own licenses.

## 2. 目录与文件规范

采用 [REUSE](https://reuse.software/) 风格：

```text
LICENSE                     # MIT（核心）
NOTICE                      # 本项目与 Apache-2.0 模块声明
LICENSES/
  MIT.txt
  Apache-2.0.txt
  BSD-3-Clause.txt          # 实际引入对应后端时再添加
  ISC.txt
docs/licensing.md           # 本文件
```

- 每个源文件头写 `SPDX-License-Identifier`，例如：
  - 核心：`// SPDX-License-Identifier: MIT`
  - 飞地实现：`// SPDX-License-Identifier: Apache-2.0`
- 每个 crate 的 `Cargo.toml` 写自己的 `license` / `license-file`，不要只在 workspace 根写一个。
- 新增模块时，同时更新本文件的映射表；CI 建议加 `reuse lint` 与 `cargo-deny`。

## 3. 飞地模块（Apache-2.0）

- 飞地作为**独立服务/独立二进制**运行，通过内核的飞地 syscall（`SYS_ENCLAVE_MAP` / `SYS_ENCLAVE_DETACH`）与 MIT 的 `libdevice` 通信；
- 不要把 Apache-2.0 实现静态链接进 MIT 内核二进制；边界越清晰，许可解释越简单；
- Apache-2.0 已包含贡献者的专利授权；对外发布前先完成专利检索/申请，避免公开破坏新颖性；
- 若未来需要“商业版 / 社区版”双许可，再单独评估 CLA；当前不引入 CLA，核心与飞地均可用 DCO（`Signed-off-by`）。

## 4. Wi-Fi 驱动策略

优先顺序：

1. **移植 BSD / ISC 驱动**（OpenBSD / NetBSD / FreeBSD / Haiku 等），逐文件核对 SPDX；
2. **Clean-room 重写**：只参考数据手册、寄存器说明与公开协议，不复制 GPL 源码；
3. **GPL 驱动隔离**：仅当没有宽松协议替代时使用，且必须满足：
   - 放在**独立仓库**（如 `morion-wifi-gpl`），单独 LICENSE；
   - 作为独立用户态进程，通过 MIT 的 `wifi_srv` L2 / 控制面 IPC（`NET_REQ` / `WIFI_REQ`）通信；
   - 不静态链接、不进入默认 ISO、不作为主仓库子目录随版本发布；
   - 文档明确“可选组件，GPL-2.0，需自行构建”。

GPL-2.0 与 MIT/Apache-2.0 的组合分发有传染风险；GPL 后端不得进入主仓库或主镜像。厂商固件不入主仓库，保留厂商许可，按需下载或单独打包。

## 5. 兼容性红线

| 组合 | 能否合并分发 |
|------|--------------|
| MIT + Apache-2.0 | ✅ 保留 NOTICE |
| MIT + BSD / ISC | ✅ |
| MIT + MPL-2.0 | ✅（MPL 文件级 copyleft） |
| MIT + GPL-2.0 | ❌ 不建议 |
| Apache-2.0 + GPL-2.0 | ❌ 不兼容 |
| Apache-2.0 + GPL-3.0 | ✅（整体变 GPLv3） |
| CDDL + GPL | ❌ 不兼容 |

“一个仓库放多种协议”本身没问题；风险在于是否把它们编译/链接成同一个衍生作品。目录分离、进程分离、独立分发可以降低风险，但不能完全消除 GPL 的解释风险。

## 6. 贡献

- 提交即表示同意所修改目录的 SPDX 协议；
- 新增文件必须带 SPDX 头；
- 不要向主仓库提交 GPL 代码、厂商固件或来源不明的二进制；
- 第三方代码请在 PR 中注明来源、原协议与 commit。
