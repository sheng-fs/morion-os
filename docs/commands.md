# Morion OS — 命令规范

本文件是**唯一的命令入口规范**：构建、运行、调试、验证都按这里写的命令与参数执行。
新增/修改任何运行方式时，先改本文件，再改 `Makefile`。

---

## 1. 构建（Makefile 目标）

| 命令 | 作用 | 产物 |
| --- | --- | --- |
| `make` | 默认目标，等价 `make iso` | `build/morion-os.iso` |
| `make kernel` | 仅构建微内核 | `build/kernel/morion-kernel` |
| `make user` | 仅构建用户态程序 | `build/user/user.bin` |
| `make boot` | 仅构建 UEFI 引导器 | `build/boot/morion-boot.efi` |
| `make iso` | 构建完整可启动 ISO | `build/morion-os.iso` |
| `make clean` | 清理 `target/` 与 `build/` | — |
| `make check` | 逐个 crate 用**各自 target** 做 `cargo check` | — |
| `make clippy` | 同上跑 clippy，**门禁**（`-D warnings`，有告警即失败） | — |
| `make fmt` | `cargo fmt --all -- --check` | — |
| `make setup` | 检查/安装工具链与依赖 | — |
| `make debug` | QEMU + GDB（`-s -S`，监听 1234） | — |

依赖关系：`iso → kernel → user`（内核用 `include_bytes!` 嵌入 `build/user/user.bin`），
且 `boot` 需要先 `make kernel` 产出 `boot/loader/morion-kernel.elf`。
**只改了用户态代码也必须走 `make iso`**，否则内核里嵌的还是旧的 `user.bin`。

`check` / `clippy` 不能写成 `cargo check --workspace`：三个 crate 的 target 各不相同
（kernel `x86_64-unknown-none`、user 自定义 `user/x86_64-morion-user.json`、boot
`x86_64-unknown-uefi`），工作区级命令会退化到宿主 target，在 no_std bin 上报
`#[panic_handler] required` 直接失败。clippy 已清零并作为门禁（`-D warnings`），
提交前应能同时通过 `make check` 与 `make clippy`。

### 常用变量（可覆盖）

```bash
make run-nvme QEMU_MEM=4G                    # 内存 (默认 2G)
make run-nvme MFS_MIB=64                     # MFS 盘大小 MiB (默认 16)
make run-nvme EXFAT_MIB=2048 EXFAT_CLU=32K   # 大 exFAT 卷 (32 KiB 簇, 位图 > 4 KiB) 验证去上限
make run-nvme NVME_CLU=64                    # fat32 大簇 (32 KiB 簇) 验证写路径
make iso OUT_DIR=build2                      # 自定义输出目录
```

| 变量 | 默认 | 说明 |
| --- | --- | --- |
| `QEMU_MEM` | `2G` | 客户机内存 |
| `QEMU_SMP` | `4` | 客户机 CPU 数 |
| `QEMU_ACCEL` | `kvm` | 加速方式（`kvm` / `hvf` / `whpx`） |
| `NVME_IMG` | `build/nvme.img` | FAT32 盘（挂 `/`） |
| `NVME_CLU` | `8` | fat32 每簇扇区数（8 = 4 KiB 簇）；`64` = 32 KiB 簇。mkfs.fat 对 64 MiB 盘默认给 512 B 簇，太碎且写大文件极慢，故固定为 4 KiB |
| `MFS_IMG` | `build/mfs.img` | MorionFS 盘（挂 `/mfs`，**空白 raw，首次挂载自动格式化**） |
| `MFS_MIB` | `16` | MorionFS 盘大小 |
| `EXT2_IMG` | `build/ext2.img` | ext2 盘（挂 `/ext2`，宿主 `mke2fs`） |
| `PARTS_IMG` | `build/parts.img` | MBR 分区测试盘（卷层解析） |
| `EXFAT_IMG` | `build/exfat.img` | exFAT 盘（挂 `/usb`，宿主 `mkfs.exfat`） |
| `EXFAT_MIB` | `16` | exFAT 盘大小 MiB |
| `EXFAT_CLU` | *(空)* | exFAT 簇大小（如 `4K`/`32K`/`128K`）；空 = 由 `mkfs.exfat` 按卷大小自选 |
| `DISK_IMG` | `build/disk.img` | IDE 回退测试盘 |

---

## 2. 运行

| 命令 | 场景 |
| --- | --- |
| `make run` | 最简运行（`-machine pc`，无磁盘） |
| `make run-nokvm` | 无硬件虚拟化环境（CI） |
| **`make run-nvme`** | **文件系统验证主用**：q35 + NVMe，五 namespace |
| `make run-ide` | IDE PIO 回退路径验证 |

### `make run-nvme` 的磁盘布局（重要）

单控制器五 namespace，`dev`（BlockReq 高位）现在是**卷号**，由 block_srv 扫描各盘分区表后分配：

| namespace | 后端镜像 | 文件系统 | 挂载点 |
| --- | --- | --- | --- |
| `nsid=1` | `build/nvme.img`（宿主机 `mkfs.fat -F 32`） | FAT32 | `/` |
| `nsid=2` | `build/mfs.img`（纯空白 raw） | MorionFS | `/mfs` |
| `nsid=3` | `build/ext2.img`（宿主机 `mke2fs -t ext2`） | ext2（只读） | `/ext2` |
| `nsid=4` | `build/parts.img`（MBR：FAT32 + ext2 两个分区） | 分区测试盘 | `/usb3`（FAT32 分区）、`/usb4`（ext2 分区） |
| `nsid=5` | `build/exfat.img`（宿主机 `mkfs.exfat`） | exFAT（读写） | `/usb` |

前三个镜像**没有分区表**，各成一个「整盘卷」，卷号恰为 0/1/2 —— 与引入卷层前一致
（`nsid=5` 的 exFAT 整盘卷号为 5）。
`build/parts.img` 是额外的一卷测试盘，用于验证 MBR 解析与类型探测。

**额外卷自动挂载（M1b）**：每个文件服务认领**一个**默认卷（fat32 → 卷 0、mfs → 卷 1、
ext2 → 卷 2、exfat → 卷 5），随后把自己那类的**其余卷**上报给 mount_srv，自动挂到
`/usb<卷号>`（卷号 = 上表里 block_srv 分配的 id）。故 `parts.img` 的两个分区会分别挂到
`/usb3`（FAT32）与 `/usb4`（ext2），启动日志里有对应诊断行：

```
mount-dbg: /usb3 domain=6 slot=7        # FAT 分区 → fat32_srv
mount-dbg: /usb4 domain=12 slot=6       # ext2 分区 → ext2_srv
```

MFS **不参与**额外卷（持久卷 + 自动格式化语义，多卷会误伤），故 `/usb<卷号>` 下不会出现 MFS。

### 接入真实 U 盘（只读）

`-drive file=/dev/sdXN,...` 可把宿主机分区当作一个 namespace 交给 MorionOS，卷层会解析出
该分区的文件系统类型并自动挂到 `/usb<卷号>`。**读别人的盘一律加 `readonly=on`** —— QEMU 层
拒绝写入，物理盘不可改（重新插拔即恢复）：

```bash
# 需要先给当前用户对该块设备的读权限 (临时; 拔插后失效):
#   sudo chgrp $(id -gn) /dev/sdb{,1,2} && sudo chmod 440 /dev/sdb{,1,2}
qemu-system-x86_64 \
  -machine q35 -m 2G -bios /usr/share/edk2/x64/OVMF.4m.fd -cdrom build/morion-os.iso \
  -device nvme,serial=MORION,id=nvme0 \
  -drive file=build/nvme.img,if=none,id=n0,format=raw -device nvme-ns,drive=n0,bus=nvme0,nsid=1 \
  -drive file=build/mfs.img, if=none,id=n1,format=raw -device nvme-ns,drive=n1,bus=nvme0,nsid=2 \
  -drive file=/dev/sdb1,    if=none,id=u6,format=raw,readonly=on -device nvme-ns,drive=u6,bus=nvme0,nsid=6 \
  -drive file=/dev/sdb2,    if=none,id=u7,format=raw,readonly=on -device nvme-ns,drive=u7,bus=nvme0,nsid=7 \
  -serial file:/tmp/usb.log -display none
```

> ⚠️ `format=raw` 必须显式写。若该分区在宿主上**已挂载**，`readonly=on` 的 `O_RDONLY` 打开
> 仍然安全（多读者无冲突），但**绝不要**去掉 `readonly`：宿主与来宾同时写同一分区会毁数据。
> 另外 **NTFS 无法挂载**（没有 NTFS 驱动），这类盘会被卷层识别成一个「未知类型」卷，
> 各服务都不会认领它 —— 既不会挂上，也不会写坏。

**块请求语义（M6c）**：`BlockReq.count` 单位是 512 B 扇区。单条 NVMe 命令上限 **256 扇区
（128 KiB，`NVME_MAX_SECTORS`）**；`count` 超过 256 时由 block_srv 按 256 扇区**切段**并连续提交，
故调用方缓冲必须是**逐页映射的连续虚拟区间**（多页段首地址页对齐）。段内多页传输由 block_srv
组织 PRP：1 页用 PRP1、2 页 PRP2 直指第 2 页、> 2 页则写 PRP 表页（逐页 `SYS_VIRT_TO_PHYS` 反查）。

`build/mfs.img` 由 Makefile 用 `dd` 生成空白盘；超级块由 `mfs_srv` 首次挂载时写入
（自动格式化）。**当前格式为 MFS6**（空闲位图 + 空间回收 + 文件间接块 + 变长目录项/长名 +
节点元数据 + inode 号间接层/硬链接）：盘上是旧格式（`MFS1`…`MFS5` 或未知 magic）时，
首次挂载会**自动重新格式化**，旧数据不再保留。**删掉 `build/mfs.img` 即回到全新盘**：

```bash
rm -f build/mfs.img && make build/mfs.img    # 重新生成空白盘
```

`build/ext2.img` 由宿主 `mke2fs` 预格式化，并用 `debugfs` 预置测试文件
（`HELLO.TXT`、`SUBDIR/NESTED.TXT`）；`ext2_srv` **只读、不自动格式化**，
删掉后重新生成即可回到干净镜像：

```bash
rm -f build/ext2.img && make build/ext2.img  # 重新生成 ext2 镜像
```

`build/exfat.img` 由宿主 `mkfs.exfat -L MORIONUSB` 预格式化（16 MiB，默认 4 KiB 簇）；
`exfat_srv`（域 13）**不自动格式化**，签名 / boot checksum 不符即拒绝挂载
（打印 `exfat: mount FAILED vol=… stage=…`）。**读写均支持**（`CREAT/WRITE/MKDIR/UNLINK/RMDIR/TRUNCATE`；
`rename`/`chmod`/`link` 不支持），app 的 FS-16 自测会在结束时把卷清空，故每轮回归后
卷内容回到「只有系统项与卷标」。镜像里**不含预置文件**（当前宿主环境无免密 loop 挂载，无法预置）。
删掉后重新生成即可回到干净镜像：

```bash
rm -f build/exfat.img && make build/exfat.img   # 重新生成 exFAT 镜像
fsck.exfat -n build/exfat.img                   # 宿主校验镜像完好 (回归后应为 clean)
```

**大容量 / 大簇验证（M6c）**：默认 16 MiB 卷只有 4 KiB 簇、位图 448 B，覆盖不到大簇与大位图
路径。要验证「块层多页 DMA + exFAT 去上限」，用 `EXFAT_MIB` / `EXFAT_CLU` 生成大卷
（例如 2048 MiB + 32 KiB 簇 = 位图 8184 B > 4 KiB；FS-16 的 100000 字节写会跨多页 DMA）：

```bash
rm -f build/exfat.img && make build/exfat.img EXFAT_MIB=2048 EXFAT_CLU=32K
# 启动前确认参数: 挂载日志应打印 exfat-dbg: … cluster=32768 … bitmap=8184 …
fsck.exfat -n build/exfat.img                   # 回归后仍应为 clean
```

> ⚠️ Make 的镜像目标是 `$(EXFAT_IMG)` 展开后的路径，自定义路径时目标名也要跟着写
> （如 `make /tmp/big.img EXFAT_IMG=/tmp/big.img EXFAT_MIB=2048 EXFAT_CLU=32K`）。

`build/parts.img` 是分区测试盘：宿主用 `sfdisk` 写 MBR 两个主分区（起点 2048 的 FAT32、
起点 34816 的 ext2），分区内容先在独立小镜像上 `mkfs` 再 `dd` 进去。它用于验证
block_srv 卷层的**分区解析 + 类型探测**，删掉后重新生成即可：

```bash
rm -f build/parts.img && make build/parts.img  # 重新生成分区测试盘
```

---

## 3. 无头验证（推荐流程）

无显示器时用 `-serial file:` 把内核输出（内核日志与用户态 `SYS_PUTS` 都镜像到 COM1）
落盘，再用 `grep` 判定成败。

```bash
# 1) 构建
make iso

# 2) 无头启动 (display none + 串口落盘 + monitor socket 备用)
rm -f build/s.log
qemu-system-x86_64 -machine q35 -m 2G \
  -bios /usr/share/edk2/x64/OVMF.4m.fd \
  -cdrom build/morion-os.iso \
  -device nvme,serial=MORION,id=nvme0 \
  -drive file=build/nvme.img,if=none,id=nvme0n1,format=raw \
  -device nvme-ns,drive=nvme0n1,bus=nvme0,nsid=1 \
  -drive file=build/mfs.img,if=none,id=nvme0n2,format=raw \
  -device nvme-ns,drive=nvme0n2,bus=nvme0,nsid=2 \
  -drive file=build/ext2.img,if=none,id=nvme0n3,format=raw \
  -device nvme-ns,drive=nvme0n3,bus=nvme0,nsid=3 \
  -drive file=build/parts.img,if=none,id=nvme0n4,format=raw \
  -device nvme-ns,drive=nvme0n4,bus=nvme0,nsid=4 \
  -drive file=build/exfat.img,if=none,id=nvme0n5,format=raw \
  -device nvme-ns,drive=nvme0n5,bus=nvme0,nsid=5 \
  -vga virtio -no-reboot -display none \
  -serial file:build/s.log \
  -monitor unix:/tmp/morion-mon.sock,server,nowait

# 3) 判定 (进程跑约 20s 后)
grep -cE "FAILED|PANIC" build/s.log     # 必须为 0
grep -n "shell: type 'help'" build/s.log # 出现即已进 shell
```

**回归脚本（推荐直接用，省去手写上面那段）**：

```bash
bash scripts/fs-regress.sh                   # 5 张模拟镜像跑全量自测 → /tmp/morion-fs-regress.log
bash scripts/usb-ro.sh /dev/sda1 /dev/sda2   # 真盘只读端到端 (前后比对盘头 sha256)
```

两者都用**退出码**表示结果（`0` = 通过），并自带日志落盘与失败明细输出。
`fs-regress.sh` 会轮询日志直到出现 `SELFTEST DONE` / `FAILED` / `PANIC` ——
别用几十秒的超时去判失败（见下）。`usb-ro.sh` 把每个参数设备以 `readonly=on` 作
namespace 6, 7, 8… 接入，跑之前需要先给当前用户**读**权限：

```bash
sudo chgrp $(id -gn) /dev/sda1 /dev/sda2 && sudo chmod 440 /dev/sda1 /dev/sda2
```

> 授权是临时的，拔插设备即恢复；重插后**盘符会变**，先用 `lsblk` 确认设备名。

**判定约定**：正常路径不打日志；只有**失败**才打印一行诊断。因此
`grep -cE "FAILED|PANIC"` 为 `0` 且能看到 `shell: type 'help' for commands` 即通过。
app 的 FS 自测（FS-1..FS-18）成功时几乎静默（末尾打印一行 `app: SELFTEST DONE` 便于确认跑完），
故「无 FAILED」即代表挂载与读写自测全通
（ext2 挂载失败会打印 `ext2: mount FAILED ...`，exFAT 打印 `exfat: mount FAILED ...`）。
**FS-17（M1b）** 是唯一验证**额外卷**的用例：它从卷表里取出 `parts.img` 两个分区的卷号，
拼出 `/usb3` / `/usb4`，读回宿主预置的 `PART1.TXT` / `PART2.TXT`（校验前 10 字节 == `partition `），
**全程只读**；失败会打印 `app: FS17 … FAILED`。
**FS-18** 是 fat32 的**大文件写路径**用例：写 100000 字节跨簇、逐簇读回校验、UNLINK 释放簇链
（4 KiB 簇跨 25 簇、32 KiB 簇跨 4 簇），补上之前只有小文件 mkdir/creat/write 的缺口。

> ⏱️ **自测整套约需 3.5~4 分钟**（约 2 万个块请求，IPC 一跳 ≈ 一个时钟 tick，故有效吞吐 ~100 请求/s）。
> 期间日志会长时间「只有 shell 提示符、没有新行」，**这不是卡死** —— 别用几十秒的超时去判定失败，
> 请给足 ≥ 300 s（脚本内先 `grep 'SELFTEST DONE'` 再判）。`mfs-dbg`/`exfat-dbg` 行在挂载后立刻出现，
> 之后到 `SELFTEST DONE` 之间的静默属正常。
`mfs-dbg: vol=… total=… free=… gen=…` 一行给出 MFS 挂载后的空间状态，可用来确认空间回收是否生效
（`free` 接近 `total`、`gen` 逐次启动单调增长）；`exfat-dbg: vol=… cluster=… clusters=… root=… bitmap=… upcase=…`
一行给出 exFAT 挂载后的卷参数，用于与宿主 `mkfs.exfat` 的参数对齐核对。
**exFAT 写路径**另用宿主 `fsck.exfat -n build/exfat.img` 交叉验证：回归后应为
`clean. directories 1, files 0`（写盘的位图 / FAT / entry set 一致性由 exfatprogs 独立判定）。

### 键盘注入（monitor socket）

QEMU **必须带** `-monitor unix:...,server,nowait`（否则后台运行无 monitor 可用）。
用 Python 连 socket 发 `sendkey`（无 `socat`/`nc` 依赖）：

```python
import socket, time
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM); s.connect('/tmp/morion-mon.sock')
time.sleep(0.8)
def cmd(c, d=0.5):
    s.sendall((c + '\n').encode()); time.sleep(d)
cmd('sendkey shift', 0.7); cmd('sendkey shift', 0.7)   # 吸收前 1~2 个键偶发丢失
for k in ['l','s','spc','slash','m','f','s']:          # 输入 `ls /mfs`
    cmd('sendkey ' + k)
cmd('sendkey ret', 1.2)
s.close()
```

- 键名：字母/数字直接用；`spc`=空格、`slash`=`/`、`ret`=回车、`minus`=`-`、`dot`=`.`。
- 每个键用**独立** `sendkey`（不能用 `-` 连接成和弦，那会同时按下）。
- 首键偶发丢失属正常，发 1~2 个 `shift` 垫掉即可。

### 截图（无头下看画面）

```python
s.sendall(b'screendump build/screen.ppm\n')
```

得到 PPM，可用 `python3 -c "from PIL import Image; Image.open('build/screen.ppm').save('build/screen.png')"`
转成 PNG 查看（验证 LOGO / 终端外观时用）。

---

## 4. 纪律（踩过的坑）

1. **不要用 `pkill -f qemu-system-x86_64`**：该模式会匹配到执行它的 shell 自身命令行，
   把当前 shell 一起杀掉（表现为命令无故 exit -1）。停 QEMU 用工具提供的停止命令，
   或先 `pgrep` 取 PID 再按 PID kill。
2. **同一条命令里清理与启动要分开**：残留的 QEMU 会持有 `build/nvme.img` / `build/mfs.img`
   的写锁，导致新 QEMU 报 `Failed to get "write" lock`。先确认无残留再启动。
3. **后台启动 QEMU 必须显式给 monitor**：不加 `-monitor` 时 QEMU 会尝试占用 stdio 而退出。
4. **改用户态代码后必须 `make iso`**（内核嵌入 `user.bin`）。
5. 验证只能用**自动可判定**的方式（`grep` 关键字 / 截图），不要依赖人工肉眼看屏。
