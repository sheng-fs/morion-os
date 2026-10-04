# MorionOS 真机网络路线图（有线 r8169 + 无线 AX210）

> 目标：让 MorionOS 在**真机**上联网 —— 先**有线**（Realtek RTL8168/8111，笔记本自带网口），
> 再**无线**（Intel AX210：802.11 扫描/关联 + WPA2-PSK + 数据面）。
>
> 现状：QEMU 侧三台网卡（virtio-net / e1000e / e1000）已端到端，IPv4/IPv6/TCP/DNS/HTTP shell 命令齐备；
> 无线只有**抽象层**（`wifi_srv`，无 radio）。见 [plan-net-v6.md](plan-net-v6.md)、[roadmap-driver.md](roadmap-driver.md)。
>
> **关键约束（决定排序）**：QEMU **既没有 RTL8168、也没有任何 802.11 设备**。这两条线**只能在真机验证**
> —— 因此「真机迭代基础设施」（日志通道 / 安全模式 / 可观测命令 / 可引导介质）**必须先行**，
> 否则驱动开发在真机上是全盲的。

---

## 1. 缺口清单

| 缺什么 | 现状 | 影响 |
| --- | --- | --- |
| 真机日志通道 | 只有 COM1（`-serial file:`）；笔记本**无物理串口** | 真机调试全盲，日志拿不出发不了 |
| 安全模式 | app 自测会对 `/`（真机上可能是**你的 ESP**）做写/删 | 真机启动有**数据风险** |
| `lspci` | 无；内核只打印 `[OK] PCI devices found: N`（计数） | 真机看不到 AX210 / RTL8111 是否被识别 |
| 固件加载器 | **完全没有**（“固件”仅指 UEFI/ACPI） | AX210 必须载 `iwlwifi-ty-*.ucode` → 起不来 |
| 802.11 栈 | 无（`wifi_srv` 是空实现） | 扫描 / 关联 / WPA 全做不了 |
| RTL8168/8111 驱动 | 无（只有 virtio-net / e1000e / e1000） | 笔记本**有线口不可用** |

**已具备（可复用）**：通用设备授权 `device::grant` / `SYS_DEVICE_*`、MMIO + DMA 池 + MSI-X、
IPC + 能力、`intel_nic` 共享核心、NIC 表（`NIC_TABLE_VADDR`，加网卡不改协议栈）、
FAT32 写路径、xHCI/U 盘驱动、shell 与 `fs-regress.sh` 回归口径。

---

## 2. 阶段划分（每步独立可验证）

### Phase 0 — 真机迭代基础设施【先做，无它则后面全盲】

- **P0.1 真机日志通道**：内核 console 输出额外进一个**有界环形缓冲**（64 KiB，满了挤掉最老）；
  新增 `SYS_LOG_TOTAL` / `SYS_LOG_READ`；shell 加 `dmesg` —— **不带参数打印**，**带路径则写入文件**
  （`dmesg /boot.log`，走既有 FAT32 写路径）。
  真机流程 = 从 U 盘启动 → `dmesg /boot.log` → 拔盘插到宿主机 → 把 `boot.log` 发我。
- **P0.2 安全模式**：`make SAFE=1 ...`（编译期变体，同 `NOGUI=1`/`INSTALL=1` 的机制，产物落
  `build/safe/`）—— **`app` 自测整体跳过**，只报一行 `app: SAFE mode …`。
  理由：真机上内核会把**本机盘**当卷挂上，而 `app` 自测会创建/删除文件、拍快照、做分区写
  （`/` 很可能就是本机盘的 ESP）—— 那等于对着真实系统盘动手。跳过它，首次真机启动就是只读动作。
  **范围边界（诚实说明）**：本阶段只挡掉"自测"这条**自主**写路径；shell 里那些破坏性命令
  （`mkfs.mfs` / `part.*` / `rm` 等）仍然可用 —— 那是操作者显式输入，首次真机启动时不要敲即可。
  "`/` 只读挂载 + 内核级拒绝写"留待后续（`mount_srv`/`mfs_srv`/`block_srv` 联动，改动面更大）。
- **P0.3 `lspci`**：内核把启动期枚举结果留一份**只读快照**，shell `lspci` 经 `SYS_PCI_INFO`
  按索引打印（`bus:dev.func  vend:dev  class` + BAR0）。真机日志里就能看到
  `8086:2725`(AX210) 与 `10ec:8168`(RTL8111) 「看得见但没有驱动」。
- **P0.4 可引导介质**：`make usbimg` 做一块 **GPT + ESP 磁盘镜像**（把 `iso` 已建好的 64 MiB
  FAT32 ESP 原样写进 EFI 系统分区），给 `dd` 写盘命令与「关 Secure Boot」说明。
  （不用 isohybrid：它要求 El Torito 载入映像 ≤ 32 MiB，而 64 MiB 已是 FAT32 的最小可用尺寸。）

**Phase 0 验收（真机，你的笔记本）**：U 盘启动 → 屏幕出现内核日志与 shell → `lspci` 列出
AX210 与 RTL8111 → `dmesg /boot.log` 成功 → 宿主机能读到该文件。
（QEMU 侧验证：`lspci` 列出全部设备、`dmesg /d.log` 落盘 21753 字节、SAFE 变体 `uname` 显示
`0.4.0-safe` 且 `app: SAFE mode …`、`usbimg` 产出带 EF00 分区的 GPT 镜像 —— 均已实测。）

### Phase 1 — 有线：Realtek RTL8168/8111（r8169）

- 目的：笔记本**插网线即可联网**，复用现有全套协议栈与 shell 命令（`net` / `ping` / `dns` / `wget`）。
- 落地：`pci::find_rtl8168`（`10ec:8168` / `8161` / `8136`，取 **MMIO BAR2**）→ 新服务
  `r8169_srv`（**域 26**）+ `device::grant`；MMIO 寄存器 + 传统 16 字节描述符环（与 `intel_nic` 同族思路）；
  PHY 自协商等待；注册进 **NIC 表**（新增 `NIC_KIND_RTL8168`）。
- 自测：广播 ARP 问网关 → `RTL1 rtl8168 OK, MAC=…, ARP reply OK`（真机，marker `RTL1`）。
- **验收（真机）**：`net` 出现 `nic3 rtl8168 up`；`ping <网关>` 有 reply；`dns example.com` 解析成功；
  插拔网线有 link 状态变化日志。
- 注：QEMU 无此设备 → 只加真机路径；**QEMU 回归不受影响**（NIC 表里没有该条目）。

### Phase 2 — 无线基础设施

- **P2.1 固件加载器**：新服务 `firmware_srv`（**域 27**）—— 从 `/lib/firmware`（FAT32/MFS）
  读 blob → 校验 → 经 IPC 交给驱动，驱动用既有 `SYS_DMA_ALLOC` 把固件 DMA 进设备。
  固件**不入仓库**（Intel 固件不可再分发）→ 用户自备，放 U 盘 `/lib/firmware`。
- **P2.2 802.11 管理层**：把 `wifi_srv` 从「空实现」扩成真实的 **MLME 抽象** ——
  信道/频段/速率、BSS 扫描、认证/关联的状态机骨架、管理帧（beacon/probe/assoc）构造与解析。
  这一层**可在 QEMU 用合成帧做单元自测**（不需要真 radio）。
- **验收**：QEMU 内合成帧单测全过；真机验证留到 Phase 3。

### Phase 3 — Intel AX210（iwlwifi）

> ⚠️ **规模**：Linux 的 iwlwifi 是数万行、多年投入。本项目从零做，**拆成四小步**，每步都真机取证、
> 每步都可停下。**不要期望一次做完。**

- **P3.1 bring-up**：PCIe + CSR 访问 → 载入 ty 固件 → `alive` → `init` 固件上下文 →
  读 MAC / 固件版本。取证：`AX210 dev=8086:2725 fw=<ver> MAC=… alive=ok init=ok`。
- **P3.2 扫描**：下发 scan 命令、收 beacon/probe → `wifi scan` 列出**真实 BSS**（SSID/信道/RSSI/加密）。
- **P3.3 关联 + WPA2-PSK**：open 关联 → EAPOL 四步握手 → 安装 CCMP 密钥。
- **P3.4 数据面**：802.11 MSDU ↔ 以太帧转换 → 以 `NIC_KIND_WIFI` 注册进 **NIC 表** →
  上层（IPv4/IPv6/TCP/DNS/HTTP 与全部 shell 命令）**零改动**复用。
- **许可**：本项目 **MIT**；Linux iwlwifi 是 **GPLv2** → **不能直接抄代码**。
  按固件接口 / 公开文档**重写**；若确需移植，则该组件单独 GPL 并与主树隔离（需你确认）。

### Phase 4 — 无线生态（远期，未排期）

WPA3 / 802.1X 企业、regulatory 域、电源管理 / 省电、漫游、多 radio 并存。

---

## 3. 执行顺序

```
P0.1 → P0.2 → P0.3 → P0.4 ──►【真机首次启动取证：lspci + boot.log 发回】
        │
        └─► P1 r8169（真机有线联网）
                 │
                 └─► P2.1 固件加载 → P2.2 802.11 抽象 → P3.1 → P3.2 → P3.3 → P3.4 → P4
```

**建议第一步 = Phase 0（P0.1–P0.4）。** 做完你就能：真机启动 → `lspci` + `dmesg > boot.log` → 发我。
有了这条迭代回路，再决定先啃 r8169（快，能立刻有线联网）还是直接上 AX210（慢，但是你的最终目标）。

---

## 4. 验收口径（两套，必须分开）

| 类别 | 判据 |
| --- | --- |
| **QEMU 回归（每步必过）** | 四道门禁不变：`fmt/check/clippy` 0 warning；内核单测；`make … run-nvme` 构建；`fs-regress.sh` → `REGRESS_EXIT=0`、失败明细「无」 |
| **真机验收（真机专有硬件才跑）** | Phase 0：`lspci` 命中 + `boot.log` 产出；Phase 1：`RTL1 … ARP reply OK` + `net`/`ping`/`dns`；Phase 3：`AX210 … init=ok` → 扫描有 BSS → 关联 → 数据面 `net` 出现 wifi 链路 |
| **不退化** | `intel_nic` 三台 QEMU 网卡、`NIC` 表、`net`/`ping`/`ping6`/`dns`/`wifi` 命令与现有 marker 逐字不变 |

---

## 5. 风险

1. **真机差异**：VT-d / ACPI / 中断路由 / GOP 在真机与 QEMU 不同（[roadmap-driver.md](roadmap-driver.md) §7 已记）→ 稳态与「只读安全模式」必须先行。
2. **真机日志**：无串口是**头号风险**；P0.1 若在真机收不到（如 xHCI 真机不可用），需回退到「屏幕拍照」或第二块 USB。
3. **固件不可再分发**：AX210 固件由用户自备；载入路径、版本匹配要单独处理。
4. **AX210 工作量**：新世代固件 API 复杂，**先做到「能枚举 + 固件跑起来」**即算 Phase 3 的实质进展。
5. **QEMU 不可测**：r8169 与 802.11 无法在 CI 覆盖 → 必须把「QEMU 必过」「真机必过」两套判据写进流程。
6. **许可**：MIT 主树不得混入 GPLv2 代码（Phase 3 尤其注意）。
