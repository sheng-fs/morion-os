//! Intel VT-d（IOMMU）DMA 重映射 —— 驱动路线 **E1b** + **E1c**。
//!
//! E1a 只**探测**（[`crate::arch::acpi::probe_dmar`]）。E1b 把 IOMMU 真正**打开**：
//!
//! 1. 按 `DRHD.reg_base` 访问 IOMMU 寄存器块（QEMU 为 `0xFED90000`，在**恒等映射**内 →
//!    可直接以物理地址当虚拟地址按 volatile 访问，与 [`crate::arch::apic`] 访问 LAPIC 同一套路）；
//! 2. 建**根表**（每总线一项）+ **上下文表**（每总线一张，每个 PCI 功能点一项）；
//! 3. 每个枚举到的 PCI 功能点都挂 **translated** 上下文；**默认窗口**不是恒等 4 GiB，而是
//!    **每台已授权设备各自一张**由 [`register_device_range`] 登记表构造的稀疏窗口 —— 只有
//!    `device::grant*` 授权的 BAR / DMA 物理范围建 4 KiB 恒等叶项（IOVA == 物理地址）；
//! 4. `GCMD.SRTP` 置根表指针 → `GCMD.TE` 打开翻译，回读 `GSTS` 确认。
//!
//! **为什么默认窗口只覆盖登记范围**：`TE = 1` 之后所有设备的 DMA 都要过 IOMMU 查表。若
//! 像早期实现那样给每个功能点铺 `[0, 4 GiB)` 恒等映射，被攻破的驱动就能 DMA 改写内核、
//! 页表或其它域的用户页。现在每台设备**开局是一张空表**，授权时才逐段补入自己的 BAR /
//! DMA 块；未登记页一律没有叶项，越界 DMA（含打向别的设备）被硬件拒绝。设备仍按物理地址
//! DMA（IOVA == PA）。
//!
//! **E1c（受限 IOVA 窗口 + 越界取证）**把 DMA 权限**显式**绑成一条窗口
//! `IOVA ∈ [0, `[`TARGET_WINDOW_LIMIT`]`)`：窗口外一律**不建叶项**，于是设备发起的越界 DMA
//! 会被 IOMMU 拒绝。目标设备（NVMe —— 只有它真走查表，virtio 默认绕过 IOMMU）单列一张
//! **更小的**窗口表（3 GiB；其余设备走上面按登记表构造的默认窗口），那张表就是 E2/E3 把
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
use alloc::vec;
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

/// `GCMD.CFI`（bit 23）= 请求**上下文缓存失效**；`GSTS.CFIS`（同一位）= 失效仍在进行。
///
/// 运行期改写上下文表项后必须做一次，否则 IOMMU 可能继续用旧的上下文项
/// （Linux `iommu_flush_context_global` 的口径：置 `CFI`，轮询 `GSTS.CFIS == 0`）。
const GCMD_CFI: u32 = 1 << 23;
const GSTS_CFIS: u32 = 1 << 23;

/// `GCMD.IVA`（bit 25）= 请求**全局 IOTLB 失效**；`GSTS.IVT`（同一位）= 失效仍在进行。
///
/// 运行期把新的登记范围补进二级页表后必须做一次，否则 IOMMU 可能继续用旧的翻译
/// （本批只做全局失效，没有按页/`PSI` 精确失效 —— 见报告里的剩余缺口）。
const GCMD_IVA: u32 = 1 << 25;
const GSTS_IVT: u32 = 1 << 25;

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

// ---------------------------------------------------------------------------
// 登记表（默认窗口 = 内核实际授权给设备的物理范围）
// ---------------------------------------------------------------------------

/// 一段**4 KiB 页对齐**的物理范围（`[base, base + len)`）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct PhysRange {
    base: u64,
    len: u64,
}

impl PhysRange {
    fn end(&self) -> u64 {
        self.base + self.len
    }
}

/// 把 `v` 向上对齐到 4 KiB；溢出返回 `None`。
fn page_align_up(v: u64) -> Option<u64> {
    let low = v & !PAGE_MASK;
    if low == 0 {
        Some(v)
    } else {
        v.checked_add(TABLE_BYTES - low)
    }
}

/// 把 `[paddr, paddr + bytes)` 规范成页对齐范围（向下对齐起点、向上对齐终点）。
///
/// 空区间 / 加法溢出返回 `None` —— 越界或零长度**绝不**登记进窗口。
fn align_range(paddr: u64, bytes: u64) -> Option<PhysRange> {
    if bytes == 0 {
        return None;
    }
    let base = paddr & PAGE_MASK;
    let end = page_align_up(paddr.checked_add(bytes)?)?;
    if end <= base {
        return None;
    }
    Some(PhysRange {
        base,
        len: end - base,
    })
}

/// 合并重叠 / 相邻的范围（纯函数）。返回按基址排序、互不相交的区间列表。
fn merge_ranges(ranges: &[PhysRange]) -> Vec<PhysRange> {
    let mut sorted: Vec<PhysRange> = ranges.iter().copied().filter(|r| r.len != 0).collect();
    sorted.sort_unstable_by_key(|r| r.base);
    let mut out: Vec<PhysRange> = Vec::new();
    for r in sorted {
        if let Some(last) = out.last_mut() {
            let last_end = last.end();
            if r.base <= last_end {
                let end = last_end.max(r.end());
                last.len = end - last.base;
                continue;
            }
        }
        out.push(r);
    }
    out
}

/// 范围是否完整落在该 AGAW 能表示的地址空间内（越界拒绝）。
fn range_within_agaw(range: PhysRange, aw: u64) -> bool {
    aw >= 12 && range.end() <= (1u64 << aw)
}

/// 失败补登记时一次补入的窗口（64 KiB，按该大小向下对齐故障地址）。
const HEAL_BYTES: u64 = 64 * 1024;
/// 补登记次数上限：超过后回退恒等窗口（宁可退化也不让驱动永久 DMA 失败）。
const MAX_WINDOW_HEALS: u64 = 8;

/// 把故障地址扩成一个 64 KiB 对齐的补登记范围。
fn heal_range(iova: u64) -> Option<PhysRange> {
    let base = iova & !(HEAL_BYTES - 1);
    align_range(base, HEAL_BYTES)
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

/// 翻译打开时选中的 **AGAW**（0 = 翻译没打开 → per-domain 窗口不可用）。
static ENABLED_AW: AtomicU64 = AtomicU64::new(0);
/// 根表物理地址（`init` 成功后填入；per-domain 上下文挂接据此找/建总线上下文表）。
static ROOT_PA: AtomicU64 = AtomicU64::new(0);
/// **默认窗口**（每台设备一张，开局为空）的共享空表 —— 未授权设备只能落到这里（无叶项）。
static DEFAULT_TOP: AtomicU64 = AtomicU64::new(0);
/// 每台已授权设备的窗口：`top_pa` 是该设备**独有**的二级页表，`ranges` 是登记表。
struct DeviceWindow {
    sid: u32,
    top_pa: u64,
    ranges: Vec<PhysRange>,
}
/// 设备 SID → 独有窗口（`device::grant*` 逐段登记 BAR / DMA 范围时惰性创建）。
static DEVICE_WINDOWS: spin::Mutex<Vec<DeviceWindow>> = spin::Mutex::new(Vec::new());
/// 枚举到的全部 PCI 功能点（回退恒等时把这些上下文全部切过去）。
static DEVICE_FUNCS: spin::Mutex<Vec<(u8, u8, u8)>> = spin::Mutex::new(Vec::new());
/// E1c 目标设备（NVMe）的窗口表：保持 `[0, TARGET_WINDOW_LIMIT)` 不变。
static TARGET_TOP: AtomicU64 = AtomicU64::new(0);
/// 补登记过多 / 建表失败时的兜底恒等窗口（惰性创建）。
static FALLBACK_TOP: AtomicU64 = AtomicU64::new(0);
/// 已补登记的窗口次数（超过 [`MAX_WINDOW_HEALS`] 回退恒等）。
static WINDOW_HEALS: AtomicU64 = AtomicU64::new(0);
/// DMAR 里 DRHD 的个数（`0` = 无 IOMMU；`1` = 支持 per-domain 上下文；`>1` = 只回退用第一条）。
static DRHD_COUNT: AtomicU64 = AtomicU64::new(0);
/// 每个 DRHD 的寄存器基址（多 DRHD 时的取证/记录来源，默认路径不读）。
static DRHD_BASES: spin::Mutex<Vec<u64>> = spin::Mutex::new(Vec::new());

/// DMAR 里 DRHD 的个数（0 = 固件没有 IOMMU，或在 SAFE 构建里被跳过）。
pub fn drhd_count() -> u64 {
    DRHD_COUNT.load(Ordering::Relaxed)
}

/// DMAR 里**全部** DRHD 的寄存器基址（按表内顺序）。
///
/// 多 DRHD 时本文件只把第一个用于翻译（见 [`init`]），其余仅记录 —— 真机多单元
/// （每单元管一部分 PCI 设备）的完整支持还没做。
pub fn drhd_bases() -> Vec<u64> {
    DRHD_BASES.lock().clone()
}

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
// 全部 DRHD 的枚举（DMAR 自扫）
// ---------------------------------------------------------------------------
//
// `arch::acpi` 只把**第一条** DRHD 放进 `DmarSummary`（本批不允许改它），而"多 DRHD 该不该
// 走 per-domain 上下文"需要每个单元的 base。这里按 ACPI 的同一套结构自扫一遍：只在启动
// 阶段用一次，读到的字节/结论与 `acpi::parse_dmar` 交叉校验（个数不符就退回摘要）。

/// ACPI 表头长度（RSDP / XSDT / RSDT / DMAR 的公共前缀）。
const ACPI_HEADER_BYTES: u64 = 36;
/// DMAR 重映射结构的起始偏移（ACPI 头 36 + 自身固定部分 12）。
const DMAR_ENTRIES_OFF: usize = 48;

fn le16(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([b[off], b[off + 1]])
}
fn le32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}
fn le64(b: &[u8], off: usize) -> u64 {
    u64::from_le_bytes([
        b[off],
        b[off + 1],
        b[off + 2],
        b[off + 3],
        b[off + 4],
        b[off + 5],
        b[off + 6],
        b[off + 7],
    ])
}

/// 把恒等映射内的物理地址读成只读字节切片；越界 / 未映射返回 `None`。
fn phys_bytes(pa: u64, len: u64) -> Option<&'static [u8]> {
    if pa == 0 || len == 0 || !is_identity_mapped(pa, len) {
        return None;
    }
    // SAFETY: `is_identity_mapped` 已确认整段可读；固件表不会被复用（ACPI 页不在帧分配器池里）。
    Some(unsafe { core::slice::from_raw_parts(pa as *const u8, len as usize) })
}

/// 从 RSDP 找到 DMAR 表（只读；任何一步不可达都返回 `None`）。
fn locate_dmar() -> Option<&'static [u8]> {
    let rsdp = crate::bootinfo::get().rsdp_addr();
    let h = phys_bytes(rsdp, 20)?;
    if !h.starts_with(b"RSD PTR ") {
        return None;
    }
    let (root, xsdt) = if h[15] >= 2 {
        let hdr = phys_bytes(rsdp, ACPI_HEADER_BYTES)?;
        (le64(hdr, 24), true)
    } else {
        (le32(h, 16) as u64, false)
    };
    if root == 0 {
        return None;
    }
    let sig: &[u8; 4] = if xsdt { b"XSDT" } else { b"RSDT" };
    let hdr = phys_bytes(root, ACPI_HEADER_BYTES)?;
    if !hdr.starts_with(sig) {
        return None;
    }
    let len = le32(hdr, 4) as u64;
    if len < ACPI_HEADER_BYTES {
        return None;
    }
    let table = phys_bytes(root, len)?;
    let ent = if xsdt { 8 } else { 4 };
    let count = (table.len() as u64 - ACPI_HEADER_BYTES) / ent;
    let mut i = 0u64;
    while i < count {
        let off = ACPI_HEADER_BYTES as usize + (i * ent) as usize;
        let pa = if xsdt {
            le64(table, off)
        } else {
            le32(table, off) as u64
        };
        if let Some(dmar) = dmar_at(pa) {
            return Some(dmar);
        }
        i += 1;
    }
    None
}

/// 若 `pa` 处是一张完整的 DMAR 表，返回它的字节切片。
fn dmar_at(pa: u64) -> Option<&'static [u8]> {
    if pa == 0 {
        return None;
    }
    let hdr = phys_bytes(pa, ACPI_HEADER_BYTES)?;
    if !hdr.starts_with(b"DMAR") {
        return None;
    }
    let len = le32(hdr, 4) as u64;
    if len < ACPI_HEADER_BYTES {
        return None;
    }
    phys_bytes(pa, len)
}

/// 解析 DMAR 表里的**全部** DRHD 寄存器基址（纯函数，便于单测）。
///
/// 扫描规则与 `acpi::parse_dmar` 一致：固定部分 48 字节后是一串
/// `type(u16) length(u16) ...`，`type == 0` 且长度 ≥ 16 的结构是 DRHD，
/// 其 `reg_base` 在结构内偏移 8。
fn parse_drhd_bases(bytes: &[u8]) -> Vec<u64> {
    let mut bases = Vec::new();
    if bytes.len() < DMAR_ENTRIES_OFF || !bytes.starts_with(b"DMAR") {
        return bases;
    }
    let len = le32(bytes, 4) as usize;
    if len < DMAR_ENTRIES_OFF || len > bytes.len() {
        return bases;
    }
    let mut off = DMAR_ENTRIES_OFF;
    while off + 4 <= len {
        let ty = le16(bytes, off);
        let slen = le16(bytes, off + 2) as usize;
        if slen < 4 || off + slen > len {
            break;
        }
        if ty == 0 && slen >= 16 {
            bases.push(le64(bytes, off + 8));
        }
        off += slen;
    }
    bases
}

/// 自扫得到的所有 DRHD 基址（机器可读不到 DMAR 时为空）。
fn enumerate_drhds() -> Vec<u64> {
    locate_dmar().map(parse_drhd_bases).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// 初始化
// ---------------------------------------------------------------------------

/// 打开 IOMMU 的 DMA 重映射（E1b）。
///
/// `devices` 是 PCI 枚举结果 —— 每个功能点都会拿到一条 **translated** 上下文项；默认窗口
/// 开局为空，随后由 `device::grant*` 调 [`register_device_range`] 逐段补入该设备的 BAR /
/// DMA 物理范围（IOVA == 物理地址），阶段一内核与既有驱动照旧按物理地址 DMA 即可。
///
/// 返回是否真的把翻译打开（无 IOMMU / 表建不起来 / `GSTS` 未生效 → `false`）。
pub fn init(devices: &[PciDevice]) -> bool {
    // SAFE 变体 = 真机首次 bring-up: **不打开** DMA 重映射。
    //
    // 真实固件的 DMAR 与 QEMU 差别很大 (多 DRHD / RMRR / 显示引擎扫描输出也走 IOMMU)。
    // 一旦打开翻译, 任何一次落在登记窗口外的固件设备 DMA 都会失败 —— 典型表现就是
    // **画面瞬间全黑** (CPU 还活着)。显示与存储路线都确认稳定后, 再用普通构建验证 E1b/E1c。
    if crate::version::IS_SAFE {
        crate::video::println("[OK] VT-d: skipped in SAFE build (DMA remapping stays off)");
        return false;
    }

    let dmar = crate::arch::acpi::probe_dmar();
    if !dmar.found || dmar.drhd_count == 0 {
        crate::video::println("[OK] VT-d: no IOMMU in firmware, DMA remapping not enabled");
        return false;
    }

    // 枚举 DMAR 里**全部** DRHD（`acpi` 只暴露第一条）。自扫结果与摘要交叉校验：
    // 个数不符就以摘要为准 —— 宁可少记录，也不拿不可靠的表去改行为。
    let scanned = enumerate_drhds();
    let scan_ok = scanned.len() as u32 == dmar.drhd_count;
    let drhd_count = if scan_ok {
        scanned.len() as u64
    } else {
        dmar.drhd_count as u64
    };
    DRHD_COUNT.store(drhd_count, Ordering::Relaxed);
    {
        let mut bases = DRHD_BASES.lock();
        bases.clear();
        if scan_ok {
            bases.extend_from_slice(&scanned);
        }
        if bases.is_empty() {
            bases.push(dmar.first_drhd.reg_base);
        }
    }
    if drhd_count > 1 {
        // 多 DRHD 超出本批范围：**退回 E1b/E1c 的既有行为**（只用第一条单元），
        // 但把每台单元的 base 留下来，便于真机定位"哪台设备归哪个单元管"。
        crate::video::print("[WARN] VT-d: ");
        crate::video::print_u64(drhd_count);
        crate::video::print(" DRHD units (multi-unit unsupported), using first base=0x");
        crate::video::print_hex(dmar.first_drhd.reg_base);
        crate::video::println("; per-domain contexts disabled");
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

    // 1. 二级页表。
    //
    // **默认窗口开局为空**：只有 `device::grant*` 通过 [`register_device_range`] 登记过的
    // BAR / DMA 物理范围才建叶项。这样被攻破的驱动无法 DMA 到未登记的内核页、页表或
    // 他人用户页 —— 这是本步要修的安全属性。
    let Some(default_top) = alloc_zeroed_page() else {
        crate::video::println("[WARN] VT-d: cannot allocate default second-level page table");
        return false;
    };
    // 目标设备（NVMe）单列一张**更小的**窗口表：收窄它的 DMA 权限不会波及其它设备，
    // 也是 E2/E3 把飞地那台设备窗口继续收小的唯一落点。
    let target = crate::arch::pci::find_nvme(devices).map(|(b, d, f, _)| (b, d, f));
    let target_top = if let Some((b, d, f)) = target {
        TARGET_SID.store(
            ((b as u64) << 8) | ((d as u64) << 3) | f as u64,
            Ordering::Relaxed,
        );
        let Some(top) = build_window_slpt(aw, TARGET_WINDOW_LIMIT) else {
            crate::video::println("[WARN] VT-d: cannot allocate target second-level page tables");
            return false;
        };
        TARGET_TOP.store(top, Ordering::Release);
        Some(top)
    } else {
        None
    };
    // 登记表从空开始；设备授权发生在 `init` 之后（`device::grant*` 会逐段补进来）。
    DEVICE_WINDOWS.lock().clear();
    {
        let mut funcs = DEVICE_FUNCS.lock();
        funcs.clear();
        for d in devices {
            funcs.push((d.bus, d.dev, d.func));
        }
    }

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
    // 性能飞地地基: 记下 per-domain 上下文需要的三样东西（AGAW / 根表 / 默认窗口表）。
    // 三者齐全才允许 [`IovaWindow`] 工作；默认路径完全不读它们。
    ENABLED_AW.store(aw, Ordering::Release);
    ROOT_PA.store(root_pa, Ordering::Release);
    DEFAULT_TOP.store(default_top, Ordering::Release);
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
    crate::video::print(" (default=registered-only, aw=");
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

// ---------------------------------------------------------------------------
// 设备授权范围登记（默认窗口由登记表构造）
// ---------------------------------------------------------------------------

/// 打印 `bus:dev.func`（只用于登记/告警行）。
fn print_bdf(bus: u8, dev: u8, func: u8) {
    crate::video::print_u64(bus as u64);
    crate::video::print(":");
    crate::video::print_u64(dev as u64);
    crate::video::print(".");
    crate::video::print_u64(func as u64);
}

/// `sid` 是否是 E1c 的目标设备（只有目标窗口确实建好时才算）。
fn is_target_sid(sid: u64) -> bool {
    sid == TARGET_SID.load(Ordering::Relaxed) && TARGET_TOP.load(Ordering::Acquire) != 0
}

/// 某功能点当前应使用的**默认窗口**顶层表。
///
/// 优先级：兜底恒等（最后手段） > E1c 目标窗口 > 该设备自己的登记窗口 > 共享空表（拒 DMA）。
fn default_top_for(sid: u64) -> u64 {
    let fallback = FALLBACK_TOP.load(Ordering::Acquire);
    if fallback != 0 {
        return fallback;
    }
    if is_target_sid(sid) {
        return TARGET_TOP.load(Ordering::Acquire);
    }
    device_window_top(sid).unwrap_or_else(|| DEFAULT_TOP.load(Ordering::Acquire))
}

/// 该 SID 的独有窗口顶层表（不存在 = 尚未授权 / 未登记）。
fn device_window_top(sid: u64) -> Option<u64> {
    DEVICE_WINDOWS
        .lock()
        .iter()
        .find(|w| w.sid as u64 == sid)
        .map(|w| w.top_pa)
}

/// 读 `iova` 处的 4 KiB 叶项物理地址（不存在 / 被大页占据返回 `None`）。
fn slpt_leaf_4k_pa(table_pa: u64, aw: u64, iova: u64) -> Option<u64> {
    let levels = slpt_levels(aw);
    let mut table = table_pa;
    let mut level = 0;
    while level + 1 < levels {
        let e = read_entry_lo(table, slpt_index(iova, aw, level));
        if e & 1 == 0 || e & SLPT_PS != 0 {
            return None;
        }
        table = e & PAGE_MASK;
        level += 1;
    }
    let e = read_entry_lo(table, slpt_index(iova, aw, level));
    if e & 1 == 0 || e & SLPT_PS != 0 {
        None
    } else {
        Some(e & PAGE_MASK)
    }
}

/// 把 `iova` 处恒等映射到 `iova`（已映射到同一物理页则视为幂等成功）。
fn map_identity_page(top_pa: u64, aw: u64, iova: u64) -> bool {
    if slpt_map_4k(top_pa, aw, iova, iova) {
        return true;
    }
    slpt_leaf_4k_pa(top_pa, aw, iova) == Some(iova)
}

/// 把一段页对齐范围逐页恒等映射进 `top_pa`。
fn map_range_into(top_pa: u64, aw: u64, range: PhysRange) -> bool {
    if !range_within_agaw(range, aw) {
        return false;
    }
    let mut pa = range.base;
    while pa < range.end() {
        if !map_identity_page(top_pa, aw, pa) {
            return false;
        }
        pa += TABLE_BYTES;
    }
    true
}

/// 登记一台设备实际会 DMA 的物理范围（BAR / DMA 块），该设备的**独有窗口**按此构造。
///
/// - 范围先做 4 KiB 页对齐；同一设备的重复/相邻范围自动合并；
/// - 首次登记时新建该设备的二级页表并把上下文切过去（授权发生在驱动启动前，无 IOTLB 语义问题）；
/// - **目标设备（NVMe）**保持 E1c 的 `[0, 3 GiB)` 窗口不变（不改表 —— 它的 BAR0 恰好落在
///   3 GiB 边界，补进去会破坏 `block_srv` 越界探针的语义）；
/// - 映射失败**不会**让驱动永久无法 DMA：打印告警并回退恒等窗口；
/// - 无 IOMMU / SAFE 构建下 `aw == 0`，直接返回 `false`，行为与之前完全一致。
pub fn register_device_range(
    bus: u8,
    dev: u8,
    func: u8,
    paddr: u64,
    bytes: u64,
    is_dma: bool,
) -> bool {
    let aw = ENABLED_AW.load(Ordering::Acquire);
    if aw == 0 {
        return false;
    }
    let Some(range) = align_range(paddr, bytes) else {
        return false;
    };
    let sid = ((bus as u64) << 8) | ((dev as u64) << 3) | func as u64;

    if is_target_sid(sid) {
        // E1c 目标窗口保持不变；合法 DMA 块本就在 [0, 3 GiB) 内，无需改表。
        return true;
    }

    let (Some(root), Some(base)) = (
        nonzero(ROOT_PA.load(Ordering::Acquire)),
        nonzero(IOMMU_BASE.load(Ordering::Acquire)),
    ) else {
        return false;
    };

    let Some((top, created)) = device_window_add(sid, range) else {
        crate::video::print("[WARN] VT-d: cannot allocate window for ");
        print_bdf(bus, dev, func);
        crate::video::println("; falling back to identity");
        let _ = fallback_to_identity("device window allocation failed");
        return false;
    };
    if created {
        let Some(ctx_pa) = context_table_for_bus(root, bus) else {
            crate::video::print("[WARN] VT-d: cannot attach window for ");
            print_bdf(bus, dev, func);
            crate::video::println("; falling back to identity");
            let _ = fallback_to_identity("device window attach failed");
            return false;
        };
        core::sync::atomic::fence(Ordering::SeqCst);
        let (lo, hi) = context_entry(CTX_TT_TRANSLATED, agaw_index(aw), top);
        write_entry(ctx_pa, devfn(dev, func), lo, hi);
        if !flush_context_cache(base) {
            crate::video::println(
                "[WARN] VT-d: context-cache flush failed after window attach; falling back",
            );
            let _ = fallback_to_identity("context-cache flush failed");
            return false;
        }
    }

    if !range_within_agaw(range, aw) {
        // 高于 AGAW 的 BAR（如 virtio-net BAR4 = 0xC000000000）只被 CPU 访问、不是 DMA 落点；
        // 跳过映射，但告警保留线索。DMA 块越界则属于严重配置错误。
        if is_dma {
            crate::video::print("[WARN] VT-d: ");
            print_bdf(bus, dev, func);
            crate::video::print(" dma range 0x");
            crate::video::print_hex(range.base);
            crate::video::println(" exceeds the AGAW window, not mapped");
        }
        return false;
    }

    if !map_range_into(top, aw, range) {
        crate::video::print("[WARN] VT-d: cannot map ");
        print_bdf(bus, dev, func);
        crate::video::print(if is_dma { " dma range 0x" } else { " bar range 0x" });
        crate::video::print_hex(range.base);
        crate::video::println("; falling back to identity");
        let _ = fallback_to_identity("device window map failed");
        return false;
    }
    let _ = flush_iotlb_global(base);
    crate::video::print("[OK] VT-d: grant ");
    print_bdf(bus, dev, func);
    crate::video::print(if is_dma { " dma [0x" } else { " bar [0x" });
    crate::video::print_hex(range.base);
    crate::video::print(",0x");
    crate::video::print_hex(range.end());
    crate::video::println(") mapped");
    true
}

/// 把 `range` 记到 `sid` 的登记表；窗口不存在时新建一张。返回 `(top_pa, created)`。
fn device_window_add(sid: u64, range: PhysRange) -> Option<(u64, bool)> {
    let mut wins = DEVICE_WINDOWS.lock();
    if let Some(w) = wins.iter_mut().find(|w| w.sid as u64 == sid) {
        let mut all = w.ranges.clone();
        all.push(range);
        w.ranges = merge_ranges(&all);
        return Some((w.top_pa, false));
    }
    let top = alloc_zeroed_page()?;
    wins.push(DeviceWindow {
        sid: sid as u32,
        top_pa: top,
        ranges: vec![range],
    });
    Some((top, true))
}

fn nonzero(v: u64) -> Option<u64> {
    if v == 0 {
        None
    } else {
        Some(v)
    }
}

/// 把**全部**已枚举功能点切到恒等 `[0, 4 GiB)` 窗口（最后手段）。
///
/// 只在登记表构造失败或补登记次数超限时调用 —— 安全属性退化，但现有驱动绝不失去 DMA。
fn fallback_to_identity(reason: &str) -> bool {
    let aw = ENABLED_AW.load(Ordering::Acquire);
    let base = IOMMU_BASE.load(Ordering::Acquire);
    let root = ROOT_PA.load(Ordering::Acquire);
    if aw < 39 || base == 0 || root == 0 {
        return false;
    }
    if FALLBACK_TOP.load(Ordering::Acquire) != 0 {
        return true;
    }
    let Some(top) = build_window_slpt(aw, DEFAULT_WINDOW_LIMIT) else {
        crate::video::println("[WARN] VT-d: cannot allocate identity fallback page tables");
        return false;
    };
    FALLBACK_TOP.store(top, Ordering::Release);
    let funcs: Vec<(u8, u8, u8)> = DEVICE_FUNCS.lock().clone();
    for (bus, dev, func) in funcs {
        let Some(ctx_pa) = context_table_for_bus(root, bus) else {
            continue;
        };
        let (lo, hi) = context_entry(CTX_TT_TRANSLATED, agaw_index(aw), top);
        write_entry(ctx_pa, devfn(dev, func), lo, hi);
    }
    core::sync::atomic::fence(Ordering::SeqCst);
    let _ = flush_context_cache(base);
    let _ = flush_iotlb_global(base);
    crate::video::print("[WARN] VT-d: ");
    crate::video::print(reason);
    crate::video::println("; all devices fall back to identity [0,4GiB)");
    true
}

/// 给 `sid` 的**独有窗口**补登记一段落在窗口外的 DMA（64 KiB 对齐），并失效 IOTLB。
fn heal_window_range(sid: u64, iova: u64) -> bool {
    let aw = ENABLED_AW.load(Ordering::Acquire);
    let base = IOMMU_BASE.load(Ordering::Acquire);
    if aw == 0 {
        return false;
    }
    let Some(range) = heal_range(iova) else {
        return false;
    };
    if !range_within_agaw(range, aw) {
        return false;
    }
    let top = {
        let mut wins = DEVICE_WINDOWS.lock();
        let Some(w) = wins.iter_mut().find(|w| w.sid as u64 == sid) else {
            return false;
        };
        let mut all = w.ranges.clone();
        all.push(range);
        w.ranges = merge_ranges(&all);
        w.top_pa
    };
    if !map_range_into(top, aw, range) {
        return false;
    }
    if base != 0 {
        let _ = flush_iotlb_global(base);
    }
    true
}

// ---------------------------------------------------------------------------
// per-domain 独立 IOVA 窗口（性能飞地地基）
// ---------------------------------------------------------------------------

/// 4 KiB 页粒度二级页表的层级数（AGAW 30 → 2、39 → 3、48 → 4）。
fn slpt_levels(aw: u64) -> u64 {
    (aw - 12).div_ceil(9)
}

/// 二级页表第 `level` 级（0 = 顶层）的页表下标（每级 9 位）。
fn slpt_index(iova: u64, aw: u64, level: u64) -> u64 {
    let shift = 12 + 9 * (slpt_levels(aw) - 1 - level);
    (iova >> shift) & 0x1FF
}

/// 4 KiB 叶项（**没有** `SLPT_PS`，与 [`slpt_identity_page`] 的大页相反）。
fn slpt_leaf_4k(pa: u64) -> u64 {
    (pa & PAGE_MASK) | SLPT_R | SLPT_W
}

/// 读一个 128 位表项的低 64 位。
fn read_entry_lo(table_pa: u64, idx: u64) -> u64 {
    // SAFETY: 调用方保证 `table_pa` 是已分配的恒等映射表页。
    unsafe { core::ptr::read_volatile((table_pa + idx * ENTRY_BYTES) as *const u64) }
}

/// `[iova, iova+bytes)` 是否整段页对齐地落在 `[base, limit)` 内（纯函数，便于单测）。
fn window_range_ok(base: u64, limit: u64, iova: u64, bytes: u64) -> bool {
    if bytes == 0 || (iova & !PAGE_MASK) != 0 || (bytes & !PAGE_MASK) != 0 {
        return false;
    }
    match iova.checked_add(bytes) {
        Some(end) => iova >= base && end <= limit,
        None => false,
    }
}

/// 把一页 `paddr` 映射到 4 KiB 叶 `iova`（中间表按需分配）。已存在叶项 / 遇到大页则返回
/// `false`（不覆盖既有映射）。
fn slpt_map_4k(top_pa: u64, aw: u64, iova: u64, paddr: u64) -> bool {
    let levels = slpt_levels(aw);
    let mut table = top_pa;
    let mut level = 0;
    while level + 1 < levels {
        let idx = slpt_index(iova, aw, level);
        let e = read_entry_lo(table, idx);
        if e & 1 == 0 {
            let Some(next) = alloc_zeroed_page() else {
                return false;
            };
            write_entry(table, idx, slpt_table_entry(next), 0);
            table = next;
        } else {
            if e & SLPT_PS != 0 {
                return false; // 既有大页叶：4 KiB 映射无法落在它下面。
            }
            table = e & PAGE_MASK;
        }
        level += 1;
    }
    let idx = slpt_index(iova, aw, level);
    if read_entry_lo(table, idx) & 1 != 0 {
        return false; // 已有叶项：拒绝重复映射（避免泄漏）。
    }
    write_entry(table, idx, slpt_leaf_4k(paddr), 0);
    true
}

/// 清掉 `iova` 处的 4 KiB 叶项；路径/叶项不存在（或被大页占据）返回 `false`。
fn slpt_unmap_4k(top_pa: u64, aw: u64, iova: u64) -> bool {
    let levels = slpt_levels(aw);
    let mut table = top_pa;
    let mut level = 0;
    while level + 1 < levels {
        let e = read_entry_lo(table, slpt_index(iova, aw, level));
        if e & 1 == 0 || e & SLPT_PS != 0 {
            return false;
        }
        table = e & PAGE_MASK;
        level += 1;
    }
    let idx = slpt_index(iova, aw, level);
    if read_entry_lo(table, idx) & 1 == 0 {
        return false;
    }
    write_entry(table, idx, 0, 0);
    true
}

/// 取 `bus` 的上下文表；根项不存在时新建一张并挂进根表。
fn context_table_for_bus(root_pa: u64, bus: u8) -> Option<u64> {
    if let Some(pa) = existing_context_table(root_pa, bus) {
        return Some(pa);
    }
    let pa = alloc_zeroed_page()?;
    let (lo, hi) = root_entry(pa);
    write_entry(root_pa, bus as u64, lo, hi);
    Some(pa)
}

/// 只查 `bus` 已有的上下文表（不分配）。
fn existing_context_table(root_pa: u64, bus: u8) -> Option<u64> {
    let e = read_entry_lo(root_pa, bus as u64);
    if e & 1 == 0 {
        return None;
    }
    let pa = e & PAGE_MASK;
    if pa == 0 {
        None
    } else {
        Some(pa)
    }
}

/// 请求上下文缓存失效并轮询 `GSTS.CFIS`（超时返回 `false`）。
fn flush_context_cache(base: u64) -> bool {
    set_gcmd(base, GCMD_CFI);
    let mut spins = 0u32;
    // SAFETY: 寄存器块已确认在恒等映射内。
    while unsafe { rd32(base, REG_GSTS) } & GSTS_CFIS != 0 {
        spins += 1;
        if spins > 1_000_000 {
            return false;
        }
    }
    true
}

/// 请求**全局 IOTLB 失效**并轮询 `GSTS.IVT` 清零（超时返回 `false`）。
fn flush_iotlb_global(base: u64) -> bool {
    set_gcmd(base, GCMD_IVA);
    let mut spins = 0u32;
    // SAFETY: 寄存器块已确认在恒等映射内。
    while unsafe { rd32(base, REG_GSTS) } & GSTS_IVT != 0 {
        spins += 1;
        if spins > 1_000_000 {
            return false;
        }
    }
    true
}

// ---------------------------------------------------------------------------
// 飞地 IOVA 分配器（每域一张 4 KiB 粒度的位图窗口）
// ---------------------------------------------------------------------------
//
// `SYS_ENCLAVE_MAP` 的 `a2 == 0` 表示"由内核选地址"。每个域各有一张**独立**的二级页表，
// 所以各域可以共用同一段 IOVA 号段；这里只做**域内**的连续页分配，域间不会互相影响。

/// 自动分配 IOVA 的每域窗口起点（1 GiB）。
///
/// 选 1 GiB 的理由：① 非 0（0 是 `DeviceGrant` 的"未绑定"哨兵）；② 避开用户虚拟地址空间
/// （`paging::USER_SPACE_BASE` = 512 GiB）；③ 严格小于 4 GiB —— 本机 QEMU 的 `intel-iommu`
/// 只翻译 < 4 GiB 的 IOVA（见 [`TARGET_WINDOW_LIMIT`] 的实测说明）。
pub const ENCLAVE_IOVA_BASE: u64 = 1 << 30;

/// 每域自动 IOVA 窗口的字节数（64 MiB = 16384 个 4 KiB 页）。
pub const ENCLAVE_IOVA_BYTES: u64 = 64 << 20;

/// 4 KiB 粒度的连续页位图（纯逻辑，单测覆盖）。
///
/// `1` = 已占用。分配采用首次适配（最低地址），释放要求整段都已占用 —— 部分释放返回
/// `false` 且不改动任何位，避免把从未分配的页悄悄标记为空闲。
struct IovaBitmap {
    pages: u64,
    words: Vec<u64>,
}

impl IovaBitmap {
    fn new(pages: u64) -> Self {
        Self {
            pages,
            words: vec![0u64; pages.div_ceil(64) as usize],
        }
    }

    fn is_used(&self, page: u64) -> bool {
        self.words[(page / 64) as usize] & (1u64 << (page % 64)) != 0
    }

    fn set_used(&mut self, page: u64, used: bool) {
        let word = &mut self.words[(page / 64) as usize];
        let bit = 1u64 << (page % 64);
        if used {
            *word |= bit;
        } else {
            *word &= !bit;
        }
    }

    /// 找 `count` 个连续空闲页（首次适配）；成功返回起始页下标。
    fn alloc(&mut self, count: u64) -> Option<u64> {
        if count == 0 || count > self.pages {
            return None;
        }
        let mut run = 0u64;
        let mut start = 0u64;
        for page in 0..self.pages {
            if self.is_used(page) {
                run = 0;
                continue;
            }
            if run == 0 {
                start = page;
            }
            run += 1;
            if run == count {
                for p in start..start + count {
                    self.set_used(p, true);
                }
                return Some(start);
            }
        }
        None
    }

    /// 释放 `[page, page + count)`；越界或含未占用页时整体拒绝。
    fn free(&mut self, page: u64, count: u64) -> bool {
        if count == 0 {
            return false;
        }
        let Some(end) = page.checked_add(count) else {
            return false;
        };
        if end > self.pages {
            return false;
        }
        for p in page..end {
            if !self.is_used(p) {
                return false;
            }
        }
        for p in page..end {
            self.set_used(p, false);
        }
        true
    }
}

/// 一个域的自动 IOVA 分配器。
struct DomainIova {
    domain: u64,
    base: u64,
    bitmap: IovaBitmap,
}

/// 每域自动 IOVA 分配器（首次使用时惰性创建）。
///
/// **默认路径零变化**：没有域请求 `a2 == 0` 时这张表始终为空。
static IOVA_ALLOCATORS: spin::Mutex<Vec<DomainIova>> = spin::Mutex::new(Vec::new());

/// 在 `domain` 的自动 IOVA 窗口里分配 `bytes`（4 KiB 对齐、非空、≤ [`ENCLAVE_IOVA_BYTES`]）。
///
/// 返回**实际 IOVA 基址**；窗口耗尽 / 参数非法返回 `None`。分配只占用位图，不建页表 ——
/// 调用方随后用 [`IovaWindow::create`] 建窗口，失败时须调 [`free_iova`] 归还。
pub fn alloc_iova(domain: u64, bytes: u64) -> Option<u64> {
    if bytes == 0 || (bytes & !PAGE_MASK) != 0 || bytes > ENCLAVE_IOVA_BYTES {
        return None;
    }
    let mut allocators = IOVA_ALLOCATORS.lock();
    let idx = match allocators.iter().position(|a| a.domain == domain) {
        Some(i) => i,
        None => {
            allocators.push(DomainIova {
                domain,
                base: ENCLAVE_IOVA_BASE,
                bitmap: IovaBitmap::new(ENCLAVE_IOVA_BYTES >> 12),
            });
            allocators.len() - 1
        }
    };
    let start = allocators[idx].bitmap.alloc(bytes >> 12)?;
    Some(allocators[idx].base + (start << 12))
}

/// 归还 [`alloc_iova`] 分配的 `[iova, iova + bytes)`。
///
/// 段越界、非对齐或含未占用页返回 `false`（位图保持原样）。
pub fn free_iova(domain: u64, iova: u64, bytes: u64) -> bool {
    if bytes == 0 || (iova & !PAGE_MASK) != 0 || (bytes & !PAGE_MASK) != 0 {
        return false;
    }
    let Some(offset) = iova.checked_sub(ENCLAVE_IOVA_BASE) else {
        return false;
    };
    let Some(end) = offset.checked_add(bytes) else {
        return false;
    };
    if end > ENCLAVE_IOVA_BYTES {
        return false;
    }
    let mut allocators = IOVA_ALLOCATORS.lock();
    let Some(idx) = allocators.iter().position(|a| a.domain == domain) else {
        return false;
    };
    allocators[idx].bitmap.free(offset >> 12, bytes >> 12)
}

/// 一个**独立 IOMMU 域**的 IOVA 窗口（性能飞地地基）。
///
/// 窗口自带一张二级页表（4 KiB 叶，独立于 [`init`] 建好的恒等表）：只有
/// [`map_contiguous`](Self::map_contiguous) 显式映射过的页才可达，窗口外的 IOVA 不建叶项 →
/// 设备 DMA 被 IOMMU 拒绝（与 E1c 的越界拒绝同一机制）。
///
/// **默认路径零变化**：不调用 [`create`](Self::create) 的域/设备仍走 `init` 的恒等窗口。
///
/// 使用顺序是硬要求：先 `map_contiguous`，再 `attach_device`。本批**没有**实现 IOTLB 失效，
/// 因此只能在设备**开始 DMA 之前**切上下文（授权阶段正好满足）；设备跑起来后改映射必须先停设备。
pub struct IovaWindow {
    domain: u64,
    aw: u64,
    top_pa: u64,
    base: u64,
    limit: u64,
}

impl IovaWindow {
    /// 为 `domain` 建一个空窗口 `[iova_base, iova_base + bytes)`（页对齐、非空）。
    ///
    /// 只在**单 DRHD 且翻译已打开**时可用；否则打印一行原因并返回 `None`（调用方继续走
    /// 恒等映射）。空窗口没有任何叶项 —— 在 `map_contiguous` 之前，该域的设备 DMA 会被拒。
    pub fn create(domain: u64, iova_base: u64, bytes: u64) -> Option<Self> {
        let aw = ENABLED_AW.load(Ordering::Acquire);
        if aw == 0 {
            crate::video::println(
                "[WARN] VT-d: per-domain IOVA window needs DMA remapping (off), identity unchanged",
            );
            return None;
        }
        let units = DRHD_COUNT.load(Ordering::Relaxed);
        if units != 1 {
            crate::video::print("[WARN] VT-d: per-domain IOVA window unsupported with ");
            crate::video::print_u64(units);
            crate::video::println(" DRHD units, identity unchanged");
            return None;
        }
        // IOVA 0 用作 DeviceGrant 的"未绑定"哨兵，窗口不能占用它。
        if bytes == 0
            || iova_base == 0
            || (iova_base & !PAGE_MASK) != 0
            || (bytes & !PAGE_MASK) != 0
        {
            crate::video::println(
                "[WARN] VT-d: IOVA window must be non-zero, non-empty and page aligned, identity unchanged",
            );
            return None;
        }
        let Some(limit) = iova_base.checked_add(bytes) else {
            crate::video::println("[WARN] VT-d: IOVA window overflows, identity unchanged");
            return None;
        };
        // IOVA 必须在该 AGAW 能表示的地址范围内。
        if limit > (1u64 << aw) {
            crate::video::println(
                "[WARN] VT-d: IOVA window exceeds the AGAW range, identity unchanged",
            );
            return None;
        }
        let Some(top_pa) = alloc_zeroed_page() else {
            crate::video::println(
                "[WARN] VT-d: cannot allocate per-domain page table, identity unchanged",
            );
            return None;
        };
        Some(Self {
            domain,
            aw,
            top_pa,
            base: iova_base,
            limit,
        })
    }

    /// 该窗口所属的 domain 号（仅用于诊断/记账）。
    pub fn domain(&self) -> u64 {
        self.domain
    }

    /// 窗口 IOVA 基址。
    pub fn base(&self) -> u64 {
        self.base
    }

    /// 窗口上界（不含）。
    pub fn limit(&self) -> u64 {
        self.limit
    }

    /// 把**物理连续**的 `[paddr, paddr+bytes)` 映射到 `iova` 起的 4 KiB 页。
    ///
    /// 要求 `iova`/`paddr`/`bytes` 都页对齐，且整段落在窗口内。中途失败会撤掉本次已建的
    /// 叶项（表页保留），返回 `false` —— 调用方应据此放弃绑定、继续用恒等映射。
    pub fn map_contiguous(&mut self, iova: u64, paddr: u64, bytes: u64) -> bool {
        if !window_range_ok(self.base, self.limit, iova, bytes)
            || (paddr & !PAGE_MASK) != 0
            || paddr.checked_add(bytes).is_none()
        {
            return false;
        }
        let pages = bytes >> 12;
        let mut done = 0u64;
        while done < pages {
            let iova_i = iova + (done << 12);
            let pa_i = paddr + (done << 12);
            if !slpt_map_4k(self.top_pa, self.aw, iova_i, pa_i) {
                self.rollback(iova, done);
                return false;
            }
            done += 1;
        }
        core::sync::atomic::fence(Ordering::SeqCst);
        true
    }

    /// 撤掉 `iova` 起 `bytes` 字节的 4 KiB 叶项；任一页不存在返回 `false`（其余页仍会被撤）。
    pub fn unmap(&mut self, iova: u64, bytes: u64) -> bool {
        if !window_range_ok(self.base, self.limit, iova, bytes) {
            return false;
        }
        let pages = bytes >> 12;
        let mut ok = true;
        let mut i = 0u64;
        while i < pages {
            ok &= slpt_unmap_4k(self.top_pa, self.aw, iova + (i << 12));
            i += 1;
        }
        core::sync::atomic::fence(Ordering::SeqCst);
        ok
    }

    /// 把该功能点（`bus:dev.func`）的上下文项切到这个窗口的二级页表。
    ///
    /// 只在单 DRHD 下可用；写项后做上下文缓存失效。失效失败或任何前置条件不满足时，
    /// **恢复默认恒等上下文**并返回 `false` —— 绝不让设备失去 DMA。
    pub fn attach_device(&self, bus: u8, dev: u8, func: u8) -> bool {
        let root = ROOT_PA.load(Ordering::Acquire);
        let base = IOMMU_BASE.load(Ordering::Acquire);
        let sid = ((bus as u64) << 8) | ((dev as u64) << 3) | func as u64;
        let default_top = default_top_for(sid);
        if root == 0 || base == 0 || default_top == 0 {
            crate::video::println(
                "[WARN] VT-d: IOMMU context not ready, device keeps identity mapping",
            );
            return false;
        }
        let Some(ctx_pa) = context_table_for_bus(root, bus) else {
            crate::video::println(
                "[WARN] VT-d: cannot get context table, device keeps identity mapping",
            );
            return false;
        };
        core::sync::atomic::fence(Ordering::SeqCst);
        let (lo, hi) = context_entry(CTX_TT_TRANSLATED, agaw_index(self.aw), self.top_pa);
        write_entry(ctx_pa, devfn(dev, func), lo, hi);
        if flush_context_cache(base) {
            return true;
        }
        // 回退：恢复默认恒等上下文（该功能点原来就是这条，行为不变）。
        let (lo, hi) = context_entry(CTX_TT_TRANSLATED, agaw_index(self.aw), default_top);
        write_entry(ctx_pa, devfn(dev, func), lo, hi);
        let _ = flush_context_cache(base);
        crate::video::println(
            "[WARN] VT-d: context-cache flush failed, device stays on identity window",
        );
        false
    }

    /// 撤掉本次已建的叶项（尽力而为，表页不回收）。
    fn rollback(&self, iova: u64, pages: u64) {
        let mut i = 0u64;
        while i < pages {
            slpt_unmap_4k(self.top_pa, self.aw, iova + (i << 12));
            i += 1;
        }
    }
}

/// 把某功能点恢复成**默认窗口**（per-domain 绑定失败 / 未来释放窗口时用）。
///
/// 目标设备恢复到 E1c 窗口，其余设备恢复到**登记表构造的默认窗口**（不是恒等 4 GiB）。
pub fn detach_device(bus: u8, dev: u8, func: u8) -> bool {
    let root = ROOT_PA.load(Ordering::Acquire);
    let base = IOMMU_BASE.load(Ordering::Acquire);
    let aw = ENABLED_AW.load(Ordering::Acquire);
    let sid = ((bus as u64) << 8) | ((dev as u64) << 3) | func as u64;
    let default_top = default_top_for(sid);
    if root == 0 || base == 0 || aw == 0 || default_top == 0 {
        return false;
    }
    let Some(ctx_pa) = existing_context_table(root, bus) else {
        return false;
    };
    let (lo, hi) = context_entry(CTX_TT_TRANSLATED, agaw_index(aw), default_top);
    core::sync::atomic::fence(Ordering::SeqCst);
    write_entry(ctx_pa, devfn(dev, func), lo, hi);
    let flushed = flush_context_cache(base);
    if flushed {
        let _ = flush_iotlb_global(base);
    }
    flushed
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
        let valid = fault_record_valid(info);
        let sid = fault_sid(info) as u64;
        let granted = device_window_top(sid).is_some();

        if valid && !is_target_sid(sid) && granted {
            // **失败安全**：登记遗漏的合法设备 DMA 不能失败 —— 先打明确告警，再把这个
            // 64 KiB 范围补进**该设备自己的**窗口。补登记次数超限时回退恒等（见下）。
            crate::video::print("[WARN] VT-d: DMA outside granted window sid=0x");
            crate::video::print_hex(sid);
            crate::video::print(" iova=0x");
            crate::video::print_hex(iova);
            crate::video::println(" (grant omission), extending device window");
            if heal_window_range(sid, iova) {
                let heals = WINDOW_HEALS.fetch_add(1, Ordering::Relaxed) + 1;
                crate::video::print("[OK] VT-d: device window extended for 0x");
                crate::video::print_hex(iova);
                crate::video::print(" heals=");
                crate::video::print_u64(heals);
                crate::video::println("");
                if heals >= MAX_WINDOW_HEALS {
                    let _ = fallback_to_identity("window healing limit reached");
                }
            } else {
                let heals = WINDOW_HEALS.fetch_add(1, Ordering::Relaxed) + 1;
                crate::video::println(
                    "[WARN] VT-d: cannot extend device window; device DMA may fail",
                );
                if heals >= MAX_WINDOW_HEALS {
                    let _ = fallback_to_identity("window healing failed repeatedly");
                }
            }
        } else {
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
            if valid {
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
        }
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

    /// 多 DRHD：自扫必须收齐每一条（而不是只看第一条），并跳过 RMRR 等其它结构。
    #[test]
    fn drhd_scan_collects_every_unit() {
        // 固定部分 48 B + 2 条 DRHD（各 16 B）+ 1 条 RMRR（4 B）。
        const LEN: usize = 48 + 16 + 16 + 4;
        let mut t = [0u8; LEN];
        t[0..4].copy_from_slice(b"DMAR");
        t[4..8].copy_from_slice(&(LEN as u32).to_le_bytes());
        // DRHD #0: type=0, len=16, flags=INCLUDE_PCI_ALL, segment=0, base=0xFED9_0000。
        t[48..50].copy_from_slice(&0u16.to_le_bytes());
        t[50..52].copy_from_slice(&16u16.to_le_bytes());
        t[52] = 1;
        t[56..64].copy_from_slice(&0xFED9_0000u64.to_le_bytes());
        // DRHD #1: 同一张表里的第二台单元。
        let off = 64;
        t[off..off + 2].copy_from_slice(&0u16.to_le_bytes());
        t[off + 2..off + 4].copy_from_slice(&16u16.to_le_bytes());
        t[off + 8..off + 16].copy_from_slice(&0xFEDA_0000u64.to_le_bytes());
        // RMRR: type=1, len=4 —— 不是 DRHD，不能被收进来。
        let off = 80;
        t[off..off + 2].copy_from_slice(&1u16.to_le_bytes());
        t[off + 2..off + 4].copy_from_slice(&4u16.to_le_bytes());

        assert_eq!(parse_drhd_bases(&t).as_slice(), &[0xFED9_0000, 0xFEDA_0000]);

        // 非 DMAR / 长度不自洽 → 空（调用方据此退回 `acpi` 摘要）。
        assert!(parse_drhd_bases(b"NOPE").is_empty());
        t[4..8].copy_from_slice(&8u32.to_le_bytes());
        assert!(parse_drhd_bases(&t).is_empty());
    }

    /// 4 KiB 页表的层级与各级下标：30/39/48 位宽度分别 2/3/4 级，索引各取 9 位。
    #[test]
    fn slpt_levels_and_indices() {
        assert_eq!(slpt_levels(30), 2);
        assert_eq!(slpt_levels(39), 3);
        assert_eq!(slpt_levels(48), 4);

        // AGAW 39：顶层（1 GiB/项）—— 1 GiB 对应下标 1。
        assert_eq!(slpt_index(0x4000_0000, 39, 0), 1);
        assert_eq!(slpt_index(0x4000_0000, 39, 1), 0);
        assert_eq!(slpt_index(0x4000_0000, 39, 2), 0);
        // 第二级（2 MiB/项）与第三级（4 KiB/项）。
        assert_eq!(slpt_index(0x0020_0000, 39, 1), 1);
        assert_eq!(slpt_index(0x1000, 39, 2), 1);
        // AGAW 48：顶层 shift = 39（512 GiB/项）。
        assert_eq!(slpt_index(1 << 39, 48, 0), 1);
        assert_eq!(slpt_index(1 << 39, 48, 1), 0);
        // AGAW 30：顶层 shift = 21（2 MiB/项），无 1 GiB 级。
        assert_eq!(slpt_index(0x20_0000, 30, 0), 1);
        assert_eq!(slpt_index(0x1000, 30, 1), 1);

        // 4 KiB 叶项必须**没有** PS 位（否则会被当成 2 MiB/1 GiB 大页）。
        let e = slpt_leaf_4k(0x1234_5000);
        assert_eq!(e & PAGE_MASK, 0x1234_5000);
        assert_eq!(e & (SLPT_R | SLPT_W), SLPT_R | SLPT_W);
        assert_eq!(e & SLPT_PS, 0);
    }

    /// 独立窗口的边界计算：页对齐、非空、整段落在 `[base, limit)` 内，溢出即拒。
    #[test]
    fn iova_window_bounds() {
        let base = 0x4000_0000u64;
        let limit = base + 0x20_0000;

        assert!(window_range_ok(base, limit, base, 0x20_0000));
        assert!(window_range_ok(base, limit, base + 0x1000, 0x1000));
        // 越界（哪怕只多一页）、基址之前、非页对齐、空区间、加法溢出。
        assert!(!window_range_ok(base, limit, base, 0x20_1000));
        assert!(!window_range_ok(base, limit, base - 0x1000, 0x1000));
        assert!(!window_range_ok(base, limit, base + 1, 0x1000));
        assert!(!window_range_ok(base, limit, base, 0));
        assert!(!window_range_ok(base, limit, base, 0x1001));
        assert!(!window_range_ok(base, limit, u64::MAX & !0xFFF, 0x2000));
    }

    /// 位图分配器：首次适配、连续页、部分释放拒绝、越界拒绝。
    #[test]
    fn iova_bitmap_tracks_runs() {
        let mut b = IovaBitmap::new(8);
        assert_eq!(b.alloc(1), Some(0));
        assert_eq!(b.alloc(2), Some(1));
        assert_eq!(b.alloc(4), Some(3));
        assert_eq!(b.alloc(1), Some(7)); // 最后一个空闲页
        assert_eq!(b.alloc(1), None); // 窗口已满

        // 释放中间两页后，最低地址的空洞优先被复用。
        assert!(b.free(1, 2));
        assert_eq!(b.alloc(2), Some(1));
        // 再释放一次成功（两页都已占用）；重复释放已空闲的段则整体拒绝。
        assert!(b.free(1, 2));
        assert!(!b.free(1, 2));
        // 越界 / 空长度 / 超窗口。
        assert!(!b.free(7, 2));
        assert!(!b.free(0, 0));
        assert_eq!(b.alloc(0), None);
        assert_eq!(b.alloc(9), None);

        // 含未占用页的整段释放同样拒绝（不会把空闲页"再释放"一次）。
        let mut gappy = IovaBitmap::new(4);
        assert_eq!(gappy.alloc(2), Some(0));
        assert!(!gappy.free(0, 4));
        assert!(gappy.free(0, 2));
        assert_eq!(gappy.alloc(4), Some(0));
    }

    /// 登记范围：页对齐（向下对齐起点、向上对齐终点）/ 零长度 / 溢出拒绝。
    #[test]
    fn grant_range_alignment_and_rejection() {
        // 未对齐的起点与长度都被扩到整页。
        let r = align_range(0x1234, 0x2001).unwrap();
        assert_eq!(r.base, 0x1000);
        assert_eq!(r.len, 0x3000);
        assert_eq!(r.end(), 0x4000);

        // 已对齐的输入不平白多给字节。
        let r = align_range(0x2000, 0x1000).unwrap();
        assert_eq!((r.base, r.len), (0x2000, 0x1000));

        // 长度 0 / 加法溢出 -> 拒绝。
        assert!(align_range(0x2000, 0).is_none());
        assert!(align_range(u64::MAX & !0xFFF, 0x2000).is_none());
        assert!(align_range(u64::MAX, 1).is_none());

        // 失败补登记按 64 KiB 向下对齐，且长度固定。
        let h = heal_range(0x1_0000 + 0x2345).unwrap();
        assert_eq!(h.base, 0x1_0000);
        assert_eq!(h.len, HEAL_BYTES);
    }

    /// 登记表合并：重叠、相邻、包含、重复都合成一条；不相交的保持排序。
    #[test]
    fn grant_ranges_merge_and_sort() {
        let r = |base, len| PhysRange { base, len };
        // 乱序 + 相邻：1000..2000 与 2000..3000 合成 1000..3000。
        assert_eq!(
            merge_ranges(&[r(0x2000, 0x1000), r(0x1000, 0x1000)]),
            vec![r(0x1000, 0x2000)]
        );
        // 重叠：取并集而不是简单相加。
        assert_eq!(
            merge_ranges(&[r(0x1000, 0x3000), r(0x2000, 0x3000)]),
            vec![r(0x1000, 0x4000)]
        );
        // 包含 / 重复。
        assert_eq!(
            merge_ranges(&[r(0x1000, 0x3000), r(0x1800, 0x1000), r(0x1000, 0x3000)]),
            vec![r(0x1000, 0x3000)]
        );
        // 不相交：保持升序，中间的空洞不合并。
        assert_eq!(
            merge_ranges(&[r(0x5000, 0x1000), r(0x1000, 0x1000)]),
            vec![r(0x1000, 0x1000), r(0x5000, 0x1000)]
        );
        // 空范围被忽略。
        assert!(merge_ranges(&[r(0x1000, 0)]).is_empty());
        assert!(merge_ranges(&[]).is_empty());
    }

    /// 越界拒绝：范围必须完整落在 AGAW 可表示地址内（跨边界也不放行）。
    #[test]
    fn grant_ranges_reject_out_of_bounds() {
        let aw = 39u64;
        let top = 1u64 << aw;
        let r = |base, len| PhysRange { base, len };

        assert!(range_within_agaw(r(0, 0x1000), aw));
        assert!(range_within_agaw(r(top - 0x1000, 0x1000), aw));
        // 终点越过 1<<39 一页即拒。
        assert!(!range_within_agaw(r(top - 0x1000, 0x2000), aw));
        assert!(!range_within_agaw(r(top, 0x1000), aw));
        // 高于 4 GiB 的 BAR（如 virtio-net BAR4 = 0xC000000000）在 39 位下不可表示。
        assert!(!range_within_agaw(r(0xC000_000000, 0x4000), aw));

        // 30 位窗口同样拒绝跨边界范围。
        let aw30 = 30u64;
        assert!(range_within_agaw(r((1 << 30) - 0x1000, 0x1000), aw30));
        assert!(!range_within_agaw(r((1 << 30) - 0x1000, 0x2000), aw30));
    }

    /// 每域 IOVA 分配器：连续、不重叠、可回收；耗尽与非法长度返回 `None`。
    #[test]
    fn iova_allocator_reuses_freed_ranges() {
        const D: u64 = 0xE000_0001; // 测试专用域号，避开真实域。
        let a = alloc_iova(D, 0x1000).unwrap();
        let b = alloc_iova(D, 0x2000).unwrap();
        assert_eq!(a, ENCLAVE_IOVA_BASE);
        assert_eq!(b, ENCLAVE_IOVA_BASE + 0x1000);
        assert_eq!(b & 0xFFF, 0);

        // 释放第一段后，分配器应把同一个最低地址还回来。
        assert!(free_iova(D, a, 0x1000));
        assert_eq!(alloc_iova(D, 0x1000), Some(a));
        // 未分配页 / 越界 / 非对齐一律拒绝。
        assert!(!free_iova(D, ENCLAVE_IOVA_BASE + 0x3000, 0x1000));
        assert!(!free_iova(D, ENCLAVE_IOVA_BASE - 0x1000, 0x1000));
        assert!(!free_iova(D, a + 1, 0x1000));

        assert!(alloc_iova(D, ENCLAVE_IOVA_BYTES + 0x1000).is_none());
        assert!(alloc_iova(D, 0).is_none());
        assert!(alloc_iova(D, 0x1001).is_none());

        // 把整张窗口一次分完；之后任何请求都应失败（窗口耗尽）。
        assert!(free_iova(D, a, 0x1000));
        assert!(free_iova(D, b, 0x2000));
        let all = alloc_iova(D, ENCLAVE_IOVA_BYTES).unwrap();
        assert_eq!(all, ENCLAVE_IOVA_BASE);
        assert!(alloc_iova(D, 0x1000).is_none());
        assert!(free_iova(D, all, ENCLAVE_IOVA_BYTES));
    }
}
