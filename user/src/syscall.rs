//! 系统调用封装 (libuser 雏形)
//!
//! 系统调用编号须与 kernel/src/syscall.rs 保持一致。
//! ABI: 编号在 `rax`, 参数在 `rdi/rsi/rdx`, 返回值在 `rax`。

// 预留的 syscall 封装 (yield/send/recv) 后续阶段才会使用, 先抑制 dead_code 警告。
#![allow(dead_code)]

use core::arch::asm;
use core::cell::UnsafeCell;

pub const SYS_YIELD: u64 = 0;
pub const SYS_SLEEP: u64 = 1;
pub const SYS_SEND: u64 = 2;
pub const SYS_RECV: u64 = 3;
pub const SYS_PUTS: u64 = 4;
pub const SYS_EXIT: u64 = 5;
pub const SYS_ALLOC_PAGE: u64 = 6;
pub const SYS_SHARE_PAGE: u64 = 7;
pub const SYS_UNMAP: u64 = 8;
pub const SYS_MAP_ANON: u64 = 9;
pub const SYS_PAGE_FAULT_REPLY: u64 = 10;
pub const SYS_CALL: u64 = 12;
pub const SYS_REPLY: u64 = 13;
pub const SYS_REGISTER_IRQ: u64 = 14;
pub const SYS_SCROLL_UP: u64 = 15;
pub const SYS_SCROLL_DOWN: u64 = 16;
pub const SYS_BACKSPACE: u64 = 17;
pub const SYS_TERM_PUT: u64 = 18;
pub const SYS_TERM_LEFT: u64 = 19;
pub const SYS_TERM_RIGHT: u64 = 20;
pub const SYS_MAP_MMIO: u64 = 21;
pub const SYS_PORT_IN8: u64 = 22;
pub const SYS_PORT_IN16: u64 = 23;
pub const SYS_PORT_OUT8: u64 = 24;
pub const SYS_PORT_OUT16: u64 = 25;
pub const SYS_VIRT_TO_PHYS: u64 = 26;
pub const SYS_READLINE: u64 = 27;
pub const SYS_CLEAR: u64 = 28;
pub const SYS_CAP_ISSUE: u64 = 29;
pub const SYS_CAP_LOOKUP: u64 = 30;
pub const SYS_CAP_DROP: u64 = 31;
pub const SYS_HANDLE_SEND: u64 = 32;
pub const SYS_CAP_SEND: u64 = 33;
pub const SYS_IRQ_POLL: u64 = 34;
pub const SYS_MSIX_ENABLE: u64 = 35;
pub const SYS_IRQ_WAIT: u64 = 36;

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

/// 消息 payload 固定大小 (与内核 `ipc::PAYLOAD_LEN` 一致)。
pub const PAYLOAD_LEN: usize = 96;

/// 发送带 payload 的消息 (payload 最多 `PAYLOAD_LEN` 字节, 超出部分截断)。
pub fn sys_send_payload(to: u64, tag: u64, payload: &[u8]) -> u64 {
    let mut buf = [0u8; PAYLOAD_LEN];
    let n = payload.len().min(PAYLOAD_LEN);
    buf[..n].copy_from_slice(&payload[..n]);
    unsafe { syscall(SYS_SEND, to, tag, buf.as_ptr() as u64) }
}

pub fn sys_recv() -> u64 {
    unsafe { syscall(SYS_RECV, 0, 0, 0) }
}

/// 阻塞接收一条消息, 把完整消息 (24 字节头 + `PAYLOAD_LEN` payload) 写入 `buf`,
/// 返回消息 tag。
pub fn sys_recv_msg(buf: *mut u8) -> u64 {
    unsafe { syscall(SYS_RECV, buf as u64, 0, 0) }
}

pub fn sys_call(to: u64, tag: u64) -> u64 {
    unsafe { syscall(SYS_CALL, to, tag, 0) }
}

/// 同步调用带 payload 的消息, 返回回复 tag。
pub fn sys_call_payload(to: u64, tag: u64, payload: &[u8]) -> u64 {
    let mut buf = [0u8; PAYLOAD_LEN];
    let n = payload.len().min(PAYLOAD_LEN);
    buf[..n].copy_from_slice(&payload[..n]);
    unsafe { syscall(SYS_CALL, to, tag, buf.as_ptr() as u64) }
}

pub fn sys_reply(tag: u64) -> u64 {
    unsafe { syscall(SYS_REPLY, tag, 0, 0) }
}

pub fn sys_register_irq(irq: u64) -> u64 {
    unsafe { syscall(SYS_REGISTER_IRQ, irq, 0, 0) }
}

/// 非阻塞取走 MSI/MSI-X 向量 `vector` 的「待处理」标志: 有中断到达返回 1。
///
/// 中断不投 IPC 消息 (那会和驱动的请求邮箱混在一起), 故驱动用本调用取位: 只有中断
/// 处理器置位才能让本调用返回 1。需持有 `Capability::Irq(vector)`。
pub fn sys_irq_poll(vector: u64) -> u64 {
    unsafe { syscall(SYS_IRQ_POLL, vector, 0, 0) }
}

/// 阻塞等待 MSI/MSI-X 向量 `vector` 的中断, 最多等 `timeout_ms` 毫秒。
///
/// 期间到达过中断返回 1, 超时返回 0 (调用方据此回退轮询)。等中断期间本域处于阻塞态,
/// CPU 交给别的域 (通常是空闲任务 `hlt`), 不占用时间片空转 —— 这正是与 `sys_irq_poll`
/// 自旋的本质区别。需持有 `Capability::Irq(vector)` 且是它的注册者。
pub fn sys_irq_wait(vector: u64, timeout_ms: u64) -> u64 {
    unsafe { syscall(SYS_IRQ_WAIT, vector, timeout_ms, 0) }
}

/// 打开本域所属设备的 MSI-X (需在写好 MSI-X 表项后调用), 成功返回 1。
///
/// 只有该设备的驱动域能调用, 且只成功一次; 配置空间写留在内核。
pub fn sys_msix_enable() -> u64 {
    unsafe { syscall(SYS_MSIX_ENABLE, 0, 0, 0) }
}

pub fn sys_scroll_up() -> u64 {
    unsafe { syscall(SYS_SCROLL_UP, 0, 0, 0) }
}

pub fn sys_scroll_down() -> u64 {
    unsafe { syscall(SYS_SCROLL_DOWN, 0, 0, 0) }
}

pub fn sys_backspace() -> u64 {
    unsafe { syscall(SYS_BACKSPACE, 0, 0, 0) }
}

pub fn sys_term_put(c: u8) -> u64 {
    unsafe { syscall(SYS_TERM_PUT, c as u64, 0, 0) }
}

pub fn sys_term_left() -> u64 {
    unsafe { syscall(SYS_TERM_LEFT, 0, 0, 0) }
}

pub fn sys_term_right() -> u64 {
    unsafe { syscall(SYS_TERM_RIGHT, 0, 0, 0) }
}

pub fn sys_alloc_page(vaddr: u64) -> u64 {
    unsafe { syscall(SYS_ALLOC_PAGE, vaddr, 0, 0) }
}

pub fn sys_share_page(vaddr: u64, to: u64) -> u64 {
    unsafe { syscall(SYS_SHARE_PAGE, vaddr, to, 0) }
}

pub fn sys_unmap(vaddr: u64) -> u64 {
    unsafe { syscall(SYS_UNMAP, vaddr, 0, 0) }
}

pub fn sys_map_anon(domain: u64, vaddr: u64) -> u64 {
    unsafe { syscall(SYS_MAP_ANON, domain, vaddr, 0) }
}

pub fn sys_page_fault_reply() -> u64 {
    unsafe { syscall(SYS_PAGE_FAULT_REPLY, 0, 0, 0) }
}

/// 把物理 MMIO 页 (`bar_paddr`, 页对齐) 映射到本域 `vaddr`, 需 Mmio 能力。
pub fn sys_map_mmio(bar_paddr: u64, vaddr: u64) -> u64 {
    unsafe { syscall(SYS_MAP_MMIO, bar_paddr, vaddr, 0) }
}

/// 从 I/O 端口 `port` 读一个字节。
pub fn sys_port_in8(port: u16) -> u8 {
    unsafe { syscall(SYS_PORT_IN8, port as u64, 0, 0) as u8 }
}

/// 从 I/O 端口 `port` 读一个 16 位字。
pub fn sys_port_in16(port: u16) -> u16 {
    unsafe { syscall(SYS_PORT_IN16, port as u64, 0, 0) as u16 }
}

/// 向 I/O 端口 `port` 写一个字节。
pub fn sys_port_out8(port: u16, value: u8) {
    unsafe {
        syscall(SYS_PORT_OUT8, port as u64, value as u64, 0);
    }
}

/// 向 I/O 端口 `port` 写一个 16 位字。
pub fn sys_port_out16(port: u16, value: u16) {
    unsafe {
        syscall(SYS_PORT_OUT16, port as u64, value as u64, 0);
    }
}

/// 查询本域用户虚拟地址 `vaddr` 对应的物理地址 (供 NVMe PRP 使用), 失败返回 0。
pub fn sys_virt_to_phys(vaddr: u64) -> u64 {
    unsafe { syscall(SYS_VIRT_TO_PHYS, vaddr, 0, 0) }
}

/// 阻塞读取一行控制台输入到 `buf` (最多 `buf.len()` 字节, 不含换行)。
/// 返回实际读到的字节数, 失败返回 `u64::MAX`。无输入时阻塞, 直到键盘回车提交一行。
pub fn sys_readline(buf: &mut [u8]) -> u64 {
    unsafe { syscall(SYS_READLINE, buf.as_mut_ptr() as u64, buf.len() as u64, 0) }
}

/// 清屏并复位终端状态 (历史 / 输入行 / 光标)。返回 1。
pub fn sys_clear() -> u64 {
    unsafe { syscall(SYS_CLEAR, 0, 0, 0) }
}

/// 「能力即句柄」: 为不透明对象 `obj` 签发一个句柄, 返回句柄索引 (0 起);
/// 句柄槽耗尽返回 `u64::MAX`。
pub fn sys_cap_issue(obj: u64) -> u64 {
    unsafe { syscall(SYS_CAP_ISSUE, obj, 0, 0) }
}

/// 校验句柄是否有效, 有效则返回其对象标识; 已被撤销 / 非法返回 `u64::MAX`。
pub fn sys_cap_lookup(handle: u64) -> u64 {
    unsafe { syscall(SYS_CAP_LOOKUP, handle, 0, 0) }
}

/// 撤销句柄 (关闭打开对象时调用), 成功返回 1。
pub fn sys_cap_drop(handle: u64) -> u64 {
    unsafe { syscall(SYS_CAP_DROP, handle, 0, 0) }
}

/// 「能力随 IPC 传递」: 把自己句柄 `handle` 指向的对象**移入**目标域 `to`,
/// 返回 `to` 域里的新句柄索引; 失败 / 目标域句柄槽满返回 `u64::MAX`。
///
/// 移动语义: 成功后本域的 `handle` 立即失效 (交出 fd 后自己不再持有)。
/// 前置能力 `SendTo(to)`。
pub fn sys_handle_send(to: u64, handle: u64) -> u64 {
    unsafe { syscall(SYS_HANDLE_SEND, to, handle, 0) }
}

/// 能力类型编码 (与内核 `cap::CAP_KIND_*` 一致)。
pub const CAP_KIND_SEND_TO: u64 = 0;
pub const CAP_KIND_MAP_INTO: u64 = 1;
pub const CAP_KIND_IRQ: u64 = 2;
pub const CAP_KIND_MMIO: u64 = 3;

/// 「能力随 IPC 传递」: 把自己**持有**的能力委派给目标域 `to`, 成功返回 1。
///
/// 前置能力 `SendTo(to)`; 且必须**确实持有**要委派的能力 (无放大: 没有的能力给不出去)。
/// `kind` 取 `CAP_KIND_*`, `arg` 是该能力的参数 (目标域 id / IRQ 号 / 页对齐 MMIO 基址)。
pub fn sys_cap_send(to: u64, kind: u64, arg: u64) -> u64 {
    unsafe { syscall(SYS_CAP_SEND, to, kind, arg) }
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
    loop {
        core::hint::spin_loop();
    }
}

// ---------------------------------------------------------------------------
// 极简打印辅助 (core-only, 无分配器)
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// 行缓冲打印
// ---------------------------------------------------------------------------
// 各域地址空间独立, 每个域有各自的缓冲; 单核下同一时刻仅一个域运行, 无需锁。
// 把「一行 = 多次 SYS_PUTS」合并为「一行 = 一次 SYS_PUTS」, 消除多域并发打印
// 在多次 syscall 之间被调度打断而造成的字符交错。

/// 可在 `static` 中存放可变数据的包装: 手动标记 `Sync`。
/// 安全前提: 单核 + 各域地址空间独立, 实际不存在对同一 static 的并发访问。
struct StaticCell<T>(UnsafeCell<T>);
unsafe impl<T> Sync for StaticCell<T> {}

impl<T> StaticCell<T> {
    const fn new(value: T) -> Self {
        StaticCell(UnsafeCell::new(value))
    }
    // 有意为之的内部可变性: 单核 + 各域地址空间独立, 不存在对同一 static 的
    // 并发访问, 故从 &self 返回 &mut T 是安全的, 无需改为 unsafe fn。
    #[allow(clippy::mut_from_ref)]
    fn borrow_mut(&self) -> &mut T {
        unsafe { &mut *self.0.get() }
    }
}

static PRINT_BUF: StaticCell<[u8; 256]> = StaticCell::new([0; 256]);
static PRINT_LEN: StaticCell<usize> = StaticCell::new(0);

/// 把字符串追加到行缓冲 (缓冲满时先提交当前行, 再继续写入)。
fn print_push(s: &str) {
    for &b in s.as_bytes() {
        let buf = PRINT_BUF.borrow_mut();
        let len = PRINT_LEN.borrow_mut();
        if *len >= buf.len() {
            // 缓冲已满: 先提交当前行, 避免后续字节被静默丢弃。
            let line = unsafe { core::str::from_utf8_unchecked(&buf[..*len]) };
            sys_puts(line);
            *len = 0;
        }
        buf[*len] = b;
        *len += 1;
    }
}

/// 提交当前行缓冲 (整行一次 syscall), 然后清空。
fn print_flush() {
    let len = PRINT_LEN.borrow_mut();
    if *len > 0 {
        let buf = PRINT_BUF.borrow_mut();
        let s = unsafe { core::str::from_utf8_unchecked(&buf[..*len]) };
        sys_puts(s);
        *len = 0;
    }
}

pub fn print(s: &str) {
    print_push(s);
}

pub fn println(s: &str) {
    print_push(s);
    print_push("\n");
    print_flush();
}

/// 立即提交当前行缓冲 (把尚未以换行结束的内容送出)。用于「行内提示符」——
/// 提示符须在用户输入前显示出来, 不能等换行。
pub fn flush() {
    print_flush();
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
    print_push(s);
}

/// 以十六进制打印无符号整数。
pub fn print_hex(mut v: u64) {
    let mut buf = [0u8; 16];
    let mut i = buf.len();
    loop {
        i -= 1;
        let d = (v & 0xF) as u8;
        buf[i] = if d < 10 { b'0' + d } else { b'a' + d - 10 };
        v >>= 4;
        if v == 0 {
            break;
        }
    }
    let s = unsafe { core::str::from_utf8_unchecked(&buf[i..]) };
    print_push(s);
}
