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
| `clear` | `clear` | 清屏并复位历史/光标/回滚状态 |

命令名**区分大小写**（须全小写）；未知命令打印 `shell: unknown command: <cmd>`。
空行（去空白后为空）直接忽略。

### `help` 输出（当前实现）

```text
commands:
  help           show this help
  echo <text>    print text
  pwd            print working directory
  ls [path]      list directory (default: current)
  cat <file>     print file content
  cd [path]      change directory (default: /)
  mkdir <path>   create directory
  touch <file>   create empty file
  rm <path>      remove file / empty directory
  clear          clear screen
  (mounts: / = fat32, /tmp = tmpfs, /mfs = MorionFS)
```

---

## 3. 逐命令细节与错误信息

约定的输出格式：**成功也打印一行**（便于交互确认），失败打印一行诊断；
诊断一律以 `<命令>: ` 开头。

### `ls [path]`

- 输出每条目一行：目录 `[DIR]  NAME`，文件 `[FILE] NAME  size=<字节>`。
- 名字按 **8.3 短名**显示（主名 + `.` + 扩展名，空格填充被去掉）。
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
4. **名字长度**：路径中的每一段按 **8.3 短名**处理——主名 ≤ 8、扩展名 ≤ 3，
   自动**转大写**（`readme.txt` 与 `README.TXT` 等价）。超长段在 shell 层即失败。
5. 路径总长超过 `CWD_MAX + SHELL_LINE_MAX` 时返回 `path too long`。

---

## 5. 挂载点与路由（`/` 是统一目录树）

应用只看到单一根 `/`，由 `mount_srv` 做**最长前缀匹配**路由：

| 挂载点 | 服务域 | 说明 |
| --- | --- | --- |
| `/tmp` | tmpfs_srv (10) | 纯内存文件系统 |
| `/mfs` | mfs_srv (11) | MorionFS（块设备后端，可持久化） |
| `/` | fat32_srv (6) | FAT32（NVMe `nsid=1`） |

- 匹配是**组件边界敏感**的：`/tmpfoo` **不会**匹配到 `/tmp`，而会落到 `/`。
- 跨服务操作需显式写路径：`cp` 之类命令尚未提供，`cat /mfs/X` 与 `ls /tmp` 各自路由。

### 例（NVMe 双盘下）

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
```

---

## 6. 输出与终端约定

- **正常运行期不打印非必要日志**（缺页、IPC、键盘、驱动事件都静默），
  以免打断 shell 提示符；仅**失败**时打印一行诊断。
- 各域（app / shell / 服务）共用同一控制台：`SYS_PUTS` 与内核 `video::print`
  都走同一输出通道，并镜像到 COM1。
- 打印会追加到当前输入行末尾，`\n` 提交为历史行；超过列宽自动换行提交。
- ↑ / ↓ 在历史区与输入行间移动光标，到顶/底再按触发回滚（`HISTORY_LINES = 512` 行环形缓冲）。
