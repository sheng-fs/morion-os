//! Morion OS 用户态测试程序 (Ring 3)
//!
//! 由内核在运行时加载到用户空间基址 USER_SPACE_BASE, 经 `switch_to_user`
//! 首次切入 Ring 3。入口 `_start` 必须位于镜像最前端 (offset 0)。

#![no_std]
#![no_main]

mod syscall;

use syscall::{print, print_u64, println, sys_alloc_page, sys_recv, sys_send, sys_share_page};

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

/// 域 0 — 发送者: 持有 SendTo(1) + MapInto(1) 能力, 无 SendTo(2) 能力。
fn sender_main() {
    println("sender (domain 0) starting...");

    // 共享内存演示: 申请一页 → 写入 → 共享给域 1 → IPC 通知。
    let page = 0x8000_0030_00u64;
    if sys_alloc_page(page) == 1 {
        let msg = "HELLO SHARED";
        unsafe {
            core::ptr::copy_nonoverlapping(msg.as_ptr(), page as *mut u8, msg.len());
        }
        println("sender: wrote \"HELLO SHARED\" to shared page");
    } else {
        println("sender: alloc_page FAILED");
    }

    // 共享给域 1 (需 MapInto(1) 能力)。
    if sys_share_page(page, 1) == 1 {
        println("sender: shared page with domain 1");
    } else {
        println("sender: share_page DENIED");
    }

    // IPC 通知 receiver 读取 (复用现有 SendTo 能力)。
    let sent = sys_send(1, 777);
    if sent == 1 {
        println("sender: notified receiver (tag=777)");
    }

    println("sender done, exiting...");
}

/// 域 1 — 接收者: 经 IPC 收到通知后, 直接从共享页读取数据。
fn receiver_main() {
    println("receiver (domain 1) starting...");

    // 等 sender 通知共享页就绪。
    let tag = sys_recv();
    print("receiver: got notify, tag = ");
    print_u64(tag);
    println("");

    // 直接读共享页 (零拷贝, 数据未经 IPC 传递)。
    let page = 0x8000_0030_00u64;
    let bytes = unsafe { core::slice::from_raw_parts(page as *const u8, 12) };
    let s = unsafe { core::str::from_utf8_unchecked(bytes) };
    print("receiver: read from shared page -> \"");
    print(s);
    println("\"");

    println("receiver done, exiting...");
}

#[cfg(target_os = "none")]
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    // 用户态 panic: 无法恢复, 直接终止本任务。
    syscall::sys_exit();
}
