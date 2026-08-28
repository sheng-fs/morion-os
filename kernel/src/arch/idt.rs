//! IDT (中断描述符表) — CPU 异常向量 0..=31 与硬件中断的处理
//!
//! 注册的处理器:
//!   - breakpoint (#BP, 向量 3)     : 用于验证 IDT 是否工作
//!   - double fault (#DF, 向量 8)   : 栈溢出等致命错误, 使用独立 IST 栈
//!   - page fault (#PF, 向量 14)    : 内存管理核心异常
//!   - timer (IRQ0, 向量 32)        : 时钟中断, 驱动抢占式调度
//!   - keyboard (IRQ1, 向量 33)     : 键盘中断, 打印 scancode

use spin::Once;
use x86_64::structures::idt::{InterruptDescriptorTable, InterruptStackFrame, PageFaultErrorCode};

use super::gdt::DOUBLE_FAULT_IST_INDEX;

static IDT: Once<InterruptDescriptorTable> = Once::new();

extern "x86-interrupt" fn breakpoint_handler(_stack_frame: InterruptStackFrame) {
    crate::video::println("[IDT] Breakpoint exception (#BP) caught");
}

extern "x86-interrupt" fn double_fault_handler(
    _stack_frame: InterruptStackFrame,
    error_code: u64,
) -> ! {
    crate::video::clear(0x330000);
    crate::video::set_cursor(20, 20);
    crate::video::println("DOUBLE FAULT");
    crate::video::print("error code: 0x");
    crate::video::print_hex(error_code);
    crate::halt();
}

extern "x86-interrupt" fn page_fault_handler(
    _stack_frame: InterruptStackFrame,
    error_code: PageFaultErrorCode,
) {
    crate::video::clear(0x003300);
    crate::video::set_cursor(20, 20);
    crate::video::println("PAGE FAULT");
    crate::video::print("error code: 0x");
    crate::video::print_hex(error_code.bits());
    crate::halt();
}

/// 时钟中断 (IRQ0, 向量 32) — 抢占式调度的时钟心跳
extern "x86-interrupt" fn timer_handler(_stack_frame: InterruptStackFrame) {
    // 先结束中断再切换: 否则被切走任务的 send_eoi 尚未执行,
    // 而首次进入的新任务不会补发 EOI, 会导致后续 IRQ0 被屏蔽。
    super::pic::send_eoi();
    crate::scheduler::tick();
}

/// 键盘中断 (IRQ1, 向量 33)
extern "x86-interrupt" fn keyboard_handler(_stack_frame: InterruptStackFrame) {
    let scancode = super::keyboard::read_scancode();
    crate::video::print("[keyboard] scancode = 0x");
    crate::video::print_hex(scancode as u64);
    crate::video::println("");
    super::pic::send_eoi();
}

/// 初始化并加载 IDT (仅需调用一次)
pub fn init() {
    let idt = IDT.call_once(|| {
        let mut idt = InterruptDescriptorTable::new();
        idt.breakpoint.set_handler_fn(breakpoint_handler);
        unsafe {
            idt.double_fault
                .set_handler_fn(double_fault_handler)
                .set_stack_index(DOUBLE_FAULT_IST_INDEX);
        }
        idt.page_fault.set_handler_fn(page_fault_handler);
        // 硬件中断 (PIC 重映射后: 时钟 → 32, 键盘 → 33)
        idt[32].set_handler_fn(timer_handler);
        idt[33].set_handler_fn(keyboard_handler);
        idt
    });
    idt.load();
}
