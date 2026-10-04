# ISO9660 只读驱动 + 安装器 设计稿（③ / P8）

> 状态：**P11a（ISO9660 只读驱动）已实现并回归通过**；**P11b（安装器）为待实现设计**。承接既定
> 串行计划 ① 03c xHCI/USB → ② 服务自愈 + 能力审计 → **③ ISO9660 + 安装器**。
> P11a 落地记录见 `docs/roadmap-driver.md` 的「03c 续」小节；回归判据 `ISO1 … read=ok`。
> 相关：`docs/roadmap-driver.md`（驱动/安装盘 V3）、`docs/architecture.md` §安装路径、
> `docs/dev-workflow.md`（加服务 9 处）、`docs/dev-reference.md`（块层 / VFS / §5 syscall 表）。

---

## 1. 目标与非目标

**目标**
1. 内核能**只读**读取 ISO9660（CD/安装盘）卷 —— 作为块层之上的一个文件系统客户端 `iso9660_srv`。
2. 提供一个**安装器**：把安装介质上的系统负载写到一块**空白目标盘**，使其**可独立引导**。
3. 全部验收纳入现有回归（`scripts/fs-regress.sh`），QEMU 内端到端可复现。

**非目标（明确排除 / 顺延）**
- **不实现 ATAPI / SCSI 光驱通路**（真机 CD 硬件）。第一阶段把 `.iso` 当**裸块设备**接进来读
  （见 §6）——这与"CD 驱动"解耦：ISO9660 是**文件系统**，CD 是**传输**。真机光驱（ATAPI PACKET）
  留作后续增量。
- **不实现 ISO9660 写**（CD 天生只读；El Torito 不可原地追加文件）。
- **不做 in-guest FAT32 mkfs**（仓库现无此能力，§2）。安装器用**整块拷 `efiboot.img`**规避（§5）。
- 不做光盘启动链改造：现有 `morion-boot.efi` 已能从 El Torito 启动，不改。

---

## 2. 现状事实（调研，含出处）

| 事实 | 出处 |
|---|---|
| 内核 ELF 是**编译期 `include_bytes!` 进引导器**的，不是运行期从盘读 | `boot/src/main.rs:933`；`Makefile:174-178` |
| 服务 ELF 运行期从 **ESP `\EFI\morion\services\<name>.elf`** 读；引导器先试"自己的卷"，**El Torito 下失败则枚举所有 SimpleFileSystem 卷**、以"能读出 `sender.elf`"判定 | `boot/src/main.rs:997-1075` |
| 服务 ELF 已进 `BOOTINFO` 模块表、**常驻内存**；运行期**没有**再读服务文件的路径 | `boot/src/main.rs:1082-1120,1238-1253` |
| 块层后端 `BACKEND_*`：NVME=0 / AHCI=1 / IDE=2 / USB=3；`Volume{nsid,start_lba,sectors,kind,backend}` | `user/srv/src/block_srv.rs:1997-2011` |
| 卷类型探测 `vol_detect_kind` **只认 exFAT/MFS/ext2/FAT**；ISO9660 → `VOL_KIND_UNKNOWN` | `block_srv.rs:2149-2207`；`user/srv/src/common.rs:478-482` |
| 分区写原语齐全：`PART_CREATE=3 / DELETE=4 / WIPE=5 / RELOAD=6 / DISK_READ=7`（GPT/MBR） | `common.rs:485-517`；`block_srv.rs:1674-1715,3155-3318` |
| VFS 服务骨架：`vol_claim` → 挂载 → `loop{sys_recv_msg; match vfs::tag_body(msg.tag)}`；tag 在 `libmorion/src/vfs.rs` | `fat32_srv.rs:1300-1471`；`libmorion/src/vfs.rs:61-372` |
| **in-guest mkfs**：只有 **mfs_srv** 有（自动 + `mkfs.mfs`）；**fat32/ext2/exfat 都没有** | `mfs_srv.rs:2270,2313,5228`；`fat32_srv.rs`/`exfat_srv.rs` 无 mkfs |
| `mount_srv` 默认表：`/`→6、`/tmp`→10、`/mfs`→11、`/ext2`→12、`/usb`→13；可运行期 `MNTA` 挂载 | `mount_srv.rs:209-215` |
| **无任何 ATAPI/SCSI 光驱代码**；`ahci_srv` 明确**跳过 ATAPI**（`PxSIG==0xEB140101`） | `ahci_srv.rs:61-62,123-137`；`kernel/src/arch/pci.rs` 无 `find_cdrom` |
| `BOOT_DOMAINS = 20`（域 0–19 用满）；`SERVICE_FILES` 长度 20 | `kernel/src/domain.rs:43`；`boot/src/main.rs:973-994` |
| 回归 QEMU：`-cdrom` 启动 + NVMe `nsid=1..7` + vblk/ahci/usb；文件注入用 mtools | `scripts/fs-regress.sh:106-126,53-70` |

**结论（两条关键判断）**
1. **ISO9660 与"CD 硬件"可解耦**：把 `.iso` 当裸块设备（`virtio-blk` 或 `nvme-ns`）接进块层，
   文件系统驱动只面对"线性扇区流"——**第一阶段无需任何 ATAPI**。
2. **安装器无需写 FAT32 格式化器**：ISO 根自带 **`efiboot.img`**（一个**已格式化的 64 MiB FAT32 ESP**，
   内含 `EFI/BOOT/BOOTX64.EFI`（**已内嵌内核**）+ `EFI/morion/services/*.elf`）。
   把它**整块裸拷**到目标盘的 ESP 分区，目标盘即**可引导**（见 §5）。

---

## 3. 关键决策

| # | 问题 | 候选 | 决定 |
|---|---|---|---|
| I1 | ISO 介质如何进系统 | (a) 写 ATAPI/SCSI CD 驱动；(b) **把 `.iso` 当裸块设备** | **(b) 先做**：解耦文件系统与传输；真机光驱（a）顺延 |
| I2 | 卷识别方式 | (a) 按 `vol_detect_kind` 新增签名；(b) 固定回退卷号（学 mfs） | **(a)**：新增 `VOL_KIND_ISO` + PVD 签名探测（更干净，见 §4.1） |
| I3 | ISO 服务挂载点 | `/iso` / `/cdrom` | **`/cdrom`**（语义清晰，安装器按它找负载） |
| I4 | 安装器载体 | (a) 新服务域；(b) **app 自测 + shell `install` 命令**；(c) 暂只要自测 | **(b)**：不新增域、复用 app/shell 既有 `SendTo(block)`；先自测后命令 |
| I5 | 目标盘布局 | (a) 仅 ESP；(b) **ESP + MFS 数据分区** | **先 (a) 最小可引导**；(b) 作为增量（MFS 已有 in-guest mkfs） |
| I6 | ESP 内容来源 | (a) 逐个文件拷；(b) **整块裸拷 `efiboot.img`** | **(b)**：免写 FAT32 mkfs；`efiboot.img` 即"黄金镜像" |

---

## 4. P11a — ISO9660 只读驱动（`iso9660_srv`）

### 4.1 卷识别
- `common.rs` 新增 `VOL_KIND_ISO`（数值接在现有 `VOL_KIND_*` 之后）。
- `block_srv::vol_detect_kind` 增一条：读卷首 **LBA 16**（绝对偏移 `0x8000`），若
  `bytes[1..6] == b"CD001"` → `VOL_KIND_ISO`（ISO9660 主卷描述符 PVD 的固定位置与标识）。
- 附注：`.iso` 接成**整盘**卷（无分区表），`start_lba = 0`，PVD 在 LBA 16。

### 4.2 服务骨架（仿 `fat32_srv`）
- `user/srv/src/iso9660_srv.rs`：`vol_claim(scratch, VOL_KIND_ISO, fallback)` → 读 LBA 16 PVD →
  解析 → `loop { sys_recv_msg; match vfs::tag_body(msg.tag) { VFS_* } }`。
- **只读**：实现 `OPEN` / `RDIR` / `READ` / `CLOSE` / `STAT`；写类（`WRIT/CREA/MKDI/UNLK/RMDI/RENM/CHMD/LINK/SYML`）
  一律回复失败（能力上诚实：CD 不可写）。
- 解析要点（ISO9660，均为**小端**字段）：
  - PVD：逻辑块大小（偏移 128）、根目录记录（偏移 156，34 字节）。
  - 目录记录：长度(0)、extent LBA(2，双端序 u32)、数据长度(10，双端序 u32)、flags(25)、
    名字长(32)、名字(33..)。目录 = flag bit1；`;1` 版本后缀需**剥离**、名字**大小写不敏感**比较。
  - 文件读：`extent_lba * blk / 512` 起、按块层扇区读，裁剪到 `data_length`。
- 目录遍历：对"目录 extent"整块读入，按记录长度步进（记录长度 0 = 该扇区目录项结束，跳到下一扇区）。

### 4.3 接线（9 处 + 域）
| # | 位置 | 改动 |
|---|---|---|
| 1 | `user/srv/src/iso9660_srv.rs` | 新建实现 |
| 2 | `user/srv/src/bin/iso9660_srv.rs` | 新建入口（打印 `[up] iso9660_srv (domain 20)`） |
| 3 | `user/srv/Cargo.toml` | `features` +`svc-iso9660_srv`；`[[bin]]` +1 |
| 4 | `user/srv/src/lib.rs` | 模块 `#[cfg(feature="svc-iso9660_srv")]` |
| 5 | `Makefile` `SRV_NAMES` | +`iso9660_srv`（→ ELF 打包，见 6） |
| 6 | `boot/src/main.rs` `SERVICE_FILES` + 长度 | +`(20, "iso9660_srv")`；长度常量 **20 → 21** |
| 7 | `kernel/src/domain.rs` `BOOT_DOMAINS` | **20 → 21**（含文件头注释与单测） |
| 8 | `kernel/src/main.rs` | 建域 20；授权 `iso9660_srv → SendTo(block)` / `iso9660_srv → MapInto(block)`；`block → MapInto(iso9660)`（暂存页回共享） |
| 9 | `user/srv/src/mfs_srv.rs` | `MFS_BOOT_DOMAINS` **20 → 21**（对齐常量） |
| + | `mount_srv.rs:209` `mounts_init` | 加 `/cdrom` → 域 20 |
| + | `kernel/src/arch/pci.rs` | **不需要**（无新设备类） |

### 4.4 验收（P11a）
- 回归 marker：`ISO1 iso9660 OK, volid=MORION_OS, root_entries=N, read=ok`
  （读取 `efiboot.img` 头 16 字节并校验其为 FAT 引导扇区 `0xAA55`，或读 `EFI/BOOT/BOOTX64.EFI`
  头 4 字节 `\x7fELF`——取其一作"真的读出内容"的证据）。
- `scripts/fs-regress.sh` 新增判据 `grep -q 'ISO1 iso9660 OK.*read=ok'`。

---

## 5. P11b — 安装器

### 5.1 核心流程（目标盘 = 空白 raw，如 `build/target.img`）
```
预置：/cdrom 已挂载（P11a）；目标盘（空白）已在卷表里（kind=unknown、整盘）
1. 读 /cdrom 的 EFIBOOT.IMG（ISO9660 名，剥离 ;1、大小写不敏感）到内存缓冲（≤64 MiB）
2. block PART_WIPE  目标盘            # 清残留分区表
3. block PART_CREATE 目标盘: ESP 分区 # GPT，type = EFI System，起始 LBA 对齐 1 MiB，大小 ≥ efiboot.img
4. block WRITE 目标盘@ESP_LBA ← efiboot.img 全部扇区   # 整块裸拷（关键：免 mkfs）
5. block PART_RELOAD 目标盘           # 重扫 → 卷表出现 ESP(FAT32)；读 LBA0 校验 0xAA55
6.（增量 I5b）PART_CREATE MFS 数据分区 → mkfs.mfs →（可选）填充根
```
**为什么可行**：`efiboot.img` 是 `Makefile` 用 `mformat` 造的**标准 FAT32 ESP**，UEFI 默认路径
`\EFI\BOOT\BOOTX64.EFI` 就在里面；该 `BOOTX64.EFI` **已内嵌内核**（`include_bytes!`），服务也在
ESP 上。故目标盘拷完即**可独立引导**，无需在客户机里格式化 FAT32。

### 5.2 载体
- **先自测**（P11b-1）：`app` 自测 `INSTALL1`：对空白 `target.img` 跑 5.1 的 1–5 步，回复并
  重读 ESP LBA0 校验 `0xAA55`，打 `INSTALL1 install OK, esp_sig=ok`。
- **再命令**（P11b-2）：shell `install <盘号>`（提示确认，`INSTALL_MODE` 变体下可放开）——
  用户可见的装机入口。
- 不新增服务域（I4）：`app`/`shell` 已持 `SendTo(block)` + `MapInto(block)`；安装器对块层的写
  全走现有 `BLOCK_OP_*` + `PartReq`。

### 5.3 验收（P11b）
- 客户机 marker：`INSTALL1 install OK, esp_sig=ok`。
- **宿主侧独立取证**（不采信自证）：`sgdisk -v build/target.img` → `No problems found`；
  `mdir -i <ESP 分区偏移> ::/EFI/BOOT/` 能列出 `BOOTX64.EFI`（或对 ESP 分区 `fsck.fat -n`）。
  ⚠️ 目标盘在**回归每轮重置为空白**，故自测后可稳定做宿主校验。

---

## 6. QEMU 测试装置

- **接 ISO 当块设备**（P11a 起）：新增空白 `build/iso.img`（= `morion-os.iso` 的**只读副本**，
  避免与 `-cdrom` 抢同一文件写锁），接成 **`nvme-ns nsid=8`**（复用 block_srv 的 NVMe nsid 扫描，
  **零内核改动**）：
  ```
  -drive file=build/iso.img,if=none,id=n8,format=raw,readonly=on -device nvme-ns,drive=n8,bus=nvme0,nsid=8
  ```
- **接目标盘**（P11b 起）：新增空白 `build/target.img`（如 64 MiB），接成 **`nvme-ns nsid=9`**：
  ```
  dd if=/dev/zero of=build/target.img bs=1M count=64 status=none
  -drive file=build/target.img,if=none,id=n9,format=raw -device nvme-ns,drive=n9,bus=nvme0,nsid=9
  ```
- Makefile：新增 `ISO_IMG` / `TARGET_IMG` 目标；`run-nvme` 依赖追加；`fs-regress.sh` 每轮重置
  `target.img`（与 `spare/pt/vblk/ahci` 同规）；`iso.img` 由 `morion-os.iso` 复制生成（每轮或
  按需）。

---

## 7. 分阶段任务与验收

| 阶段 | 内容 | 验收 |
|---|---|---|
| **P11a** | `iso9660_srv`（域 20）+ `VOL_KIND_ISO` 探测 + `/cdrom` 挂载 + nsid=8 装置 | `ISO1 … read=ok`；四道门禁 + 全量回归绿 |
| **P11b-1** | 安装器自测 `INSTALL1`（wipe→partition→裸拷 efiboot.img）| `INSTALL1 … esp_sig=ok`；宿主 `sgdisk -v` 通过 |
| **P11b-2** | shell `install <盘号>` 命令（用户可见入口） | probe 注入 `install 9` → 目标盘可引导（可选：QEMU 从 target.img 起） |
| **I5b（增量）** | ESP + MFS 数据分区（`mkfs.mfs` 复用） | 目标盘 `/` 落在 MFS，回归另加判据 |

**回归口径**：沿用四道门禁（fmt/check/clippy 0 warning、`cargo test --lib`、镜像构建、
`fs-regress.sh` 退出码 0）；新增 `ISO1` / `INSTALL1` 判据；驱动/IOMMU 相关额外跑 `IOMMU=1`。

---

## 8. 风险与未决

1. **真机光驱（ATAPI）**：第一阶段用裸块设备读 ISO，**真机 CD/DVD 引导读盘**仍需 ATAPI PACKET
   —— 明确列为后续增量（复用 `ahci_srv` 的 H2D FIS 框架，加 `0xA0 PACKET` + `READ(10)`）。
2. **`efiboot.img` 大小与目标 ESP 容量**：ESP 分区须 ≥ `efiboot.img`（现 64 MiB）。安装器需按
   实际大小分配；GPT 分区起始对齐 1 MiB、ESP 类型 GUID 正确。
3. **卷号/装置稳定性**：新增 nsid=8/9 后，NVMe 扫描顺序改变，**勿把卷号写死进其它服务的判据**
   （沿用既有"`vol_claim` 按 kind 认领 + 回退卷号"约定）。回归判据只认 marker、不认绝对卷号。
4. **`iso.img` 副本的必要性**：若 QEMU 允许同一 ISO 同时作 `-cdrom` 与 `readonly` 块设备，可省；
   否则用副本（默认按副本设计，最稳）。
5. **未决**：I4 安装器最终形态（是否升级为独立 `installer_srv`）、I5 是否一步到"ESP+MFS 根"、
   是否支持从**运行系统内存镜像**（而非 ISO）取负载 —— 留待 P11 开工前按实际工作量定。
