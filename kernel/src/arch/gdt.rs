//! GDT (全局描述符表) + TSS (任务状态段) 初始化
//!
//! 在 x86_64 长模式下, 分段被大幅弱化, GDT 主要用于:
//!   - 提供 TSS 段描述符 (硬件任务切换 / 内核栈切换 / 特权级)
//!   - 提供 syscall/sysret 与中断门所需的段选择子
//!
//! 后续用户态 (Ring 3) 支持时再补充 user_code/user_data 段。

use spin::Once;
use x86_64::instructions::segmentation::{Segment, CS, DS, ES, SS};
use x86_64::instructions::tables::load_tss;
use x86_64::structures::gdt::{Descriptor, GlobalDescriptorTable, SegmentSelector};
use x86_64::structures::tss::TaskStateSegment;
use x86_64::VirtAddr;

/// 双重异常 (double fault) 使用的中断栈表 (IST) 索引
pub const DOUBLE_FAULT_IST_INDEX: u16 = 0;

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