# 开发工作流 —— 环境 · 门禁 · 并行协作

> 面向所有会话 / 贡献者的**常驻**工作流文档。内容原先散落在本地临时交接文件里（不入库、
> 随会话过期），现沉淀进仓库正式文档，供新会话冷启动直接参考。
>
> 相邻权威文档：
> - 阶段进度**唯一账本**：[dev-reference.md](dev-reference.md) §9
> - 路线图：[roadmap-fs.md](roadmap-fs.md) / [roadmap-driver.md](roadmap-driver.md) / [roadmap-gfx.md](roadmap-gfx.md)
> - 测试脚本：仓库根 `scripts/`

---

## 1. 环境准备与陷阱

```bash
# 每个新终端 / 每个会话都先执行（这台机器的 shell PATH 里没有 cargo）
export PATH="/usr/local/sbin:/usr/local/bin:/usr/bin:/bin:$HOME/.cargo/bin"
cd /home/jinjun/文档/morion-os

# 最小自检（约 1 分钟）
cargo fmt --all -- --check && make check && make clippy
cargo test --lib -p morion-kernel          # 期望 34 passed
```

**都是踩过的坑**：

1. **PATH**：这台机器的 shell 里没有 cargo。每条命令前加
   `export PATH="/usr/local/sbin:/usr/local/bin:/usr/bin:/bin:$HOME/.cargo/bin";`
2. **shell 是 zsh**：`$VAR` **不做词分割**。把 QEMU 参数放进变量再传给命令，必须写 `${=VAR}`。
3. **回归必须用磁盘 `OUT_DIR=build`**：`/tmp` 是 tmpfs，镜像进内存会 OOM。
4. **手测 QEMU 加 `-enable-kvm -cpu host`**（宿主有 `/dev/kvm`）。
5. **QEMU 11.x**：`-machine q35,intel-iommu=on` 属性**已被移除**，开 VT-d 必须
   `-machine q35 -device intel-iommu`。
6. **耗时预期**：全量 FS 回归有 KVM 也要 **~6 分钟**。日志里长时间"只有 shell 提示符"不是卡死。
7. **内核 panic 是黑匣子**：打印 `KERNEL PANIC` + `at 文件:行:列` + `msg`，先看这三行定位。
8. **`scripts/fs-regress.sh` 会把 QEMU 的 stderr 丢掉**。查 IOMMU / QEMU 侧错误时，
   必须手工跑一遍 QEMU 把 `2>` 落到文件，否则什么都看不到。

---

## 2. 回归门禁（每步做完必须全过）

```bash
export PATH="/usr/local/sbin:/usr/local/bin:/usr/bin:/bin:$HOME/.cargo/bin"
cd /home/jinjun/文档/morion-os

# ① 静态检查（必须 0 warning）
cargo fmt --all && make fmt && make check && make clippy

# ② 内核单测（当前 36 passed）
cargo test --lib -p morion-kernel

# ③ 只构建镜像（不启动 QEMU）
make OUT_DIR=build QEMU=/bin/true run-nvme

# ④ 全量 FS 回归（~6 分钟，退出码 0 = 通过）
OUT_DIR=build bash scripts/fs-regress.sh /tmp/regress.log
```

**判据**（缺一不可）：

- `SELFTEST DONE` ≥ 1；`FAILED` / `PANIC` = 0
- `[OK] 25 service ELFs loaded`
- **`irq_cmds == cmds` 且 `poll_cmds = 0`**（**不要硬比历史数字**：`cmds` 的绝对值会随块层
  缓存 / 预读、新增自测而变化；判的是**两者相等**且**没有退化成轮询**）
- `VBLK1 virtio-blk OK, cap=2048, sector0 sig=MORION-VBLK-TST!, sig=ok, rw=ok`
- `NET1 virtio-net up, MAC=52:54:00:12:34:56, ARP reply OK`
- `AHCI1 ahci OK, cap=2048, sector0 sig=MORION-AHCI-TST!, sig=ok`（测试盘由 Makefile 的
  `AHCI_IMG` 规则生成，**`fs-regress.sh` 自己会挂它**）
- `sgdisk -v build/pt.img` → `No problems found`

**改到驱动 / IOMMU 的，再跑一遍开 IOMMU 的版本**：

```bash
make run-nvme IOMMU=1                                          # 交互手测（+ -enable-kvm -cpu host）
IOMMU=1 OUT_DIR=build bash scripts/fs-regress.sh /tmp/iommu.log
```

**无图形变体**（改到 shell / 构建开关时）：

```bash
NOGUI=1 make QEMU=/bin/true run-nvme
OUT_DIR=build/nogui bash scripts/fs-regress.sh /tmp/nogui.log
```

**变体镜像入口**：`REGRESS_ISO=build/install/morion-os.iso`（回归）/ `PROBE_ISO=…`（probe）；
磁盘镜像仍共用 `build/*.img`。`make INSTALL=1 iso` / `make NOGUI=1 iso` 都是**编译期环境变量**，
切回默认会重新编译 → 日常回归务必用**默认变体**。

---

## 3. 已知坑清单（当 checklist 用）

- **`dd` 写镜像必须 `conv=notrunc`**，否则整盘被截断（曾导致 `rw=BAD`）。
- **`BOOT_DOMAINS` 与各全局表长度必须一致**：域 id 是 `irq`/`cap`/`ipc`/`pager` 等表的**下标**，
  加了域却忘了改 `BOOT_DOMAINS`（或反过来）会**越界 panic**。同理别漏 `boot/src/main.rs` 里
  `SERVICE_FILES` 的**长度常量**（现在 25）。
- **DMAR 表体**：重映射结构从偏移 **48** 起（表头 36 + `Host Address Width` 1 + `Flags` 1 + 保留 10），
  按 36 解析会出现"表找到了但 `drhd=0 rmrr=0`"。
- **VT-d 上下文项 `TT`（bits 3:2）**：`0b00` = translated（走二级页表）、`0b01` = Device TLB、
  `0b10` = pass-through。**写反过**：写成 `0b01` 时 QEMU 直接拒
  （`vtd_ce_type_check: DT specified but not supported`）。
- **virtio ≠ IOMMU 证据**：virtio 设备默认**绕过 IOMMU**（QEMU 没开 `iommu_platform`）。
  要证明"翻译生效"只能用 **NVMe**。
- **VT-d 窗口必须严格 < 4 GiB**：该 QEMU 只翻译 < 4 GiB 的 IOVA，恰好 4 GiB 的 PRP 会绕开翻译。
- **`FEDATA`/`FEADDR`/`FEUADDR` 不含故障内容**：它们是故障事件（MSI）的配置寄存器；
  SID / 原因 / 故障地址在 **FRCD**（`0xB0 + 16*i`）。本机 QEMU 只置 `FSTS.PPF` 不置 `FRI`，
  所以"有没有故障"按 `FSTS != 0` 判。
- **文档是验收的一部分**：`dev-reference.md` §9 追加一行、`roadmap-*.md` 改状态 + 验收表、
  `README.md` 勾选同步。

**文件系统四流 + 收口轮踩到的**（2026-10-02）：

- **手拼 QEMU 命令行必须挂全 7 个 namespace**（`nvme/mfs/ext2/parts/exfat/spare/pt` = nsid 1..7）。
  缺 nsid=7 会让自测 **FS-26 直接 FAILED 并终止整套自测**，此后**永远等不到 `SELFTEST DONE`**。
- **残留 QEMU 会锁住 `build/*.img`**：后续 QEMU 秒退，现象是"日志 0 行 / 回归耗时 8 秒"，报错
  `Failed to get "write lock"`。注意 **kill 掉 bash 包装不一定杀掉 QEMU**（它是孙进程）——
  用 `pgrep -f qemu-system-x86_64`（`pgrep -x` 因进程名 >15 字符会直接报错）找到 qemu 的 pid 再杀。
- **宿主侧改 MFS 镜像必须重算块头 CRC**：块头 `[magic u32 | crc32 u32 | payload]`，CRC 覆盖 payload
  （偏移 8..块尾；算法 CRC-32/IEEE 反射 = Python `zlib.crc32`）。只改 payload 不改 CRC → `mfs_ok`
  校验不过 → `mfs_fsck_walk_inos` 当场 `return false`（现象 `mfs: fsck walk FAILED`）。
- **probe 的按键注入在系统忙时会丢字**：自测还在跑时 18 s 条间间隔会把后续命令整条丢掉 →
  用 `PROBE_CMD_GAP` 放长，或**一条命令一轮启动**最稳。
- **`probe-shell.sh` 现在有 4 个开关**：`PROBE_ISO` / `PROBE_LOG` / `PROBE_WAIT_PATTERN`（等日志里
  出现某串再注入，如 `SELFTEST DONE`；QEMU 中途退出时立刻收手，不再白等满超时）/ `PROBE_CMD_GAP`。
- **`scripts/fsck-leak.sh` 的预期噪声与耗时**：第 2/3 轮卷上带着上一轮自测的遗留 + **故意**制造的
  泄漏，自测会有若干条打 `FAILED`（`FS5 mkdir /mfs/D`、`FS31` 报非 0 泄漏）—— 判定**只看**
  `mfs.fsck:` 三行；整轮 ~17 分钟（三轮都要等自测结束）。
- **块层缓存改变了 `cmds` 的绝对值**（28672 → 16384）：`irq_cmds == cmds` 的判据不变，但**别把
  历史数字写死进脚本或文档**。

---

## 4. 加一个用户态服务要同步改的地方（**10 处**，漏一个就编不过或越界）

1. `user/srv/src/<name>.rs`（实现）
2. `user/srv/src/bin/<name>.rs`（`#![no_std]`/`#![no_main]` 入口，`announce("<name>", domain_id)` 后调 `run()`）
3. `user/srv/Cargo.toml`：`features` 加 `svc-<name>`（**并把它加进 `default`**）+ `[[bin]]` 加一条
4. `user/srv/src/lib.rs`：模块 `#[cfg(feature = "svc-<name>")]` 门控
5. `Makefile`：`SRV_NAMES` 加名字
6. `boot/src/main.rs`：`SERVICE_FILES` 表加 `(域号, "<name>")` —— **以及它的长度常量**
7. `kernel/src/domain.rs`：`BOOT_DOMAINS` +1（含文件头注释与单测）
8. `kernel/src/main.rs`：建域 + 能力授权 +（如需要）设备声明
9. `user/srv/src/init.rs`：`SUPERVISED` 加一条（要纳入监督的话）
10. `user/srv/src/mfs_srv.rs`：`MFS_BOOT_DOMAINS` 对齐 `BOOT_DOMAINS`（该常量按引导域数判身份，漏改会让新域被当成非引导域）

> 若服务要用设备：另加 `kernel/src/arch/pci.rs` 的按类查找 + `libdevice`（`grant`/`mmio`/`msix`/`virtio`）。

> **加服务 = 动公共文件**，在多会话并行时会直接破坏"文件互斥"。并行轮次里**不要**顺手加服务。

---

## 5. 并行协作（同一工作区 · 不开分支、不建 worktree）

多个会话可以共享**同一个工作区**并行推进，靠**文件互斥**避免互相踩。公共接线点
（syscall 号、`lib.rs` 模块声明等）在需要时**一次性预置好**，之后各任务都不用碰公共文件。

### 5.1 并行接线层

新 syscall 号与 `syscall.rs` 的转发臂（尚未实现的先置为占位/转发）由主会话**一次性预置**，
各并行任务只填各自模块的实现，从而不必去改 `syscall.rs` 这个公共文件。参见提交 `d3abfbd`
（`SYS_UNAME`/`SYS_DEVICE_INFO`/`SYS_DEVICE_GRANT` 的预留）。

### 5.2 三条铁律

1. **只改自己的独占文件**。要动公共文件（`main.rs` / `syscall.rs` / `lib.rs` / `domain.rs` /
   `boot/src/main.rs` / `Makefile` / 别人的模块）就**停下来**先对齐。
2. **提交只 `git add` 自己列的文件**：
   ```bash
   git add kernel/src/version.rs user/srv/src/shell.rs README.md   # 只列自己的
   git commit -m "..."
   ```
   ⚠️ **绝对不要** `git add -A` / `git add .` —— 会把别的会话**还没写完的**改动一起提交。
   ⚠️ 不要 `git stash` / `git checkout -- .` / `git reset --hard` —— 会清掉别人的活。
   ⚠️ 提交前先 `git status` 看一眼：**工作区里可能有别人的半成品**。
3. **重活串行**：全量 QEMU 回归**同一时刻只跑一个**（共享 `build/` 与 KVM）。
   一个在跑回归时，别的会话做不跑 QEMU 的活（写代码、`cargo test --lib -p morion-kernel`）。

### 5.3 万一真撞了

- `git` 报 `index.lock`，或 cargo 报"等待文件锁" → 另一个会话正在提交/编译，**等几秒重试**。
- 发现自己的改动被覆盖 → 以**磁盘当前内容**为准，把自己的改动补回去，然后立刻提交。
- 千万不要用 `git reset --hard` / `git checkout -- .` "恢复"。

---

## 6. 测试 / 验证脚本分工

| 脚本 | 角色 |
|---|---|
| `scripts/fs-regress.sh` | **门禁第 ④ 条**：全量 QEMU 回归 + 判据汇总（含 `AHCI1 … sig=ok`）。每轮重置 `spare/pt/vblk/ahci`，并就地往 `nvme.img` 补 `hello.mex` 与服务镜像 |
| `scripts/probe-shell.sh` | 注入探针：起机 + 经 QEMU monitor 敲 shell 命令，~1 分钟出结果（4 个开关见 §3） |
| `scripts/fsck-leak.sh` | `mfs.fsck --repair` 的泄漏回收用例（宿主改镜像 + 三轮启动，~17 分钟；判定只看 `mfs.fsck:` 三行） |
| `scripts/usb-rw.sh` | 真机 U 盘端到端读写（`part.wipe` → `mkfs.mfs` → 写文件 → 只读重启读回 + 宿主解 MFS8 超级块） |
| `scripts/ra-check.sh` | rust-analyzer 的项目级检查入口（配合仓库根 `rust-analyzer.toml`，消 no_std crate 的 `#[panic_handler]` 假红） |
