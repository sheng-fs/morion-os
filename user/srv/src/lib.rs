//! Morion OS 用户态系统服务库 (E2b 拆分产物)。
//!
//! 过渡态把 14 个服务塞进同一份镜像、按 `domain_id` 分流; 这里把它们拆成各自独立的
//! 程序 —— 每个程序 crate 只实现自己的 `morion_main`, 调用本库对应模块的 `run()`。
//! 各模块间唯一的共享面是 [`common`]。(E3c / G1 起又新增 `init` 与 `gfx_srv`, 共 16 个。)
//!
//! 每个服务模块用 `#[cfg(feature = "svc-<name>")]` 门控: 一个 bin 只编译自己的
//! 模块 (加始终编译的 `common`), 不把其余服务的代码也带进来。

#![no_std]
#![allow(dead_code)]

pub mod common;

#[cfg(feature = "svc-app")]
pub mod app;
#[cfg(feature = "svc-block_srv")]
pub mod block_srv;
#[cfg(feature = "svc-echo")]
pub mod echo;
#[cfg(feature = "svc-exfat_srv")]
pub mod exfat_srv;

/// 每个服务程序在入口打印一行身份 (名字 + 自己的域 id)。
///
/// 由各 `src/bin/*.rs` 在调用对应模块的 `run()` 之前调用 —— 它是"每个服务各自独立
/// ELF、跑在自己的域 / 地址空间里"最直接的正面证据 (启动日志里能看到 14 行, 名字与
/// 域号一一对应)。
pub fn announce(name: &str, domain_id: u64) {
    use morion::syscall::{print, print_u64, println};
    print("[up] ");
    print(name);
    print(" (domain ");
    print_u64(domain_id);
    println(")");
}
#[cfg(feature = "svc-ext2_srv")]
pub mod ext2_srv;
#[cfg(feature = "svc-fat32_srv")]
pub mod fat32_srv;
#[cfg(feature = "svc-gfx_srv")]
pub mod gfx;
#[cfg(feature = "svc-gfx_srv")]
pub mod gfx_srv;
#[cfg(feature = "svc-init")]
pub mod init;
#[cfg(feature = "svc-kbd")]
pub mod kbd;
#[cfg(feature = "svc-mfs_srv")]
pub mod mfs_srv;
#[cfg(feature = "svc-mount_srv")]
pub mod mount_srv;
#[cfg(feature = "svc-net_srv")]
pub mod net_srv;
#[cfg(feature = "svc-pager")]
pub mod pager;
#[cfg(feature = "svc-receiver")]
pub mod receiver;
#[cfg(feature = "svc-sender")]
pub mod sender;
#[cfg(feature = "svc-shell")]
pub mod shell;
#[cfg(feature = "svc-tmpfs_srv")]
pub mod tmpfs_srv;
#[cfg(feature = "svc-virtio_blk_srv")]
pub mod virtio_blk_srv;
