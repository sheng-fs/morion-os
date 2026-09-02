//! IDT (中断描述符表) — CPU 异常向量 0..=31 与硬件中断的处理
//!
//! 注册的处理器:
//!   - breakpoint (#BP, 向量 3)     : 用于验证 IDT 是否工作
//!   - double fault (#DF, 向量 8)   : 栈溢出等致命错误, 使用独立 IST 栈
//!   - page fault (#PF, 向量 14)    : 内存管理核心异常
//!   - timer (IRQ0, 向量 32)        : 时钟中断, 驱动抢占式调度
//!   - keyboard (IRQ1, 向量 33)     : 键盘中断, 转发 scancode 给用户态驱动

use spin::Once;
use x86_64::structures::idt::{InterruptDescriptorTable, InterruptStackFrame, PageFaultErrorCode};

use super::gdt::DOUBLE_FAULT_IST_INDEX;

static IDT: Once<InterruptDescriptorTable> = Once::new();

extern "x86-interrupt" fn breakpoint_handler(_stack_frame: InterruptStackFrame) {
    crate::video::println("[IDT] Breakpoint exception (#BP) caught");
}

extern "x86-interrupt" fn double_fault_handler(
    stack_frame: InterruptStackFrame,
    error_code: u64,
) -> ! {
    crate::video::clear(0x330000);
    crate::video::set_cursor(20, 20);
    crate::video::println("DOUBLE FAULT");
    crate::video::print("error code: 0x");
    crate::video::print_hex(error_code);
    crate::video::println("");
    crate::video::print("rip: 0x");
    crate::video::print_hex(stack_frame.instruction_pointer.as_u64());
    crate::video::println("");
    crate::video::print("rsp: 0x");
    crate::video::print_hex(stack_frame.stack_pointer.as_u64());
    crate::video::println("");
    crate::video::print("cs: 0x");
    crate::video::print_hex(stack_frame.code_segment.0 as u64);
    crate::video::println("");
    crate::halt();
}

/// 页表逐级 walk, 用于定位内核态缺页时到底是哪一级条目非法。
/// 通过 offset 映射 (PHYS_OFFSET) 访问页表, 不依赖被破坏的映射本身。
fn dump_page_walk(addr: u64) {
    let off = crate::memory::paging::PHYS_OFFSET;
    let cr3 = x86_64::registers::control::Cr3::read()
        .0
        .start_address()
        .as_u64();
    let phys_mask = 0x000F_FFFF_FFFF_F000u64;

    let pml4 = (off + cr3) as *const u64;
    let p4e = unsafe { *pml4.add(((addr >> 39) & 0x1FF) as usize) };
    crate::video::print("P4E:  0x");
    crate::video::print_hex(p4e);
    crate::video::println("");
    if p4e & 1 == 0 {
        return;
    }

    let pdpt = (off + (p4e & phys_mask)) as *const u64;
    let pdpe = unsafe { *pdpt.add(((addr >> 30) & 0x1FF) as usize) };
    crate::video::print("PDPE: 0x");
    crate::video::print_hex(pdpe);
    crate::video::println("");
    if pdpe & 1 == 0 {
        return;
    }
    if pdpe & 0x80 != 0 {
        crate::video::println("(1 GiB 大页)");
        return;
    }

    let pd = (off + (pdpe & phys_mask)) as *const u64;
    let pde = unsafe { *pd.add(((addr >> 21) & 0x1FF) as usize) };
    crate::video::print("PDE:  0x");
    crate::video::print_hex(pde);
    crate::video::println("");
    if pde & 1 == 0 {
        return;
    }
    if pde & 0x80 != 0 {
        crate::video::println("(2 MiB 大页)");
        return;
    }

    let pt = (off + (pde & phys_mask)) as *const u64;
    let pte = unsafe { *pt.add(((addr >> 12) & 0x1FF) as usize) };
    crate::video::print("PTE:  0x");
    crate::video::print_hex(pte);
    crate::video::println("");
}

extern "x86-interrupt" fn page_fault_handler(
    stack_frame: InterruptStackFrame,
    error_code: PageFaultErrorCode,
) {
    let fault_addr = x86_64::registers::control::Cr2::read()
        .unwrap_or(x86_64::VirtAddr::zero())
        .as_u64();

    // 内核态缺页不是「按需分页」, 而是越界访问/坏指针等 bug。若仍按分页器
    // 转发, 会把内核态地址当作用户缺页处理, 导致调度状态混乱 → 三重故障。
    // 因此这里直接红屏打印现场并停机, 便于定位。
    if !error_code.contains(PageFaultErrorCode::USER_MODE) {
        crate::video::clear(0x003300);
        crate::video::set_cursor(4, 4);
        crate::video::println("=== KERNEL PAGE FAULT (bug) ===");
        crate::video::print("cr2:        0x");
        crate::video::print_hex(fault_addr);
        crate::video::println("");
        crate::video::print("rip:        0x");
        crate::video::print_hex(stack_frame.instruction_pointer.as_u64());
        crate::video::println("");
        crate::video::print("error code: 0x");
        crate::video::print_hex(error_code.bits() as u64);
        crate::video::println("");
        crate::video::print("rsp:        0x");
        crate::video::print_hex(stack_frame.stack_pointer.as_u64());
        crate::video::println("");
        dump_page_walk(fault_addr);
        crate::halt();
    }

    // 按需分页: 捕获缺页地址并转发给该域的分页器, 然后阻塞当前任务。
    // 分页器映射页面并 reply 后, 本任务被重新调度, iretq 恢复现场并
    // 重新执行那条缺页指令。
    let fault_domain = crate::scheduler::current_domain();
    let pager = crate::pager::of(fault_domain);

    crate::pager::deliver_fault(
        pager,
        crate::pager::PageFaultInfo {
            fault_domain,
            fault_addr,
            error_code: error_code.bits(),
        },
    );
    crate::scheduler::block_current(fault_domain);
}

/// 时钟中断 (IRQ0, 向量 32) — 抢占式调度的时钟心跳
extern "x86-interrupt" fn timer_handler(_stack_frame: InterruptStackFrame) {
    // 先结束中断再切换: 否则被切走任务的 send_eoi 尚未执行,
    // 而首次进入的新任务不会补发 EOI, 会导致后续 IRQ0 被屏蔽。
    super::pic::send_eoi();
    crate::scheduler::tick();
}

/// 键盘中断 (IRQ1, 向量 33) — 读取 scancode 后转发给用户态键盘驱动域。
extern "x86-interrupt" fn keyboard_handler(_stack_frame: InterruptStackFrame) {
    let scancode = super::keyboard::read_scancode();
    // 中断即 IPC: 把 scancode 作为消息 tag 转发给注册了 IRQ1 的驱动域。
    crate::irq::dispatch(1, scancode as u64);
    super::pic::send_eoi();
}

// ---------------------------------------------------------------------------
// CPU 异常兜底 ("崩溃黑匣子")
//
// 未单独处理的异常全部注册为: 红屏打印现场后停机 (不再返回)。
// 否则未处理异常 → #DF → 三重故障 → QEMU 直接退出, 无法定位崩溃点。
// ---------------------------------------------------------------------------

const EXCEPTION_NAMES: [&str; 32] = [
    "#DE 除零",
    "#DB 调试",
    "NMI 不可屏蔽中断",
    "#BP 断点",
    "#OF 溢出",
    "#BR 边界越界",
    "#UD 非法指令",
    "#NM 设备不可用",
    "#DF 双重故障",
    "保留(9)",
    "#TS 无效TSS",
    "#NP 段不存在",
    "#SS 栈段错误",
    "#GP 一般保护错误",
    "#PF 页错误",
    "保留(15)",
    "#x87 FPU 错误",
    "#AC 对齐检查",
    "#MC 机器检查",
    "#XM SIMD 浮点",
    "#VE 虚拟化",
    "#CP 控制保护",
    "保留(22)",
    "保留(23)",
    "保留(24)",
    "保留(25)",
    "保留(26)",
    "保留(27)",
    "#HV Hypervisor",
    "#VC VMM 通信",
    "#SX 安全",
    "保留(31)",
];

/// 崩溃报告: 红屏打印异常现场后停机。
///
/// 注意: 各 print 依次调用, 若崩溃发生在 video::print 持锁期间则屏幕只显示
/// 到 "CPU EXCEPTION" 之前 — 那本身也是有价值的定位信息。
fn report_crash(vector: usize, error_code: Option<u64>, frame: &InterruptStackFrame) -> ! {
    crate::video::clear(0x330000);
    crate::video::set_cursor(4, 4);
    let name = EXCEPTION_NAMES.get(vector).copied().unwrap_or("未知异常");
    crate::video::println("=========== CPU EXCEPTION (崩溃黑匣子) ===========");
    crate::video::print("exception:  ");
    crate::video::println(name);
    crate::video::print("vector:     ");
    crate::video::print_u64(vector as u64);
    crate::video::println("");
    if let Some(code) = error_code {
        crate::video::print("error code: 0x");
        crate::video::print_hex(code);
        crate::video::println("");
    }
    let cr2 = x86_64::registers::control::Cr2::read()
        .map(|a| a.as_u64())
        .unwrap_or(0);
    crate::video::print("cr2:        0x");
    crate::video::print_hex(cr2);
    crate::video::println("");
    crate::video::print("rip:        0x");
    crate::video::print_hex(frame.instruction_pointer.as_u64());
    crate::video::println("");
    crate::video::print("cs:         0x");
    crate::video::print_hex(frame.code_segment.0 as u64);
    crate::video::println("");
    crate::video::print("rsp:        0x");
    crate::video::print_hex(frame.stack_pointer.as_u64());
    crate::video::println("");
    crate::video::print("rflags:     0x");
    crate::video::print_hex(frame.cpu_flags.bits());
    crate::video::println("");
    // 最后再取域号 (若崩溃时持有调度锁会卡住, 但此前信息已在屏上)。
    crate::video::print("domain:     ");
    crate::video::print_u64(crate::scheduler::current_domain());
    crate::video::println("");
    crate::video::println("");
    crate::video::println("[HALTED] CPU 已停机, QEMU 保持运行, 请记录以上信息");
    crate::halt();
}

/// 生成异常兜底处理器 (带/不带错误码两种 ABI)。
/// 函数体以 report_crash (-> !) 结尾, 实际不会返回。
macro_rules! crash_handler {
    ($name:ident, $vector:expr, has_code) => {
        extern "x86-interrupt" fn $name(frame: InterruptStackFrame, code: u64) {
            report_crash($vector, Some(code), &frame)
        }
    };
    ($name:ident, $vector:expr, no_code) => {
        extern "x86-interrupt" fn $name(frame: InterruptStackFrame) {
            report_crash($vector, None, &frame)
        }
    };
}

// 3 (#BP), 8 (#DF), 14 (#PF) 已单独注册, 不生成兜底处理器。
crash_handler!(de_handler, 0, no_code);
crash_handler!(db_handler, 1, no_code);
crash_handler!(nmi_handler, 2, no_code);
crash_handler!(of_handler, 4, no_code);
crash_handler!(br_handler, 5, no_code);
crash_handler!(ud_handler, 6, no_code);
crash_handler!(nm_handler, 7, no_code);
crash_handler!(ts_handler, 10, has_code);
crash_handler!(np_handler, 11, has_code);
crash_handler!(ss_handler, 12, has_code);
crash_handler!(gp_handler, 13, has_code);
crash_handler!(x87_handler, 16, no_code);
crash_handler!(ac_handler, 17, has_code);
crash_handler!(simd_handler, 19, no_code);
crash_handler!(ve_handler, 20, no_code);
crash_handler!(sx_handler, 30, has_code);

/// #MC 的处理器类型要求发散签名。
extern "x86-interrupt" fn mc_handler(frame: InterruptStackFrame) -> ! {
    report_crash(18, None, &frame)
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

        // 崩溃黑匣子: 其余全部 CPU 异常 → 红屏报告 + 停机。
        // 索引路径只接受 fn(InterruptStackFrame); 带错误码的异常走具名字段。
        idt[0].set_handler_fn(de_handler);
        idt[1].set_handler_fn(db_handler);
        idt[2].set_handler_fn(nmi_handler);
        idt[4].set_handler_fn(of_handler);
        idt[5].set_handler_fn(br_handler);
        idt[6].set_handler_fn(ud_handler);
        idt[7].set_handler_fn(nm_handler);
        idt.invalid_tss.set_handler_fn(ts_handler);
        idt.segment_not_present.set_handler_fn(np_handler);
        idt.stack_segment_fault.set_handler_fn(ss_handler);
        idt.general_protection_fault.set_handler_fn(gp_handler);
        idt[16].set_handler_fn(x87_handler);
        idt.alignment_check.set_handler_fn(ac_handler);
        idt.machine_check.set_handler_fn(mc_handler);
        idt[19].set_handler_fn(simd_handler);
        idt[20].set_handler_fn(ve_handler);
        idt.security_exception.set_handler_fn(sx_handler);

        // 硬件中断 (PIC 重映射后: 时钟 → 32, 键盘 → 33)
        idt[32].set_handler_fn(timer_handler);
        idt[33].set_handler_fn(keyboard_handler);
        idt
    });
    idt.load();
}
