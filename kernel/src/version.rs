//! 版本串 —— 收口路线 **V1**。
//!
//! 号与转发臂已经备好（[`crate::syscall::SYS_UNAME`]，见仓库根 `HANDOFF.md` 第 3.0 节），
//! 故 V1 只填本文件 + 用户态封装/命令。这里是**版本常量的唯一来源** ——
//! `README.md` 的「版本」段与 `CHANGELOG.md` 必须与本文件保持一致。
//!
//! V2 的「无图形」变体通过 `Makefile` 注入的编译期环境变量 `MORION_NOGUI` 选择：
//! 置位时 release 加 `-nogui` 后缀。同一约定也用于用户态（见 `morion::syscall::NOGUI`）。

/// 系统名（POSIX `uname` 的 `sysname`）。
pub const SYSTEM_NAME: &str = "MorionOS";
/// 版本号（不含变体后缀，见 [`VARIANT`]）。
pub const VERSION: &str = "0.4.0";
/// 目标架构（POSIX `uname` 的 `machine`）。
pub const MACHINE: &str = "x86_64";

/// 是否**无图形**构建（V2）：由 `Makefile` 注入的 `MORION_NOGUI` 决定。
///
/// 用编译期环境变量而非 `--cfg`：后者会在 `clippy -D warnings` 下触发
/// `unexpected_cfgs` 告警（未在 `build.rs` 里声明），而环境变量无此问题。
pub const IS_NOGUI: bool = option_env!("MORION_NOGUI").is_some();

/// 变体后缀：无图形构建为 `-nogui`，否则为空。
pub const VARIANT: &str = if IS_NOGUI { "-nogui" } else { "" };

/// 构建号：`Makefile` 注入的 `MORION_BUILD`（git 短哈希，无 git 时用日期）；
/// 直接用 `cargo` 手工构建时缺失，回落为 `dev`。
pub const BUILD: &str = match option_env!("MORION_BUILD") {
    Some(s) => s,
    None => "dev",
};

/// 写入用户缓冲器的上限（含结尾 NUL）。
const UNAME_MAX: usize = 96;

/// 追加一段 ASCII 到 `buf`；放不下返回 `false`（调用方据此整体失败）。
fn push(buf: &mut [u8], n: &mut usize, s: &str) -> bool {
    let b = s.as_bytes();
    if *n + b.len() > buf.len() {
        return false;
    }
    buf[*n..*n + b.len()].copy_from_slice(b);
    *n += b.len();
    true
}

/// `SYS_UNAME` 的处理入口。
///
/// 参数与返回：
/// - `a1` = 用户缓冲指针（可写、已映射）；
/// - `a2` = 缓冲长度（字节）；
/// - `a3` = 选择串：`0` = 整行 `MorionOS <release> <machine>`、`1` = release
///   （如 `0.4.0-nogui`）、`2` = 构建号；
/// - 返回**写入字节数**（不含结尾 NUL），失败返回 `0`。
///
/// 复制前按 [`crate::memory::paging::is_user_address`] 校验用户区间（与 `SYS_FB_INFO`
/// 同口径），并检查缓冲长度 —— 不信任调用方。
pub fn handle(a1: u64, a2: u64, a3: u64) -> u64 {
    let mut buf = [0u8; UNAME_MAX];
    let mut n = 0usize;
    let ok = match a3 {
        0 => {
            push(&mut buf, &mut n, SYSTEM_NAME)
                && push(&mut buf, &mut n, " ")
                && push(&mut buf, &mut n, VERSION)
                && push(&mut buf, &mut n, VARIANT)
                && push(&mut buf, &mut n, " ")
                && push(&mut buf, &mut n, MACHINE)
        }
        1 => push(&mut buf, &mut n, VERSION) && push(&mut buf, &mut n, VARIANT),
        2 => push(&mut buf, &mut n, BUILD),
        _ => false,
    };
    // 结尾 NUL: 保证调用方即使不看返回值也能当 C 串用。
    if !ok || n + 1 > buf.len() {
        return 0;
    }
    buf[n] = 0;
    let total = (n + 1) as u64;
    if a1 == 0 || a2 < total {
        return 0;
    }
    if !crate::memory::paging::is_user_address(a1)
        || !crate::memory::paging::is_user_address(a1 + total - 1)
    {
        return 0;
    }
    unsafe {
        core::ptr::copy_nonoverlapping(buf.as_ptr(), a1 as *mut u8, total as usize);
    }
    n as u64
}
