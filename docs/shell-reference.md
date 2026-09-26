# Morion OS — Shell 命令规范

Shell 是域 8 的用户态进程（[user/src/main.rs](../../user/src/main.rs)），通过 libvfs
经 `mount_srv` 路由到各文件服务。本文件是 shell 命令的**行为契约**：新增/修改命令时先改这里。

---

## 1. 提示符与生命周期

```text
shell: type 'help' for commands
[morion@morion <cwd>]$ <用户输入>
```

- 提示符 = `[morion@morion ` + 当前工作目录 + `]$ `（无空格分隔的 `]` 与 `$`）。
- 提示符与用户输入在**同一行**编辑；内核终端会记录「输入起点」，回车只提交用户输入段，
  不把提示符当作命令。
- 启动时 cwd = `/`。
- `sys_readline` 阻塞读一行；读失败打印 `shell: readline FAILED` 并退出。
- 每行只解析**第一个空格**：`cmd` = 空格前，`arg` = 其余部分（两侧去空白）。
  因此参数内可含空格（如 `echo a b` 输出 `a b`）。

---

## 2. 命令总表

| 命令 | 语法 | 语义 |
| --- | --- | --- |
| `help` | `help` | 打印命令列表与挂载点 |
| `echo` | `echo <text>` | 原样打印 `<text>`（可含空格） |
| `pwd` | `pwd` | 打印当前工作目录 |
| `ls` | `ls [path]` | 列目录，默认当前目录 |
| `cat` | `cat <file>` | 打印文件内容（最多 4096 字节） |
| `cd` | `cd [path]` | 切换工作目录，默认 `/` |
| `mkdir` | `mkdir <path>` | 创建目录 |
| `touch` | `touch <file>` | 创建空文件（已存在则等价打开，不报错） |
| `rm` | `rm <path>` | 删除文件；失败则按**空目录**删除 |
| `mv` | `mv <src> <dst>` | 重命名 / 移动（同一次请求内跨目录；**不支持跨文件系统**） |
| `ln` | `ln <file> <new-name>` | 给已有文件再加一个名字（**硬链接**；同文件系统、仅限文件） |
| `ln -s` | `ln -s <target> <link-name>` | 建**软链接**（M5c；仅 MFS；目标原样存，可悬空） |
| `chmod` | `chmod <octal-mode> <path>` | 设置权限位（仅 MFS 提供；只存储与显示，**不强制**） |
| `truncate` | `truncate <file> <size>` | 把文件截断/扩展到 `<size>` 字节（扩展为稀疏） |
| `stat` | `stat <path>` | 打印权限 / 属主 / 链接数 / 大小 / 时间（**跟随**软链接） |
| `lstat` | `lstat <path>` | 同 `stat`，但作用于**链接自身**（不跟随；悬空链接也能看） |
| `readlink` | `readlink <link>` | 打印软链接的目标字符串 |
| `mkfs.mfs` | `mkfs.mfs <vol>` | 在指定卷上创建 MorionFS（**擦除**该卷；只接受空白卷或已有 MFS 卷） |
| `mfs.primary` | `mfs.primary <vol>` | 把一块已有数据的 MFS 卷换为主卷（**不动数据**；只接受 MFS 卷） |
| `df` | `df` | 报告 `/mfs`（MorionFS）的空间用量 |
| `clear` | `clear` | 清屏并复位历史/光标/回滚状态 |

命令名**区分大小写**（须全小写）；未知命令打印 `shell: unknown command: <cmd>`。
空行（去空白后为空）直接忽略。

### `help` 输出（当前实现）

```text
commands:
  help           show this help
  echo <text>    print text
  pwd            print working directory
  ls [-l] [path] list directory (-l: long form)
  cat <file>     print file content
  cd [path]      change directory (default: /)
  mkdir <path>   create directory
  touch <file>   create empty file
  rm <path>      remove file / empty directory
  mv <src> <dst> rename / move (same filesystem)
  ln <src> <dst> hard link an existing file (same filesystem)
  ln -s <target> <name>  symbolic link (target kept verbatim; MFS only)
  chmod <mode> <path>  set permission bits (octal, display-only)
  truncate <file> <size>  resize a file (sparse on grow)
  stat <path>    show metadata (mode / owner / links / times)
  lstat <path>   like stat but on the link itself (no follow)
  readlink <link>  print a symbolic link's target (no follow)
  mkfs.mfs <vol>   create a MorionFS filesystem on a volume (ERASES it)
  mfs.primary <vol>   mark a MorionFS volume primary (keeps data)
  df             show MorionFS space usage (/mfs)
  clear          clear screen
  (mounts: / = fat32, /tmp = tmpfs, /mfs = MorionFS, /ext2 = ext2 ro, /usb = exFAT)
  (extra volumes auto-mounted as /usb<N>, N = volume id in the boot volume list)
```

---

## 3. 逐命令细节与错误信息

约定的输出格式：**成功也打印一行**（便于交互确认），失败打印一行诊断；
诊断一律以 `<命令>: ` 开头。

### `ls [-l] [path]`

- 短格式每条目一行：目录 `[DIR]  NAME`，文件 `[FILE] NAME  size=<字节>`。
- `-l` 长格式：`<类型+权限> owner=<域id> links=<n> size=<宽度8>  <YYYY-MM-DD HH:MM>  NAME`。
  权限串形如 `-rw-r--r--`（目录首位为 `d`）；元数据由文件服务的 `readdir` 一并回传，
  故 `-l` **不产生额外 IPC**。非 MFS 的服务只填默认值（权限按目录/文件给 0755/0644、
  属主 0、链接数 1、时间 `(unknown)`）。
- 名字优先用**长名**（VFAT LFN / ext2 名字 / MFS 名字），没有则回退 8.3 短名。
- 错误：
  - `ls: path too long` — 拼出的绝对路径超过内部缓冲
  - `ls: cannot open <path>` — 路径不存在 / 服务未挂载
  - `ls: not a directory: <path>` — 打开的是文件

### `cat <file>`

- 无参数：`cat: missing file operand`
- 打开失败：`cat: cannot open <path>`
- 读取失败（例如目标是目录）：`cat: read failed (is it a directory?)`
- 内容中不可打印字节显示为 `.`。

### `cd [path]`

- 无参数回到 `/`。
- 目标须是**已存在的目录**：
  - `cd: no such directory: <path>`
  - `cd: not a directory: <path>`
- 成功后 `pwd` 反映新目录；`cd /mfs` 后提示符变为 `[morion@morion /mfs]$`。

### `mkdir <path>` / `touch <file>`

- 无参数：`mkdir: missing operand` / `touch: missing operand`
- 成功：`mkdir: created <path>` / `touch: created <path>`
- 失败：`mkdir: failed (exists or bad parent): <path>` / `touch: failed (bad parent?): <path>`
- `touch` 对已存在文件不报错（`creat` 语义）。

### `rm <path>`

- 无参数：`rm: missing operand`
- 先 `unlink`：成功打印 `rm: removed <path>`
- 失败再试 `rmdir`（空目录）：成功打印 `rm: removed directory <path>`
- 都失败：`rm: failed (not found, or directory not empty): <path>`

### `mv <src> <dst>`

- 语义：把 `<src>` 改名 / 移到 `<dst>`。**同一次请求内可跨目录**，但两个路径必须落在
  同一个文件服务（跨挂载点的搬迁不支持）。
- 目标已存在且是文件 → **覆盖**；目标是非空目录、或类型不匹配（文件 ↔ 目录）→ 拒绝。
- 目录不能被移进自己的子孙（会形成环）→ 拒绝。
- 用法错误：`mv: usage: mv <src> <dst>`
- 失败：`mv: failed (cross-fs, bad target, or directory loop): <src>`

### `ln <src> <dst>`

- 给已存在的**文件** `<src>` 再加一个名字 `<dst>`（硬链接）。两个名字指向同一个 inode：
  从任一个名字改写内容，另一个名字都会看到（`ls -l` 的 `links=` 显示链接数）。
- 仅限**同一文件系统**（跨挂载点不支持）；目录不能硬链接（避免成环）；`<dst>` 必须不存在。
- 成功打印 `ln: <dst> -> <src>`；用法错误：`ln: usage: ln <existing-file> <new-name>`。
- 失败：`ln: failed (needs an existing file, new name, same fs): <src>`。

### `ln -s <target> <link-name>`

- 建**软链接**（M5c，仅 MFS 支持）：存的是**目标路径字符串**，解析时才解释。
- `<target>` **不做路径解析**，原样存进链接节点：
  - 以 `/` 开头 = 绝对路径。落在**同一挂载点内**时前缀会被剥掉
    （`ln -s /mfs/a /mfs/b` 存下的是 `/a`）；指到**别的文件系统**（或没有挂载点的路径）
    时**直接拒绝创建** —— 跨文件系统的软链接不支持，存成悬空链接只会让人分不清。
  - 否则 = 相对**链接所在目录**（不是相对当前目录）。
- 目标**不必存在**（可以之后才创建，此时链接悬空：`ls -l` 能看到它，`cat`/`open` 会失败）。
- 跟随语义：`cat` / `stat` / `open` 会**跟随**到目标；`rm` / `mv` / `rmdir` 作用于**链接自身**
  —— `rm link` 只摘掉链接，目标文件不受影响；`rmdir <指向目录的链接>` 会失败（它不是目录条目）。
  链接互相指、或指向自己时会**解析失败**（限深 16 层），不会挂死。
- `ls -l` 里显示为 `lrwxrwxrwx`，`size=` 是**目标字符串长度**（不是目标文件大小）。
  要看它指向哪里用 `readlink`，要看链接自身的元数据用 `lstat`。
- 成功打印 `ln -s: <link-name> -> <target>`；用法错误：`ln: usage: ln -s <target> <link-name>`。
- 失败：`ln -s: failed (name exists, target empty/too long, or no MFS): <link-name>`。

### `chmod <octal-mode> <path>`

- `<octal-mode>` 是八进制权限（如 `644`、`755`、`1777`，低 12 位有效）。
- 成功打印 `chmod: mode=<十进制> <path>`；非法的 mode：`chmod: bad mode: <mode>`。
- 失败：`chmod: failed: <path>`。
- **当前只存储与显示，不做访问判定**（系统还没有多用户概念）。非 MFS 的服务不支持，
  会返回失败。

### `truncate <file> <size>`

- 把文件长度改成 `<size>` 字节。截短会释放尾部数据块；扩展是**稀疏**的 ——
  未写过的区间读回全 0。
- 成功打印 `truncate: size=<n> <path>`；非法 size：`truncate: bad size: <size>`。
- 失败（目录 / 无法打开）：`truncate: failed (directory or bad file): <path>`。
- 只支持 MFS。
- **S3b 起 size 是 64 位**（不再卡在 `u32::MAX`）：MFS 上可以截到 **>4 GiB**（如 5 GiB）并稀疏扩展，
  `stat` / `ls -l` 也会如实报出 64 位大小；跨 4 GiB 的读写走文件的**三级间接块**。

### `stat <path>`

- 打印 `File / Type / Mode / Owner / Links / Size / Modify / Change` 各行。
- 时间格式 `YYYY-MM-DD HH:MM`（UTC，来自 CMOS RTC）；未知时间打印 `(unknown)`。
- 失败：`stat: cannot stat <path>`。
- 非 MFS 的服务返回默认值（权限 0755/0644、属主 0、链接数 1、时间未知）。
- **跟随软链接**：`stat link` 打印的是目标文件的信息。

### `lstat <path>`

- 与 `stat` 同一套输出，区别是**不跟随**末段软链接 —— 打印链接自身：`Type: symbolic link`、
  `Size` = 目标字符串长度。
- 悬空链接 `stat` 会失败（`stat: cannot stat <path>`），`lstat` 仍能正常看到它是链接。
- 失败：`lstat: cannot stat <path>`（与 `stat` 只差前缀）。

### `readlink <link>`

- 只打印软链接的目标字符串（**不跟随**）—— 与 Unix `readlink` 一致，只有目标一行，
  不加 `link -> ` 前缀。返回的是建链时存下的那段路径，客户端已把挂载前缀加回
  （服务端存 `/a`，这里显示 `/mfs/a`）。
- 路径不是软链接（或为悬空链接之外的一般失败）：`readlink: not a symbolic link: <link>`。
- 缺参数：`readlink: missing operand`；路径过长：`readlink: path too long`。

### `mkfs.mfs <vol>`

在**卷号**为 `<vol>` 的卷上写一个全新的 MorionFS 文件系统。**会擦除该卷原有内容。**

- `<vol>` 取自块服务启动时打印的卷表：每卷一行
  `vol: <卷号> nsid=<n> lba=<n> sectors=<n> kind=<名>`（`kind=unknown` 表示该卷没有文件系统）。
- **护栏在 mfs_srv 里，不在 shell 里**：只接受 `kind=mfs`（重新格式化）或
  `kind=unknown`（未格式化）的卷。FAT / exFAT / ext2 等别人的分区与不存在的卷号一律被拒，
  **绝不自动吞掉**。命令与 FS-22 自测走同一条路径，判定只有一处。
- 成功后：卷上有一个空 MorionFS 根目录；若该卷**不是** MFS 主卷，会立刻挂到 `/usb<卷号>`；
  若就是主卷（`/mfs`），原地重建。
- **同时把这卷标记为主卷**：超级块里记下「主卷序号 = 现有最大 + 1」，于是**下次启动** `/mfs`
  就是它 —— 这就是「切换主卷」的手段，与卷表扫描顺序无关。序号只增不减，所以重复格式化
  同一块卷它仍会胜出。本次运行的挂载点不变（换主卷要重启才生效）。
- 输出：`mkfs.mfs: volume <vol> formatted, marked primary (serial <n>) -> /mfs after next boot;
  other volumes mount at /usb<volume-id>`（`serial` 是**从盘上回读**的序号，即为标记已落盘的证据）；
  被拒或失败：`mkfs.mfs: refused volume <vol> (not blank, not MFS, or no such volume)`。
- 缺参数 / 非数字：`mkfs.mfs: usage: mkfs.mfs <volume-id>   (see the 'vol:' lines in the boot log)`。
- 相关诊断（服务端打印，属正常护栏证据）：`mfs: mkfs refused (volume holds another filesystem)`、
  `mfs: mkfs refused (no such volume)`、`mfs: mkfs OK but primary mark missing on disk`（异常）。

> 典型用法（真盘上「新买一块盘」）：启动日志里找到目标卷的 `vol:` 行（例如
> `vol: 6 nsid=6 lba=0 sectors=32768 kind=unknown`），敲 `mkfs.mfs 6`，然后
> `ls /usb6` / `touch /usb6/T.TXT`。此后这台机器重启，`/mfs` 就落在卷 6 上。
> 想切回去，用 `mfs.primary 1`（**不动数据**）；只有在确实想重建卷 1 时才用
> `mkfs.mfs 1`（那会**擦除**卷上的文件）。

> ⚠️ 主卷标记是**持久**的：跑过自测的镜像上，`spare.img` 会被 FS-22/FS-24/FS-25 格成 MFS
> 并逐步升到更高序号 —— 再次 `make run-nvme` 而不重置镜像时，`/mfs` 就落在那块 16 MiB 的
> 空白盘上（`fs-regress.sh` 每轮都会重置两份卷，故回归不受影响）。想复位就用 `mfs.primary 1`
> 把主卷换回卷 1（不动数据），或删掉 `build/spare.img` 重新来。参考 [roadmap-fs.md](roadmap-fs.md) 的「S2 补齐」。

### `mfs.primary <vol>`

把**已经有数据**的 MorionFS 卷 `<vol>` 升为主卷 —— **不改变卷上的一个字节**。

- 与 `mkfs.mfs <vol>` 的分工：`mkfs` 建新文件系统、会**擦除**卷上的文件；本命令只改超级块里
  的主卷序号。把它俩分开，是因为「把一块已有数据的盘升为主卷」在 `mkfs` 下等同于删数据。
- **只接受已经是 MFS 的卷**（读得出有效超级块），空白盘 / FAT / exFAT / ext2 / 不存在的卷号
  一律被拒 —— 这里**没有**「格式化兜底」，因为目标卷上放着用户的文件。
- 序号取「现有最大 + 1」（与 `mkfs` **共用**同一个只增计数器），故它同样只增不减。**下次启动**
  `/mfs` 就认领到这块卷；本次运行的挂载点不变（换主卷要重启才生效）。
- 输出：`mfs.primary: volume <vol> marked primary (serial <n>) -> /mfs after next boot;
  data on it was NOT touched`（`serial` 是**从盘上回读**的序号，即为标记已落盘的证据）；
  被拒或失败：`mfs.primary: refused volume <vol> (not a MorionFS volume, or no such volume)`。
- 缺参数 / 非数字：`mfs.primary: usage: mfs.primary <volume-id>   (see the 'vol:' lines in the boot log)`。
- 相关诊断（服务端打印）：`mfs: set-primary refused (no such volume)`、
  `mfs: set-primary refused (not a MorionFS volume)`、`mfs: set-primary OK but primary mark missing on disk`（异常）。

### `df`

报告**已挂载文件系统**的空间用量。目前只有 `/mfs` 一行 —— **MorionFS 是唯一维护块分配、
报得出容量的服务**（FAT32 / tmpfs / ext2 / exFAT 不维护空闲位图，没有「容量」可报）。

- 数字来自 mfs_srv 的内存态（`MSST` tag）：`MSST` **不带卷参数**，服务端按**默认卷 = 主卷**
  取数，所以这里报的就是 `/mfs`，与 `/usb<卷号>` 上那些额外 MFS 卷无关。
- 单位是 **4 KiB 块**（与 MFS 内部块一致）：输出
  `df: /mfs (MorionFS): total <T> blocks, used <U>, free <F> (<P>% used; 1 block = 4 KiB)`，
  其中 `U = T - F`、`P` 为整数百分比（`U * 100 / T`，向下取整）。
- 带参数：`df: usage: df   (only MorionFS reports capacity; other filesystems do not)`。
- 查询失败（MorionFS 未挂载）：`df: /mfs unavailable (MorionFS not mounted?)`。

### `clear`

调用 `SYS_CLEAR`：清屏 + 复位历史环形缓冲 / 输入行 / 光标 / 回滚偏移，
并用背景渐变重铺整屏。

---

## 4. 路径规则

1. **绝对路径**：以 `/` 开头，直接使用。
2. **相对路径**：相对当前 cwd 拼接（`cwd + "/" + arg`）。
3. **归一化**：结果一律以 `/` 开头；空段与 `.` 丢弃；`..` 回退一级，**已在根时保持根**；
   重复 `/` 与结尾 `/` 折叠。例：
   - cwd=`/mfs`，`ls ../tmp` → `/tmp`
   - cwd=`/`，`cd ..` → `/`
4. **名字匹配**由目标文件服务决定，shell 只做路径拆分：
   - FAT32：段先按 **8.3 短名**（主名 ≤ 8、扩展名 ≤ 3，**自动转大写**，`readme.txt` 等价 `README.TXT`）
     匹配；未命中再按 **VFAT 长名**（ASCII 大小写不敏感）回退——因此带空格的长名段可直接书写，
     如 `cat "/dir1/Long File Name.txt"`（shell 无引号语法，整行空格即段内字符，命令名后第一段空格才分隔参数）。
   - tmpfs（`/tmp`）：仅支持 8.3 短名（自动转大写）。
   - ext2（`/ext2`）：按 ext2 语义**大小写敏感**精确匹配，未命中再做一次 ASCII 大小写不敏感回退。
   - exFAT（`/usb`）：按 exFAT 语义**ASCII 大小写不敏感**匹配（名字为 UTF-16 → UTF-8，shell 只显示 ASCII）。
5. 路径总长超过 `CWD_MAX + SHELL_LINE_MAX` 时返回 `path too long`。

---

## 5. 挂载点与路由（`/` 是统一目录树）

应用只看到单一根 `/`，由 `mount_srv` 做**最长前缀匹配**路由：

| 挂载点 | 服务域 | 说明 |
| --- | --- | --- |
| `/tmp` | tmpfs_srv (10) | 纯内存文件系统 |
| `/mfs` | mfs_srv (11) | MorionFS（块设备后端，可持久化） |
| `/ext2` | ext2_srv (12) | ext2 **只读**（NVMe `nsid=3`，宿主预格式化的既有分区） |
| `/usb` | exfat_srv (13) | exFAT（**读 + 写**；NVMe `nsid=5`，宿主 `mkfs.exfat` 预格式化） |
| `/` | fat32_srv (6) | FAT32（NVMe `nsid=1`） |
| `/usb<卷号>` | fat32_srv (6) / ext2_srv (12) / exfat_srv (13) / mfs_srv (11) | **额外卷（M1b）**：各服务把自己那类的非默认卷自动挂到这里（`<卷号>` = block_srv 卷表里的 id，如 `/usb3` = FAT32 分区、`/usb4` = ext2 分区、`/usb6` = `mkfs.mfs` 格出来的 MFS 卷）。卷层探测不出文件系统的空白卷**不会**被自动挂载 —— MFS 的空白卷要先 `mkfs.mfs <卷号>`。 |

- 匹配是**组件边界敏感**的：`/tmpfoo` **不会**匹配到 `/tmp`，而会落到 `/`。
- 跨服务操作需显式写路径：`cp` 之类命令尚未提供，`cat /mfs/X` 与 `ls /tmp` 各自路由。
- `/ext2` 是只读挂载：`touch`/`mkdir`/`rm` 等写命令会失败（`ext2_srv` 对写请求一律拒绝）。
  `/usb`（exFAT）**可写**；但 `mv`（rename）与 `ln`（硬链接）在该服务上不支持，会失败。
- `/usb<卷号>` 下各服务仍遵守自己的写能力：挂到 `ext2_srv` 的额外卷**只读**，挂到 `fat32_srv`/`exfat_srv` 的可写。
  接入真实 U 盘时建议以 `readonly=on` 打开块设备（见 [commands.md](commands.md#接入真实-u-盘只读)），
  此时写入会在块层失败 —— 这是预期的保护行为，不是 bug。

### 例（NVMe 五盘下）

```text
[morion@morion /]$ ls
...
[morion@morion /]$ ls /mfs
[FILE] PERSIST.TXT  size=6
[morion@morion /]$ cd /mfs
[morion@morion /mfs]$ mkdir D
mkdir: created /mfs/D
[morion@morion /mfs]$ touch D/F.TXT
touch: created /mfs/D/F.TXT
[morion@morion /mfs]$ rm D/F.TXT
rm: removed /mfs/D/F.TXT
[morion@morion /mfs]$ cd /ext2
[morion@morion /ext2]$ ls
[DIR]  lost+found
[FILE] HELLO.TXT  size=48
[DIR]  SUBDIR
[morion@morion /ext2]$ cat HELLO.TXT
Hello from ext2!
This is a read-only test file.
[morion@morion /ext2]$ cd /usb
[morion@morion /usb]$ ls                  # 空 exFAT 卷: 只含系统项与卷标, 故无输出
[morion@morion /usb]$ touch A.TXT
touch: created /usb/A.TXT
[morion@morion /usb]$ ls
[FILE] A.TXT  size=0
[morion@morion /usb]$ rm A.TXT
rm: removed /usb/A.TXT
```

---

## 6. 输出与终端约定

- **正常运行期不打印非必要日志**（缺页、IPC、键盘、驱动事件都静默），
  以免打断 shell 提示符；仅**失败**时打印一行诊断。
- 各域（app / shell / 服务）共用同一控制台：`SYS_PUTS` 与内核 `video::print`
  都走同一输出通道，并镜像到 COM1。
- 打印会追加到当前输入行末尾，`\n` 提交为历史行；超过列宽自动换行提交。
- ↑ / ↓ 在历史区与输入行间移动光标，到顶/底再按触发回滚（`HISTORY_LINES = 512` 行环形缓冲）。
