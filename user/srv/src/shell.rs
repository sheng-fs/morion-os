use crate::common::*;
use morion::syscall::*;
use morion::{syscall, vfs};

// 包管理 MVP (`pkg`)。放在 shell 模块内, 不新增 crate / lib.rs 条目。
#[path = "pkg.rs"]
mod pkg;

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
/// 进程内环境变量槽数 (最小实现: 固定槽位, 不分配堆)。
const ENV_SLOTS: usize = 8;
/// 环境变量名 / 值最大长度。
const ENV_NAME_MAX: usize = 24;
const ENV_VAL_MAX: usize = 64;

/// 一条环境变量 (`name_len == 0` = 空槽)。
#[derive(Clone, Copy)]
struct EnvVar {
    name_len: u8,
    val_len: u8,
    name: [u8; ENV_NAME_MAX],
    val: [u8; ENV_VAL_MAX],
}

const ENV_EMPTY: EnvVar = EnvVar {
    name_len: 0,
    val_len: 0,
    name: [0; ENV_NAME_MAX],
    val: [0; ENV_VAL_MAX],
};

// ---------------------------------------------------------------------------
// 账户数据库与会话 (login / passwd / su / logout)
// ---------------------------------------------------------------------------
//
// 账户文件存放在 MorionFS 的根目录, 由 shell 在启动时**额外**把 MFS 根挂到 `/etc`
// (MFS 本身仍挂在 `/mfs`), 于是对用户可见的路径就是 `/etc/passwd` 与 `/etc/shadow`:
//
//   /etc/passwd   name:uid:gid:home        (每行一个账户)
//   /etc/shadow   name:salt:hash           (salt / hash 为 16 位十六进制)
//
// 权限: `passwd` 0644 (所有用户可读, 登录需要), `shadow` 0600 且属主 0:0 —— 只有
// uid 0 能读。文件服务按**真实内核身份**判定 (见 mfs_srv 的 `mfs_check_access`),
// 所以 uid 1000 的进程读 `/etc/shadow` 会拿到 `EACCES`。
//
// 哈希: 加盐 FNV-1a 64 + 64 轮混合。**非密码学强度, 仅占位** —— 没有随机源、没有
// 内存硬化、也没有抗 GPU 的设计; 真做认证应换成 Argon2/scrypt 一类的 KDF。
const ACCT_DIR: &str = "/etc";
const ACCT_PASSWD: &str = "/etc/passwd";
const ACCT_SHADOW: &str = "/etc/shadow";
/// 账户表最大条目数 (无堆分配, 固定槽位)。
const ACCT_MAX: usize = 8;
/// 用户名最大长度 (`/etc/passwd` 首字段)。
const ACCT_NAME_MAX: usize = 16;
/// 家目录最大长度。
const ACCT_HOME_MAX: usize = 96;
/// `/etc/passwd` 读取上限。
const PASSWD_FILE_MAX: usize = 512;
/// `/etc/shadow` 读取上限。
const SHADOW_FILE_MAX: usize = 1024;
/// 会话栈深度 (root → user → root 再加一层余量)。
const SESSION_MAX: usize = 4;
/// 口令 / 用户名的单次输入上限。
const SECRET_MAX: usize = 64;
/// 认证失败次数上限; 到顶后放弃并回到当前会话。
const AUTH_TRIES: u32 = 3;

/// 一条账户记录 (`/etc/passwd` 的一行)。
#[derive(Clone, Copy)]
struct Acct {
    name_len: u8,
    name: [u8; ACCT_NAME_MAX],
    uid: u16,
    gid: u16,
    home_len: u8,
    home: [u8; ACCT_HOME_MAX],
}

const ACCT_EMPTY: Acct = Acct {
    name_len: 0,
    name: [0; ACCT_NAME_MAX],
    uid: 0,
    gid: 0,
    home_len: 0,
    home: [0; ACCT_HOME_MAX],
};

impl Acct {
    fn name(&self) -> &str {
        unsafe { core::str::from_utf8_unchecked(&self.name[..self.name_len as usize]) }
    }
    fn home(&self) -> &str {
        unsafe { core::str::from_utf8_unchecked(&self.home[..self.home_len as usize]) }
    }
}

/// 一个登录会话: 身份 + 家目录 + 当时的 cwd (logout 时原样恢复)。
#[derive(Clone, Copy)]
struct Session {
    uid: u16,
    gid: u16,
    name_len: u8,
    name: [u8; ACCT_NAME_MAX],
    home_len: u8,
    home: [u8; ACCT_HOME_MAX],
    cwd_len: u8,
    cwd: [u8; CWD_MAX],
}

const SESSION_EMPTY: Session = Session {
    uid: 0,
    gid: 0,
    name_len: 0,
    name: [0; ACCT_NAME_MAX],
    home_len: 0,
    home: [0; ACCT_HOME_MAX],
    cwd_len: 1,
    cwd: [0; CWD_MAX],
};

impl Session {
    fn name(&self) -> &str {
        unsafe { core::str::from_utf8_unchecked(&self.name[..self.name_len as usize]) }
    }
    fn home(&self) -> &str {
        unsafe { core::str::from_utf8_unchecked(&self.home[..self.home_len as usize]) }
    }
    fn cwd_str(&self) -> &str {
        unsafe { core::str::from_utf8_unchecked(&self.cwd[..self.cwd_len as usize]) }
    }
}

/// 由账户记录生成会话 (cwd 暂置 `/`, 由 `apply_session` 切家目录后再同步)。
fn session_of(a: &Acct) -> Session {
    let mut s = SESSION_EMPTY;
    s.uid = a.uid;
    s.gid = a.gid;
    s.name_len = a.name_len;
    s.name = a.name;
    s.home_len = a.home_len;
    s.home = a.home;
    s.cwd[0] = b'/';
    s.cwd_len = 1;
    s
}

/// shell 运行时状态: 当前工作目录 (绝对路径, 以 '/' 开头) 与环境变量表。
struct ShellState {
    cwd: [u8; CWD_MAX],
    cwd_len: usize,
    env: [EnvVar; ENV_SLOTS],
    /// 当前会话身份。
    session: Session,
    /// 上一个会话 (logout 恢复用)。
    stack: [Session; SESSION_MAX],
    depth: usize,
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

    fn env_find(&self, name: &str) -> Option<usize> {
        let b = name.as_bytes();
        self.env
            .iter()
            .position(|e| e.name_len as usize == b.len() && &e.name[..b.len()] == b)
    }

    /// 写入 / 覆盖一条环境变量; 名字或值过长 (或槽位已满) 返回 false。
    fn env_set(&mut self, name: &str, val: &str) -> bool {
        let nb = name.as_bytes();
        let vb = val.as_bytes();
        if nb.is_empty() || nb.len() > ENV_NAME_MAX || vb.len() > ENV_VAL_MAX {
            return false;
        }
        let idx = match self.env_find(name) {
            Some(i) => i,
            None => match self.env.iter().position(|e| e.name_len == 0) {
                Some(i) => i,
                None => return false,
            },
        };
        let e = &mut self.env[idx];
        e.name = [0; ENV_NAME_MAX];
        e.val = [0; ENV_VAL_MAX];
        e.name[..nb.len()].copy_from_slice(nb);
        e.val[..vb.len()].copy_from_slice(vb);
        e.name_len = nb.len() as u8;
        e.val_len = vb.len() as u8;
        true
    }

    /// 把会话身份写进 `USER` / `HOME` (先拷到栈上, 避免同时借用 `self.session`)。
    fn sync_session_env(&mut self) {
        let mut uname = [0u8; ACCT_NAME_MAX];
        let un = self.session.name_len as usize;
        uname[..un].copy_from_slice(&self.session.name[..un]);
        let mut uhome = [0u8; ACCT_HOME_MAX];
        let hn = self.session.home_len as usize;
        uhome[..hn].copy_from_slice(&self.session.home[..hn]);
        let uname_s = unsafe { core::str::from_utf8_unchecked(&uname[..un]) };
        let home_s = unsafe { core::str::from_utf8_unchecked(&uhome[..hn]) };
        self.env_set("USER", uname_s);
        self.env_set("HOME", home_s);
    }
}

/// 解析结果静态缓冲 (单域单线程; 调用方须在下次解析前消费返回的字符串)。
static mut PATH_BUF: [u8; PATH_MAX] = [0u8; PATH_MAX];

/// `run <名字>` 的 `/apps/` 回退缓冲。真机镜像把应用放在 `/apps/` 下，
/// 而用户习惯直接敲 `run calc`，所以要能补全目录与 `.mex` 后缀。
static mut APP_BUF: [u8; PATH_MAX] = [0u8; PATH_MAX];

/// 拼一个应用候选路径 `/apps/<name><suffix>`，写入静态缓冲。
fn app_candidate(name: &str, suffix: &str) -> Option<&'static str> {
    unsafe {
        let buf = &mut *core::ptr::addr_of_mut!(APP_BUF);
        let mut n = 0usize;
        for part in ["/apps/", name, suffix] {
            let b = part.as_bytes();
            if n + b.len() > buf.len() {
                return None;
            }
            buf[n..n + b.len()].copy_from_slice(b);
            n += b.len();
        }
        Some(core::str::from_utf8_unchecked(&buf[..n]))
    }
}

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

/// 等屏幕控制台就绪的上限 (毫秒)。`gfx_srv` 接管帧缓冲的实测量级是几十毫秒; 这个上限只是
/// 「最坏情况别把 shell 卡住」的保险 —— 到点就退回"只写串口/内核终端"的老路。
const CONSOLE_WAIT_MS: u64 = 1000;

/// shell 命令名（Tab 补全用；与 [`shell_exec`] 的 match 保持一致）。
const COMMANDS: &[&str] = &[
    "help",
    "echo",
    "uname",
    "version",
    "pwd",
    "whoami",
    "id",
    "su",
    "login",
    "passwd",
    "logout",
    "exit",
    "env",
    "export",
    "ls",
    "cat",
    "run",
    "wget",
    "net",
    "ifconfig",
    "ping",
    "ping6",
    "dns",
    "nslookup",
    "wifi",
    "lspci",
    "dmesg",
    "cd",
    "mkdir",
    "touch",
    "rm",
    "mv",
    "ln",
    "chmod",
    "chown",
    "truncate",
    "stat",
    "lstat",
    "readlink",
    "mkfs.mfs",
    "mfs.primary",
    "mfs.snap",
    "mfs.snaps",
    "mfs.rollback",
    "mfs.gc",
    "mfs.fsck",
    "mfs.sync",
    "df",
    "pkg",
    "part.create",
    "part.del",
    "part.wipe",
    "part.reload",
    "clear",
    "poweroff",
    "halt",
    "shutdown",
    "reboot",
];

/// 域 8 — Shell: 分配结果页 → 循环「提示符 → 读行 → 执行命令」。
/// shell 主入口（G4：输入完全在用户态，内核只搬按键字节）。
pub fn run() {
    // 分配 shell 专用结果页并共享给各文件服务 (同地址映射); ls/cat 的结果写在此页。
    if sys_alloc_page(vfs::SHELL_RESULT_BUF) != 1 {
        println("shell: alloc result buf FAILED");
        return;
    }
    // 写入数据也必须走 shell 自己的共享写页：`vfs::write` 默认的 WRITE_BUF 是 app 的
    // 地址，shell 域没有映射/共享它；用错页的后果是文件数据全 0（目录项正常、内容为空）。
    if sys_alloc_page(vfs::SHELL_WRITE_BUF) != 1 {
        println("shell: alloc write buf FAILED");
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
    if sys_share_page(vfs::SHELL_WRITE_BUF, vfs::FAT32_DOMAIN) != 1
        || sys_share_page(vfs::SHELL_WRITE_BUF, vfs::TMPFS_DOMAIN) != 1
        || sys_share_page(vfs::SHELL_WRITE_BUF, vfs::MFS_DOMAIN) != 1
        || sys_share_page(vfs::SHELL_WRITE_BUF, vfs::EXT2_DOMAIN) != 1
        || sys_share_page(vfs::SHELL_WRITE_BUF, vfs::EXFAT_DOMAIN) != 1
    {
        println("shell: share write buf FAILED");
        return;
    }
    // G3b: 帧缓冲归用户态后, 把 shell 的输出**镜像**一份到屏幕控制台 (`gfx_srv`) —— 内核终端
    // 与 COM1 那条路照旧, 屏幕只是"多出来的一份" (D6: 串口是回归的命脉, 绝不能替换掉)。
    //
    // `gfx_srv` 是后启动的服务域 (域 15), 帧缓冲要等它接管之后才写得进去, 故在这里**有界等待**
    // 一下; 上限到点就走"没有屏幕"的老路 —— 绝不能因为它没起来就把 shell 卡死。
    //
    // V2 无图形变体 (`NOGUI`): 不等控制台、不开屏幕镜像, 输入/回显直接走串口
    // (G4 的 `readline` 本就只依赖 `SYS_KEY_READ` + `sink`, 不依赖 `gfx_srv`)。
    let mut mirrored = false;
    if NOGUI {
        println("shell: no-gui build (screen mirror off; serial console only)");
    } else {
        let mut waits = 0u64;
        while waits < CONSOLE_WAIT_MS && !sys_console_ready() {
            sys_sleep(1);
            waits += 1;
        }
        mirrored = sys_console_ready() && sys_domain_alive(morion::gfx::GFX_DOMAIN) != 0;
        if mirrored {
            // 镜像走 `SYS_CALL`, 目标域没有活任务时会一直等回复, 故上面先确认它活着。
            screen_mirror_on();
        }
    }

    println("shell: type 'help' for commands");
    // V1 取证: 启动即报告版本串 —— 自动化回归无需交互就能在串口日志里读到,
    // 变体后缀 (`-nogui`) 与构建号也在其中 (交互式 `uname` / `version` 打同一份串)。
    shell_version();
    // 用户态打印中文: 一份经 `SYS_PUTS` 给内核终端 (内核顺带写 COM1), 开了屏幕镜像再给
    // `gfx_srv` 一份 —— Ring 3 的 UTF-8 一路走到 16x16 点阵字形 (全角标点是双宽度)。
    println("你好，世界！MorionOS 终端支持中文、全角标点与宽窄混排。");

    // G3b 自证 (串口可见, 不占屏幕控制台): 上面两行已过屏幕镜像写进 `gfx_srv` 的终端,
    // 问一下它的光标就知道字真的落了笔 —— 盲测环境下这就是"shell 输出上屏"的证据。
    if mirrored {
        match morion::gfx::cursor() {
            Some((_, row)) if row > 0 => {
                sys_puts("shell: screen console mirror OK (gfx_srv cursor advanced)\n")
            }
            _ => sys_puts("shell: screen console mirror FAILED (gfx_srv cursor not advanced)\n"),
        }
    }

    // 初始工作目录为根, 初始身份由内核给出 (引导期服务域 = root)。
    let mut st = ShellState {
        cwd: [0; CWD_MAX],
        cwd_len: 1,
        env: [ENV_EMPTY; ENV_SLOTS],
        session: SESSION_EMPTY,
        stack: [SESSION_EMPTY; SESSION_MAX],
        depth: 0,
    };
    st.cwd[0] = b'/';
    st.env_set("PATH", "/");

    // 账户数据库: 挂到 `/etc` 并首次生成默认 root / user。
    // 挂载点 `/etc` 由镜像构建时预创建（见 Makefile 的 ESP 目录），运行期不再 mkdir：
    // 真机启动早期文件服务可能正忙，逐目录探测会把 shell 卡在 SYS_CALL 上。
    shell_setup_accounts(true);

    let start = acct_find_uid(0).unwrap_or_else(|| {
        let mut a = ACCT_EMPTY;
        a.uid = 0;
        a.gid = 0;
        let n = b"root";
        a.name[..n.len()].copy_from_slice(n);
        a.name_len = n.len() as u8;
        let h = b"/home/root";
        a.home[..h.len()].copy_from_slice(h);
        a.home_len = h.len() as u8;
        a
    });
    st.session = session_of(&start);
    st.session.cwd[0] = b'/';
    st.session.cwd_len = 1;
    st.sync_session_env();

    // G4: **输入完全在用户态** —— 内核只把按键字节搬进队列, 行编辑/回显由
    // `morion::console::readline` 做 (它经 `SYS_KEY_READ` 阻塞, 有键才醒)。
    // 回显走 `SYS_PUTS`, 内核终端与串口都能看到 —— 不依赖 `gfx_srv` 是否接管帧缓冲。
    //
    // SAFE 变体 (真机) 下 `gfx_srv` 不接管帧缓冲, 屏幕归**内核终端**: 输出经
    // `SYS_PUTS` 照样上屏, 输入照旧来自 `kbd_srv` 的按键队列 —— 所以这里绝不能退出。
    // 从前直接 `return` 的后果就是真机永远没有命令行 (屏幕上只剩内核日志)。
    if !mirrored && !NOGUI {
        println("shell: gfx console not taken over - using kernel terminal");
    }

    let mut line = [0u8; SHELL_LINE_MAX];
    // 提示符先拼成一个字符串：Tab 补全需要换行重画时由 readline 原样再打一遍。
    let mut prompt = [0u8; 128];
    loop {
        let mut plen = 0usize;
        for &b in b"[morion@morion " {
            prompt[plen] = b;
            plen += 1;
        }
        for &b in st.cwd_str().as_bytes() {
            if plen < prompt.len() {
                prompt[plen] = b;
                plen += 1;
            }
        }
        for &b in b"]$ " {
            if plen < prompt.len() {
                prompt[plen] = b;
                plen += 1;
            }
        }
        let prompt = unsafe { core::str::from_utf8_unchecked(&prompt[..plen]) };
        print(prompt);
        syscall::flush();

        let n = morion::console::readline(&mut line, prompt, COMMANDS);
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
            println("  uname          print system name / release / machine");
            println("  version        print version and build");
            println("  pwd            print working directory");
            println("  whoami         print the current user name");
            println("  id             print uid / gid of this process");
            println("  su [user|uid]  switch identity (target password; root may skip with an empty one)");
            println("  login [user]   log in as a user (asks for that user's password)");
            println("  passwd [user]  change a password (own password needs the old one)");
            println("  logout         return to the previous session (alias: exit)");
            println("  env            print environment variables");
            println("  export K=V     set an environment variable");
            println("  ls [-l] [path] list directory (-l: long form)");
            println("  cat <file>     print file content");
            println("  run <file>     load a .mex program from a file and run it (new domain)");
            println("  wget [path]    fetch a page from the guest's built-in HTTP server (10.0.2.15:80)");
            println("  net            show link status (kind / mac / ipv4 / gw / ipv6)");
            println("  ping <ip|host> ICMP echo over IPv4 (IPv6 literal delegates to ping6)");
            println("  ping6 <ip|host>  ICMPv6 echo over IPv6");
            println("  dns <name>     resolve A and AAAA records (alias: nslookup)");
            println(
                "  wifi [status|scan|connect <ssid> [psk]]  wireless station (abstraction layer)",
            );
            println("  lspci          list PCI devices seen by the kernel (vendor:device:class)");
            println(
                "  dmesg [-d ms] [-a] [-g pat] [path]  console log; slow per-line by default (-d ms = delay, -a = full speed, -g pat = only lines containing pat), path = write to file",
            );
            println("  cd [path]      change directory (default: /)");
            println("  mkdir <path>   create directory");
            println("  touch <file>   create empty file");
            println("  rm <path>      remove file / empty directory");
            println("  mv <src> <dst> rename / move (same filesystem)");
            println("  ln <src> <dst> hard link an existing file (same filesystem)");
            println("  ln -s <target> <name>  symbolic link (target kept verbatim; MFS only)");
            println("  chmod <mode> <path>  set permission bits (octal; enforced since 04b)");
            println("  chown <uid>:<gid> <path>  change owner/group (MFS; uid 0 only)");
            println("  truncate <file> <size>  resize a file (sparse on grow)");
            println("  stat <path>    show metadata (mode / owner / links / times)");
            println("  lstat <path>   like stat but on the link itself (no follow)");
            println("  readlink <link>  print a symbolic link's target (no follow)");
            println(
                "  mkfs.mfs <vol> [--force]  create a MorionFS filesystem on a volume (ERASES it)",
            );
            println("  mfs.primary <vol>   mark a MorionFS volume primary (keeps data)");
            println("  mfs.snap       take a MorionFS snapshot (COW root + generation)");
            println("  mfs.snaps      list MorionFS snapshots (index / generation)");
            println("  mfs.rollback <index>  roll the mounted MorionFS back to a snapshot");
            println("  mfs.gc         reclaim unreachable MorionFS blocks (keeps snapshots)");
            println("  mfs.fsck [--repair]  reconcile MFS inodes (report only; --repair reclaims)");
            println("  mfs.sync       flush MorionFS to disk now (prints the on-disk generation)");
            println("  df             show MorionFS space usage (/mfs)");
            println("  pkg build <dir> <out.mpk>  pack a directory into an .mpk archive");
            println("  pkg install <.mpk>  verify + unpack to /nix/store, then add a generation");
            println("  pkg remove <name>  drop a package from the current generation");
            println("  pkg list       list packages in the current generation");
            println("  pkg info <name>  show store path / hash / manifest");
            println("  pkg generations  list kept generations");
            println("  pkg rollback   switch to the previous generation");
            println("  pkg switch <N>  switch to generation N");
            println("  pkg gc         delete store paths no generation references");
            println("  pkg selftest   run the package-manager end-to-end self test");
            println(
                "  part.create <nsid> <MiB> [mbr]  create a partition (disk-wide, blank = GPT)",
            );
            println("  part.del <nsid> <index>  delete a partition entry (keeps data)");
            println("  part.wipe <nsid>   clear the partition table (disk becomes blank)");
            println("  part.reload      re-read all partition tables");
            println("  clear          clear screen");
            println("  poweroff       power off the machine (ACPI S5; alias: halt/shutdown)");
            println("  reboot         reset and restart the machine");
            println("  (Ctrl+C: cancel the current input line, or abort a long dump like dmesg)");
            println("  (mounts: / = fat32, /tmp = tmpfs, /mfs = MorionFS, /ext2 = ext2 ro, /usb = exFAT)");
            println(
                "  (extra volumes auto-mounted as /usb<N>, N = volume id in the boot volume list)",
            );
        }
        "echo" => println(arg),
        "uname" => shell_uname(),
        "version" => shell_version(),
        "pwd" => println(st.cwd_str()),
        "whoami" => shell_whoami(st),
        "id" => shell_id(st),
        "su" => shell_su(st, arg),
        "login" => shell_login(st, arg),
        "passwd" => shell_passwd(st, arg),
        "logout" | "exit" => shell_logout(st),
        "env" => shell_env(st),
        "export" => shell_export(st, arg),
        "ls" => shell_ls(st, if arg.is_empty() { "." } else { arg }),
        "cat" => shell_cat(st, arg),
        "run" => shell_run(st, arg),
        "wget" => shell_wget(arg),
        "net" | "ifconfig" => shell_net(),
        "ping" => shell_ping(arg),
        "ping6" => shell_ping6(arg),
        "dns" | "nslookup" => shell_dns(arg),
        "wifi" => shell_wifi(arg),
        "lspci" => shell_lspci(),
        "dmesg" => shell_dmesg(st, arg),
        "cd" => shell_cd(st, if arg.is_empty() { "/" } else { arg }),
        "mkdir" => shell_mkdir(st, arg),
        "touch" => shell_touch(st, arg),
        "rm" => shell_rm(st, arg),
        "mv" => shell_mv(st, arg),
        "ln" => shell_ln(st, arg),
        "chmod" => shell_chmod(st, arg),
        "chown" => shell_chown(st, arg),
        "truncate" => shell_truncate(st, arg),
        "stat" => shell_stat(st, arg, false),
        "lstat" => shell_stat(st, arg, true),
        "readlink" => shell_readlink(st, arg),
        "mkfs.mfs" => shell_mkfs(arg),
        "mfs.primary" => shell_mfs_primary(arg),
        "mfs.snap" => shell_mfs_snap(arg),
        "mfs.snaps" => shell_mfs_snaps(arg),
        "mfs.rollback" => shell_mfs_rollback(arg),
        "mfs.gc" => shell_mfs_gc(arg),
        "mfs.fsck" => shell_mfs_fsck(arg),
        "mfs.sync" => shell_mfs_sync(arg),
        "df" => shell_df(arg),
        "pkg" => pkg::run(st.cwd_str(), arg),
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
            // 屏幕已交 gfx_srv 时清的是**屏幕控制台** (内核终端此时根本不画屏);
            // 没交出去 (极早期) 才回落到内核终端。
            if sys_console_ready() {
                morion::gfx::clear_screen();
            } else {
                sys_clear();
            }
        }
        "poweroff" | "halt" | "shutdown" => {
            println("poweroff: ACPI S5 ...");
            sys_poweroff();
            println("poweroff: FAILED (firmware has no usable ACPI S5)");
        }
        "reboot" => {
            println("reboot: resetting ...");
            sys_reboot();
            println("reboot: FAILED (no usable reset path)");
        }
        _ => {
            print("shell: unknown command: ");
            println(cmd);
        }
    }
}

/// 逐字节打印（仅可打印 ASCII 与换行；其余跳过），避免对非 UTF-8 数据用 `from_utf8_unchecked`。
fn print_bytes(b: &[u8]) {
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c == b'\n' || c == b'\r' || (0x20..0x7f).contains(&c) {
            let one = [c];
            unsafe {
                print(core::str::from_utf8_unchecked(&one));
            }
        }
        i += 1;
    }
}

/// `wget <path>` — 从客户机**内建 HTTP 服务**（`10.0.2.15:80`，见独立服务 httpd_srv）取一个页面并打印。
///
/// 走完整的 TCP 客户端路径（socket/bind/connect/send/recv/close），握手与数据经栈内**回环**
/// 与服务端完成 —— 无需任何外部服务端。
fn shell_wget(arg: &str) {
    let path = if arg.is_empty() { "/" } else { arg };
    let sock = morion::net::tcp_socket();
    if sock == 0 || !morion::net::tcp_bind(sock, 12349) {
        println("wget: socket/bind failed");
        return;
    }
    if !morion::net::tcp_connect(sock, morion::net::ip4(10, 0, 2, 15), 80) {
        println("wget: connect failed");
        let _ = morion::net::tcp_close(sock);
        return;
    }
    // 组装 "GET <path> HTTP/1.0\r\nHost: 10.0.2.15\r\n\r\n"。
    let mut req = [0u8; 160];
    let mut n = 0usize;
    for &b in b"GET " {
        req[n] = b;
        n += 1;
    }
    for &b in path.as_bytes() {
        if n < req.len() - 30 {
            req[n] = b;
            n += 1;
        }
    }
    for &b in b" HTTP/1.0\r\nHost: 10.0.2.15\r\n\r\n" {
        if n < req.len() {
            req[n] = b;
            n += 1;
        }
    }
    if !morion::net::tcp_send(sock, &req[..n]) {
        println("wget: send failed");
        let _ = morion::net::tcp_close(sock);
        return;
    }
    let mut buf = [0u8; 1024];
    let mut i = 0;
    while i < 50 {
        let r = morion::net::tcp_recv(sock, &mut buf);
        if r > 0 {
            print_bytes(&buf[..r as usize]);
            break;
        }
        sys_sleep(20);
        i += 1;
    }
    println("");
    let _ = morion::net::tcp_close(sock);
}

// ---------------------------------------------------------------------------
// 网络命令（net / ping / ping6 / dns）
// ---------------------------------------------------------------------------

/// 打印点分十进制 IPv4。
fn print_ipv4(ip: [u8; 4]) {
    print_u64(ip[0] as u64);
    print(".");
    print_u64(ip[1] as u64);
    print(".");
    print_u64(ip[2] as u64);
    print(".");
    print_u64(ip[3] as u64);
}

/// 打印一个 MAC（每字节 2 位 16 进制，冒号分隔）。
fn print_mac(mac: [u8; 6]) {
    let mut i = 0usize;
    while i < 6 {
        if i != 0 {
            print(":");
        }
        print_hex((mac[i] >> 4) as u64);
        print_hex((mac[i] & 0xF) as u64);
        i += 1;
    }
}

/// 打印 IPv6（8 组 16 进制，冒号分隔；与协议栈口径一致，不做 `::` 压缩）。
fn print_ipv6(a: &[u8; 16]) {
    let mut i = 0usize;
    while i < 8 {
        print_hex(((a[i * 2] as u64) << 8) | a[i * 2 + 1] as u64);
        if i != 7 {
            print(":");
        }
        i += 1;
    }
}

/// `net` / `ifconfig` — 打印各网卡的链路状态（型号 / MAC / IPv4 / 网关 / IPv6）。
fn shell_net() {
    let mut infos = [morion::net::LinkInfo::zeroed(); 8];
    let n = morion::net::link_infos(&mut infos);
    if n == 0 {
        println("net: no link (netstack unavailable?)");
        return;
    }
    let mut i = 0usize;
    while i < n {
        let l = &infos[i];
        print("nic");
        print_u64(l.nic);
        print(" ");
        print(l.kind_name());
        print(" ");
        print(if l.is_up() { "up" } else { "down" });
        print(" mac=");
        print_mac(l.mac_bytes());
        print(" ipv4=");
        print_ipv4(l.ipv4());
        print(" gw=");
        print_ipv4(l.gw4());
        if l.v6_up != 0 {
            print(" ipv6=");
            print_ipv6(&l.v6);
            print(" router=");
            print_ipv6(&l.v6_gw);
        }
        println("");
        i += 1;
    }
}

/// 打印固定 `digits` 位十六进制 (PCI id / class 用; 小写, 补前导零)。
fn print_hex_pad(mut v: u64, digits: usize) {
    let mut buf = [0u8; 16];
    let mut i = buf.len();
    for _ in 0..digits {
        let d = (v & 0xF) as u8;
        i -= 1;
        buf[i] = if d < 10 { b'0' + d } else { b'a' + d - 10 };
        v >>= 4;
    }
    print(unsafe { core::str::from_utf8_unchecked(&buf[i..]) });
}

/// 读一段日志，但**绝不越过 `end`**（命令开始时的快照）。
///
/// 两个坑：
/// - 打印本身会继续写进同一个环形缓冲，若"一直读到没数据"就会把自己的输出再读一遍
///   → 自我循环（`dmesg` 越跑越长）；
/// - 缓冲满后最老字节被挤掉，内核会把过早的起始偏移夹到最老处；调用方若仍按
///   "偏移 += 读到的字节数"前进，就会原地反复读同一段 → 同一块日志被重复打印。
///   故每轮先把 `off` 抬到内核给出的最老偏移。
fn log_read_upto(off: &mut u64, end: u64, buf: &mut [u8]) -> usize {
    let oldest = sys_log_oldest();
    if *off < oldest {
        *off = oldest;
    }
    if *off >= end {
        return 0;
    }
    let left = (end - *off) as usize;
    let want = if left < buf.len() { left } else { buf.len() };
    let n = sys_log_read(*off, &mut buf[..want]);
    *off += n as u64;
    n
}

/// `dmesg [-d ms] [-a] [-g pat] [path]` — 打印 / 导出控制台日志环形缓冲（Phase 0 / P0.1）。
///
/// 逐行打印（默认每行停顿 `-d` 毫秒，便于按 Ctrl+C 中止长日志）；`-a` 全速、
/// `-g pat` 只打含该子串的行并报告匹配行数。带 `path`: 把日志原样写入该文件
/// （如 `dmesg /boot.log`），供真机拔盘取走。日志环形缓冲满后挤掉最老字节，
/// 故保留的是**最近的** 64 KiB。
///
/// **只输出「命令开始执行之前」的日志**：本命令自己的输出也会写进同一个环形缓冲，
/// 若按「一直读到没数据为止」，就会把自己的输出再读出来（自我循环，越跑越长）。
/// 故开头先做一次 `total` 快照，读到快照为止。
fn shell_dmesg(st: &ShellState, arg: &str) {
    static mut BUF: [u8; 4096] = [0u8; 4096];
    static mut LINE: [u8; 512] = [0u8; 512];
    let buf = unsafe { &mut *core::ptr::addr_of_mut!(BUF) };
    let line = unsafe { &mut *core::ptr::addr_of_mut!(LINE) };
    // 快照: 只读到这里为止（之后的字节都是本命令自己的输出）。
    let end = sys_log_total();
    if end == 0 {
        println("dmesg: log is empty");
        return;
    }

    // 选项解析: `-a` 全速、`-d <ms>` 行间延迟、`-g <pat>` 过滤; 其余位置参数是导出路径。
    let mut fast = false;
    let mut delay_ms = 5u64;
    let mut pattern: Option<&str> = None;
    let mut path_arg: Option<&str> = None;
    let mut it = arg.split_whitespace();
    while let Some(tok) = it.next() {
        match tok {
            "-a" | "--all" => fast = true,
            "-d" => match it.next().and_then(parse_dec) {
                Some(ms) => delay_ms = ms,
                None => {
                    println("dmesg: -d needs a millisecond count (e.g. dmesg -d 200)");
                    return;
                }
            },
            "-g" => match it.next() {
                Some(p) => pattern = Some(p),
                None => {
                    println("dmesg: -g needs a substring (e.g. dmesg -g xhci)");
                    return;
                }
            },
            _ => path_arg = Some(tok),
        }
    }

    // 导出模式: 原样写整段日志（真机把日志带出机器的唯一通路）。
    if let Some(p) = path_arg {
        let path = match resolve_in_cwd(st.cwd_str(), p) {
            Some(p) => p,
            None => {
                println("dmesg: path too long");
                return;
            }
        };
        // 覆盖写: `creat` 对已存在的文件是"打开且不截断", 会留下旧尾巴, 故先删。
        vfs::unlink(path);
        let fd = vfs::creat(path);
        if fd == u64::MAX {
            print("dmesg: cannot create ");
            println(path);
            return;
        }
        let mut off = 0u64; // 日志绝对偏移
        let mut woff = 0u64; // 文件内偏移
        loop {
            let n = log_read_upto(&mut off, end, buf);
            if n == 0 {
                break;
            }
            let w = vfs::write_into(fd, woff, &buf[..n], vfs::SHELL_WRITE_BUF);
            if w != n as u64 {
                println("dmesg: write FAILED (dumped prefix only)");
                break;
            }
            woff += w;
        }
        vfs::close(fd);
        print("dmesg: wrote ");
        print_u64(woff);
        print(" bytes to ");
        println(path);
        return;
    }

    // 行模式: 逐行输出, 支持 -g 过滤、-d 节流与 Ctrl+C 中止。
    let mut off = 0u64;
    let mut got = 0u64;
    let mut matched = 0u64;
    let mut ll = 0usize;
    loop {
        let n = log_read_upto(&mut off, end, buf);
        if n == 0 {
            break;
        }
        got += n as u64;
        for &b in &buf[..n] {
            if b == b'\n' {
                let show = match pattern {
                    None => true,
                    Some(p) => bytes_contains(&line[..ll], p.as_bytes()),
                };
                if show {
                    print_bytes(&line[..ll]);
                    println("");
                    matched += 1;
                    if !fast {
                        sys_sleep(delay_ms);
                    }
                }
                ll = 0;
                // 只在行边界检查按键: dmesg 是诊断命令, Ctrl+C 必须能中止长输出。
                if let Some(0x03) = sys_key_try_read() {
                    println("^C");
                    return;
                }
            } else if ll < line.len() {
                line[ll] = b;
                ll += 1;
            }
        }
    }
    if ll > 0 {
        let show = match pattern {
            None => true,
            Some(p) => bytes_contains(&line[..ll], p.as_bytes()),
        };
        if show {
            print_bytes(&line[..ll]);
            println("");
            matched += 1;
        }
    }
    if pattern.is_some() {
        print("dmesg: ");
        print_u64(matched);
        println(" lines matched");
    } else {
        print("dmesg: ");
        print_u64(got);
        println(" bytes");
    }
}

/// 判断 `needle` 是否出现在 `haystack` 里（空 pattern 恒匹配）。
fn bytes_contains(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// `lspci` — 列出启动期枚举到的 PCI 设备（Phase 0 / P0.3）。
///
/// 真机意义: 让「内核看得见、但还没有驱动的硬件」（如 Intel AX210 `8086:2725`、
/// Realtek RTL8111 `10ec:8168`）成为可带走的日志证据。
fn shell_lspci() {
    let mut i = 0u64;
    let mut n = 0u64;
    while let Some(rec) = sys_pci_info(i) {
        let ids = rec[0];
        let loc = rec[1];
        let bar0 = rec[2];
        let vendor = (ids & 0xFFFF) as u32;
        let device = ((ids >> 16) & 0xFFFF) as u32;
        let class = (loc & 0xFF) as u32;
        let subclass = ((loc >> 8) & 0xFF) as u32;
        let progif = ((loc >> 16) & 0xFF) as u32;
        let bus = ((loc >> 24) & 0xFF) as u32;
        let dev = ((loc >> 32) & 0xFF) as u32;
        let func = ((loc >> 40) & 0xFF) as u32;
        print_hex_pad(bus as u64, 2);
        print(":");
        print_hex_pad(dev as u64, 2);
        print(".");
        print_hex_pad(func as u64, 1);
        print("  vend=");
        print_hex_pad(vendor as u64, 4);
        print(" dev=");
        print_hex_pad(device as u64, 4);
        print(" class=");
        print_hex_pad(class as u64, 2);
        print(":");
        print_hex_pad(subclass as u64, 2);
        print(":");
        print_hex_pad(progif as u64, 2);
        if bar0 != 0 {
            print(" bar0=0x");
            print_hex(bar0);
        }
        println("");
        i += 1;
        n += 1;
    }
    if n == 0 {
        println("lspci: no PCI devices recorded");
    } else {
        print("lspci: ");
        print_u64(n);
        println(" device(s)");
    }
}

/// `ping6 <ipv6>`（或 `ping <ipv6>`）—— ICMPv6 echo，经协议栈走真实网卡。
fn shell_ping6_addr(addr: &[u8; 16]) {
    print("ping6 ");
    print_ipv6(addr);
    print(" ... ");
    if morion::net::ping6(addr, 0) {
        println("reply");
    } else {
        println("no reply");
    }
}

/// `ping6 <ipv6>` / `ping6 <hostname>`。
fn shell_ping6(arg: &str) {
    if arg.is_empty() {
        println("usage: ping6 <ipv6|hostname>");
        return;
    }
    if let Some(a) = morion::net::parse_ipv6(arg) {
        shell_ping6_addr(&a);
        return;
    }
    let mut a = [0u8; 16];
    if morion::net::getaddrinfo6(arg, &mut a) {
        shell_ping6_addr(&a);
    } else {
        println("ping6: cannot resolve (need IPv6 literal or AAAA record)");
    }
}

/// `ping <ipv4|ipv6|hostname>` — IPv4 走 ICMP echo；v6 字面量自动转 `ping6`。
fn shell_ping(arg: &str) {
    if arg.is_empty() {
        println("usage: ping <ipv4|hostname>");
        return;
    }
    if let Some(a6) = morion::net::parse_ipv6(arg) {
        shell_ping6_addr(&a6);
        return;
    }
    let addr = match morion::net::parse_ipv4(arg) {
        Some(a) => a,
        None => match morion::net::resolve(arg) {
            Some(morion::net::IpAddr::V4(a)) => a,
            Some(morion::net::IpAddr::V6(a)) => {
                shell_ping6_addr(&a);
                return;
            }
            None => {
                println("ping: cannot resolve (need IPv4 literal or A record)");
                return;
            }
        },
    };
    print("ping ");
    print_ipv4(addr);
    print(" ... ");
    if morion::net::ping4(addr, 0) {
        println("reply");
    } else {
        println("no reply");
    }
}

/// `dns <name>` / `nslookup <name>` — 解析 A 与 AAAA 并打印。
fn shell_dns(name: &str) {
    if name.is_empty() {
        println("usage: dns <name>");
        return;
    }
    let mut v4 = [0u8; 4];
    if morion::net::getaddrinfo(name, &mut v4) {
        print("dns: ");
        print(name);
        print(" A = ");
        print_ipv4(v4);
        println("");
    } else {
        print("dns: ");
        print(name);
        println(" A = (none)");
    }
    let mut v6 = [0u8; 16];
    if morion::net::getaddrinfo6(name, &mut v6) {
        print("dns: ");
        print(name);
        print(" AAAA = ");
        print_ipv6(&v6);
        println("");
    } else {
        print("dns: ");
        print(name);
        println(" AAAA = (none)");
    }
}

/// 打印一段 ASCII 字节（非 ASCII 段跳过）。
fn print_ascii(b: &[u8]) {
    if let Ok(s) = core::str::from_utf8(b) {
        print(s);
    }
}

/// `wifi [status|scan|connect <ssid> [psk]]` —— 无线 station（`wifi_srv`，域 25）。
///
/// 本轮是**抽象层**（plan §3.4）：无 radio 时 status 报 `radio=0`、scan 返 0 条、
/// connect 恒失败。真机接入前后命令接口不变。
fn shell_wifi(arg: &str) {
    let (cmd, rest) = match arg.split_once(' ') {
        Some((c, r)) => (c, r.trim()),
        None => (arg, ""),
    };
    match cmd {
        "" | "status" => shell_wifi_status(),
        "scan" => shell_wifi_scan(),
        "connect" => shell_wifi_connect(rest),
        _ => println("usage: wifi [status|scan|connect <ssid> [psk]]"),
    }
}

fn shell_wifi_status() {
    match morion::wifi::status() {
        Some(st) => {
            print("wifi: radio=");
            print_u64(st.radio);
            print(" firmware=");
            print_u64(st.firmware);
            print(" associated=");
            print_u64(st.associated);
            if st.associated != 0 {
                print(" ssid=");
                print_ascii(st.ssid_str());
            }
            println("");
            if st.radio == 0 {
                println("wifi: no wireless radio on this machine (abstraction layer only)");
            }
        }
        None => println("wifi: wifi_srv unreachable"),
    }
}

fn shell_wifi_scan() {
    let mut bss = [morion::wifi::WifiBss::zeroed(); 16];
    let n = morion::wifi::scan(&mut bss);
    if n == 0 {
        println("wifi: 0 BSS (no radio / abstraction layer only)");
        return;
    }
    let mut i = 0usize;
    while i < n {
        let b = &bss[i];
        print("wifi: ssid=");
        print_ascii(b.ssid_str());
        print(" rssi=");
        print_u64(b.rssi);
        print(" ch=");
        print_u64(b.channel);
        println("");
        i += 1;
    }
}

fn shell_wifi_connect(rest: &str) {
    let (ssid, psk) = match rest.split_once(' ') {
        Some((s, p)) => (s.trim(), p.trim()),
        None => (rest.trim(), ""),
    };
    if ssid.is_empty() {
        println("usage: wifi connect <ssid> [psk]");
        return;
    }
    print("wifi: connecting ");
    print(ssid);
    print(" ... ");
    if morion::wifi::assoc(ssid.as_bytes(), psk.as_bytes()) {
        println("associated");
    } else {
        println("failed (no radio / abstraction layer only)");
    }
}

/// `uname` — 打印内核报告的整行 `MorionOS <release> <machine>`（V1）。
///
/// 串由内核 `version.rs` **单一维护**（见 `SYS_UNAME`），shell 只负责显示。
fn shell_uname() {
    let mut buf = [0u8; 96];
    let n = sys_uname(&mut buf, 0);
    if n == 0 {
        println("uname: SYS_UNAME failed");
        return;
    }
    println(unsafe { core::str::from_utf8_unchecked(&buf[..n as usize]) });
}

/// `version` — 打印版本号（含变体后缀）与构建号（V1 / V2）。
fn shell_version() {
    let mut buf = [0u8; 96];
    let n = sys_uname(&mut buf, 1);
    if n == 0 {
        println("version: SYS_UNAME failed");
        return;
    }
    print("MorionOS v");
    print(unsafe { core::str::from_utf8_unchecked(&buf[..n as usize]) });
    let m = sys_uname(&mut buf, 2);
    if m > 0 {
        print(" (build ");
        print(unsafe { core::str::from_utf8_unchecked(&buf[..m as usize]) });
        print(")");
    }
    println("");
}

// ---------------------------------------------------------------------------
// 多用户: 账户数据库 (/etc/passwd, /etc/shadow) + 会话 (login/passwd/su/logout)
// ---------------------------------------------------------------------------
//
// 设计说明 (重要):
//   * 文件服务按**真实内核身份**判定权限; 内核的 `SYS_CRED` 只允许**降权**
//     (uid 0 → 其它), 不允许在同一域内把身份再提回 0。
//   * 因此 shell 作为**会话管理器**保持引导期身份 (uid 0): 登录/切换只改变 shell
//     记录的会话身份与 USER/HOME/cwd。否则 root → user 之后 `logout` 永远回不到
//     root (内核会拒绝), `passwd` 也无法再写 0600 的 /etc/shadow。
//   * 账户文件的 0600 权限仍由 MFS 对**其它域**强制执行 (uid 1000 的应用读
//     /etc/shadow 会得到 EACCES); shell 侧再按会话身份拦一次 (见 `cat` 的守卫)。
//
// 口令哈希: 加盐 FNV-1a 64 位 + 64 轮混合 —— **非密码学强度, 仅占位**。

/// 从文件读入最多 `out.len()` 字节; 失败返回 `None`。
fn fs_read_file(path: &str, out: &mut [u8]) -> Option<usize> {
    let fd = vfs::open(path);
    if fd == u64::MAX {
        return None;
    }
    let mut off = 0usize;
    loop {
        let left = out.len() - off;
        if left == 0 {
            break;
        }
        let want = left.min(4096) as u32;
        let n = vfs::read_into(fd, off as u64, want, vfs::SHELL_RESULT_BUF);
        if n == u64::MAX {
            vfs::close(fd);
            return None;
        }
        let n = n as usize;
        if n == 0 {
            break;
        }
        let take = n.min(left);
        unsafe {
            core::ptr::copy_nonoverlapping(
                vfs::SHELL_RESULT_BUF as *const u8,
                out[off..].as_mut_ptr(),
                take,
            );
        }
        off += take;
        if n < want as usize {
            break;
        }
    }
    vfs::close(fd);
    Some(off)
}

/// 覆盖写整个文件 (先截断), 单次最多一页; 成功返回 true。
fn fs_write_file(path: &str, data: &[u8]) -> bool {
    let fd = vfs::creat(path);
    if fd == u64::MAX {
        return false;
    }
    if vfs::truncate(fd, 0) == u64::MAX {
        vfs::close(fd);
        return false;
    }
    let mut off = 0usize;
    while off < data.len() {
        let n = (data.len() - off).min(4096);
        let r = vfs::write_into(fd, off as u64, &data[off..off + n], vfs::SHELL_WRITE_BUF);
        if r == u64::MAX || r as usize != n {
            vfs::close(fd);
            return false;
        }
        off += n;
    }
    vfs::close(fd);
    true
}

/// 路径是否可打开 (存在且可读)。
fn fs_exists(path: &str) -> bool {
    let fd = vfs::open(path);
    if fd == u64::MAX {
        false
    } else {
        vfs::close(fd);
        true
    }
}

/// 设置权限位与属主 (MFS; 只有 uid 0 能改属主)。成功返回 true。
fn fs_set_mode_owner(path: &str, mode: u32, uid: u16, gid: u16) -> bool {
    let m = vfs::chmod_into(path, mode, vfs::SHELL_RESULT_BUF);
    let o = vfs::chown_into(path, uid, gid, vfs::SHELL_RESULT_BUF);
    m == 1 && o == 1
}

/// 去掉行尾 / 行首的 ASCII 空白。
fn trim_ascii(mut b: &[u8]) -> &[u8] {
    while let Some((&c, rest)) = b.split_first() {
        if c.is_ascii_whitespace() {
            b = rest;
        } else {
            break;
        }
    }
    while let Some((&c, _)) = b.split_last() {
        if c.is_ascii_whitespace() {
            b = &b[..b.len() - 1];
        } else {
            break;
        }
    }
    b
}

/// 解析十进制 u16 (字节切片)。
fn parse_u16_bytes(b: &[u8]) -> Option<u16> {
    parse_u16_dec(core::str::from_utf8(b).ok()?)
}

/// 解析 16 进制 u64 (最多 16 位)。
fn parse_hex64(b: &[u8]) -> Option<u64> {
    if b.is_empty() || b.len() > 16 {
        return None;
    }
    let mut v = 0u64;
    for &c in b {
        let d = match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'f' => c - b'a' + 10,
            b'A'..=b'F' => c - b'A' + 10,
            _ => return None,
        };
        v = (v << 4) | d as u64;
    }
    Some(v)
}

/// 把 `v` 写成 16 位小写十六进制。
fn hex64(v: u64, out: &mut [u8; 16]) {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut i = out.len();
    let mut x = v;
    while i > 0 {
        i -= 1;
        out[i] = DIGITS[(x & 0xF) as usize];
        x >>= 4;
    }
}

/// 复制 `src` 到 `dst`, 返回实际长度 (最多 `dst.len()`)。
fn copy_name(dst: &mut [u8], src: &[u8]) -> u8 {
    let n = src.len().min(dst.len());
    dst[..n].copy_from_slice(&src[..n]);
    n as u8
}

/// 读取并解析 `/etc/passwd`, 返回条目数。
fn acct_load(out: &mut [Acct]) -> usize {
    let mut text = [0u8; PASSWD_FILE_MAX];
    let n = match fs_read_file(ACCT_PASSWD, &mut text) {
        Some(n) => n,
        None => return 0,
    };
    let mut count = 0usize;
    for raw in text[..n].split(|&b| b == b'\n') {
        if count >= out.len() {
            break;
        }
        let line = trim_ascii(raw);
        if line.is_empty() || line[0] == b'#' {
            continue;
        }
        // name:uid:gid:home
        let mut fields: [&[u8]; 4] = [b"", b"", b"", b""];
        let mut fi = 0usize;
        for f in line.split(|&b| b == b':') {
            if fi < fields.len() {
                fields[fi] = f;
                fi += 1;
            }
        }
        if fi < 4 || fields[0].is_empty() {
            continue;
        }
        let uid = match parse_u16_bytes(fields[1]) {
            Some(v) => v,
            None => continue,
        };
        let gid = match parse_u16_bytes(fields[2]) {
            Some(v) => v,
            None => continue,
        };
        let mut a = ACCT_EMPTY;
        a.name_len = copy_name(&mut a.name, fields[0]);
        a.uid = uid;
        a.gid = gid;
        a.home_len = copy_name(&mut a.home, fields[3]);
        out[count] = a;
        count += 1;
    }
    count
}

/// 按用户名查账户。
fn acct_find(name: &str) -> Option<Acct> {
    let mut list = [ACCT_EMPTY; ACCT_MAX];
    let n = acct_load(&mut list);
    let mut i = 0usize;
    while i < n {
        if list[i].name() == name {
            return Some(list[i]);
        }
        i += 1;
    }
    None
}

/// 按 uid 查账户。
fn acct_find_uid(uid: u16) -> Option<Acct> {
    let mut list = [ACCT_EMPTY; ACCT_MAX];
    let n = acct_load(&mut list);
    let mut i = 0usize;
    while i < n {
        if list[i].uid == uid {
            return Some(list[i]);
        }
        i += 1;
    }
    None
}

/// 把 `uid` 的十进制写法写进 `out`, 返回长度。
fn uid_decimal(uid: u16, out: &mut [u8]) -> usize {
    let mut tmp = [0u8; 5];
    let mut n = 0;
    let mut v = uid;
    loop {
        tmp[n] = b'0' + (v % 10) as u8;
        n += 1;
        v /= 10;
        if v == 0 || n == tmp.len() {
            break;
        }
    }
    let m = n.min(out.len());
    for i in 0..m {
        out[i] = tmp[n - 1 - i];
    }
    m
}

/// 解析 `su` / `login` 的目标: 先查账户表, 再按十进制 uid 查; `allow_unknown_uid`
/// (仅无 `/etc/shadow` 的兼容模式) 允许切到账户表里没有的 uid。
fn acct_resolve(spec: &str, allow_unknown_uid: bool) -> Option<Acct> {
    if let Some(a) = acct_find(spec) {
        return Some(a);
    }
    let uid = parse_u16_dec(spec)?;
    if let Some(a) = acct_find_uid(uid) {
        return Some(a);
    }
    if !allow_unknown_uid {
        return None;
    }
    let mut a = ACCT_EMPTY;
    a.uid = uid;
    a.gid = if uid == 0 { 0 } else { uid };
    let mut tmp = [0u8; 8];
    let n = uid_decimal(uid, &mut tmp);
    let mut name = [0u8; ACCT_NAME_MAX];
    let mut hn = 0usize;
    // 名字仍按 uid 数字, 家目录 /home/<uid>。
    name[..n].copy_from_slice(&tmp[..n]);
    let prefix = b"/home/";
    a.home[..prefix.len()].copy_from_slice(prefix);
    hn += prefix.len();
    a.home[hn..hn + n].copy_from_slice(&tmp[..n]);
    hn += n;
    a.name_len = n as u8;
    a.name = name;
    a.home_len = hn as u8;
    Some(a)
}

/// 加盐 FNV-1a 64 位混合 (**非密码学强度, 仅占位**)。
fn fnv1a(h: u64, bytes: &[u8]) -> u64 {
    let mut x = h;
    for &b in bytes {
        x ^= b as u64;
        x = x.wrapping_mul(0x0000_0100_0000_01B3);
    }
    x
}

/// 口令 → 64 位哈希。`salt` 让相同口令在不同账户 / 不同次修改下不同。
fn kdf(password: &[u8], salt: u64) -> u64 {
    let mut h = fnv1a(0xCBF2_9CE4_8422_2325 ^ salt, b"morion");
    let mut round = 0u64;
    while round < 64 {
        h = fnv1a(h, password);
        h = fnv1a(h, &salt.to_le_bytes());
        h ^= round;
        h = h.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        h ^= h >> 29;
        round += 1;
    }
    h
}

/// 生成盐。没有随机源, 只用计数器 + 用户名混合 —— **明确不是密码学随机**。
fn new_salt(name: &str, uid: u16) -> u64 {
    static mut SEQ: u64 = 0;
    let seq = unsafe {
        SEQ = SEQ.wrapping_add(1);
        SEQ
    };
    let mut h = fnv1a(0x243F_6A88_85A3_08D3, name.as_bytes());
    h = fnv1a(h, &uid.to_le_bytes());
    h = fnv1a(h, &seq.to_le_bytes());
    let mut x = h ^ 0x9E37_79B9_7F4A_7C15;
    x ^= x >> 30;
    x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x ^= x >> 27;
    x = x.wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

/// 在 `shadow` 文本里查 `name` 的 `(salt, hash)`。
fn shadow_lookup(name: &str) -> Option<(u64, u64)> {
    let mut text = [0u8; SHADOW_FILE_MAX];
    let n = fs_read_file(ACCT_SHADOW, &mut text)?;
    for raw in text[..n].split(|&b| b == b'\n') {
        let line = trim_ascii(raw);
        if line.is_empty() {
            continue;
        }
        let mut it = line.split(|&b| b == b':');
        let field = match it.next() {
            Some(f) => f,
            None => continue,
        };
        if field != name.as_bytes() {
            continue;
        }
        let salt = match it.next().and_then(parse_hex64) {
            Some(v) => v,
            None => continue,
        };
        let hash = match it.next().and_then(parse_hex64) {
            Some(v) => v,
            None => continue,
        };
        return Some((salt, hash));
    }
    None
}

/// 校验口令。
fn verify_password(name: &str, password: &[u8]) -> bool {
    match shadow_lookup(name) {
        Some((salt, hash)) => kdf(password, salt) == hash,
        None => false,
    }
}

/// 追加一条 `name:salt:hash\n` 到 `out`, 返回写入长度。
fn push_shadow_line(out: &mut [u8], name: &str, salt: u64, hash: u64) -> usize {
    let mut n = 0usize;
    let mut put = |src: &[u8], n: &mut usize| {
        let k = src.len().min(out.len() - *n);
        out[*n..*n + k].copy_from_slice(&src[..k]);
        *n += k;
    };
    put(name.as_bytes(), &mut n);
    put(b":", &mut n);
    let mut salt_hex = [b'0'; 16];
    hex64(salt, &mut salt_hex);
    put(&salt_hex, &mut n);
    put(b":", &mut n);
    let mut hash_hex = [b'0'; 16];
    hex64(hash, &mut hash_hex);
    put(&hash_hex, &mut n);
    put(b"\n", &mut n);
    n
}

/// 更新 (或追加) `name` 的 shadow 行, 并保持 0600 / 0:0。
fn shadow_update(name: &str, salt: u64, hash: u64) -> bool {
    let mut text = [0u8; SHADOW_FILE_MAX];
    let n = fs_read_file(ACCT_SHADOW, &mut text).unwrap_or(0);
    let mut out = [0u8; SHADOW_FILE_MAX];
    let mut o = 0usize;
    let mut replaced = false;
    for raw in text[..n].split(|&b| b == b'\n') {
        let line = trim_ascii(raw);
        if line.is_empty() {
            continue;
        }
        let name_end = line.iter().position(|&b| b == b':').unwrap_or(line.len());
        if &line[..name_end] == name.as_bytes() {
            if replaced {
                continue;
            }
            let k = push_shadow_line(&mut out[o..], name, salt, hash);
            o += k;
            replaced = true;
        } else {
            let k = line.len().min(out.len() - o);
            out[o..o + k].copy_from_slice(&line[..k]);
            o += k;
            if o < out.len() {
                out[o] = b'\n';
                o += 1;
            }
        }
    }
    if !replaced {
        let k = push_shadow_line(&mut out[o..], name, salt, hash);
        o += k;
    }
    fs_write_file(ACCT_SHADOW, &out[..o]) && fs_set_mode_owner(ACCT_SHADOW, 0o600, 0, 0)
}

/// 启动时准备账户数据库: 把 tmpfs 挂到 `/etc`, 首次使用时生成 root / user。
///
/// 账户只在本会话有效（没有可用的持久卷时，这是唯一不会阻塞启动的选择）。
fn shell_setup_accounts(announce: bool) -> bool {
    // 账户库固定放 tmpfs（RAM）。
    //
    // 不再尝试挂 MFS：真机没有 MFS 分区时，`/etc` 的挂表项会指向一个没有卷的服务，
    // 对它的任何访问都会阻塞在 SYS_CALL 上（内核没有调用超时），shell 启动就卡住。
    // 代价是账户/口令只在本次会话有效 —— 明确打印，不假装持久。
    let _ = vfs::mount(ACCT_DIR, vfs::TMPFS_DOMAIN);
    if !fs_exists(ACCT_DIR) {
        if announce {
            println("login: account database unavailable (tmpfs /etc mount failed)");
        }
        return false;
    }
    if announce {
        println("login: account db in RAM (session-only) - run 'passwd' to change");
    }
    if !fs_exists(ACCT_PASSWD) {
        let def = b"root:0:0:/home/root\nuser:1000:1000:/home/user\n";
        if !fs_write_file(ACCT_PASSWD, def) {
            if announce {
                println("login: cannot create /etc/passwd");
            }
            return false;
        }
    }
    if !fs_exists(ACCT_SHADOW) {
        // 默认口令 root/root、user/user —— 仅占位, 登录后请用 `passwd` 改掉。
        let mut buf = [0u8; SHADOW_FILE_MAX];
        let mut o = 0usize;
        let defaults: [(&str, &[u8], u16); 2] = [("root", b"root", 0), ("user", b"user", 1000)];
        for (name, pw, uid) in defaults {
            let salt = new_salt(name, uid);
            let k = push_shadow_line(&mut buf[o..], name, salt, kdf(pw, salt));
            o += k;
        }
        if !fs_write_file(ACCT_SHADOW, &buf[..o]) {
            if announce {
                println("login: cannot create /etc/shadow");
            }
            return false;
        }
        if announce {
            println("login: created default accounts root (uid 0) and user (uid 1000)");
        }
    }
    let _ = fs_set_mode_owner(ACCT_PASSWD, 0o644, 0, 0);
    let _ = fs_set_mode_owner(ACCT_SHADOW, 0o600, 0, 0);
    // 家目录约定 `/home/<user>`: 不存在就建一个, 让登录后真的能切过去。
    ensure_home_dir("/home");
    ensure_home_dir("/home/root");
    ensure_home_dir("/home/user");
    if announce {
        println("login: account database ready (/etc/passwd, /etc/shadow; shadow 0600)");
    }
    true
}

/// 建 `path` 目录 (已存在或失败都忽略)。按 `/home` → `/home/<user>` 的顺序调用。
fn ensure_home_dir(path: &str) {
    if !fs_exists(path) {
        let _ = vfs::mkdir(path);
    }
}

/// 账户数据库是否可用。
///
/// 只有**两个文件都不在** (从未初始化, 例如启动时 MFS 还没就绪) 才再尝试生成一次;
/// 若 `passwd` 在而 `shadow` 被删掉, 视为用户主动移除口令库, 退回旧的 `su` 语义。
fn accounts_ready() -> bool {
    if fs_exists(ACCT_SHADOW) {
        return true;
    }
    if fs_exists(ACCT_PASSWD) {
        return false;
    }
    shell_setup_accounts(false)
}

/// 切到家目录: 存在且是目录才 `cd`; 否则只提示 (约定如此, 不自动创建)。
fn enter_home(st: &mut ShellState, home: &str, who: &str) {
    let fd = vfs::open(home);
    if fd == u64::MAX {
        print(who);
        print(": home directory ");
        print(home);
        println(" not found (staying put; create it with mkdir if you want one)");
        return;
    }
    let n = vfs::readdir_into(fd, vfs::SHELL_RESULT_BUF);
    vfs::close(fd);
    if n == u64::MAX {
        print(who);
        print(": home ");
        print(home);
        println(" is not a directory");
        return;
    }
    st.set_cwd(home);
}

/// 把当前会话 (含 cwd) 快照一份, 供 logout 恢复。
fn snapshot_session(st: &ShellState) -> Session {
    let mut s = st.session;
    s.cwd = [0; CWD_MAX];
    let n = st.cwd_len.min(CWD_MAX);
    s.cwd[..n].copy_from_slice(&st.cwd[..n]);
    s.cwd_len = n as u8;
    s
}

/// 进入 `a` 对应的会话 (可选压栈), 设置 USER/HOME 并切家目录。
fn apply_session(st: &mut ShellState, a: &Acct, push: bool) {
    if push && st.depth < SESSION_MAX {
        st.stack[st.depth] = snapshot_session(st);
        st.depth += 1;
    }
    st.session = session_of(a);
    let mut uname = [0u8; ACCT_NAME_MAX];
    let un = copy_name(&mut uname, a.name().as_bytes());
    let mut uhome = [0u8; ACCT_HOME_MAX];
    let hn = copy_name(&mut uhome, a.home().as_bytes());
    let uname_s = unsafe { core::str::from_utf8_unchecked(&uname[..un as usize]) };
    let home_s = unsafe { core::str::from_utf8_unchecked(&uhome[..hn as usize]) };
    st.env_set("USER", uname_s);
    st.env_set("HOME", home_s);
    enter_home(st, home_s, "login");
    // 记录切换后的 cwd, 让 logout 恢复的是"离开时的位置"。
    let n = st.cwd_len.min(CWD_MAX);
    st.session.cwd[..n].copy_from_slice(&st.cwd[..n]);
    st.session.cwd_len = n as u8;
}

/// 读一行不显示内容的口令 (只回显 `*`); 返回长度。Ctrl+C 返回 0。
fn read_secret(buf: &mut [u8]) -> usize {
    let mut n = 0usize;
    loop {
        let c = sys_key_read();
        if c == u64::MAX {
            return n;
        }
        if c > 0xFF {
            continue;
        }
        let c = c as u8;
        match c {
            b'\r' | b'\n' => {
                print("\n");
                flush();
                return n;
            }
            0x08 | 0x7F => {
                if n > 0 {
                    n -= 1;
                    print("\x08 \x08");
                    flush();
                }
            }
            0x03 => {
                print("^C\n");
                flush();
                return 0;
            }
            b if (0x20..=0x7E).contains(&b) && n < buf.len() => {
                buf[n] = b;
                n += 1;
                print("*");
                flush();
            }
            _ => {}
        }
    }
}

/// 读一行普通输入 (用户名等), 去掉首尾空白; 返回长度。
fn read_plain(buf: &mut [u8]) -> usize {
    let n = morion::console::readline(buf, "", COMMANDS);
    if n == u64::MAX {
        return 0;
    }
    let n = n as usize;
    let (start, k) = {
        let s = unsafe { core::str::from_utf8_unchecked(&buf[..n]) };
        let t = s.trim();
        (t.as_ptr() as usize - buf.as_ptr() as usize, t.len())
    };
    buf.copy_within(start..start + k, 0);
    k
}

/// `login [user]` — 以指定用户身份登录 (需要该用户的口令, 3 次失败回到当前会话)。
fn shell_login(st: &mut ShellState, arg: &str) {
    if !accounts_ready() {
        println("login: account database unavailable (no /etc/shadow)");
        return;
    }
    let mut namebuf = [0u8; ACCT_NAME_MAX];
    let name: &str = if arg.is_empty() {
        print("login: ");
        flush();
        let n = read_plain(&mut namebuf);
        if n == 0 {
            println("login: cancelled");
            return;
        }
        unsafe { core::str::from_utf8_unchecked(&namebuf[..n]) }
    } else {
        arg
    };
    let acct = match acct_find(name) {
        Some(a) => a,
        None => {
            print("login: unknown user: ");
            println(name);
            return;
        }
    };
    let mut pw = [0u8; SECRET_MAX];
    let mut tries = 0u32;
    while tries < AUTH_TRIES {
        print("Password: ");
        flush();
        let n = read_secret(&mut pw);
        if verify_password(acct.name(), &pw[..n]) {
            apply_session(st, &acct, true);
            print("login: welcome, ");
            println(acct.name());
            return;
        }
        println("login: incorrect password");
        tries += 1;
    }
    println("login: 3 failed attempts, staying in the current session");
}

/// `passwd [user]` — 改口令。改自己的要先验旧口令; 改别人只有 root 可以。
fn shell_passwd(st: &mut ShellState, arg: &str) {
    if !accounts_ready() {
        println("passwd: account database unavailable (no /etc/shadow)");
        return;
    }
    let mut own = [0u8; ACCT_NAME_MAX];
    let on = copy_name(&mut own, st.session.name().as_bytes());
    let own_s = unsafe { core::str::from_utf8_unchecked(&own[..on as usize]) };
    let target = if arg.is_empty() { own_s } else { arg };
    let acct = match acct_find(target) {
        Some(a) => a,
        None => {
            print("passwd: unknown user: ");
            println(target);
            return;
        }
    };
    let is_self = acct.name() == own_s;
    if !is_self && st.session.uid != 0 {
        println("passwd: permission denied (only root may change another user's password)");
        return;
    }
    let mut pw = [0u8; SECRET_MAX];
    if is_self {
        let mut ok = false;
        let mut tries = 0u32;
        while tries < AUTH_TRIES {
            print("Current password: ");
            flush();
            let n = read_secret(&mut pw);
            if verify_password(acct.name(), &pw[..n]) {
                ok = true;
                break;
            }
            println("passwd: incorrect password");
            tries += 1;
        }
        if !ok {
            println("passwd: 3 failed attempts, password unchanged");
            return;
        }
    }
    let mut pw1 = [0u8; SECRET_MAX];
    print("New password: ");
    flush();
    let n1 = read_secret(&mut pw1);
    if n1 == 0 {
        println("passwd: empty password not allowed");
        return;
    }
    let mut pw2 = [0u8; SECRET_MAX];
    print("Retype new password: ");
    flush();
    let n2 = read_secret(&mut pw2);
    if pw1[..n1] != pw2[..n2] {
        println("passwd: passwords do not match");
        return;
    }
    let salt = new_salt(acct.name(), acct.uid);
    if shadow_update(acct.name(), salt, kdf(&pw1[..n1], salt)) {
        print("passwd: password updated for ");
        println(acct.name());
    } else {
        println("passwd: failed to update /etc/shadow");
    }
}

/// `su [user|uid]` — 切换会话身份。
///
/// * 有账户数据库: 非 root 切换必须输入**目标用户的口令** (3 次); root 可以空口令跳过。
/// * 没有 `/etc/shadow` (兼容旧行为): 只有 root 可以切换, 由内核真实降权。
fn shell_su(st: &mut ShellState, arg: &str) {
    let has_db = accounts_ready();
    let spec = if arg.is_empty() { "root" } else { arg };
    let acct = match acct_resolve(spec, !has_db) {
        Some(a) => a,
        None => {
            print("su: unknown user: ");
            println(spec);
            return;
        }
    };
    if acct.uid == st.session.uid && acct.gid == st.session.gid {
        print("su: already ");
        println(acct.name());
        return;
    }
    let mut pw = [0u8; SECRET_MAX];
    if has_db {
        let mut ok = false;
        if st.session.uid == 0 {
            // root 可以直接切换; 也接受输入目标口令 (空行 = 跳过)。
            print("Password for ");
            print(acct.name());
            print(" (empty = skip): ");
            flush();
            let n = read_secret(&mut pw);
            ok = n == 0 || verify_password(acct.name(), &pw[..n]);
            if !ok {
                println("su: incorrect password");
                return;
            }
        } else {
            let mut tries = 0u32;
            while tries < AUTH_TRIES {
                print("Password: ");
                flush();
                let n = read_secret(&mut pw);
                if verify_password(acct.name(), &pw[..n]) {
                    ok = true;
                    break;
                }
                println("su: incorrect password");
                tries += 1;
            }
            if !ok {
                print("su: 3 failed attempts, staying as ");
                println(st.session.name());
                return;
            }
        }
    } else {
        // 兼容: 没有账户数据库时, 只有 root 可以 su (真实降权)。
        if st.session.uid != 0 {
            println("su: permission denied: only uid 0 may switch users (no password mechanism)");
            return;
        }
        if !sys_cred_set(acct.uid, acct.gid) {
            println("su: setuid failed (kernel rejected the change)");
            return;
        }
    }
    apply_session(st, &acct, true);
    print("su: now ");
    println(acct.name());
}

/// `logout` / `exit` — 回到上一个会话 (栈空时给出提示)。
fn shell_logout(st: &mut ShellState) {
    if st.depth == 0 {
        println("logout: no previous session (already at the top level)");
        return;
    }
    let prev = st.stack[st.depth - 1];
    if !fs_exists(ACCT_SHADOW) {
        // 兼容旧语义: 恢复需要内核允许 (非 root 的提权会被拒绝)。
        if !sys_cred_set(prev.uid, prev.gid) {
            print("logout: cannot restore previous identity (SYS_CRED is drop-only); staying as ");
            println(st.session.name());
            return;
        }
    }
    st.depth -= 1;
    st.session = prev;
    let mut uname = [0u8; ACCT_NAME_MAX];
    let un = copy_name(&mut uname, prev.name().as_bytes());
    let mut uhome = [0u8; ACCT_HOME_MAX];
    let hn = copy_name(&mut uhome, prev.home().as_bytes());
    let uname_s = unsafe { core::str::from_utf8_unchecked(&uname[..un as usize]) };
    let home_s = unsafe { core::str::from_utf8_unchecked(&uhome[..hn as usize]) };
    st.env_set("USER", uname_s);
    st.env_set("HOME", home_s);
    st.set_cwd(prev.cwd_str());
    print("logout: back to ");
    println(uname_s);
}

/// `whoami` — 打印当前**会话**用户名。
fn shell_whoami(st: &ShellState) {
    println(st.session.name());
}

/// `id` — 打印当前**会话** uid / gid 与用户名。
fn shell_id(st: &ShellState) {
    print("uid=");
    print_u64(st.session.uid as u64);
    print("(");
    print(st.session.name());
    print(") gid=");
    print_u64(st.session.gid as u64);
    print("(");
    print(st.session.name());
    print(")");
    println("");
}

/// 环境变量名是否合法 (字母或 `_` 开头, 其后字母/数字/`_`)。
fn valid_env_name(name: &str) -> bool {
    let b = name.as_bytes();
    if b.is_empty() {
        return false;
    }
    let first = b[0];
    if !(first.is_ascii_alphabetic() || first == b'_') {
        return false;
    }
    b[1..]
        .iter()
        .all(|c| c.is_ascii_alphanumeric() || *c == b'_')
}

/// `env` — 打印进程内环境变量表 (附动态的 `PWD`)。
fn shell_env(st: &ShellState) {
    for e in st.env.iter() {
        if e.name_len == 0 {
            continue;
        }
        let name = unsafe { core::str::from_utf8_unchecked(&e.name[..e.name_len as usize]) };
        let val = unsafe { core::str::from_utf8_unchecked(&e.val[..e.val_len as usize]) };
        print(name);
        print("=");
        println(val);
    }
    print("PWD=");
    println(st.cwd_str());
}

/// `export NAME=VALUE` — 设置/覆盖进程内环境变量; 无参数时等同 `env`。
fn shell_export(st: &mut ShellState, arg: &str) {
    if arg.is_empty() {
        shell_env(st);
        return;
    }
    let (name, val) = match arg.find('=') {
        Some(i) => (&arg[..i], &arg[i + 1..]),
        None => {
            println("export: usage: export NAME=VALUE");
            return;
        }
    };
    if !valid_env_name(name) {
        print("export: bad name: ");
        println(name);
        return;
    }
    if !st.env_set(name, val) {
        println("export: name/value too long or env table full");
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
    let mut used = path;
    let mut dom = morion::exec::spawn_file(path);
    // 真机镜像把预装应用放在 `/apps/` 下；`run calc` 这种裸名字额外试
    // `/apps/<name>.mex` 与 `/apps/<name>`，避免用户必须记住完整路径。
    if dom.is_none() && !arg.contains('/') {
        for suffix in [".mex", ""] {
            if let Some(p) = app_candidate(arg, suffix) {
                dom = morion::exec::spawn_file(p);
                if dom.is_some() {
                    used = p;
                    break;
                }
            }
        }
    }
    match dom {
        Some(domain) => {
            print("run: loaded ");
            print(used);
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
            print(" uid=");
            print_u64(de.uid as u64);
            print(" gid=");
            print_u64(de.gid as u64);
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
/// 04b 起权限位**参与访问判定** (见 `mfs_srv` 的 `mfs_check_access`), 不再只是显示。
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

/// `chown <uid>:<gid> <path>` — 改属主 / 属组 (04b; 仅 MFS, 只有 `uid 0` 能成功)。
fn shell_chown(st: &ShellState, arg: &str) {
    let (spec, path_s) = match arg.find(' ') {
        Some(i) => (&arg[..i], arg[i + 1..].trim()),
        None => {
            println("chown: usage: chown <uid>:<gid> <path>");
            return;
        }
    };
    if path_s.is_empty() {
        println("chown: usage: chown <uid>:<gid> <path>");
        return;
    }
    let (uid_s, gid_s) = match spec.find(':') {
        Some(i) => (&spec[..i], &spec[i + 1..]),
        None => {
            println("chown: usage: chown <uid>:<gid> <path>");
            return;
        }
    };
    let uid = match parse_u16_dec(uid_s) {
        Some(v) => v,
        None => {
            println("chown: bad uid");
            return;
        }
    };
    let gid = match parse_u16_dec(gid_s) {
        Some(v) => v,
        None => {
            println("chown: bad gid");
            return;
        }
    };
    let path = match resolve_in_cwd(st.cwd_str(), path_s) {
        Some(p) => p,
        None => {
            println("chown: path too long");
            return;
        }
    };
    if vfs::chown_into(path, uid, gid, vfs::SHELL_RESULT_BUF) == 1 {
        print("chown: ");
        print(path);
        print(" -> ");
        print_u64(uid as u64);
        print(":");
        print_u64(gid as u64);
        println("");
    } else {
        print("chown: failed (uid 0 required): ");
        println(path);
    }
}

/// 十进制无符号 u16 解析 (空串 / 非数字 / 越界 -> `None`)。
fn parse_u16_dec(s: &str) -> Option<u16> {
    let b = s.as_bytes();
    if b.is_empty() {
        return None;
    }
    let mut v: u32 = 0;
    for &c in b {
        if !c.is_ascii_digit() {
            return None;
        }
        v = v * 10 + (c - b'0') as u32;
        if v > u16::MAX as u32 {
            return None;
        }
    }
    Some(v as u16)
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
    print("  Uid: ");
    print_u64(st.uid as u64);
    print("  Gid: ");
    print_u64(st.gid as u64);
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

/// `mkfs.mfs <卷号> [--force]` — 在指定卷上创建 MorionFS 文件系统 (**擦除**该卷现有内容)。
///
/// 卷号来自块服务启动时打印的卷表 (`vol: <卷号> nsid=... kind=...`)。护栏不在这里
/// 而在服务端: mfs_srv 只接受 `kind=mfs` (重新格式化) 或 `kind=unknown` (未格式化)
/// 的卷, FAT / exFAT / ext2 一律拒绝 —— 命令与自测走同一条路径, 判定只有一处。
/// `--force` 是唯一的例外, 必须显式写出来才会走 (见 `vfs::mfs_mkfs_force`)。
///
/// 格式化同时把这卷标记为**主卷**: **下次启动** `/mfs` 就是它 (本次运行的挂载不变)。
fn shell_mkfs(arg: &str) {
    const USAGE: &str =
        "mkfs.mfs: usage: mkfs.mfs <volume-id> [--force]   (see the 'vol:' lines in the boot log)";
    let mut it = arg.split_whitespace();
    let vol = match it.next().and_then(parse_dec) {
        Some(v) => v,
        None => {
            println(USAGE);
            return;
        }
    };
    let force = match it.next() {
        None => false,
        Some("--force") => true,
        Some(_) => {
            println(USAGE);
            return;
        }
    };
    if it.next().is_some() {
        println(USAGE);
        return;
    }
    let serial = if force {
        vfs::mfs_mkfs_force(vol)
    } else {
        vfs::mfs_mkfs(vol)
    };
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
        // 安装盘里非空白卷是放行的, 被拒只可能是「卷号不存在」—— 别再把 --force 当建议提。
        if !force && !INSTALL_MODE {
            println("mkfs.mfs: the volume may hold another filesystem; --force overwrites it");
        }
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

/// `mfs.snap` — 给 MorionFS **主卷** (即 `/mfs`) 拍一张快照。
///
/// 快照是 COW 语义下"留旧版本"的钩子: 拍完之后**即使继续覆盖写**, 被快照引用的旧块也不会
/// 被 GC 回收, 于是 `mfs.rollback <索引>` 能把整卷 (含全部文件) 退回这一刻的状态。
/// 快照环跨启动**持久化**在超级块里 (上限 8 条, 满时淘汰最旧一条, 索引整体前移)。
fn shell_mfs_snap(arg: &str) {
    if !arg.is_empty() {
        println("mfs.snap: usage: mfs.snap   (snapshots the MorionFS volume mounted at /mfs)");
        return;
    }
    let idx = vfs::mfs_snapshot();
    if idx == u64::MAX {
        println("mfs.snap: FAILED (MorionFS not mounted?)");
    } else {
        print("mfs.snap: snapshot ");
        print_u64(idx);
        println(" taken -> `mfs.rollback <index>` returns to it");
    }
}

/// `mfs.snaps` — 列出主卷上的快照 (索引 / 代际 / 根 inode 表块)。
fn shell_mfs_snaps(arg: &str) {
    if !arg.is_empty() {
        println("mfs.snaps: usage: mfs.snaps");
        return;
    }
    let n = vfs::mfs_snapshot_list(vfs::SHELL_RESULT_BUF);
    if n == u64::MAX {
        println("mfs.snaps: FAILED (MorionFS not mounted?)");
        return;
    }
    let count = n as usize / vfs::SNAP_REC_LEN;
    if count == 0 {
        println("mfs.snaps: no snapshots (take one with `mfs.snap`)");
        return;
    }
    print("mfs.snaps: ");
    print_u64(count as u64);
    println(" snapshot(s):");
    let base = vfs::SHELL_RESULT_BUF as *const u8;
    for i in 0..count {
        // 记录与 mfs_srv / libvfs 一致: gen(u64) / itab(u32) / ino_hint(u32) / alloc_hint(u32)。
        let rec = unsafe { base.add(i * vfs::SNAP_REC_LEN) };
        print("  [");
        print_u64(i as u64);
        print("] gen=");
        print_u64(read_u64(rec));
        print(" root_itab=");
        print_u64(read_u32(unsafe { rec.add(8) }) as u64);
        print(" ino_hint=");
        print_u64(read_u32(unsafe { rec.add(12) }) as u64);
        print(" alloc_hint=");
        print_u64(read_u32(unsafe { rec.add(16) }) as u64);
        println("");
    }
}

/// `mfs.rollback <索引>` — 把主卷退回指定快照 (索引见 `mfs.snaps`)。
///
/// 只改超级块里的根 inode 表与代际指针, **不搬数据**: 快照之后写的块成为不可达垃圾,
/// 由 GC 回收。回滚本身也会落盘 (下次启动仍是回滚后的状态)。
fn shell_mfs_rollback(arg: &str) {
    let idx = match parse_dec(arg) {
        Some(v) => v,
        None => {
            println("mfs.rollback: usage: mfs.rollback <index>   (see `mfs.snaps`)");
            return;
        }
    };
    if idx > u32::MAX as u64 {
        println("mfs.rollback: index out of range");
        return;
    }
    if vfs::mfs_snapshot_restore(idx as u32) == 1 {
        print("mfs.rollback: /mfs rolled back to snapshot ");
        print_u64(idx);
        println(" (files now show that snapshot's state)");
    } else {
        print("mfs.rollback: FAILED (no such snapshot index ");
        print_u64(idx);
        println(", or MorionFS unavailable)");
    }
}

/// `mfs.gc` — 回收主卷上不可达的 COW 旧块 (mark & sweep)。
///
/// 可达性以当前根 + 全部快照为起点重算, 故**快照还引用着的旧版本不会被回收** ——
/// 这也是回滚能一直生效的前提。返回本次回收的块数。
fn shell_mfs_gc(arg: &str) {
    if !arg.is_empty() {
        println("mfs.gc: usage: mfs.gc");
        return;
    }
    let freed = vfs::mfs_gc();
    if freed == u64::MAX {
        println("mfs.gc: FAILED (MorionFS not mounted?)");
    } else {
        print("mfs.gc: reclaimed ");
        print_u64(freed);
        println(" block(s) unreachable from the root (snapshot-held blocks kept)");
    }
}

/// `mfs.fsck [--repair]` — 对账 `/mfs` 主卷上「已分配但不可达」的 inode 槽。
///
/// 默认**只报不修** (不写盘): 打印泄漏 inode 数与它们名下可回收的块数。
/// 加 `--repair` 才真正回收 (清槽 + 按可达性安全回收块, 快照仍引用的版本不会被动)。
fn shell_mfs_fsck(arg: &str) {
    let repair = match arg {
        "" => false,
        "--repair" => true,
        _ => {
            println("mfs.fsck: usage: mfs.fsck [--repair]   (default: report only, no writes)");
            return;
        }
    };
    let r = vfs::mfs_fsck(repair);
    if r == u64::MAX {
        println("mfs.fsck: FAILED (MorionFS not mounted?)");
        return;
    }
    let inos = r >> 32;
    let blocks = r & 0xFFFF_FFFF;
    if repair {
        print("mfs.fsck: repaired ");
        print_u64(inos);
        print(" leaked inode(s), reclaimed ");
        print_u64(blocks);
        println(" block(s)");
    } else {
        print("mfs.fsck: ");
        print_u64(inos);
        print(" leaked inode(s), ");
        print_u64(blocks);
        println(" reclaimable block(s) - report only (rerun with --repair to reclaim)");
    }
}

/// `mfs.sync` — 把 `/mfs` 主卷显式落盘一次 (刷新位图与两份超级块), 打印落盘后的代际 gen。
///
/// 幂等: 重复调用无副作用 (只推进 gen)。用途是给「崩溃一致性」自测一个可断言的落盘点。
fn shell_mfs_sync(arg: &str) {
    if !arg.is_empty() {
        println("mfs.sync: usage: mfs.sync   (flush the MorionFS volume at /mfs)");
        return;
    }
    let gen = vfs::mfs_sync();
    if gen == u64::MAX {
        println("mfs.sync: FAILED (MorionFS not mounted?)");
    } else {
        print("mfs.sync: /mfs flushed to disk, generation ");
        print_u64(gen);
        println("");
    }
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
    // `/etc/shadow` 只有 uid 0 可读: MFS 的 0600 拦得住其它域, 但 shell 自己以 uid 0
    // 运行 (会话管理器), 故这里再按**会话身份**拦一次。
    if path == ACCT_SHADOW && st.session.uid != 0 {
        println("cat: /etc/shadow: permission denied (only uid 0 may read it)");
        return;
    }
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
