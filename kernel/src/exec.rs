//! 运行时加载可执行文件并启动 —— `SYS_SPAWN_ELF` 的落点
//!
//! 一条完整链路: 解析校验 (`elf::parse`) → 建新域 → 逐段映射 (含 `.bss` 补零) →
//! 映射用户栈 → `spawn_user` 起一个 Ring 3 任务。新域**零能力**, 且它的分页器登记为
//! **调用者** —— 加载者就是这个程序的 loader（后续缺页交给它决定怎么补）。
//!
//! 与引导期的 `load_user_program` 相比, 这里加载的是**任意镜像**而不是编译期嵌入的
//! 那一份, 并且每个程序拿到自己的域/地址空间, 不再共享"同一份镜像 + 域 id 分流"。

use crate::elf;
use crate::memory::frame_allocator::{self, FRAME_SIZE};
use crate::memory::paging::{self, map_user_page, USER_STACK_PAGES, USER_STACK_TOP};

/// 一次加载允许占用的最大物理页数（防止一个坏镜像把内存吃光）。
pub const MAX_IMAGE_PAGES: u64 = 4096; // 16 MiB

/// 加载 `image` 里的 ELF 并启动它; 成功返回新域 id。
///
/// `loader` 是发起加载的域, 同时被登记为新域的分页器。任一环节失败返回 `None`,
/// 已建立的映射不回收（域销毁尚未实现, 见 roadmap「E1 未做」）。
pub fn spawn_elf(image: &[u8], loader: u64) -> Option<u64> {
    let elf = elf::parse(image)?;

    // 预算页数: 各段按页向上取整之和 + 栈, 超上限直接拒绝。
    let mut pages: u64 = USER_STACK_PAGES;
    for seg in elf.segments() {
        pages = pages.checked_add((seg.memsz / FRAME_SIZE as u64) + 1)?;
    }
    if pages > MAX_IMAGE_PAGES {
        return None;
    }

    // 建域, 并把各全局表补齐到新域（能力/邮箱/分页器都按域 id 索引）。
    let domain = crate::domain::create();
    crate::cap::add_domain();
    crate::ipc::add_domain();
    crate::pager::add_domain(loader);

    // 逐段映射: 一页只映射一次 —— 相邻段（如 .data 紧接 .text）常共享边界页,
    // 重复 map 会撞 `PageAlreadyMapped`。已映射的页直接复用其物理帧。
    for &seg in elf.segments() {
        let first = seg.vaddr & !(FRAME_SIZE as u64 - 1);
        let end = seg.vaddr + seg.memsz;
        let mut page_vaddr = first;
        while page_vaddr < end {
            let paddr = match paging::resolve_user_page(domain, page_vaddr) {
                Some(p) => p,
                None => {
                    let p = frame_allocator::allocate_frame()?;
                    // 清零: `.bss` 与段末尾的填充都依赖"新页为 0", 而分配器不保证内容。
                    // 物理地址 < 4 GiB 在恒等映射内, 可直接按虚拟地址写。
                    unsafe {
                        core::ptr::write_bytes(p as *mut u8, 0, FRAME_SIZE);
                    }
                    map_user_page(domain, page_vaddr, p);
                    p
                }
            };
            copy_segment_page(image, seg, page_vaddr, paddr);
            page_vaddr += FRAME_SIZE as u64;
        }
    }

    // 用户栈: 与引导期同一布局（所有程序共用一套链接地址）。
    let stack_base = USER_STACK_TOP - USER_STACK_PAGES * FRAME_SIZE as u64;
    for i in 0..USER_STACK_PAGES {
        let paddr = frame_allocator::allocate_frame()?;
        unsafe {
            core::ptr::write_bytes(paddr as *mut u8, 0, FRAME_SIZE);
        }
        map_user_page(domain, stack_base + i * FRAME_SIZE as u64, paddr);
    }

    // 起任务; 任务表满则失败（不 panic —— 这是用户可触发的路径）。
    if !crate::scheduler::try_spawn_user(elf.entry, USER_STACK_TOP, domain) {
        return None;
    }
    Some(domain)
}

/// 把 `seg` 落在 `page_vaddr` 这一页里的文件内容拷进物理帧 `paddr`。
///
/// 只拷 `[vaddr, vaddr+filesz)` 与这一页的交集; 其余保持 0（首次映射时已清零）。
fn copy_segment_page(image: &[u8], seg: elf::Segment, page_vaddr: u64, paddr: u64) {
    let file_end = seg.vaddr + seg.filesz;
    let page_end = page_vaddr + FRAME_SIZE as u64;
    let lo = core::cmp::max(page_vaddr, seg.vaddr);
    let hi = core::cmp::min(page_end, file_end);
    if lo >= hi {
        return; // 这一页没有文件内容（纯 .bss 部分）
    }
    let src_off = (seg.offset + (lo - seg.vaddr)) as usize;
    let dst_off = (lo - page_vaddr) as usize;
    let n = (hi - lo) as usize;
    unsafe {
        core::ptr::copy_nonoverlapping(
            image.as_ptr().add(src_off),
            (paddr as *mut u8).add(dst_off),
            n,
        );
    }
}
