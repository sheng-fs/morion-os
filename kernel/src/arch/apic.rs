//! LAPIC (x86_64 本地 APIC) — MSI/MSI-X 的最小支撑 (阶段 4)
//!
//! 为什么需要它: MSI/MSI-X 中断的物理形式是**设备向 LAPIC 的「中断消息」地址写一条
//! 消息** (物理地址 = `0xFEE0_0000 | (apic_id << 12)`, 消息数据里带向量), LAPIC 收下
//! 后再交给 CPU。本项目此前只有 8259A PIC + PIT, 没有任何 LAPIC 代码 —— 不把 LAPIC
//! 打开, 这条写入就是打在一块「没有接收方」的地址上, MSI 永远不会到达。
//!
//! 这里只做 MSI 所必需的最小集, 且**不接管**现有 PIC 中断:
//!   - `IA32_APIC_BASE` (MSR 0x1B): 取基址, 确保 bit11 (EN) 置位 (LAPIC 硬使能);
//!   - `SVR`  (base+0xF0): 软使能 (bit8) + 伪中断向量 0xFF;
//!   - `TPR`  (base+0x80): 置 0, 所有优先级的向量都能投递;
//!   - `LVT0` (base+0x350): 必须保持「投递模式 ExtINT + 不屏蔽」。LAPIC 一旦使能,
//!     8259A 的 PIC 中断就改由 LINT0 以 ExtINT 方式透传给 CPU; KVM 正是以 LVT0 是否
//!     如此来判断「PIC 中断还要不要投递」(kvm_apic_accept_pic_intr), 若 LVT0 被屏蔽
//!     则时钟/键盘立刻失效。故这里只在它不满足时改写, 从不无谓覆盖固件已设好的值。
//!   - `EOI`  (base+0xB0): MSI 向量处理器结束时写 0 (ExtINT 透传来的中断不经 LAPIC
//!     的 ISR, 仍由 `pic::send_eoi()` 收尾, 无需 LAPIC EOI)。

use x86_64::registers::model_specific::Msr;

/// `IA32_APIC_BASE` MSR 编号。
const MSR_APIC_BASE: u32 = 0x001B;
/// LAPIC 寄存器相对基址的偏移。
const REG_ID: u64 = 0x20;
const REG_TPR: u64 = 0x80;
const REG_EOI: u64 = 0xB0;
const REG_SVR: u64 = 0xF0;
const REG_LVT0: u64 = 0x350;

/// `IA32_APIC_BASE` bit11: LAPIC 全局使能。
const APIC_BASE_ENABLE: u64 = 1 << 11;
/// `SVR` bit8: LAPIC 软件使能。
const SVR_SW_ENABLE: u32 = 1 << 8;
/// 伪中断向量 (`SVR` bits 0:7): 取最高向量 0xFF, 与 MSI 向量段 (0x50..0x5F) 不重叠。
const SPURIOUS_VECTOR: u32 = 0xFF;
/// `LVT0`: 投递模式 ExtINT (bits 10:8 = 0b111), mask (bit16) 清 0。
const LVT0_EXTINT: u32 = 0x700;
/// LVT 项的 mask 位。
const LVT_MASKED: u32 = 1 << 16;
/// MSI 中断消息地址的高位固定部分 (`0xFEE`), bits 19:12 放目的 APIC ID。
const MSI_ADDR_BASE: u32 = 0xFEE0_0000;

/// LAPIC 寄存器基址 (页对齐的物理地址, 位于恒等映射内 → 可直接当虚拟地址用)。
static mut BASE: u64 = 0;
/// 本 CPU 的 APIC ID。
static mut APIC_ID: u32 = 0;

/// 读/写一个 LAPIC 寄存器 (MMIO, 必须 volatile)。
unsafe fn rd(off: u64) -> u32 {
    core::ptr::read_volatile((BASE + off) as *const u32)
}

unsafe fn wr(off: u64, val: u32) {
    core::ptr::write_volatile((BASE + off) as *mut u32, val);
}

/// 使能 LAPIC, 返回本 CPU 的 APIC ID; 无 LAPIC (基址为 0) 返回 `None`。
///
/// 幂等: 可重复调用 (重复调用只读回当前状态并打印诊断)。
pub fn init() -> Option<u32> {
    unsafe {
        let mut msr = Msr::new(MSR_APIC_BASE);
        let val = msr.read();
        let base = val & 0x000F_FFFF_FFFF_F000;
        if base == 0 {
            crate::video::println("apic: no LAPIC base, MSI unavailable");
            return None;
        }
        if val & APIC_BASE_ENABLE == 0 {
            msr.write(val | APIC_BASE_ENABLE);
        }
        BASE = base;

        // 软使能 + 伪中断向量 (保留其余位)。
        let svr = rd(REG_SVR);
        wr(REG_SVR, (svr & !0xFF) | SVR_SW_ENABLE | SPURIOUS_VECTOR);
        // TPR = 0: 不按优先级屏蔽任何向量。
        wr(REG_TPR, 0);
        // LVT0: 保证 8259A 的 PIC 中断仍能透传 (见文件头说明)。
        let lvt0 = rd(REG_LVT0);
        if lvt0 & (LVT0_EXTINT | LVT_MASKED) != LVT0_EXTINT {
            wr(REG_LVT0, LVT0_EXTINT);
        }
        APIC_ID = rd(REG_ID) >> 24;

        // 启动诊断 (也是「MSI-X 前置条件已就绪」的证据)。
        crate::video::print("apic: enabled base=0x");
        crate::video::print_hex(BASE);
        crate::video::print(" id=");
        crate::video::print_u64(APIC_ID as u64);
        crate::video::print(" svr=0x");
        crate::video::print_hex(rd(REG_SVR) as u64);
        crate::video::print(" lvt0=0x");
        crate::video::print_hex(rd(REG_LVT0) as u64);
        crate::video::print(" msi_addr=0x");
        crate::video::print_hex(msi_address() as u64);
        crate::video::println("");
        Some(APIC_ID)
    }
}

/// MSI/MSI-X 中断消息地址 (物理目的模式, 目的地 = 本 CPU 的 APIC ID)。
pub fn msi_address() -> u32 {
    MSI_ADDR_BASE | (unsafe { APIC_ID } << 12)
}

/// 结束中断: 向 EOI 寄存器写 0。仅 MSI 向量处理器需要调用。
pub fn eoi() {
    unsafe { wr(REG_EOI, 0) }
}
