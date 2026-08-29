//! 系统调用封装 (libuser 雏形)
//!
//! 系统调用编号须与 kernel/src/syscall.rs 保持一致。
//! ABI: 编号在 `rax`, 参数在 `rdi/rsi/rdx`, 返回值在 `rax`。

// 预留的 syscall 封装 (yield/send/recv) 后续阶段才会使用, 先抑制 dead_code 警告。
#![allow(dead_code)]

use core::arch::asm;

pub const SYS_YIELD: u64 = 0;
pub const SYS_SLEEP: u64 = 1;
pub const SYS_SEND: u64 = 2;
pub const SYS_RECV: u64 = 3;
pub const SYS_PUTS: u64 = 4;
pub const SYS_EXIT: u64 = 5;
pub const SYS_ALLOC_PAGE: u64 = 6;
pub const SYS_SHARE_PAGE: u64 = 7;

#[inline(always)]
unsafe fn syscall(n: u64, a1: u64, a2: u64, a3: u64) -> u64 {
    let ret: u64;
    asm!(
        "syscall",
        inlateout("rax") n => ret,
        // rdi/rsi/rdx 是 syscall 参数寄存器, 内核 syscall_entry 会改写它们
        // (rdi←编号, rsi←a1, rdx←a2), 故须用 inout 声明并丢弃输出, 否则
        // 编译器会假设它们跨 syscall 不变 (复用 rdi 作写地址导致页错误)。
        inout("rdi") a1 => _,
        inout("rsi") a2 => _,
        inout("rdx") a3 => _,
        // rcx/r11 被 syscall 指令本身改写; r8/r9/r10 是 caller-saved,
        // 内核 syscall_entry 并不保存它们 (会经 syscall_dispatch 被破坏)。
        // 必须声明为 clobber, 否则编译器会假设它们跨 syscall 不变。
        lateout("rcx") _,
        lateout("r11") _,
        lateout("r8") _,
        lateout("r9") _,
        lateout("r10") _,
        options(nostack)
    );
    ret
}

pub fn sys_yield() {
    unsafe {
        syscall(SYS_YIELD, 0, 0, 0);
    }
}

pub fn sys_sleep(ms: u64) {
    unsafe {
        syscall(SYS_SLEEP, ms, 0, 0);
    }
}

pub fn sys_send(to: u64, tag: u64) -> u64 {
    unsafe { syscall(SYS_SEND, to, tag, 0) }
}

pub fn sys_recv() -> u64 {
    unsafe { syscall(SYS_RECV, 0, 0, 0) }
}

pub fn sys_alloc_page(vaddr: u64) -> u64 {
    unsafe { syscall(SYS_ALLOC_PAGE, vaddr, 0, 0) }
}

pub fn sys_share_page(vaddr: u64, to: u64) -> u64 {
    unsafe { syscall(SYS_SHARE_PAGE, vaddr, to, 0) }
}

pub fn sys_puts(s: &str) {
    unsafe {
        syscall(SYS_PUTS, s.as_ptr() as u64, s.len() as u64, 0);
    }
}

/// 终止当前用户任务 (永不返回)。
pub fn sys_exit() -> ! {
    unsafe {
        syscall(SYS_EXIT, 0, 0, 0);
    }
    loop {}
}

// ---------------------------------------------------------------------------
// 极简打印辅助 (core-only, 无分配器)
// ---------------------------------------------------------------------------

pub fn print(s: &str) {
    sys_puts(s);
}

pub fn println(s: &str) {
    sys_puts(s);
    sys_puts("\n");
}

/// 以十进制打印无符号整数。
pub fn print_u64(mut v: u64) {
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    loop {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    let s = unsafe { core::str::from_utf8_unchecked(&buf[i..]) };
    sys_puts(s);
}
