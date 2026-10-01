//! Intel VT-d（IOMMU）DMA 重映射 —— 驱动路线 **E1b**。
//!
//! E1a 只**探测**（[`crate::arch::acpi::probe_dmar`]）。E1b 把 IOMMU 真正**打开**：
//!
//! 1. 按 `DRHD.reg_base` 访问 IOMMU 寄存器块（QEMU 为 `0xFED90000`，在**恒等映射**内 →
//!    可直接以物理地址当虚拟地址按 volatile 访问，与 [`crate::arch::apic`] 访问 LAPIC 同一套路）；
//! 2. 建**根表**（每总线一项）+ **上下文表**（每总线一张，每个 PCI 功能点一项）；
//! 3. 每个枚举到的 PCI 功能点都挂 **translated** 上下文，二级页表铺**恒等**映射
//!    （IOVA == 物理地址，覆盖前 4 GiB）；
//! 4. `GCMD.SRTP` 置根表指针 → `GCMD.TE` 打开翻译，回读 `GSTS` 确认。
//!
//! **为什么全部 translated + 恒等**：`TE = 1` 之后**所有**设备的 DMA 都要过 IOMMU 查表，而
//! 阶段一内核与既有驱动仍按**物理地址**做 DMA —— 只要 IOVA 仍等于物理地址（恒等）它们就照常
//! 工作，同时每个功能点在 IOMMU 里都有明确一项，不留"表里没有就放行"的隐式口子。E1c 再把飞地
//! 那台设备的二级页表换成**受限窗口**，用"越界 DMA 被拒"取证隔离能力。
//!
//! **为什么不用 pass-through**：`TT = 0b10` 才是 pass-through，本步不需要 —— 每个功能点都用
//! translated + 恒等就足够让既有驱动照常工作，还顺手把"该设备能 DMA 到哪"变成 IOMMU 里一条
//! 可改的项（E1c 受限窗口的前提）。
//!
//! ⚠️ `TT` 的取值是**易错点**（bits 3:2，Linux/QEMU 口径 `0b00` = translated / `0b01` =
//! Device TLB / `0b10` = pass-through）：写错成 `0b01` 时 QEMU 会报
//! `vtd_ce_type_check: DT specified but not supported`，并把该设备的所有 DMA 判成故障。
//!
//! ⚠️ 本步**只在固件存在 IOMMU 时生效**（QEMU 需 `-device intel-iommu`）；无 IOMMU 时直接
//! 返回 `false`、行为与之前完全一致（回归因此不受影响）。

use crate::arch::pci::PciDevice;
use crate::memory::frame_allocator::allocate_frame;
use crate::memory::paging::is_identity_mapped;
use alloc::vec::Vec;

// ---------------------------------------------------------------------------
// 寄存器（VT-d spec 11.4，都是先 `+ base` 的偏移）
// ---------------------------------------------------------------------------

const REG_VER: u64 = 0x00;
const REG_CAP: u64 = 0x08;
const REG_ECAP: u64 = 0x10;
const REG_GCMD: u64 = 0x18;
const REG_GSTS: u64 = 0x1C;
const REG_RTADDR: u64 = 0x20;

/// `GCMD`：置根表指针 / 打开翻译。
const GCMD_SRTP: u32 = 1 << 30;
const GCMD_TE: u32 = 1 << 31;
/// `GSTS`：上面两者的"已生效"回读位。
const GSTS_RTPS: u32 = 1 << 30;
const GSTS_TES: u32 = 1 << 31;

/// `CAP.SAGAW`（bits 12:8）各地址宽度支持位。
const CAP_SAGAW_30: u64 = 1 << 8;
const CAP_SAGAW_39: u64 = 1 << 9;
const CAP_SAGAW_48: u64 = 1 << 10;

// ---------------------------------------------------------------------------
// 表结构
// ---------------------------------------------------------------------------

const TABLE_BYTES: u64 = 4096;
const PAGE_MASK: u64 = !0xFFF;
/// 根表 / 上下文表的项大小（都是 128 位 = 16 字节）。
const ENTRY_BYTES: u64 = 16;

/// 二级页表条目位（VT-d spec 表 30）。
const SLPT_R: u64 = 1 << 0;
const SLPT_W: u64 = 1 << 1;
/// 大页（1 GiB / 2 MiB 级）标志。
const SLPT_PS: u64 = 1 << 7;

/// 上下文项 `Translation Type`（bits 3:2）：**0b00 = translated**（走 SLPTPTR 指向的
/// 二级页表）。按 Linux `CONTEXT_TT_MULTI_LEVEL` / QEMU `VTD_CONTEXT_TT_MULTI_LEVEL` 的口径，
/// 另两个取值是 `0b01` = Device TLB（`CONTEXT_TT_DEV_IOTLB`）、`0b10` = pass-through。
const CTX_TT_TRANSLATED: u64 = 0b00;

/// 根表项：present + 上下文表物理地址。
fn root_entry(ctx_pa: u64) -> (u64, u64) {
    ((ctx_pa & PAGE_MASK) | 1, 0)
}

/// 上下文表项：`tt` + 地址宽度**索引** `aw`（见 [`agaw_index`]）+ 二级页表物理地址。
///
/// ⚠️ 地址宽度在**高位 64 位的 bits 2:0**（Linux `context_address_width(c) = (c).hi & 7`、
/// QEMU `vtd_ce_get_agaw(ce) = 30 + (ce->hi & 7) * 9`），且值是**索引**而不是位数本身：
/// 0 = 30 位 / 1 = 39 位 / 2 = 48 位 / 3 = 57 位；QEMU 会核对它是否为 `CAP.SAGAW` 支持的宽度。
fn context_entry(tt: u64, aw: u64, slpt_pa: u64) -> (u64, u64) {
    let lo = 1u64                        // present
        | ((tt & 0b11) << 2)             // bits 3:2 Translation Type
        | (slpt_pa & PAGE_MASK); // bits 63:12 Second-Level Page Table
    let hi = aw & 0b111; // 高位 64 位 bits 2:0 = Address Width 索引
    (lo, hi)
}

/// 把 AGAW（30 / 39 / 48 / 57）换算成上下文项里的**地址宽度索引** `(AGAW - 30) / 9`。
fn agaw_index(agaw: u64) -> u64 {
    (agaw - 30) / 9
}

/// 二级页表**非叶**项：present，指向下一级表。
fn slpt_table_entry(next_pa: u64) -> u64 {
    (next_pa & PAGE_MASK) | SLPT_R | SLPT_W
}

/// 二级页表**大页叶**项（恒等映射，故条目地址就是 IOVA）。
fn slpt_identity_page(pa: u64) -> u64 {
    (pa & PAGE_MASK) | SLPT_R | SLPT_W | SLPT_PS
}

/// 覆盖前 4 GiB 需要的 1 GiB 大页数。
const IDENTITY_1G_PAGES: u64 = 4;

// ---------------------------------------------------------------------------
// MMIO / 物理页读写
// ---------------------------------------------------------------------------

unsafe fn rd32(base: u64, off: u64) -> u32 {
    core::ptr::read_volatile((base + off) as *const u32)
}
unsafe fn wr32(base: u64, off: u64, v: u32) {
    core::ptr::write_volatile((base + off) as *mut u32, v);
}
unsafe fn rd64(base: u64, off: u64) -> u64 {
    core::ptr::read_volatile((base + off) as *const u64)
}
unsafe fn wr64(base: u64, off: u64, v: u64) {
    core::ptr::write_volatile((base + off) as *mut u64, v);
}

/// 申请一页并清零（表必须全 0 起步 —— 0 表示"项不存在"）。
fn alloc_zeroed_page() -> Option<u64> {
    let pa = allocate_frame()?;
    let mut off = 0u64;
    while off < TABLE_BYTES {
        // SAFETY: `allocate_frame` 返回恒等映射内的帧，按物理地址即可写。
        unsafe { core::ptr::write_volatile((pa + off) as *mut u8, 0) };
        off += 1;
    }
    Some(pa)
}

/// 把 `v` 写进恒等映射内物理地址 `pa` 处的一个 `u64`（表结构用；不是 MMIO 寄存器）。
fn write_pa64(pa: u64, v: u64) {
    // SAFETY: 调用方保证 `pa` 落在已分配的恒等映射表页内。
    unsafe { core::ptr::write_volatile(pa as *mut u64, v) };
}

/// 写一条 128 位表项（`idx` 是项下标）。
fn write_entry(table_pa: u64, idx: u64, lo: u64, hi: u64) {
    let at = table_pa + idx * ENTRY_BYTES;
    write_pa64(at, lo);
    write_pa64(at + 8, hi);
}

/// 上下文表下标：`dev << 3 | func`。
fn devfn(dev: u8, func: u8) -> u64 {
    ((dev as u64) << 3) | ((func as u64) & 0b111)
}

// ---------------------------------------------------------------------------
// 初始化
// ---------------------------------------------------------------------------

/// 打开 IOMMU 的 DMA 重映射（E1b）。
///
/// `devices` 是 PCI 枚举结果 —— 每个功能点都会拿到一条 **translated + 恒等**的上下文项
/// （IOVA == 物理地址），阶段一内核与既有驱动照旧按物理地址 DMA 即可。
///
/// 返回是否真的把翻译打开（无 IOMMU / 表建不起来 / `GSTS` 未生效 → `false`）。
pub fn init(devices: &[PciDevice]) -> bool {
    let dmar = crate::arch::acpi::probe_dmar();
    if !dmar.found || dmar.drhd_count == 0 {
        crate::video::println("[OK] VT-d: no IOMMU in firmware, DMA remapping not enabled");
        return false;
    }
    let base = dmar.first_drhd.reg_base;
    if !is_identity_mapped(base, TABLE_BYTES) {
        crate::video::println("[WARN] VT-d: IOMMU register block outside identity map, skipping");
        return false;
    }

    let ver = unsafe { rd32(base, REG_VER) };
    let cap = unsafe { rd64(base, REG_CAP) };
    let ecap = unsafe { rd64(base, REG_ECAP) };
    crate::video::print("[OK] VT-d: IOMMU reg=0x");
    crate::video::print_hex(base);
    crate::video::print(" ver=");
    crate::video::print_u64((ver >> 4) as u64 & 0xF);
    crate::video::print(".");
    crate::video::print_u64((ver & 0xF) as u64);
    crate::video::print(" sagaw=0x");
    crate::video::print_hex((cap >> 8) & 0x1F);
    crate::video::print(" ecap=0x");
    crate::video::print_hex(ecap);
    crate::video::println("");

    // 选地址宽度：优先 39 位（顶层直接是 1 GiB 大页，恒等 4 GiB 只需 1 张表）。
    let aw: u64 = if cap & CAP_SAGAW_39 != 0 {
        39
    } else if cap & CAP_SAGAW_48 != 0 {
        48
    } else if cap & CAP_SAGAW_30 != 0 {
        30
    } else {
        crate::video::println("[WARN] VT-d: no supported SAGAW, skipping");
        return false;
    };
    // 上下文项里的地址宽度是**索引**（0=30/1=39/2=48/3=57 位）。
    let aw_idx = agaw_index(aw);

    // 1. 二级页表：恒等映射前 4 GiB。
    let Some(top) = build_identity_slpt(aw) else {
        crate::video::println("[WARN] VT-d: cannot allocate second-level page tables");
        return false;
    };

    // 2. 根表 + 每总线一张上下文表；每个功能点一条 translated + 恒等上下文项。
    let Some(root_pa) = alloc_zeroed_page() else {
        crate::video::println("[WARN] VT-d: cannot allocate root table");
        return false;
    };
    let mut ctx_buses: Vec<(u8, u64)> = Vec::new();
    let mut translated = 0u64;
    for d in devices {
        let Some(ctx_pa) = ctx_table_for(&mut ctx_buses, d.bus) else {
            crate::video::println("[WARN] VT-d: cannot allocate context table");
            return false;
        };
        let (lo, hi) = context_entry(CTX_TT_TRANSLATED, aw_idx, top);
        write_entry(ctx_pa, devfn(d.dev, d.func), lo, hi);
        translated += 1;
    }

    // 3. 根表挂上各总线的上下文表。
    for (bus, ctx_pa) in &ctx_buses {
        let (lo, hi) = root_entry(*ctx_pa);
        write_entry(root_pa, *bus as u64, lo, hi);
    }

    // 4. 写根表指针 → 打开翻译。表必须先对 IOMMU 可见（x86 写序 + 编译器栅栏足够，
    //    memory-typed 结构由 IOMMU 直接读内存）。
    core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
    unsafe {
        wr64(base, REG_RTADDR, root_pa);
    }
    set_gcmd(base, GCMD_SRTP);
    if !wait_gsts(base, GSTS_RTPS) {
        crate::video::println("[WARN] VT-d: root table pointer did not take effect");
        return false;
    }
    set_gcmd(base, GCMD_TE);
    if !wait_gsts(base, GSTS_TES) {
        crate::video::print("[WARN] VT-d: translation enable did not take effect (GSTS=0x");
        crate::video::print_hex(unsafe { rd32(base, REG_GSTS) } as u64);
        crate::video::println(")");
        return false;
    }

    let gsts = unsafe { rd32(base, REG_GSTS) };
    crate::video::print("[OK] VT-d: remap ON root=0x");
    crate::video::print_hex(root_pa);
    crate::video::print(" ctx_buses=");
    crate::video::print_u64(ctx_buses.len() as u64);
    crate::video::print(" translated=");
    crate::video::print_u64(translated);
    crate::video::print(" (iova=identity 4GiB, aw=");
    crate::video::print_u64(aw);
    crate::video::print(") gsts=0x");
    crate::video::print_hex(gsts as u64);
    crate::video::println("");
    true
}

/// 取（或新建）`bus` 的上下文表物理地址。
fn ctx_table_for(buses: &mut Vec<(u8, u64)>, bus: u8) -> Option<u64> {
    if let Some((_, pa)) = buses.iter().find(|(b, _)| *b == bus) {
        return Some(*pa);
    }
    let pa = alloc_zeroed_page()?;
    buses.push((bus, pa));
    Some(pa)
}

/// 建覆盖前 4 GiB 的**恒等**二级页表，返回顶层表物理地址。
///
/// `aw = 39` 时顶层就是 1 GiB 大页级（1 张表）；`aw = 48` 时多一级（PML4 → PML3，2 张表）。
fn build_identity_slpt(aw: u64) -> Option<u64> {
    let top = alloc_zeroed_page()?;
    if aw == 48 {
        // PML4[0] → PML3（1 GiB 大页级）。
        let pml3 = alloc_zeroed_page()?;
        write_entry(top, 0, slpt_table_entry(pml3), 0);
        fill_1g_ident(pml3);
    } else {
        fill_1g_ident(top);
    }
    Some(top)
}

/// 在 1 GiB 大页级表里铺 `IDENTITY_1G_PAGES` 个恒等大页（PA == IOVA）。
fn fill_1g_ident(table_pa: u64) {
    let mut i = 0u64;
    while i < IDENTITY_1G_PAGES {
        write_entry(table_pa, i, slpt_identity_page(i << 30), 0);
        i += 1;
    }
}

/// `GCMD` 置位（读-改-写）。
fn set_gcmd(base: u64, bits: u32) {
    unsafe {
        let cur = rd32(base, REG_GCMD);
        wr32(base, REG_GCMD, cur | bits);
    }
}

/// 轮询 `GSTS` 直到 `bits` 全部置位（超时返回 `false`）。
fn wait_gsts(base: u64, bits: u32) -> bool {
    let mut spins = 0u32;
    // SAFETY: 寄存器块已确认在恒等映射内。
    while unsafe { rd32(base, REG_GSTS) } & bits != bits {
        spins += 1;
        if spins > 1_000_000 {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_and_context_entry_fields() {
        // 根表项：present + 上下文表地址（低 12 位被清）。
        let (lo, hi) = root_entry(0x1234_5000);
        assert_eq!(lo & 1, 1);
        assert_eq!(lo & PAGE_MASK, 0x1234_5000);
        assert_eq!(hi, 0);

        // 上下文项：translated + AW 索引 1（AGAW 39）+ 二级页表。
        // AW 在**高位** 64 位的 bits 2:0，低位 64 位不放地址宽度。
        let (lo, hi) = context_entry(CTX_TT_TRANSLATED, agaw_index(39), 0xABCD_E000);
        assert_eq!(lo & 1, 1);
        assert_eq!((lo >> 2) & 0b11, CTX_TT_TRANSLATED);
        assert_eq!(lo & PAGE_MASK, 0xABCD_E000);
        assert_eq!(hi & 0b111, 1);
        // 低位 64 位的 bits 11:4 必须没有地址宽度的残留。
        assert_eq!((lo >> 4) & 0xFF, 0);

        // TT 取值（Linux/QEMU 口径）：0b00 = translated（二级页表），0b01 = Device TLB，
        // 0b10 = pass-through。本实现只用 translated —— 写错会让 QEMU 把所有 DMA 判故障。
        assert_eq!(CTX_TT_TRANSLATED, 0b00);

        // AGAW → 索引：30/39/48/57 → 0/1/2/3。
        assert_eq!(agaw_index(30), 0);
        assert_eq!(agaw_index(39), 1);
        assert_eq!(agaw_index(48), 2);
        assert_eq!(agaw_index(57), 3);
    }

    #[test]
    fn identity_page_and_table_entries() {
        // 恒等大页：present + W + PS，地址即 IOVA。
        let e = slpt_identity_page(0x4000_0000);
        assert_eq!(e & SLPT_R, SLPT_R);
        assert_eq!(e & SLPT_W, SLPT_W);
        assert_eq!(e & SLPT_PS, SLPT_PS);
        assert_eq!(e & PAGE_MASK, 0x4000_0000);

        // 非叶项：present，**没有** PS（否则会被当成叶）。
        let e = slpt_table_entry(0x2000);
        assert_eq!(e & 1, 1);
        assert_eq!(e & SLPT_PS, 0);
        assert_eq!(e & PAGE_MASK, 0x2000);
    }

    #[test]
    fn devfn_encoding() {
        assert_eq!(devfn(0, 0), 0);
        assert_eq!(devfn(1, 0), 8);
        assert_eq!(devfn(0, 3), 3);
        assert_eq!(devfn(4, 1), 33); // 0x21 = dev 4, func 1
    }
}
