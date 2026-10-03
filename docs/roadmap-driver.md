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
| 用户态驱动 | **5 个**：NVMe 块设备（[block_srv.rs](../user/srv/src/block_srv.rs)，域 5）、键盘（[kbd.rs](../user/srv/src/kbd.rs)，域 4 —— 内核读 PS/2 scancode → IRQ1 投递 → 用户态解码）、virtio-net 网卡（[net_srv.rs](../user/srv/src/net_srv.rs)，域 16，N0–N3b）、virtio-blk（[virtio_blk_srv.rs](../user/srv/src/virtio_blk_srv.rs)，域 17，D3）、**AHCI/SATA**（[ahci_srv.rs](../user/srv/src/ahci_srv.rs)，域 18，D4，读+写并经 IPC 接进块服务卷层） |
| PCI / MSI-X | [arch/pci.rs](../kernel/src/arch/pci.rs)：bus/dev/func 枚举、能力链表遍历、MSI-X 定位/使能 |
| MMIO 授权 | `Capability::Mmio(页对齐物理基址)` + `SYS_MAP_MMIO(21)`（4 KiB 页 + `NO_CACHE` + `NO_EXECUTE`） |
| 中断 | `SYS_REGISTER_IRQ(14)` / `SYS_IRQ_POLL(34)` / `SYS_MSIX_ENABLE(35)` / `SYS_IRQ_WAIT(36)`（含多向量 `wait_any`） |
| 域 / 能力 | `SYS_SPAWN_ELF(37)` / `SYS_SPAWN_ELF_AT(41)` / `SYS_SPAWN_ELF_MODULE(43)` / `SYS_DOMAIN_*` / `SYS_FRAME_FREE(40)`；能力系统**可随 IPC 传递**（移交句柄 + 委派） |
| 设备保留帧 | [frame_allocator::pin_range](../kernel/src/memory/frame_allocator.rs)（G6 引入）：登记"任何路径不得释放"的保留区间 |
| 服务自愈 | `init` 监督 11 个服务（含 `net_srv`）；「同域重启」`restart_in_place` 可用（`block_srv` / `virtio_blk_srv` 因内核侧设备映射未登记为保留而暂不监督） |

**缺口**

- **设备 bring-up 曾是内核专属**（D1 已解决）：原 `kernel/src/nvme.rs` 写死了"DMA 7 页 / BAR0 4 页 / 3 条 MSI-X / 配置结构 + 约定虚拟地址" —— 加新驱动**必须改内核**。现已抽成通用 [`device.rs`](../kernel/src/device.rs)（`DeviceGrant` + `grant()`），NVMe 只剩一条声明式需求。
- **没有通用 DMA 池原语**：内核直接 `frame_allocator` 分配物理连续帧、写进设备配置结构交出去；用户驱动没有"申请物理连续 DMA 缓冲"的正规通道。
- **没有 I/O 端口通道**：无 `SYS_IO_IN/OUT`，无 I/O 端口能力 —— 纯 port-mapped 设备（如部分旧网卡/串口）无从下手。**→ 已由 D0 补上**：`Capability::IoPort(base, len)` + 既有 `SYS_PORT_*`（22–25）门禁。
- **没有设备注册表 / 资源描述标准**：设备命名、BAR 资源、IRQ 的"标准化描述"不存在。**→ 资源描述标准已由 D1/D1b 补上**：`DeviceGrant` 即标准化描述（BAR + 连续 DMA 块 + MSI-X 参数 + `label` 命名），D1b 又加了域→设备绑定表（`domain → bus/dev/func`）。
- **域号扩容**（N0 / D3 已做）：`domain::BOOT_DOMAINS` 原为 16，0..15 全部分配（block=5 / fat32=6 /
  app=7 / shell=8 / mount=9 / tmpfs=10 / mfs=11 / ext2=12 / exfat=13 / init=14 / gfx=15）。**N0** 扩到
  **17** 给 `net_srv`(16) 腾号、**D3** 扩到 **18** 给 `virtio_blk_srv`(17) 腾号、**D4** 扩到 **19** 给
  `ahci_srv`(18) 腾号（各表是 `Vec` 且按需增长，
  机制上可行；**boot 侧 `SERVICE_FILES`/模块表已同步**）。再加驱动时继续按需扩。
- **没有网络驱动**（N0–N3b 已解决）：`net_srv` 走通用授权 + virtio-modern，ARP 端到端自测（`NET1`）+ **最小 IPv4 栈**（IPv4 头构造/解析 + ICMP echo 收发 + UDP，端到端 `NET2`）；TCP 未做。
- **没有 IOMMU**（`grep` 内核无任何 DMAR / VT-d 代码，E1a 只加了**探测**）→ 直通设备的 DMA **无法隔离**，这是飞地的**安全前提**（重映射域与拒绝取证 = E1b/E1c）。**→ 已由 E1b 落地**（建根表/上下文表 + 恒等二级页表、打开 `GCMD.TE`）与 **E1c**（目标设备窗口收成 `[0, 3 GiB)`、越界 DMA 被拒并留证）。
- **没有 LibDevice**（D2 已解决首批）：`user/libdevice` 抽出 `grant`/`mmio`/`msix`，三个驱动共用；设备**语义**（vring 等）留 D2b 去重。
- **没有飞地管理器**、没有 `create_enclave` 之类的内核原语。
- **没有版本串 / 没有无图形界面构建开关**。**→ 已由 V1 落地**（`SYS_UNAME(51)` + shell `uname`/`version`）与 **V2**（`make NOGUI=1`，release 串带 `-nogui`、shell 不开屏幕镜像、产物落 `build/nogui/`）。

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

### D0 — I/O 端口能力 ✅ 已完成
- `Capability::IoPort(base, len)` + 复用既有的 `SYS_PORT_IN8/IN16/OUT8/OUT16`（22–25，此前**无门禁**）。
- 与 `Mmio` 同构：启动期静态授权；区别是端口是**纯平坦地址空间**，故按**半开区间** `[base, base+len)` 匹配（`cap::has_port`），而不是像 `Mmio` 那样逐页。
- 引导期按需授权：`block_srv` `0x1F0..0x1F8`（IDE PIO 回退路径）、`mfs_srv` / `exfat_srv` `0x70..0x72`（CMOS RTC 时间戳）；被拒返回 `u64::MAX`。
- 取证：`app: D0 port capability gate OK (ungranted I/O port denied)`（app 无 `IoPort`，读 `0x71` 被拒）+ 内核单测（区间半开 / 编码越界拒绝）。

### D1 — 通用设备授权 ✅ 已完成（boot 路径；运行期 syscall 路径见 D1b）
- **内核**：把 `nvme.rs` 的设备专属逻辑抽成通用原语 —— 已落地为 [`kernel/src/device.rs`](../kernel/src/device.rs)（**`nvme.rs` 已删除**）：
  - `DeviceGrant` 描述结构（BAR + 连续 DMA 块 + MSI-X 参数，**不含**设备语义）+ `GrantRequest` + `grant()`；
  - `grant_empty()` 降级、`enable_msix()`（`SYS_MSIX_ENABLE` 转调它）；
  - **MSI 向量段按设备分配**（游标 `MSI_NEXT` 从 `idt::MSI_VECTOR_BASE` 递增，段尽则降级轮询）—— 原来每台设备都从段首拿固定几条，只够一台 NVMe。
  - `main.rs` 里 NVMe 只剩一条**声明式需求**（`bar_pages: 4 / dma_pages: 7 / msix_vectors: 3`）。
- **用户态**：[`block_srv`](../user/srv/src/block_srv.rs) 读通用描述后**自行推导队列布局**（`DMA_OFF_*` 页偏移）—— 设备专属知识回到驱动域。
- **验证**：纯重构，行为零变化 —— 全量 FS 回归 `SELFTEST DONE`×1、`FAILED`/`PANIC` 0、`irq_cmds=28672 poll_cmds=0`（MSI 向量段仍 `0x50..0x52`）。
- **D1b ✅ 已完成**：把授权从"boot 期内核代做"变成**运行期 syscall**（`SYS_DEVICE_INFO` / `SYS_DEVICE_GRANT`，号 52/53）。
  - `SYS_DEVICE_INFO(a1)` → 回打包的 `vendor/device/class`（`vendor<<48 | device<<32 | class24`）；`a1 = u64::MAX` 表示"本域设备"，否则按 `pci_addr`（`bus<<16 | dev<<8 | func`）精确匹配；无设备 / 不属于本域返回 `0`。
  - `SYS_DEVICE_GRANT(a1)` → 定位本域设备 → **能力门禁**（须持有该设备 BAR 的 `Mmio` 凭证，由 `grant()` 一并签发；被拒时内核日志 `dev: device grant denied (no Mmio capability)`）→ 幂等确认描述页 `magic` 后返回描述页虚拟地址（`DeviceGrant`），无设备 / 被拒返回 `0`。
  - **资源仍在 boot 期由声明式 `grant()` 一次性备好**（域号是 ABI 且域在 boot 期即建好，运行期重复分配会与既有映射冲突）；运行期 syscall 提供的是**申请 + 门禁 + 幂等确认**的完整语义，`main.rs` 的 `device::grant` 声明**不变**。
  - 用户侧 `user/libdevice/src/grant.rs` 的 `DeviceGrant::load()` 改为发 `SYS_DEVICE_GRANT` 后读回描述（libdevice 仍**零依赖**：本地编号 + 裸 `syscall` 指令）。**三个驱动（`block_srv` NVMe / `net_srv` / `virtio_blk_srv`）共用这一个入口，因此零改动即完成迁移**。
  - 验证：纯行为等价 —— 全量回归 `SELFTEST DONE`×1、`FAILED`/`PANIC` 0、`irq_cmds == cmds` 且 `poll_cmds = 0`、`NET1 … ARP reply OK`、`VBLK1 … sig=ok, rw=ok` 与改造前逐字一致；内核单测新增 3 条（`pci_addr` 编解码边界 / 设备选择 / INFO 打包）。

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

**N2b（已完成）—— MSI-X 中断化**
- D1 的 `grant` 现在支持"表不在设备 BAR 上"：按 MSI-X 能力的 `BIR` 读出那根 BAR、非缓存地映射
  到新窗口 `DEVICE_MSIX_VADDR`（`USER_BASE + 0x84_0000`），并把窗口基址写进新的
  `DeviceGrant.msix_table_vaddr`（表与设备 BAR 同根时 = `bar_vaddr`，故 NVMe 行为不变）。
- `net_srv`：写 MSI-X 表项（RX→表项 0、TX→表项 1）→ 设 `queue_msix_vector` → 注册两条向量 →
  `SYS_MSIX_ENABLE` → 收帧走中断（`SYS_IRQ_POLL` 快路径 + `SYS_IRQ_WAIT` 阻塞，200ms 超时
  回落重扫）；掩码按 `向量段基址 - MSI_VECTOR_BASE` 整体左移（多设备各占一段：NVMe `0x50..0x52`、
  net `0x53..0x54`）。
- 取证：`net: MSI-X prepared vectors=0x53..0x54 table_bar=1 table_vaddr=0x8000840000`、
  `net: MSI-X enabled vectors=0x53..0x54`；NVMe 仍 `irq_cmds=28672 poll_cmds=0`。

#### N3 — 自测（端到端取证）✅ 已完成
- `net_srv` 起来后**自己**发一帧**广播 ARP 请求**（问 QEMU user-net 网关 `10.0.2.2` 的 MAC，
  源 `10.0.2.15`）→ 主循环收帧 → 校验是以太类型 `0x0806`、oper=`reply`、发送方 IP = 网关 →
  打自测标记 `NET1 virtio-net up, MAC=52:54:00:12:34:56, ARP reply OK`。这条链路同时验证了
  **TX（描述符 + avail + 门铃）→ 设备发包 → slirp 应答 → RX（used 环）**整条通路。
- **踩到的坑**：virtio-net 包头长度对 **modern**（`VIRTIO_F_VERSION_1`）设备恒为 **12 字节**
  （`num_buffers` 总在；只有 legacy 且未协商 `MRG_RXBUF` 才是 10）。起初按 10 拼包，设备**已发出**
  （`tx_used=1`）但 slirp 因解包错位丢弃、无应答；改成 12 后立刻收到应答。
- 驱动起来后即由 `init` 监督（域 16），与其它服务一致。

#### N3b — 最小 IPv4 栈（IPv4 / ICMP / UDP）✅ 已完成
- 目标：在**不新增服务 / 不新增 syscall / 不动公共文件**的前提下，给 `net_srv` 补上网卡之上的
  协议语义（实现全在 [`net_srv.rs`](../user/srv/src/net_srv.rs) 内）。
- 做法：
  - **IPv4**：20 字节定长头（`IHL=5`、DF 置位）构造 + 解析；Internet 校验和（RFC 1071 反码和）
    覆盖头部，解析时校验。解析拒非 IPv4 / 版本非 4 / IHL 或总长越界 / 校验和非法 /
    **任何分片**（不实现重组，`MF` 与 `frag_offset≠0` 一律拒）。UDP 校验和置 0（IPv4 允许）。
  - **ICMP echo**：向网关 `10.0.2.2` 发 echo request（id/seq + 负载），收 reply 校验
    type=0 / id / seq / 校验和 → **端到端**；另加「收发往 `OUR_IP` 的 echo request → 回 echo reply」
    （目的 MAC 取请求源 MAC）。
  - **UDP**：构造报头 + 发送；slirp 对未监听端口回 **ICMP 目的不可达（type 3 code 3）**，
    解析内嵌原始 IP/UDP 作副证据。
  - **缓冲区**：IP 帧走**独立的一页 TX**（`IP_TX_BUF_PAGE=7`，与 ARP 的页 6 错开），发送**串行复用**
    （`tx_send` 等 TX used 环前进再复用）；网关 MAC 从 ARP 应答的 `sha` 取，不写死。
- **无显示器自证**：「收 request 回 reply」路径用**合成 echo request**（栈上）喂给 responder，
  再解析生成的回包断言字段 + IP/ICMP 双校验和（确定性，不依赖对端）。最终打一行
  `NET2 ipv4/icmp OK, echo reply from 10.0.2.2, udp TX 10.0.2.2:9999 -> icmp unreachable, echo-reply path OK`；
  探针有界收手，超时字段标 `timeout`（绝不静默）。
- 判据：串口出现上述 `NET2` 行且四段全成立；`NET1 … ARP reply OK` 不变；全量回归不退化。
- 不做（明确排除）：IP 分片/重组、TCP、DHCP、多网卡、ARP 缓存老化。

### D2 — LibDevice 双形态 ✅ 首批已完成（驱动底座）
- 新 crate [`user/libdevice/`](../../user/libdevice)（`#![no_std]`，零依赖）：把**与"我在服务进程
  还是飞地应用"无关**的底座集中 —— `grant`（唯一来源的 `DeviceGrant` + 地址/magic + `load/is_valid/page`）、
  `mmio`（易失读写原语 + `fence`）、`msix`（表项写入）。
- **服务形态**：`block_srv`（NVMe）与 `net_srv`（virtio-net）都已链接它 —— 各自删掉重复的
  `DeviceGrant` 镜像 / MMIO 函数 / MSI-X 表项写入，成为纯消费者（服务协议仍在各自进程里）。
- **直通形态**：飞地应用链接同一个库，运行时直接 MMIO/DMA（E3）。
- **D2b（✅ 已完成）**：把设备**语义**里「所有 virtio 设备都一样」的那层抽成 [`libdevice::virtio`](../user/libdevice/src/virtio.rs)
  —— `discover_caps` / `Caps`（common cfg + `reset`/`negotiate`/`driver_ok`/`notify`/`read_isr`）/ `setup_queue` /
  `Vq`（`set_desc`/`avail_push`/`kick`/`used_idx`/`used_elem`）/ `zero_page` + 常量。`net_srv` 与 `virtio_blk_srv`
  各删约 150 行重复，只剩**设备语义**（网卡：包头 + ARP；块设备：三段式请求链）。`discover_caps` 把
  「读 PCI 配置空间」做成参数，`libdevice` 保持**零依赖**（飞地直通形态 E3 原样复用）。行为零变化（回归全绿）。
  NVMe 的队列/协议状态机**仍未抽**（等 E3 飞地这个第二消费者）。

### D3 — 第二个真实驱动：virtio-blk ✅ 已完成
- 目的：验证 **D1 通用路径**（内核里没有块设备专属逻辑，只多一个按类查找器 + 一行声明）+ 给回归加一条独立可自动化的块设备取证。
- QEMU 原生支持（`-device virtio-blk-pci`），走 **virtio-modern**（与 `net_srv` 同款 PCI transport）。
- 落地：
  - 内核：`pci::find_virtio_blk`（class `0x01` + vendor `1AF4` + device `1042`/`1001` → BAR4）、`BOOT_DOMAINS` 17 → **18**、`main.rs` 建域 17 + 一行 `device::grant`（`label: "vblk"`, `dma_pages: 8`, `msix_vectors: 2`）。**未加任何 virtio-blk 协议代码**。
  - 驱动 [`virtio_blk_srv`](../user/srv/src/virtio_blk_srv.rs)（域 17，**self-contained**）：`SYS_DEVICE_CONFIG_READ` 自解析能力 → 复位 → 协商（只接 `VIRTIO_F_VERSION_1`）→ 读容量 → 建**单个**请求队列 → MSI-X（表在 BAR1，表项 0 → 向量 `0x55`）→ `DRIVER_OK`。设备语义 = **三段式描述符链**（header 16B / data 512B / status 1B）。
  - 自测：读扇区 0 校验宿主预写签名 + 写扇区 1 读回校验 → `VBLK1 virtio-blk OK, cap=2048, sector0 sig=MORION-VBLK-TST!, sig=ok, rw=ok`。
  - QEMU（`make run-nvme` 与 `scripts/fs-regress.sh`）：加 `-device virtio-blk-pci,drive=vblk0`，测试盘 `build/vblk.img`（1 MiB）扇区 0 由宿主预写已知签名。
- **踩到的坑**：`dd` 写签名时漏 `conv=notrunc` 会把 1 MiB 镜像**截断成 512 字节**（QEMU 报 `cap=1` 扇区）→ 写扇区 1 越界，现象是 `sig=ok` 但 `rw=BAD`。
- **注**：设备语义（vring）在 `net_srv` 与 `virtio_blk_srv` 里**各有一份**，二者的公共面已可对照 —— **D2b** 即去重进 `libdevice::virtio`。

### D4 — 真机存储驱动：`ahci_srv`（SATA/AHCI）✅ 已完成（03b：读+写 + 接进卷层）
- 目的：让真机不再只有 NVMe —— 用同一套通用授权路径加 **AHCI/SATA** 驱动。**03b 起读+写**，并经 IPC 接进 `block_srv` 卷层（`backend=ahci`）；xHCI/USB 留后续（03c）。
- 落地：
  - 内核：`pci::find_ahci`（大容量存储类 `01:06:01` → **BAR5 = ABAR**）、`BOOT_DOMAINS` 18 → **19**、`main.rs` 建域 18 + 一行 `device::grant`（`label: "ahci"`, `bar_pages: 2`, `dma_pages: 6`, **`msix_vectors: 0`**）。
  - 驱动 [`ahci_srv`](../user/srv/src/ahci_srv.rs)（域 18，**self-contained**，仍走 `DeviceGrant::load()` 的 `SYS_DEVICE_GRANT` 运行期申请）：`GHC.AE` → 遍历 `PI` 选端口（`PxSSTS.DET==3` 且 `PxSIG==0101h`，跳过 ATAPI / 端口倍增器）→ 建命令列表(1 KiB) / Received FIS(256 B) / 命令表(1 KiB) / 数据页（**页对齐**天然满足对齐要求）→ 启动端口 → `IDENTIFY DEVICE(0xEC)` 判类型 / 取容量 → `READ DMA EXT(0x25, LBA48)` 读扇区 0。
  - **中断策略**：本仓库只有 MSI-X 一条中断通路（无 INTx、无 MSI 非 X），而 AHCI 常态用 INTx/MSI —— 故第一版**全轮询**（`PxCI` / `PxIS`），**不申请向量**（`msix_vectors = 0`），`irq_cmds == cmds` 判据因此保持不变。日后要走中断需先补 MSI/INTx 通路（要动内核中断层，不属本轮）。
  - 自测：读扇区 0 校验宿主预写签名 → `AHCI1 ahci OK, cap=…, sector0 sig=MORION-AHCI-TST!, sig=ok`。
- **03b 追加（接进块服务卷层 + 读+写）**：
  - 命令：自测后 `BLOCK_OP_ATTACH` **异步**通知 `block_srv`（用 `send` 不用 `call` —— `block_srv` 收后要**回调** ahci 校验，同步等回复会自锁）；`block_srv` 分配一页传输暂存页**同址共享**给 ahci，登记成 `backend=ahci` 的卷。
  - 读/写：`block_srv` 按 8 扇区（一页）切分，`sys_call` 转发；`ahci_srv` 用 `WRITE DMA EXT(0x35)` + `FLUSH CACHE EXT(0xEA)` 在自己的 DMA 通路上完成，数据经共享暂存页互拷。⚠️ **写方向必须先拷数据再发 IPC**（顺序反了 ahci 取到上一笔残留 —— 这正是本轮首次回归 `rw=bad(cmp)` 的根因）。
  - 取证：`block: ahci volume attached (vol=8, sectors=2048, sig=ok)` + `AHCI2 ahci volume rw OK, vol=8, lba=2047, rw=ok`；卷表末行 `backend=ahci`，且 `PART_RELOAD` 重建卷表后仍在（AHCI 卷另存一份、重扫后挂回表尾）。
- 授权（03b 新增，仍只是能力签发）：`block_srv → SendTo+MapInto → ahci_srv`、`ahci_srv → SendTo → block_srv`。
- QEMU：q35 自带 ICH9 AHCI（`8086:2922`，class `01:06:01`）。测试盘由 **Makefile 规则**
  `$(AHCI_IMG)` 生成（1 MiB，扇区 0 预写 `MORION-AHCI-TST!`，`dd … conv=notrunc,sync` ——
  注意 D3 记过的"漏 `conv=notrunc` 会截断"坑），**已接进 `make run-nvme` 与 `scripts/fs-regress.sh`**
  （挂 `-drive file=…ahci.img,if=none,id=ahci0,format=raw -device ide-hd,drive=ahci0`），
  回归判定新增 `AHCI1 … sig=ok`、`block: ahci volume attached … sig=ok`、`AHCI2 … rw=ok`。
- **不做（明确排除）**：热插拔、xHCI/USB（03c）、MSI/INTx 通路。

### E1 — IOMMU (Intel VT-d)
- 解析 ACPI **DMAR** 表 → 找到 DRHD（各 IOMMU 单元与管辖范围）→ 建**根表/上下表** → 为设备建 **DMA 重映射域**。
- 与 D1 结合：`SYS_DEVICE_GRANT` 在飞地场景下把设备的 DMA 权限绑到一个 **IOVA 窗口**（只映射飞地自己的缓冲），其余一律**拒绝**。
- 取证：让飞地故意对**未映射**地址发 DMA → 观察 IOMMU 报错（QEMU 需 `-device intel-iommu`），且系统**不受影响**。
- ⚠️ 与内核恒等映射的关系：内核自身 DMA（如 NVMe bring-up 早期）要保持可翻译，需明确哪些域**绕过**或**放行**。

分三步（每步独立可回归）：

#### E1a — ACPI DMAR 探测 ✅ 已完成
- **RSDP 来源**：引导器在 `exit_boot_services` **之前**从 UEFI 配置表取（优先 `ACPI2_GUID` → 带 XSDT 指针，退 `ACPI_GUID`），经 `BootInfo.rsdp_addr` 交给内核（布局 version 3 → 4，`BOOT_VERSION` 配套校验）。内核此前**完全不碰 ACPI**，这是第一条 ACPI 路径。
- **[`kernel/src/arch/acpi.rs`](../kernel/src/arch/acpi.rs)**：`probe_dmar() -> DmarSummary` —— RSDP 20 字节自校验和 → XSDT（每项 8B）/ RSDT（每项 4B）逐项找 `DMAR` → 解析重映射结构（DRHD 的 `flags`/`segment`/`reg_base` + 设备范围计数、RMRR 计数）。所有物理访问前过 `is_identity_mapped`；畸形表只降级、不 panic。
- **踩到的坑**：DMAR 表体在 ACPI 表头后还有 12 字节（`Host Address Width` + `Flags` + `Reserved`），重映射结构从偏移 **48** 起而非 36 —— 按 36 解析会把 `Host Address Width` 当成 `type`，现象是"表找到了但 `drhd=0 rmrr=0`"。
- **取证**：无 IOMMU → `[OK] no ACPI DMAR (no IOMMU), VT-d disabled`（优雅降级）；开了 → `[OK] ACPI DMAR found: len=128 aw=47 drhd=1 rmrr=0 checksum=ok` + `DRHD[0] base=0xFED90000 segment=0 include_pci_all=no scopes=8`。纯函数单测（合成表 + 畸形表）24 → 26。
- **注**：本机 QEMU 已 11.x，`-machine intel-iommu=on` 属性**已移除**，须 `-machine q35 -device intel-iommu`。

#### E1b — 重映射域 ✅ 已完成
- **[`kernel/src/arch/iommu.rs`](../kernel/src/arch/iommu.rs)**：按 `DRHD[0].reg_base`（QEMU `0xFED90000`，在恒等映射内 → 直接以物理地址当虚拟地址 volatile 访问，同 `apic.rs` 访问 LAPIC）读 `VER/CAP/ECAP` → 选地址宽度（优先 39 位）→ 建**根表**（每总线一项）+ **上下文表**（每总线一张）+ **恒等二级页表**（AW=39 时顶层即 1 GiB 大页级，1 张表覆盖前 4 GiB）→ **每个枚举到的 PCI 功能点**写一条 `translated + 恒等`上下文项 → `RTADDR` → `GCMD.SRTP` → `GCMD.TE`，回读 `GSTS.RTPS/TES` 确认。
- **口径**：`TE = 1` 后所有设备 DMA 都要查表；阶段一内核与既有驱动仍按物理地址 DMA → 用**恒等翻译**放行（行为零变化），且每个功能点在 IOMMU 里都有明确一项，不留"表里没有就放行"的隐式口子（E1c 换受限窗口的前提）。本步**不需要** pass-through。
- **踩到的坑（关键）**：上下文项 `TT`（bits 3:2）取值写反 —— 按 Linux/QEMU 口径是 **`0b00` = translated**、`0b01` = Device TLB、`0b10` = pass-through。写成 `0b01` 时 QEMU 直接报 `vtd_ce_type_check: DT specified but not supported`，并把该设备所有 DMA 判成故障（现象 = NVMe `Identify Controller FAILED`）。注意 **virtio 设备默认绕过 IOMMU** —— 所以 `VBLK1` 通过**不代表**翻译生效，只有 NVMe 真正走查表，是这一步的关键证据来源。
- **取证**（`make run-nvme IOMMU=1`，或 `IOMMU=1 OUT_DIR=build bash scripts/fs-regress.sh`）：`[OK] VT-d: IOMMU reg=0xFED90000 ver=1.0 sagaw=0x6 ecap=0xF02` + `[OK] VT-d: remap ON root=0x686000 ctx_buses=1 translated=8 (iova=identity 4GiB, aw=39) gsts=0xC0000000`（`TE=1` + `RTPS=1` 回读确认）；`-device intel-iommu` 下全量回归全绿（`SELFTEST DONE`×1、`FAILED`/`PANIC` 0、`irq_cmds == cmds == 28672` 且 `poll_cmds = 0`、`VBLK1 … sig=ok, rw=ok`、`NET1 … ARP reply OK`），**QEMU 侧零 VT-d 故障**；无 IOMMU 时 `init` 直接返回、行为零变化；内核单测 26 → 29。
- **注**：本机 QEMU 11.x 有已知缺陷把 `ECAP.PT` 错放进 `CAP`（实测 `ecap=0x0F02`，bit 6 = 0）—— 本步不依赖 pass-through 能力，故不受影响。

#### E1c — 受限 IOVA 窗口 + 越界 DMA 拒绝取证 ✅ 已完成
- **受限窗口**：[`kernel/src/arch/iommu.rs`](../kernel/src/arch/iommu.rs) 把允许的 IOVA 集合写成一条**显式窗口**：其余设备仍是 `[0, 4 GiB)` 恒等（行为零变化），**目标设备（NVMe）单列一张更小的窗口表 `[0, 3 GiB)`** —— 窗口外一律**不建叶项**，设备发起的越界 DMA 一定查表失败。那张表是 E2/E3 把飞地那台设备窗口继续收小的**唯一落点**，不会波及其它设备。
- **目标为什么只能是 NVMe**：virtio 默认绕过 IOMMU（见 E1b 的坑），只有 NVMe 真走查表。
- **触发方式（设备发起，不是 CPU）**：`block_srv` 在**卷扫描之后**提交一条 **1 扇区 NVMe 读，PRP1 指到窗口外第一个地址**（3 GiB 整）。选 NVM 读而不是 Admin Identify —— 卷扫描一路都在走同一条读路径，它确定会让设备去取 PRP。探针是**有界轮询**、不碰中断模式、不计入 `nvme: stats`，所以既不会把验收口径 `poll_cmds = 0` 打掉，也不改变命令计数。
- **取证（内核侧）**：越界 DMA 由用户态驱动在启动阶段发起，而内核没有任何周期性钩子 → 由**空闲任务**（`task_idle` 的 `hlt` 循环，`main.rs` 加一行）调用 `poll_faults()` 读故障寄存器并打印。
  - **寄存器口径（易错，两处实测教训）**：① `FEDATA`/`FEADDR`/`FEUADDR`（0x3C/0x40/0x44）是**故障事件（MSI）的配置**、**不含**故障内容，SID/原因/地址在 **FRCD**（`0xB0 + 16*i`）；② 本机 QEMU 只置 `FSTS.PPF`（bit 1）而**不置** `FRI`（bit 0），所以"有没有故障"必须按 `FSTS != 0` 判；③ 该 QEMU 只在故障中断**可投递**时才写 FRCD，我们没给 IOMMU 设备开 MSI，故内核日志里 FRCD 为 0 —— 那一侧证据取 QEMU 自己的日志行。
  - **窗口边界必须 < 4 GiB**：该 QEMU 的 `intel-iommu` 只翻译 < 4 GiB 的 IOVA，**恰好 4 GiB 的 PRP 会直接落到系统地址空间**（实测：既无 `vtd_iommu_translate` 日志、也不产生 FSTS 故障记录）。所以窗口取 3 GiB 而不是 4 GiB。
- **取证（`make run-nvme IOMMU=1`，或 `IOMMU=1 OUT_DIR=build bash scripts/fs-regress.sh`）**：
  - 内核日志：`[OK] VT-d: remap ON … target=0x10/win=0xc0000000 …` + `IOMMU1 out-of-window DMA probe: prp=0xc0000000 window=[0,0xc0000000) probe=ok` + `[OK] VT-d: DMA refused target-sid=0x10 fsts=0x2 fectl=0x0 frcd[0]=0x0 (FRCD 未写: 故障中断未投递)`。
  - QEMU stderr（脚本把 stderr 丢掉，手测时 `2>file` 才看得到）：`vtd_iova_to_sspte: detected sspte permission error (iova=0xc0000000, level=0x3, sspte=0x0, write=1, …)` + `vtd_iommu_translate: detected translation failure (dev=00:02:00, iova=0xc0000000)`。
  - **系统不受影响**：同一次运行里卷表照常、FS 自测与全量回归全绿（`SELFTEST DONE`×1、`FAILED`/`PANIC` 0、`VBLK1 … sig=ok, rw=ok`、`NET1 … ARP reply OK`）；无 IOMMU 时窗口代码整段不执行、行为零变化（探针照跑，落到 q35 上不属于任何内存区的地址，写入被丢弃）。
  - 内核单测 29 → 31（窗口边界/页数、故障记录解码）。

### E2 — 飞地管理器 `enclave-mgr`
- 唯一持 `SYS_ENCLAVE_*` 的特权服务（D6）。
- `create_enclave(mem_size, devices[], caps[])`：校验策略 → 内核建保护域 → 映射设备 MMIO → 建 IOMMU 域 → 注册 IRQ → 装 LibDevice → 返回飞地入口能力。
- 生命周期：暂停 / 恢复 / 销毁 / 资源回收；**每飞地一条日志流**（构造/退出/异常）推到审计侧，飞地自身不可篡改。

### E3 — 示例飞地应用
- 直通接管一个设备（先用块设备 / virtio-blk），证明：
  1. **零内核陷落**：命令提交全在用户态 MMIO 写；
  2. **IOMMU 隔离**：错误 DMA 被拒且不伤系统；
  3. 与普通路径**行为一致**（同一 LibDevice，两种形态）。

### V1 — 版本串 ✅ 已完成
- `SYS_UNAME(51)`（号与转发臂见 3.0 接线层）返回系统名 / release / 构建号：写回调用方缓冲并返回长度，
  内核侧实现在 [`kernel/src/version.rs`](../kernel/src/version.rs)，用户态封装 `morion::syscall::sys_uname`，
  shell 加 `uname` / `version`。
- 版本常量**单一来源** = `version.rs`（`SYSTEM_NAME` / `VERSION` / `MACHINE` / `VARIANT` / `BUILD`），
  README 的「版本」段与 CHANGELOG 与之保持一致。
- 取证：shell `uname` → `MorionOS 0.4.0 x86_64`、`version` → `MorionOS v0.4.0 (build <git短哈希>)`。

### V2 — 无图形界面版本收口 ✅ 已完成
- 构建开关：`make NOGUI=1 ...` 注入编译期环境变量 `MORION_NOGUI`（内核 `version.rs` 与用户态
  `morion::syscall::NOGUI` 同一约定）—— release 串带 `-nogui`，**shell 不开屏幕镜像**、输入/回显退回串口。
- ⚠️ 与 D8 原文的差异（有意）：`gfx_srv` **仍照常加载**（boot 的服务表写死 18 项、init 又监督域 15；
  真正"不启"要动 boot/kernel/init 的公共文件，且会让「18 个服务 ELF」判据变 17）。故本步只关
  **屏幕镜像**这条用户可见的图形路径 —— 其余服务与**全量回归口径不变**。
- CHANGELOG（新建）+ README「版本」段 + **打 tag `v0.4.0-nogui`**。
- 验收即"全量 FS 回归 + 驱动自测"全绿（含 `NOGUI=1` 变体回归）。

### V3 — 安装盘变体（装机用）✅ 已完成
- 动机：发行版装机的标准流程是**先用 U 盘启动、再把系统装到本机盘上** —— 那时盘上原有的
  文件系统（Windows / exFAT / ext2…）正是**要被覆盖**的东西，而 `mkfs.mfs` 的护栏默认拒绝
  非空白卷。逐条写 `--force` 会把安装脚本写得很脆，且「当前是安装环境」这个事实本该由
  **镜像变体**表达，而不是由每条命令的开关表达。
- 构建开关：`make INSTALL=1 iso` 注入编译期环境变量 `MORION_INSTALL`（内核 `version.rs` 的
  `IS_INSTALL` 与用户态 `morion::syscall::INSTALL_MODE` 同一约定）—— release 串带 `-install`
  （与 `-nogui` 可组合成 `-nogui-install`），产物落 `build/install/`。
- 行为差异（**全镜像只有这一处**）：`mfs_srv` 的 `mkfs` 护栏在该变体里对**非空白卷**默认放开，
  放行时打印 `mfs: mkfs: overwriting an existing filesystem on volume <n> (install image; …)`。
  日常镜像 `INSTALL_MODE == false`，护栏一字不放宽（`--force` 仍是日常镜像的唯一出口）。
- 自测口径：FS-22 的「别人的分区必须被拒」两条断言在安装盘里**跳过**并打印 SKIPPED —— 那种
  镜像下护栏已放开，照旧去调就等于**当场把正在跑的 FAT 根卷格掉**；护栏本身由日常镜像的
  全量回归守着。「不存在的卷号一律被拒」在任何变体下都保留。
- 取证：安装盘 `uname` → `MorionOS 0.4.0-install x86_64`；对 exFAT 卷 `mkfs.mfs 5`（**不带**
  `--force`）成功并在 `/usb5` 挂上新卷；日常镜像同一命令仍被拒。
- 回归：安装盘 `REGRESS_ISO=build/install/morion-os.iso bash scripts/fs-regress.sh` →
  `SELFTEST DONE` 1 次、`FAILED/PANIC` 0 次（耗时 324 s，日志里有 `FS22 guard checks SKIPPED`）；
  日常镜像同口径全绿，且**没有**那行 SKIPPED（护栏断言照旧执行）。
- 顺带修了回归脚本的一个误判：`shell: screen console mirror FAILED (gfx_srv cursor not advanced)`
  是启动竞态的无害提示，脚本原先把它当"失败"提前收工（实测 13 秒即退、`SELFTEST DONE` 0），
  现已在收工判据与失败计数里统一滤掉。

### 远期（跨模块，尚未排期）
- **引导安全链密码库（GmSSL）**：`boot/src/security/` 的国密实现目前部分为桩 —— SM3 映像哈希与 SM2 验签已用 RustCrypto `no_std` 纯 Rust，**TPM 2.0 PCR 测量仍为桩**（待接 `EFI_TCG2_PROTOCOL`）。计划把桩替换为 **GmSSL (C)** 实现，并打通自加密镜像解封与飞地预认证。
- **独立高精度定时器（hrtimer / TSC，1 ms 精度）**：模仿 Linux `hrtimer` 思路，**不改动全局 100 Hz 调度 tick**，另实现一套基于 APIC/TSC 的独立高精度定时器；普通任务 `sleep` 走普通 tick，游戏 / 多媒体经**新 syscall** 走 hrtimer 做 1 ms 精度等待。取舍：只有需要高精度的任务受影响，其余系统部分不受拖累、功耗可控；调度抢占仍 10 ms 一次，但程序休眠唤醒可做到 1 ms。**注（02b-2 续 → 1000 Hz）**：全局调度 tick 已由 100 Hz 提到 **500 Hz**、再提到 **1000 Hz**（tick 1 ms）—— 实测「阻塞→唤醒」被 tick 量化正是文件系统 I/O 墙钟的主要来源，全量回归 118 s → 33 s → **23 s**；提 1000 Hz 前先修掉一条**启动期测试时序竞态**（`sender` 委派 `SendTo(3)` 与 `receiver` 零能力负例赛跑，见 [dev-reference.md](dev-reference.md) §9 第 85 行）。hrtimer 的动机与「只让高精度任务受影响、功耗可控」的取舍不变，只是普通 tick 粒度已从 10 ms 收到 1 ms。

---

## 5. 执行顺序（每步独立可回归）

1. **D1 通用设备授权**（✅ 已完成，boot 路径）：抽 `device.rs` 通用原语 + 描述结构，把 `nvme` 迁过去；**功能零变化**，回归口径不变。运行期 syscall 路径（D1b）随网络/后续驱动一起做。
2. **N0 域表扩容**（✅ 已完成）：`BOOT_DOMAINS` 16 → 17 + boot 侧服务表同步 + `net_srv` 骨架。
3. **N1 PCI 通用查找 + 设备声明**（✅ 已完成）：按类找 virtio-net（BAR4）+ `device::grant` 声明 + QEMU 加网卡；**MSI-X 表在 BAR1** 的缺口留给 N2。
4. **N2 `net_srv`**：virtio-net 初始化（✅ N2a：PCI 能力 / MAC / virtqueue / `DRIVER_OK` / 轮询取帧；✅ N2b：MSI-X 中断化，表在 BAR1 由内核另映射）。
5. **N3 网络自测**（✅ 已完成）：ARP 请求 → 应答取证（`NET1`）；✅ **N3b** 最小 IPv4 栈（IPv4 头 + ICMP echo 收发 + UDP，端到端 `NET2`）。
6. **D2 LibDevice**（✅ 首批完成，驱动底座）：抽 `libdevice`（`grant`/`mmio`/`msix`），`block_srv` 与 `net_srv` 改为消费者；✅ **D2b** 已完成：`virtio` 传输层 + vring 去重进 `libdevice::virtio`（`net_srv` 与 `virtio_blk_srv` 共用）；NVMe 队列语义仍留待 E3。
7. **D3 virtio-blk**（✅ 已完成）：用通用路径加第二个真实驱动 `virtio_blk_srv`（域 17）+ 读签名/写读回自测（`VBLK1`）。**注**：boot 期**声明式**授权（内核只多一个按类查找器 + 一行声明），运行期 `SYS_DEVICE_*`（D1b）✅ 已完成（见第 4 节 D1b）。**D4** 真机存储驱动 `ahci_srv`（✅ 已完成，域 18，SATA/AHCI，全轮询、不申请中断；03b 起读+写并经 IPC 接进 `block_srv` 卷层）。
8. **D0 I/O 端口能力**（✅ 已完成）：`Capability::IoPort(base, len)` + 给既有的 `SYS_PORT_*`（22–25）加门禁（此前无门禁）；按半开区间授权，只给 `block_srv`（IDE）与 `mfs_srv`/`exfat_srv`（CMOS）。
9. **E1 IOMMU (VT-d)**：DMAR 探测（✅ **E1a**：RSDP → XSDT/RSDT → DRHD）+ 重映射域（✅ **E1b**：全设备 `translated + 恒等`，打开 `GCMD.TE`）+ 受限 IOVA 窗口与越界 DMA 拒绝取证（✅ **E1c**：目标设备窗口收到 3 GiB，设备发起的窗口外 DMA 被拒并留证）。
10. **E2 enclave-mgr**：飞地生命周期 + 日志流 + 审计。
11. **E3 示例飞地**：直通接管设备，零陷落 + 隔离取证。
12. **V1 版本串**（✅ 已完成：`SYS_UNAME(51)` + shell `uname`/`version`）+ **V2 无图形版本收口**（✅ 已完成：`NOGUI=1` 变体 + CHANGELOG；tag `v0.4.0-nogui` 待打）。

---

## 6. 验收（沿用现有口径 + 新增）

| 项 | 判据 |
|---|---|
| 不退化 | `make fmt` / `check` / `clippy` 全 0；内核单测全过；**全量 NVMe FS 回归**仍 `SELFTEST DONE`×1、`FAILED`/`PANIC` 0、`irq_cmds == cmds` 且 `poll_cmds = 0`、宿主 `sgdisk -v` "No problems found" |
| 串口 sink | 任何迁移都不得让关键行从 `-serial file:` 消失（沿用图形的 D6 硬约束） |
| D1 | `nvme` 驱动走 `SYS_DEVICE_*`，内核里不再有 NVMe 专属结构；块设备自测与全量回归不变 |
| D1b | 三个驱动（NVMe / net / vblk）经运行期 `SYS_DEVICE_GRANT` 取授权，带 `Mmio` 能力门禁；行为与改造前逐字一致，内核单测覆盖门禁与编码 —— ✅ |
| N0 | 扩容后全量回归仍全绿（存活域数基线、`SELFTEST DONE`×1、`FAILED`/`PANIC` 0）—— ✅ `[OK] 17 service ELFs loaded` + `[up] net_srv (domain 16)` |
| N1 | 内核找到 virtio-net 并把 `DeviceGrant` 交给域 16：日志有 `[OK] virtio-net modern BAR4=…` 与 `net: device grant …`；全量回归不退化 —— ✅ |
| N2a | net_srv 读到 MAC、RX/TX virtqueue 建好、`DRIVER_OK`，并能取到帧 —— ✅ `net: virtio-net up MAC=… rx=8 tx=8`、`net: rx frames=1` |
| N2b | MSI-X 中断化（表在 BAR1，D1 支持另映射 MSI-X 表 BAR）：`net: MSI-X prepared … table_bar=1`、`net: MSI-X enabled vectors=0x53..0x54` —— ✅ |
| N3 | 收到 ARP 应答（`NET1 virtio-net up, MAC=52:54:00:12:34:56, ARP reply OK`）—— ✅ |
| N3b | 最小 IPv4 栈（IPv4 头构造/解析 + 校验和 + 拒分片；ICMP echo 发 request 收 reply + 收 request 回 reply；UDP 构造/发送）—— ✅ `NET2 ipv4/icmp OK, echo reply from 10.0.2.2, udp TX 10.0.2.2:9999 -> icmp unreachable, echo-reply path OK`；`NET1 … ARP reply OK` 不变；全量回归 `SELFTEST DONE`×1、`FAILED`/`PANIC` 0 |
| D3 | virtio-blk 读写自测通过（`app:` marker），且未改内核设备代码 —— ✅ `VBLK1 virtio-blk OK, cap=2048, sector0 sig=MORION-VBLK-TST!, sig=ok, rw=ok`；内核侧只多 `pci::find_virtio_blk` + 一行 `device::grant`（无 virtio-blk 协议代码） |
| D4 | AHCI/SATA **只读**驱动落地（域 18），仍走通用授权、内核无设备专属逻辑；全轮询、不申请中断 —— ✅ `[OK] 19 service ELFs loaded` + `AHCI1 ahci OK, cap=2048, sector0 sig=MORION-AHCI-TST!, sig=ok`；`irq_cmds == cmds == 28672` 且 `poll_cmds = 0`；宿主 `sha256sum build/ahci.img` 运行前后一致（只读；`dd … conv=notrunc` 预写签名）；全量回归 `SELFTEST DONE`×1、`FAILED`/`PANIC` 0（隔离树验证：HEAD + 仅 03 补丁）；**收口后已把测试盘接进 Makefile 与标准回归**（`AHCI_IMG` 规则 + `scripts/fs-regress.sh` 判定 `AHCI1 … sig=ok`），不再需要手工 `QEMU_EXTRA` | ✅ |
| D2b | virtio 传输层 + vring 去重进 `libdevice::virtio`（两个驱动共用，`libdevice` 保持零依赖），行为零变化 —— ✅ `NET1 … ARP reply OK` + `VBLK1 … sig=ok, rw=ok` 不变，全量回归全绿 |
| D0 | I/O 端口 syscall 加 `IoPort` 门禁：未授权域读写端口被拒，`block_srv` / `mfs_srv` / `exfat_srv` 照常 —— ✅ `app: D0 port capability gate OK (ungranted I/O port denied)`；FS-13/FS-16 仍写得出时间戳（两处授权放行）；内核单测 24/24 |
| E1 | 飞地越界 DMA 被 IOMMU 拒绝；系统与其他域不受影响 |
| E1a | 内核能找到 DMAR 并报出 DRHD/设备范围；固件无 IOMMU 时优雅降级 —— ✅ `[OK] ACPI DMAR found: len=128 aw=47 drhd=1 checksum=ok` + `DRHD[0] base=0xFED90000 scopes=8`（`-device intel-iommu`）；无 IOMMU 时 `[OK] no ACPI DMAR (no IOMMU), VT-d disabled`，全量回归零变化；内核单测 26/26 |
| E1b | 打开 VT-d DMA 重映射后全量回归不退化 —— ✅ `[OK] VT-d: remap ON root=0x686000 ctx_buses=1 translated=8 (iova=identity 4GiB, aw=39) gsts=0xC0000000`（`TE=1`+`RTPS=1`）；`-device intel-iommu` 下 `SELFTEST DONE`×1、`FAILED`/`PANIC` 0、`irq_cmds == cmds == 28672` 且 `poll_cmds = 0`、`VBLK1 … sig=ok, rw=ok`、`NET1 … ARP reply OK`，QEMU 侧零 VT-d 故障；无 IOMMU 时行为零变化；内核单测 29/29 |
| E1c | 设备发起的**窗口外 DMA 被 IOMMU 拒绝**并留证，系统与其他域不受影响 —— ✅ 目标设备（NVMe，SID 0x10）窗口 = `[0, 3 GiB)`；`block_srv` 探针把 NVM 读的 PRP1 指到 3 GiB → 内核日志 `IOMMU1 out-of-window DMA probe: prp=0xc0000000 window=[0,0xc0000000) probe=ok` + `[OK] VT-d: DMA refused target-sid=0x10 fsts=0x2 …`，QEMU stderr `vtd_iommu_translate: detected translation failure (dev=00:02:00, iova=0xc0000000)`；同一次运行全量回归全绿（`SELFTEST DONE`×1、`FAILED`/`PANIC` 0、`irq_cmds == cmds == 28672` 且 `poll_cmds = 0`、`VBLK1 … sig=ok, rw=ok`、`NET1 … ARP reply OK`）；无 IOMMU 时零变化；内核单测 31/31 |
| E3 | 飞地直通命令成功 + 隔离取证同时成立 |
| V2 | 无图形构建下全量回归通过；`SYS_UNAME` 报告 `v0.4.0-nogui` —— ✅ `MorionOS v0.4.0-nogui (build …)` + `shell: no-gui build (screen mirror off; serial console only)`；`NOGUI=1` 变体全量回归 `SELFTEST DONE`×1、`FAILED`／`PANIC` 0、`irq_cmds == cmds == 28672` 且 `poll_cmds = 0`、`VBLK1 … sig=ok, rw=ok`（产物落 `build/nogui/`） |

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
