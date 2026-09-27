//! Morion OS 可执行文件加载演示程序 —— 一份**独立的 ELF**。
//!
//! 它不是主程序（`user/src/main.rs`）的一部分, 也不在引导期被加载: 它由运行中的域
//! 经 `SYS_SPAWN_ELF` 从内存里的镜像**运行时载入**到一个**新域**并启动
//! （自测里先把这段二进制写进文件系统再读回来, 走的就是"可执行文件加载"这条路）。
//!
//! 因此它自己带一份最小 syscall 桩: 目前还没有共享运行库 (libmorion) —— 那是下一步
//! 「运行库 + 服务拆分」的事, 这里刻意保持零依赖, 只验证加载器本身。

#![no_std]
#![no_main]

use core::arch::asm;

const SYS_PUTS: u64 = 4;
const SYS_EXIT: u64 = 5;

#[inline(always)]
unsafe fn syscall(n: u64, a1: u64, a2: u64, a3: u64) -> u64 {
    let ret: u64;
    asm!(
        "syscall",
        inlateout("rax") n => ret,
        // rdi/rsi/rdx 是 syscall 参数寄存器, 内核 syscall_entry 会改写它们,
        // 故须用 inout 声明并丢弃输出 (与 user/src/syscall.rs 同一份 ABI 约定)。
        inout("rdi") a1 => _,
        inout("rsi") a2 => _,
        inout("rdx") a3 => _,
        // rcx/r11 被 syscall 指令改写; r8/r9/r10 是 caller-saved 且内核不保存。
        lateout("rcx") _,
        lateout("r11") _,
        lateout("r8") _,
        lateout("r9") _,
        lateout("r10") _,
        options(nostack)
    );
    ret
}

fn puts(s: &str) {
    unsafe {
        syscall(SYS_PUTS, s.as_ptr() as u64, s.len() as u64, 0);
    }
}

/// 打印 u64 的十六进制（不带前缀, 省略前导零）。
fn put_hex(v: u64) {
    let digits = b"0123456789abcdef";
    let mut buf = [0u8; 16];
    let mut i = 16;
    let mut val = v;
    loop {
        i -= 1;
        buf[i] = digits[(val & 0xF) as usize];
        val >>= 4;
        if val == 0 {
            break;
        }
    }
    puts(unsafe { core::str::from_utf8_unchecked(&buf[i..]) });
}

fn print_u64(v: u64) {
    let mut buf = [0u8; 20];
    let mut i = 20;
    let mut val = v;
    if val == 0 {
        puts("0");
        return;
    }
    while val > 0 && i > 0 {
        i -= 1;
        buf[i] = (val % 10) as u8 + b'0';
        val /= 10;
    }
    puts(unsafe { core::str::from_utf8_unchecked(&buf[i..]) });
}

/// 入口（必须位于镜像最前端, 见 `user/linker.ld` 的 `ENTRY(_start)` 约定）。
///
/// 内核 `switch_to_user` 把新域 id 放进 RDI 当作第一个参数, 与主程序同一约定。
#[link_section = ".text._start"]
#[no_mangle]
pub extern "C" fn _start(domain_id: u64) -> ! {
    puts("exec: 我是运行时被加载的独立 ELF 程序 (morion-hello)");
    puts(", 我的域 = ");
    print_u64(domain_id);
    puts(", 入口 = 0x");
    // 打印自身入口地址: 证明镜像确实被载入到它链接的那个虚拟地址上。
    put_hex(_start as *const () as usize as u64);
    puts("\n");
    unsafe {
        syscall(SYS_EXIT, 0, 0, 0);
    }
    loop {
        core::hint::spin_loop();
    }
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    puts("exec: hello program panicked\n");
    unsafe {
        syscall(SYS_EXIT, 1, 0, 0);
    }
    loop {
        core::hint::spin_loop();
    }
}
