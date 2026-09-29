# Morion OS 驱动与飞地路线图 (草案)

> 目标：把「**驱动**」与「**飞地 (Enclave)**」补齐 —— 驱动从"内核里写死的 NVMe 专属代码"
> 变成**通用设备授权 + 用户态驱动**；飞地从零到"设备直通 + IOMMU 硬件隔离 + 飞地管理器"。
> 二者完备后，打一个**无图形界面**的版本号（GUI 留到之后再上，风险大）。
>
> **驱动顺序（已定）**：先 **D1 通用设备授权**（✅ 已完成），再**从网络开始** —— 也就是
> **N0 域表扩容 → N1 PCI 通用查找 + 设备声明 → N2 `net_srv`(virtio-net) 驱动 → N3 自测**；
> 之后才是块设备的第二形态（D2/D3 virtio-blk）与其它驱动（RTC 等）。
>
> 方向：内核只提供机制（**设备资源授权 + MMIO/DMA 映射 + 中断路由 + IPC + 能力**），
> 设备怎么驱动、飞地怎么管全在用户态。与文件系统 / 图形子系统同构。

---

## 1. 目标链路

```
应用 / 服务 ── libdevice(双形态) ──► 驱动服务 (设备共享, 走 IPC)
                     │
                     └──(直通)──► 飞地应用 ──(MMIO/DMA 直写)──► 设备
                                          ▲
                    enclave-mgr ── SYS_ENCLAVE_* ── 内核 (建域 + IOMMU 映射 + IRQ 授权)
```

最终可验证：
1. 新驱动**不改内核**即可落地（通用 `device grant`）；
2. 飞地应用**直通**一个设备，**零内核陷落**地下发命令；
3. IOMMU 把该设备的 DMA **锁死**在飞地的物理窗口内 —— **越界 DMA 被硬件拒绝**。

---

## 2. 现状与关键前置

**已具备**

| 项 | 现状 |
| --- | --- |
| 用户态驱动 | 只有 **2 个**：NVMe 块设备（[block_srv.rs](../user/srv/src/block_srv.rs)，域 5）、键盘（[kbd.rs](../user/srv/src/kbd.rs)，域 4 —— 内核读 PS/2 scancode → IRQ1 投递 → 用户态解码） |
| PCI / MSI-X | [arch/pci.rs](../kernel/src/arch/pci.rs)：bus/dev/func 枚举、能力链表遍历、MSI-X 定位/使能 |
| MMIO 授权 | `Capability::Mmio(页对齐物理基址)` + `SYS_MAP_MMIO(21)`（4 KiB 页 + `NO_CACHE` + `NO_EXECUTE`） |
| 中断 | `SYS_REGISTER_IRQ(14)` / `SYS_IRQ_POLL(34)` / `SYS_MSIX_ENABLE(35)` / `SYS_IRQ_WAIT(36)`（含多向量 `wait_any`） |
| 域 / 能力 | `SYS_SPAWN_ELF(37)` / `SYS_SPAWN_ELF_AT(41)` / `SYS_SPAWN_ELF_MODULE(43)` / `SYS_DOMAIN_*` / `SYS_FRAME_FREE(40)`；能力系统**可随 IPC 传递**（移交句柄 + 委派） |
| 设备保留帧 | [frame_allocator::pin_range](../kernel/src/memory/frame_allocator.rs)（G6 引入）：登记"任何路径不得释放"的保留区间 |
| 服务自愈 | `init` 监督 10 个服务；「同域重启」`restart_in_place` 可用 |

**缺口**

- **设备 bring-up 曾是内核专属**（D1 已解决）：原 `kernel/src/nvme.rs` 写死了"DMA 7 页 / BAR0 4 页 / 3 条 MSI-X / 配置结构 + 约定虚拟地址" —— 加新驱动**必须改内核**。现已抽成通用 [`device.rs`](../kernel/src/device.rs)（`DeviceGrant` + `grant()`），NVMe 只剩一条声明式需求。
- **没有通用 DMA 池原语**：内核直接 `frame_allocator` 分配物理连续帧、写进设备配置结构交出去；用户驱动没有"申请物理连续 DMA 缓冲"的正规通道。
- **没有 I/O 端口通道**：无 `SYS_IO_IN/OUT`，无 I/O 端口能力 —— 纯 port-mapped 设备（如部分旧网卡/串口）无从下手。
- **没有设备注册表 / 资源描述标准**：设备命名、BAR 资源、IRQ 的"标准化描述"不存在。
- **域号已用满**（N0 已扩）：`domain::BOOT_DOMAINS` 原为 16，0..15 全部分配（block=5 / fat32=6 /
  app=7 / shell=8 / mount=9 / tmpfs=10 / mfs=11 / ext2=12 / exfat=13 / init=14 / gfx=15）。已扩到
  **17** 给 `net_srv`(16) 腾号（各表是 `Vec` 且按需增长，机制上可行；**boot 侧 `SERVICE_FILES`/
  模块表已同步**）。再加驱动时继续按需扩。
- **没有网络驱动**：无网卡驱动、无 PCI 网络类设备查找、无 virtqueue。
- **没有 IOMMU**（`grep` 内核无任何 DMAR / VT-d 代码）→ 直通设备的 DMA **无法隔离**，这是飞地的**安全前提**。
- **没有 LibDevice**：驱动核心逻辑与"服务/直通"运行时未分离。
- **没有飞地管理器**、没有 `create_enclave` 之类的内核原语。
- **没有版本串 / 没有无图形界面构建开关**。

---

## 3. 关键决策（待定 → 定）

| # | 问题 | 候选 | 决定 |
|---|---|---|---|
| D1 | 设备授权形态 | (a) 继续在内核加设备专属代码；(b) **通用 `SYS_DEVICE_*` 原语 + 资源描述结构** | **(b)**：内核按 PCI 地址解析 BAR/能力、分配 DMA 池、配置 MSI-X，把 `Mmio/Dma/Irq` 能力 + 资源描述交给请求域；`nvme.rs` 降为**第一个消费者**。这是 E1/E2 的直接前置 |
| D2 | 用户态 DMA 内存 | (a) 内核代分配物理连续帧写进配置结构；(b) **用户态 `SYS_DMA_ALLOC`**（物理连续 + 帧锁定） | **(b)**：驱动自己申请，内核只保证"物理连续 + 不被回收"；飞地的 DMA 窗口也复用同一原语 |
| D3 | 端口 I/O | (a) 不做；(b) `SYS_IO_IN/OUT` + `Capability::IoPort(range)` | **按需 (b)**：不阻塞主线，但补上 port-mapped 设备的通道（低优先） |
| D4 | 驱动代码复用 | (a) 各服务各写一份；(b) **抽 `libdevice` crate**，服务形态与直通形态共享核心 | **(b)**：寄存器/队列/协议与运行时解耦，两种形态共享 ≥90% 代码（对应 README「Anykernel 双形态」） |
| D5 | 飞地隔离基础 | (a) IOMMU(VT-d)；(b) CHERI | **先 (a) IOMMU**：QEMU 可验、无需特殊硬件；CHERI 作为将来的可选增强 |
| D6 | 飞地管理器权限 | (a) 任何应用可申请飞地；(b) **唯一特权服务 `enclave-mgr`** 持 `SYS_ENCLAVE_*` | **(b)**：内核只认管理器的飞地原语；申请走 IPC 让管理器校验策略 |
| D7 | 无图形版本命名 | tag 形式 | **`v0.4.0-nogui`**（版本串由 `SYS_UNAME` 报告，见 V1） |
| D8 | 图形在无 GUI 版本中去留 | (a) 删掉；(b) **保留代码、默认不启 `gfx_srv` / 不开屏幕镜像** | **(b)**：`gfx_srv` 成为可选的 15 号服务，构建/引导配置开关控制 |

---

## 4. 设计要点

### D0 — I/O 端口能力（可选，低优先）
- `Capability::IoPort(base, len)` + `SYS_IO_IN(port, width)` / `SYS_IO_OUT(port, width, val)`。
- 与 `Mmio` 同构：启动期静态授权，运行期逐次校验端口落在授权区间内。

### D1 — 通用设备授权 ✅ 已完成（boot 路径；运行期 syscall 路径见 D1b）
- **内核**：把 `nvme.rs` 的设备专属逻辑抽成通用原语 —— 已落地为 [`kernel/src/device.rs`](../kernel/src/device.rs)（**`nvme.rs` 已删除**）：
  - `DeviceGrant` 描述结构（BAR + 连续 DMA 块 + MSI-X 参数，**不含**设备语义）+ `GrantRequest` + `grant()`；
  - `grant_empty()` 降级、`enable_msix()`（`SYS_MSIX_ENABLE` 转调它）；
  - **MSI 向量段按设备分配**（游标 `MSI_NEXT` 从 `idt::MSI_VECTOR_BASE` 递增，段尽则降级轮询）—— 原来每台设备都从段首拿固定几条，只够一台 NVMe。
  - `main.rs` 里 NVMe 只剩一条**声明式需求**（`bar_pages: 4 / dma_pages: 7 / msix_vectors: 3`）。
- **用户态**：[`block_srv`](../user/srv/src/block_srv.rs) 读通用描述后**自行推导队列布局**（`DMA_OFF_*` 页偏移）—— 设备专属知识回到驱动域。
- **验证**：纯重构，行为零变化 —— 全量 FS 回归 `SELFTEST DONE`×1、`FAILED`/`PANIC` 0、`irq_cmds=28672 poll_cmds=0`（MSI 向量段仍 `0x50..0x52`）。
- **D1b（待做）**：把授权从"boot 期内核代做"变成**运行期 syscall**（`SYS_DEVICE_INFO/GRANT`）—— 这是 virtio-blk 与飞地（E2）真正"不改内核"的前提。
  - 原计划：`SYS_DEVICE_INFO(pci_addr)` → 回 `vendor/device/class`、各 BAR 的 `(base, len, is_mmio)`、MSI-X 能力位置；`SYS_DEVICE_GRANT(pci_addr, want_irq, want_msix_vectors)` → 建立设备域绑定、回**资源描述结构**。

### N — 网络驱动（virtio-net，本批主线，**最先做**）

选 virtio-net 的理由：QEMU 原生支持、规范简单（比真网卡少一堆寄存器）、且能顺手把
**"用通用授权加一台新驱动"**这条路走通（正是 D1 的目的）。

#### N0 — 前置：域表扩容（`BOOT_DOMAINS` 16 → 17）✅ 已完成
- 域 0..15 已满，`net_srv` 需要新域号（取 **16**）。各表（`domain::DOMAINS`、`cap::CAP_TABLE`/
  `HANDLE_TABLE`、`ipc::MAILBOXES`、`irq::ANY_MASK`）都是 `Vec` 并按需增长，**机制已够**；
  实际改的是"谁在启动时创建它"：`domain::BOOT_DOMAINS`（16 → 17）、内核 `main.rs` 多建一个域、
  引导器 `SERVICE_FILES` 加 `(16, "net_srv")`、`Makefile` 的 `SRV_NAMES`、`morion-srv` 的
  feature/`[[bin]]`、`init` 的 `SUPERVISED`（10 → 11）。
- **踩到的坑**：内核 `ipc::init`/`cap::init`/`pager::init` 原来传**字面量 16**，而这三张表都按
  **域 id 下标**访问 —— 扩了建域数而不同步表长，访问 16 号域会**越界 panic**。已改成一律用
  `domain::BOOT_DOMAINS`。
- ✅ 验证：启动日志 `[OK] 17 service ELFs loaded (boot modules)` + `[up] net_srv (domain 16)` +
  `init: supervising …/net_srv`；全量回归全绿（`SELFTEST DONE`×1、`FAILED`/`PANIC` 0、
  `irq_cmds=28672 poll_cmds=0`、`sgdisk` No problems found）。运行时 spawn 域号随之 16 → 17，
  FS-28 用动态基线故不受影响。

#### N1 — PCI 通用查找 + 设备声明 ✅ 已完成
- `arch/pci.rs`：`read_bar0` 抽成通用 `read_bar(index)`（顺带修一个坑：**高于 4 GiB 的 64 位 BAR**
  低 dword 只剩标志位，不能拿它判"未实现"—— virtio-net 的 BAR4 = `0xC000000000` 正踩这个坑）；
  新增按类查找 `find_net`（网络类 `0x02` + vendor `1AF4` + device `1041`/`1000`，取 **BAR4**，
  即 virtio-modern 的 common/notify/device/isr 配置区）。
- `main.rs`：给域 16 加一条**声明式需求**（与 NVMe 同款）`bar_pages: 4 / dma_pages: 8 /
  msix_vectors: 2 / label: "net"`；`net_srv` 读 `DeviceGrant` 打印取证。
- QEMU：`-netdev user,id=n0 -device virtio-net-pci,netdev=n0,mac=52:54:00:12:34:56`（回归脚本
  与 `make run-nvme` 同步）。取证：`[OK] virtio-net modern BAR4=0x000000C000000000` +
  `net: device grant bar_vaddr=0x8000820000 … dma_bytes=32768`。
- **已知缺口（→ N2）**：virtio-net 的 **MSI-X 表在 BAR1**，与设备 BAR 不是同一根；D1 的 `grant`
  目前只映射一根 BAR 且要求 `table_bir == 0`，故现在**降级轮询**（日志
  `net: MSI-X table in BAR1, not BAR0 -> polling`）。N2 需让授权支持"另映射 MSI-X 表所在 BAR"。

#### N2 — `net_srv`（用户态 virtio-net 驱动）
**N2a（已完成）—— 设备 bring-up + 轮询取帧**
- 新增内核窄接口 **`SYS_DEVICE_CONFIG_READ(50)`**（只放行"读**自己那台**设备"的配置空间 dword）：
  驱动据此**自行解析** virtio PCI 能力（common/notify/ISR/device 四个区域都在内核交给的 BAR 里），
  内核因此**不必懂 virtio 协议**。
- `net_srv`：复位 → `ACKNOWLEDGE|DRIVER` → 协商特性（必须 `VIRTIO_F_VERSION_1`，另取
  `VIRTIO_NET_F_MAC`）→ `FEATURES_OK` → 读 device cfg 取 **MAC** → 建 RX(0)/TX(1) virtqueue
  （三环落在内核交出的**连续 DMA 块**里，深度 8）→ 投 RX 缓冲 → `DRIVER_OK` → 轮询 used 环
  取帧 + 补投 + 统计。
- 取证：`net: virtio-net up MAC=563412005452 num_queues=3 rx=8 tx=8`、`net: DRIVER_OK …`，
  回归里**真的收到帧**（`net: rx frames=1`）。

**N2b（待做）—— MSI-X 中断化**
- virtio-net 的 **MSI-X 表在 BAR1**（与设备 BAR 不同根）：D1 的 `grant` 目前只映射一根 BAR 且
  要求 `table_bir == 0`，故现在打印 `net: MSI-X table in BAR1 -> polling`（**降级轮询**）。
- 要做的：让 `grant` 支持"另映射 MSI-X 表所在 BAR"（新增 `DeviceGrant.msix_table_vaddr`），
  驱动写表项 → 设 `queue_msix_vector` → `SYS_MSIX_ENABLE` → 注册向量 → 用 `SYS_IRQ_WAIT` 收
  RX 中断（`SYS_IRQ_POLL` 快路径 + 轮询兜底）。

#### N3 — 自测（端到端取证）
- 发一帧 **ARP 请求**（问 QEMU user-net 网关的 MAC）→ 等 **ARP 应答** → 断言收到了长度/类型
  正确的回帧；串口打 `app:` marker（如 `NET1 virtio-net up, MAC=…, ARP reply OK`）。
- 驱动起来后即由 `init` 监督（域 16），与其它服务一致。

### D2 — LibDevice 双形态
- 新 crate `user/libdevice/`：把驱动核心（寄存器定义、队列环、协议状态机）做成**不依赖"我在服务进程还是应用里"**的库。
- **服务形态**：`block_srv` 链接它，对外暴露 IPC 协议（现状）。
- **直通形态**：飞地应用链接它，运行时直接 MMIO/DMA（E3）。
- 先拿 NVMe 把两种形态都跑通，再抽 virtio-blk。

### D3 — 第二个真实驱动：virtio-blk（**排在网络之后**）
- 目的：验证 **D1 通用路径**（新驱动不改内核）+ 给回归加一条独立可自动化的块设备取证。
- QEMU 原生支持（`-drive if=virtio` / `-device virtio-blk-pci`），virtio 规范简单（legacy/modern 二选一，先 modern）。
- 自测：挂一块 virtio-blk 盘 → 读第一扇区签名 / 写读回校验 → 串口 marker。

### E1 — IOMMU (Intel VT-d)
- 解析 ACPI **DMAR** 表 → 找到 DRHD（各 IOMMU 单元与管辖范围）→ 建**根表/上下表** → 为设备建 **DMA 重映射域**。
- 与 D1 结合：`SYS_DEVICE_GRANT` 在飞地场景下把设备的 DMA 权限绑到一个 **IOVA 窗口**（只映射飞地自己的缓冲），其余一律**拒绝**。
- 取证：让飞地故意对**未映射**地址发 DMA → 观察 IOMMU 报错（QEMU `-machine q35,intel-iommu=on` 可复现），且系统**不受影响**。
- ⚠️ 与内核恒等映射的关系：内核自身 DMA（如 NVMe bring-up 早期）要保持可翻译，需明确哪些域**绕过**或**放行**。

### E2 — 飞地管理器 `enclave-mgr`
- 唯一持 `SYS_ENCLAVE_*` 的特权服务（D6）。
- `create_enclave(mem_size, devices[], caps[])`：校验策略 → 内核建保护域 → 映射设备 MMIO → 建 IOMMU 域 → 注册 IRQ → 装 LibDevice → 返回飞地入口能力。
- 生命周期：暂停 / 恢复 / 销毁 / 资源回收；**每飞地一条日志流**（构造/退出/异常）推到审计侧，飞地自身不可篡改。

### E3 — 示例飞地应用
- 直通接管一个设备（先用块设备 / virtio-blk），证明：
  1. **零内核陷落**：命令提交全在用户态 MMIO 写；
  2. **IOMMU 隔离**：错误 DMA 被拒且不伤系统；
  3. 与普通路径**行为一致**（同一 LibDevice，两种形态）。

### V1 — 版本串
- `SYS_UNAME`（或复用既有信息接口）返回系统名/版本/构建号；shell `uname` / `version`。
- 版本常量单一来源（内核 + README 同步）。

### V2 — 无图形界面版本收口
- 构建开关：`gfx_srv` 不启、shell 不开屏幕镜像（D8）—— 其余服务与回归口径不变。
- CHANGELOG + README「版本」段 + **打 tag `v0.4.0-nogui`**。
- 验收即"全量 FS 回归 + 驱动自测 + 飞地取证"全绿。

---

## 5. 执行顺序（每步独立可回归）

1. **D1 通用设备授权**（✅ 已完成，boot 路径）：抽 `device.rs` 通用原语 + 描述结构，把 `nvme` 迁过去；**功能零变化**，回归口径不变。运行期 syscall 路径（D1b）随网络/后续驱动一起做。
2. **N0 域表扩容**（✅ 已完成）：`BOOT_DOMAINS` 16 → 17 + boot 侧服务表同步 + `net_srv` 骨架。
3. **N1 PCI 通用查找 + 设备声明**（✅ 已完成）：按类找 virtio-net（BAR4）+ `device::grant` 声明 + QEMU 加网卡；**MSI-X 表在 BAR1** 的缺口留给 N2。
4. **N2 `net_srv`**：virtio-net 初始化（✅ N2a：PCI 能力 / MAC / virtqueue / `DRIVER_OK` / 轮询取帧；N2b：MSI-X 中断化）。
5. **N3 网络自测**：ARP 请求 → 应答取证（`NET1`）。
6. **D2 LibDevice 双形态**：抽 `libdevice` crate，`block_srv` 改为服务形态消费者。
7. **D3 virtio-blk**：用通用路径加第二个驱动 + 自测（含 D1b 的运行期 `SYS_DEVICE_*`）。
8. **D0（可选）I/O 端口能力**。
9. **E1 IOMMU (VT-d)**：DMAR + 重映射域 + 越界 DMA 拒绝取证。
10. **E2 enclave-mgr**：飞地生命周期 + 日志流 + 审计。
11. **E3 示例飞地**：直通接管设备，零陷落 + 隔离取证。
12. **V1 版本串** + **V2 无图形版本收口**（CHANGELOG + tag）。

---

## 6. 验收（沿用现有口径 + 新增）

| 项 | 判据 |
|---|---|
| 不退化 | `make fmt` / `check` / `clippy` 全 0；内核单测全过；**全量 NVMe FS 回归**仍 `SELFTEST DONE`×1、`FAILED`/`PANIC` 0、`irq_cmds == cmds` 且 `poll_cmds = 0`、宿主 `sgdisk -v` "No problems found" |
| 串口 sink | 任何迁移都不得让关键行从 `-serial file:` 消失（沿用图形的 D6 硬约束） |
| D1 | `nvme` 驱动走 `SYS_DEVICE_*`，内核里不再有 NVMe 专属结构；块设备自测与全量回归不变 |
| N0 | 扩容后全量回归仍全绿（存活域数基线、`SELFTEST DONE`×1、`FAILED`/`PANIC` 0）—— ✅ `[OK] 17 service ELFs loaded` + `[up] net_srv (domain 16)` |
| N1 | 内核找到 virtio-net 并把 `DeviceGrant` 交给域 16：日志有 `[OK] virtio-net modern BAR4=…` 与 `net: device grant …`；全量回归不退化 —— ✅ |
| N2a | net_srv 读到 MAC、RX/TX virtqueue 建好、`DRIVER_OK`，并能取到帧 —— ✅ `net: virtio-net up MAC=… rx=8 tx=8`、`net: rx frames=1` |
| N2b | MSI-X 中断化（表在 BAR1，需 D1 支持另映射 MSI-X 表 BAR）：出现 `net: MSI-X prepared …` 且中断驱动 |
| N3 | 收到 ARP 应答（`NET1 virtio-net up, MAC=…, ARP reply OK`） |
| D3 | virtio-blk 读写自测通过（`app:` marker），且未改内核设备代码 |
| E1 | 飞地越界 DMA 被 IOMMU 拒绝；系统与其他域不受影响 |
| E3 | 飞地直通命令成功 + 隔离取证同时成立 |
| V2 | 无图形构建下全量回归通过；`SYS_UNAME` 报告 `v0.4.0-nogui` |

---

## 7. 主要风险

1. **IOMMU 环境差异**：QEMU `intel-iommu` 与真机 VT-d 行为不同（尤其：哪些域绕过翻译、AGP/华硕怪癖、RMRR 保留区）；先只在 QEMU 取证，真机另立验收。
2. **内核自身 DMA 的可翻译性**：给设备套上 DMA 重映射后，**内核早期/自身**访问该设备会失败 —— 必须先理清"哪些访问走 IOMMU、哪些绕过"。
3. **中断到飞地**：MSI-X 目标地址/向量要按**飞地域**配置，比现在的全局向量段更复杂；先支持单飞地单设备。
4. **DMA 池物理连续**：当前靠 `frame_allocator` 连续帧分配；内存碎片化后可能失败 —— 需要预留池或 `RMRR`/预留区。
5. **通用化不改行为**：D1 是**纯重构**，必须保证块设备路径字节级行为一致（回归是主要防线）。
6. **回归仍依赖 COM1**：任何驱动迁移要显式保留串口输出。
7. **virtio 规范分支**：legacy vs modern、不同 QEMU 版本差异 —— 固定一种并钉住 QEMU 命令行。
8. **域表扩容的连带**：`BOOT_DOMAINS` 与引导器 `SERVICE_FILES`/模块表必须**同改同增**，否则启动期服务清单与内核建域数对不上；自测里"存活域数"断言也要同步。
9. **QEMU 网络配置**：`-netdev user` 的 ARP/DHCP 行为、网关/MAC 随 QEMU 版本变化 —— 自测要按**收到应答**判定，不钉死具体 MAC/IP。

---

## 8. 相关文档

- 内核速查：[dev-reference.md](dev-reference.md)（`SYS_MAP_MMIO`、`paging::map_mmio`、`irq.rs`、能力系统）
- 应用开发：[app-dev-guide.md](app-dev-guide.md)
- 架构（飞地 / Anykernel 目标）：[architecture.md](architecture.md)「性能飞地」「设备驱动程序——Anykernel 双形态」
- 图形子系统路线（前置，已完成）：[roadmap-gfx.md](roadmap-gfx.md)
- 文件系统路线（本轮之前的批次）：[roadmap-fs.md](roadmap-fs.md)
