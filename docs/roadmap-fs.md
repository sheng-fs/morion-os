# Morion OS 文件系统路线图

> 目标：在 QEMU 里跑通「应用 → libvfs → fat32_srv → nvme_srv → NVMe 磁盘」的读文件链路。
> 方向：**NVMe 块设备驱动 + FAT32 文件系统**，均以用户态服务实现（微内核不解析文件系统元数据）。

---

## 1. 目标链路

```
应用(域) ── libvfs ──► fat32_srv ──(块设备 IPC)──► nvme_srv ──(MMIO/DMA)──► NVMe 磁盘
                         (文件服务)                (块设备驱动)
```

最终可验证：用户程序 `read("/hello.txt")` 读到 FAT32 镜像里的文件内容。

---

## 2. 关键前置（内核能力缺口）

| 缺口 | 现状 | 需补齐 |
| --- | --- | --- |
| PCI 枚举 | ✅ 已补齐（阶段 0） | 扫 bus/device/function，找 NVMe 控制器（class `01:08:02`），读 BAR0 |
| MMIO 映射能力 | ✅ 已补齐（阶段 0） | 新增 `Capability::Mmio` + `map_mmio` syscall，把 BAR0 映射到驱动域 |
| DMA 物理页 | ✅ 已补齐（阶段 0） | 分配**页对齐、物理连续**的队列与数据缓冲，映射到驱动域 |
| 块设备能力 | 无 | 新增能力，让 `fat32_srv` 能调用 `nvme_srv` 读写块 |
| 块设备协议 | 无 | 定义 `read_lba` / `write_lba` IPC 消息格式 |
| 中断 | ✅ 已补齐（阶段 39） | PIC + **LAPIC/MSI-X**：NVMe 完成走中断驱动，等不到中断则自动回退轮询 |

---

## 3. 阶段划分

### 阶段 0 — 内核基础设施 ✅ 已完成

- [x] PCI 枚举（[arch/pci.rs](../kernel/src/arch/pci.rs)，I/O 端口 `0xCF8/0xCFC`，后续可换 MMCFG）。
- [x] `Capability::Mmio` + `sys_map_mmio`（编号 21）：把设备 BAR 映射到驱动域（`paging::map_mmio`，置 `NO_CACHE` 非缓存）。
- [x] DMA 页分配：帧分配器 `allocate_frames(n)` 支持「连续 N 帧」分配，页对齐且物理连续。

**验证（已通过）**：`make run-nvme`（QEMU q35 + `-device nvme`）下，启动日志正确打印 PCI 设备列表并定位
NVMe 控制器（class `01:08:02`，vendor `0x1B36`），读出 BAR0 = `0xC0000000`。

### 阶段 1 — NVMe 驱动服务 `nvme_srv` ✅ 已完成

QEMU 挂载 NVMe：

```bash
qemu-system-x86_64 \
  -machine q35 \
  -drive file=fat.img,if=none,id=nvme0,format=raw \
  -device nvme,serial=MORION,drive=nvme0 \
  ...
```

用户态初始化流程（NVM Express 规范）：

1. 禁用控制器（`CC.EN=0`）。
2. 配置 admin queue：写 `AQA`（队列深度）、`ASQ`（admin submission queue 基址）、`ACQ`（admin completion queue 基址）。
3. `CC` 置 `EN=1`，轮询 `CSTS.RDY=1`。
4. `Identify Controller/Namespace`（admin 命令）拿到扇区数 + 扇区大小。
5. 创建 I/O Submission/Completion Queue。
6. 实现 `read_lba`/`write_lba`（提交 SQE → 写 doorbell → 轮询 CQ 完成）。

**验证**：读第 0 扇区，打印前 512 字节（应看到 FAT32 BPB 特征：`0x55AA` 结尾 + "FAT32" 卷标）。

### 阶段 2 — FAT32 文件服务 `fat32_srv` ✅ 已完成

- 解析 BPB → FAT 表 → 根目录 → 目录项 → 簇链。
- 实现：读目录（列出根目录）、读文件（沿 FAT 链读簇）。
- 通过块设备能力调用 `nvme_srv` 的 `read_lba`。
- **读优先，写后置**（写文件/建目录放阶段 4）。

**验证**：列出根目录 + `cat` 一个文件。

### 阶段 3 — libvfs + 集成 ✅ 已完成

- [x] `libvfs` 客户端库：把 `open`/`read`/`write`/`readdir`/`close`/`mkdir`/… 封装成对文件服务的 IPC。
- [x] 宿主机 `mkfs.fat -F 32` 生成镜像 → 挂到 QEMU NVMe → 应用读写。
- [x] FAT32 写支持（建目录 / 写文件 / 删除）。
- [x] Shell 命令行（域 8）：`help/echo/pwd/ls/cat/cd/mkdir/touch/rm/clear`。

**验证（已通过）**：用户程序 `open("/HELLO.TXT")` 读到内容；shell 交互式增删查。

### 阶段 C1 — 挂载层 `mount_srv` ✅ 已完成

- [x] 新增 `mount_srv`（域 9）：维护「挂载点前缀 → 文件服务域」表，提供最长前缀匹配查询。
- [x] libvfs 由「写死 fat32 域」改为「按挂载表路由」：路径先查 `mount_srv`，再下发子路径；
      对外 fd 编码 `(服务域 << 32) | 服务内 fd`，使 `read/write/readdir/close` 无状态路由。
- [x] fat32_srv 挂到 `/`。

### 阶段 C2 — tmpfs 内存文件系统 ✅ 已完成

- [x] 新增 `tmpfs_srv`（域 10）：平铺节点表 + 32 KiB 字节区，名称限定 8.3 短名并转大写。
- [x] 挂载到 `/tmp`，与 fat32 共存构成**统一目录树**（应用只看到单一根 `/`）。
- [x] app FS-4 自测 + shell 交互验证：`ls /tmp`→`mkdir /tmp/d`→`touch /tmp/d/f.txt`→`ls /tmp/d` 全通，
      `ls /` 仍由 fat32 服务。

### 阶段 C3 — 原创 MorionFS (MFS) 与 ext2 只读兼容

- [x] **MFS（原创文件系统）** ✅ 已完成：块设备后端（NVMe 第二 namespace → 独立 `build/mfs.img`，
      默认 64 MiB，空白盘首次挂载自动格式化 —— **M7 起按该卷真实容量定尺寸**，不再写死）。
- [x] **块设备后端（推荐）** ✅ 已完成：新增 `mfs_srv`（域 11），挂载到 `/mfs`，与 fat32(`/`)、tmpfs(`/tmp`) 共存。
- [x] 块格式：4 KiB 块 + 8 字节块头（`magic u32` + `CRC32 u32`，payload 4088）；块 0/1 为**超级块 A/B 双副本**
      （A/B 交替写 + generation 取新），块 2 起为**分配区**（MFS2 起由空闲位图管理，
      MFS3 起文件可用一/二级间接块，MFS4 起目录用变长目录项 + 扩展目录块，
      MFS5 起节点带元数据，见阶段 D）。
- [x] **写时复制**：叶子（文件/目录）→ 逐级上溯父目录 → 根的 COW 重写；旧块不覆盖写，从而支撑快照。
- [x] **内建快照**：`{gen, root_block, alloc_next}`；IPC tag `MSNP`(创建)/`MSNL`(列表)/`MSNR`(回滚)，
      libvfs 提供 `mfs_snapshot / mfs_snapshot_list / mfs_snapshot_restore`。
      快照表（上限 `MFS_MAX_SNAP = 8`）随超级块持久化，且为**环形**：满时淘汰最旧一条再写入，
      否则启动 8 次后（自测每次建一个快照）就再也建不出快照；淘汰只丢快照记录，被引用的旧块仍由 COW 保留。
- [x] 文件操作：`open/read/write/creat/mkdir/readdir/unlink/rmdir/stat/close`（经 libvfs 路由）。
- [x] 校验：每次读块校验 magic + CRC32，损坏即报错不误用。
- [x] 自测：app FS-5 —— mkdir/creat/write → 创建快照 → 覆盖写 → 回滚 → 读回旧内容（验证 COW），
      以及重挂载后读回 `PERSIST.TXT`（验证持久化）。
- [x] **能力句柄钩子** ✅ 已完成：内核新增每域句柄表（`HANDLE_SLOTS = 32`）与系统调用
      `SYS_CAP_ISSUE`(29)/`SYS_CAP_LOOKUP`(30)/`SYS_CAP_DROP`(31)；libvfs 把句柄索引编进对外
      fd 高 16 位（`[63:48] 句柄 | [47:32] 服务域 | [31:0] 服务内 fd`），`open`/`creat` 签发、
      每次 I/O 前 `cap_guard` 校验、`close` 撤销。句柄被撤销后 fd 上任何操作都失败
      —— 「无能力即不可访问」在文件 I/O 路径上落地。
- [x] **ext2_srv（只读）** ✅ 已完成：新增 `ext2_srv`（域 12），挂载于 `/ext2`，后端为 NVMe 第三
      namespace（独立 `build/ext2.img`）。解析超级块（@1024，magic `0xEF53`）→ 块大小 / 每组块数与
      inode 数 / inode 大小；块组描述符表缓存每组 inode 表起始块；inode `block[15]` 的直接 / 一级 /
      二级间接块映射（三级不实现）；目录项 `inode/rec_len/name_len/file_type/name` 顺序遍历。
      只服务 `OPEN/READ/READDIR/STAT/CLOSE`（写类 tag 一律 `u64::MAX`）；**不写盘、不自动格式化**
      —— 超级块无效即挂载失败（定位是「读既有 Linux 分区」）。
- [x] VFAT 长名读取 ✅ 已完成：`readdir` 拼接 LFN 项（32 字节/项、逻辑逆序、校验和存于 LFN 项偏移 13）
      转成 UTF-8 存入 `DirEntry.long`；`open` 短名优先、长名（ASCII 大小写不敏感）回退；shell 显示长名。
      **只读长名，不生成 LFN 项**（写入仍只写 8.3 短名）。
- [x] 挂载服务支持**运行时挂载**（`MNTA`/`MNTD`）与自动分配，而非编译期 `MOUNT_TABLE` ✅ 已完成：
      `MOUNT_MAX = 8` 槽的运行时挂载表；启动时写入引导默认项（`/`、`/tmp`、`/mfs`、`/ext2`），
      `MNTA` 可运行时挂载任意文件服务（前缀留空则自动分配最小的空闲 `/mnt<N>`），
      `MNTD` 卸载（根 `/` 不可卸载）。
- [x] libvfs 暴露 `vfs::mount(prefix, domain)` / `vfs::umount(prefix)`；app FS-6 自测覆盖
      「未挂载→不可路由 / 自动挂载→可达 / 卸载→不可路由且不影响原有挂载 / 句柄槽复用 40 轮」。
- [x] 渲染性能：`Framebuffer::pixel` / `fill_rect` 改 32 位单次写（未缓存 MMIO 帧缓冲下相机码流降 ~4x），
      终端打字只重绘底部输入行（`redraw_input_line`），LOGO 批量追加后一次性重绘。

**验证（已通过）**：`make run-nvme` 后 `ls /mfs` → `[FILE] PERSIST.TXT size=6`（内容 `MFS-OK`）；
空白盘首次启动自动格式化成功，二次启动读回旧数据（持久化）；app FS-6（运行时挂载 + 句柄生命周期）通过；
app FS-7（ext2 只读：根目录列出 `HELLO.TXT`/`SUBDIR`、读 `HELLO.TXT`/`SUBDIR/NESTED.TXT`、
`stat` 元数据、大小写不敏感回退、`creat` 被拒）与 FS-8（VFAT 长名：`readdir` 长名字段、
按长名打开与读取、大小写不敏感）通过；
shell 交互 `ls /mfs` / `cat /mfs/PERSIST.TXT` / `ls /ext2` / `cat /ext2/HELLO.TXT` 正常；
日志无 `FAILED`/`PANIC`。

### 阶段 D — 卷层与 MFS v2（进行中）

> 目标：FAT32 只作为**旧设备兼容**保留；把**原创 MorionFS 做成主力文件系统**（支持大文件、空间回收、
> 长名与元数据），并通过**分区/卷层**兼容更多真实设备文件系统（exFAT 等）。
>
> 决策（已定）：分区解析**扩展 block_srv**（`dev` 由「namespace 号」升级为「卷号」）；
> 卷上的文件系统**按签名自动探测并挂载**；MFS 每次改动磁盘布局都**升版 magic**
> （`MFS1` → `MFS2` → `MFS3` → `MFS4` → `MFS5`…），遇到旧版本/未知 magic **自动重新格式化**。

| # | 里程碑 | 产出 | 依赖 |
| --- | --- | --- | --- |
| M1 ✅ | 分区解析 + 卷管理（前置） | MBR/GPT 解析 → 卷表；`dev` = 卷号（含分区偏移）；FS 类型探测；卷列表 IPC；现有 FS 按类型自动认领主卷 | — |
| M1b ✅ | 多卷挂载（额外卷 `/usb<卷号>`） | 卷号随请求下发（tag 高位卷编码）/ 一次打开绑定一个卷；卷切换时重新解析该卷几何；服务把自己那类的额外卷自动挂到 `/usb<卷号>`；fat32 整簇缓冲去 4 KiB 簇上限 | M1、M6c |
| M2 ✅ | MFS v2 格式定稿 + 空间回收/GC | 空闲位图 + mark & sweep 回收（快照根同为可达根），COW 不再只增不减 | M1 |
| M3 ✅ | MFS v2 大文件 | 二级间接块 + 「活动间接块」写缓存，突破 ≈4 MiB（单文件上限 = 整卷容量） | M2 |
| M4 ✅ | MFS v2 目录与长名 | 目录溢出块（>170 项）、名字 ≤255 字节、更深目录 | M2 |
| M5 ✅ | MFS v2 元数据 | 时间戳、权限/owner、`rename`/`truncate`；链接顺延到 M5b | M2 |
| M5b ✅ | MFS inode 号间接层 + 硬链接 | 目录项改存 inode 号；inode 表（索引块 → 表块）使多个名字共享一个对象，`ln` 落地 | M5 |
| M5c ✅ | 软链接 | 新节点类型 `MFSL`；路径解析跟随目标（含相对路径）+ 限深防环 | M5b |
| M6a ✅ | exFAT 只读兼容 | 新文件服务域 `exfat_srv`（域 13）：引导区 + boot checksum、FAT 链、entry set 解析、分配位图、upcase 表；认领既有 exFAT 卷挂到 `/usb` | M1 |
| M6b ✅ | exFAT 读写 | 位图分配/释放、FAT 链扩展、entry set 增删 + set checksum / NameHash 生成；`CREAT/WRITE/MKDIR/UNLINK/RMDIR/TRUNCATE` | M6a |
| M6c ✅ | 大容量卷（块层多页 DMA + exFAT 去上限） | 块层 PRP 表（单命令 ≤ 128 KiB，更大的请求自动切分）；exFAT 去掉 4 KiB 簇 / 4 KiB 位图 / 8 KiB upcase 三处硬上限（集群缓冲按簇大小分配、位图与 upcase 改为按需扇区窗口）；另加 MFS「非 MFS 卷拒绝格式化」护栏 | M6b |
| M7 ✅ | MFS 按卷几何格式化（容量前置） | block_srv 补 `Identify Namespace`(CNS=0) 的 **NSZE**，整盘卷的 `sectors` 不再恒为 0；`mfs_format` 按该卷真实容量定总块数（此前写死 4096 块 = 16 MiB），挂载时校验盘上总块数不超过卷容量；卷表查询辅助收敛为 `vol_find_desc` | M2 |
| M8 ✅ | MFS 显式格式化 + 多卷（S2） | 新 tag `MKFS` + 护栏（**只接受 `MFS` / `UNKNOWN` 卷**，「新盘可格式化、别人的分区绝不吞」）；`mfs_load_state` 拆出（切卷只载入不格式化）；mfs_srv **按请求切卷**并参与额外卷挂载 `/usb<卷号>`；block_srv 打印卷表；shell 加 `mkfs.mfs <卷号>` | M7 |
| S3a ✅ | MFS 容量扩容（位图外置） | 位图移出超级块 → 独立位图数据块 + `MFBH` 头块；`mfs_bmp_flush` 取代 `mfs_write_super` 作唯一提交出口（**脏区间增量落盘** + 每块 CRC32）；三张位图由编译期定长数组改为**动态页窗口**（挂载前按卷容量预算，只增不缩）；容量上限 ≈119 MiB → **≈127.25 GiB**；新增 FS-23(a) | M8 |
| S3b ✅ | MFS 单文件突破 4 GiB（协议 u64 + 三级间接） | VFS 协议 `offset`/`size` 端到端 u64（`ReadReq`/`WriteReq`/`TruncateReq` + `Stat`/`DirEntry`）；文件节点 `size` → u64、`MFI3` 三级间接块（直接区 1008 → 1005、元数据偏移不变）；32 位内部的服务在协议边界加守卫；GC 补齐 `MFI3` 可达标记；新增 FS-23(b)(c) | S3a |
| S2 补齐 ✅ | MFS 主卷切换（持久化标记） | 主卷序号存进超级块 payload `+256`（`MFS_SB_PRIMARY`，不升 magic）；`mkfs.mfs` 置「现有最大 + 1」并随提交落盘；认领改走 `mfs_vol_claim`（序号最大者胜出 → 第一个 MFS 卷 → 回退约定卷号）；`MKFS` 回复改为**盘上回读**的序号；新增 FS-24 | M8 |

#### M1 已完成 ✅

实现要点：
- block_srv 新增**卷层**（[user/src/main.rs](../user/src/main.rs)）：启动时逐 namespace 解析
  MBR/GPT 分区表（无分区表则整盘一个卷），按卷首签名探测类型（exFAT / MFS `MFS1..MFS5` / ext2 / FAT），
  把 `dev` 从「namespace 号」改为「**卷号**」，实际 I/O 用 `(vol.nsid, vol.start_lba + lba)`。
- 新增 opcode `2 = 查询卷表`（把 `VolumeDesc` 数组写进调用方共享页，回复卷数）。
- namespace 列表由 **Identify Controller 的 `NN`（偏移 516）** 推导为 `1..=NN`。
- fat32/mfs/ext2 启动时**认领主卷**（fat32 → 第一个 FAT 卷；ext2 → 第一个 ext2 卷；
  mfs → 第一个 MFS 卷，空白盘回退约定卷号 1）。**mfs 那一支自「S2 补齐」起改为按主卷序号认领**
  （见 [S2 补齐](#s2-补齐主卷切换已完成-)，fat32/ext2/exFAT 不变）。
- Makefile 新增 `build/parts.img`（MBR：FAT32@2048 + ext2@34816 两个分区），作为 nsid 4 接入 `run-nvme`。

**验证（已通过）**：
- app 新增 **FS-9 自测**：卷表 ≥ 5 个卷；卷 0 = ns1 整盘 FAT、卷 2 = ns3 整盘 ext2（`start_lba=0`，
  证明向后兼容）；ns4 的两个分区被分别识别为 `FAT@2048` 与 `ext2@34816`。
- 回归：`ls /ext2`（lost+found / HELLO.TXT / SUBDIR）、`cat /ext2/hello.txt`、`ls /mfs`
  （PERSIST.TXT）全部正常，日志 `grep -cE "FAILED|PANIC"` = 0 —— 引入卷层未改变原有行为。
- `make clippy` exit 0（三 crate 0 warning）。

#### M1 设计要点（实施记录）

- **卷 = (nsid, start_lba, sectors, kind)**。扫描每个 namespace：若首个扇区含 MBR/GPT 分区表，
  则每个非空分区各成一个卷；否则**整个 namespace 视为一个卷**（向后兼容现有三个整盘镜像）。
- **FS 类型探测**（读卷首 4096 字节）：`EXFAT   `(偏移 3) → `MFS1`..`MFS5`(偏移 0，低字节是版本号)
  → ext2 magic `0xEF53`(偏移 1080) → `0x55AA`(偏移 510，FAT 系) → 未知。
- **`dev` 语义变更**：`BlockReq.op` 高位由「namespace 号」改为「**卷号**」；实际 I/O 用
  `nvme_rw_sectors(op, vol.nsid, lba = vol.start_lba + req.lba, ...)`。IDE 回退路径同理（单盘分区表）。
- **卷列表 IPC**：`BlockReq.op` 低 8 位新增 opcode `2 = list volumes`，把 `VolumeDesc` 数组写进调用方
  共享页（复用现有 `buf` 字段），回复卷数。
- **认领策略**（服务启动时解析自身卷号）：
  - fat32_srv → 第一个 `kind == FAT32` 的卷；
  - ext2_srv → 第一个 `kind == EXT2` 的卷；
  - mfs_srv → 第一个 `kind == MFS` 的卷；**都没有则回退到卷号 1**（空白盘无 magic，必须先格式化才能探测）。
- **namespace 枚举**：NVMe `Identify CNS=2` 取 NSID 列表（失败回退 `1..=3`）。
- **向后兼容**：现有 `nvme.img`/`mfs.img`/`ext2.img` 均为无分区表整盘镜像 → 各成一个卷，
  卷号 0/1/2 与今天的 `dev` 0/1/2 一致，四套文件系统行为不变。

#### M1b 已完成 ✅

**动机**：M1 只「按类型认领第一个匹配卷」，于是真盘上的**第二个分区**完全不可见 ——
Kali U 盘（53.6 GB FAT32 + 5 GB ext3）、多分区移动硬盘都读不了。这一刀去掉「一个文件服务
只服务一个卷」的限制。

实现要点：
- **卷号随请求下发**：挂载表新增**卷编码**（0 = 该服务的默认卷，否则 = 卷号 + 1）；`MNTQ`
  回复布局改为 `[63:40] 卷编码 | [39:32] 服务域 | [31:0] 前缀长度`。libvfs `route()` 把卷编码
  写进**请求 tag 的高 32 位**（tag 正文仍是 4 字节 ASCII，服务端用 `vfs::tag_body` 剥掉高位）——
  路径类请求因此天然带上目标卷，不必给每个请求结构体加字段。
- **一次打开绑定一个卷**：`OPEN`/`CREAT` 把当前卷记进各自服务的 fd（fat32 / exfat / ext2 各一份），
  `READ`/`WRITE`/`READDIR`/`TRUNCATE` 由 fd 决定卷；服务端用一个「当前卷寄存器」承接，
  既有的读写扇区调用全部无需改签名。
- **按卷切换几何**：同一个服务可能同时服务多个卷，而各卷的 BPB / 超级块 / 引导区几何都不同。
  卷切换（`vol` 变化）时**重新解析**该卷元数据：fat32 重载 BPB、ext2 重挂载解析
  超级块与块组描述符、exFAT 重跑 `exfat_mount`（代价是一次元数据读，换来「每卷各自正确」）。
- **额外卷自动挂载**：服务解析完自身元数据后，把「同类但非默认卷」的卷经新 tag `MNTV` 上报
  mount_srv，自动挂到 **`/usb<卷号>`**（命名直接用卷号，多个服务同时挂也不会撞名，
  且挂载点与卷表一一对应，便于排查）。
- **能力补齐**：`SYS_CALL` 需 `Capability::SendTo(to)`，内核给 fat32 / ext2 / exfat 三个域补了
  `SendTo(mount_srv)` —— 缺授权时调用被**静默拒绝**（返回 `u64::MAX`），是本次最先踩的坑。

**fat32 大簇（同一刀里的前置）**：
- 原来「整簇读进单页缓冲」把 FAT32 限制在**簇 ≤ 4 KiB**，而 53.6 GB 的 U 盘分区按容量算必然是
  **32 KiB 簇** —— 挂得上、读不出。现改为**整簇缓冲**（固定 16 页 = 64 KiB，即 BPB 的
  `SecPerClus` 只有 1 字节、最多 128 扇区所允许的最大簇），目录簇扫描与文件数据暂存共用它。
  几何不合法（非 512 B 扇区 / 簇为 0 / 簇超上限）在挂载期直接拒绝，绝不用错几何去读盘。
- ext2 块组数上限由 16 提到 **4096**（覆盖约 32 GiB 卷）；exFAT 集群缓冲改为「只补分配差额」，
  使换卷时可增大簇而**不重复共享同一页**（同地址重复共享会触发内核 `PageAlreadyMapped`）。

**验证（已通过）**：
- 新增 **FS-17 自测**：由卷表查出分区测试盘（nsid 4）的 FAT32 / ext2 分区卷号，拼出
  `/usb<卷号>` 打开目录并**读出宿主预置的 PART1.TXT / PART2.TXT**（全程只读，不写额外卷）。
- 模拟镜像全量回归：`FAILED/PANIC = 0` 且 `app: SELFTEST DONE`；shell 里 `ls /usb3` / `ls /usb4`
  分别列出分区盘两个分区的内容，`/`、`/ext2`、`/usb` 不受影响。
- 顺带修掉一处自测脆弱点：FS-12 缺「幂等准备」，上一轮被中断留下的 `/mfs/DIR12` 会让本轮
  `mkdir` 直接失败并把上一次的失败传染下去（FS-5 早有同样处理，FS-12 现已补齐）。

**真机 U 盘只读验证（已通过，2026-09-16）**：
三块真盘的分区各作一个 namespace 接入（`-drive file=/dev/sdXN,format=raw,readonly=on`，QEMU 层
拒绝一切写入），与 5 张模拟镜像同跑：

| namespace | 设备 | 内容 | 结果 |
| --- | --- | --- | --- |
| `nsid=6` | `/dev/sda1` | 53.6 GB FAT32（Kali，**32 KiB 簇**） | 卷 6 → `/usb6`，根目录 21 项、`EFI/`→`BOOT`+`KALI`、`BOOT/`→`GRUB`、`EFI/KALI/GRUB.CFG` 三级、`cat README.TXT`/`README.html` 内容与宿主一致 |
| `nsid=7` | `/dev/sda2` | 5 GB ext3（persistence） | 卷 7 → `/usb7`，根目录 `lost+found` + `persistence.conf` |
| `nsid=8` | `/dev/sdb1` | 976.6 GB **NTFS**（无驱动） | 卷 8 **无服务认领** → 不挂载，`ls /usb8` 报 `cannot open`；默认挂载树不受影响 |

- 全量自测在三块真盘同时在线的条件下跑通：`SELFTEST DONE` 1 次、`FAILED/PANIC` 0 次。
- **数据零改动**：测试前后三块盘前 8 MiB 的 sha256 完全一致。
- 这一轮测出并修掉两个**只有真盘才能暴露**的问题：
  1. **VBR 被当成 MBR**：真实 FAT32 的引导代码落在 446..509（MBR 分区表的位置），卷层按
     「type != 0 且 count != 0」把它读成 4 条主分区项 → 真 FAT32 卷**从未登记**，只登记出
     4 个指向非法 LBA 的假卷（读它们返回 `LBA Out of Range`），卷号被挤到 10 以后。
     `mkfs.fat` 造的小镜像该区域恰好全 0，所以模拟镜像一直测不出。修法：主分区项额外要求
     `boot ∈ {0x00, 0x80}` 且 `lba_start != 0`（见 [dev-reference.md](dev-reference.md) 卷层条目）。
  2. **挂载表被额外卷撑满**：`MOUNT_MAX = 8` 时「引导默认项 5 + 额外卷 3」即满，`MNTA` 自动
     分配的 `/mnt<N>` 拿不到槽位（FS-6 因此失败）。提到 **16**。
- 未覆盖：只做了**读**（写入靠 `readonly=on` 从块层挡住）；真盘上的**写**路径未验证（会改数据，
  需要一块可牺牲的盘）。

#### M1b 未覆盖

- **NTFS**：`sda` 这类整盘 NTFS 的移动硬盘仍**无法挂载**（卷层只把它判成未知类型）——
  需要新的文件服务，不在本阶段。
- MFS 不参与额外卷挂载（MFS 是自有格式，暂不把第二个 MFS 卷挂到 `/usb<卷号>`）。**→ 已由 M8 补上**（新增显式格式化入口 + 按请求切卷）。

#### M2 已完成 ✅

格式定稿（magic 由 `MFS1` 升为 **`MFS2`**，版本字段 = 2；超级块 payload 布局见下）：

| payload 偏移 | 字段 |
| --- | --- |
| +0 / +4 | `version` / `block_size` |
| +8 / +12 / +16 | `total_blocks` / `root` / `alloc_hint`（分配游标，原 `alloc_next`） |
| +20 / +24 | `snap_count` / `gen`（u64） |
| +32 + i×16 | 快照表（8 条：`gen u64` + `root u32` + `alloc_next u32`） |
| +256 … +4088 | **空闲位图**（1 = 占用；3832 字节 → 最大 30656 块 ≈ 119 MiB）※**MFS7 起该区不再使用**，位图移出超级块，见「S3a 已完成」 |

实现要点：
- **空闲位图分配器**：`mfs_alloc_block` 由「`alloc_next++` 只增不回收」改为在位图中从分配游标起
  两段扫描（`[hint, total)` → `[2, hint)`）取第一个空闲位；`mfs_bmp_set/clear` 增量维护空闲块数。
- **空间回收（mark & sweep）**：`mfs_gc` 从**当前根 + 全部快照根**出发标记可达块（目录入栈展开、
  文件展开数据块、数据块为叶），再以标记结果整体重建位图 —— 旧版本块只要还有任一快照引用就不回收，
  因此 **GC 与快照回滚兼容**。遍历或写盘失败时**不改动位图**（宁可漏回收，也不把仍被引用的块发出去）。
- **回收时机**：GC 只在「没有在建 COW 操作」时触发 —— 请求分派前（低水位 `total/16`）自动整理，
  或经新增的 `MFS_GC_TAG` 显式触发。分配失败路径**不**做回收：那时 A/B/C 缓冲里可能正持有本次
  操作已分配但尚未被根引用的块，GC 会把它们误判为垃圾。
- **位图持久化**：位图随超级块一起 COW 交替写入 A/B 双副本（CRC 覆盖），挂载时按位图重算空闲块数，
  并强制置位超级块与根所在块。
- **旧格式升级**：`mfs_mount_or_format` 要求 magic + 版本同时匹配，MFS1/未知格式一律走
  `mfs_format`（旧超级块的位图区是保留字节，没有可信空闲信息）。

**验证（已通过）**：
- app 新增 **FS-10 自测**：`MFS_STAT_TAG` 取空间用量 → 64 KiB 文件覆盖写 4 轮（空闲块显著下降，
  证明 COW 持续重新分配）→ 删除后 `mfs_gc` 回收 ≥ 100 块；随后**快照安全**用例：快照 → 覆盖 →
  GC → 再制造分配扰动（32 KiB 新文件）→ 回滚 → 读回仍是快照时的旧内容（GC 未回收快照引用的块）。
- 实机（四 namespace）连续 6 次启动：**0 FAILED / 0 PANIC**；每轮启动 `mfs-dbg` 的 `gen` 单调增长、
  盘上 GC 后占用块数稳定在 ~12→26（仅随持久化快照环缓慢增长，最终收敛），证明 COW 垃圾确实被回收。
- 旧 `MFS1` 盘首启自动重新格式化（`MFS1` magic → 格式化后 `gen=1`、`free=4093`）。
- `make clippy` exit 0（三 crate 0 warning）。

顺带修一处块层脆弱点：NVMe 轮询 CQE 的迭代上限由 1 万提到 100 万（[user/src/main.rs](../user/src/main.rs)）。
M2 的 COW 写放大使块 I/O 显著变重，宿主机负载稍高时原预算会误判超时，导致一次正常的块写失败
（曾在第 3 次连续启动时表现为 `FS10 chunked write FAILED`）。

#### M2 未覆盖（后续切片）

- 回收目前是「按需整理」（低水位或显式触发），**不做增量/后台回收**；分配失败的那次操作会直接返回
  ENOSPC，由下一次请求的空闲整理兜底。若要写满场景更平滑，可加「写前预留」或事务级回收。
- 快照环（8 条）是**持久化**的，被其引用的历史块在淘汰前不会被回收，故空间占用有一个上界而非归零。

#### M3 已完成 ✅

文件节点布局（magic 升为 **`MFS3`**、版本 3；inode 为 4 KiB 块，下表为 payload 偏移）：

| payload 偏移 | 字段 |
| --- | --- |
| +0 / +4 | `size`(u32) / `nblocks`(u32，最大逻辑块索引 + 1) |
| +8 … +4040 | **1008 个直接块指针**（≈3.9 MiB 以内零额外 I/O，小文件路径不变） |
| +4032 / +4036 | **一级间接指针** / **二级间接指针**（※MFS8 起该布局已变，见「S3b 已完成」） |
| +4040 … +4088 | 保留 40 字节（预留给 M5 的元数据：时间戳 / 权限 / 链接数） |

实现要点：
- 逻辑块 `bi` 三段映射：直接区（1008）→ 一级间接区（1022）→ 二级间接区（1022 × 1022）；
  合计 `MFS_FILE_MAX_BLOCKS ≈ 1.05M` 块（≈4 GiB），**实际单文件上限 = 整卷可用块数**
  （M2–M8 受内联位图容量限制 ≈119 MiB；**MFS7 起上限提到 ≈127.25 GiB**，见「S3a 已完成」；
  **MFS8 起改为四段映射** —— 直接区 1005 + 三级间接，见「S3b 已完成」）。
- 间接块自身也是 4 KiB COW 块，新 magic：`MFIN`（一级，槽位全是数据块指针）、
  `MFI2`（二级，槽位全是一级块指针）；每块 1022 个指针。
- **写路径带「活动间接块」缓存**（[user/src/main.rs](../user/src/main.rs)）：一次写调用常跨 1~2 个
  逻辑块且同属一个间接块，缓存它可避免对同一间接块反复「读-改-COW」；跨组时先 flush
  （COW 一级块 → 回写 inode 一级指针或二级块槽位、COW 二级块）再加载新组。缓冲复用 B：
  写数据阶段 A = inode、C = 数据块、B = 缓存（COW 上溯才用 B，那时已 flush）。
- **GC 扩展为四类块**：目录展开子项；文件标记直接槽并推入一/二级间接块；`MFIN` 标记其槽位
  （数据块为叶）；`MFI2` 推入其槽位指向的一级块。GC **不再依赖 `nblocks`** —— 计数若损坏，
  少标就会把仍在用的数据块回收掉。
- 写入长度改用 64 位校验（`offset + count > u32::MAX` 直接拒绝），避免逼近 4 GiB 时 `size` 静默溢出。
  （MFS8 起 `size` 本身已是 u64、协议偏移也是 u64，这条收窄守卫已**删除**，见「S3b 已完成」。）

**验证（已通过）**：
- app 新增 **FS-11 自测**：起点取直接区末尾 3 块处连写 5 页（必然从直接区跨进一级间接区），
  再在二级间接区写 1 页 → `stat` 大小 > 4 MiB（突破旧上限）→ 读回逐字节比对 →
  **GC + 分配扰动后再读回**（GC 若漏标间接块，扰动会立刻复用并覆盖它，从而暴露）→ 清理并回收。
- **负向对照**：临时改成「GC 不标记间接块」，FS-11 立刻报 `FS11 read back after GC FAILED`，
  证明该断点确实有效（M2 曾有过「GC 漏标数据块」的漏网 bug，这次先做了对照）。
- 实机（四 namespace）连续 3 次启动：**0 FAILED / 0 PANIC / 0 超时**；旧 `MFS2` 盘首挂自动重格式化
  （`4d465332` → 格式化后 `4d465333`、`gen=1`、`free=4093`）。
- `make clippy` exit 0（三 crate 0 warning）。

#### M3 未覆盖（后续切片）

- 只做到二级间接。三级间接在本位图容量（30656 块 ≈119 MiB）下没有意义 —— 覆盖整卷只需
  二级；若将来把位图移出超级块并扩容到 >100 万块，再补三级与 64 位 `size`。
  （**位图外置已于 S3a 完成**，容量上限 33_357_824 块 ≈127.25 GiB，>100 万块故三级间接与
  64 位 `size` 的条件已具备 —— **两者均已由 S3b 补上**，见「S3b 已完成」。）
- 间接块的写缓存只在**单次写调用**内有效，跨调用不复用（不引入跨请求的脏状态）。

#### M4 已完成 ✅

目录布局（magic 升为 **`MFS4`**、版本 4；目录为 4 KiB 块，下表为 payload 偏移）：

| payload 偏移 | 字段 |
| --- | --- |
| +0 / +4 | `ext`（扩展索引块号，0 = 无）/ pad |
| +8 … +4088 | **变长目录项区**（4080 字节） |

目录项（ext2 风格，4 字节对齐）：`+0 block(u32)` / `+4 type(u8)` / `+5 name_len(u8)` /
`+6 rec_len(u16)` / `+8 name`（无 NUL，长度由 `name_len` 给出）。`name_len == 0` = 空槽。
新 magic：`MFXI`（扩展索引块，payload 全是扩展目录块指针，1022 个槽位）。

实现要点：
- **变长目录项**：条目按 `rec_len` 串联，整块即一条链；插入用 ext2 first-fit —— 找到
  `rec_len >= 已用 + 需要` 的条目，**把该条的 `rec_len` 缩回已用**，在余量处落新条目，
  再把剩余切成空槽。删除时把空出的长度**并给前一条目**（回收碎片，不产生不可达空洞）。
  ⚠️ 「缩小前一条目 rec_len」不能省：省掉后新条目会被前一条目的 `rec_len` 跨过，
  表现为「插入成功却查不到」。
- **名字 ≤255 字节、大小写敏感**：`mfs_normalize` 只做 `.`/`..`/重复 `/` 规整，**不做** 8.3
  大写化（与 tmpfs 的 `tmp_normalize` 分道）。端到端再受单条 IPC 路径长度限制（见下）。
- **目录溢出块**：节点块条目区放不下时（16 字节名 + 8 字节条目头 = 24 字节/项 → 恰好 170 项）
  物化一个 `MFXI` 索引块，其槽位指向扩展目录块（各自也是完整目录块，`ext` 恒 0）。查找/遍历
  是「节点块 → 索引块槽位顺序」；修改则是**固定三层 COW**（扩展块 → 索引块 → 节点块），
  代价与目录大小无关。容量 = 1 + 1022 个块。
- **IPC payload 32 → 96 字节**（内核 `ipc::PAYLOAD_LEN`、`Message`、libvfs `encode_path`
  全链路同改），否则长名一进 VFS 请求就被截断；`DirEntry.long` 64 → 128 字节同步放宽回传，
  `RESULT_MAX_ENTRIES` 因此由 46 变 26（结果页仍只有一页，readdir 按上限截断）。
- **更深目录**：`MFS_MAX_DEPTH` 12 → 48（旧上限先于 IPC 路径长度成为瓶颈）；深度限制现在只
  来自路径编码长度，不再来自目录结构。
- **GC 扩展**：目录分支按变长条目遍历（`name_len != 0` 的子节点入栈）并把 `ext` 索引块入栈；
  新增 `MFXI` 分支（槽位全是扩展目录块）。遍历到「条目区末尾」是**正常收尾**，与「rec_len
  损坏」区分开 —— 早前版本把二者都当失败，导致挂载后第一次回收就报 `mfs: gc FAILED`。

**验证（已通过）**：
- app 新增 **FS-12 自测**：`/mfs/DIR12` 下建 200 个 16 字节名文件（节点块只容 170 项，其余
  必进扩展块）→ 逐项按名 `open` + 读回唯一字节（第 171 项起的查找必须穿过扩展块）→
  `readdir` 必须正好写满 `RESULT_MAX_ENTRIES` 且长名回传完整 → 57 字节长名文件建/查/读/回传
  全链路 → 20 级深目录建/写/读 → **GC + 分配扰动后抽样读回** → 清理（200 项 + 深浅目录）并回收。
- 实机（四 namespace）连续 3 次启动：**0 FAILED / 0 PANIC**；旧 `MFS3` 盘首挂自动重格式化
  （`4d465333` → 格式化后 `4d465334`、`gen=1`、`free=4093`）；第 2、3 次启动挂载已有 MFS4 盘，
  `free` 与盘上位图按位一致（19 个占用块 → `free=4077`）。
- 修掉一个真 bug：**GC 把「遍历到条目区末尾」误判为结构损坏** → 挂载后第一次回收即失败
  （`FS10 GC did not reclaim space FAILED`）。改为让 `mfs_ent_step` 在区尾正常返回，由循环条件收尾。
- 另修一个真 bug：**插入时未缩小前一条目的 `rec_len`** → 新条目被串联链跨过，出现
  「`creat` 成功但 `open` 失败」。见上「实现要点」。
- `make clippy` exit 0（三 crate 0 warning）。

#### M4 未覆盖（后续切片）

- 目录索引只有一层（`MFXI`），1022 个扩展块 × 170 项 ≈ 17 万项即满；到那一步再补二级索引。
- 名字比较是**按字节精确匹配**（大小写敏感），不做 Unicode 归一化；磁盘侧上限 255 字节，
  但端到端实际可用约 90 字节（受单条 IPC 路径 95 字节限制）。若将来要放开，需把路径改成
  多段传输或引入「目录 fd + 名字」形式的相对操作。
- 目录项不缓存，每次查找都要读「节点块 +（如有）索引块 + 扩展块」。

#### M5 已完成 ✅

节点元数据布局（magic 升为 **`MFS5`**、版本 5；40 字节，文件与目录结构相同、位置不同）：

| 相对元数据起点 | 字段 |
| --- | --- |
| +0 / +2 / +4 | `mode`(u16，低 12 位权限) / `owner`(u16，创建者域 id) / `nlink`(u32) |
| +8 / +16 / +24 | `mtime` / `ctime` / `atime`（各 u64，Unix 秒 UTC） |
| +32 … +40 | 保留 |

- **文件节点**：元数据落在原有的 40 字节保留区（payload +4056），文件节点布局因此不变。
- **目录节点**：头部由 `ext(u32) + pad(u32)` 扩成 `ext + pad + 元数据(40)`，即 `MFS_DIR_HDR`
  8 → 48，条目区 4080 → 4040 字节（16 字节名 ≈168 项/块，原为 170）。
- 新 magic：无（本切片不引入新块类型）。

实现要点：
- **时间源是 CMOS RTC**，由用户态经 `SYS_PORT_IN8/OUT8` 直接读端口 0x70/0x71（内核没有时间
  系统调用，也不需要新增）。处理 BCD / 12 小时制 / update-in-progress，并把民用历换成 Unix 秒
  （Howard Hinnant 的 `days_from_civil`）；连读两次要求一致。读失败或字段不合理 → 记 0
  （"时间未知"），不阻塞任何操作。
- **权限只存储与显示，不强制**。系统没有多用户/uid 概念，强制检查没有语义；`owner` 记创建者
  域 id，`mode` 由 `chmod` 设置、`ls -l` / `stat` 显示。这是**明确的当前边界**，等引入 uid 再启用。
- **`atime` 不随读更新** —— 否则每次 `read` 都要 COW 整个 inode 并逐级上溯到根，读路径会退化
  成写路径（与 M2 之后"读不写盘"的性质相冲突）。`mtime`/`ctime` 在写入与元数据变更时更新。
- **目录的 mtime 顺带刷新**：任何插入/删除本来就要 COW 目录节点块，把元数据改动搭在同一次
  COW 里，**不产生额外块**；写入文件时同理（inode 本就要提交）。
- **`truncate`**（`TRNC`）：截短保留前 `ceil(size / 4096)` 个逻辑块，其余映射清 0 —— 整组不再
  需要时直接丢一/二级间接指针（块本体交给 GC 按可达性回收），只有部分保留的那组才逐槽清理
  （复用 M3 的「活动间接块」缓存，避免对同一间接块反复读-改-COW）。扩展是**稀疏**的：只抬高
  `size`，不分配块，未写过的区间读回 0。
- **`rename`**（`RENM`）：可跨目录。单条 IPC payload 装不下两条绝对路径，故两条路径放进调用方
  **共享页**（`TwoPathReq { a_len, b_len, buf }`，与 `readdir` 的 `DirReq.buf` 同模式），
  布局 `src\0dst`。顺序刻意做成「**先建新名 → 再删旧名**」：反过来会留一段"inode 不可达"的
  窗口，万一插入失败文件就真丢了（块会被下一次 GC 回收）。目标已存在且是文件 → 覆盖；
  目标是非空目录或类型不匹配 → 拒绝；拒绝把目录移进自己的子孙（成环）。
- **`stat` / `chmod` 改用共享页传路径**（`PathReq { aux, buf }`）：既因为 payload 装不下
  "路径 + 结果页地址"，也因为结果页地址必须由客户端指定（shell 与 app 的结果页不同，
  此前 `stat` 硬编码写 `RESULT_BUF`，只有 app 能用）。
- **`DirEntry` 增加 `mode` / `owner` / `nlink` / `mtime`**（152 → 168 字节）：`ls -l` 因此
  只需一次 `readdir`，不必逐条目 `stat`。非 MFS 的服务填默认值。`Stat` 同步扩到 40 字节。
- **shell 新增** `mv` / `chmod` / `truncate` / `stat` 与 `ls -l`；`ls -l` 打印
  `权限属主链接数大小时间`，时间由 `civil_from_days` 反算成 `YYYY-MM-DD HH:MM`。

**验证（已通过）**：
- app 新增 **FS-13 自测**：建 12 KiB 文件 → `stat` 校验 size / owner（= app 域 7）/ 默认
  `mode=644` / `nlink=1` / **`mtime` 与 `ctime` ≥ 2020-01-01**（RTC 真的读到了日历时间，而不是 0）
  → `chmod 600` 后读回并**经一次 GC 仍保持** → `truncate` 到 4 KiB（保留页内容不变、越过新末尾
  读到 0 字节）→ 扩到 8 KiB（新区全 0）→ 截到 0 再写一页 → **rename 跨目录**（旧名消失、
  新名内容与 mode 保持）→ rename 目录 → **拒绝把目录移进自己的子孙** → **rename 覆盖已存在
  文件**（内容变成源的）→ `readdir` 校验目录条目也带 mode/mtime/owner → **GC + 分配扰动后
  重读内容与 mode 均不变** → 清理并回收。
- 实机（四 namespace）启动：**0 FAILED / 0 PANIC**；旧 `MFS4` 盘首挂自动重格式化
  （`4d465334` → 格式化后 `4d465335`、`gen=1`、`free=4093`）。
- `make clippy` exit 0（三 crate 0 warning）。

#### M5 未覆盖（后续切片）

- **软链接顺延到 M5c**（硬链接已在 M5b 落地）。软链接需要新节点类型 + 路径解析跟随
  目标（含相对路径）+ 限深防环，是独立的一块机制。
- `rename` 不改动被移动节点的 `ctime`（POSIX 语义上应更新），以免为一次改名额外 COW 整个
  inode；目录的 mtime 则随插入/删除顺带更新。
- `mode` 不参与访问判定（无 uid 概念）；`owner` 只是展示。
- 跨文件服务（跨挂载点）的 `rename` / `ln` 不支持 —— 需要搬迁数据，届时更适合做 `cp`。

#### M5b 已完成 ✅

**动机（M5 收尾时记录的架构冲突）**：MFS5 的目录项**直接存对象块号**，而 COW 让块号每次
改写都变。写入多链接文件时，`mfs_write_file` 只把「当前路径」的父条目重指向新块，另一个
硬链接条目仍指向旧块 → **内容分叉**。两条出路：① 引入「inode 号 → 块号」间接层；
② `nlink > 1` 时按路径全树重定向。**选定 ①**（`nlink == 1` 的常见路径也不再有任何额外开销，
且 ② 每次写多链接文件都要扫全树）。

布局（magic 升为 **`MFS6`**、版本 6）：

| 结构 | magic | 内容 |
| --- | --- | --- |
| inode 表索引块 | `MFIX` | 1022 个槽，第 k 槽 → 第 k 个表块（0 = 未分配） |
| inode 表块 | `MFIT` | 1022 个槽，第 j 槽 → `ino = k*1022 + j` 的对象块号 |
| 目录项 | （沿用） | 4 字节子字段语义由「块号」变为 **inode 号**，其余（type / name_len / rec_len）不变 |

- **`ino` 0 保留为无效/空闲；根目录恒为 `ino 1`** —— 根永不改名/删除，故超级块里
  **不需要存根号**，只需 `itab_root`。
- 超级块新增：`+12 ino_count`、`+32 itab_root`、`+36 ino_hint`；快照记录从 16 字节扩到
  **24 字节**（`gen u64` + `itab_root` + `ino_hint` + `alloc_hint`）—— 快照要连**它自己那版
  inode 表**一起记，否则回滚后 ino 会翻译到回滚后的对象上。
- 索引块内容在内存里留一份完整镜像（`MFS_ITAB_MEM`，4088 字节），查表**不读索引块**；
  只有表块要读，且带单条目缓存（连续 ino 只读一次）。

实现要点：
- **写路径的统一出口是 `mfs_commit_object(ino, buf, magic)`**：COW 对象块 → 更新表槽
  （COW 表块）→ COW 索引块 → 写超级块。表块/索引块是普通分配块，GC 必须把它们标记为可达。
- **链式传播整体删除**：目录项存 ino 后，父条目在对象更新时**不变**，因此
  `mfs_propagate` / `mfs_dir_set_child` / `MfsFrame`（从根到叶的回写链）全部不再需要。
  收益是**写代价与目录深度无关**：往深目录里建文件不再重写整条祖先链。这是 inode
  间接层带来的最大简化。
- **硬链接** `LINK`：`src` 必须存在且是文件（目录不链接，避免成环），`dst` 必须不存在
  （不覆盖）。实现就是「在目标目录插一个指向**同一个 ino** 的条目」+ 抬 `nlink`。
  有 inode 表之后，`mfs_write_file` 只更新那一个表槽 → **所有名字自动看到新内容**。
- **`unlink` 是「摘名字」而非「删对象」**：`nlink > 1` 只递减计数，减到 0 才 `mfs_free_ino`
  释放 ino 槽；对象块随即不可达，由 GC 按可达性回收。
- **GC 按根分别翻译**：可达根是「(当前 inode 表, 根 ino) + 每个快照的 (表, 根 ino)」。
  遍历某个根时，先用**该根自己的**索引块（载入 X 缓冲）+ 表块（GC 专用缓冲）把目录项里的
  ino 翻成块号，并**标记索引块与全部已用表块**（它们是元数据，漏标会被回收后重新分配出去）。
  用当前表翻译快照的条目会把历史块当成最新版本而漏标，故必须分开。
- **GC 的去重必须按根做**（实机回归暴露）：M2 起 GC 用可达位图兼作「已访问」来去重，这在
  目录项存块号时是安全的（同一目录块的子节点与根无关）。引入 inode 表后不再成立 ——
  同一个目录块被当前树与某快照同时引用时，块内 ino 在两张表下翻译出**不同**的对象块；
  若沿用可达位图去重，快照那次会被整个跳过，快照引用的旧块漏标 → 被 GC 回收 → 新分配
  覆盖 → 回滚后读到垃圾（FS-10 以 `read after restore+GC FAILED` 当场暴露）。修法：
  另开一张「本根已访问」位图，每换一个根就清零，可达位图只累积并集。
- **挂载必须把持久化位图载回内存**（实机持久化回归暴露）：位图随超级块 COW 交替写 A/B
  双副本以保证崩溃一致，但挂载路径最初只 `mfs_bmp_recount()` 重算空闲数，**从未把副本里的
  位图字节拷回 `MFS_BITMAP`**。新进程的该静态量是全零，于是每次重挂载都把整盘当作空闲 ——
  挂载仅再补标超级块与当前表元数据（`free` 异常接近 `total` 即为症状），随后分配立刻覆盖
  当前树与快照仍在引用的块；等到首次 GC 翻译快照表时读到被覆盖的块，`mfs_gc` 失败
  （`mfs: gc FAILED` + `FS10 GC did not reclaim space FAILED`），且失败使位图更不再被正确重建，
  恶性累积。修法：在采纳某份超级块副本后、`mfs_bmp_recount()` 之前，把该副本 `+256` 起的
  `MFS_BITMAP_BYTES` 字节整体拷回 `MFS_BITMAP`；并新增 `mfs_mark_itab_meta()`，挂载时把
  **当前表与每张快照的表**（索引块 + 全部已用表块）强制标为占用，作为陈旧位图下的第二道防线。
- GC 遍历栈顶指针改用 `addr_of_mut!` 取裸指针（对 `static mut` 造 `&mut` 属未定义行为）。
- **崩溃残留的取舍（已知、已记录）**：若在「摘掉条目」与「释放 ino」之间掉电，会留下一个
  指向已删除对象的非零槽位。它**不会被重新分配**（分配只看零槽），也**没有任何路径解引用它**
  —— 所以不会别名到别的文件，只是一次轻微泄漏（耗尽 inode 槽需要极多次这种崩溃）。
  本轮不做 GC 期表修复，以保持回收尾段（位图重建之后不再分配块）的简单性。
- shell 新增 `ln <src> <dst>`。

**验证（已通过）**：
- app 新增 **FS-14 自测**：建文件 → `link` 出第二个名字 → 两名字 `nlink == 2` 且
  mtime/owner 一致（同一个 inode）→ **从第二个名字改写，第一个名字读到新内容**（旧实现
  在这一步会分叉）→ GC + 分配扰动后仍一致 → 摘掉一个名字后数据仍在且 `nlink == 1`
  → 「再链一次 + 摘掉原名字 + 改名」后内容仍可读 → 负向用例（链接目录 / 目标已存在 /
  源不存在都必须失败）→ 清理并回收。
- 回归覆盖 FS-1..FS-14（含 M2 空间回收、M3 大文件、M4 多块目录、**M5 快照回滚**）。
- **持久化连续性回归**：对同一 MFS6 盘连续启动 3 次（首次空白自动格式化 + 两次重挂载），
  三次均 `FAILED/PANIC = 0`，`mfs-dbg` 的 `free` 随 GC 正常回升、`gen` 单调增长；
  盘上快照的索引块与表块在空闲位图中均为**已占用**（修复前为未占用）。
- `make clippy` exit 0（三 crate 0 warning）。
- 旧的 `MFS5` 及更早的盘首挂自动重格式化。

#### M5c 已完成 ✅

**动机**：软链接是 Unix 语义里「路径即数据」的那一半 —— 存的只是一段目标字符串，
**解析时**才解释，且要求能跨目录、能悬空（目标可以之后才建）。它需要新节点类型 +
路径解析跟随 + 防环，与硬链接（M5b，共享同一个 inode）是两套完全不同的机制。

节点类型与布局：

| 结构 | magic / type | 内容 |
| --- | --- | --- |
| 软链接节点 | `MFSL` | `+0 size` = **目标路径字节数**、`+8 起` = 目标字节（无 NUL）、`+MFS_FILE_RESERVED_OFF 起` 40 字节元数据 |
| 目录项 type | `MFS_TYPE_LINK = 3` | 与文件（1）/目录（2）并列 |

- **沿用文件布局**是刻意的：软链接没有数据块（目标内联，fast symlink），把元数据放在与
  文件相同的偏移后，**所有元数据读写函数用 `is_dir = false` 就能作用于软链接**，不必给
  十几个函数再加一种节点类型分支。`size` 复用为「目标串长度」，正好对上 `lstat` 的语义。
- `mode` 的**高 4 位编码节点类型**（与 ext2 `i_mode` 的 `S_IFMT` 同构），低 12 位是权限。
  为什么需要它：`vfs::Stat` / `vfs::DirEntry` 只有 `is_dir` 一个类型信号，光靠它区分不出
  「普通文件」与「软链接」。编码进已经存在的 `mode` 字段即可让 `ls -l` 显示 `l`，不必改
  协议结构体。`chmod` 只改低 12 位（类型位随节点固定，否则能把文件 chmod 成目录）；非 MFS
  的文件服务不填类型位，客户端按 `is_dir` 回退显示，故对既有服务无影响。

解析跟随（`mfs_resolve_ex(canon, follow_leaf)`）：
- **就地展开 + 整条重走**：把路径里的链接分量替换成它的目标（绝对目标直接用，相对目标
  接在**链接所在目录**之后），保留其后的剩余分量，重新规范化后从头再走一遍。不接着展开点
  往下走，是因为目标里的 `..` 可能吃掉展开点**之前**的目录，分量位置会整体变化。
- **限深防环**：`MFS_SYMLINK_MAX_DEPTH = 16`，超出即解析失败 —— 相互指向与自指链接只会
  报错，不会无限展开。展开后超过 `MFS_PATH_MAX`（= 单条 IPC 路径上限）也直接失败，
  不截断成一条错路径。
- **两种跟随模式**：`mfs_resolve`（跟随末段，供 `open`/`read`/`stat`/`chmod`/`truncate`）
  与 `mfs_resolve_no_follow`（**不**跟随末段，供 `unlink`/`rmdir`/`rename`）。路径**中间**
  分量上的链接两种模式都跟随 —— `/a/link/b` 必须走进 link 指到的目录才找得到 b。
- **绝对目标是服务命名空间里的路径，不含挂载前缀**：文件服务只看得见自己那棵子树，
  直接把 `/mfs/a` 存下去会被解释成服务内部的 `ROOT/mfs/a`，解析必然失败（真机探测当场
  复现：`ln -s` 成功、`ls -l` 显示正常、`cat` 却打不开）。修法在**懂挂载层**的客户端：
  `vfs::symlink_into` 把落在**同一挂载点内**的绝对目标剥掉前缀（`/mfs/a` -> `/a`）；
  落在别的文件系统上的原样存下，成为悬空链接（见「未覆盖」）。相对目标不受影响。

顺带修掉的两个 bug（都属于「只有引入新节点类型才会暴露」的一类）：
- **GC 遇到 `MFSL` 会放弃整次回收**：`mfs_gc_drain` 对「既不是目录也不是文件」的可达块
  判定为「盘上结构不可信」并中止。少了软链接这一支，**只要卷上存在软链接，空间回收就
  静默失效**。软链接目标内联、不引用任何其它块，按叶子处理即可。
- **`rename` 撞上悬空软链接会插出两条同名条目**：`mfs_dir_insert` 本身不查重，而改名/建链
  的存在性判断原用跟随式解析 —— 悬空链接解不出目标，于是被当成「不存在」而直接插入。
  改为不跟随式判断后，悬空链接也按「已存在」处理。同时把重新插入时的条目类型从
  「非目录即文件」改为按**节点魔数**取，否则 `mv` 一个软链接会把它的条目写成普通文件。

shell 新增 `ln -s <target> <name>`（`ln` 仍是硬链接）。`-s` 时目标**不做路径解析**
（`resolve_in_cwd` 只作用于链接自身的路径）—— 它只是一段要存进节点的字符串。

配套补齐的两个缺口（`readlink` / `lstat`）：
- **`readlink`**（协议 tag `RDLK`）：不跟随式解析到链接自身，把节点 payload 里的目标串
  写回共享页。回复的是**服务命名空间内**的目标串，客户端 `vfs::readlink_into` 按挂载前缀
  **加回**去（存的时候剥掉、读的时候加回），界面上看到的才是用户输入的原始路径。
- **`lstat`**（协议 tag `LSTA`）：与 `stat` 同一路径，只是改用不跟随式解析，于是悬空链接
  也能看到「它自己是链接、size = 目标串长度」。
- 跨文件系统的绝对目标**改为在创建时拒绝**（`vfs::symlink_into` 剥前缀时发现目标不在同一
  挂载点即返回失败）。之前是原样存下变成悬空链接 —— 那不是「软链接」而是「指向不存在路径
  的链接」，用户无从分辨，不如直接报错。

**验证（已通过）**：
- app 新增 **FS-19 自测**（15 项断言）：绝对目标 / 同目录相对目标 / 带 `..` 的相对目标 /
  **路径中间分量是目录链接**；`stat` 跟随到目标类型（`Mode` 是 `-rw-r--r--` 而不是链接）；
  `readdir` 里它才是链接（`mode` 高位 = LINK、`size` = 目标串长度）；`rm` 摘掉链接后**目标
  内容不变**、`rmdir <指向目录的链接>` 必须失败；`mv` 移动链接自身且类型不丢；悬空链接
  「建得出来但 `open` 失败、且该名字已被占（不能重复创建）」；相互指向与自指链接解析失败
  （不挂死）；末尾清理保证幂等。
- app 新增 **FS-20 自测**（9 组）：`readlink` 对绝对 / 相对 / 带 `..` 三种目标**逐字节**
  往返（绝对目标要能还原出挂载前缀）；`lstat` 看链接自身（类型 `LINK`、size = 目标串长度）
  而 `stat` 跟随到目标（类型 `FILE`、size = 4096）；悬空链接 `stat` 失败但 `readlink`
  与 `lstat` 正常；非链接上 `readlink` 必须失败；`lstat` 对文件 / 目录与 `stat` 等价；
  跨挂载点的绝对目标**创建必须失败**（`/usb/...`、`/tmp/...`、不存在的挂载点）。
- shell 实测：`ln -s` → `ls -l` 显示 `lrwxrwxrwx ... links=1 size=00000007`（07 = 剥掉
  挂载前缀后的目标 `/PT.TXT`）→ `stat` 报 `Type: regular file`（跟随）→ `cat` 经链接读到
  内容 → `rm` 只摘链接、目标仍在。
- 全量回归 `SELFTEST DONE 1 次 / FAILED+PANIC 0 次`（覆盖 FS-1..FS-20）。
- `make clippy` exit 0（三 crate 0 warning）。

#### M5c 未覆盖（后续切片）

- **跨文件系统的软链接**：创建时即被拒绝（目标不在同一挂载点）。要有真正的跨 FS 链接，
  需要在 VFS 层做「重解析 + 二次路由」，是独立的一块机制。
- 硬链接不能指向软链接（`ln` 要求目标是普通文件），与 Unix 允许 `link` 到 symlink 不同。
- 软链接目标长度上限由 `MFS_LINK_MAX`（≈4 KiB）与单条 IPC 路径上限共同约束。

#### M6a 已完成 ✅

**动机**：真实可移动设备（U 盘 / SD 卡）在 >32 GiB 上普遍用 exFAT，`fat32_srv` 读不了。
先把**读**做扎实（这一半只需要解析，不需要写盘的一致性推理），写留到 M6b。

挂载与后端：
- 新增 `exfat_srv`（**域 13**），挂 `build/exfat.img`（NVMe **namespace 5**、`nsid=5`），
  挂载点 **`/usb`**。镜像由宿主 `mkfs.exfat -L MORIONUSB` 预格式化
  （Makefile 新增 `EXFAT_IMG` / `EXFAT_MIB = 16` 与 `$(EXFAT_IMG)` 目标），
  与 ext2 一样**只读、不自动格式化**：签名/校验和不符即挂载失败。
- `exfat_srv` 复用 ext2 的结构模板：`vol_claim(.., VOL_KIND_EXFAT, 5)` 认领 exFAT 卷 →
  5 页块缓冲（`+0x11_4000` A/B/C、`+0x11_7000` D、`+0x11_8000` E，**D/E 必须连续**）→
  VFS tag 分发。

解析要点（对齐 Microsoft exFAT 规范）：
- **引导区**：主引导区在 sector 0、备份在 sector 12，各占 12 扇区；`jmpBoot[3]` 后是 `EXFAT   `
  签名、`0x55AA`；`BytesPerSectorShift`(108) 必须为 9、`SectorsPerClusterShift`(109) ≤ 3；
  `FatOffset`(80) / `FatLength`(84) / `ClusterHeapOffset`(88) / `ClusterCount`(92) /
  `FirstClusterOfRootDirectory`(96)。簇号 `c` 的 LBA = `heap_offset + (c - 2) * spc`。
- **boot checksum**：sector 11 的低 4 字节 = 对 sector 0..10 逐字节
  `sum = rot(sum) + (sum >> 1) + byte`（`rot` = 最低位为 1 时置最高位），**跳过偏移 106/107/112**。
  ⚠️ 实现陷阱：块层单次 READ 上限是**一页 / 8 扇区**，而此处要读 11 扇区 → 必须分两段
  （0..7 读进页对齐的临时页 B 再拷进 scratch，8..10 同理），否则一次 `11` 扇区的请求直接被块层拒绝。
- **FAT 链**：每簇一个 u32（`0` = 空闲；`>= 0xFFFFFFF8` = 链尾）；`NoFatChain`（GeneralSecondaryFlags
  的 bit1）为 1 时文件/目录连续、**忽略 FAT 链**，直接 `first + i` 顺序取簇。
- **目录 entry set**：`0x85` File + `0xC0` Stream Extension + N×`0xC1` File Name，
  每条 32 字节，每个 `0xC1` 承载 15 个 UTF-16 码元；`0x00` = 目录结尾。
  名字 UTF-16（含代理对）→ UTF-8；`FileAttributes`(0x0010) 判目录；`DataLength` / `ValidDataLength`
  取 64 位文件大小；时间由 `Create/Modify` 时间戳（DOS 风格 + 10 ms 增量）换算为 Unix 秒。
  `set checksum` 校验 entry set 完整性（跳过首项的字节 2/3）。
  NameHash 只做**校验跳过**，查名仍走逐字符比较（ASCII 大小写不敏感），故不做 hash 生成。
- **系统项**：`0x81` 分配位图（1 = 已占用，位 `cluster - 2`）与 `0x82` upcase 表在挂载时
  **整体载入内存**（位图 ≤ 4096 字节、upcase 表 ≤ 8192 字节），`0x83` 卷标忽略。
  根目录所在簇必须被位图标为占用 —— 这一步同时作为位图解析的端到端校验。
- **只读分发**：`OPEN` / `READ` / `READDIR` / `STAT` / `CLOSE`，其余 tag 一律回 `u64::MAX`。
  fd 表 16 槽，只记路径（`read`/`readdir` 时按路径重新解析，无跨请求脏状态）。

客户端改动：`vfs::EXFAT_DOMAIN = 13`；`mount_srv` 默认表加 `/usb → exfat_srv(13)`；
app/shell 的共享缓冲页（`RESULT_BUF` / `WRITE_BUF` / `SHELL_RESULT_BUF`）共享给域 13；
内核新增域 13（`ipc::init(14)` / `cap::init(14)` / `pager::init(14)` + 能力授权 + 加载/派生）。

**验证（已通过）**：
- app 扩展 **FS-9**（卷表 ≥ 6 且含 `kind=EXFAT / nsid=5 / start_lba=0` 的卷）与
  新增 **FS-15**（`open /usb` → `readdir` 必须正好 0 条（空卷只含系统项 + 卷标）→
  `stat /usb` 必须 `is_dir == 1` → `open /usb/NOPE.TXT` 必须失败）。
- 实机（五 namespace）启动：**0 FAILED / 0 PANIC**，`app: SELFTEST DONE`；
  启动日志 `exfat-dbg: vol=5 cluster=4096 clusters=3584 root=5 bitmap=448 upcase=5836`
  与宿主 `mkfs.exfat` 的 16 MiB 参数逐项一致（4 KiB 簇 / 3584 簇 / 根簇 5 / 位图 448 B /
  upcase 表 5836 B）。
- 宿主 `fsck.exfat -n build/exfat.img` 干净（只读挂载未改动镜像）。
- 持久化连续性回归：对同一组镜像连续启动 3 次（首次空盘 + 两次重挂载）均 `FAILED/PANIC = 0`，
  `mfs-dbg` 的 `free` 随 GC 回升、`gen` 单调增长，`exfat-dbg` 三次一致。
- `make clippy` exit 0（三 crate 0 warning）。

#### M6a 未覆盖（已由 M6b 补齐）

- **只读**：M6a 期间 `CREAT` / `WRITE` / `MKDIR` / `UNLINK` / `RMDIR` / `TRUNCATE` 一律返回失败；
  M6b 已实现（`rename` / `chmod` / `link` 仍不支持，返回失败）。
- 边界：只支持 **512 B 扇区**（`BytesPerSectorShift == 9`）、**簇 ≤ 4 KiB**（`SectorsPerClusterShift ≤ 3`，
  受块层「单次 READ ≤ 一页」约束）、位图 ≤ 4096 字节、upcase 表 ≤ 8192 字节；
  更大参数在挂载时被拒绝（`stage` 号可定位）。
- 名字仍以 **ASCII / BMP 为主**：UTF-8 → UTF-16 只处理基本多文种平面（4 字节序列拒绝）；
  upcase 表（本卷 2918 项）用于生成 NameHash。
- 无免密/无 loop 挂载的宿主机环境**无法向 exFAT 预置文件**，故读侧只能验证自建对象；
  M6b 的写盘结果可用 `fsck.exfat` 反向校验（已完成，见下）。

#### M6b 已完成 ✅

**范围**：`CREAT` / `WRITE` / `MKDIR` / `UNLINK` / `RMDIR` / `TRUNCATE`（不含 `rename` / `chmod` / `link`）。
仍复用域 13 与 `/usb`，无新增域、无 IPC 协议变更（复用既有 VFS tag）。

实现要点：
- **写原语**：`block_write_dev` 逐扇区/逐簇写入（与读同样受「单次 ≤ 一页 / 8 扇区」约束）；
  `exfat_fat_set` 读-改-写 FAT 表项（`num_fats == 2` 时同步镜像第二份 FAT）；
  分配位图以内存副本为准，**每次改动后整体回写位图链**（只写覆盖到的扇区，源保持页对齐）。
- **簇分配 / 释放**：`exfat_alloc_cluster` 从分配游标起找第一个空闲位 → 置位 + FAT 置链尾 → 落盘；
  `exfat_free_chain` 清位图并把 FAT 表项归零。
- **统一 FAT 链**：新建的文件/目录一律用 FAT 链（不置 `NoFatChain`）；改写既有的**连续文件**时
  先按 `first + i` 补齐簇间 FAT 链接（转成链式）—— 之后读写只有一种寻簇方式，路径不再分叉。
- **entry set 构造**：`0x85` File + `0xC0` Stream Extension + N×`0xC1` File Name；
  - 名字走 UTF-8 → UTF-16（仅 BMP，4 字节序列拒绝）；
  - **NameHash** 用已载入内存的 **upcase 表**（本卷 2918 项，超出表尾映射为自身）：
    逐码元 `hash = hash.rotate_right(1) + upcase(c)`，末尾再 `rotate_right(1)` —— 这是
    M6a 只读时唯一没用到、写路径必须生成的东西；
  - **SetChecksum** 覆盖整组（跳过首项字节 2/3）；
  - 时间戳由 CMOS RTC（复用 M5 的 `mfs_now()`）换算成 exFAT 打包时间（含 10 ms 增量）。
- **目录增删**：`exfat_dir_locate` 定位条目组（簇 + 簇内下标，供原地改写/删除）、
  `exfat_dir_find_slot` 找可容纳整组的连续空位（已删除条目可复用，成组条目整体跳过）、
  `exfat_dir_put_set` / `exfat_dir_del_set` 读-改-写所在簇；
  组放不下时 `exfat_dir_grow` 追加一个清零簇（转成链式）。
  ⚠️ **条目组不跨簇**：找空位只在单簇内计数，否则读侧 `parse_file_set` 会把跨簇的组判为无效。
- **一致性顺序**（不留下悬空引用）：创建 = 先备好簇与数据、**最后写目录项**；
  删除 = **先摘目录项**（对象即刻不可达）、再释放簇；改写文件 = 先写数据、再更新条目长度。
- **`truncate`**：exFAT 没有稀疏文件 —— 扩展会**真实分配并清零**新簇；
  截短时把新尾簇的 FAT 置链尾并释放后续链。`ValidDataLength ≤ DataLength` 按语义同时更新。
- **目录防"洞"**：扩容时把链尾簇剩余的未使用项（`0x00`）填成非 0 的填充项（`0x20`，
  「已删除的良性次级项」）。

**两个实机回归暴露的真 bug**：
1. **目录扫描遇 `0x00` 整体停止** → 当某一簇「剩余空间放不下整组条目」而扩容、其尾部留下 `0x00` 时，
   读侧会把该 `0x00` 当成目录结尾，**看不到后续簇里的条目**（表现为建得进去、`unlink` 找不到，
   批量删除时 `FS16 bulk unlink FAILED`）。修法：`0x00` 只表示「本簇余下未使用」，
   跳过本簇余下部分后**继续沿链扫描**（`exfat_dir_scan` / `exfat_dir_locate`）。
2. **扩容留下 `0x00` 空位 → 宿主 `fsck.exfat` 判卷损坏**：报
   `other entry(type: 0x05) follows unused entry`。exFAT 要求「非 0 项不得出现在 `0x00` 之后」，
   故扩容前必须把链尾的 `0x00` 空位填成非 0（见上）。修前 `fsck.exfat` 报 corrupted，修后 clean。

**验证（已通过）**：
- app 新增 **FS-16 自测**（M6b）：`creat` → 写 6000 字节（**跨簇**，验证链扩展）→ `stat` 大小 →
  分页读回逐字节比对 → `truncate` 截到 1000（前缀保留、超出部分读不到）→ 扩回 6000
  （新区必须读到 **0**）→ `mkdir` + 新目录 `readdir` 为 0 → 目录内建文件/写/读 →
  **非空 `rmdir` 被拒** → **`unlink` 目录被拒** → 建 45 个文件（135 条目 > 单簇 128 项，
  **强制目录扩容**）→ 列举写满结果页 → 逐个 `unlink` + `rmdir` 清理 →
  **收尾 `readdir` 必须为 0**（证明系统项/卷标被正确跳过、卷已清空）。FS-15 因此放宽为
  「列举成功且为整条目数」，把「空卷」强校验交给 FS-16 收尾。
- **宿主交叉验证**：每次实机启动后 `fsck.exfat -n build/exfat.img` 均为
  `clean. directories 1, files 0` —— 这是 M6a 只读阶段无法获得的独立校验（写路径的正确性
  由 exfatprogs 确认，而非自证）。
- 实机（五 namespace）连续 3 次启动（全新盘 + 两次复用已删除条目 / 已扩容目录）：
  **0 FAILED / 0 PANIC**，`app: SELFTEST DONE`，`fsck.exfat` 三次全 clean。
- `make clippy` exit 0（三 crate 0 warning）。

#### M6b 未覆盖（后续）

- `rename` / `chmod` / `link` 不支持（`RENM`/`CHMD`/`LINK` 一律回 `u64::MAX`）。
- 无「已用簇计数」（`PercentInUse`）与卷脏标记维护；写路径已保证位图/FAT/条目一致，
  但不更新引导扇区里的统计字段。
- 目录只增不缩：扩容出的簇在文件删完后不回收（`0x20` 填充项会保留）。
  空间回收需要额外的目录压缩逻辑（把后续条目前移），留待需要时再做。
- 名字仍是 ASCII/BMP；补充平面（4 字节 UTF-8）与大小写折叠不做完整 Unicode 处理。

#### M6c 已完成 ✅

**动机**：M6a/M6b 虽已能读写 exFAT，但被三处硬编码上限卡住，**真实 U 盘/大容量卡基本都用不了**：
簇 ≤ 4 KiB、分配位图 ≤ 4096 字节（→ 卷 ≤ 约 128 MiB）、upcase 表 ≤ 8192 字节。根因是块层
单次 READ 只支持一页（≤ 8 扇区）、且 PRP1 单页。这一刀把「兼容真实设备」真正兑现。

块层（对所有文件服务生效）：
- **多页 DMA**：`nvme_rw_sectors` 按 NVMe 规范组织 PRP —— 1 页只用 PRP1；2 页时 PRP2 直接
  指向第 2 页；**> 2 页时 PRP2 指向 block_srv 私有的 PRP 表页**，表项依次是第 2..N 页的
  物理地址（逐页 `sys_virt_to_phys` 反查，末项可指半页）。单条命令上限 256 扇区 = 128 KiB。
- **大请求切分**：`block_srv` 把超过单命令上限的请求按 256 扇区切段（段大小是页的整数倍，
  故每段缓冲天然页对齐）；`BlockReq.count` 因此不再有 256 的语义限制。
- 旧代码里 `count > 8 → false` 的拒绝直接消失 —— 这正是过去 fat32/exFAT 只能读整簇 ≤ 4 KiB 的原因。

exFAT 去上限：
- **集群缓冲动态化**：挂载解析出簇大小后，按 `簇字节数 / 4096` 页逐页 `sys_alloc_page` +
  同地址 `sys_share_page` 给 block_srv（上限 64 页 = 256 KiB 簇，`spc_shift ≤ 9`）。
  目录扫描/改写、文件数据暂存的整簇 I/O 都走这块缓冲。
- **位图改为按需窗口**：不再整体载入内存。`exfat_bitmap_byte` 定位目标位所在 **512B 扇区**，
  以**写回缓存**方式维护（一次只驻留一页窗口，切换扇区前把脏扇区落盘）。
  大容量卷的位图可达数十 KB..MB 级，也能正确工作；顺带用「位图位数 ≥ 簇数」做挂载期校验。
- **upcase 改为按需窗口**：只在生成 NameHash 时用到，同样按扇区窗口读（命中缓存时连
  FAT 链换算都省掉）。表大小不再受限。
- boot checksum 改为**逐扇区**读取累加（不再需要「连续两页」缓冲）。

**踩到的真坑（内核 panic）**：
- exFAT 集群缓冲是**按簇大小动态占页**的，最大会一直铺到 `0x15_3FFF`。而 block_srv 原本把自己的
  卷扫描页/PRP 表页放在 `0x12_0000/0x12_1000` —— 大簇时两者**重叠**：exFAT 把集群缓冲页
  「同地址共享」给 block_srv 时，目标域该地址已被映射 → 内核直接
  `KERNEL PANIC: map_user_page: PageAlreadyMapped`。
  修法：把 block_srv 的私有页移到 exFAT 预留段之上（`0x16_0000/0x16_1000`），并在源码里
  写清**用户态固定地址分区表**（见 `VOL_SCRATCH_VADDR` 注释），新增固定地址必须对照。
  同时修掉一个相关笔误：集群缓冲页数应按 `簇字节数 / 页大小` 算，而不是 `1 << spc_shift`（那是扇区数）。

安全护栏（接真盘必需）：
- **MFS 不再无脑格式化**：`mfs_srv` 认领卷后先查卷表，只有「已是 MFS」或「整盘无文件系统
  (UNKNOWN)」才允许挂载/格式化；是 FAT/exFAT/ext2 等**别人的分区就拒绝**并打印
  `mfs: refuse to format non-MFS volume`。此前卷号回退一旦算错，自动格式化会把用户分区直接写掉。

**验证（已通过）**：
- 两种 exFAT 镜像各跑一轮完整实机回归，`FAILED/PANIC = 0` 且 `app: SELFTEST DONE`：
  - 16 MiB / **4 KiB 簇**（`bitmap=448`）—— 覆盖原有小簇路径（单页、无 PRP 表）；
  - 2048 MiB / **32 KiB 簇**（`cluster=32768 clusters=65472 bitmap=8184`）—— 覆盖 **8 页传输
    （走 PRP 表）**、**位图 8184 字节 > 旧 4096 上限**、按需 upcase；
    同一镜像连续启动两次（第二次复用已删除条目/已扩容目录）均通过。
- 两轮结束后宿主 `fsck.exfat -n` 都是 `clean. directories 1, files 0`（写盘一致性由 exfatprogs 独立判定）。
- `make clippy` exit 0（三 crate 0 warning）。
- ⏱️ **整套 FS 自测约需 4~4.5 分钟**（约 2 万个块请求；IPC 一跳要等下一个时钟 tick，故有效吞吐 ~100 请求/s）。
  判定必须等到 `app: SELFTEST DONE`；挂载行之后到该行之间会长时间无输出，**很容易被误判成卡死**
  （本次 M6c 收尾就在这上面绕了弯路：用 150~200 s 超时观察，误以为死锁并做了一轮内核级排查）。
- ⏱️ 耗时**几乎全在 FS-12**（MFS 目录与长名，200 项 + 中途 3 遍显式全卷 GC），空白卷上 ~170 s；
  其余用例都在 40 s 以内。且 **MFS 卷是持久的**：FS-5 / FS-10 每轮各留一个快照（环形上限 8），
  约 4 轮饱和 —— 快照钉住历史块、`mfs_gc` 又要按「当前表 + 每个快照」逐个重走，故**同一个二进制
  越跑越慢**（实测空白卷 258 s → 饱和后 524 s，FS-12 由 171 s 涨到 274 s）。
  `scripts/fs-regress.sh` 因此默认重置 `build/mfs.img`（`MFS_KEEP=1` 可保留）。

**验证期间顺带修掉的真 bug（用户态栈太小）**：
- 全部域共用同一份用户程序、同一处用户栈。栈原来只有 **1 页 (4 KiB)**，而 VFS 请求/回复要在栈上
  构造 `Message`（96 B payload）并层层调用 —— 用内核缺页日志实测，**app 域在最早的几次 VFS 调用
  就越过栈底**（`cr2 = USER_BASE + 0x3F_F780`，即栈底之下 ~2 KiB），当时靠按需分页
  （pager 对任何用户缺页都补一个匿名零页）把缺的页静默补上，属于「碰巧能用」。
- 现改为 **8 页 (32 KiB)**（`USER_STACK_PAGES`，栈顶不变），修复后 app 域不再产生任何缺页。
  这类「靠 pager 兜底」的栈在代码/时序变化时会表现为难查的随机故障，故一并修掉。

#### M6c 未覆盖

- **扇区**：仍只支持 512 B 扇区（`BytesPerSectorShift == 9`）；4Kn 盘需另一套换算。
- **簇**：`spc_shift ≤ 9`（256 KiB 簇）；更大簇会因集群缓冲页数上限被拒绝（`stage=13`）。
- **真实 U 盘端到端**：`/dev/sdX` 属 `root:disk`，非 root 无法直通给 QEMU；且现有卷层是
  「按类型认领第一个匹配卷」，要让真盘出现在 shell 里还需 **M1b**（额外卷挂到 `/usb<N>`）。
- fat32_srv 仍按「整簇读进单页缓冲」工作，**大簇 FAT32 分区（如 32 KiB 簇的 53.6 GiB U 盘）
  尚不能读**；块层已就绪，等 fat32 侧改造。

#### M7 已完成 ✅

**动机**：要让 MorionFS 从「挂在 `/mfs` 的第二棵树」变成**主力文件系统**，先得让它能
真正用满一块盘。此前有两个把 MFS 限制成玩具的硬伤：

- **格式化尺寸写死**：`mfs_format` 里 `MFS_TOTAL_BLOCKS = MFS_DEFAULT_TOTAL_BLOCKS`
  （4096 块 = 16 MiB）—— **不看卷有多大**。整块 2 TiB 新盘接上，也会被格成 16 MiB。
- **整盘卷容量未知**：卷层对「无分区表的整盘」记 `sectors = 0`，于是连「该格多大」都无从算起。

实现要点：

- **block_srv 补 NSZE**：`Volume`/`VolumeDesc.sectors` 的含义从「整盘其余部分，未知」改为
  **真实容量**。做法是初始化阶段对每个 namespace 发一次 `Identify Namespace`(CNS=0)，取返回
  数据偏移 0 的 `NSZE`（u64，512 B 逻辑块下即扇区数），缓存在 `NVME_NS_SECTORS`。
  走 **Admin 队列**（只在 init 发一次，不占 I/O 队列）；顺带删掉了原先那条「发 NSID=1 的
  Identify 却从不读结果」的死代码，改成按 namespace 列表逐个查询。
  `sectors == 0` 仍保留「未知」语义（仅当连盘也问不出容量时），需要容量的上层必须
  按默认值兜底，**不能把 0 当成零长度卷**。IDE PIO 回退路径后来也补上了容量探测
  （ATA IDENTIFY DEVICE，见「M7 未覆盖」末条 → 已补）。
- **mfs_srv 按几何格式化**：`mfs_format_total_blocks()` = `卷容量 / 8 扇区每块`，夹在
  `[MFS_MIN_TOTAL_BLOCKS = 64, MFS_MAX_BLOCKS]` 之间；容量未知时退回 `MFS_DEFAULT_TOTAL_BLOCKS`。
  新增下限是因为卷再小也得放得下两份超级块 + 根目录 + inode 表。
- **挂载时校验容量**：超级块记录的总块数若超过该卷实际容量（换过镜像 / 卷号认领错 / 卷被
  缩小过），该副本直接判为不可用 → 两份都不可用就走格式化。此前只有「不超过
  `MFS_MAX_BLOCKS`」这一条上界检查，盘上记着比卷还大的尺寸时会一路读到盘外。
- 顺带把卷表查询收敛成一个 `vol_find_desc(scratch, vol) -> Option<VolumeDesc>`，
  `vol_kind_of` / `vol_sectors` 都基于它 —— 原来每加一个字段就要抄一遍扫描循环。
- `mfs-dbg` 增 `volsec=`（卷容量扇区数）：`total × 8 == volsec` 一眼可验「文件系统铺满卷」。

**验证（已通过）**：
- **FS-21 自测**：断言 `MFS 总块数 × 8 == 它所在卷的 sectors`。一条关系式同时锁两件事 ——
  NSZE 真的填进了卷表（整盘卷此前恒为 0），以及格式化确实按几何取尺寸。若把
  `mfs_format` 改回写死 4096 块，在 64 MiB 测试卷上这条立刻失败。
- 测试卷默认由 16 MiB 提到 **64 MiB**（`MFS_MIB ?= 64`）：卷比旧的写死值大，「按几何定尺寸」
  这条路径才会被真正走到（16 MiB 卷上新旧行为恰好相同，测不出差别）。
  实测 `mfs-dbg: vol=1 total=16384 free=16378 gen=3 snap=0 volsec=131072` —— 16384 块 × 4 KiB
  = 64 MiB = 131072 扇区。
- 全量回归 `SELFTEST DONE 1 / FAILED+PANIC 0`（覆盖 FS-1..FS-21）；`make clippy` 三 crate 0 warning。

**M7 未覆盖**：
- **总量上限仍是 ≈119 MiB**：空闲位图内联在超级块 payload 里（3832 字节 → 30656 块），
  超过就被 `clamp` 截断 —— 而校验是等号，所以测试卷一旦超过 119 MiB，FS-21 会**刻意失败**，
  提醒该做下面这件事。要放开必须先**把位图挪出超级块**（独立位图块 + 按窗口读写），
  并由 GC 分块多趟扫描（mark/seen 两张全盘位图现在常驻内存，1 TB 卷光位图就几十 MB）。
  **→ 已由 S3a 补上**：位图外置到独立数据块 + `MFBH` 头块，mark/seen 改为动态页窗口，
  上限提到 ≈127.25 GiB（见「S3a 已完成」）。
- **MFS 额外卷与显式格式化已由 M8 补上**（见下）；M7 遗留的只有「容量」这一条。
- IDE PIO 回退路径没有容量信息（`sectors = 0`），MFS 在它上面只能用默认尺寸。
  **→ 已补**：block_srv 的 IDE 路径改从 **ATA IDENTIFY DEVICE**（`0xEC`）现问现取容量 ——
  优先 LBA48（word 100-103，需 word 83 bit10），否则 LBA28（word 60-61），并夹在 **28 位 LBA
  上限**（`0x0FFF_FFFF` 扇区 = 128 GiB）内（本驱动只发 28 位 LBA，报更大容量会让上层往读不到
  的区域写）；问不出来（无盘 / ABRT / 超时）才退回 `sectors = 0`。实测 `make run-ide` 的
  1024 MiB 盘由 `vol: 0 … sectors=0` 变成 `sectors=2097152`(= 1024 MiB)。

#### M8 已完成 ✅

**目标**：让 MorionFS 能当**主力文件系统**用 —— 真盘上「新买一块盘 → 格式化 → 立刻用」这条路
要能走通，同时**绝不能**碰到盘上别人的分区。

实现要点：
- **显式格式化入口**：新增 VFS tag `MKFS`（`vfs::mfs_mkfs(vol)`，payload 就是卷号）。
  它**按卷号寻址**（还没有文件系统时没有路径可走），故不经挂载层路由，直接发给 mfs_srv。
- **护栏**（`mfs_mkfs_volume`）：先 `vol_find_desc` 取卷描述符，**只接受 `VOL_KIND_MFS`
  （重新格式化）或 `VOL_KIND_UNKNOWN`（未格式化）**；FAT / exFAT / ext2 分区与不存在的卷号
  一律拒绝并打印 `mfs: mkfs refused (...)`。判定只有一处，shell 命令与 FS-22 自测走同一条路径。
- **`mfs_load_state` 从 `mfs_mount_or_format` 拆出**：前者**只载入不格式化**，后者 = 载得动就载、
  载不动才格式化。切卷路径必须用前者 —— 用后者的话，一块暂时读不出来的盘会被直接抹掉。
- **按请求切卷**：MFS 的内存态（`MFS_BITMAP` / `MFS_ITAB_MEM` / 快照表 / 各游标）只有**一份**，
  对应**一个**卷，所以它不能像 fat32/ext2/exFAT 那样靠「切卷重解析几何」共享状态 —— 换卷就是
  换整套状态。新增 `MFS_CUR_VOL` / `MFS_CUR_SECTORS`：请求 tag 高位的卷编码（fd 类请求则由
  新增的 `MfsFd.vol` 决定，因为 `close`/`read` 这类请求不带路径）与当前卷不同时，
  `mfs_switch_vol` 设好卷号与容量再 `mfs_load_state()` 把新卷的超级块（含空闲位图）载回内存。
  **能这样切的前提是每次改动都随超级块落盘**（`mfs_itab_set` → `mfs_itab_flush` →
  `mfs_write_super`），故请求边界上盘上状态总是自洽的 —— 这与 fat32 切卷要重读 BPB 是同一类
  论证，只是 MFS 要重读的东西多得多。（MFS7 起提交出口改名 `mfs_bmp_flush`，位图内存态由
  定长数组改为动态页窗口，见「S3a 已完成」；切卷需重建的东西相应变多。）
- **MFS 参与额外卷挂载**：`mount_extra_volumes(..., VOL_KIND_MFS, ...)`，非主卷挂到 `/usb<卷号>`。
  注意只挂卷层**已探测为 MFS** 的额外卷：空白额外卷**不会**被自动格式化（要显式 `mkfs.mfs`）——
  这正是「自动格式化」与「多卷」两者此前冲突的地方，现在由「显式入口」解开。
- **内核补授权**：mfs_srv 此前没有 `Capability::SendTo(mount_srv)`（不参与额外卷就不需要）。
  缺这条时 `ipc::call` 被**静默拒绝**（返回 `u64::MAX`、不报错），额外卷会挂不上且无任何日志。
- **卷表打印**（block_srv）：启动时逐卷打印 `vol: <卷号> nsid=… lba=… sectors=… kind=…`。
  卷号由扫描顺序决定，不打出来 `mkfs.mfs <卷号>` 就只能靠猜 —— 这是让命令可用的前提。
- **shell 命令**：`mkfs.mfs <卷号>`（护栏在服务端，shell 只做参数解析与结果打印）。
- **测试盘**：Makefile 新增 `build/spare.img`（16 MiB 纯零，**不含任何文件系统**）作 nsid 6，
  正是真盘上「新买一块盘」的样子；`scripts/fs-regress.sh` 每轮把它重置为空白。

**验证（已通过）**：
- **FS-22 自测**（四段）：① 对 FAT 卷 / ext2 卷 / 不存在的卷号调 `mfs_mkfs` 必须被拒；
  ② 空白盘格式化成功并作为额外卷挂到 `/usb6`，能在上面建文件并读回；
  ③ 格式化**别的**卷之后主卷 `/mfs` 的标记文件内容仍完好 —— 证明内存态被正确重建回主卷
  （若忘了重建，后续会用新卷的位图去写主卷，很快毁数据）；④ 在额外卷与主卷间交替读写，
  两边内容不串 —— 证明按请求切卷生效。
- 实测日志：`vol: 6 nsid=6 lba=0 sectors=32768 kind=unknown` → `mfs: mkfs refused (...)`
  ×3（护栏）→ `mount-dbg: /usb6 domain=11 slot=8`（格式化后自动挂上）。
- 全量回归 `SELFTEST DONE 1 / FAILED+PANIC 0`（覆盖 FS-1..FS-22），**268 s** —— 与 M7 时的
  268 s 完全一致，说明切卷/多卷没有引入额外开销；`make clippy` 三 crate 0 warning。

**M8 未覆盖**：
- **总量上限仍是 ≈119 MiB**（内联位图，见上）—— 与 M7 同一条遗留，**已由 S3a 解决**（见下）。
- **`mkfs.mfs` 不写分区表**：只能格式化卷层**已经存在**的卷（整盘卷或 MBR/GPT 里已有条目的
  分区）。「在没有分区表的盘上创建新分区」还需要 block_srv 的写分区表路径（删卷同理）。
  **→ 已由「S2 卷管理收口」补上**（block_srv 支持建/删/清空 GPT 与 MBR，shell 有 `part.*`，见下）。
- **没有「切换主卷」**：`vol_claim` 仍是「第一个 MFS 卷」（空白盘回退卷号 1），故重启后 `/mfs`
  指向哪一个 MFS 卷由卷表顺序决定，跟你上一次 `mkfs.mfs` 过谁无关；非主卷要靠 `/usb<卷号>` 访问。
  **→ 已由「S2 补齐」补上**（主卷序号持久化在超级块里，认领时序号最大者胜出，见下）。
- IDE PIO 回退路径下 `sectors = 0`，格式化只能用默认尺寸（与 M7 相同）。
  **→ 已补**（同 M7 末条）：IDE 路径改从 ATA IDENTIFY DEVICE 取容量，夹在 28 位 LBA 上限内；
  只有问不出来时才退回默认尺寸。

#### S3a 已完成 ✅

**目标**：把 MorionFS 的容量上限从内联位图的 ≈119 MiB 提到 ≈127 GiB —— 这是「MFS 当主力
文件系统」的硬前提（否则再大的盘也只能用 119 MiB）。magic 升为 **`MFS7`**（版本字段 = 7），
旧盘首挂自动重新格式化（卷层按 `MFS0..MFS9` 通配，无需改动）。

**盘上布局（MFS7）**：

| 块 | 用途 |
| --- | --- |
| 0 / 1 | 超级块 A / B（payload +256 起**不再内联位图**，该区留空保留） |
| 2 / 3 | **位图头块** A / B，magic `MFBH`（`0x4D46_4248`） |
| 4 … 4+bb | 位图数据副本 A（**裸 4096 字节，无块头**） |
| 4+bb … 4+2bb | 位图数据副本 B |

- `bb = ceil(total_blocks / 32768)`：一个 4 KiB 位图数据块覆盖 32768 块（= 128 MiB）。
- `MFS_DATA_START = 4 + 2*bb`：分配器 / GC / 格式化的起点，**它之前的块一律强制标记占用**。
- 位图头块 payload：`+0 gen(u64) / +8 total_blocks(u32) / +12 data_blocks(u32) / +16 CRC32 数组`
  （容量 `MFS_MAX_BMP_DATA_BLOCKS = (MFS_PAYLOAD-16)/4 = 1018`，每个位图数据块一项）。
- 容量上限 `MFS_MAX_BLOCKS = 1018 × 32768 = 33_357_824` 块 ≈ **127.25 GiB**，由格式化上界
  校验；再大需要给位图本身加一层间接（不在本阶段）。

**提交协议（原子性）**：新函数 `mfs_bmp_flush()` **取代** `mfs_write_super()`，成为唯一提交
出口（调用点：`mfs_itab_flush` / `mfs_gc` / `mfs_format` / 快照创建与回滚）。流程：

1. `gen += 1`，按窗口内容重算全部位图数据块的 CRC32（填常驻 `MFS_BMP_CRC`）；
2. 对 `copy ∈ {0,1}` 依次：写**脏区间**的位图数据块 → 写该副本头块（gen/total/bb + 全量 CRC）
   → 写该副本超级块（带新 gen）；
3. 两份都写完后清 dirty。

脏跟踪：`mfs_bmp_set/clear` 时按 `blk = b / 32768` 置位 `MFS_BMP_DIRTY`，故只写变动过的区间。
崩溃在任一步骤时，另一份仍是**旧代但自洽**的，加载取「gen 高且全部校验通过」者 —— 因此
MFS6 的副本选择状态 `MFS_SB_COPY` 已删除。

**内存态：编译期定长数组 → 动态页窗口**（新容量需要 ≈4.17 MB/窗口，静态放不下）：

| 窗口 | 地址 | 共享给 block_srv |
| --- | --- | --- |
| 主空闲位图 | `0x0000_0080_0100_0000` | **是**（直接作位图数据块的 DMA 缓冲） |
| GC 可达标记 | `0x0000_0080_0140_0000` | 否 |
| GC「本根已访问」 | `0x0000_0080_0180_0000` | 否 |
| 位图头块缓冲 | `0x0000_0080_0016_2000` | 是 |

- 删除了 `MFS_BITMAP` / `MFS_GC_MARK` / `MFS_GC_SEEN` 三个定长数组。
- `mfs_win_ensure(pages)` 按需逐页分配，**只增不缩**（卷变小也不 `sys_unmap`，避免共享页引用
  计数泄漏）；`mfs_main` 在**挂载前**按**卷容量**预算上界铺开（此时盘上 bb 还读不到）。
- 位图数据块直接以窗口页作 `block_read/write_dev` 缓冲（窗口第 k 页 ↔ 该副本第 k 个数据块），
  免拷贝。位/字节数全部动态：字节数 `(total+7)/8`。

**加载与兜底**：选定 SB（magic/version、`total ≤ 卷真实容量`、itab 有效）后，要求对应副本头块
`gen == sb.gen`、`total == sb.total`、`data_blocks == bb`，并**逐块读位图数据块算 CRC32 与头块
数组比对**（结果填入 `MFS_BMP_CRC`）。实现细节：MFS7 一次提交把两份副本写成**同一 gen**，故
选副本的规则从「取 gen 更高者」改为「取 gen 更高者；同代时优先已成功载入位图的那份」—— 否则
先试的副本位图坏了会白白触发重建，即使另一份完好。

两份位图都不可用但 SB + itab 有效 → `mfs_rebuild_bitmap()`：位图先**全置占用**（FREE = 0 的
安全态），再跑 `mfs_gc()` 从可达根重建；**不格式化**。全置占用是安全方向 —— 即使 GC 失败，
最坏是「空间没回收」，绝不会把在用的块发出去。

**验证**：`scripts/fs-regress.sh` 默认卷 `MFS_MIB` 64 → **256 MiB**（→ `total = 65536`、
`bb = 2`，多块位图路径被真正走到）；Makefile 同步。新增 **FS-23(a)** 自测盯几何关系：
`total × 8 == 卷 sectors` 且 `total > 32768`（即 bb ≥ 2）。多块位图的读回 / CRC / 重建由每次
挂载校验与 FS-12 的 3 次全卷 GC 覆盖（GC 会把所有 chunk 置脏并整体落盘），故不真写满 128 MiB
数据（IPC 往返代价不可接受）。

**S3a 未覆盖**（已由 S3b 全部补上，见下）：
- VFS 协议端到端仍不是 u64（offset/size），单文件跨 4 GiB 之外的寻址未打通。
- 只做到二级间接块，没有三级间接。
- 位图数据块数上限 1018 是可达到的硬顶（≈127.25 GiB），再大要给位图加间接层。

#### S3b 已完成 ✅

**目标**：让单文件上限从二级间接的 ≈3.98 GiB 提到与卷容量同量级 —— 这是「MFS 当主力文件系统」
的最后一处硬伤（127 GiB 的卷里放不下一个 >4 GiB 的文件）。magic 升为 **`MFS8`**（版本字段 = 8），
旧盘首挂自动重新格式化。

**改动分三层，缺一不可**（只改协议则 `size` u32 报不出 4 GiB 以上；只加三级则协议装不下偏移）：

1. **VFS 协议端到端 u64**：`ReadReq`/`WriteReq` 的 `offset`、`TruncateReq` 的 `size`、
   `Stat.size` / `DirEntry.size` 全部改 `u64`；客户端 API `read/write/truncate` 的 offset/size
   同步改 u64。`count` 保持 `u32`（单次 I/O 上限一页）。
   **内部仍是 32 位的服务（fat32 / tmpfs / ext2 / exFAT）在协议边界加守卫**：
   `offset > u32::MAX` 直接拒绝，再收窄成内部 `u32` —— 这些格式自身的文件大小字段就是 32 位，
   不做内部重写。只有 MFS 真正走 u64。
2. **文件节点 size → u64**：payload 由 `size u32 | nblocks u32 | direct[1008] …` 改为
   `size u64 | nblocks u32 | pad u32 | direct[1005] | ind1 | ind2 | ind3 | 元数据(40)`。
3. **三级间接块**（新 magic `MFI3`）：块映射分四段（直接 → 一级 → 二级 → 三级），
   `MFS_FILE_MAX_BLOCKS = 1005 + 1022 + 1022² + 1022³ ≈ 1.07e9 块`，远超位图能描述的块数，
   故**单文件上限实际等于整卷可用块数**。

**直接区为何是 1005 而不是 1007**：payload 只有 4088 字节，`size` 由 u32 变 u64 多占 4 字节、
对齐 `pad` 再占 4 字节、新增 `ind3` 再占 4 字节 —— 合计要从直接区缩掉 3 个指针
（1008 → 1005）。这样 `ind1/ind2/ind3` 正好结束于 4056，**`MFS_FILE_RESERVED_OFF` 保持
4056、元数据偏移与布局完全不变**（有编译期断言 `MFS_FILE_RESERVED >= 40` 兜底）。

**写路径**：现有的「活动间接块」缓存（`MfsIndCache`）只记录**一级父级**，承载不了三级。
故 kind 1 / kind 2 维持原缓存路径不变，**kind 3 走新增的无缓存链路**
`mfs_ind_peek3` / `mfs_ind_link3`（逐级读；回写时自下而上「读-改-COW」：
COW ind1 → 写回父 ind2 → COW ind2 → 写回 ind3 → COW ind3 → 写回 inode 指针），
全程只用 B 一页缓冲（每级读完立刻取走槽值再复用）。转发点放在 `mfs_ind_peek` / `mfs_ind_link`
内部（进入 kind 3 前先 `mfs_ind_flush` 并清空缓存），这样 `truncate` 的「末块尾部清零」与
`mfs_unmap_block` 也自动获得三级支持。取舍：三级只服务 >4 GiB 文件，用实现简单换掉一点性能，
常规文件完全不受影响。

**GC**：`mfs_gc_drain` 的文件分支除直接槽 / `MFIN` / `MFI2` 外**必须把 `MFI3` 一并入栈**，
并新增 `MFI3` 分支（其槽位全是二级块）。漏标会让三级块被当作垃圾回收后重新分配出去，
直接损坏 >4 GiB 文件 —— 这是本阶段最容易出错的地方。

**验证**：新增两项自测（`scripts/fs-regress.sh` 全量回归覆盖）：
- **FS-23(b)**：`truncate` 出一个 **5 GiB + 12345 字节的稀疏文件**（稀疏扩展不真写数据），
  `stat` 断言 size 如实报出 u64 值、起始读为空洞全 0；再在 **4 GiB + 4 KiB** 偏移写 16 字节并
  逐字节读回校验 —— 二级区的字节上限约 3.98 GiB（`(1005+1022+1022²) × 4088`），故该偏移必然
  落在**三级区**，一条断言同时覆盖「u64 offset 端到端」与「三级块真被用到」。
- **FS-23(c)**：重新 `open` 后读同偏移内容一致（size/指针已持久化，不依赖内存态）；再调
  `vfs::mfs_gc()` 显式 GC，**GC 后复读仍一致** —— 若 GC 漏标 `MFI3`，三级块会被回收并重新分配，
  这条读取必然失败或读到错内容。

实测：`SELFTEST DONE 1 / FAILED+PANIC 0`，**344 s**（与 S3a 的 338 s 基本持平，说明稀疏大文件
与三级链路没有引入可感开销）。

**S3b 未覆盖**：
- 三级区之外的更大单文件（>≈4.3 TB）需要四级间接，实际无意义（卷上限 ≈127 GiB）。
- fat32 / ext2 / tmpfs / exFAT 的内部大小字段仍是 32 位，>4 GiB 文件只在 MFS 上成立。

#### S2 补齐（主卷切换）已完成 ✅

**目标**：收掉 M8 的最后一条遗留 —— 「重启后 `/mfs` 落在哪块 MFS 卷上」由卷表扫描顺序决定，
显式 `mkfs.mfs` 过谁毫无影响。真盘上插入第二块 MFS 盘之后，这条不确定性会直接决定用户的数据
落在哪，必须可控。

**盘上格式（不升 magic）**：主卷标记放在超级块 payload `+256`（`MFS_SB_PRIMARY`，u64）。
S3a 把位图移出超级块之后这片区域本就整片保留，所以：

- 老 MFS8 卷该处为 0 → 语义是「不是主卷」，**不触发重新格式化**，也不必升 magic/版本；
- 标记随 `mfs_build_super` 每次提交一并回写（提交是「重写整块超级块」，漏写就会把标记清掉）；
- 载入时由 `mfs_load_state` 读回内存态，故一次普通写盘不会丢失标记。

**语义（序号最大者胜出）**：

- `mkfs.mfs <卷号>` 把目标卷的序号置成**现有所有 MFS 卷与它自身的最大序号 + 1**
  （`mfs_next_primary_serial`；直接按卷号读超级块，与已冻结的卷表无关），然后随格式化提交落盘。
- `mfs.primary <卷号>` 走**同一个** `mfs_next_primary_serial` 取号，但**不格式化** —— 只把标记
  写进目标卷的超级块（见下「只改标记入口」）。
- 认领端 `mfs_vol_claim` 取代通用的 `vol_claim`：① 序号最大且 >0 的 MFS 卷 → ② 卷表里第一个
  MFS 卷（老卷/序号全为 0，保持旧行为）→ ③ 回退约定卷号 1（空白盘没有 magic）。
- **只加 1、不回写别的卷**：单主卷由「最大者胜出」保证，不需要在 mkfs 时去改写其它卷的超级块。
- 因此「最近一次显式格式化过的卷」稳定地就是**下次启动**的 `/mfs`；本次运行不换挂载点
  （换挂载点会让所有已打开的路径句柄失效）。自动格式化（首次挂载）**不**认领主卷 ——
  标记只由显式的 `mkfs.mfs` / `mfs.primary` 设置。

**返回值**：`MKFS` 的成功回复由裸 `1` 改为**从盘上回读的主卷序号**（`>0`，失败仍 `u64::MAX`）
—— 回读证明标记确实写进了超级块并能再读出来，而不是「我记得我设过」。shell 与自测据此判定。

**新增 FS-24**：会话内无法重启，故把链路拆成三个可观测环节 —— ① mkfs 回复的序号来自盘上回读；
② 再次格式化同一卷序号严格变大；③ 两次之间对该卷做一次**普通写提交**，序号仍继续变大
（若标记被普通写盘抹成 0，序号会掉回 1 立刻失败）；④ 全程主卷 `/mfs` 照常可读。

实测：`SELFTEST DONE 1 / FAILED+PANIC 0`，**343 s**（与 S3b 的 344 s 一致；多出来的三次
`mkfs.mfs` 都在 16 MiB 空白盘上，代价可忽略 —— 回归日志里 `mount-dbg: /usb6` 由 FS-22 的
1 次变成 4 次，正是 FS-24 那三次格式化的痕迹）。

**只改标记入口（`mfs.primary`，新增 FS-25）**：`mkfs.mfs` 也能换主卷，但它会**擦掉**卷上的
文件 —— 「把一块**已有数据**的盘升为主卷」在 `mkfs` 下等同于删数据。补一条**不动数据**的路：

- 新 VFS 标签 `MFS_SETPRIMARY_TAG` + 封装 `vfs::mfs_set_primary(vol)`；服务端
  `mfs_set_primary_volume(vol)`：先用 `mfs_sb_probe` 确认目标卷**确实是 MFS**（读得出有效超级块），
  再 `mfs_next_primary_serial` 取号，临时把当前卷切到目标卷 → `mfs_load_state` 载入其内存态 →
  置 `MFS_PRIMARY_SERIAL` → `mfs_bmp_flush` 提交 → 切回原卷并重载状态。
- **与 `mkfs` 的分工是硬约束**：目标卷上放着用户的文件，所以这里**没有**「格式化兜底」——
  空白盘 / 别人的分区 / 不存在的卷号一律拒绝。为此把原来的 `mfs_primary_of_vol` 拆出一个
  `mfs_sb_probe(...) -> Option<u64>`：「不是 MFS」（`None`）与「是 MFS 但序号为 0」必须分得开。
- 提交时位图**没有脏块**，故 `mfs_bmp_flush` 实际只写头块 + 超级块两份 —— 正好是「只改标记」
  的最小写集；序号与 `mkfs` **共用**同一个只增计数器，故两者可交叉使用，序号始终单调。
- shell 命令 `mfs.primary <卷号>`，命令表 / `help` 输出 / `mkfs.mfs` 段落的「想切回去」建议
  都已同步（[shell-reference.md](shell-reference.md)）。

**新增 FS-25**：自给自足（不依赖 FS-24 残留）—— ① `mfs_mkfs` 把目标卷置成确定状态并记序号基线；
② 写一个文件（`FS25MARK`）；③ `mfs_set_primary` 后序号**严格大于**基线（证明共用计数器）且文件
**逐字节一致**（证明数据没被动，这正是与 `mkfs` 的本质区别）；④ 对 FAT 卷 / 不存在卷号一律被拒。

实测：`SELFTEST DONE 1 / FAILED+PANIC 0`，**343 s**（FS-25 额外的一次 mkfs + 一次 set-primary
都在 16 MiB 空白盘上，代价可忽略）。

**S2 补齐 未覆盖**：
- 序号是「每次 +1」的计数器，没有时间语义，也没有持久化的「当前主卷」回执查询；跨机器搬盘时
  序号会随之带走（不同机器之间不可比较）。
- 与自动格式化同样的取舍：跑过自测的镜像上 `spare.img` 会被升成主卷，`make run-nvme` 复用镜像
  时 `/mfs` 会落到那块 16 MiB 空白盘上（`fs-regress.sh` 每轮重置两份卷，回归不受影响）。

### S2 卷管理收口 已完成 ✅

**目标**：把「新买一块盘 → 分区 → 格式化 → 用」这条路在**客户机里**走完，不再需要去宿主用
`fdisk`/`sgdisk`。M8 留下的正是这条缺口：`mkfs.mfs` 只能格式化卷层**已经存在**的卷。

**实现要点（block_srv 写分区表）**：

- **opcode 3..7**：`3` 建分区 / `4` 删分区 / `5` 清空分区表 / `6` 重读分区表 / `7` 裸读一扇区。
  一律**按 `nsid` 寻址**（分区表属于整块盘，卷号只是它的产物），请求复用 32 字节 payload ——
  新增 `PartReq { op, nsid, arg0, arg1 }`，与 `BlockReq` 同为 4 × u64、字段一一对应。
- **表风格自适应**：读 LBA 0 判「无表 / MBR / GPT」（判据与卷层解析同一套），已有表就沿用其风格，
  **空白盘默认 GPT**；`flags` bit0 可强制 MBR，但**仅限空白盘**（已有 GPT 时强制转换会毁表，拒绝）。
- **GPT 写全两份**：保护性 MBR(0) + 主头(1) + 主项数组(2..33) + 备份项数组(盘尾 32 扇区) +
  备份头(末扇区)；`first_usable = 34`、`last_usable = 容量 - 34`；头**先清零 CRC 字段再算前 92 字节
  CRC32**；项数组 16 KiB 超一页，故**按页(8 扇区 = 32 项)流式读-改-写**并边读边累加数组 CRC32
  （为此把 `mfs_crc32` 重构成 `!crc32_update(0xFFFF_FFFF, data)`，行为不变）。
- **只动表、不动数据**：删分区只清条目；删到最后一个时整张表清零（盘回「无分区表」），
  于是同一块盘能在 GPT / 空白 / MBR 之间来回折腾。起点对齐 1 MiB，GUID **确定性派生**（可复现）。
- **改动后立即重扫全部 namespace 重建卷表**并打印；`create` 把新分区翻成卷号回给调用方，
  于是「建分区 → `mkfs.mfs <卷号>`」一条龙。
- shell：`part.create <nsid> <MiB> [mbr]` / `part.del <nsid> <index>` / `part.wipe <nsid>` /
  `part.reload`（shell → block_srv 的 `SendTo`，不需要共享页）。

**新增测试盘与 FS-26**：`build/pt.img`（nsid 7，64 MiB 纯空白，与「解析既有表」的 `parts.img`
分工）。FS-26 覆盖：清空 → 强制 MBR 建/删（逐字节校验 LBA 0）→ GPT 建（校验保护性 MBR、头、
**头 CRC32**、**项数组 CRC32**、项里的起止 LBA）→ 已有 GPT 时强制 MBR 被拒 → 新分区上 `mkfs.mfs`
后 `kind` 变 `mfs` → 删分区 → 空白盘上的三条护栏（无表可删 / 超限 / 不存在的盘）。
结束时**留一张 GPT 在盘上**，好让 `fs-regress.sh` 用宿主 `sgdisk -v` **跨实现**校验它 ——
客户机里复算 CRC32 只能证明这张表「自洽」，证不了 GUID 字节序、头字段这些规范细节。

**未覆盖**：
- **IDE PIO 回退路径没有分区写入**：它只有一块整盘、也没有 nsid 概念，这些 opcode 在那边回 0
  （回归走 NVMe）。
- **不做服务化**：分区/卷元数据仍在 block_srv 里（服务化列在阶段 4）。
- **改分区表会重建卷表**：卷号由扫描顺序决定，动靠前的盘会让后面卷号整体后移，而已挂载服务
  仍记着启动时的旧卷号 —— 分区操作应限于最后一类盘，或改完重启（自测只动 nsid 7，故 0..6 稳定）。
- 只有**主分区 / GPT 项**，不做扩展分区（EBR 链）、不做分区属性/名字的完整编辑（名字写死
  `MorionFS`），也不做分区**内容**迁移。

### S3 NVMe 中断化（MSI/MSI-X）已完成 ✅

**目标**：把 NVMe 的完成等待从「轮询 CQ（每轮读一次 CSTS 强制 VM exit）」换成**真正的完成中断**，
并保留轮询作为保底 —— 中断化是优化，不该让盘上的任何一条 I/O 因此卡死。

**实现要点**：

- 内核侧（阶段 39）：`arch/apic.rs` 只做 MSI 必需的最小 LAPIC 支撑 —— 取 `IA32_APIC_BASE`
  基址并确保 EN 置位、`SVR` 软使能、`TPR=0`、`LVT0` 保持「ExtINT + 不屏蔽」（LAPIC 一旦使能，
  8259A 的中断改由 LINT0 透传；KVM 正是据此决定还投不投 PIC 中断，屏蔽掉的话时钟/键盘立刻死），
  `eoi()` 写 `base+0xB0`。`arch/pci.rs` 补能力链表遍历 + MSI-X 定位（`table offset/BIR/size`）、
  关 INTx、置 Enable/清 Function Mask。`arch/idt.rs` 为向量段 `0x50..0x5F` 装处理器。
- **「中断即 IPC」在 MSI 上不适用**：中断与驱动收请求是同一个邮箱，投成 IPC 会被当成块请求消费，
  还会改写内核记录的回复目标，`reply` 会投错域。故 MSI 中断只置一个**待处理位**（`irq::set_pending`），
  驱动用新 syscall `SYS_IRQ_POLL`（非阻塞取位，需 `Capability::Irq(vector)`）主动取。
- **分工**：内核管中断配置（LAPIC + PCI 配置空间 + 向量段），**驱动写 MSI-X 表**（表在 BAR0 内，
  那个 BAR 由固件分配在 4 GiB 以上，内核的地址空间到不了；而它本就按非缓存映射给了驱动）。
  顺序：驱动写表项 0 → `SYS_MSIX_ENABLE` 请内核打开 MSI-X（配置空间写留在内核，且只允许该控制器
  的驱动域调用）→ `SYS_REGISTER_IRQ(vector)` 注册。
- 驱动 `submit_wait` 变为「写门铃 → **先等中断**（设备保证先写 CQE 再发中断）→ 查 CQE」，
  并保留原轮询路径：等不到中断（预算耗尽）就**粘性回退**轮询并打一行原因。CQE 判定与出错打印
  由两条路径共用同一个 `try_complete`，避免「换了路径」顺带换了语义。
- **踩到的坑（值得记）**：`Create I/O CQ` 的 CDW11 只写了 `PC=1`，漏了 **IEN=1** —— 该 CQ 于是
  根本不投中断。轮询路径完全看不出来（CQE 照样写进内存），只有中断路径会一直等不到。
- **踩到的坑（值得记）**：**纯自旋等中断会让宿主停摆**。`syscall`/`sysret` 在 KVM 里不产生 VM exit，
  而 QEMU 的 NVMe 是在主循环里 post CQE 并投中断的 —— 「只等中断、不碰 MMIO」的等待循环会让每条
  命令都拖到下一个时钟 tick（实测 41 条/s，比轮询慢一半，整套自测因此超时）。等待循环里每轮读一次
  CSTS 强制 VM exit 后即与轮询持平（82 条/s）。即：单靠「改成中断」省不掉「踢宿主」，除非内核提供
  让域真正阻塞（vCPU 得以 HLT）的等待原语。

**证据（`bash scripts/fs-regress.sh`）**：内核启动打 `apic: enabled …`、`nvme: MSI-X prepared
vector=0x50 …`；驱动打 `MSI-X table[0] programmed` 与内核的 `MSI-X enabled`；运行期打
`nvme: after volume scan cmds=27 irq_cmds=27 poll_cmds=0 irqs=27 mode=irq`，随后每 4096 条命令一行
（实测最后一行 `cmds=28672 irq_cmds=28672 poll_cmds=0 irqs=28672 mode=irq`）——即整轮自测的所有块 I/O
都由中断完成，零回退，`SELFTEST DONE` 1 次、`FAILED/PANIC` 0 次、宿主 `sgdisk` 校验无问题。

回退路径同样单独跑过一轮完整自测：把等待预算临时置 0，驱动在第一条命令上打
`nvme: irq wait exhausted, fallback to polling` 并**粘性**回退，其后每行都是
`cmds=28672 irq_cmds=0 poll_cmds=28672 irqs=0 mode=poll`，自测结论一致（`SELFTEST DONE` 1 次、
`FAILED/PANIC` 0 次）——证明「等不到中断」不会把盘上的 I/O 卡死。

**S3 未覆盖**：

- 只有 **1 个中断向量**（所有队列共用一个）：够用但没做「每队列独立向量 / 多向量分发」。
- LAPIC 只做 MSI 所需的最小集：没有 APIC 定时器、没有 I/O APIC、MSI（非 MSI-X）能力未用、
  没有 `SYS_MSIX_ENABLE` 之外的 PCI 配置空间接口。
- 中断只当「完成通知」，不做完成批量收割（一次中断收一条 CQE）。
- 驱动侧等待仍是「自旋 + 每轮踢一次宿主」，不是阻塞等待（内核没有「中断唤醒阻塞域」的原语），
  所以省不掉 VM exit —— 中断化在当前实现里换的是**语义**（完成由设备通知而非靠读 CQE 猜），
  吞吐与轮询持平而非更高。

### 阶段 4 — 远期

- 卷管理器服务化（把分区/卷元数据从 block_srv 抽出为独立服务）。
- exFAT/NTFS/ISO9660 之外的更多文件系统（读写 ext4、HFS+、UDF）。

---

## 4. 主要风险

1. **MSI/MSI-X 未支持** → 阶段 1 用轮询 CQ，功能优先，性能后补。
   **→ 已由 S3 补上**（LAPIC + MSI-X 中断驱动完成等待，轮询保留为自动回退路径）。
2. **OVMF 退出后 NVMe 状态未知** → 内核自己完整初始化控制器，不依赖固件（与之前 i8042 键盘同理）。
3. **DMA 物理连续性** → NVMe 队列/缓冲必须物理连续且页对齐，帧分配器需支持连续多帧分配。
4. **用户态 DMA 地址翻译** → 驱动域需拿到「物理地址」写进 SQE，须确保映射关系正确（内核提供 vaddr→paddr 解析）。

---

## 5. 相关文档

- 应用开发接口：[app-dev-guide.md](app-dev-guide.md)
- 内核速查：[dev-reference.md](dev-reference.md)
- 总体架构：[architecture.md](architecture.md)（第 45-52 行「文件系统——用户态服务集合」）
