//! Morion OS 可执行文件加载演示程序 —— 一份**独立的 ELF**（`.mex`）。
//!
//! 它不是引导期服务程序（那些在 `user/srv/` 里，各自一份独立 ELF）的一部分, 也不在引导期被加载: 它由运行中的域
//! 经 `SYS_SPAWN_ELF` 从**文件系统里的一个文件**运行时载入到一个**新域**并启动。
//!
//! 程序本身只写主逻辑 —— 入口 `_start`（crt0）、panic 处理、syscall 封装与打印都来自
//! 运行库 **libmorion**（这就是"运行库"存在的意义: 一份程序不再各带一套桩）。
//!
//! 除打印自身身份外, 它还是 **04b 权限与多用户的端到端自测载体**: 运行期新建的域在
//! `mfs_srv` 的静态身份表里是**低权身份 uid 1000**, 正好用来验证权限强制 (见
//! `perm_selftest`)。该自测只在夹具开关 `/mfs/pub/perm.go` 存在时执行。

#![no_std]
#![no_main]

use morion::syscall::{
    domain_id, print, print_hex, print_u64, println, sys_alloc_page, sys_share_page,
};
use morion::vfs;

/// MFS 服务域 (与内核建域顺序一致)。
const MFS_DOMAIN: u64 = 11;
/// MFS 错误码 (低 16 位; 与 `mfs_srv` 的 `MFS_E*` 一致)。
const EPERM: u64 = 1;
const EACCES: u64 = 2;
const ENOENT: u64 = 3;

/// 夹具路径 (全部由 app 自测以 root 身份预置)。
const CASE34: &str = "/mfs/pub/case34.txt";
const CASE35: &str = "/mfs/pub/case35.txt";
const OWN: &str = "/mfs/pub/own.txt";
const GATE: &str = "/mfs/pub/perm.go";
const RESULT: &str = "/mfs/pub/perm.result";
const STICKY_ROOT: &str = "/mfs/sticky/rootfile.txt";
const STICKY_OWN: &str = "/mfs/sticky/ownfile.txt";
const MISSING: &str = "/mfs/pub/no-such-file-xyz";

/// 自测用的共享缓冲页地址 —— **必须区别于 app / shell / mfs_srv 自己已映射的地址**:
/// `SYS_SHARE_PAGE` 是把本域页按**同一虚拟地址**映射进目标服务域, 撞上已映射地址会让
/// 内核 `PageAlreadyMapped` panic。app 用 `0x8000104000/0x8000105000`、shell 用
/// `0x8000106000/0x8000107000`、mfs_srv 自身的块缓冲在 `0x8000108000..0x800010B000`,
/// 故这里取紧邻的空位 `0x800010C000/0x800010D000`。
const BUF_RESULT: u64 = 0x0000_0080_0010_C000;
const BUF_WRITE: u64 = 0x0000_0080_0010_D000;

/// 程序主函数（由 libmorion 的 `_start` 调用）。
///
/// 打印自己的身份、自己的域 id 与**自己的入口地址** —— 后者用来证明镜像确实被载入到
/// 它链接时约定的那个虚拟地址（`user/linker.ld` 的 `USER_SPACE_BASE`），而不是"随便找块内存跑起来"。
#[no_mangle]
pub extern "C" fn morion_main(_domain_id: u64) {
    print("exec: 我是运行时被加载的独立 ELF 程序 (morion-hello), 我的域 = ");
    print_u64(domain_id());
    print(", 入口 = 0x");
    print_hex(morion::_start as *const () as usize as u64);
    println("");
    perm_selftest();
}

/// FS-34..37 (04b): 以**低权身份**(运行期新建域 => uid 1000)跑权限端到端用例。
///
/// 只在夹具开关 `/mfs/pub/perm.go` 存在时执行(由 app 自测在调用前创建), 否则静默跳过 ——
/// FS-27/28 复用的 hello 实例不会误跑。结果写成 4 个 `'0'`/`'1'` 字节到 `/mfs/pub/perm.result`,
/// 由 app 读回断言。
fn perm_selftest() {
    // 开关文件不存在 => 不是权限自测场景, 直接返回 (保持 FS-27/28 的原有输出)。
    // **必须在 setup_buffers 之前**: 否则 FS-27/28 的 hello 实例也会去共享页, 而
    // `SYS_SHARE_PAGE` 的同地址映射会与服务域里已有的映射相撞 (内核 panic)。
    let gate = vfs::open(GATE);
    if gate == u64::MAX {
        return;
    }
    vfs::close(gate);
    if !setup_buffers() {
        println("perm: buffer setup FAILED");
        return;
    }

    let c34 = check_open_denied(CASE34, EACCES);
    let c35 = check_owner_and_perm();
    let c36 = check_sticky();
    let c37 = check_enoent();
    let bits = [
        if c34 { b'1' } else { b'0' },
        if c35 { b'1' } else { b'0' },
        if c36 { b'1' } else { b'0' },
        if c37 { b'1' } else { b'0' },
    ];
    // 写回结果位图 (app 读回)。
    let fd = vfs::creat(RESULT);
    if fd != u64::MAX {
        let _ = vfs::write_into(fd, 0, &bits, BUF_WRITE);
        vfs::close(fd);
    }
    print("perm: FS34..37 = ");
    print(unsafe { core::str::from_utf8_unchecked(&bits) });
    println(" (1=pass)");
}

/// 分配并把自己的结果 / 写缓冲页共享给 MFS 服务 (vfs 的 `_into` 系列要用)。
fn setup_buffers() -> bool {
    if sys_alloc_page(BUF_RESULT) != 1 || sys_alloc_page(BUF_WRITE) != 1 {
        return false;
    }
    sys_share_page(BUF_RESULT, MFS_DOMAIN) == 1 && sys_share_page(BUF_WRITE, MFS_DOMAIN) == 1
}

/// FS-34: 打开 root 拥有、`mode 0000` 的文件必须被拒, 且 errno = `EACCES`。
fn check_open_denied(path: &str, want_err: u64) -> bool {
    let fd = vfs::open(path);
    if fd != u64::MAX {
        vfs::close(fd);
        return false;
    }
    vfs::mfs_last_errno() == want_err
}

/// FS-35: root 的 `0666` 文件可读; 非属主 `chmod` -> `EPERM`; 自建文件属主 = `1000`;
/// 非 root `chown` -> `EPERM`。
fn check_owner_and_perm() -> bool {
    let fd = vfs::open(CASE35);
    if fd == u64::MAX {
        return false;
    }
    let n = vfs::read_into(fd, 0, 8, BUF_RESULT);
    vfs::close(fd);
    if n != 5 {
        // app 写进去的是 "hello" (5 字节)。
        return false;
    }
    // 非属主 chmod -> EPERM
    if vfs::chmod_into(CASE35, 0o600, BUF_RESULT) != u64::MAX {
        return false;
    }
    if vfs::mfs_last_errno() != EPERM {
        return false;
    }
    // 自建文件 -> 属主 uid/gid = 1000:1000
    let fd = vfs::creat(OWN);
    if fd == u64::MAX {
        return false;
    }
    let _ = vfs::write_into(fd, 0, b"hi", BUF_WRITE);
    vfs::close(fd);
    let got = vfs::stat_into(OWN, BUF_RESULT);
    if got != core::mem::size_of::<vfs::Stat>() as u64 {
        return false;
    }
    let st = unsafe { core::ptr::read_unaligned(BUF_RESULT as *const vfs::Stat) };
    if st.uid != 1000 || st.gid != 1000 {
        return false;
    }
    // 非 root chown -> EPERM
    if vfs::chown_into(OWN, 5, 5, BUF_RESULT) != u64::MAX {
        return false;
    }
    vfs::mfs_last_errno() == EPERM
}

/// FS-36: sticky 目录里删他人文件 -> `EACCES`; 删自己的文件 -> 成功。
fn check_sticky() -> bool {
    if vfs::unlink(STICKY_ROOT) != u64::MAX {
        return false;
    }
    if vfs::mfs_last_errno() != EACCES {
        return false;
    }
    let fd = vfs::creat(STICKY_OWN);
    if fd == u64::MAX {
        return false;
    }
    vfs::close(fd);
    vfs::unlink(STICKY_OWN) == 1
}

/// FS-37: 不存在的路径回 `ENOENT` (而非 `EACCES`) —— 错误码可区分。
fn check_enoent() -> bool {
    let fd = vfs::open(MISSING);
    if fd != u64::MAX {
        vfs::close(fd);
        return false;
    }
    vfs::mfs_last_errno() == ENOENT
}
