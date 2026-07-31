//! 物理内存管理 — 页帧分配器
//!
//! 使用位图 (Bitmap) 管理 4KB 页帧。
//! 每个位代表一个页帧, 1 = 已用, 0 = 空闲。
//! 后续可升级为 Buddy 分配器以提高连续页分配效率。

use crate::boot::{MemoryDescriptor, MemoryType};

/// 位图容量: 每个 u64 覆盖 64 个页帧 (256KB)
/// 当前配置支持最多 8GB 物理内存
const BITMAP_WORDS: usize = 32768; // 32K words × 64 bits × 4KB = 8GB
static mut BITMAP: [u64; BITMAP_WORDS] = [0; BITMAP_WORDS];

/// 已追踪的总页帧数
static TOTAL_FRAMES: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// 分配搜索起点 (避免每次从头扫描)
static NEXT_HINT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// 初始化物理内存管理器
///
/// 遍历 UEFI 内存映射:
///   1. 计算最大物理地址, 确定位图覆盖范围
///   2. 默认将所有帧标记为已用
///   3. 将 Conventional / BootServices 类型内存标记为可用
pub fn init(memory_map: &[MemoryDescriptor]) {
    // 第一遍: 计算最大物理地址
    let mut max_page = 0u64;
    for desc in memory_map {
        let end = desc
            .physical_start
            .saturating_add(desc.number_of_pages.saturating_mul(4096));
        let last_page = end.saturating_sub(1) / 4096;
        if last_page > max_page {
            max_page = last_page;
        }
    }

    let total_frames = (max_page + 1) as usize;
    assert!(
        total_frames <= BITMAP_WORDS * 64,
        "物理内存超出位图容量限制 (当前: 8GB)"
    );
    TOTAL_FRAMES.store(total_frames as u64, core::sync::atomic::Ordering::Release);

    // 初始化位图: 全部标记为已用
    unsafe {
        for word in BITMAP.iter_mut() {
            *word = u64::MAX;
        }
    }

    // 第二遍: 将可用内存类型标记为空闲
    // BootServices 类型在 ExitBootServices 后可以回收
    let usable_types = [
        MemoryType::Conventional,
        MemoryType::BootServicesCode,
        MemoryType::BootServicesData,
    ];

    for desc in memory_map {
        if usable_types.contains(&desc.ty) {
            let start_frame = (desc.physical_start / 4096) as usize;
            let num_frames = desc.number_of_pages as usize;
            unsafe {
                mark_range_free(start_frame, num_frames);
            }
        }
    }

    // TODO: 标记内核自身占用的物理帧为已用
    // 需要链接脚本导出 _kernel_start / _kernel_end 符号
    // unsafe {
    //     let kernel_start = ... / 4096;
    //     let kernel_end = (... + 4095) / 4096;
    //     mark_range_used(kernel_start, kernel_end - kernel_start);
    // }
}

/// 分配一个 4KB 物理页帧
///
/// 搜索策略: 从上次分配位置向后查找, 提高局部性。
/// 返回物理地址, 失败返回 None。
pub fn alloc_frame() -> Option<u64> {
    let total = TOTAL_FRAMES.load(core::sync::atomic::Ordering::Acquire) as usize;
    if total == 0 {
        return None;
    }
    let hint = NEXT_HINT.load(core::sync::atomic::Ordering::Relaxed) as usize;

    unsafe {
        // 从 hint 位置向后搜索
        let (found, idx) = find_free_frame(hint, total);
        if found {
            let frame = idx as usize;
            mark_frame_used(frame);
            NEXT_HINT.store(
                (idx + 1).min(total as u64).wrapping_sub(1),
                core::sync::atomic::Ordering::Relaxed,
            );
            return Some(frame as u64 * 4096);
        }

        // 回绕: 从 0 搜索到 hint
        let (found, idx) = find_free_frame(0, hint);
        if found {
            let frame = idx as usize;
            mark_frame_used(frame);
            NEXT_HINT.store(
                (idx + 1).min(total as u64).wrapping_sub(1),
                core::sync::atomic::Ordering::Relaxed,
            );
            return Some(frame as u64 * 4096);
        }
    }

    None
}

/// 释放一个 4KB 物理页帧
pub fn free_frame(addr: u64) {
    let frame = (addr / 4096) as usize;
    let total = TOTAL_FRAMES.load(core::sync::atomic::Ordering::Acquire) as usize;
    if frame < total {
        unsafe {
            mark_frame_free(frame);
        }
    }
}

// ============================================================
// 内部辅助函数
// ============================================================

/// 在位图 [from, to) 范围内查找空闲页帧
unsafe fn find_free_frame(from: usize, to: usize) -> (bool, u64) {
    let start_word = from / 64;
    let end_word = (to + 63) / 64;

    for word_idx in start_word..end_word.min(BITMAP_WORDS) {
        let word = BITMAP[word_idx];
        if word != u64::MAX {
            let bit_offset = if word_idx == start_word {
                from % 64
            } else {
                0
            };
            for bit in bit_offset..64 {
                let global_idx = word_idx * 64 + bit;
                if global_idx >= to {
                    return (false, 0);
                }
                if word & (1 << bit) == 0 {
                    return (true, global_idx as u64);
                }
            }
        }
    }
    (false, 0)
}

unsafe fn mark_frame_used(frame: usize) {
    BITMAP[frame / 64] |= 1 << (frame % 64);
}

unsafe fn mark_frame_free(frame: usize) {
    BITMAP[frame / 64] &= !(1 << (frame % 64));
}

unsafe fn mark_range_free(start: usize, count: usize) {
    for i in 0..count {
        mark_frame_free(start + i);
    }
}
