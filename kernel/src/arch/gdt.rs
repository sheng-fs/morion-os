//! GDT (全局描述符表) + TSS (任务状态段) 初始化
//!
//! 在 x86_64 长模式下, 分段被大幅弱化, GDT 主要用于:
//!   - 提供 TSS 段描述符 (内核栈切换 / 特权级)
//!   - 提供 syscall/sysret 与中断门所需的段选择子
//!
//! 阶段 9: 补充 user_code / user_data 段, 支持 Ring 3 用户态;
//! TSS 的 RSP0 供 Ring3 → Ring0 的中断/异常切换内核栈使用。

use spin::Once;
use x86_64::instructions::segmentation::{Segment, CS, DS, ES, SS};
use x86_64::instructions::tables::load_tss;
use x86_64::structures::gdt::{Descriptor, GlobalDescriptorTable, SegmentSelector};
use x86_64::structures::tss::TaskStateSegment;
use x86_64::VirtAddr;

/// 双重异常 (double fault) 使用的中断栈表 (IST) 索引
pub const DOUBLE_FAULT_IST_INDEX: u16 = 0;

/// GDT 段选择子 (由 `init` 中 `append` 顺序决定):
///
///   null(0) | kernel_code(0x08) | kernel_data(0x10)
///   | user_data(0x18) | user_code(0x20) | tss(0x28, 16 字节)
pub const KERNEL_CODE_SEL: u16 = 0x08;
pub const KERNEL_DATA_SEL: u16 = 0x10;
pub const USER_DATA_SEL: u16 = 0x18; // index 3
pub const USER_CODE_SEL: u16 = 0x20; // index 4

/// 用户态 (RPL=3) 选择子。
pub const USER_DATA_SEL_RPL3: u16 = USER_DATA_SEL | 3; // 0x1B
pub const USER_CODE_SEL_RPL3: u16 = USER_CODE_SEL | 3; // 0x23

struct Selectors {
    code: SegmentSelector,
    data: SegmentSelector,
    tss: SegmentSelector,
}

static TSS: Once<TaskStateSegment> = Once::new();
static GDT: Once<(GlobalDescriptorTable, Selectors)> = Once::new();

/// 初始化并加载 GDT + TSS (仅需调用一次)
pub fn init() {
    // TSS 需要 'static 生命周期, 故用 Once 延迟初始化后取静态引用
    let tss = TSS.call_once(|| {
        let mut tss = TaskStateSegment::new();
        // 为双重异常分配独立的 IST 栈, 避免栈溢出时触发 triple fault
        tss.interrupt_stack_table[DOUBLE_FAULT_IST_INDEX as usize] = {
            const STACK_SIZE: usize = 4096 * 5;
            static mut STACK: [u8; STACK_SIZE] = [0; STACK_SIZE];
            let stack_start = VirtAddr::from_ptr(unsafe { STACK.as_ptr() });
            stack_start + STACK_SIZE as u64
        };
        tss
    });

    let (gdt, selectors) = GDT.call_once(|| {
        let mut gdt = GlobalDescriptorTable::new();
        let code = gdt.append(Descriptor::kernel_code_segment());
        let data = gdt.append(Descriptor::kernel_data_segment());
        // 用户段: 先 data 后 code (选择子须与上方常量一致)
        gdt.append(Descriptor::user_data_segment());
        gdt.append(Descriptor::user_code_segment());
        let tss = gdt.append(Descriptor::tss_segment(tss));
        (gdt, Selectors { code, data, tss })
    });

    gdt.load();
    unsafe {
        CS::set_reg(selectors.code);
        DS::set_reg(selectors.data);
        ES::set_reg(selectors.data);
        SS::set_reg(selectors.data);
        load_tss(selectors.tss);
    }
}

/// 更新 TSS 的 RSP0 — Ring3 → Ring0 中断/异常时使用的内核栈顶。
///
/// 调度器在切换任务时调用, 将 RSP0 指向新任务的内核栈顶。
pub fn set_rsp0(stack_top: u64) {
    let tss = TSS
        .get()
        .expect("gdt::set_rsp0: TSS not initialized") as *const TaskStateSegment
        as *mut TaskStateSegment;
    unsafe {
        (*tss).privilege_stack_table[0] = VirtAddr::new(stack_top);
    }
}
