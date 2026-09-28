use crate::common::*;
use morion::syscall::*;
use morion::{syscall, vfs};

// ===========================================================================
// 域 8 — Shell (命令行解释器)
// ===========================================================================
// SH-3 阶段: 在基础命令之上补齐路径支持 (cwd + 相对路径) 与写命令
// cd / pwd / mkdir / rm / touch。
// ls / cat / cd 等经 libvfs 调用 fat32_srv; shell 使用独立的 `SHELL_RESULT_BUF`
// 缓冲页, 与 app 域的结果页地址不同, 避免在 fat32_srv 地址空间内互相覆盖。
//
// 约定: 提示符必须以换行结束 (println), 使内核输入行缓冲在用户输入前为空,
// 这样 SYS_READLINE 取回的行只含用户键入的字符, 不含提示符。

/// shell 读入单行的最大长度。
const SHELL_LINE_MAX: usize = 128;
/// 当前工作目录最大长度。
const CWD_MAX: usize = 128;
/// 解析后绝对路径的静态缓冲大小。
const PATH_MAX: usize = 256;

/// shell 运行时状态: 当前工作目录 (绝对路径, 以 '/' 开头)。
struct ShellState {
    cwd: [u8; CWD_MAX],
    cwd_len: usize,
}

impl ShellState {
    fn cwd_str(&self) -> &str {
        unsafe { core::str::from_utf8_unchecked(&self.cwd[..self.cwd_len]) }
    }
    fn set_cwd(&mut self, path: &str) {
        let n = path.len().min(CWD_MAX);
        self.cwd[..n].copy_from_slice(&path.as_bytes()[..n]);
        self.cwd_len = n;
    }
}

/// 解析结果静态缓冲 (单域单线程; 调用方须在下次解析前消费返回的字符串)。
static mut PATH_BUF: [u8; PATH_MAX] = [0u8; PATH_MAX];

/// 把 `arg` 相对 `cwd` 解析为归一化的绝对路径, 写入 `out`, 返回长度。
/// `arg` 以 '/' 开头时按绝对路径处理; 处理 "." / ".." 与重复 '/'。越界返回 None。
fn join_path(cwd: &str, arg: &str, out: &mut [u8]) -> Option<usize> {
    // 1. 拼接 raw: 绝对参数直接使用, 否则 cwd + '/' + arg。
    let mut raw = [0u8; CWD_MAX + SHELL_LINE_MAX];
    let mut rlen = 0usize;
    if !arg.starts_with('/') {
        let c = cwd.as_bytes();
        let n = c.len().min(CWD_MAX);
        raw[..n].copy_from_slice(&c[..n]);
        rlen += n;
        if rlen == 0 || raw[rlen - 1] != b'/' {
            if rlen >= raw.len() {
                return None;
            }
            raw[rlen] = b'/';
            rlen += 1;
        }
    }
    let a = arg.as_bytes();
    if rlen + a.len() > raw.len() {
        return None;
    }
    raw[rlen..rlen + a.len()].copy_from_slice(a);
    rlen += a.len();

    // 2. 逐段归一化, 结果以 '/' 开头。
    if out.is_empty() {
        return None;
    }
    let mut n = 0usize;
    out[n] = b'/';
    n += 1;
    for seg in raw[..rlen].split(|&b| b == b'/') {
        if seg.is_empty() || seg == b"." {
            continue;
        }
        if seg == b".." {
            // 回退一级 (已在根时不动)。
            if n > 1 {
                let mut i = n - 1;
                let mut found = false;
                while i >= 1 {
                    if out[i] == b'/' {
                        n = i;
                        found = true;
                        break;
                    }
                    i -= 1;
                }
                if !found {
                    n = 1;
                }
            }
            continue;
        }
        if n > 1 {
            if n >= out.len() {
                return None;
            }
            out[n] = b'/';
            n += 1;
        }
        if n + seg.len() > out.len() {
            return None;
        }
        out[n..n + seg.len()].copy_from_slice(seg);
        n += seg.len();
    }
    Some(n)
}

/// 把 `arg` 解析为相对当前工作目录的绝对路径字符串 (写入静态 `PATH_BUF`)。
fn resolve_in_cwd(cwd: &str, arg: &str) -> Option<&'static str> {
    unsafe {
        let buf = &mut *core::ptr::addr_of_mut!(PATH_BUF);
        let n = join_path(cwd, arg, buf)?;
        Some(core::str::from_utf8_unchecked(&buf[..n]))
    }
}

/// 域 8 — Shell: 分配结果页 → 循环「提示符 → 读行 → 执行命令」。
pub fn run() {
    // 分配 shell 专用结果页并共享给各文件服务 (同地址映射); ls/cat 的结果写在此页。
    if sys_alloc_page(vfs::SHELL_RESULT_BUF) != 1 {
        println("shell: alloc result buf FAILED");
        return;
    }
    if sys_share_page(vfs::SHELL_RESULT_BUF, vfs::FAT32_DOMAIN) != 1
        || sys_share_page(vfs::SHELL_RESULT_BUF, vfs::TMPFS_DOMAIN) != 1
        || sys_share_page(vfs::SHELL_RESULT_BUF, vfs::MFS_DOMAIN) != 1
        || sys_share_page(vfs::SHELL_RESULT_BUF, vfs::EXT2_DOMAIN) != 1
        || sys_share_page(vfs::SHELL_RESULT_BUF, vfs::EXFAT_DOMAIN) != 1
    {
        println("shell: share result buf FAILED");
        return;
    }
    println("shell: type 'help' for commands");
    // 用户态打印中文: 经 `SYS_PUTS` 把 UTF-8 原样交给内核终端 —— Ring 3 一路到
    // 16x16 点阵字形 (全角标点也是双宽度), 这条是端到端的渲染验证。
    println("你好，世界！MorionOS 终端支持中文、全角标点与宽窄混排。");

    // 初始工作目录为根。
    let mut st = ShellState {
        cwd: [0; CWD_MAX],
        cwd_len: 1,
    };
    st.cwd[0] = b'/';

    let mut line = [0u8; SHELL_LINE_MAX];
    loop {
        // 行内提示符: 与用户输入同处一行, 形如正常终端 `[user@host cwd]$ `。
        print("[morion@morion ");
        print(st.cwd_str());
        print("]$ ");
        syscall::flush();

        let n = sys_readline(&mut line);
        if n == u64::MAX {
            println("shell: readline FAILED");
            return;
        }
        shell_exec(&mut st, &line[..n as usize]);
    }
}

/// 解析并执行一行命令 (命令与参数以首个空格分隔)。
fn shell_exec(st: &mut ShellState, line: &[u8]) {
    let s = unsafe { core::str::from_utf8_unchecked(line) };
    let s = s.trim();
    if s.is_empty() {
        return;
    }
    let (cmd, arg) = match s.find(' ') {
        Some(i) => (&s[..i], s[i + 1..].trim()),
        None => (s, ""),
    };

    match cmd {
        "help" => {
            println("commands:");
            println("  help           show this help");
            println("  echo <text>    print text");
            println("  pwd            print working directory");
            println("  ls [-l] [path] list directory (-l: long form)");
            println("  cat <file>     print file content");
            println("  run <file>     load a .mex program from a file and run it (new domain)");
            println("  cd [path]      change directory (default: /)");
            println("  mkdir <path>   create directory");
            println("  touch <file>   create empty file");
            println("  rm <path>      remove file / empty directory");
            println("  mv <src> <dst> rename / move (same filesystem)");
            println("  ln <src> <dst> hard link an existing file (same filesystem)");
            println("  ln -s <target> <name>  symbolic link (target kept verbatim; MFS only)");
            println("  chmod <mode> <path>  set permission bits (octal, display-only)");
            println("  truncate <file> <size>  resize a file (sparse on grow)");
            println("  stat <path>    show metadata (mode / owner / links / times)");
            println("  lstat <path>   like stat but on the link itself (no follow)");
            println("  readlink <link>  print a symbolic link's target (no follow)");
            println("  mkfs.mfs <vol>   create a MorionFS filesystem on a volume (ERASES it)");
            println("  mfs.primary <vol>   mark a MorionFS volume primary (keeps data)");
            println("  df             show MorionFS space usage (/mfs)");
            println(
                "  part.create <nsid> <MiB> [mbr]  create a partition (disk-wide, blank = GPT)",
            );
            println("  part.del <nsid> <index>  delete a partition entry (keeps data)");
            println("  part.wipe <nsid>   clear the partition table (disk becomes blank)");
            println("  part.reload      re-read all partition tables");
            println("  clear          clear screen");
            println("  (mounts: / = fat32, /tmp = tmpfs, /mfs = MorionFS, /ext2 = ext2 ro, /usb = exFAT)");
            println(
                "  (extra volumes auto-mounted as /usb<N>, N = volume id in the boot volume list)",
            );
        }
        "echo" => println(arg),
        "pwd" => println(st.cwd_str()),
        "ls" => shell_ls(st, if arg.is_empty() { "." } else { arg }),
        "cat" => shell_cat(st, arg),
        "run" => shell_run(st, arg),
        "cd" => shell_cd(st, if arg.is_empty() { "/" } else { arg }),
        "mkdir" => shell_mkdir(st, arg),
        "touch" => shell_touch(st, arg),
        "rm" => shell_rm(st, arg),
        "mv" => shell_mv(st, arg),
        "ln" => shell_ln(st, arg),
        "chmod" => shell_chmod(st, arg),
        "truncate" => shell_truncate(st, arg),
        "stat" => shell_stat(st, arg, false),
        "lstat" => shell_stat(st, arg, true),
        "readlink" => shell_readlink(st, arg),
        "mkfs.mfs" => shell_mkfs(arg),
        "mfs.primary" => shell_mfs_primary(arg),
        "df" => shell_df(arg),
        "part.create" => shell_part_create(arg),
        "part.del" => shell_part_delete(arg),
        "part.wipe" => shell_part_wipe(arg),
        "part.reload" => {
            let n = block_part_reload();
            if n > 0 {
                println("part.reload: partition tables re-read (volume table printed above)");
            } else {
                println("part.reload: FAILED");
            }
        }
        "clear" => {
            sys_clear();
        }
        _ => {
            print("shell: unknown command: ");
            println(cmd);
        }
    }
}

/// `run <path>` — 从文件系统加载一个可执行文件并启动它（E1/E2）。
///
/// 路径由内容决定能不能跑：加载器只认文件头（ELF64 `ET_EXEC`），后缀/名字只是给人的提示。
/// 加载到的是一个**新域**（新进程），子程序自己打印它的输出；shell 不等待它结束。
fn shell_run(st: &ShellState, arg: &str) {
    if arg.is_empty() {
        println("run: usage: run <file>   (e.g. run /hello.mex)");
        return;
    }
    let path = match resolve_in_cwd(st.cwd_str(), arg) {
        Some(p) => p,
        None => {
            println("run: path too long");
            return;
        }
    };
    match morion::exec::spawn_file(path) {
        Some(domain) => {
            print("run: loaded ");
            print(path);
            print(" -> new domain ");
            print_u64(domain);
            println("");
        }
        None => {
            print("run: cannot load ");
            print(path);
            println(" (missing file, or not a valid ELF64 program)");
        }
    }
}

/// `ls [-l] [path]` — 列出目录条目; `-l` 时附权限 / 属主 / 链接数 / 时间。
fn shell_ls(st: &ShellState, arg: &str) {
    // `-l` 解析: 只支持这一个开关, 其余部分当路径。
    let (long, rest) = match arg.strip_prefix("-l") {
        Some(r) => (true, r.trim()),
        None => (false, arg),
    };
    let arg = if rest.is_empty() { "." } else { rest };
    let path = match resolve_in_cwd(st.cwd_str(), arg) {
        Some(p) => p,
        None => {
            println("ls: path too long");
            return;
        }
    };
    let fd = vfs::open(path);
    if fd == u64::MAX {
        print("ls: cannot open ");
        println(path);
        return;
    }
    let n = vfs::readdir_into(fd, vfs::SHELL_RESULT_BUF);
    if n == u64::MAX {
        print("ls: not a directory: ");
        println(path);
        vfs::close(fd);
        return;
    }
    let entry_size = core::mem::size_of::<vfs::DirEntry>();
    let count = n as usize / entry_size;
    let list = unsafe {
        core::slice::from_raw_parts(vfs::SHELL_RESULT_BUF as *const vfs::DirEntry, count)
    };
    for de in list {
        if long {
            print_mode(de.mode, de.is_dir != 0);
            print(" owner=");
            print_u64(de.owner as u64);
            print(" links=");
            print_u64(de.nlink as u64);
            print(" size=");
            print_u64_pad(de.size, 8);
            print("  ");
            print_time(de.mtime);
            print("  ");
            print_entry_name(de);
            println("");
        } else {
            print(entry_kind_label(de));
            print_entry_name(de);
            if de.is_dir == 0 {
                print("  size=");
                print_u64(de.size);
            }
            println("");
        }
    }
    vfs::close(fd);
}

/// `chmod <mode> <path>` — 修改权限位 (八进制, 低 12 位有效)。
///
/// 权限位当前只存储与显示, 不参与访问判定 (系统还没有多用户概念)。
fn shell_chmod(st: &ShellState, arg: &str) {
    let (mode_s, path_s) = match arg.find(' ') {
        Some(i) => (&arg[..i], arg[i + 1..].trim()),
        None => {
            println("chmod: usage: chmod <octal-mode> <path>");
            return;
        }
    };
    if path_s.is_empty() {
        println("chmod: usage: chmod <octal-mode> <path>");
        return;
    }
    let mode = match parse_octal(mode_s) {
        Some(m) => m,
        None => {
            print("chmod: bad mode: ");
            println(mode_s);
            return;
        }
    };
    let path = match resolve_in_cwd(st.cwd_str(), path_s) {
        Some(p) => p,
        None => {
            println("chmod: path too long");
            return;
        }
    };
    if vfs::chmod_into(path, mode, vfs::SHELL_RESULT_BUF) == 1 {
        print("chmod: mode=");
        print_u64(mode as u64);
        print(" ");
        println(path);
    } else {
        print("chmod: failed: ");
        println(path);
    }
}

/// `mv <src> <dst>` — 重命名 / 移动 (同一次请求内跨目录; 不支持跨文件系统)。
fn shell_mv(st: &ShellState, arg: &str) {
    let (src_s, dst_s) = match arg.find(' ') {
        Some(i) => (&arg[..i], arg[i + 1..].trim()),
        None => {
            println("mv: usage: mv <src> <dst>");
            return;
        }
    };
    if dst_s.is_empty() {
        println("mv: usage: mv <src> <dst>");
        return;
    }
    let src = match resolve_in_cwd(st.cwd_str(), src_s) {
        Some(p) => p,
        None => {
            println("mv: source path too long");
            return;
        }
    };
    let dst = match resolve_in_cwd(st.cwd_str(), dst_s) {
        Some(p) => p,
        None => {
            println("mv: target path too long");
            return;
        }
    };
    if vfs::rename_into(src, dst, vfs::SHELL_RESULT_BUF) == 1 {
        print("mv: ");
        print(src);
        print(" -> ");
        println(dst);
    } else {
        print("mv: failed (cross-fs, bad target, or directory loop): ");
        println(src);
    }
}

/// `ln [-s] <target> <name>` — 硬链接 (`ln`) 或软链接 (`ln -s`, 阶段 D/M5c)。
///
/// 硬链接: 两个名字共享同一个 inode, 从任一个名字改写内容, 另一个名字都会看到。
/// 软链接: 存的是**目标路径字符串**, `target` 原样落盘 —— 相对路径相对链接所在目录,
/// 允许悬空 (目标可以之后才创建)。`rm` 一个软链接只摘掉链接, 不动目标。
fn shell_ln(st: &ShellState, arg: &str) {
    let (sym, rest) = match arg.strip_prefix("-s") {
        Some(r) => (true, r.trim()),
        None => (false, arg),
    };
    let (src_s, dst_s) = match rest.find(' ') {
        Some(i) => (&rest[..i], rest[i + 1..].trim()),
        None => {
            println(if sym {
                "ln: usage: ln -s <target> <link-name>"
            } else {
                "ln: usage: ln <existing-file> <new-name>"
            });
            return;
        }
    };
    if dst_s.is_empty() {
        println(if sym {
            "ln: usage: ln -s <target> <link-name>"
        } else {
            "ln: usage: ln <existing-file> <new-name>"
        });
        return;
    }
    let dst = match resolve_in_cwd(st.cwd_str(), dst_s) {
        Some(p) => p,
        None => {
            println("ln: target path too long");
            return;
        }
    };
    if sym {
        // 目标**不做路径解析**: 它只是一段要存进链接节点的字符串。
        if vfs::symlink_into(src_s, dst, vfs::SHELL_RESULT_BUF) == 1 {
            print("ln -s: ");
            print(dst);
            print(" -> ");
            println(src_s);
        } else {
            print("ln -s: failed (name exists, target empty/too long, or no MFS): ");
            println(dst);
        }
        return;
    }
    let src = match resolve_in_cwd(st.cwd_str(), src_s) {
        Some(p) => p,
        None => {
            println("ln: source path too long");
            return;
        }
    };
    if vfs::link_into(src, dst, vfs::SHELL_RESULT_BUF) == 1 {
        print("ln: ");
        print(dst);
        print(" -> ");
        println(src);
    } else {
        print("ln: failed (needs an existing file, new name, same fs): ");
        println(src);
    }
}

/// `truncate <path> <size>` — 把文件截断/扩展到指定字节数。
fn shell_truncate(st: &ShellState, arg: &str) {
    let (path_s, size_s) = match arg.find(' ') {
        Some(i) => (&arg[..i], arg[i + 1..].trim()),
        None => {
            println("truncate: usage: truncate <path> <size>");
            return;
        }
    };
    // size 现在是 u64: 不再把上限卡在 4 GiB, 与协议 / MFS 的能力对齐。
    let size = match parse_dec(size_s) {
        Some(v) => v,
        _ => {
            print("truncate: bad size: ");
            println(size_s);
            return;
        }
    };
    let path = match resolve_in_cwd(st.cwd_str(), path_s) {
        Some(p) => p,
        None => {
            println("truncate: path too long");
            return;
        }
    };
    let fd = vfs::open(path);
    if fd == u64::MAX {
        print("truncate: cannot open ");
        println(path);
        return;
    }
    let r = vfs::truncate(fd, size);
    vfs::close(fd);
    if r == 1 {
        print("truncate: size=");
        print_u64(size);
        print(" ");
        println(path);
    } else {
        print("truncate: failed (directory or bad file): ");
        println(path);
    }
}

/// `stat <path>` / `lstat <path>` — 打印单条路径的完整元数据。
///
/// `no_follow = false` (`stat`) 跟随末段软链接 (悬空链接会失败, 与 Unix `stat` 一致);
/// `no_follow = true` (`lstat`) 作用于链接自身 —— 类型显示为 `symbolic link`、
/// `Size` 是**目标串长度**, 悬空链接也照样看得到。
fn shell_stat(st: &ShellState, arg: &str, no_follow: bool) {
    if arg.is_empty() {
        println("stat: missing operand");
        return;
    }
    let path = match resolve_in_cwd(st.cwd_str(), arg) {
        Some(p) => p,
        None => {
            println("stat: path too long");
            return;
        }
    };
    let want = core::mem::size_of::<vfs::Stat>() as u64;
    let got = if no_follow {
        vfs::lstat_into(path, vfs::SHELL_RESULT_BUF)
    } else {
        vfs::stat_into(path, vfs::SHELL_RESULT_BUF)
    };
    if got != want {
        print(if no_follow {
            "lstat: cannot stat "
        } else {
            "stat: cannot stat "
        });
        println(path);
        return;
    }
    let st = unsafe { core::ptr::read_unaligned(vfs::SHELL_RESULT_BUF as *const vfs::Stat) };
    print("  File: ");
    println(path);
    print("  Type: ");
    println(entry_type_name(st.mode, st.is_dir != 0));
    print("  Mode: ");
    print_mode(st.mode, st.is_dir != 0);
    print("  Owner: domain ");
    print_u64(st.owner as u64);
    print("  Links: ");
    print_u64(st.nlink as u64);
    print("  Size:  ");
    print_u64(st.size);
    print("  Modify: ");
    print_time(st.mtime);
    print("  Change: ");
    print_time(st.ctime);
}

/// `readlink <link>` — 打印软链接**自身**的目标串 (不跟随)。
///
/// 输出的正是当初 `ln -s` 写的那个路径: 服务端存的是服务命名空间里的形式, 客户端
/// `vfs::readlink_into` 会把挂载前缀加回去 (`/a` -> `/mfs/a`)。
fn shell_readlink(st: &ShellState, arg: &str) {
    if arg.is_empty() {
        println("readlink: missing operand");
        return;
    }
    let path = match resolve_in_cwd(st.cwd_str(), arg) {
        Some(p) => p,
        None => {
            println("readlink: path too long");
            return;
        }
    };
    let n = vfs::readlink_into(path, vfs::SHELL_RESULT_BUF);
    if n == u64::MAX || n == 0 {
        print("readlink: not a symbolic link: ");
        println(path);
        return;
    }
    let raw =
        unsafe { core::slice::from_raw_parts(vfs::SHELL_RESULT_BUF as *const u8, n as usize) };
    let target = unsafe { core::str::from_utf8_unchecked(raw) };
    print_sanitized(target);
    println("");
}

/// `mkfs.mfs <卷号>` — 在指定卷上创建 MorionFS 文件系统 (**擦除**该卷现有内容)。
///
/// 卷号来自块服务启动时打印的卷表 (`vol: <卷号> nsid=... kind=...`)。护栏不在这里
/// 而在服务端: mfs_srv 只接受 `kind=mfs` (重新格式化) 或 `kind=unknown` (未格式化)
/// 的卷, FAT / exFAT / ext2 一律拒绝 —— 命令与自测走同一条路径, 判定只有一处。
///
/// 格式化同时把这卷标记为**主卷**: **下次启动** `/mfs` 就是它 (本次运行的挂载不变)。
fn shell_mkfs(arg: &str) {
    let vol = match parse_dec(arg) {
        Some(v) => v,
        None => {
            println(
                "mkfs.mfs: usage: mkfs.mfs <volume-id>   (see the 'vol:' lines in the boot log)",
            );
            return;
        }
    };
    let serial = vfs::mfs_mkfs(vol);
    if serial != u64::MAX {
        print("mkfs.mfs: volume ");
        print_u64(vol);
        print(" formatted, marked primary (serial ");
        print_u64(serial);
        println(") -> /mfs after next boot; other volumes mount at /usb<volume-id>");
    } else {
        print("mkfs.mfs: refused volume ");
        print_u64(vol);
        println(" (not blank, not MFS, or no such volume)");
    }
}

/// `mfs.primary <卷号>` — 把一块**已经有数据**的 MFS 卷换成主卷 (**不动数据**)。
///
/// 与 `mkfs.mfs <卷号>` 的区别: 后者建新文件系统、会**擦掉**卷上的文件; 本命令只改
/// 超级块里的主卷序号, 卷上的文件原样保留 —— 日常换主卷用这个。
///
/// 只接受已经是 MFS 的卷 (空盘 / FAT / ext2 / exFAT 一律拒绝), 也不会自动格式化。
/// 生效时机与 mkfs 一致: **下次启动** `/mfs` 认领到它, 本次运行的挂载点不变。
fn shell_mfs_primary(arg: &str) {
    let vol = match parse_dec(arg) {
        Some(v) => v,
        None => {
            println("mfs.primary: usage: mfs.primary <volume-id>   (see the 'vol:' lines in the boot log)");
            return;
        }
    };
    let serial = vfs::mfs_set_primary(vol);
    if serial != u64::MAX {
        print("mfs.primary: volume ");
        print_u64(vol);
        print(" marked primary (serial ");
        print_u64(serial);
        println(") -> /mfs after next boot; data on it was NOT touched");
    } else {
        print("mfs.primary: refused volume ");
        print_u64(vol);
        println(" (not a MorionFS volume, or no such volume)");
    }
}

/// `df` — 报告**已挂载文件系统**的空间用量。
///
/// 目前只有 MorionFS 有「容量」概念 (别的服务不维护块分配, 报不出数字), 故只有 `/mfs` 一行。
/// 数字来自 mfs_srv 的内存态: `MSST` 不带卷参数, 服务端按**默认卷 = 主卷**取数, 故这里报的
/// 就是 `/mfs` —— 与 `/usb<卷号>` 上那些额外 MFS 卷无关。
fn shell_df(arg: &str) {
    if !arg.is_empty() {
        println("df: usage: df   (only MorionFS reports capacity; other filesystems do not)");
        return;
    }
    let usage = vfs::mfs_stat();
    if usage == u64::MAX {
        println("df: /mfs unavailable (MorionFS not mounted?)");
        return;
    }
    let total = usage >> 32;
    let free = usage & 0xFFFF_FFFF;
    let used = total.saturating_sub(free);
    let pct = (used * 100).checked_div(total).unwrap_or(0);
    print("df: /mfs (MorionFS): total ");
    print_u64(total);
    print(" blocks, used ");
    print_u64(used);
    print(", free ");
    print_u64(free);
    print(" (");
    print_u64(pct);
    println("% used; 1 block = 4 KiB)");
}

/// `part.create <nsid> <MiB> [mbr]` — 在盘 `<nsid>` 上建一个 `<MiB>` 的分区 (`0` = 用尽剩余空间)。
///
/// 表风格**按盘自适应**: 盘上已有分区表就沿用它的风格; **空白盘默认建 GPT**, 参数加 `mbr`
/// 改建 MBR (只在空白盘上有效 —— 把已有 GPT 的盘改成 MBR 会毁掉那张表, 一律拒绝)。
/// 起点对齐到 1 MiB。成功后块服务会**立刻重读分区表**, 新分区作为新卷出现在 `vol:` 表里。
fn shell_part_create(arg: &str) {
    const USAGE: &str = "part.create: usage: part.create <nsid> <MiB> [mbr]   (nsid from the 'vol:' lines; MiB 0 = all free space)";
    let mut it = arg.split_whitespace();
    let (nsid, mib) = match (it.next().and_then(parse_dec), it.next().and_then(parse_dec)) {
        (Some(a), Some(b)) => (a, b),
        _ => {
            println(USAGE);
            return;
        }
    };
    let force_mbr = match it.next() {
        None => false,
        Some("mbr") => true,
        Some(_) => {
            println(USAGE);
            return;
        }
    };
    if it.next().is_some() {
        println(USAGE);
        return;
    }
    // 1 MiB = 2048 个 512 字节扇区。
    let vol = block_part_create(nsid, mib * 2048, force_mbr);
    if vol != u64::MAX {
        print("part.create: nsid ");
        print_u64(nsid);
        print(" -> volume ");
        print_u64(vol);
        println("  (new 'vol:' line above; format it with mkfs.mfs)");
    } else {
        print("part.create: refused nsid ");
        print_u64(nsid);
        println(" (no room / no such disk / unsupported conversion)");
    }
}

/// `part.del <nsid> <index>` — 删掉盘 `<nsid>` 上序号为 `<index>` 的分区。
///
/// **只清条目**: 数据区一个字节都不动 (要回收空间请重新格式化那个卷)。删完若一个分区都不剩,
/// 整张表被清空, 盘回到「无分区表」。
fn shell_part_delete(arg: &str) {
    const USAGE: &str = "part.del: usage: part.del <nsid> <index>   (index = 项下标, 从 0 起)";
    let mut it = arg.split_whitespace();
    let (nsid, index) = match (it.next().and_then(parse_dec), it.next().and_then(parse_dec)) {
        (Some(a), Some(b)) => (a, b),
        _ => {
            println(USAGE);
            return;
        }
    };
    if block_part_delete(nsid, index) == 1 {
        print("part.del: nsid ");
        print_u64(nsid);
        print(" entry ");
        print_u64(index);
        println(" removed (data untouched; volume table reparsed)");
    } else {
        print("part.del: refused nsid ");
        print_u64(nsid);
        print(" entry ");
        print_u64(index);
        println(" (no such partition, or no partition table)");
    }
}

/// `part.wipe <nsid>` — 清空盘 `<nsid>` 的分区表, 让它回到「无分区表」(整盘卷)。
///
/// 与 `part.del` 一样**只动表**: GPT 的头与两份项数组、MBR 的签名与 4 个项都被清零, 数据区
/// 不动。用来把一块测试盘复位成空白, 好从头再建另一种风格的表。
fn shell_part_wipe(arg: &str) {
    let nsid = match parse_dec(arg) {
        Some(v) => v,
        None => {
            println("part.wipe: usage: part.wipe <nsid>");
            return;
        }
    };
    if block_part_wipe(nsid) == 1 {
        print("part.wipe: nsid ");
        print_u64(nsid);
        println(" partition table cleared (disk is blank again; volume table reparsed)");
    } else {
        print("part.wipe: FAILED on nsid ");
        print_u64(nsid);
        println("");
    }
}

/// 按 8 / 10 进制解析无符号整数 (不带前缀, 空串/非法字符返回 None)。
fn parse_octal(s: &str) -> Option<u32> {
    parse_radix(s, 8)
}
fn parse_dec(s: &str) -> Option<u64> {
    parse_radix(s, 10).map(|v| v as u64)
}
fn parse_radix(s: &str, radix: u32) -> Option<u32> {
    if s.is_empty() {
        return None;
    }
    let mut v: u32 = 0;
    for c in s.bytes() {
        let d = match c {
            b'0'..=b'9' => (c - b'0') as u32,
            b'a'..=b'f' => (c - b'a') as u32 + 10,
            _ => return None,
        };
        if d >= radix {
            return None;
        }
        v = v.checked_mul(radix)?.checked_add(d)?;
    }
    Some(v)
}

/// 打印 `ls -l` / `stat` 用的类型+权限字符串 (10 字符)。
///
/// 首字符优先取 `mode` 高 4 位的类型位 (MFS 会填, 能区分出软链接 `l`); 没有类型位
/// 的文件服务 (fat32 / ext2 / exFAT / tmpfs) 回退到 `is_dir`。
fn print_mode(mode: u16, is_dir: bool) {
    const RWX: [u8; 9] = *b"rwxrwxrwx";
    let mut out = [b'-'; 10];
    out[0] = match mode & vfs::MODE_FTYPE_MASK {
        vfs::MODE_FTYPE_DIR => b'd',
        vfs::MODE_FTYPE_LINK => b'l',
        vfs::MODE_FTYPE_FILE => b'-',
        _ => {
            if is_dir {
                b'd'
            } else {
                b'-'
            }
        }
    };
    for (i, slot) in out.iter_mut().enumerate().skip(1) {
        let bit = 9 - i; // i=1 -> bit8 (owner r) ... i=9 -> bit0 (other x)
        if mode & (1 << bit) != 0 {
            *slot = RWX[i - 1];
        }
    }
    print(unsafe { core::str::from_utf8_unchecked(&out) });
}

/// 条目的类型短标签 (供 `ls` 非长格式显示)。
fn entry_kind_label(de: &vfs::DirEntry) -> &'static str {
    match de.mode & vfs::MODE_FTYPE_MASK {
        vfs::MODE_FTYPE_DIR => "[DIR]  ",
        vfs::MODE_FTYPE_LINK => "[LINK] ",
        _ => {
            if de.is_dir != 0 {
                "[DIR]  "
            } else {
                "[FILE] "
            }
        }
    }
}

/// `stat` 的 Type 行文本。
fn entry_type_name(mode: u16, is_dir: bool) -> &'static str {
    match mode & vfs::MODE_FTYPE_MASK {
        vfs::MODE_FTYPE_DIR => "directory",
        vfs::MODE_FTYPE_LINK => "symbolic link",
        vfs::MODE_FTYPE_FILE => "regular file",
        _ => {
            if is_dir {
                "directory"
            } else {
                "regular file"
            }
        }
    }
}

/// 打印无符号整数, 不足 `width` 位左侧补 '0'。
fn print_u64_pad(v: u64, width: usize) {
    let mut buf = [b'0'; 24];
    let mut i = buf.len();
    let mut x = v;
    loop {
        i -= 1;
        buf[i] = b'0' + (x % 10) as u8;
        x /= 10;
        if x == 0 {
            break;
        }
    }
    while buf.len() - i < width && i > 0 {
        i -= 1;
        buf[i] = b'0';
    }
    print(unsafe { core::str::from_utf8_unchecked(&buf[i..]) });
}

/// 打印 Unix 秒 (UTC) 为 `YYYY-MM-DD HH:MM`; 0 表示"未知", 打印占位符。
fn print_time(secs: u64) {
    if secs == 0 {
        print("(unknown)");
        return;
    }
    let days = (secs / 86400) as i64;
    let rem = secs % 86400;
    let (y, m, d) = civil_from_days(days);
    print_u64_pad(y as u64, 4);
    print("-");
    print_u64_pad(m as u64, 2);
    print("-");
    print_u64_pad(d as u64, 2);
    print(" ");
    print_u64_pad(rem / 3600, 2);
    print(":");
    print_u64_pad((rem % 3600) / 60, 2);
}

/// `cat <file>` — 打印文件内容 (不可打印字节替换为 '.')。
fn shell_cat(st: &ShellState, arg: &str) {
    if arg.is_empty() {
        println("cat: missing file operand");
        return;
    }
    let path = match resolve_in_cwd(st.cwd_str(), arg) {
        Some(p) => p,
        None => {
            println("cat: path too long");
            return;
        }
    };
    let fd = vfs::open(path);
    if fd == u64::MAX {
        print("cat: cannot open ");
        println(path);
        return;
    }
    let n = vfs::read_into(fd, 0, 4096, vfs::SHELL_RESULT_BUF);
    if n == u64::MAX {
        println("cat: read failed (is it a directory?)");
    } else {
        let content =
            unsafe { core::slice::from_raw_parts(vfs::SHELL_RESULT_BUF as *const u8, n as usize) };
        let s = unsafe { core::str::from_utf8_unchecked(content) };
        print_sanitized(s);
        println("");
    }
    vfs::close(fd);
}

/// `cd [path]` — 切换工作目录 (须为已存在目录); 无参数回到根目录。
fn shell_cd(st: &mut ShellState, arg: &str) {
    let path = match resolve_in_cwd(st.cwd_str(), arg) {
        Some(p) => p,
        None => {
            println("cd: path too long");
            return;
        }
    };
    let fd = vfs::open(path);
    if fd == u64::MAX {
        print("cd: no such directory: ");
        println(path);
        return;
    }
    let n = vfs::readdir_into(fd, vfs::SHELL_RESULT_BUF);
    vfs::close(fd);
    if n == u64::MAX {
        print("cd: not a directory: ");
        println(path);
        return;
    }
    st.set_cwd(path);
}

/// `mkdir <path>` — 创建目录。
fn shell_mkdir(st: &ShellState, arg: &str) {
    if arg.is_empty() {
        println("mkdir: missing operand");
        return;
    }
    let path = match resolve_in_cwd(st.cwd_str(), arg) {
        Some(p) => p,
        None => {
            println("mkdir: path too long");
            return;
        }
    };
    if vfs::mkdir(path) == 1 {
        print("mkdir: created ");
        println(path);
    } else {
        print("mkdir: failed (exists or bad parent): ");
        println(path);
    }
}

/// `touch <file>` — 创建空文件 (已存在则视为打开, 不报错)。
fn shell_touch(st: &ShellState, arg: &str) {
    if arg.is_empty() {
        println("touch: missing operand");
        return;
    }
    let path = match resolve_in_cwd(st.cwd_str(), arg) {
        Some(p) => p,
        None => {
            println("touch: path too long");
            return;
        }
    };
    let fd = vfs::creat(path);
    if fd == u64::MAX {
        print("touch: failed (bad parent?): ");
        println(path);
    } else {
        vfs::close(fd);
        print("touch: created ");
        println(path);
    }
}

/// `rm <path>` — 删除文件; 文件删除失败时尝试按空目录删除。
fn shell_rm(st: &ShellState, arg: &str) {
    if arg.is_empty() {
        println("rm: missing operand");
        return;
    }
    let path = match resolve_in_cwd(st.cwd_str(), arg) {
        Some(p) => p,
        None => {
            println("rm: path too long");
            return;
        }
    };
    if vfs::unlink(path) == 1 {
        print("rm: removed ");
        println(path);
    } else if vfs::rmdir(path) == 1 {
        print("rm: removed directory ");
        println(path);
    } else {
        print("rm: failed (not found, or directory not empty): ");
        println(path);
    }
}

/// 列出目录 fd 的条目, 判断是否存在短名为 `name` 且类型匹配 `want_dir` 的条目。
/// readdir 失败或未命中返回 false。
fn readdir_has(fd: u64, name: &str, want_dir: bool) -> bool {
    let sn = match short_name_from_query(name.as_bytes()) {
        Some(sn) => sn,
        None => return false,
    };
    let n = vfs::readdir(fd);
    if n == u64::MAX {
        return false;
    }
    let entry_size = core::mem::size_of::<vfs::DirEntry>();
    let count = n as usize / entry_size;
    let list =
        unsafe { core::slice::from_raw_parts(vfs::RESULT_BUF as *const vfs::DirEntry, count) };
    for de in list {
        // 目录项里是盘上的原始 11 字节 (字节大小写不保证, 见 `entry_name_matches`),
        // 故按大小写不敏感比对, 不能直接 `de.name == sn`。
        if entry_name_matches(de.name.as_ptr(), &sn) && (de.is_dir != 0) == want_dir {
            return true;
        }
    }
    false
}

/// 在 readdir 结果中按**长名字段**查找条目 (VFAT LFN / ext2 名字)。
fn readdir_has_long(fd: u64, name: &str, want_dir: bool) -> bool {
    let n = vfs::readdir(fd);
    if n == u64::MAX {
        return false;
    }
    let entry_size = core::mem::size_of::<vfs::DirEntry>();
    let count = n as usize / entry_size;
    let list =
        unsafe { core::slice::from_raw_parts(vfs::RESULT_BUF as *const vfs::DirEntry, count) };
    let q = name.as_bytes();
    for de in list {
        let llen = de.long_len as usize;
        if llen == q.len() && &de.long[..llen] == q && (de.is_dir != 0) == want_dir {
            return true;
        }
    }
    false
}

/// 打印长名 (UTF-8): ASCII 原样输出, 一个非 ASCII 字符折成一个 '?'。
///
/// 内核字体只有 ASCII 0x20..0x7E, 非 ASCII 字节不会被绘制 (会在屏幕上留空隙),
/// 故在用户态先折成 '?' 再送出去, 保证名字长度与视觉都对得上。
fn print_long_name(name: &[u8]) {
    let mut out = [0u8; vfs::DIR_LONG_MAX];
    let mut n = 0usize;
    let mut i = 0usize;
    while i < name.len() && n < out.len() {
        let b = name[i];
        if b < 0x80 {
            out[n] = if (0x20..0x7F).contains(&b) { b } else { b'?' };
            n += 1;
            i += 1;
        } else {
            // 整个 UTF-8 序列折成一个 '?' (跳过后续 10xxxxxx 续字节)。
            out[n] = b'?';
            n += 1;
            i += 1;
            while i < name.len() && name[i] & 0xC0 == 0x80 {
                i += 1;
            }
        }
    }
    print(unsafe { core::str::from_utf8_unchecked(&out[..n]) });
}

/// 打印目录条目名: 有条目长名 (VFAT / ext2) 就用长名, 否则退回 8.3 短名。
fn print_entry_name(de: &vfs::DirEntry) {
    if de.long_len > 0 {
        print_long_name(&de.long[..de.long_len as usize]);
    } else {
        print_83_name(&de.name);
    }
}

/// 把 8.3 短名格式化为 "主名[.扩展]" 并打印 (去尾随空格)。
fn print_83_name(name: &[u8; 11]) {
    let mut out = [0u8; 13];
    let mut n = 0usize;

    let base = &name[..8];
    let ext = &name[8..11];

    let mut end = 8;
    while end > 0 && base[end - 1] == b' ' {
        end -= 1;
    }
    out[n..n + end].copy_from_slice(&base[..end]);
    n += end;

    if ext[0] != b' ' {
        out[n] = b'.';
        n += 1;
        let mut eend = 3;
        while eend > 0 && ext[eend - 1] == b' ' {
            eend -= 1;
        }
        out[n..n + eend].copy_from_slice(&ext[..eend]);
        n += eend;
    }

    let s = unsafe { core::str::from_utf8_unchecked(&out[..n]) };
    print(s);
}
