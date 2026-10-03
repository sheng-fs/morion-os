# 变更记录 · Changelog

本文件记录 MorionOS 面向用户的版本变更。版本串由内核
[`kernel/src/version.rs`](./kernel/src/version.rs) **单一维护**，shell 的 `uname` / `version`
命令经 `SYS_UNAME(51)` 读出。

格式参考 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)。

## [Unreleased] — 2026-10-02

**文件系统四条并行流 + 驱动 D4 / 安装盘变体 V3 的收口**（承接 `0.4.0-nogui`；版本串仍是 `0.4.0`）。
记账口径：未发版的改动先记在本 `[Unreleased]` 段，发版时再归档为版本段。

### 新增

- **真机存储驱动（D4）**：新增 `ahci_srv`（域 18，SATA/AHCI **只读**）—— 用通用设备授权驱动一台新类型设备，
  内核只加 `pci::find_ahci`（class `01:06:01` → BAR5 = ABAR）与一行 `device::grant`；第一版**全轮询**、
  不申请中断（本仓库只有 MSI-X 一条中断通路，AHCI 常态用 INTx/MSI），`IDENTIFY DEVICE` + LBA48
  `READ DMA EXT` 读扇区 0 校验签名（`AHCI1 ahci OK, … sig=ok`，只读由宿主 `sha256sum` 取证）。
- **安装盘变体（V3）**：`make INSTALL=1 iso` —— release 串带 `-install`（可与 `-nogui` 组合），产物落
  `build/install/`；该变体下 `mkfs.mfs` 对**非空白卷**的护栏默认放开（装机天生要覆盖旧文件系统），
  日常镜像一字不放宽（`--force` 仍是唯一出口）。
- **MFS 健壮性收口（文件系统流 01）**：magic 判定改**三态** —— 空白卷才自动格式化；**MFS 系但修订 ≠ 本构建
  MFS8 → 拒绝挂载 + 打印盘上/期望 magic 与处置建议，且一个字节都不写盘**（宿主 `sha256` 起机前后一致）；
  新增 `mfs.fsck [--repair]`（可达性只从当前目录树走、默认只报不修）与 `mfs.sync`（幂等落盘 + 回复 gen）；
  自测 FS-30/31/32，另加 `scripts/fsck-leak.sh` 的 `--repair` 泄漏回收用例。
- **块层只读缓存 + 顺序预读（文件系统流 02）**：`block_srv` 内 128 行 × 4 KiB 缓存（键 `(卷号, lba)`），
  写穿透 + 整卷失效，四个文件服务**零改动**（纯透明加速）；命中率 ≈80%、NVMe 命令 28672 → 16384、
  全量回归 **324 s → 258 s**。
- **权限与多用户设计稿（文件系统流 04）**：新增 [docs/design-permissions.md](docs/design-permissions.md)
  （uid/gid 放 MFS 元数据 `+32` 保留区、不升 magic、`MFS_E*` 错误码、能力在前权限在后）。
- **权限与多用户实现（04b）**：按设计稿在 `mfs_srv` 内落地 uid/gid + rwx 强制 —— `Cred{uid,gid}` 由
  发起域静态映射（引导期服务域 = `0:0`，运行期新建域 = `1000:1000`）、元数据 `+32 uid / +34 gid` 不改布局、
  `mfs_check_access` + 全部检查点（含新增 `CHOWN`）、`open` 权限快照进 fd；`MFS_E*` 错误码在客户端归一化回
  `u64::MAX`（原始码经 `mfs_last_errno()`）；新增 shell `chown <uid>:<gid>` 与 `ls -l`/`stat` 的 uid/gid；
  `exec::spawn_elf` 给运行期程序授最小文件系统能力面（`SendTo` mount/MFS + `MapInto` MFS）；FS-34..37
  用运行期新建域（uid 1000）端到端验证低权拒绝路径。
- **MFS 目录项索引缓存（02b）**：`mfs_srv` 内按 `(卷号, 目录 ino)` 把目录条目解析进内存表
  （`name -> (块, 块内偏移)`），首次查找一次性遍历、之后纯内存比对 —— 免去每次 `mfs_dir_lookup`
  的多跳 `block_srv` IPC；8 目录槽 × 8 KiB arena、满则 clock 淘汰、装不下回退线性扫描（绝不误判
  "不存在"）；失效钩子覆盖 `mfs_itab_set` / `mfs_itab_reload`（挂载与快照回滚）/ `mfs_gc` / `mfs_format`。
  回归 before 274 s → after 259 s（单次，≈5.5%），`mfs-didx` 命中率 ≈82%。
- **AHCI 盘接进块服务卷层（03b）**：`ahci_srv` 自测后 `BLOCK_OP_ATTACH` 通知 `block_srv`，把 SATA 盘登记成
  `backend=ahci` 的卷；`block_srv` 分配一页传输暂存页同址共享给 ahci，读/写按 8 扇区切分经 IPC 转发
  （`WRITE DMA EXT` + `FLUSH CACHE EXT`），上层文件系统对 AHCI/NVMe 无感；挂载时读扇区 0 校验签名
  （`block: ahci volume attached … sig=ok`）+ 写回读自测（`AHCI2 … rw=ok`）。内核只加 `block ↔ ahci` 三条
  能力授权。分区表重扫后 AHCI 卷仍保留。
- **USB 存储驱动（03c）**：新增 `xhci_srv`（域 19，xHCI/USB 存储**读写**）—— 用同一套通用设备授权驱动**又一台**
  类型迥异的控制器（xHCI），内核只加 `pci::find_xhci`（class `0C:03:30` → BAR0）与一行 `device::grant`，
  另补 `block ↔ xhci` 三条能力授权；第一版**全轮询、不申请中断**（本仓库只有 MSI-X，xHCI 常态用 INTx/MSI）。
  全链路：控制器复位 → 端口复位 → `Enable Slot` → `Address Device` → 取设备/配置描述符 →
  `SET_CONFIGURATION` → `Configure Endpoint`（两条 Bulk）→ **BOT（Bulk-Only Transport）+ SCSI 透明命令集**
  （`INQUIRY` / `READ CAPACITY(10)` / `READ(10)` / `WRITE(10)` / `REQUEST SENSE`，含 UNIT ATTENTION 重试）。
  自测后 `BLOCK_OP_ATTACH` 挂进 `block_srv` 卷层（`backend=usb`，区别于 `backend=ahci` 的 USB 暂存页），
  读/写按 8 扇区经 IPC 转发；端到端 `USB1 … sig=ok` + `block: usb volume attached … sig=ok` +
  `USB2 … rw=ok`，分区表重扫后 USB 卷仍保留，`IOMMU=1` 复跑亦通过。首版排掉三坑：输入上下文位偏移
  （Slot 端口号在 DWORD1 bits 23:16、EP 字段全在 DWORD1）、interrupter 的 `ERSTBA=0x10`/`ERDP=0x18`、
  以及把物理地址 `data_pa` 当虚拟地址用。
- **块层批量提交 / 等完成（02b-2）**：量化发现耗时主因是**每条 NVMe 命令的完成等待**（16384 条命令
  摊到 258 s ≈ 15.7 ms/条，不是每请求一跳 IPC），故改为「一次下发 K 条命令再统一等完成」。三层：
  ① `block_srv` 的 `nvme_batch_rw` —— 一条 I/O 队列排 k 条 SQE、只敲一次门铃、统一等完成（中断优先、
  回退轮询）；② `common.rs` 新增 `BLOCK_OP_BATCH_READ/WRITE` + `BatchEnt` + `block_batch()`（每子请求
  ≤ 8 扇区 = 一页，非 NVMe 后端自动退回逐块）；③ `mfs_srv` 写回攒批 —— COW 内容块（`mfs_commit`）与
  位图/头块/超级块（`mfs_write_blk`）共用 16 页**写暂存窗**，满窗或 `mfs_bmp_flush` 末尾才一次批写，
  `mfs_read_blk` 命中暂存块先落盘（写后读一致），GC/fsck/换卷/裸读超级块前强制落盘。全量回归
  **258 s → 133 s（-48%）**，NVMe 命令数不变、中断等待 16385 → 6159（`mfs-wb` 平均 9 条一批）。
  另加通用可选写背缓存并把 `fat32_srv`/`exfat_srv` 接上（实测这两服务写的块极少，收益仅数秒）。
- **块层完成路径改轮询 + 时钟 tick 100→500 Hz（02b-2 续）**：继续量化 —— ① 只读缓存 128 → 240 行
  （淘汰 2036 → 1522、未命中 2875 → 2632）但耗时**不变**（118 s），可见**读缓存不是瓶颈**；② NVMe
  完成**默认改轮询**（`NVME_POLL_FIRST`，MSI-X 仍照常写表 + 逐向量注册，中断路径代码与 `irq_cmds`/`irqs`
  证据保留），A/B 同代码：100 Hz 下 118 s vs 中断 133 s；③ `arch/pit::TARGET_FREQ` **100 → 500 Hz** ——
  根因确认是**调度 / IPC 唤醒被时钟 tick 量化**（100 Hz 时一次「阻塞→唤醒」最坏等 10 ms，那轮 QEMU 进程
  CPU 仅 ~9%、91% 在等 tick），同一份代码全量回归 **118 s → 33 s（3.6×）**、逐条命令墙钟 7.2 → 1.4 ms
  （`cmds` 不变 16384）。1000 Hz 更快（→ 23 s）但 ~18% 概率触发内核潜藏竞态（能力负例测试偶发「域 1
  零能力却调通 echo」→ 门禁失败），故停在 500 Hz（**连续 16 次全绿**）；修好该竞态后可再上 1000 Hz。
- **内核调度 1000 Hz + 启动握手修复**：把 `arch::pit::TARGET_FREQ` 由 500 提到 **1000 Hz**（tick 1 ms）。
  1000 Hz 下会以 ~18% 概率让能力负例「意外通过」，本轮定位到根因是**测试时序**（`sender` 委派
  `SendTo(3)` 与 `receiver` 的「启动时零能力」负例赛跑）而**不是内核 bug** —— 修法是 sender/receiver
  **显式握手**（`RECV_HANDSHAKE_TAG/ACK`，顺序由同步而非时间片决定；回 ACK 时 receiver 仍零能力，
  `reply` 走内核记录的回复目标、不需 `SendTo`，故「零能力启动」前提不变）。全量回归 **33 s → 23 s**
  （较 02b-2 前 258 s 约 11×），**连续 5 次全绿**。
- **`net_srv` 最小 IPv4 栈（N3b）**：纯 `net_srv` 内、不加服务 / 不改公共文件 —— IPv4 头构造·解析
  （RFC 1071 校验和 + 拒分片）、ICMP echo（发 request 收 reply + 收 request 回 reply）、UDP 构造·发送
  （slirp 对未监听端口回 ICMP 端口不可达作副证据）；IP 帧走独立 TX 页串行复用，网关 MAC 从 ARP 应答取。
  端到端 `NET2 ipv4/icmp OK, …`；TCP / DHCP / 多网卡 / 分片重组未做。
- **`net_srv` DHCP 客户端（N3c）**：纯 `net_srv` 内（不加服务 / 不改公共文件）—— 以太广播 + IPv4
  (`0.0.0.0`→`255.255.255.255`) + UDP `68→67` 上跑 `DHCPDISCOVER` → `DHCPOFFER` → `DHCPREQUEST`
  （option 50 请求 IP + 54 服务端标识）→ `DHCPACK`，解析 `yiaddr` 与掩码 / 路由器 / DNS；报文补齐到
  BOOTP 最小 300 字节、`flags` 置广播位；DHCP 与随后的 ARP 串行复用同一 TX 页。取到租约后把写死的
  `10.0.2.15`/`10.0.2.2` 换成运行期值（失败回落）。端到端
  `NET3 dhcp OK, ip=10.0.2.15 mask=255.255.255.0 gw=10.0.2.2 dns=10.0.2.3`；`NET1`/`NET2` 不变。
  不做：租约续约 / 过期、静态地址、多网卡、DHCPv6。
- **`net_srv` 最小 TCP + ARP 缓存老化（N4）**：仍只在 `net_srv.rs` 内（不加服务 / 不改公共文件）——
  TCP 头构造/解析（校验和含 12 字节伪首部）+ 三次握手（SYN/SYN-ACK/ACK）+ 单段 PSH/ACK；自证用
  **确定性**路线（合成 SYN-ACK → 解析断言字段/序号/校验和 → 握手 ACK → 数据段 → 篡改载荷确认校验和
  拦截），并另发真实 SYN 到 `10.0.2.2:12345` 取链路副证据（slirp 回 RST）。ARP 缓存老化：网关 MAC
  进 TTL 30s 缓存、过期重发广播 ARP。端到端
  `NET4 tcp OK, handshake+data selftest OK, peer refused(RST)` + `net: arp cache aging OK, hits=3 ttl_ms=30000`；
  `NET1`/`NET2`/`NET3` 不变。不做：重传 / 拥塞控制 / 窗口管理 / 连接状态机 / TCP 选项。
- **ext2 只读 → 有限写**：`ext2_srv`（域 12）新增 `CREAT` / `WRITE`（直接块 + 一级间接）/ `UNLINK` ——
  块与 inode 位图按组分配/释放、目录项插入（复用空槽 / 切分尾项 / 目录块满则追加）、同步超级块 `s_free_*`
  与块组描述符 `bg_free_*`；`app.rs` 把 `WRITE_BUF` 也共享给 ext2 域。实测 `build/ext2.img` 不含
  metadata_csum（`mke2fs -t ext2`）故不做校验和；只维护**主**超级块/主 GDT，不建 htree、不支持二级/三级间接写入。
  FS7 由「创建必须被拒」改为**写往返自证**（creat → write → read 断言内容 → unlink → 再 open 必失败；自清理使
  free counts 复原、可反复跑）：`app: FS7 ext2 write round-trip OK (creat/write/read/unlink)`。
- **图形 G5：surface 合成 / 多窗口**：`gfx_srv` 内新增 `Compositor`（窗口表 `MAX_WINDOWS=4`，**槽 0 = 文本控制台窗口**），
  窗口 = 几何 + z 序 + 一块**同址共享**表面；合成 = 桌面底色 + 按 z 序 blit（与目标矩形取交 → 屏幕边界 + 窗口
  边界双向裁剪），文本终端渲染进控制台窗口的**后备表面**后由合成器上屏 —— 控制台重绘不再擦掉客户端窗口，
  `GFX_OP_TEXT/CLEAR/MOVE/QUERY` 对外语义不变（shell 镜像自证照旧）。客户端新增 `morion::gfx::Window`
  （建窗/移动/置顶/销毁）与多表面槽（`SURFACE_SLOTS=3`）；协议新增 `GFX_OP_WIN_CREATE/MOVE/RAISE/DESTROY/FLUSH`、
  `GFX_OP_PIXEL`、`GFX_OP_COMPOSE`、`GFX_OP_INFO`。自测 GS-3（两个重叠窗口：z 序 / 裁剪 / `raise` 翻转 /
  窗口外=桌面底色，全部靠**服务端回读帧缓冲**断言）：`app: GS3 window compositor OK (z-order + clipping verified on framebuffer)`。
  **坑（关键）**：窗口 id 最初直接用槽号，**槽 2 的 id 撞上 `GFX_REPLY_NO_SESSION(2)`** → 客户端误判「会话失效」、
  对已映射页重发 `SYS_SHARE_PAGE` → 内核 `map_user_page` 撞 `PageAlreadyMapped` panic；改用独立 id 命名空间
  `WIN_ID_BASE(0x100) + 槽号`（避开小整数回复码）后修复。
- **MFS 元数据读批量化评估（02b-3，结论：不实现）**：量化后 GC 遍历读 = 0、可批的「扇形展开」读仅
  234 / 16384 条命令（<1% 收益），其余是数据依赖的串行链（下一块号依赖上一块读回）；故不做批量化，
  计数插桩已回滚，行为零变化。
- **开发工作流文档**：新增 [docs/dev-workflow.md](docs/dev-workflow.md) —— 环境陷阱 / 回归门禁 / 已知坑清单 /
  加一个用户态服务的 9 处接线 / 并行协作三铁律；原先散落在本地临时交接文件里的这部分内容沉淀入库。

### 文档

- `docs/dev-reference.md` §9 阶段进度表补齐 **74–87 行**（D4 / V3 / FS 流 01 / FS 流 02 / 收口补测 / 04b / 02b / 03b / 02b-2 / 02b-2 续 / N3b / 1000 Hz / N3c / N4）；§3 地址表与模块 API 同步（只读缓存 240 行、PIT 1000 Hz、`NVME_POLL_FIRST`）。
- `docs/roadmap-fs.md` 新增「02b-2 续 —— 完成路径改轮询 + 时钟 tick 100→500 Hz」与「02b-3 评估（结论：不实现）」；`docs/roadmap-driver.md` 新增 N3b / N3c / N4 小节并把 hrtimer 注同步到 1000 Hz；`README.md` 状态与勾选同步。
- `docs/dev-reference.md` §9 阶段进度表补齐 **88–89 行**（ext2 有限写 / 图形 G5）；§3 地址表新增「图形表面 / 控制台后备」布局。`docs/roadmap-fs.md` 的 ext2 段标注「已补有限写」；`docs/roadmap-gfx.md` 的 G5 标记完成（含窗口 id 命名空间那个坑）；`docs/roadmap-driver.md` 新增 **03c xHCI/USB 存储（设计，未实现）** 小节；`README.md` 状态与勾选同步。

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
