# 变更记录 · Changelog

本文件记录 MorionOS 面向用户的版本变更。版本串由内核
[`kernel/src/version.rs`](./kernel/src/version.rs) **单一维护**，shell 的 `uname` / `version`
命令经 `SYS_UNAME(51)` 读出。

格式参考 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)。

## [0.4.0-nogui] — 2026-10-01

**驱动 + 性能飞地路线**的首个收口版本；对外发布**无图形**变体（tag `v0.4.0-nogui`）。

### 新增

- **版本串（V1）**：新系统调用 `SYS_UNAME(51)` 报告系统名 / release / 构建号（构建号 = 构建时 git 短哈希，
  由 `Makefile` 注入 `MORION_BUILD`）；shell 新增 `uname` 与 `version` 命令；版本常量单一来源。
- **无图形变体（V2）**：`make NOGUI=1 ...` 注入编译期变体开关 —— release 串带 `-nogui`，shell **不开屏幕
  镜像**（输入/回显走串口），产物落在 `build/nogui/`；其余服务与全量回归口径不变。
- **I/O 端口能力（D0）**：`Capability::IoPort(base, len)`，给既有 `SYS_PORT_*` 加门禁（此前无门禁）。
- **用户态驱动底座（D1 / D2 / D3）**：通用设备授权 `device.rs`（`DeviceGrant` = BAR + 连续 DMA 块 + MSI-X
  参数，内核**无设备专属代码**）；`libdevice` 公共库（`grant`/`mmio`/`msix`/`virtio`）；两个真实驱动
  `net_srv`（virtio-net，域 16）与 `virtio_blk_srv`（virtio-blk，域 17）。
- **网络（N0–N3）**：`net_srv` 走 virtio-modern bring-up + MSI-X 中断收帧 + ARP 端到端自测（`NET1`）。
- **IOMMU / VT-d（E1a / E1b / E1c）**：ACPI DMAR 探测（RSDP → XSDT/RSDT → DRHD）；建根表/上下文表 + 恒等二级
  页表并打开 `GCMD.TE`，每个枚举到的 PCI 功能点都显式建立 `translated + 恒等` 上下文项；**E1c** 把目标设备
  （NVMe）的 DMA 权限收成**显式窗口 `[0, 3 GiB)`**（窗口外不建叶项），设备发起的窗口外 DMA 被 IOMMU 拒绝，
  并由空闲任务读 `FSTS` / `FRCD` 打印取证 —— 其余设备仍是 4 GiB 恒等，行为零变化。
- **运行期设备授权（D1b）**：新增 `SYS_DEVICE_INFO(52)` / `SYS_DEVICE_GRANT(53)` —— 域可在运行期查询本域
  设备并申请 `DeviceGrant`，含 `Mmio` 能力门禁；NVMe / `net_srv` / `virtio_blk_srv` 经统一入口
  `libdevice` 的 `DeviceGrant::load()` 取授权，行为零变化。

### 变更

- 内核终端在 `gfx_srv` 接管帧缓冲后只保留 COM1 输出（headless 回归不受影响）。
- `OUT_DIR` 默认值改为随变体切换（`build` / `build/nogui`）。

### 已知限制

- 无图形变体**仍加载** `gfx_srv`（引导器服务表与 init 监督均为固定 18 项）；本变体只关闭 shell 的
  屏幕镜像这条用户可见图形路径。真正的"不启 `gfx_srv`"留待后续重构。
- 飞地管理器（E2 / E3）**尚未实现**。

[0.4.0-nogui]: https://github.com/sheng-fs/morion-os/releases/tag/v0.4.0-nogui
