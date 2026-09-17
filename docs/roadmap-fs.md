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
| 中断 | 仅 PIC；NVMe 需 MSI/MSI-X | **先轮询 CQ 完成队列**，MSI/MSI-X 后置 |

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

- [x] **MFS（原创文件系统）** ✅ 已完成：块设备后端（NVMe 第二 namespace → 独立 `build/mfs.img`，16 MiB，
      空白盘首次挂载自动格式化）。
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
| M5c | 软链接 | 新节点类型 `MFSL`；路径解析跟随目标（含相对路径）+ 限深防环 | M5b |
| M6a ✅ | exFAT 只读兼容 | 新文件服务域 `exfat_srv`（域 13）：引导区 + boot checksum、FAT 链、entry set 解析、分配位图、upcase 表；认领既有 exFAT 卷挂到 `/usb` | M1 |
| M6b ✅ | exFAT 读写 | 位图分配/释放、FAT 链扩展、entry set 增删 + set checksum / NameHash 生成；`CREAT/WRITE/MKDIR/UNLINK/RMDIR/TRUNCATE` | M6a |
| M6c ✅ | 大容量卷（块层多页 DMA + exFAT 去上限） | 块层 PRP 表（单命令 ≤ 128 KiB，更大的请求自动切分）；exFAT 去掉 4 KiB 簇 / 4 KiB 位图 / 8 KiB upcase 三处硬上限（集群缓冲按簇大小分配、位图与 upcase 改为按需扇区窗口）；另加 MFS「非 MFS 卷拒绝格式化」护栏 | M6b |

#### M1 已完成 ✅

实现要点：
- block_srv 新增**卷层**（[user/src/main.rs](../user/src/main.rs)）：启动时逐 namespace 解析
  MBR/GPT 分区表（无分区表则整盘一个卷），按卷首签名探测类型（exFAT / MFS `MFS1..MFS5` / ext2 / FAT），
  把 `dev` 从「namespace 号」改为「**卷号**」，实际 I/O 用 `(vol.nsid, vol.start_lba + lba)`。
- 新增 opcode `2 = 查询卷表`（把 `VolumeDesc` 数组写进调用方共享页，回复卷数）。
- namespace 列表由 **Identify Controller 的 `NN`（偏移 516）** 推导为 `1..=NN`。
- fat32/mfs/ext2 启动时**认领主卷**（fat32 → 第一个 FAT 卷；ext2 → 第一个 ext2 卷；
  mfs → 第一个 MFS 卷，空白盘回退约定卷号 1）。
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
- MFS 不参与额外卷挂载（MFS 是自有格式，暂不把第二个 MFS 卷挂到 `/usb<卷号>`）。

#### M2 已完成 ✅

格式定稿（magic 由 `MFS1` 升为 **`MFS2`**，版本字段 = 2；超级块 payload 布局见下）：

| payload 偏移 | 字段 |
| --- | --- |
| +0 / +4 | `version` / `block_size` |
| +8 / +12 / +16 | `total_blocks` / `root` / `alloc_hint`（分配游标，原 `alloc_next`） |
| +20 / +24 | `snap_count` / `gen`（u64） |
| +32 + i×16 | 快照表（8 条：`gen u64` + `root u32` + `alloc_next u32`） |
| +256 … +4088 | **空闲位图**（1 = 占用；3832 字节 → 最大 30656 块 ≈ 119 MiB） |

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
| +4032 / +4036 | **一级间接指针** / **二级间接指针** |
| +4040 … +4088 | 保留 40 字节（预留给 M5 的元数据：时间戳 / 权限 / 链接数） |

实现要点：
- 逻辑块 `bi` 三段映射：直接区（1008）→ 一级间接区（1022）→ 二级间接区（1022 × 1022）；
  合计 `MFS_FILE_MAX_BLOCKS ≈ 1.05M` 块（≈4 GiB），**实际单文件上限 = 整卷可用块数**
  （受空闲位图容量限制 ≈119 MiB）。
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
- ⏱️ **整套 FS 自测约需 3.5~4 分钟**（约 2 万个块请求；IPC 一跳要等下一个时钟 tick，故有效吞吐 ~100 请求/s）。
  判定必须等到 `app: SELFTEST DONE`；挂载行之后到该行之间会长时间无输出，**很容易被误判成卡死**
  （本次 M6c 收尾就在这上面绕了弯路：用 150~200 s 超时观察，误以为死锁并做了一轮内核级排查）。

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

### 阶段 4 — 远期

- MSI/MSI-X 中断替代 NVMe 轮询。
- 卷管理器服务化（把分区/卷元数据从 block_srv 抽出为独立服务）。
- exFAT/NTFS/ISO9660 之外的更多文件系统（读写 ext4、HFS+、UDF）。

---

## 4. 主要风险

1. **MSI/MSI-X 未支持** → 阶段 1 用轮询 CQ，功能优先，性能后补。
2. **OVMF 退出后 NVMe 状态未知** → 内核自己完整初始化控制器，不依赖固件（与之前 i8042 键盘同理）。
3. **DMA 物理连续性** → NVMe 队列/缓冲必须物理连续且页对齐，帧分配器需支持连续多帧分配。
4. **用户态 DMA 地址翻译** → 驱动域需拿到「物理地址」写进 SQE，须确保映射关系正确（内核提供 vaddr→paddr 解析）。

---

## 5. 相关文档

- 应用开发接口：[app-dev-guide.md](app-dev-guide.md)
- 内核速查：[dev-reference.md](dev-reference.md)
- 总体架构：[architecture.md](architecture.md)（第 45-52 行「文件系统——用户态服务集合」）
