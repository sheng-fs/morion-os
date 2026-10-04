# Morion OS — Shell 命令规范

Shell 是域 8 的用户态进程（[user/srv/src/shell.rs](../../user/srv/src/shell.rs)），通过 libvfs
经 `mount_srv` 路由到各文件服务。本文件是 shell 命令的**行为契约**：新增/修改命令时先改这里。

---

## 1. 提示符与生命周期

```text
shell: type 'help' for commands
[morion@morion <cwd>]$ <用户输入>
```

- 提示符 = `[morion@morion ` + 当前工作目录 + `]$ `（无空格分隔的 `]` 与 `$`）。
- 提示符与用户输入在**同一行**显示：shell 用 `print` 打印提示符后 `flush()`，随后由
  [`morion::console::readline`](../user/libmorion/src/console.rs) **就地回显**键入字符（G4 起行编辑在客户端）。
- 启动时 cwd = `/`。
- `morion::console::readline` 阻塞取键读一行（回车提交，退格就地擦除）；读失败（返回 `u64::MAX`）
  打印 `shell: readline FAILED` 并退出。**没有屏幕控制台就没有回显通道**，此时 shell 打印
  `shell: screen console unavailable (gfx_srv) - no input channel` 并退出。
- 每行只解析**第一个空格**：`cmd` = 空格前，`arg` = 其余部分（两侧去空白）。
  因此参数内可含空格（如 `echo a b` 输出 `a b`）。

---

## 2. 命令总表

| 命令 | 语法 | 语义 |
| --- | --- | --- |
| `help` | `help` | 打印命令列表与挂载点 |
| `echo` | `echo <text>` | 原样打印 `<text>`（可含空格） |
| `uname` | `uname` | 打印内核报告的整行系统标识 `MorionOS <release> <machine>`（如 `MorionOS 0.4.0 x86_64`）；串由内核 [`kernel/src/version.rs`](../../kernel/src/version.rs) 单一维护，经 `SYS_UNAME(51)` 取得；无图形变体（`make NOGUI=1`）下 release 带 `-nogui` 后缀（如 `0.4.0-nogui`） |
| `version` | `version` | 打印 `MorionOS v<release> (build <构建号>)`，如 `MorionOS v0.4.0 (build 3183f3f)`；`<构建号>` = 构建时 git 短哈希，由 Makefile 注入 |
| `pwd` | `pwd` | 打印当前工作目录 |
| `ls` | `ls [path]` | 列目录，默认当前目录 |
| `cat` | `cat <file>` | 打印文件内容（最多 4096 字节） |
| `run` | `run <file>` | **从文件加载并运行一个程序**（E1/E2）：`morion::exec::spawn_file` 读入镜像 → 内核 `SYS_SPAWN_ELF` 载入**新域**；不等待它结束 |
| `wget` | `wget [path]` | 从**客户机内建 HTTP 服务**（`10.0.2.15:80`，独立服务 `httpd_srv` 域 23）取一页并打印：走完整 TCP 客户端路径（`tcp_socket` → `tcp_bind 12349` → `tcp_connect` → `tcp_send "GET <path> HTTP/1.0"` → `tcp_recv` → `tcp_close`），握手与数据经栈内**回环**与服务端完成，无需任何外部服务端；`path` 省略则默认 `/` |
| `net` / `ifconfig` | `net` | 打印各网卡链路状态：`nic<i> <型号> <up|down> mac=… ipv4=… gw=… [ipv6=… router=…]`（经 `morion::net::link_infos`） |
| `ping` | `ping <ipv4\|ipv6\|host>` | IPv4 走 ICMP echo（`morion::net::ping4`）；IPv6 字面量自动转 `ping6`；主机名先 `resolve`（先 A 后 AAAA） |
| `ping6` | `ping6 <ipv6\|host>` | IPv6 走 ICMPv6 echo（`morion::net::ping6`）；主机名先查 AAAA |
| `dns` / `nslookup` | `dns <name>` | 解析 `<name>` 的 A 与 AAAA 记录各打印一行（走 slirp 内置 DNS `10.0.2.3`） |
| `cd` | `cd [path]` | 切换工作目录，默认 `/` |
| `mkdir` | `mkdir <path>` | 创建目录 |
| `touch` | `touch <file>` | 创建空文件（已存在则等价打开，不报错） |
| `rm` | `rm <path>` | 删除文件；失败则按**空目录**删除 |
| `mv` | `mv <src> <dst>` | 重命名 / 移动（同一次请求内跨目录；**不支持跨文件系统**） |
| `ln` | `ln <file> <new-name>` | 给已有文件再加一个名字（**硬链接**；同文件系统、仅限文件） |
| `ln -s` | `ln -s <target> <link-name>` | 建**软链接**（M5c；仅 MFS；目标原样存，可悬空） |
| `chmod` | `chmod <octal-mode> <path>` | 设置权限位（仅 MFS；04b 起**参与访问判定**，仅属主或 uid 0 可改） |
| `chown` | `chown <uid>:<gid> <path>` | 改属主 / 属组（仅 MFS；只有 uid 0 能成功） |
| `truncate` | `truncate <file> <size>` | 把文件截断/扩展到 `<size>` 字节（扩展为稀疏） |
| `stat` | `stat <path>` | 打印权限 / 属主 / 链接数 / 大小 / 时间（**跟随**软链接） |
| `lstat` | `lstat <path>` | 同 `stat`，但作用于**链接自身**（不跟随；悬空链接也能看） |
| `readlink` | `readlink <link>` | 打印软链接的目标字符串 |
| `mkfs.mfs` | `mkfs.mfs <vol> [--force]` | 在指定卷上创建 MorionFS（**擦除**该卷；默认只接受空白卷或已有 MFS 卷，`--force` 才允许覆盖别人的文件系统） |
| `mfs.primary` | `mfs.primary <vol>` | 把一块已有数据的 MFS 卷换为主卷（**不动数据**；只接受 MFS 卷） |
| `mfs.snap` | `mfs.snap` | 给主卷 `/mfs` 拍一张 COW 快照（持久化在超级块里，环上限 8 条） |
| `mfs.snaps` | `mfs.snaps` | 列出现有快照（索引 / 代际 / 根 inode 表块） |
| `mfs.rollback` | `mfs.rollback <index>` | 把 `/mfs` 退回指定快照（只改根指针，不搬数据） |
| `mfs.gc` | `mfs.gc` | 回收不可达的 COW 旧块（快照仍引用的块保留） |
| `mfs.fsck` | `mfs.fsck [--repair]` | 对账「已分配但不可达」的 inode（默认只报不修，`--repair` 才回收） |
| `mfs.sync` | `mfs.sync` | 把 `/mfs` 显式落盘一次（刷新位图 + 超级块），打印盘上代际 gen |
| `df` | `df` | 报告 `/mfs`（MorionFS）的空间用量 |
| `part.create` | `part.create <nsid> <MiB> [mbr]` | 在**整块盘**上建分区（空白盘默认 GPT；`MiB 0` = 用尽剩余空间） |
| `part.del` | `part.del <nsid> <index>` | 删分区项（**不动数据**；删完最后一个则整张表清空） |
| `part.wipe` | `part.wipe <nsid>` | 清空分区表，盘回到「无分区表」 |
| `part.reload` | `part.reload` | 重读分区表（重建并打印卷表） |
| `clear` | `clear` | 清屏并复位历史/光标/回滚状态 |

命令名**区分大小写**（须全小写）；未知命令打印 `shell: unknown command: <cmd>`。
空行（去空白后为空）直接忽略。

### `help` 输出（当前实现）

```text
commands:
  help           show this help
  echo <text>    print text
  uname          print system name / release / machine
  version        print version and build
  pwd            print working directory
  ls [-l] [path] list directory (-l: long form)
  cat <file>     print file content
  run <file>     load a .mex program from a file and run it (new domain)
  wget [path]    fetch a page from the guest's built-in HTTP server (10.0.2.15:80)
  net            show link status (kind / mac / ipv4 / gw / ipv6)
  ping <ip|host> ICMP echo over IPv4 (IPv6 literal delegates to ping6)
  ping6 <ip|host>  ICMPv6 echo over IPv6
  dns <name>     resolve A and AAAA records (alias: nslookup)
  cd [path]      change directory (default: /)
  mkdir <path>   create directory
  touch <file>   create empty file
  rm <path>      remove file / empty directory
  mv <src> <dst> rename / move (same filesystem)
  ln <src> <dst> hard link an existing file (same filesystem)
  ln -s <target> <name>  symbolic link (target kept verbatim; MFS only)
  chmod <mode> <path>  set permission bits (octal; enforced since 04b)
  chown <uid>:<gid> <path>  change owner/group (MFS; uid 0 only)
  truncate <file> <size>  resize a file (sparse on grow)
  stat <path>    show metadata (mode / owner / links / times)
  lstat <path>   like stat but on the link itself (no follow)
  readlink <link>  print a symbolic link's target (no follow)
  mkfs.mfs <vol> [--force]  create a MorionFS filesystem on a volume (ERASES it)
  mfs.primary <vol>   mark a MorionFS volume primary (keeps data)
  mfs.snap       take a MorionFS snapshot (COW root + generation)
  mfs.snaps      list MorionFS snapshots (index / generation)
  mfs.rollback <index>  roll the mounted MorionFS back to a snapshot
  mfs.gc         reclaim unreachable MorionFS blocks (keeps snapshots)
  mfs.fsck [--repair]  reconcile MFS inodes (report only; --repair reclaims)
  mfs.sync       flush MorionFS to disk now (prints the on-disk generation)
  df             show MorionFS space usage (/mfs)
  part.create <nsid> <MiB> [mbr]  create a partition (disk-wide, blank = GPT)
  part.del <nsid> <index>  delete a partition entry (keeps data)
  part.wipe <nsid>   clear the partition table (disk becomes blank)
  part.reload      re-read all partition tables
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
- `-l` 长格式：`<类型+权限> owner=<域id> uid=<uid> gid=<gid> links=<n> size=<宽度8>  <YYYY-MM-DD HH:MM>  NAME`。
  权限串形如 `-rw-r--r--`（目录首位为 `d`）；元数据由文件服务的 `readdir` 一并回传，
  故 `-l` **不产生额外 IPC**。非 MFS 的服务只填默认值（权限按目录/文件给 0755/0644、
  `owner/uid/gid` 0、链接数 1、时间 `(unknown)`）。
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

### `run <file>`

从文件系统加载一个可执行文件并启动它（E1/E2 可执行文件加载）。**看内容不看后缀**：
镜像必须是 ELF64 `ET_EXEC`（`.mex`），由内核全量校验。

- 无参数：`run: usage: run <file>   (e.g. run /hello.mex)`
- 路径过长：`run: path too long`
- 成功：`run: loaded <path> -> new domain <n>`；紧接着子程序会打印它自己的输出
  （不等待它结束，shell 立即回到提示符）。
- 失败：`run: cannot load <path> (missing file, or not a valid ELF64 program)`

需要 shell 持有 `Capability::Spawn`（内核引导期授予）。当前镜像里可直接试：

```text
[morion@morion /]$ run /hello.mex
run: loaded /hello.mex -> new domain 14
exec: 我是运行时被加载的独立 ELF 程序 (morion-hello), 我的域 = 14, 入口 = 0x8000000000
[morion@morion /]$
```

### `net` / `ifconfig`

打印**每张网卡一行**链路状态（数据来自协议栈 `netstack_srv`，经 `NETS_OP_NETINFO`）：

```text
nic0 virtio-net up mac=52:54:00:12:34:56 ipv4=10.0.2.15 gw=10.0.2.2 ipv6=fec0:0:0:0:5054:ff:fe12:3456 router=fe80:0:0:0:0:0:0:2
nic1 e1000e up mac=52:54:00:aa:bb:cc ipv4=10.0.2.15 gw=10.0.2.2
```

- 字段：`nic<i>` = 网卡索引（与 `socket_on(nic)` 一致）；型号按内核 NIC 表的 kind 报出（`virtio-net` / `e1000e` / `e1000`）；
  `ipv4` / `gw` 是本机地址与网关；`ipv6` / `router` 仅在**该网卡完成 SLAAC（收到 RA）**后出现。
- 不需要端口能力（只发一条 `NETINFO` 请求），只需 shell 持 `SendTo(netstack_srv)`（内核已授）。
- 协议栈未起 / 没有链路：`net: no link (netstack unavailable?)`。

### `ping <ipv4|ipv6|host>`

ICMP echo（IPv4）/ ICMPv6 echo（IPv6）：

```text
[morion@morion /]$ ping 10.0.2.2
ping 10.0.2.2 ... reply
[morion@morion /]$ ping fe80::2
ping6 fe80:0:0:0:0:0:0:2 ... reply
```

- 参数是 **IPv6 字面量**时自动转 `ping6`；否则先按 IPv4 字面量解析，失败再 `resolve`（先 A 后 AAAA），解析到 v6 也转 `ping6`。
- 输出 `ping <addr> ... reply` / `... no reply`（`ping6` 同理，前缀为 `ping6`）；应答有 **1 秒**上限，超时打 `no reply`。
- 解析失败：`ping: cannot resolve (need IPv4 literal or A record)`。缺参数：`ping: usage: ping <ipv4|hostname>`。
- 语义：经协议栈走**真实网卡** —— v4 目的为网关本身或经网关转发（发送前需先学到网关 MAC）；v6 链路本地目的用链路本地源 + RA 记录的路由器 MAC，全局目的用 SLAAC 地址。

### `ping6 <ipv6|host>`

```text
[morion@morion /]$ ping6 fe80::2
ping6 fe80:0:0:0:0:0:0:2 ... reply
```

- 只做 IPv6；参数非字面量时先查 **AAAA** 记录。
- 解析失败：`ping6: cannot resolve (need IPv6 literal or AAAA record)`；缺参数：`ping6: usage: ping6 <ipv6|hostname>`。

### `dns <name>` / `nslookup <name>`

解析 `A` 与 `AAAA` 各打一行：

```text
[morion@morion /]$ dns example.com
dns: example.com A = 93.184.216.34
dns: example.com AAAA = 2606:2800:220:1:248:1893:25c8:1946
```

- 走 slirp 内置 DNS `10.0.2.3`（UDP 53），源端口 = `12345 + 域号`（shell 域 8 → **12353**）——
  内核端口归属表**一个端口只归一个域**，故 DNS 源端口按域派生（否则 app 先绑的端口会让 shell 绑不上）；
  它落在 shell 的 `Net(12345,12399)` 能力内，`netstack_srv` 再用 `SYS_NET_OWNER` 核对端口归属。
- 查不到该类型时打印 `(none)`（离线时可能两条都是 `(none)`）；缺参数：`dns: usage: dns <name>`。

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
- **04b 起权限位参与访问判定**：仅**节点属主**或 `uid 0` 可改（否则服务回 `EPERM`）。
  shell 身份受引导期静态表影响 —— 引导期服务域（含 shell 自己）= `uid 0`，故这里的
  `chmod` 实际总是被放行；低权身份（运行期新建域 = `uid 1000`）才会被拒。
- 非 MFS 的服务不支持，会返回失败。

### `chown <uid>:<gid> <path>`

- 改文件 / 目录 / 软链接的**属主 uid** 与**属组 gid**（仅 MFS）。
- 成功打印 `chown: <path> -> <uid>:<gid>`；非法参数：`chown: bad uid` / `chown: bad gid`。
- 失败：`chown: failed (uid 0 required): <path>` —— 04b 最小实现里**只有 `uid 0`** 能改
  （演进项：属主可把自己文件的 gid 改到所属组）。
- 用途：老 MFS8 盘的 `uid/gid` 为 0（旧文件全归 root），把目录/文件 `chown` 给低权用户即可
  让运行期程序读写；`mkfs.mfs` 是另一条处置路径（重建文件系统）。

### `truncate <file> <size>`

- 把文件长度改成 `<size>` 字节。截短会释放尾部数据块；扩展是**稀疏**的 ——
  未写过的区间读回全 0。
- 成功打印 `truncate: size=<n> <path>`；非法 size：`truncate: bad size: <size>`。
- 失败（目录 / 无法打开）：`truncate: failed (directory or bad file): <path>`。
- 只支持 MFS。
- **S3b 起 size 是 64 位**（不再卡在 `u32::MAX`）：MFS 上可以截到 **>4 GiB**（如 5 GiB）并稀疏扩展，
  `stat` / `ls -l` 也会如实报出 64 位大小；跨 4 GiB 的读写走文件的**三级间接块**。

### `stat <path>`

- 打印 `File / Type / Mode / Owner / Uid / Gid / Links / Size / Modify / Change` 各行；
  `Owner` 是创建者**域号**（诊断用），`Uid`/`Gid` 是访问判定用的属主身份（04b）。
- 时间格式 `YYYY-MM-DD HH:MM`（UTC，来自 CMOS RTC）；未知时间打印 `(unknown)`。
- 失败：`stat: cannot stat <path>`。
- 非 MFS 的服务返回默认值（权限 0755/0644、属主/uid/gid 0、链接数 1、时间未知）。
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

### `mkfs.mfs <vol> [--force]`

在**卷号**为 `<vol>` 的卷上写一个全新的 MorionFS 文件系统。**会擦除该卷原有内容。**

- `<vol>` 取自块服务启动时打印的卷表：每卷一行
  `vol: <卷号> nsid=<n> lba=<n> sectors=<n> kind=<名>`（`kind=unknown` 表示该卷没有文件系统）。
- **护栏在 mfs_srv 里，不在 shell 里**：只接受 `kind=mfs`（重新格式化）或
  `kind=unknown`（未格式化）的卷。FAT / exFAT / ext2 等别人的分区与不存在的卷号一律被拒，
  **绝不自动吞掉**。命令与 FS-22 自测走同一条路径，判定只有一处。
- `--force` 是护栏的**唯一例外**，且必须显式写出来（`vfs::mfs_mkfs_force`：`MKFS` payload 的
  第 2 个字置 `MKFS_FLAG_FORCE` 位；不带标志的老调用方那里是零填充，故默认行为一个字都没变）。
  它存在的原因是**分区表之外的残留**：`part.wipe` 只清分区表、不动数据，旧文件系统的 VBR
  还在原处，于是 `part.create` 建出的新分区照样被卷层探测成 `exfat`（真盘实测），而
  「我就是要在这块盘上建 MorionFS」这个意图只有用户能表达。放行时服务端先打印一行
  `mfs: mkfs: overwriting an existing filesystem on volume <n> (--force; its files are lost)`。
- **安装盘**构建（`make INSTALL=1 iso`）里，这条护栏对非空白卷**默认放开**（不必写 `--force`，
  服务端打印的原因改为 `(install image; its files are lost)`）—— 装机天生就是「先 U 盘启动、
  再把系统装进本机盘」，要覆盖的正是盘上原有的文件系统。日常镜像 `INSTALL_MODE` 为 `false`，
  行为与本文其余部分完全一致。
- 成功后：卷上有一个空 MorionFS 根目录；若该卷**不是** MFS 主卷，会立刻挂到 `/usb<卷号>`；
  若就是主卷（`/mfs`），原地重建。
- **同时把这卷标记为主卷**：超级块里记下「主卷序号 = 现有最大 + 1」，于是**下次启动** `/mfs`
  就是它 —— 这就是「切换主卷」的手段，与卷表扫描顺序无关。序号只增不减，所以重复格式化
  同一块卷它仍会胜出。本次运行的挂载点不变（换主卷要重启才生效）。
- 输出：`mkfs.mfs: volume <vol> formatted, marked primary (serial <n>) -> /mfs after next boot;
  other volumes mount at /usb<volume-id>`（`serial` 是**从盘上回读**的序号，即为标记已落盘的证据）；
  被拒或失败：`mkfs.mfs: refused volume <vol> (not blank, not MFS, or no such volume)`，未加
  `--force` 时再补一行 `mkfs.mfs: the volume may hold another filesystem; --force overwrites it`。
- 缺参数 / 多参数 / 非数字 / 未知开关：
  `mkfs.mfs: usage: mkfs.mfs <volume-id> [--force]   (see the 'vol:' lines in the boot log)`。
- 相关诊断（服务端打印，属正常护栏证据）：`mfs: mkfs refused (volume holds another filesystem;
  --force overwrites it)`、`mfs: mkfs refused (no such volume)`、
  `mfs: mkfs: overwriting an existing filesystem on volume <n> (--force; …)`（`--force` 生效）或
  `… (install image; …)`（安装盘默认放行）、
  `mfs: mkfs OK but primary mark missing on disk`（异常）。

> 典型用法（真盘上「新买一块盘」）：启动日志里找到目标卷的 `vol:` 行（例如
> `vol: 6 nsid=6 lba=0 sectors=32768 kind=unknown`），敲 `mkfs.mfs 6`，然后
> `ls /usb6` / `touch /usb6/T.TXT`。此后这台机器重启，`/mfs` 就落在卷 6 上。
> 想切回去，用 `mfs.primary 1`（**不动数据**）；只有在确实想重建卷 1 时才用
> `mkfs.mfs 1`（那会**擦除**卷上的文件）。

> 盘上留着**旧文件系统**时（`part.wipe` 之后仍被探测成 `exfat`），确认这块盘就是要建
> MorionFS 后用 `mkfs.mfs <卷号> --force` 覆盖它。另一条路是先在宿主侧清零盘头
> （`scripts/usb-rw.sh` 就是这么做的：它验证的是「盘级擦除重建」这条路，与「用户显式覆盖」
> 是两条独立路径）。

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

### `mfs.snap` / `mfs.snaps` / `mfs.rollback <index>` / `mfs.gc`

MorionFS 的**快照与空间回收**四件套（此前只有自测在用，现已开成 shell 命令）。
四条命令都作用于**主卷 = `/mfs`** —— 与 `df` 同源（这些 tag 直接发给 mfs_srv，不带路径、
也不经挂载层路由，故不看 `cwd`，也不能对 `/usb<卷号>` 上的额外 MFS 卷下手）。

- `mfs.snap`：记录当前根 inode 表 + 代际 + 分配游标，回复新快照索引。输出
  `mfs.snap: snapshot <idx> taken -> ...`（后半句提示用 `mfs.rollback <index>` 退回）。
  失败（未挂载）：`mfs.snap: FAILED (MorionFS not mounted?)`；带参数：`mfs.snap: usage: mfs.snap ...`。
  快照表**持久化在超级块**里、跨启动有效，环上限 **8** 条；写满后**淘汰最旧一条**（索引整体
  前移一位，旧索引随即失效），而不是报错。
- `mfs.snaps`：经 `MSNL` 把快照表写进 shell 的结果页再逐条解析，输出形如
  `mfs.snaps: <n> snapshot(s):` + 每行 `  [<idx>] gen=<代际> root_itab=<块> ino_hint=<n> alloc_hint=<n>`。
  空表时：`mfs.snaps: no snapshots (take one with 'mfs.snap')`。
- `mfs.rollback <index>`：把根指针/代际换回快照那一版并落盘（索引镜像同步重载），
  输出 `mfs.rollback: /mfs rolled back to snapshot <idx> (files now show that snapshot's state)`。
  失败（索引越界 / 未挂载）：`mfs.rollback: FAILED (no such snapshot index <idx>, or MorionFS unavailable)`；
  非数字：`mfs.rollback: usage: mfs.rollback <index>   (see 'mfs.snaps')`。
- `mfs.gc`：以**当前根 + 全部快照**为起点重算可达性，回收不可达的 COW 旧块，输出
  `mfs.gc: reclaimed <n> block(s) unreachable from the root (snapshot-held blocks kept)`。
  只要还有快照引用旧版本，那些块就**不会被回收** —— 这正是回滚能一直生效的前提。

> 典型用法：改文件前 `mfs.snap` → 写坏了 `mfs.snaps` 看索引 → `mfs.rollback 0` 退回。
> 快照只「留旧版本」，占用的空间由被淘汰的快照释放，日常想回收垃圾敲 `mfs.gc`。

### `mfs.fsck [--repair]` / `mfs.sync`

**`mfs.fsck`**：对账 `/mfs` 主卷上「**已分配但不可达**」的 inode 槽（如目录项插入与 inode
登记之间掉电留下的泄漏）。可达性以**当前根目录树**为准；可回收块数则按 GC 的**全根可达性**
（含快照）判定 —— 快照仍引用的历史版本不会被算进可回收。

- 默认**只报不修**（不写盘）：`mfs.fsck: <N> leaked inode(s), <M> reclaimable block(s) - report only (rerun with --repair to reclaim)`。
- `--repair` 才回收：清掉泄漏 inode 槽 + 按可达性安全回收块（快照引用的块保留），输出
  `mfs.fsck: repaired <N> leaked inode(s), reclaimed <M> block(s)`。
- 带非法参数：`mfs.fsck: usage: mfs.fsck [--repair]   (default: report only, no writes)`；
  未挂载 / 失败：`mfs.fsck: FAILED (MorionFS not mounted?)`。

**`mfs.sync`**：把 `/mfs` 显式落盘一次（幂等：刷新位图 + 两份超级块；重复调用只推进代际），
打印**落盘后**的代际 gen：`mfs.sync: /mfs flushed to disk, generation <gen>`。
用途是给「崩溃一致性」自测一个可断言的落盘点（回归里 `FS-32` 会用裸读扇区 0 校验盘上 gen
与回复一致）。带参数：`mfs.sync: usage: mfs.sync   (flush the MorionFS volume at /mfs)`。

> **magic 策略（01 起）**：只有**空白卷**才在首挂时自动格式化。属 MFS 系但修订不匹配本构建
> 的卷（更旧或更新）在挂载时**被拒绝且零写盘**，日志打印盘上 magic、本构建期望 `MFS8` 与
> `mkfs.mfs <vol>` 建议；本构建 `MFS8` 但超级块损坏时同样拒绝重格。旧盘要变新格式，必须
> **显式** `mkfs.mfs <卷号>`（会擦除）。

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

### `part.create <nsid> <MiB> [mbr]`

在**整块盘**上建一个分区。这是「新买一块盘」的第一步 —— 建完分区它立刻成为一个卷，可以
直接 `mkfs.mfs <卷号>`。

- **按 `nsid` 寻址，不是卷号**：分区表属于整块盘，而卷号只是分区表的产物（建之前没有这个卷、
  删完又没了）。`nsid` 见启动日志的 `vol: <卷号> nsid=<n> …` 行。
- **`MiB` 是分区大小**，`0` 表示**用尽剩余空间**（对齐后一直到 GPT 的 `last_usable`）。请求
  超出剩余空间会**被拒**，不会悄悄截断。
- **表风格按盘自适应**：盘上已有 GPT 就继续 GPT、已有 MBR 就继续 MBR；**空白盘默认建 GPT**
  （`>2 TiB` 的盘也只能 GPT）。加 `mbr` 参数可强制建 MBR —— **只在盘上还没有分区表时有效**，
  已有 GPT 时强制转换会毁掉整张表，一律拒绝。
- 分区起点对齐到 **1 MiB**（2048 扇区）；GPT 类型 GUID 取通用的「Linux 文件系统数据」，
  MBR 类型字节 `0x83` —— 建分区时还不知道要 format 成什么（MorionFS 是之后 `mkfs.mfs` 建的），
  卷层探测类型看的是卷首签名，不看这里。
- GPT 会写**完整的两份**：保护性 MBR（LBA 0）、主头（LBA 1）、主项数组（LBA 2..33）、盘尾的
  备份项数组与备份头，并算好头 CRC32 与项数组 CRC32 —— 宿主 `sgdisk`/`firmware` 认这张表。
- 成功后**立刻重读分区表**，新分区作为新卷出现在 `vol:` 表里（块服务打印 `part-dbg: create …`
  与新表）。
- 输出：`part.create: nsid <nsid> -> volume <卷号>  (new 'vol:' line above; format it with mkfs.mfs)`；
  被拒：`part.create: refused nsid <nsid> (no room / no such disk / unsupported conversion)`。
  服务端诊断：**护栏拒绝**一律是 `block: part create refused (…)`（无剩余空间 / 表已满 / 盘太小 /
  容量未知 / 已有 GPT 却要 `mbr`），只有**真出错**才打 `block: part create FAILED (write GPT)` 之类。
- 缺参 / 非数字：`part.create: usage: part.create <nsid> <MiB> [mbr]   (nsid from the 'vol:' lines; MiB 0 = all free space)`。

> 典型用法（真盘上「新买一块盘」）：启动日志里找到目标盘的 `vol:` 行（例如
> `vol: 7 nsid=7 lba=0 sectors=131072 kind=unknown`），敲 `part.create 7 16`，看新出现的
> `vol: … nsid=7 lba=2048 sectors=32768 kind=unknown` 拿到卷号，再 `mkfs.mfs <卷号>`。

### `part.del <nsid> <index>`

删掉盘 `<nsid>` 上第 `<index>` 个分区项（`index` 是项下标，从 0 起，不是卷号）。

- **只清条目**：数据区一个字节都不动 —— 要回收空间请重新格式化那个卷（或建新表）。
- 删完若**一个分区都不剩**，整张表被清空，盘回到「无分区表」的整盘卷状态。这样一块盘能在
  「GPT → 清空 → MBR」之间来回折腾，不必依赖宿主工具。
- GPT 删除会**重算**项数组 CRC32 与两个头（主 + 备份）的 CRC32。
- 输出：`part.del: nsid <nsid> entry <index> removed (data untouched; volume table reparsed)`；
  失败：`part.del: refused nsid <nsid> entry <index> (no such partition, or no partition table)`。

### `part.wipe <nsid>`

清空盘 `<nsid>` 的分区表，让它回到「无分区表」（整盘一个卷）。

- 与 `part.del` 一样**只动表**：MBR 的磁盘签名与 4 个项、GPT 的头与两份项数组都被清零，
  数据区不动。
- 主要用途是把一块盘**复位成空白**（自测每轮都先 `part.wipe` 一次，于是复用镜像也不会被上一轮
  的残留影响）。
- 输出：`part.wipe: nsid <nsid> partition table cleared (disk is blank again; volume table reparsed)`；
  失败：`part.wipe: FAILED on nsid <nsid>`。

### `part.reload`

重新扫描**所有**盘的分区表并重建卷表（然后打印），用来在不重启的前提下确认表改动生效。

- 改动分区表后卷表会被自动重读，所以这条命令主要用于「别人动了盘」或**文件系统被建/删之后**
  —— 卷的 `kind` 是扫描时按卷首签名探出来的，`mkfs.mfs` 之后要重读一次 `kind` 才会变成 `mfs`。
- ⚠️ 卷号由扫描顺序决定：**改动靠前的盘**（例如 nsid 更小的盘）会让它后面那些盘的卷号整体后移，
  而已挂载的文件服务仍记着启动时的旧卷号。安全做法是把分区操作限制在**最后一类**盘上，或者
  改完就重启。自测只动 nsid 7（最后一块盘），所以卷号 0..6 不受影响。
- 输出：`part.reload: partition tables re-read (volume table printed above)`。

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
[FILE] hello.txt  size=48
[DIR]  subdir
[morion@morion /ext2]$ cat hello.txt
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
- 只要 shell **正在等你输入**（含"提示符已显示、你还没敲第一个键"那段时间），输出就会先把
  「提示符 + 已输入」整体摘下暂存、打完再原样接回：别的域/后台自测的日志因此不会清空、
  不会顶掉提示符、也不会提前提交你的半截命令 —— **只有回车才提交执行**。
  跨行（超列宽）输入在回车时自动拼接成完整一行，不再只提交最后一段。
- ↑ / ↓ 在历史区与输入行间移动光标，到顶/底再按触发回滚（`HISTORY_LINES = 512` 行环形缓冲）。
