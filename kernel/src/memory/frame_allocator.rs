//! 物理帧分配器 — 位图实现
//!
//! 每个位代表一个 4 KiB 物理帧; 1 = 已占用, 0 = 空闲。
//! 采用白名单策略: 初始化时全部标记为占用, 仅将内存图中
//! `CONVENTIONAL` 且位于内核镜像之后的帧标记为空闲。

use crate::bootinfo::{BootInfo, MemoryDescriptor, MEMORY_CONVENTIONAL};
use x86_64::registers::control::Cr3;

/// 帧大小 (4 KiB)
pub const FRAME_SIZE: usize = 4096;

/// 页表项物理地址掩码 (bits 12..51)。
const PTE_ADDR_MASK: usize = 0x000F_FFFF_FFFF_F000;
/// 页表项 PS 位 (大页标志)。
const PTE_HUGE: usize = 1 << 7;

/// 位图容量: 1 MiB = 8 Mi 帧 = 32 GiB 物理内存上限
const BITMAP_SIZE: usize = 1024 * 1024;
const MAX_MANAGED_FRAMES: usize = BITMAP_SIZE * 8;

static mut FRAME_BITMAP: [u8; BITMAP_SIZE] = [0; BITMAP_SIZE];
static mut TOTAL_FRAMES: usize = 0;
static mut FREE_FRAMES: usize = 0;

/// 内核**保留**的物理帧区间 (页对齐的 `[start, end)`), 任何路径都不得释放。
///
/// 目前只有帧缓冲: 它被 `SYS_FB_MAP` 映射进 `gfx_srv` 的用户空间 (`map_mmio`, **不**走
/// 引用计数), 于是「同域重启」里 `free_user_space` 逐页归还时, 会把显存当"本域独占帧"
/// 直接 `free_frame` —— 大内存配置下等于把屏幕内存交回分配器, 之后谁拿到谁涂花屏。
/// 登记成保留区间后 `free_frame` 对它们空操作。与 `reserve_frame` 的区别: 那个只在初始化
/// 时"占位", 这个挡的是运行期的释放。
const MAX_PINNED_RANGES: usize = 4;
static mut PINNED_RANGES: [(u64, u64); MAX_PINNED_RANGES] = [(0, 0); MAX_PINNED_RANGES];
static mut PINNED_LEN: usize = 0;

/// 登记一段**保留**物理区间 `[start, end)` (字节地址, 内部按帧对齐)。
fn pin_range(start: u64, end: u64) {
    let s = start & !(FRAME_SIZE as u64 - 1);
    let e = (end + FRAME_SIZE as u64 - 1) & !(FRAME_SIZE as u64 - 1);
    unsafe {
        if PINNED_LEN < MAX_PINNED_RANGES {
            PINNED_RANGES[PINNED_LEN] = (s, e);
            PINNED_LEN += 1;
        }
    }
}

/// 该物理帧是否落在保留区间内 (保留帧不可释放)。
fn is_pinned(addr: u64) -> bool {
    unsafe {
        PINNED_RANGES[..PINNED_LEN]
            .iter()
            .any(|&(s, e)| addr >= s && addr < e)
    }
}

// 链接脚本导出的内核镜像结束地址
extern "C" {
    static _kernel_end: u8;
}

#[inline]
fn bitmap_set(idx: usize) {
    unsafe {
        FRAME_BITMAP[idx / 8] |= 1u8 << (idx % 8);
    }
}

#[inline]
fn bitmap_clear(idx: usize) {
    unsafe {
        FRAME_BITMAP[idx / 8] &= !(1u8 << (idx % 8));
    }
}

#[inline]
fn bitmap_test(idx: usize) -> bool {
    unsafe { FRAME_BITMAP[idx / 8] & (1u8 << (idx % 8)) != 0 }
}

/// 根据内存图初始化位图 (仅需调用一次)。
pub fn init(info: &BootInfo) {
    let mmap_addr = info.mmap_addr as usize;
    let count = info.mmap_entry_count as usize;
    let entry_size = info.mmap_entry_size as usize;
    // 向上取整到 64 KiB 作为保留上界: 链接符号 `_kernel_end` 与镜像实际占用末尾可能
    // 有少量出入, 留出余量可确保内核栈顶所在的帧绝不会被当作空闲帧分配出去 (栈顶就在
    // 镜像末尾附近, 一旦被复用为页表, 栈写入会立刻破坏地址翻译)。
    let kernel_end = {
        let raw = unsafe { &_kernel_end as *const u8 as usize };
        (raw + 0x1_0000 - 1) & !(0x1_0000 - 1)
    };

    // 帧缓冲占用的物理帧区间 (按页对齐), 防止被当作空闲帧分配后覆盖屏幕。
    let fb_bytes = info.fb_height as u64 * info.fb_stride as u64 * (info.fb_bpp as u64 / 8);
    let fb_start_frame = info.fb_addr / FRAME_SIZE as u64;
    let fb_end_frame = (info.fb_addr + fb_bytes).div_ceil(FRAME_SIZE as u64);

    // 帧缓冲是**内核保留**区间: 既在下面被排除出空闲集, 也要挡住运行期误释放
    // (`SYS_FB_MAP` 不走引用计数, 「同域重启」清地址空间时会想归还它)。
    pin_range(info.fb_addr, info.fb_addr + fb_bytes);

    // 白名单策略: 全部标记为占用
    unsafe {
        FRAME_BITMAP.fill(0xFF);
        TOTAL_FRAMES = 0;
        FREE_FRAMES = 0;
    }

    let mut total = 0usize;
    let mut free = 0usize;

    for i in 0..count {
        let desc = unsafe { &*((mmap_addr + i * entry_size) as *const MemoryDescriptor) };
        if desc.ty != MEMORY_CONVENTIONAL {
            continue;
        }

        let start = desc.phys_start;
        let end = start + desc.page_count * FRAME_SIZE as u64;

        for frame_addr in (start..end).step_by(FRAME_SIZE) {
            // 越过可管理的物理地址上限
            let idx = (frame_addr / FRAME_SIZE as u64) as usize;
            if idx >= MAX_MANAGED_FRAMES {
                break;
            }
            // 保留低内存与内核镜像本身
            if (frame_addr as usize) < kernel_end {
                continue;
            }
            // 保留帧缓冲占用的帧, 防止被分配后覆盖屏幕
            if (idx as u64) >= fb_start_frame && (idx as u64) < fb_end_frame {
                continue;
            }
            bitmap_clear(idx);
            total += 1;
            free += 1;
        }
    }

    unsafe {
        TOTAL_FRAMES = total;
        FREE_FRAMES = free;
    }

    // 保留当前活动页表 (引导器/UEFI 遗留) 占用的物理帧。
    reserve_active_page_tables();
}

/// 保留当前活动页表层级引用的物理帧。
///
/// 内核在 `paging::setup_page_tables` 加载自己的 CR3 之前, 仍运行在引导器遗留的
/// 页表下。这些页表所在的物理帧在 UEFI 内存图中可能已是 CONVENTIONAL (引导服务已退出),
/// 因而被当作空闲帧。若把它们分配出去并清零 (例如分配作新 PML4), 会立刻摧毁正在生效的
/// 地址翻译, 触发取指 #PF → #DF → triple fault (表现为启动到 paging::init 即崩溃,
/// 且随内核镜像大小变化时有时无)。故在此把 CR3 页表层级引用的帧全部标记为已占用。
///
/// 仅需保留「页表帧」本身: 大页 (1 GiB / 2 MiB) 映射的目标区域不属于地址翻译结构,
/// 被分配不会破坏翻译。
fn reserve_active_page_tables() {
    let (pml4_frame, _) = Cr3::read();
    let pml4 = pml4_frame.start_address().as_u64() as usize;
    reserve_frame(pml4);

    unsafe {
        let p4 = pml4 as *const usize;
        for i in 0..512 {
            let e4 = *p4.add(i);
            if e4 & 1 == 0 {
                continue;
            }
            let pdpt = e4 & PTE_ADDR_MASK;
            reserve_frame(pdpt);
            let p3 = pdpt as *const usize;
            for j in 0..512 {
                let e3 = *p3.add(j);
                if e3 & 1 == 0 || e3 & PTE_HUGE != 0 {
                    continue; // 1 GiB 大页
                }
                let pd = e3 & PTE_ADDR_MASK;
                reserve_frame(pd);
                let p2 = pd as *const usize;
                for k in 0..512 {
                    let e2 = *p2.add(k);
                    if e2 & 1 == 0 || e2 & PTE_HUGE != 0 {
                        continue; // 2 MiB 大页
                    }
                    reserve_frame(e2 & PTE_ADDR_MASK);
                }
            }
        }
    }
}

/// 把单个物理帧标记为已占用 (若原本空闲则同步递减空闲计数)。
fn reserve_frame(addr: usize) {
    let idx = addr / FRAME_SIZE;
    if idx < MAX_MANAGED_FRAMES && !bitmap_test(idx) {
        bitmap_set(idx);
        unsafe {
            FREE_FRAMES = FREE_FRAMES.saturating_sub(1);
        }
    }
}

/// 分配一个空闲物理帧, 返回其物理地址。
pub fn allocate_frame() -> Option<u64> {
    for idx in 0..MAX_MANAGED_FRAMES {
        if !bitmap_test(idx) {
            bitmap_set(idx);
            unsafe {
                FREE_FRAMES -= 1;
            }
            return Some((idx * FRAME_SIZE) as u64);
        }
    }
    None
}

/// 分配连续 `count` 个物理帧 (物理连续、页对齐), 返回首帧物理地址。
///
/// 用于 DMA 缓冲 / NVMe 队列: 设备要求队列与数据缓冲物理连续且页对齐。
/// 位图线性扫描, 找一段连续空闲 run; 找不到返回 `None`。
pub fn allocate_frames(count: usize) -> Option<u64> {
    if count == 0 {
        return None;
    }
    let mut run = 0usize;
    let mut run_start = 0usize;
    for idx in 0..MAX_MANAGED_FRAMES {
        if !bitmap_test(idx) {
            if run == 0 {
                run_start = idx;
            }
            run += 1;
            if run == count {
                for i in run_start..run_start + count {
                    bitmap_set(i);
                }
                unsafe {
                    FREE_FRAMES -= count;
                }
                return Some((run_start * FRAME_SIZE) as u64);
            }
        } else {
            run = 0;
        }
    }
    None
}

/// 释放一个物理帧。
pub fn free_frame(addr: u64) {
    // 内核保留区间 (帧缓冲) 永不释放 —— 见 `PINNED_RANGES`。
    if is_pinned(addr) {
        return;
    }
    let idx = (addr / FRAME_SIZE as u64) as usize;
    if idx < MAX_MANAGED_FRAMES && bitmap_test(idx) {
        bitmap_clear(idx);
        unsafe {
            FREE_FRAMES += 1;
        }
    }
}

// ---------------------------------------------------------------------------
// 共享帧引用计数
// ---------------------------------------------------------------------------
// 记录「用户显式申请 / 共享」的帧及其被映射的域数, 用于在解除映射时判断
// 何时真正释放。仅覆盖 SYS_ALLOC_PAGE / SYS_SHARE_PAGE / SYS_UNMAP 这条路径;
// 镜像页与栈帧 (加载器直接分配) 不登记 —— 域销毁时按 `release_user_frame`
// 的规则处理: 登记过的按计数递减, 未登记的视为该域独占直接释放。
const MAX_SHARED_FRAMES: usize = 64;

static mut SHARED_FRAMES: [(u64, u8); MAX_SHARED_FRAMES] = [(0, 0); MAX_SHARED_FRAMES];

/// 增加某物理帧的引用计数 (已存在则递增, 否则插入新槽位)。
pub fn inc_ref(addr: u64) {
    unsafe {
        for slot in SHARED_FRAMES.iter_mut() {
            if slot.0 == addr && slot.1 > 0 {
                slot.1 += 1;
                return;
            }
        }
        for slot in SHARED_FRAMES.iter_mut() {
            if slot.1 == 0 {
                slot.0 = addr;
                slot.1 = 1;
                return;
            }
        }
    }
}

/// 减少某物理帧的引用计数; 返回是否降为 0 (即应真正释放)。
pub fn dec_ref(addr: u64) -> bool {
    unsafe {
        for slot in SHARED_FRAMES.iter_mut() {
            if slot.0 == addr && slot.1 > 0 {
                slot.1 -= 1;
                if slot.1 == 0 {
                    slot.0 = 0;
                    return true;
                }
                return false;
            }
        }
    }
    false
}

/// 该物理帧是否**登记过**引用计数 (即经 `SYS_ALLOC_PAGE` / `SYS_SHARE_PAGE` 而来)。
pub fn is_tracked(addr: u64) -> bool {
    unsafe {
        SHARED_FRAMES
            .iter()
            .any(|slot| slot.0 == addr && slot.1 > 0)
    }
}

/// 归还一个「由某个域的用户空间持有」的物理帧 —— 域销毁 / 摘除映射时逐页调用。
///
/// 记账规则 (就这一条, 与 `SYS_ALLOC_PAGE`/`SYS_SHARE_PAGE` 的分工配套):
///
/// - **登记过引用计数**的帧 (用户显式申请或共享出去的): 按计数递减, **归零**才真正
///   `free_frame` —— 还有别的域把它映射在自己的地址空间里时不能释放;
/// - **未登记**的帧 (镜像页 / 用户栈帧: 由加载器直接 `allocate_frame` + 映射, 从不经
///   syscall): 归该域独占, 直接释放。
///
/// 半个前提: 同一物理帧在**同一个域**里只会被映射到一个虚拟地址 (`SYS_ALLOC_PAGE`
/// 对已映射地址返回 0, `SYS_SHARE_PAGE` 的目标域不是本域), 所以"一页一扫"不会重复计数。
pub fn release_user_frame(addr: u64) {
    if is_tracked(addr) {
        if dec_ref(addr) {
            free_frame(addr);
        }
    } else {
        free_frame(addr);
    }
}

/// 已管理 (空闲 + 已分配) 的帧总数。
pub fn total_frames() -> usize {
    unsafe { TOTAL_FRAMES }
}

/// 当前空闲帧数。
pub fn free_frames() -> usize {
    unsafe { FREE_FRAMES }
}

/// 可用物理内存总字节数。
pub fn total_memory_bytes() -> u64 {
    (total_frames() * FRAME_SIZE) as u64
}

/// 空闲物理内存字节数。
pub fn free_memory_bytes() -> u64 {
    (free_frames() * FRAME_SIZE) as u64
}

/// 打印内存统计信息到屏幕。
pub fn print_stats() {
    crate::video::println("[OK] Physical frame allocator initialized");
    crate::video::print("  Managed frames: ");
    crate::video::print_u64(total_frames() as u64);
    crate::video::println("");
    crate::video::print("  Free frames:    ");
    crate::video::print_u64(free_frames() as u64);
    crate::video::println("");
    crate::video::print("  Total memory:   ");
    crate::video::print_u64(total_memory_bytes());
    crate::video::println(" bytes");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 域销毁逐页归还时遵守「谁拥有谁释放」的记账规则。
    ///
    /// 单测跑在宿主上 (位图未初始化), 故不依赖真实内存: 用 `reserve_frame` 把待测帧
    /// 标记为"已占用", 以位图是否翻转判定"有没有被真正释放"。
    /// 断言全放在一个测试里 —— 它们共享全局位图与引用计数表, 拆开会被并行执行互相干扰。
    #[test]
    fn release_user_frame_follows_ownership_rule() {
        // (1) 未登记的帧 (镜像页 / 用户栈帧): 本域独占 → 直接释放。
        let fresh: u64 = 7 * FRAME_SIZE as u64;
        reserve_frame(fresh as usize);
        assert!(bitmap_test(7));
        assert!(!is_tracked(fresh));
        release_user_frame(fresh);
        assert!(!bitmap_test(7), "未登记的帧应被释放");

        // (2) 登记过的帧 (alloc_page / share_page): 计数递减, 归零才释放。
        let shared: u64 = 11 * FRAME_SIZE as u64;
        reserve_frame(shared as usize);
        inc_ref(shared); // 本域 alloc_page
        inc_ref(shared); // 又共享给了另一个域
        assert!(is_tracked(shared));
        release_user_frame(shared);
        assert!(bitmap_test(11), "还有别的域映射着它, 不能释放");
        assert!(is_tracked(shared));
        release_user_frame(shared);
        assert!(!bitmap_test(11), "计数归零后应释放");
        assert!(!is_tracked(shared));
    }
}
