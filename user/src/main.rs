//! Morion OS 用户态测试程序 (Ring 3)
//!
//! 由内核在运行时加载到用户空间基址 USER_SPACE_BASE, 经 `switch_to_user`
//! 首次切入 Ring 3。入口 `_start` 必须位于镜像最前端 (offset 0)。

#![no_std]
#![no_main]

mod syscall;

use syscall::{print, print_u64, println, sys_recv, sys_send};

/// 用户程序入口 — 内核已设好用户栈 (rsp) 与用户参数 (rdi=域 id),
/// 此处按所属域 id 分流到不同角色后退出。
#[link_section = ".text._start"]
#[no_mangle]
pub extern "C" fn _start(domain_id: u64) -> ! {
    match domain_id {
        0 => sender_main(),
        1 => receiver_main(),
        _ => {}
    }
    syscall::sys_exit();
}

/// 域 0 — 发送者: 持有 SendTo(1) 能力, 无 SendTo(2) 能力。
fn sender_main() {
    println("sender (domain 0) starting...");

    // 1. 有能力的发送 → 应成功 (返回 1)。
    let ok = sys_send(1, 42);
    if ok == 1 {
        println("send to domain 1: OK (tag=42)");
    } else {
        println("send to domain 1: FAILED");
    }

    // 2. 无能力的发送 → 应被拒绝 (返回 0)。
    let denied = sys_send(2, 99);
    if denied == 0 {
        println("send to domain 2: DENIED (no capability)");
    } else {
        println("send to domain 2: UNEXPECTED SUCCESS");
    }

    println("sender done, exiting...");
}

/// 域 1 — 接收者: 从本域邮箱接收消息并打印 tag。
fn receiver_main() {
    println("receiver (domain 1) starting...");

    let tag = sys_recv();
    print("received message, tag = ");
    print_u64(tag);
    println("");

    println("receiver done, exiting...");
}

#[cfg(target_os = "none")]
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    // 用户态 panic: 无法恢复, 直接终止本任务。
    syscall::sys_exit();
}
