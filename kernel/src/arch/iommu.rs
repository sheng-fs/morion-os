//! Intel VT-d（IOMMU）DMA 重映射 —— 驱动路线 **E1b** + **E1c**。
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
//! 工作，同时每个功能点在 IOMMU 里都有明确一项，不留"表里没有就放行"的隐式口子。
//!
//! **E1c（受限 IOVA 窗口 + 越界取证）**把 DMA 权限**显式**绑成一条窗口
//! `IOVA ∈ [0, `[`TARGET_WINDOW_LIMIT`]`)`：窗口外一律**不建叶项**，于是设备发起的越界 DMA
//! 会被 IOMMU 拒绝。目标设备（NVMe —— 只有它真走查表，virtio 默认绕过 IOMMU）单列一张
//! **更小的**窗口表（3 GiB，其余设备仍是 4 GiB 恒等 → 行为零变化）；那张表就是 E2/E3 把
//! 飞地那台设备窗口继续收小的唯一落点，改它不会波及其它设备。
//!
//! 取证由用户态驱动 `block_srv` 触发（它故意提交一条 PRP 落在窗口外的 NVMe 读，设备就会去打
//! 那个地址），由空闲任务调用的 [`poll_faults`] 把 `FSTS`/`FECTL`/`FRCD` 打到内核日志 ——
//! 内核里没有别的"周期性钩子"，而越界 DMA 是启动阶段由用户态驱动发起的。
//!
//! ⚠️ 取证寄存器口径（易错）：`FEDATA`/`FEADDR`/`FEUADDR`（0x3C/0x40/0x44）是**故障事件
//! （MSI）的配置**，不含故障内容；SID / 原因 / 故障地址在 **FRCD**（`0xB0 + 16*i`）。另有两个
//! 本机 QEMU 的实测行为：① 判"有没有故障"要用 `FSTS != 0`（它置 `PPF` 而不置 `FRI`）；
//! ② 只有在故障中断**可投递**时才写 FRCD，我们没给 IOMMU 设备开 MSI，所以内核日志里
//! `FRCD` 为 0 —— 那一侧的证据取 QEMU 自己的
//! `vtd_iommu_translate: detected translation failure (dev=BB:DD:F, iova=0x...)`。
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
use core::sync::atomic::{AtomicU64, Ordering};

// ---------------------------------------------------------------------------
// 寄存器（VT-d spec 11.4，都是先 `+ base` 的偏移）
// ---------------------------------------------------------------------------

const REG_VER: u64 = 0x00;
const REG_CAP: u64 = 0x08;
const REG_ECAP: u64 = 0x10;
const REG_GCMD: u64 = 0x18;
const REG_GSTS: u64 = 0x1C;
const REG_RTADDR: u64 = 0x20;
/// 故障记录寄存器块（VT-d spec 11.4.6，E1c 取证用）。
///
/// ⚠️ `FEDATA`/`FEADDR`/`FEUADDR`（0x3C/0x40/0x44）是**故障事件（MSI）的配置**寄存器 ——
/// 软件写它们来告诉 IOMMU 故障中断投到哪，**不含故障内容**；故障的 SID / 原因 / 地址在
/// **FRCD**（`0xB0 + 16*i`，128 位：低 64 位 = Fault Info，高 64 位 = 故障地址）里。
const REG_FSTS: u64 = 0x34;
const REG_FECTL: u64 = 0x38;
/// 第 `i` 条故障记录（`i` 取自 [`fault_record_index`]）。
const REG_FRCD: u64 = 0xB0;
/// `FRCD` 项大小（128 位）。
const FRCD_BYTES: u64 = 16;

/// `FSTS.PPF`（bit 1）= 有未处理的故障 —— 本机 QEMU 实际置的就是这一位（FRI 位不动）。
///
/// 判"有没有故障"必须用 `FSTS != 0`；只盯 `FRI`（bit 0）会漏掉所有故障，[`poll_faults`]
/// 因此按非零判。
#[cfg(test)]
const FSTS_PPF: u32 = 1 << 1;

/// 已报告的故障条数上限 —— 故障风暴时不把日志刷爆。
const MAX_FAULT_REPORTS: u64 = 8;
/// 上一次报告过的故障"签名"（`FEDATA` + 故障地址低 32 位），用来对同一处故障去重。
static LAST_FAULT_SIG: AtomicU64 = AtomicU64::new(u64::MAX);

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

/// 覆盖窗口 `[0, limit)` 需要的 1 GiB 大页数（`limit` 必须是 1 GiB 的整数倍）。
fn window_1g_pages(limit: u64) -> u64 {
    limit >> 30
}

/// `iova` 是否落在允许窗口内。窗口外 = 二级页表里**没有叶项** → 设备发起的 DMA 被 IOMMU 拒绝。
fn window_permits(iova: u64, limit: u64) -> bool {
    iova < limit
}

/// `FRCD` Fault Info 高 16 位 = 发起故障的 **SID**（`bus << 8 | dev << 3 | func`）。
fn fault_sid(fedata: u32) -> u16 {
    (fedata >> 16) as u16
}

/// `FRCD` Fault Info bits 11:8 = 故障原因（VT-d spec 表 32；越界 DMA 是"叶子项不存在"一档）。
fn fault_reason(fedata: u32) -> u8 {
    ((fedata >> 8) & 0xF) as u8
}

/// `FRCD` 高 64 位 = 故障地址（低 32 位 + 高 32 位拼起来），低 12 位无效（规范只保证页对齐）。
fn fault_iova(feaddr: u32, feuaddr: u32) -> u64 {
    (((feuaddr as u64) << 32) | (feaddr as u64)) & PAGE_MASK
}

/// `FSTS` bits 15:8 = **故障记录索引**：故障信息在 `FRCD`（`0xB0 + 16 * index`）里。
fn fault_record_index(fsts: u32) -> u64 {
    ((fsts >> 8) & 0xFF) as u64
}

/// `FRCD` Fault Info 的 bit 0 = `F`（这条记录有效）。
fn fault_record_valid(info: u32) -> bool {
    info & 1 != 0
}

// ---------------------------------------------------------------------------
// E1c 状态（供空闲任务读取故障记录）
// ---------------------------------------------------------------------------

/// IOMMU 寄存器块物理地址（0 = 没有打开翻译，[`poll_faults`] 直接空转）。
static IOMMU_BASE: AtomicU64 = AtomicU64::new(0);
/// 目标设备（NVMe）的 SID —— 取证行里点名"是谁的 DMA 被拒"（QEMU 未写 FRCD 时的替代来源）。
static TARGET_SID: AtomicU64 = AtomicU64::new(0);
/// 已报告的故障条数（见 [`MAX_FAULT_REPORTS`]）。
static FAULT_REPORTS: AtomicU64 = AtomicU64::new(0);

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

/// 默认窗口（**其余设备**）：恒等 `[0, 4 GiB)` —— 与 E1b 完全相同，行为零变化。
const DEFAULT_WINDOW_LIMIT: u64 = 4 * (1 << 30);

/// **E1c 目标设备（NVMe）的受限 IOVA 窗口**：只映射 `IOVA ∈ [0, TARGET_WINDOW_LIMIT)`，
/// 窗口外不建叶项 —— 这是 E1c 隔离能力的全部依据。
///
/// 取 **3 GiB** 的两个理由（都是实测出来的）：
/// 1. 必须覆盖设备**所有合法 DMA 落点**：内核与既有驱动按物理地址 DMA，RAM 里的页恒在
///    3 GiB 以下（回归用 `-m 2G`），所以窗口收到 3 GiB 恰好覆盖它们而不再多给。
/// 2. 必须**严格小于 4 GiB**：本机 QEMU 的 `intel-iommu` 只翻译 < 4 GiB 的 IOVA，等于 4 GiB
///    的 PRP 会直接落到系统地址空间（实测既无 `vtd_iommu_translate` 日志、也不产生 FSTS
///    故障记录）—— 也就是说"窗口外"的取证地址只能取在 4 GiB 以下。
///
/// ⚠️ 用户态驱动 `block_srv` 的越界探针硬编码了同一个数字（它够不到内核常量），改这里要同步改。
pub const TARGET_WINDOW_LIMIT: u64 = 3 * (1 << 30);

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

    // 1. 二级页表：**E1c 的受限窗口**（恒等，故 IOVA == 物理地址）。
    //    其余设备用默认窗口（4 GiB，行为零变化）。
    let Some(default_top) = build_window_slpt(aw, DEFAULT_WINDOW_LIMIT) else {
        crate::video::println("[WARN] VT-d: cannot allocate second-level page tables");
        return false;
    };
    // 目标设备（NVMe）单列一张**更小的**窗口表：收窄它的 DMA 权限不会波及其它设备，
    // 也是 E2/E3 把飞地那台设备窗口继续收小的唯一落点。
    let target = crate::arch::pci::find_nvme(devices).map(|(b, d, f, _)| (b, d, f));
    if let Some((b, d, f)) = target {
        TARGET_SID.store(
            ((b as u64) << 8) | ((d as u64) << 3) | f as u64,
            Ordering::Relaxed,
        );
    }
    let target_top = if target.is_some() {
        let Some(top) = build_window_slpt(aw, TARGET_WINDOW_LIMIT) else {
            crate::video::println("[WARN] VT-d: cannot allocate target second-level page tables");
            return false;
        };
        Some(top)
    } else {
        None
    };

    // 2. 根表 + 每总线一张上下文表；每个功能点一条 translated + 窗口内恒等上下文项。
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
        let is_target = target == Some((d.bus, d.dev, d.func));
        let top = if is_target {
            target_top.unwrap_or(default_top)
        } else {
            default_top
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
    // E1c: 记录寄存器块地址 —— 空闲任务据此读故障记录 (FSTS/FEDATA/FEADDR)。
    IOMMU_BASE.store(base, Ordering::Release);
    // E1c: **打开故障记录**。QEMU 在故障中断被屏蔽时只置 `FSTS.PPF` 而**不写**
    // `FRCD`/`FEDATA`/`FEADDR`，取证就拿不到 SID/原因/地址；把 `FECTL.IM` 清掉即可。
    // 故障事件 MSI 我们没配置（`FEADDR`/`FEDATA` 为 0），中断不会被真正投递
    // （QEMU 打印 "Interrupt Mask set, irq is not generated" 后丢弃）—— 我们只读寄存器，
    // 不需要那条中断。
    unsafe { wr32(base, REG_FECTL, 0) };
    crate::video::print("[OK] VT-d: remap ON root=0x");
    crate::video::print_hex(root_pa);
    crate::video::print(" ctx_buses=");
    crate::video::print_u64(ctx_buses.len() as u64);
    crate::video::print(" translated=");
    crate::video::print_u64(translated);
    crate::video::print(" (iova=[0,0x");
    crate::video::print_hex(DEFAULT_WINDOW_LIMIT);
    crate::video::print("), aw=");
    crate::video::print_u64(aw);
    crate::video::print(") target=");
    match target {
        Some((b, d, f)) => {
            crate::video::print_hex(((b as u64) << 8) | ((d as u64) << 3) | f as u64);
            crate::video::print("/win=0x");
            crate::video::print_hex(TARGET_WINDOW_LIMIT);
        }
        None => crate::video::print("none"),
    }
    crate::video::print(" gsts=0x");
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

/// 建覆盖窗口 `[0, limit)` 的**恒等**二级页表，返回顶层表物理地址。
///
/// `aw = 39` 时顶层就是 1 GiB 大页级（1 张表）；`aw = 48` 时多一级（PML4 → PML3，2 张表）。
/// 窗口外的 IOVA 不建叶项 —— 这正是 E1c「越界 DMA 被拒」的机制。
fn build_window_slpt(aw: u64, limit: u64) -> Option<u64> {
    let top = alloc_zeroed_page()?;
    if aw == 48 {
        // PML4[0] → PML3（1 GiB 大页级）。
        let pml3 = alloc_zeroed_page()?;
        write_entry(top, 0, slpt_table_entry(pml3), 0);
        fill_1g_ident(pml3, limit);
    } else {
        fill_1g_ident(top, limit);
    }
    Some(top)
}

/// 在 1 GiB 大页级表里铺满窗口 `[0, limit)` 的恒等大页（PA == IOVA）。
///
/// 每一项都过一遍 [`window_permits`] —— 窗口外**绝不能**建叶项，这是 E1c 隔离能力的全部依据。
fn fill_1g_ident(table_pa: u64, limit: u64) {
    let pages = window_1g_pages(limit);
    let mut i = 0u64;
    while i < pages {
        let iova = i << 30;
        assert!(
            window_permits(iova, limit),
            "VT-d: identity page outside the IOVA window"
        );
        write_entry(table_pa, i, slpt_identity_page(iova), 0);
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

// ---------------------------------------------------------------------------
// E1c 取证: 越界 DMA 的故障记录
// ---------------------------------------------------------------------------

/// 读一次（并清）IOMMU 故障记录寄存器，有故障就打印一行取证。
///
/// **为什么由空闲任务调用**：越界 DMA 是**设备发起**的，触发它的是用户态驱动 `block_srv`
/// 的启动自测；而内核里没有任何周期性钩子（`init` 只在启动 Stage 4.7 跑一次）。空闲任务
/// 是最廉价、也总会被调度到的观察点。
///
/// 幂等且极廉价：没打开翻译（[`IOMMU_BASE`] == 0）或已达 [`MAX_FAULT_REPORTS`] 时只读一个
/// 原子量就返回。读到的 `FSTS` 非零即算"有故障"——本机 QEMU 只置 `PPF`（bit 1）而**不置**
/// `FRI`（bit 0），按位 0 判会漏掉所有故障。打印后把读到的位按 W1C 写回，让后续故障仍被记录。
pub fn poll_faults() {
    let base = IOMMU_BASE.load(Ordering::Relaxed);
    if base == 0 || FAULT_REPORTS.load(Ordering::Relaxed) >= MAX_FAULT_REPORTS {
        return;
    }
    let fsts = unsafe { rd32(base, REG_FSTS) };
    if fsts == 0 {
        return;
    }
    let fectl = unsafe { rd32(base, REG_FECTL) };

    // 故障内容在 FRCD：索引由 FSTS bits 15:8 给出；该条无效（F 位为 0）时退回第 0 条 ——
    // QEMU 实际只维护一条记录。
    let mut idx = fault_record_index(fsts);
    let mut at = REG_FRCD + idx * FRCD_BYTES;
    let mut info = unsafe { rd32(base, at) };
    if !fault_record_valid(info) && idx != 0 {
        idx = 0;
        at = REG_FRCD;
        info = unsafe { rd32(base, at) };
    }
    let addr_lo = unsafe { rd32(base, at + 8) };
    let addr_hi = unsafe { rd32(base, at + 12) };
    let iova = fault_iova(addr_lo, addr_hi);

    // 同一处故障只打印一次：`FSTS.PPF` 是"有未处理故障"的粘滞位，实测 W1C 清不掉，
    // 不按签名去重就会把同一行刷满日志。
    let sig = ((info as u64) << 32) | (iova & 0xFFFF_FFFF);
    if sig != LAST_FAULT_SIG.load(Ordering::Relaxed) {
        LAST_FAULT_SIG.store(sig, Ordering::Relaxed);
        crate::video::print("[OK] VT-d: DMA refused target-sid=0x");
        crate::video::print_hex(TARGET_SID.load(Ordering::Relaxed));
        crate::video::print(" fsts=0x");
        crate::video::print_hex(fsts as u64);
        crate::video::print(" fectl=0x");
        crate::video::print_hex(fectl as u64);
        crate::video::print(" frcd[");
        crate::video::print_u64(idx);
        crate::video::print("]=0x");
        crate::video::print_hex(info as u64);
        if fault_record_valid(info) {
            crate::video::print(" frcd-sid=0x");
            crate::video::print_hex(fault_sid(info) as u64);
            crate::video::print(" reason=0x");
            crate::video::print_hex(fault_reason(info) as u64);
            crate::video::print(" iova=0x");
            crate::video::print_hex(iova);
        } else {
            // QEMU 只在故障中断**可投递**时才写 FRCD；我们没给 IOMMU 设备开 MSI，故这里为 0。
            // 设备与地址的证据在 QEMU 自己的 stderr 行里
            // (`vtd_iommu_translate: detected translation failure (dev=0:2:0, iova=0x...)`)。
            crate::video::print(" (FRCD 未写: 故障中断未投递)");
        }
        crate::video::println("");
        FAULT_REPORTS.fetch_add(1, Ordering::Relaxed);
    }

    // W1C 清掉读到的状态位: 不清的话后续故障会一直"未记录"(compression)。
    unsafe { wr32(base, REG_FSTS, fsts) };
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

    /// E1c: 窗口边界 —— 窗口内放行、窗口外（第一个越界地址）拒绝。
    ///
    /// `block_srv` 的越界探针正是打在 `TARGET_WINDOW_LIMIT` 上，所以这条断言就是「探针为什么
    /// 会被拒」的静态依据。
    #[test]
    fn window_boundary_and_page_count() {
        assert!(window_permits(0, TARGET_WINDOW_LIMIT));
        assert!(window_permits(TARGET_WINDOW_LIMIT - 1, TARGET_WINDOW_LIMIT));
        assert!(!window_permits(TARGET_WINDOW_LIMIT, TARGET_WINDOW_LIMIT));

        // 1 GiB 大页铺满窗口：3 GiB → 3 页；默认窗口 4 GiB → 4 页。
        assert_eq!(window_1g_pages(TARGET_WINDOW_LIMIT), 3);
        assert_eq!(window_1g_pages(DEFAULT_WINDOW_LIMIT), 4);
        // 顶层索引 = IOVA >> 30：窗口外第一个地址的索引必须 ≥ 页数（无叶项）。
        assert!(window_1g_pages(TARGET_WINDOW_LIMIT) <= TARGET_WINDOW_LIMIT >> 30);

        // 目标窗口必须**严格小于** 4 GiB：本机 QEMU 的 intel-iommu 不翻译 ≥ 4 GiB 的 IOVA，
        // 取 4 GiB 会让"窗口外"的取证地址落进 QEMU 的旁路区间，什么都抓不到。
        assert!(TARGET_WINDOW_LIMIT < DEFAULT_WINDOW_LIMIT);
        assert!(TARGET_WINDOW_LIMIT < 1 << 32);
    }

    /// E1c: 故障记录的字段解码（FEDATA = SID + 原因，FEADDR/FEUADDR = 故障地址）。
    #[test]
    fn fault_record_decoding() {
        // SID 0x00F0 = bus 0 / dev 0x1E / func 0；原因 0x1 = 叶子项不存在。
        let fedata = (0x00F0u32 << 16) | (0x1 << 8);
        assert_eq!(fault_sid(fedata), 0x00F0);
        assert_eq!(fault_reason(fedata), 0x1);

        // 故障地址 = FEUADDR << 32 | FEADDR，且低 12 位被清（规范只保证页对齐）。
        assert_eq!(fault_iova(0, 1), 1 << 32);
        assert_eq!(fault_iova(0x0000_1234, 0), 0x1000);
        assert_eq!(fault_iova(0xFFFF_F000, 0x0000_0002), 0x0000_0002_FFFF_F000);

        // 证据判据：本机 QEMU 置 PPF 而不置 FRI，故必须按 `FSTS != 0` 判"有故障"。
        assert_ne!(FSTS_PPF, 0);
        assert_eq!(FSTS_PPF & 0b1, 0);
    }
}
