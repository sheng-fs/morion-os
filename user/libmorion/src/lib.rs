//! MorionOS 用户态运行库（libmorion）
//!
//! 这是所有用户态程序共用的"运行时"—— 相当于 C 程序的 crt0 + libc 的最小子集：
//!
//! - [`syscall`]：系统调用封装（`sys_*`）+ 终端打印（`print` / `println` / `print_u64` …）
//! - [`vfs`]：libvfs（fd / 挂载路由 / 能力句柄守卫），文件服务的客户端
//! - [`exec`]：可执行文件加载 —— 从文件系统读一个 `.mex` 镜像交给内核启动
//! - 程序入口样板：`_start`（crt0）+ `#[panic_handler]`，程序只写自己的 `morion_main`
//!
//! # 程序怎么写
//!
//! ```ignore
//! #![no_std]
//! #![no_main]
//!
//! fn main_body(domain_id: u64) { /* ... */ }
//!
//! #[no_mangle]
//! pub extern "C" fn morion_main(domain_id: u64) {
//!     main_body(domain_id);
//! }
//! ```
//!
//! 入口 `_start` 由本库提供（放在 `.text._start`，链接脚本保证它在镜像最前端），
//! 内核 `switch_to_user` 把域 id 放进 RDI 当第一个参数。
//!
//! # 链接约定
//!
//! 库本身不传链接参数：`-T user/linker.ld`（固定基址 `USER_SPACE_BASE`、`ENTRY(_start)`）
//! 与 `-nostdlib` 由**每个程序**自己的 `build.rs` 传（`cargo:rustc-link-arg`）。
//! 所有程序共用同一套链接地址与固定布局（见 `user/linker.ld` 与内核 `paging::USER_STACK_*`）。

#![no_std]

pub mod exec;
pub mod syscall;
pub mod vfs;

extern "C" {
    /// 程序主函数：由程序自己定义（`#[no_mangle] pub extern "C" fn morion_main(domain_id: u64)`）。
    ///
    /// 没有 `main` 返回值的概念 —— 返回即视为程序结束。
    fn morion_main(domain_id: u64);
}

/// 程序入口（crt0）：内核已设好用户栈，RDI = 本域 id。
///
/// 必须位于 `.text._start`（链接脚本把它排在镜像最前端，`ENTRY(_start)`），
/// 否则会变成"镜像里第一段不是入口"。
#[link_section = ".text._start"]
#[no_mangle]
pub extern "C" fn _start(domain_id: u64) -> ! {
    // 记下域 id: 程序与库都能随时问 `morion::syscall::domain_id()`。
    syscall::set_domain_id(domain_id);
    unsafe { morion_main(domain_id) };
    // 程序主函数返回 = 正常结束（`sys_exit` 不返回）。
    syscall::sys_exit()
}

/// 恐慌处理：打印一行（走 `SYS_PUTS`，与其它输出同一通路）后退出本域。
///
/// 在库里定义意味着**每个程序自动获得**它，程序不必自己写；一个二进制里只能有一份，
/// 而依赖图里只有 libmorion 提供，故不会冲突。
#[cfg(target_os = "none")]
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    syscall::println("程序恐慌 (panic)，已终止本域");
    syscall::sys_exit()
}
