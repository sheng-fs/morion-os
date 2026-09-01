//! 物理帧分配器 — 位图实现
//!
//! 每个位代表一个 4 KiB 物理帧; 1 = 已占用, 0 = 空闲。
//! 采用白名单策略: 初始化时全部标记为占用, 仅将内存图中
//! `CONVENTIONAL` 且位于内核镜像之后的帧标记为空闲。

use crate::bootinfo::{BootInfo, MemoryDescriptor, MEMORY_CONVENTIONAL};
use spin::Mutex;

/// 帧大小 (4 KiB)
pub const FRAME_SIZE: usize = 4096;

/// 位图容量: 1 MiB = 8 Mi 帧 = 32 GiB 物理内存上限
const BITMAP_SIZE: usize = 1024 * 1024;
const MAX_MANAGED_FRAMES: usize = BITMAP_SIZE * 8;

static mut FRAME_BITMAP: [u8; BITMAP_SIZE] = [0; BITMAP_SIZE];
static mut TOTAL_FRAMES: usize = 0;
static mut FREE_FRAMES: usize = 0;

/// 保护位图与引用计数表的自旋锁 (须在关中断或原子上下文中使用)。
static FRAME_LOCK: Mutex<()> = Mutex::new(());

// 链接脚本导出的内核镜像结束地址
extern "C" {
    static _kernel_end: u8;
}

#[inline]
fn bitmap_set(idx: usize) {
    unsafe { FRAME_BITMAP[idx / 8] |= 1u8 << (idx % 8); }
}

#[inline]
fn bitmap_clear(idx: usize) {
    unsafe { FRAME_BITMAP[idx / 8] &= !(1u8 << (idx % 8)); }
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
    let kernel_end = unsafe { &_kernel_end as *const u8 as usize };

    // 帧缓冲占用的物理帧区间 (按页对齐), 防止被当作空闲帧分配后覆盖屏幕。
    let fb_bytes = info.fb_height as u64 * info.fb_stride as u64 * (info.fb_bpp as u64 / 8);
    let fb_start_frame = info.fb_addr / FRAME_SIZE as u64;
    let fb_end_frame = (info.fb_addr + fb_bytes + FRAME_SIZE as u64 - 1) / FRAME_SIZE as u64;

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
}

/// 分配一个空闲物理帧, 返回其物理地址。
pub fn allocate_frame() -> Option<u64> {
    let _guard = FRAME_LOCK.lock();
    for idx in 0..MAX_MANAGED_FRAMES {
        if !bitmap_test(idx) {
            bitmap_set(idx);
            unsafe { FREE_FRAMES -= 1; }
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
    let _guard = FRAME_LOCK.lock();
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
                unsafe { FREE_FRAMES -= count; }
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
    let _guard = FRAME_LOCK.lock();
    let idx = (addr / FRAME_SIZE as u64) as usize;
    if idx < MAX_MANAGED_FRAMES && bitmap_test(idx) {
        bitmap_clear(idx);
        unsafe { FREE_FRAMES += 1; }
    }
}

// ---------------------------------------------------------------------------
// 共享帧引用计数
// ---------------------------------------------------------------------------
// 记录「用户显式申请 / 共享」的帧及其被映射的域数, 用于在解除映射时判断
// 何时真正释放。仅覆盖 SYS_ALLOC_PAGE / SYS_SHARE_PAGE / SYS_UNMAP 这条路径;
// 镜像页与栈帧 (load_user_program) 不纳入, 其生命周期随任务。
const MAX_SHARED_FRAMES: usize = 64;

static mut SHARED_FRAMES: [(u64, u16); MAX_SHARED_FRAMES] = [(0, 0); MAX_SHARED_FRAMES];

/// 增加某物理帧的引用计数 (已存在则递增, 否则插入新槽位)。
/// 返回 `false` 表示引用计数表已满或溢出, 调用方应回滚已分配的帧。
pub fn inc_ref(addr: u64) -> bool {
    let _guard = FRAME_LOCK.lock();
    unsafe {
        for slot in SHARED_FRAMES.iter_mut() {
            if slot.0 == addr && slot.1 > 0 {
                if slot.1 < u16::MAX {
                    slot.1 += 1;
                    return true;
                }
                return false; // 溢出保护
            }
        }
        for slot in SHARED_FRAMES.iter_mut() {
            if slot.1 == 0 {
                slot.0 = addr;
                slot.1 = 1;
                return true;
            }
        }
    }
    false // 表已满
}

/// 减少某物理帧的引用计数; 返回是否降为 0 (即应真正释放)。
pub fn dec_ref(addr: u64) -> bool {
    let _guard = FRAME_LOCK.lock();
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
