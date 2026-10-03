# MFS 权限与多用户——设计与接口草案（04）

> 本文件是**设计稿**，不含实现代码。对应的任务书见 [plan-fs-streams.md](plan-fs-streams.md) §3「04」，
> 并行协作约定见 [dev-workflow.md](dev-workflow.md)。
> 行号锚点基于 2026-10-02 工作区状态，只作定位用；实现（04b）时须重读确认。
> **本轮（04）一行 Rust 都不改** —— `user/srv/src/mfs_srv.rs` 归流 01 独占。

---

## 0. 范围与不变量

**本轮产出**：本文 + `docs/roadmap-fs.md` 的 04 小节（指向本文）。

**设计约束（沿用任务书）**：

1. 权限强制必须落在**文件服务内部**（`mfs_srv` 的每个 open/read/write 前），内核不解析文件语义。
2. 不升 magic、不改盘上布局 —— 否则会把既有 MFS8 盘全判成「不匹配」。
3. 与流 01 的 magic 策略（未知/更新版 magic → 拒绝挂载）**联动但不冲突**：本设计始终停留在 MFS8。
4. 不新增内核 syscall（04b 的最小实现不碰内核，见 §7）。

**已确认的三项取舍**（04 会话确认）：

- 身份模型：**两阶段** —— 目标形态为认证服务签发凭证，04b 最小可用为引导期静态「域号 → 凭证」表。
- 落盘：**uid/gid 放进元数据 `+32` 的保留区**（各 u16），`owner` 保留原义（创建者域号）；**不升 magic**。
- 拒绝语义：**定义错误码**（一组 `MFS_E*`），而非继续只回 `u64::MAX`。

---

## 1. 现状与缺口（带锚点）

| 事实 | 位置 |
|---|---|
| `chmod` 只存不判 | `user/srv/src/mfs_srv.rs:3428-3459`（`mfs_chmod`）、`user/libmorion/src/vfs.rs:917-941` |
| `mode` 唯二用途都是**显示** | `mfs_srv.rs:3493`（readdir 填 `DirEntry`）、`mfs_srv.rs:4083`（`stat`） |
| `owner` 存的是**创建者域号**（u16），无 uid、无 gid | 注释 `mfs_srv.rs:98-99`；赋值 `mfs_srv.rs:3819`（creat/mkdir）、`:3859`（symlink）；协议注释 `vfs.rs:512-513` |
| 元数据 40 B，末尾 `+32` 是 **8 字节保留区** | 布局注释 `mfs_srv.rs:95-107`；偏移函数 `mfs_meta_off` `mfs_srv.rs:1824-1831` |
| **「能力即句柄」已落地**：open/creat 签发内核句柄，每次 I/O 前校验 | `kernel/src/cap.rs:93-203`（句柄表）；`vfs.rs:351-363`（fd 打包）、`vfs.rs:381-391`（`cap_guard`）、`vfs.rs:620-638`（`wrap_open`） |
| 能力类型仅资源类（`SendTo/MapInto/Irq/Mmio/IoPort/Fb/Spawn`），**没有身份/凭证** | `kernel/src/cap.rs:13-42` |
| 引导期域数 `BOOT_DOMAINS = 18`（流 03 合入后成 19），域号会**复用** | `kernel/src/domain.rs:33-47` |
| 架构规划了「认证服务 + 能力模型」，尚未实现 | `docs/architecture.md:134-142`、`:56` |

**缺口一句话**：I/O 路径已经知道「你有没有这个打开对象」（能力），但**不知道该以谁的名义做**（没有 uid/gid）、也**不检查**能否做（`mode` 不参与判定）。

---

## 2. 身份模型（Q1）

### 2.1 主体表示

```text
Cred { uid: u16, gid: u16 }
```

- 用 `u16` 与落盘字段一致，避免协议/磁盘宽度不一致带来的截断。
- `uid = 0`：超级用户；`gid = 0`：系统组（沿用 Unix 惯例的数值约定）。
- **只有一个主 gid，无附加组**（本切片明确不做组列表，属已知限制）。

### 2.2 凭证来源（两阶段）

| 阶段 | 形态 | 说明 |
|---|---|---|
| **04b 最小可用** | mfs_srv 内部**静态表**：按请求者域号 `msg.from` 查 `域 → Cred` | 不碰内核、不加 syscall；只覆盖引导期固定域 |
| **目标形态** | **认证服务签发凭证**，随初始能力空间一起下放（`architecture.md:138`） | 登录成功后，认证服务把 `Cred` 与能力引用一并授予会话域；`chown`/setuid 等操作凭能力放行 |

**播种规则（04b 静态表，需与真实域表对齐）**：

| 域号 | uid | gid | 说明 |
|---|---|---|---|
| `id < BOOT_DOMAINS`（引导期长期服务域） | 0 | 0 | 系统身份 |
| `id >= BOOT_DOMAINS`（运行时 `SYS_SPAWN_ELF` 建的用户域） | 1000 | 1000 | 低权用户身份 |

> 既有 FS 自测跑在 app 域内；04b 落地前**必须复核**该域是否落在引导域范围：若在，则自测天然是 root（不会破坏现有 FS 用例）；若不在，需把测试文件建在测试身份名下或在用例里先 `chown`。

### 2.3 防伪造与域复用

- **身份一律由服务侧按 `msg.from` 查表得出**，请求消息里**不携带 uid/gid** —— 消息里的数字可被伪造，域号由内核在 IPC 层保证。
- **域号会复用**（`domain.rs:33-47` 的 `slot_for`）。凭证表必须随域销毁失效，否则新域会继承旧域的 uid。04b 处置：表项带「域存活」校验（复用 `SYS_DOMAIN_ALIVE` 或惰性重查），或由内核在 `destroy_domain` 时通知清表（后者需动内核，**不在 04b 最小范围**，列为演进项）。

### 2.4 与既有能力模型的衔接

- 架构规划「**每个打开的文件是一个能力**」——在本仓库**已在 I/O 路径上部分成立**：`open`/`creat` 签发句柄（`vfs.rs:620-638`），`close` 撤销（`vfs.rs:733-744`），每次 I/O 前 `cap_guard`（`vfs.rs:381-391`）。
- 本设计不重造这套机制，只在其上**补一层身份**：能力回答「这个对象还能不能碰」，`Cred` 回答「以谁的名义碰、能不能碰」。
- `uid = 0` **只绕过 rwx 位判定，不绕过能力/句柄判定** —— 这是与 `architecture.md:140`「消除 root 可做任何事」的折中：没有句柄连请求都发不出去（内核强制），root 只是在服务内权限位层面放行。

---

## 3. 落盘模型（Q2）

### 3.1 布局（不升 magic、不改大小）

MFS 节点元数据 40 B（`mfs_srv.rs:95-107`），目录在 `MFS_HDR + 8`、文件在节点尾部保留区，二者偏移由 `mfs_meta_off(is_dir)`（`mfs_srv.rs:1824-1831`）给出。本次只启用其中**原本就预留的 `+32` u64**：

```text
元数据（40 B）
  +0  mode(u16)      ← 复用：低 12 位权限 + 高 4 位节点类型
  +2  owner(u16)     ← 保留原义：创建者域号（诊断/兼容用）
  +4  nlink(u32)
  +8  mtime(u64) / +16 ctime(u64) / +24 atime(u64)
  +32 uid(u16)       ← 新增
  +34 gid(u16)       ← 新增
  +36 reserved(u32)  ← 继续保留
```

新增常量（04b 落地处）：`MFS_META_UID = 32`、`MFS_META_GID = 34`，读写函数仿 `mfs_get_owner`/`mfs_set_owner`（`mfs_srv.rs:1843-1848`）写 `mfs_get_uid/set_uid/get_gid/set_gid`。

### 3.2 为什么不升 magic

- **本设计不改变任何块/元数据的字节数**，只给保留区赋语义 —— magic 保持 `MFS8`（`mfs_srv.rs:72-74`）。
- 因此与流 01 的策略**天然不冲突**：01 判「magic 属 MFS 系但不等于本构建 MFS8 → 拒绝挂载」；本设计下盘中 magic 仍是 MFS8，01 合入后旧盘照常挂载。
- **迁移含义（必须写清）**：旧 MFS8 盘的 `+32` 为 0，读出来就是 `uid=0, gid=0` —— **既有的全部文件归 root**，低权身份默认无写权限。旧盘要么 `chown`，要么 `mkfs.mfs`。这是有意的、可控的取舍。
- 若将来确需扩元数据（ACL、多字段），再走**升 magic → 01 的拒绝 + 显式 `mkfs.mfs`** 路径；本轮**明确不升**。

### 3.3 赋值规则

- `creat`/`mkdir`/`symlink`：新节点的 `uid/gid` = **发起者 `Cred`**（`mfs_srv.rs:3810-3821`、`4191`）。
- `chown`（04b 新增 tag）：改 `uid/gid`，仅 `uid == 0` 或被授权者（见 §4）。
- `owner` 字段**继续写创建者域号**，不参与判定、仅供诊断 —— 与既有行为（`mfs_srv.rs:3819`）保持一致，避免「同一字段两种语义」。

---

## 4. 权限判定与检查点清单（Q3）

### 4.1 判定函数（04b 新增，mfs_srv 内）

```text
fn mfs_check_access(buf: *const u8, is_dir: bool, cred: Cred, want: Access) -> bool
```

规则（经典 Unix 位，按优先级）：

1. 先确认节点 magic 合法（`MFS_MAGIC_DIR` / `MFS_MAGIC_FILE` / `MFS_MAGIC_LINK`）。
2. `cred.uid == 0` → **直接放行**（rwx 位层面，不绕过能力）。
3. `cred.uid == node.uid` → 取 user 三位。
4. `cred.gid == node.gid` → 取 group 三位。
5. 否则取 other 三位。

`want` 取值：`R`(read)、`W`(write)、`X`(execute)。

- **目录语义**：读取条目列表需 `R`；**穿越/查找**（解析路径中间分量、在目录内创建/删除）需 `X`；在目录内创建/删除/改名还需父目录 `W`。
- **sticky 位**（`mode` 的 `0o1000`）：置位的目录里，`unlink`/`rename` 只允许「文件属主 / 目录属主 / uid 0」执行。

### 4.2 检查时机：两处，语义不同

- **打开时判一次（Unix 语义）**：`open`/`creat` 时判定，并把结果**权限快照**记进 fd（`MfsFd`，`mfs_srv.rs:466-481`）。已打开的 fd 在 `chmod` 之后**仍可继续**读写 —— 与 Unix 一致。
  - 04b 给 `MfsFd` 增加 `cred: Cred` 与 `perm: u8`（`R/W/X` 位）。
- **路径类操作每次重判**：`mkdir`/`unlink`/`rmdir`/`rename`/`link`/`symlink`/`chmod` 在解析到**父目录 inode**后即时判定（这些操作 fd 语义弱、且要防「用旧 fd 绕过目录权限」）。

### 4.3 检查点清单（函数名 + 行号锚点 + 判据 + 拒绝码）

分派入口：`mfs_srv::run()` 的 `match tag`，`mfs_srv.rs:3732-4004`。锚点用 arm 起始行。

| 请求 | 锚点 | 判定对象 / 判据 | 拒绝码 |
|---|---|---|---|
| `OPEN` | `mfs_srv.rs:3733` | 目标节点：文件 `R`；目录 `R+X`。成功则 fd 记权限快照 | `EACCES` |
| `READ` | `mfs_srv.rs:3749` | fd 快照 `R`（open 已判，这里只复核快照） | `EACCES` |
| `WRITE` | `mfs_srv.rs:3767` | fd 快照 `W` | `EACCES` |
| `READDIR` | `mfs_srv.rs:3788` | fd 快照 `R+X` | `EACCES` |
| `CREAT` / `MKDIR` | `mfs_srv.rs:3810` | **父目录** `W+X`；新节点 uid/gid = 发起者 | `EACCES` |
| `UNLINK` / `RMDIR` | `mfs_srv.rs:3822` | **父目录** `W+X`；父目录 sticky 时再加「属主/uid 0」 | `EACCES` |
| `TRUNCATE` | `mfs_srv.rs:3832` | fd 快照 `W`（fd 为准） | `EACCES` |
| `RENAME` | `mfs_srv.rs:3850` | **源父目录 与 目标父目录**均 `W+X`；sticky 同 `UNLINK` | `EACCES` |
| `LINK` | `mfs_srv.rs:3853` | 对源 inode 的引用 + **目标父目录** `W+X`（比照 Linux `protected_hardlinks`：无源写权限时仅当 uid 相同或 `uid 0`） | `EACCES`/`EPERM` |
| `SYMLINK` | `mfs_srv.rs:3858` | **链接自身父目录** `W+X`；目标路径**不判**（符号链接不触发目标权限） | `EACCES` |
| `CHMOD` | `mfs_srv.rs:3864`（实现 `mfs_chmod` `mfs_srv.rs:3428-3459`） | 仅**节点属主**或 `uid 0` | `EPERM` |
| `STAT` | `mfs_srv.rs:3871`（实现 `mfs_stat_into` `mfs_srv.rs:4064`） | 路径可达（沿途目录 `X`） | `EACCES` |
| `LSTAT` | `mfs_srv.rs:3895` | 同 `STAT`（末段不跟随） | `EACCES` |
| `READLINK` | `mfs_srv.rs:3886` | 链接自身父目录 `X`（读链接目标串） | `EACCES` |
| `CHOWN`（04b 新增 tag） | 无 | 仅 `uid 0`（后续可加「属主可改自己 gid 到所属组」） | `EPERM` |

> 说明：fd 类请求（READ/WRITE/READDIR/TRUNCATE）在 `run()` 里先由 `mfs_fd_get` 取 fd（`mfs_srv.rs:3722`）；权限快照随 fd 一起取出，不重复解析路径。路径类请求在 `mfs_normalize`（`mfs_srv.rs:2860`）→ `mfs_resolve`（`mfs_srv.rs:2936`）之后、具体操作前判定。

---

## 5. 拒绝语义 / 错误码（Q3 续）

现状：**所有失败一律回 `u64::MAX`**（如 `mfs_chmod` 的每个失败分支 `mfs_srv.rs:3432-3451`），客户端无法区分「不存在」与「权限不足」。

### 5.1 错误码编码

回复仍是单个 `u64`，用一个**保留高 16 位**的波段表示错误：

```text
MFS_ERR_BASE = 0xFFFF_FFFF_FFFF_0000
mfs_err(code) = MFS_ERR_BASE | (code & 0xFFFF)
mfs_is_err(v) = (v >> 48) == 0xFFFF
```

**为什么安全（不与现有回复冲突）**：

- fd 打包把**能力句柄放在高 16 位**（`vfs.rs:351-363`），句柄索引 `< 32`（`HANDLE_SLOTS`）→ fd 的高 16 位最大 `0x001F`，**永不落在 `0xFFFF` 段**。
- 字节数 / 条目数等成功返回值都是小整数，同样不落该段。
- `u64::MAX` 本身 = `MFS_ERR(GENERIC)`：**旧的通用失败语义天然并入新体系**，向后兼容。

### 5.2 错误码表（04b 落地常量）

| 名 | 值 | 含义 |
|---|---|---|
| `MFS_EPERM` | 1 | 操作被拒（如 `chmod`/`chown` 非属主） |
| `MFS_EACCES` | 2 | 权限位不足（rwx 判定失败） |
| `MFS_ENOENT` | 3 | 路径不存在 |
| `MFS_EEXIST` | 4 | 目标已存在 |
| `MFS_ENOTDIR` | 5 | 路径分量不是目录 |
| `MFS_EISDIR` | 6 | 对目录做了文件操作 |
| `MFS_ENOTEMPTY` | 7 | 目录非空 |
| `MFS_EINVAL` | 8 | 参数非法 |
| `MFS_EROFS` | 9 | 只读（预留） |
| `MFS_ENOSPC` | 10 | 空间不足 |
| `MFS_EIO` | 11 | I/O / 一致性错误 |
| `MFS_EGENERIC` | 0xFFFF | 通用失败（== `u64::MAX`，legacy） |

### 5.3 客户端迁移要求（04b 必做）

- `vfs.rs` 新增 `pub fn mfs_is_err(v: u64) -> bool` 与 `errno` 解出；所有 MFS 客户端封装（`vfs.rs:639-1063`）在**返回值判定**处从 `== u64::MAX` 改为 `mfs_is_err`。
- **不迁移的风险**：`read` 返回 `EACCES`（一个巨大整数）会被旧判定当成「读到了天文数字字节」，属静默错误。因此错误码与客户端判定必须**同一次提交落地**。
- 其它文件服务（fat32/ext2/exfat/tmpfs）本轮不动，继续用 `u64::MAX`；`mfs_is_err(u64::MAX) == true`，两类服务对「通用失败」表现一致。

---

## 6. 与能力系统的边界（Q5）

**两层判定，顺序固定：能力在前，权限在后；服务内判定为最终权威。**

```text
应用 ──(IPC, 携带句柄)──▶ mfs_srv
  ① 内核 IPC 层：SendTo 能力（ipc.rs:88/194）
  ② open/creat：服务内判 rwx → 通过才签发句柄（vfs.rs:620-638）
  ③ 后续每次 I/O：libvfs cap_guard 校验句柄（vfs.rs:381-391）
  ④ 服务内：用 fd 记录的权限快照放行/拒绝（本设计新增）
```

- **谁先谁后**：能力先行。没有句柄/句柄被 `close` 撤销后，请求在客户端就被 `cap_guard` 拦下（内核句柄表是唯一凭证，`kernel/src/cap.rs:93-203`）；服务甚至收不到请求。
- **谁兜底**：**服务内权限判定**。内核不知道「文件」是什么（`kernel/src/cap.rs:96-102` 注释明说），无法判 rwx，故权限语义的权威在服务侧。
- **职责划分**：
  - 能力（内核）负责「对象可达性 + 吊销」——粗粒度、不可绕过、跨域委派（`delegate`/`handle_move`，`cap.rs:232-365`）。
  - 权限位（服务）负责「以什么身份、做哪种操作」——细粒度、随 `chmod`/`chown` 变化。
- `uid 0` 的边界：**只绕过服务内 rwx 位**，不绕过能力（见 §2.4）。
- 04b 不改内核；目标形态（认证服务签发 `Cred`）落地时，凭 `Cred` 与既有能力委派机制（`cap_send`）一起下放，仍不改内核 syscall 表。

---

## 7. 自测规划（Q4）

**本轮不占号**（任务书 §4.2）。**04b 从 `FS-34` 起分配**（01 已占 `FS-30..33`），由 04b 提交时落进 `user/srv/src/app.rs` 的自测表。

规划用例（04b，端到端）：

| 编号（预留） | 用例 | 断言 |
|---|---|---|
| `FS-34` | **低权身份读 root 文件** | 低权 `Cred` `open` root 拥有的 `0444` 文件 → `EACCES`；同 uid 文件可读 |
| `FS-35` | **chmod/chown 语义** | `chmod 600` 后同 uid 可读、异 uid `EACCES`；`chown` 非 `uid 0` 且非属主 → `EPERM`，`uid 0` 成功；改后重新挂载读回新 uid/gid |
| `FS-36` | **目录语义 / sticky** | 无 `X` 的目录无法穿越（`ENOTDIR`/`EACCES`）；sticky 目录里删除他人文件 → `EACCES`，属主可删 |
| `FS-37` | **错误码可区分** | 不存在的路径回 `ENOENT`（非 `EACCES`）；`mfs_is_err` 判定在 `read` 大整数返回下不误判 |

**「低权身份」的产生方式**：由目标形态的认证服务（或 04b 静态表里的低权 `uid 1000`）派生，测试侧通过一个运行时新建域发起请求；不引入 `setuid` 提权路径。

---

## 8. 04b 任务分解（依赖 01 合入）

**前置**：流 01 已合入并释放 `user/srv/src/mfs_srv.rs`、`user/libmorion/src/vfs.rs`、`user/srv/src/shell.rs` 的独占权。

| 层 | 动作 | 文件 |
|---|---|---|
| 内核 | **无改动**（最小实现靠静态凭证表，不动 syscall 表） | — |
| 服务 | `Cred` 结构 + `域→Cred` 静态表；`mfs_check_access`；`MFS_META_UID/GID` 读写；`creat`/`mkdir`/`symlink` 赋 uid/gid；各检查点接线；`MfsFd` 增 `cred`/`perm`；新增 `CHOWN` tag 分支；把权限相关失败从 `u64::MAX` 改为 `mfs_err(EACCES/EPERM)` | `user/srv/src/mfs_srv.rs` |
| 协议 | 新增 `VFS_CHOWN_TAG` + 客户端封装；`mfs_is_err`/errno 解出；所有 MFS 客户端返回值判定改用 `mfs_is_err`；`Stat`/`DirEntry` 暴露 uid/gid（可选新增字段） | `user/libmorion/src/vfs.rs` |
| Shell | `chown <uid>:<gid>`；`ls -l` 显示 uid/gid（与既有 `owner` 展示区分） | `user/srv/src/shell.rs` |
| 自测 | `FS-34..37` | `user/srv/src/app.rs` |
| 文档 | `shell-reference.md`（新命令）、`roadmap-fs.md`（04b 小节）、`dev-reference.md`（权限条目）、`README.md`（勾选行） | 各文档 |

**演进项（不在 04b）**：认证服务签发 `Cred`；域销毁时内核通知清凭证表；附加组 / ACL；`setuid` 提权位；W^X 之外的更细能力。

---

## 9. 明确不做与风险

**不做**（本轮 + 04b 范围外）：

- 不实现认证服务、不做密码/生物特征/令牌；不做 ACL、附加组。
- 不做 `setuid`/`setgid` 提权语义（`mode` 高位的 setuid 位**仍不生效**）。
- 不升 magic、不改盘上布局、不做日志/回放。
- 不改内核 syscall 号表、不改非 MFS 文件服务。
- **04 本轮不改任何 Rust 代码**。

**风险**：

1. **旧盘 uid/gid = 0**：既有文件全归 root，低权身份默认无写权限 —— 需文档显著提示，并提供 `chown`/`mkfs.mfs` 两条处置路径。
2. **域号复用**：凭证表若随域销毁清理不及时，新域会继承旧身份；04b 需引入域存活校验（见 §2.3）。
3. **错误码迁移面**：所有 MFS 客户端（`vfs.rs`）必须在**同一次提交**改用 `mfs_is_err`，否则大整数错误码会被误当成功字节数（见 §5.3）。
4. **root 的定位**：`uid 0` 绕过 rwx 位是本设计的务实取舍，与 `architecture.md:140`「消除 root 可做任何事」的**能力层**目标不冲突（能力仍强制），但需在文档里说明这一层差异。
5. **行号漂移**：本文锚点基于 2026-10-02；01/02/03 合入后 `mfs_srv.rs` 行号会变，04b 动笔前必须重读定位。

---

## 10. 相关文档

- 任务书：[plan-fs-streams.md](plan-fs-streams.md) §3「04」
- 架构（能力模型）：[architecture.md](architecture.md) §三「用户与权限管理——基于能力的安全模型」（`:134-142`）
- 文件系统路线图：[roadmap-fs.md](roadmap-fs.md)
- 内核速查（能力/句柄 syscall）：[dev-reference.md](dev-reference.md) 第 5 节
