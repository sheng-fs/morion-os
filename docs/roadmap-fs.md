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
      （A/B 交替写 + generation 取新），块 2 起为**只增不回收的 COW 分配区**。
- [x] **写时复制**：叶子（文件/目录）→ 逐级上溯父目录 → 根的 COW 重写；旧块永久保留，从而支撑快照。
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
- [ ] **ext2_srv（只读）**：复用现有 block_srv，解析超级块 / inode / 目录；先把既有 Linux 分区挂进来。
- [x] 挂载服务支持**运行时挂载**（`MNTA`/`MNTD`）与自动分配，而非编译期 `MOUNT_TABLE` ✅ 已完成：
      `MOUNT_MAX = 8` 槽的运行时挂载表；启动时写入引导默认项（`/`、`/tmp`、`/mfs`），
      `MNTA` 可运行时挂载任意文件服务（前缀留空则自动分配最小的空闲 `/mnt<N>`），
      `MNTD` 卸载（根 `/` 不可卸载）。
- [x] libvfs 暴露 `vfs::mount(prefix, domain)` / `vfs::umount(prefix)`；app FS-6 自测覆盖
      「未挂载→不可路由 / 自动挂载→可达 / 卸载→不可路由且不影响原有挂载 / 句柄槽复用 40 轮」。
- [x] 渲染性能：`Framebuffer::pixel` / `fill_rect` 改 32 位单次写（未缓存 MMIO 帧缓冲下相机码流降 ~4x），
      终端打字只重绘底部输入行（`redraw_input_line`），LOGO 批量追加后一次性重绘。

**验证（已通过）**：`make run-nvme` 后 `ls /mfs` → `[FILE] PERSIST.TXT size=6`（内容 `MFS-OK`）；
空白盘首次启动自动格式化成功，二次启动读回旧数据（持久化）；app FS-6（运行时挂载 + 句柄生命周期）通过；
shell 交互 `ls /mfs` / `cat /mfs/PERSIST.TXT` 正常；日志无 `FAILED`/`PANIC`。

### 阶段 4 — 远期

- MSI/MSI-X 中断替代 NVMe 轮询。
- 分区解析（MBR/GPT）+ 卷管理器。

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
