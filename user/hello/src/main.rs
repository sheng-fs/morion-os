//! Morion OS 可执行文件加载演示程序 —— 一份**独立的 ELF**（`.mex`）。
//!
//! 它不是引导期服务程序（那些在 `user/srv/` 里，各自一份独立 ELF）的一部分, 也不在引导期被加载: 它由运行中的域
//! 经 `SYS_SPAWN_ELF` 从**文件系统里的一个文件**运行时载入到一个**新域**并启动。
//!
//! 程序本身只写主逻辑 —— 入口 `_start`（crt0）、panic 处理、syscall 封装与打印都来自
//! 运行库 **libmorion**（这就是"运行库"存在的意义: 一份程序不再各带一套桩）。

#![no_std]
#![no_main]

use morion::syscall::{domain_id, print, print_hex, print_u64, println};

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
}
