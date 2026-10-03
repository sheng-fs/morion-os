# 文件系统下一轮：四条并行流（规划 / 任务书）

> 环境准备、回归门禁与并行协作约定见 [dev-workflow.md](dev-workflow.md)。
> 本文是**规划与验收口径**（入库）；每条流的字段都以「可被另一个会话照着做完」为准。
> 行号基于 2026-10-02 的工作区状态，**会随改动漂移**，只作锚点用。

---

## 1. 现状快照（为什么"还差这些"）

路线图 [roadmap-fs.md](roadmap-fs.md) 里 M1–M8、S2 补齐、S3a/S3b、S6 **全部已完成 ✅**：

- 五个文件服务：`fat32_srv`(rw) / `exfat_srv`(rw) / `ext2_srv`(**只读**) / `tmpfs_srv` / `mfs_srv`（自研 MorionFS，MFS8）。
- MFS 能力面：读写、COW、空闲位图外置（上限 ≈127.25 GiB）、三级间接（单文件 >4 GiB）、目录溢出块 + 长名（≤255 B）、时间戳/权限/owner、`rename`/`truncate`、硬链接、软链接、快照（环 8）+ 回滚 + GC、`mkfs.mfs`/`mfs.primary`/`mfs.snap…` shell 命令。
- 多卷：MBR/GPT 分区解析 + 卷表 + 额外卷自动挂 `/usb<卷号>`；`part.*` 建/删/清分区表。
- 真盘：2026-10-02 用 238.5G thinkplus 跑通「擦除重建 → mkfs → 写文件 → 重启读回」，宿主侧解出 MFS8 超级块、卷容量被 clamp 到 130304 MiB(127.25 GiB)。

**所以缺口不在计划表里**，而在计划没覆盖的四类（下面每条流对应一类）。

---

## 2. 四条流总览

| # | 流 | 一句话目标 | 产出 | 合并顺序 |
|---|---|---|---|---|
| 01 | 健壮性收口 | 把"未知/新版 magic 自动重格"改成拒绝挂载；补最小对账（fsck）与显式 sync | `mfs_srv` + 新 tag + shell 命令 + FS-30..33 | 第 1 |
| 02 | 块层性能 | 块服务内加只读缓存 + 顺序预读（对上层**透明**） | `block_srv` + 命中率计数 | 第 2 |
| 03 | 真机存储驱动 | 新增 `ahci_srv`（SATA/AHCI，含自测），让真机不再只有 NVMe | 新服务域 + 内核 PCI/声明接线 | 第 3 |
| 04 | 权限与多用户 | 本轮只交**设计 + 接口草案**（实现依赖 01 的 `mfs_srv` 地盘） | `docs/design-permissions.md` | 第 4 |

**为什么 04 只做设计**：权限强制必须落在文件服务内部（`mfs_srv` 的每个 open/read/write 前），而 `mfs_srv.rs` 在本轮**归 01 独占**。并行动手、按编号合并，是这份规划的核心取舍（见 §4）。

**后续第二轮（不属于本轮，写在这里防丢）**：

- 02b：目录索引（把 `mfs_dir_scan` 的线性扫描换掉）+ 请求批量化（把"每个操作一次 IPC 往返"降下来）—— 要动 `mfs_srv.rs`，等 01 合入。
  （**目录索引部分已完成**，见 [roadmap-fs.md](roadmap-fs.md) 阶段 02 的「02b 实现」；请求批量化仍待。）
- 04b：权限强制 + uid/gid + 认证服务对接 —— 同样等 01 合入。
- 03b：把 AHCI 盘接进块服务卷层（`block_srv.rs` 归 02）。（**已完成**，见本文件 §03 的「03b」小节。）
- 03c：xHCI/USB 存储（真机 U 盘启动的真正前置）。

---

## 3. 逐条任务书

### 01 — MFS 健壮性收口 ✅ 已完成

**缺口证据（当前代码）**

| 问题 | 位置 |
|---|---|
| 「旧修订/未知 magic 一律视为无效 → 挂载时自动重新格式化」 | `user/srv/src/mfs_srv.rs:63-71`（注释）、`mfs_ok()` :504-511、`mfs_load_state()` :1512-1613、**触发点 `mfs_mount_or_format()` :1726-1736 → `mfs_format()` :1754** |
| 卷层按 `MFS0..MFS9` 通配，任何 MFS 系 magic 都算"我们的卷" | `user/srv/src/block_srv.rs:1542-1567`（判据在 :1553-1557） |
| 无 fsck：两个"已分配但不可达"的泄漏窗口 | 创建 `mfs_create` :4160→:4164（登记 inode）→:4169（插目录项）；删除 `mfs_remove` :4354/:4360（摘条目）→:4383（释放 ino）。中间掉电 = inode（+块）泄漏 |
| 无显式持久化原语 | 唯一提交出口 `mfs_bmp_flush()` :1456-1505；VFS tag 表里没有 SYNC 类（`user/libmorion/src/vfs.rs:50-103`） |

**目标（三件，缺一不可）**

1. **magic/版本策略改三态**（默认保护数据）：
   - 空白卷（`kind=unknown`）→ 照旧自动格式化（首次挂载路径不变）；
   - magic 属 MFS 系但**不匹配本构建**（更旧或更新）→ **拒绝挂载 + 明确日志**（打印盘上 magic 字节、本构建期望值、处置建议），**一个字节都不写盘**；
   - 非 MFS 卷 → 维持现有拒绝（`run()` :3669-3677）。
   - 旧盘要变成新格式，必须**显式** `mkfs.mfs`（已存在）。
2. **最小 fsck（对账）**：新增 tag（建议 `MFS_FSCK_TAG = "MFSC"`）。
   - 扫 inode 表 + 沿根目录树可达性对账，找出"已分配但不可达"的 inode 槽；
   - 默认**只报不修**（返回 `{可回收 inode 数, 可回收块数}`），`--repair` 才回收（复用 `mfs_gc_load_itab` :838 / `mfs_gc_ino_block` :864 / `mfs_gc` :887 的遍历骨架）；
   - shell 加 `mfs.fsck [--repair]`（结果页走 `SHELL_RESULT_BUF`，见 §4.4 约定）。
3. **显式 sync**：新增 tag（建议 `MFS_SYNC_TAG = "MSYN"`）→ 幂等调一次 `mfs_bmp_flush()`，回复落盘后的 gen；shell 加 `mfs.sync`。用途：给"崩溃一致性"自测一个可断言的落盘点。

**不做（本轮明确排除）**：不改盘上布局、不升 magic（一升 magic 就会把老盘全判成"不匹配"）、不做日志/回放、不做完整 fsck（块级交叉校验、目录树修复留给后续）。

**交付物**：`user/srv/src/mfs_srv.rs`、`user/libmorion/src/vfs.rs`、`user/srv/src/shell.rs`、`user/srv/src/app.rs`（新增 **FS-30/31/32**）、`docs/shell-reference.md`、`docs/roadmap-fs.md`（新增小节）、`README.md`（勾选行）。

**验收**

- 门禁四道全过（见 [dev-workflow.md](dev-workflow.md) §2）；
- 新增自测：FS-30 = 用宿主预置的"旧 magic 卷"（或临时改 magic 的镜像）断言**拒绝挂载且盘未变**（宿主 sha256 比对）；FS-31 = 造一个泄漏（`creat` 后手动丢目录项不可行时，用 `mfs.fsck` 对已知镜像报 0 后再 `--repair` 幂等）；FS-32 = `write → mfs.sync → 重挂载读回`，且 sync 回复的 gen 与重挂载后的 gen 一致；
- 全量回归 `SELFTEST DONE ≥1`、`FAILED/PANIC = 0`。

**风险 / 坑**

- 拒绝挂载会让**开发期的旧镜像**挂不上 —— 回归脚本每轮 dd 重置 `mfs.img`/`spare.img`，不受影响；但手上有旧 MFS 镜像的要先 `mkfs.mfs`。这是有意的取舍，文档里要写清楚。
- `mfs_format` 现在同时承担"空白卷首挂"与"旧版重格"，改三态时必须保证**空白卷那条路一字不变**（FS-21/22/24 都在盯它）。

---

### 02 — 块层性能（只读缓存 + 预读）✅ 已完成

**缺口证据**

| 问题 | 位置 |
|---|---|
| 块服务没有缓存：每次 `BLOCK_OP_READ` 都真下盘 | 分派 `user/srv/src/block_srv.rs:1042-1184`（READ 分支）、请求结构 `user/srv/src/common.rs:76-83`、opcode `common.rs:314-330` |
| 全链路每请求一跳 IPC（≈1 tick = 10 ms），实测 ~100 请求/s | [dev-workflow.md](dev-workflow.md) §1 第 6 条；`docs/roadmap-fs.md:10-12`（FS-12 单例 ~170 s） |
| 上层各自有小缓存，但块层没有共享缓存/预读 | `mfs_srv.rs:1103`（inode 表单块缓存）、`exfat_srv.rs:1390`（分配游标） |

**目标**

1. 在 `block_srv` 内加**只读扇区缓存**：键 `(卷号, lba)`，命中直接回数据；**写路径写穿透 + 失效对应区间**（不允许脏数据）。
2. **顺序预读**：检测连续 lba 递增读，或由上层经新 opcode `BLOCK_OP_PREFETCH`（可选）显式提示 → 预取后续若干扇区进缓存。
3. **量化**：启动日志打印 `blk-cache: hits=… miss=… prefetch=…`；并把回归总耗时 before/after 写进文档。

**不做**：不改任何 FS 服务（`fat32_srv`/`mfs_srv`/`ext2_srv`/`exfat_srv` 一行都不动，纯透明加速）；不做写回（write-back）缓存 —— 那会把"掉电一致性"这口锅引进来；不动 `mfs_srv` 的目录扫描（02b）。

**交付物**：`user/srv/src/block_srv.rs`、`user/srv/src/common.rs`（如需新 opcode）、`docs/dev-reference.md`（块层条目）、`docs/roadmap-fs.md`（新增小节）、`README.md`（勾选行）。

**验收**

- 门禁四道全过；回归全绿（缓存不能改变任何可见语义）；
- 日志里 `hits > 0`、`prefetch > 0`；
- **有数字**：同一台机器上 `before/after` 的回归总耗时 + FS-12 相关耗时对比（允许"提升有限"，但要如实记录，并给出为什么 —— 例如大头是 IPC 往返而非块 I/O，那正是 02b 的动机）。

**风险 / 坑**

- 失效必须**按卷隔离**（多卷共用 cache，卷号是键的一部分）；`part.*`/`mkfs.mfs` 这类"整卷改写"操作后必须**整卷失效**。
- 缓存预算要按页算（内核只给你映射好的页），别申请过大批量连续 DMA。
- 回归是**串行资源**（见 [dev-workflow.md](dev-workflow.md) 的「并行协作」铁律 3），别和 01/03 同时跑。

---

### 03 — 真机存储驱动：`ahci_srv`（SATA/AHCI）✅ 已完成（03b：读+写 + 接进卷层）

**缺口证据**

| 事实 | 位置 |
|---|---|
| 唯一"块驱动"就是 NVMe，且**驱动与块服务、卷层全塞在一个文件**里 | `user/srv/src/block_srv.rs`（域 5）：`nvme_main()` :682 起、卷层 :1457 起、IDE PIO 回退 `ide_block_main()` :2529 |
| 内核侧只有"通用授权 + 按 PCI 类查找" | `kernel/src/device.rs:99`(`grant`)、`kernel/src/arch/pci.rs:112/131/150`(`find_nvme`/`find_net`/`find_virtio_blk`) |
| 第二个真实驱动的**现成模板** | `user/srv/src/virtio_blk_srv.rs`（域 17）：`run()` :130、自解析 PCI 能力 :141-148、`setup_queue` :188、MSI-X :212-229、三段式描述符链 :82、自测 :246-301 |
| ⚠️ **中断只有 MSI-X 通路**：没有 INTx、也没有 MSI(非 X) | `kernel/src/arch/pci.rs:26` 只认 `CAP_ID_MSIX=0x11`；INTx 只有 `disable_intx()` :282 |
| 仓库无 AHCI/SATA/xHCI/USB 存储 | 全仓 grep 无命中 |
| AHCI 常态用 INTx 或 MSI(非 X) | 与本仓库现状冲突 → **第一版必须走轮询**（或先补 MSI 通路，但那要动内核中断层，不属本轮） |

**目标（3a，本轮唯一交付）**：新增 `ahci_srv`（新域，18）：

1. 内核侧：`pci.rs` 加 `find_ahci()`（class `01:06:01`）、`kernel/src/main.rs` 的声明段照 `find_virtio_blk` :463-484 加一段 `device::grant(...)`；
2. 服务侧：初始化 HBA（`CAP/GHC/IS/PI` 端口位图）→ 选定端口 → 建命令表/命令列表/FIS 区（DMA 页）→ `IDENTIFY DEVICE` → 以 LBA48 DMA READ 读扇区；
3. **完成后走轮询**（`PxCI`/`PxIS` 轮询），不碰中断（理由见上）；把"若日后要走中断需要补 MSI/INTx 通路"写进文档；
4. **自测**：读该盘扇区 0，校验宿主预写的签名（照 `VBLK1` 的做法），打 `AHCI1 … sig=ok` marker。

**不做（3a 当时明确排除；其中"接进卷层 + 写路径"已由 03b 补上）**：热插拔、xHCI/USB（03c）、MSI/INTx 通路。

**03b（已完成）—— 把 AHCI 盘接进块服务卷层（读 + 写）**

3a 只读自测完就常驻等待；03b 让它成为真正的**块后端**：

1. **分层**：`ahci_srv` 自测通过后用 `BLOCK_OP_ATTACH` **异步**通知 `block_srv`（用 `send` 不用 `call`
   —— `block_srv` 收到后会**回调** ahci 做读写校验，同步等待回复会自锁）；`block_srv` 分配一个传输
   暂存页并**同址共享**给 ahci，把它登记成 `backend=ahci` 的卷（对外 `BlockReq` 卷号与 NVMe 一致）。
2. **读/写都经卷层转发**：`block_srv` 按 8 扇区（一页）切分，`sys_call` 请 `ahci_srv` 在自己的 DMA
   通路上完成（`WRITE DMA EXT` + `FLUSH CACHE EXT`），数据经共享暂存页互拷。上层文件系统完全不必
   知道盘挂在 AHCI 还是 NVMe。⚠️ **写方向必须先拷数据再发 IPC**（反过来 ahci 取到的是上一笔残留）。
3. **挂载取证**：读扇区 0 校验签名（`block: ahci volume attached … sig=ok`）；签名匹配（安全门，
   避免在真盘上写坏数据）才做写回读自测（`AHCI2 … rw=ok`）。
4. **持久**：分区表重扫（`PART_RELOAD`）会重建 NVMe 卷表，AHCI 卷另存一份并在重扫后重新挂回表尾。

**接线（03b 新增授权）**：`kernel/src/main.rs` —— `block_srv → SendTo+MapInto → ahci_srv`、
`ahci_srv → SendTo → block_srv`（内核无设备专属逻辑，仍只是能力签发）。


**接线清单（9 处，见 [dev-workflow.md](dev-workflow.md) §4）**：`user/srv/Cargo.toml`（features + `[[bin]]`）、`user/srv/src/lib.rs`（门控）、`user/srv/src/bin/ahci_srv.rs`（新）、`Makefile`（`SRV_NAMES`）、`boot/src/main.rs`（`SERVICE_FILES` **和它的长度 `18`**）、`kernel/src/domain.rs`（`BOOT_DOMAINS` +1，以及文件头注释与单测）、`kernel/src/main.rs`（建域 + 授权 + 设备声明）、`kernel/src/arch/pci.rs`。

**验收**

- 门禁四道全过；回归全绿；
- **判据要更新**（这是本流的"接线副作用"，必须同步到 docs）：`[OK] 18 service ELFs loaded` → `19`；`BOOT_DOMAINS` 与域表长度一致；若驱动占用 MSI 向量则 `irq_cmds == cmds` 的口径要重新确认（**建议第一版不申请中断，向量数保持 0，别动这条判据**）；
- QEMU 验证：q35 自带 AHCI（`ich9-ahci`），挂盘示例 `-drive id=d0,if=none,file=build/ahci.img,format=raw -device ide-hd,drive=d0,bus=ide.0`（具体总线名以 `qemu-system-x86_64 -device ahci,help` 为准），宿主预写签名 → 日志出现 `AHCI1 … sig=ok`；
- 03b 追加判据：`block: ahci volume attached … sig=ok`、`AHCI2 … rw=ok`，且卷表末行出现 `backend=ahci`（03b 起**可写**，不再是"只读、盘内容未变"）。

**风险 / 坑**

- AHCI 的 DMA 缓冲必须**物理连续 + 页对齐**（内核 `device::grant` 的 `dma_pages`）；命令表基址要 1 KiB 对齐、命令列表 1 KiB、FIS 256 B —— 对齐算错会静默收不到完成。
- 端口可能有设备但**不是 SATA 盘**（ATAPI/PM 端口倍增器）→ `IDENTIFY` 后按 `word 0` 判类型，非磁盘直接跳过。
- 轮询要带超时（`PxTFD.STS.BSY/DRQ`），否则坏盘会把服务挂死。

---

### 04 — 权限与多用户（本轮：设计 + 接口草案）✅ 设计稿已完成（实现顺延 04b）

**缺口证据**

| 事实 | 位置 |
|---|---|
| `chmod` 只存不判 | `mfs_srv.rs:3428-3459`(`mfs_chmod`)、`vfs.rs:917-941`；全库 `mfs_get_mode` 只有两处**显示**用途（`mfs_srv.rs:3493`、`4083`） |
| `owner` 存的是**发起请求的域号**(u16)，没有 uid、也没有 gid | 注释 `mfs_srv.rs:98-99`、赋值 `:3819`/`:3859`、说明 `:4098`；协议注释 `vfs.rs:512` |
| 架构上规划的"认证服务 + 能力模型"尚未实现 | `docs/architecture.md:134-140`、`:56` |

**目标（本轮）**：产出 `docs/design-permissions.md`，必须回答：

1. **身份模型**：uid/gid 从哪来（认证服务签发？init 分配域身份？），域 ↔ 身份如何映射；与既有 `Capability` 模型怎么衔接（"每个打开的文件是一个能力"是否落地）。
2. **落盘模型**：MFS inode 现有 40 B 元数据里 `mode`/`owner` 怎么扩（`MFS_META_*` 常量表 `mfs_srv.rs:101-107`；加 gid 要不要升 magic —— 若升，**必须与 01 的 magic 策略联动**）。
3. **检查点清单**：`open/creat/mkdir/unlink/rmdir/rename/chmod/chown/truncate/readdir` 各在哪判、判什么（给函数名与行号锚点），拒绝时回什么错误码（现在没有 `EACCES` 这一类）。
4. **自测规划**：给出 FS 自测编号（本轮**不占号**，留给 04b）+ 用一个"低权身份"的端到端用例。
5. **与能力的边界**：文件服务内判权限 vs 能力系统在 IPC 层判，孰先孰后、谁兜底。

**不做（本轮）**：不写实现代码、不动 `mfs_srv.rs`（01 独占）、不动内核 syscall 号表（新增 syscall 由主会话预置，不属并行流）。

**交付物**：`docs/design-permissions.md`（新）、`docs/roadmap-fs.md`（新增小节，指向设计稿）、`README.md`（如需要）。

**验收**：设计稿能通过"照它就能实现"的检验 —— 每个检查点都有**文件:行号锚点**与**拒绝语义**；并明确写出 04b 的任务分解（谁动哪些文件、依赖 01 的哪些成果）。

---

## 4. 并行落地约定

### 4.1 文件互斥矩阵（本轮，硬约束）

| 文件 / 区域 | 01 | 02 | 03 | 04 |
|---|---|---|---|---|
| `user/srv/src/mfs_srv.rs` | ✅ 独占 | — | — | — |
| `user/libmorion/src/vfs.rs` | ✅ 独占 | — | — | — |
| `user/srv/src/shell.rs` | ✅ 独占 | — | — | — |
| `user/srv/src/app.rs`（FS 自测表） | ✅ 独占（**FS-30..33**） | — | — | — |
| `docs/shell-reference.md` | ✅ 独占 | — | — | — |
| `user/srv/src/block_srv.rs` | — | ✅ 独占 | —（3b 等 02） | — |
| `user/srv/src/common.rs` | — | ✅ 独占 | — | — |
| `docs/dev-reference.md`（块层条目） | — | ✅ | ✅（PCI/驱动条目） | — |
| `kernel/src/arch/pci.rs` | — | — | ✅ 独占 | — |
| `kernel/src/domain.rs` / `kernel/src/main.rs`（声明段） | — | — | ✅ 独占 | — |
| `boot/src/main.rs`（`SERVICE_FILES`） / `Makefile`（`SRV_NAMES`） | — | — | ✅ 独占 | — |
| `user/srv/Cargo.toml` / `user/srv/src/lib.rs` / `user/srv/src/bin/*` / `user/srv/src/ahci_srv.rs`（新） | — | — | ✅ 独占 | — |
| `docs/roadmap-driver.md` | — | — | ✅（自己小节） | — |
| `docs/design-permissions.md`（新） | — | — | — | ✅ 独占 |
| `docs/roadmap-fs.md` | ✅ 自己小节 | ✅ 自己小节 | — | ✅ 自己小节 |
| `docs/plan-fs-streams.md`（本文） | 完成后追加"已完成"标记 | 同左 | 同左 | 同左 |
| `README.md` | ✅ 自己行 | ✅ 自己行 | ✅ 自己行 | — |

> `docs/roadmap-fs.md` 与 `README.md` 是**共享文档**：只改自己的小节/行，写完**立刻提交**；动之前先重读一遍当前内容（别人可能刚改过）。

### 4.2 自测编号分配（防止撞号）

- 01：**FS-30 / FS-31 / FS-32**（magic 拒绝、fsck 对账、sync 落盘）→ 加上限号 FS-33 备用。
- 02：**不占 app 自测号** —— 块层缓存由 `block_srv` 自己打印计数，回归里 grep 即判。
- 03：**不占号** —— 驱动自测像 `virtio_blk_srv` 一样打自己的 marker（`AHCI1 …`）。
- 04：本轮不占号（04b 时再分配）。

### 4.3 三条铁律（沿用 [dev-workflow.md](dev-workflow.md) 的「并行协作」）

1. **只改自己的独占文件**；发现必须动公共文件 → 停下来，先跟主会话对齐。
2. **提交只 `git add` 自己的文件**；**绝不** `git add -A` / `git add .` / `git stash` / `git checkout -- .` / `git reset --hard`。
3. **重活串行**：全量 QEMU 回归同一时刻只跑一个（共享 `build/` 与 KVM）；一个会话在跑回归时，另外几个做不跑 QEMU 的活（写代码、`cargo test --lib -p morion-kernel`）。

### 4.4 MFS 扩展点约定（01 会用到，别人也不用另立门户）

- 新 tag：`user/libmorion/src/vfs.rs` 的 MFS 段（当前 `:168-199`）加 `pub const XXX_TAG: u64`（4 字节 ASCII，低 32 位唯一）+ 一个客户端封装函数（仿 `:208-273`）。
- 服务端分派：`user/srv/src/mfs_srv.rs` 的 `run()` 里 `match tag`（当前 `:3732-4004`）加一条分支；按卷寻址的注意 `:3717` 的卷切分。
- shell 命令：`user/srv/src/shell.rs` 的 `match cmd`（当前 `:232`，MFS arm 在 `:294-300`）+ `help` 文案（`:255-263`）+ 一个 `shell_xxx(arg)` 函数；返回数据统一走 `vfs::SHELL_RESULT_BUF`（定义 `vfs.rs:45`，shell 侧分配/共享在 `shell.rs:131-139`）。

### 4.5 与既有文档的关系

- 长期路线图：`docs/roadmap-fs.md`（每条流完成时在**自己小节**标记 ✅）。
- 驱动线路线图：`docs/roadmap-driver.md`（03 在这里加小节）。
- 环境与门禁、并行铁律：[dev-workflow.md](dev-workflow.md)。

---

## 5. 落地状态（2026-10-02 收口）

四条流全部执行完毕，**合并后的工作区**由主会话复验了一遍（不是各流自证）：

| 流 | 状态 | 关键产物 |
|---|---|---|
| 01 健壮性 | ✅ 已完成 | `mfs_sb_magic_state` 三态判定（拒绝挂载 + 明确日志）、`MFS_FSCK_TAG="MFSC"` + `mfs.fsck [--repair]`、`MFS_SYNC_TAG="MSYN"` + `mfs.sync`、FS-31/FS-32 自测 |
| 02 块层性能 | ✅ 已完成 | `block_srv` 只读缓存（128 行 × 4 KiB = 512 KiB）+ 顺序预读 + `blk-cache:` 计数 |
| 03 真机存储驱动 | ✅ 已完成（含 03b） | `ahci_srv`（域 18，SATA/AHCI，全轮询）+ `pci::find_ahci`（BAR5=ABAR）+ `BOOT_DOMAINS` 19；03b 起读+写并经 IPC 接进 `block_srv` 卷层（`backend=ahci`） |
| 04 权限设计 | ✅ 设计稿完成 | [design-permissions.md](design-permissions.md)（uid/gid 放 `+32` 保留区、不升 magic、`MFS_E*` 错误码、能力在前权限在后） |

**合并后复验结果**：

- 门禁 ①：`cargo fmt` / `make fmt` / `make check` / `make clippy` **全 0 warning**；
- 门禁 ②：内核单测 **34 passed**（原 29）；
- 门禁 ③：`make run-nvme`（仅构建）通过；
- 门禁 ④：全量回归 `SELFTEST DONE=1`、`FAILED/PANIC=0`、失败明细（无），`[OK] 19 service ELFs loaded`、
  `VBLK1 … sig=ok, rw=ok`、`sgdisk -v` no problems；
- **性能实测**（02 的效果）：回归总耗时 **324 s → 259 s**；`blk-cache` 命中约
  `hits=11745 / miss=2942`（≈80%），NVMe 实际命令数 `28672 → 16384`；
- **FS-30 补验**（01 文档里声称"宿主侧取证"但当时未留记录，本次补做）：把 `build/mfs.img`
  两份超级块 magic 改成 `MFS7` → 起机日志 `mfs: refuse to mount: on-disk magic MFS7 … != expected MFS8 …
  (data left untouched)`，且盘 `sha256` **起机前后完全一致**（一个字节都没写盘）。
- **03 的 AHCI 在合并树上复验**：`AHCI1 ahci OK, cap=2048, sector0 sig=MORION-AHCI-TST!, sig=ok`；
  `SELFTEST DONE=1`、`FAILED/PANIC=0`（258 s）。
- **03b 复验（AHCI 接进卷层 + 读+写，本轮）**：标准回归已挂 `build/ahci.img`（`-device ide-hd`），
  日志出现 `block: ahci volume attached (vol=8, sectors=2048, sig=ok)` 与
  `AHCI2 ahci volume rw OK, vol=8, lba=2047, rw=ok`，卷表末行 `backend=ahci` 在多次
  `PART_RELOAD` 后仍在；`SELFTEST DONE=1`、`FAILED/PANIC=0`（258 s，exit 0）。

**收口时发现的待办 —— 前两条已在收口后补掉**：

1. ~~`--repair` 真正回收泄漏 inode 的路径还没有用例~~ → ✅ **已补**：新增
   [scripts/fsck-leak.sh](../../scripts/fsck-leak.sh) —— 宿主侧把 `/mfs/LEAK.TXT` 的目录项改成
   空槽（`name_len=0` + `ino=0`，**并重算块头 CRC**，否则 `mfs_ok` 不过、fsck 直接放弃遍历），
   再让客户机报案/回收/复查（三轮启动，复查在**重启之后**）：实测
   `mfs.fsck: 1 leaked inode(s)` → `mfs.fsck: repaired 1 leaked inode(s), reclaimed 12 block(s)`
   → 重启后 `mfs.fsck: 0 leaked inode(s)`（`LEAK-RC=0`）。
   为什么不做成 app 自测：泄漏只出现在两次 COW 提交之间的掉电窗口里，用户态 API 造不出来。
2. ~~`build/ahci.img` 靠手工 `dd` 造、Makefile 无规则~~ → ✅ **已补**：`Makefile` 新增
   `AHCI_IMG`/`AHCI_MIB` 与 `$(AHCI_IMG)` 规则（1 MiB + 扇区 0 预写 `MORION-AHCI-TST!`，
   规则重建的镜像与 03 那份 **sha256 逐字节一致**），并**接进 `make run-nvme` 与
   `scripts/fs-regress.sh`**（回归判定新增 `AHCI1 … sig=ok`；03b 起再加
   `block: ahci volume attached … sig=ok` 与 `AHCI2 … rw=ok`；带判据的全量回归实测 258 s 全绿）。
3. 02b（**目录索引已完成**；请求批量化待）、04b（权限强制，**已完成**）按 §2 末尾的排期等前序合入；
   **03b（AHCI 接进卷层 + 读+写）本轮已完成**。

**顺带修掉的工具问题**（都在收口这一轮踩到）：`scripts/probe-shell.sh` 原先**漏挂 nsid=7
（`pt.img`）**，导致自测 FS-26 `part wipe FAILED` 后整套自测停住、`PROBE_WAIT_PATTERN` 白等满
超时（已补齐 7 个 namespace，并让等待循环在 QEMU 退出时立刻收手）；同时新增
`PROBE_WAIT_PATTERN`/`PROBE_WAIT_TIMEOUT`（等日志出现某串再注入）与 `PROBE_CMD_GAP`
（系统忙时按键会丢，需放长条间间隔）两个开关。

**提交状态**：已按"工具类 / 01 / 02 / 03+安装盘 / 文档"拆成 5 条提交落库
（`b0c8031` 工具 → `fd24690` D4+安装盘 → `bca117a` 01 → `b515d38` 02 → `d49dc88` 文档）；
本节的收口追加（AHCI 测试盘规则 + 泄漏回收用例）单独一条提交。
文件级互斥拆分没法做到"每条提交一处改动"：`shell.rs`/`vfs.rs`/`mfs_srv.rs` 里 01 与
`mkfs.mfs --force` 同行级交织、`Makefile` 里 D4 与安装盘变体交织，故这两处只能合并成一条。
会话交接类临时文件按约定留在本地不入库（其长期价值部分已沉淀进 [dev-workflow.md](dev-workflow.md)）。
