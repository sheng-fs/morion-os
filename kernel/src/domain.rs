//! 保护域 (Domain) — 地址空间隔离的基础抽象
//!
//! 微内核核心原语 · 第 3 小步。
//!
//! 每个域拥有独立的页表 (PML4)。创建时复制当前内核页表的非空条目,
//! 从而共享内核空间映射 (恒等 + offset + 内核堆), 保证内核代码在所有
//! 域中均可运行; 域私有的用户空间映射将在后续步骤建立。

use alloc::vec::Vec;
use spin::Mutex;
use x86_64::registers::control::Cr3;
use x86_64::structures::paging::PageTable;

use crate::memory::frame_allocator;

/// 用户空间在 PML4 里的下标 (P4[1], 见 `paging::USER_SPACE_BASE`)。
const USER_SPACE_P4_INDEX: usize = 1;

/// 保护域。
pub struct Domain {
    pub id: u64,
    /// 该域页表根 (PML4) 的物理地址。
    pub pml4: u64,
}

/// 全局域表 (按 id 索引)。
static DOMAINS: Mutex<Vec<Domain>> = Mutex::new(Vec::new());

/// 创建一个新保护域, 返回其 id。
pub fn create() -> u64 {
    let mut domains = DOMAINS.lock();
    let id = domains.len() as u64;
    domains.push(Domain::new(id));
    id
}

/// 查询指定域的 PML4 物理地址。
pub fn pml4_of(id: u64) -> u64 {
    let domains = DOMAINS.lock();
    domains[id as usize].pml4
}

impl Domain {
    fn new(id: u64) -> Self {
        let pml4 = frame_allocator::allocate_frame().expect("allocate domain PML4");
        unsafe { core::ptr::write_bytes(pml4 as *mut u8, 0, 4096) };

        // 复制当前内核 PML4 的**内核空间**条目, 共享内核映射 (代码 / 恒等 / offset / 内核堆)。
        //
        // ⚠️ 必须跳过 P4[1] (= 用户空间, 见 `paging::is_user_address`): 引导期建域时它还是空的,
        // 但**运行时**(`SYS_SPAWN_ELF` 在新域的创建发生在调用者的 syscall 里, CR3 = 调用者的 PML4)
        // 若照抄过去, 新域就会与调用者**共用同一棵用户空间页表** —— 既没有地址空间隔离,
        // 映射新程序时还会与调用者自己的镜像撞车 (`PageAlreadyMapped` panic)。
        // 新域的用户空间必须从零开始, 由加载器 (exec::spawn_elf) 一页页建立。
        let (kernel_frame, _) = Cr3::read();
        let src = kernel_frame.start_address().as_u64() as *const PageTable;
        let dst = pml4 as *mut PageTable;
        unsafe {
            let src_ref = &*src;
            let dst_ref = &mut *dst;
            for i in 0..512 {
                if i == USER_SPACE_P4_INDEX {
                    continue;
                }
                if !src_ref[i].is_unused() {
                    dst_ref[i] = src_ref[i].clone();
                }
            }
        }

        Self { id, pml4 }
    }
}
