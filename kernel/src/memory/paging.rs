//! 虚拟内存管理 (阶段三) — 4 级页表 + offset 映射 + 内核堆
//!
//! 建立方式:
//!   1. 手动构造初始页表 (2 MiB 大页), 将物理内存前 4 GiB 同时映射到:
//!        - 恒等映射 (P4[0])  : 虚拟地址 == 物理地址, 供现有代码 / 帧缓冲使用
//!        - offset 映射 (P4[256]): 虚拟地址 = PHYS_OFFSET + 物理地址, 供页表自身访问
//!   2. 加载 CR3
//!   3. 用 OffsetPageTable 把内核堆映射到高位虚拟地址, 并初始化全局分配器

use linked_list_allocator::LockedHeap;
use x86_64::registers::control::Cr3;
use x86_64::structures::paging::{
    FrameAllocator, Mapper, OffsetPageTable, Page, PageTable, PageTableFlags, PhysFrame, Size4KiB,
};
use x86_64::{PhysAddr, VirtAddr};

use crate::memory::frame_allocator;

/// 物理内存 offset 映射的虚拟地址偏移
pub const PHYS_OFFSET: u64 = 0xFFFF_8000_0000_0000;

/// 用户空间基址 (P4[1], 512 GiB), 与内核的恒等/offset 映射分离。
pub const USER_SPACE_BASE: u64 = 0x0000_0080_0000_0000;

/// 内核堆起始虚拟地址 (未使用的上半区地址)
const HEAP_START: u64 = 0x4444_4444_0000;
/// 内核堆大小
const HEAP_SIZE: usize = 256 * 1024; // 256 KiB

/// 可管理的物理内存上限 (前 4 GiB, 覆盖 QEMU 2 GiB 内存)
const MANAGED_MEMORY: u64 = 4 * 1024 * 1024 * 1024;

/// 帧分配器适配 — 桥接位图分配器与 x86_64 crate 的 FrameAllocator trait
struct KernelFrameAllocator;

unsafe impl FrameAllocator<Size4KiB> for KernelFrameAllocator {
    fn allocate_frame(&mut self) -> Option<PhysFrame<Size4KiB>> {
        frame_allocator::allocate_frame()
            .map(|addr| PhysFrame::containing_address(PhysAddr::new(addr)))
    }
}

/// 全局内核堆分配器
#[global_allocator]
static ALLOCATOR: LockedHeap = LockedHeap::empty();

/// 初始化分页 (建立页表 + 加载 CR3 + 初始化内核堆)
pub fn init() {
    let pml4_phys = setup_page_tables();
    load_cr3(pml4_phys);
    init_heap(pml4_phys);
}

/// 手动构造初始页表, 返回 PML4 的物理地址。
///
/// 此时 CPU 仍运行在 UEFI 的恒等映射下, 物理地址可直接作为虚拟地址访问。
fn setup_page_tables() -> PhysAddr {
    // 分配页表帧: 1 PML4 + 1 PDPT + 4 PD (每个 PD 用 2 MiB 大页覆盖 1 GiB)
    let pml4_phys = frame_allocator::allocate_frame().expect("allocate PML4");
    let pdpt_phys = frame_allocator::allocate_frame().expect("allocate PDPT");
    let pd_count = (MANAGED_MEMORY / (512 * 0x20_0000)) as usize; // 4 GiB → 4 个 PD
    let mut pd_phys = [0u64; 4];
    for (i, slot) in pd_phys.iter_mut().enumerate().take(pd_count) {
        *slot = frame_allocator::allocate_frame().expect("allocate PD");
        let _ = i;
    }

    // 零填充页表帧
    unsafe {
        core::ptr::write_bytes(pml4_phys as *mut u8, 0, 4096);
        core::ptr::write_bytes(pdpt_phys as *mut u8, 0, 4096);
        for &p in &pd_phys {
            if p != 0 {
                core::ptr::write_bytes(p as *mut u8, 0, 4096);
            }
        }
    }

    let flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE;

    // P4[0] (恒等) 与 P4[256] (offset) 都指向同一 PDPT
    let pml4 = unsafe { &mut *(pml4_phys as *mut PageTable) };
    pml4[0].set_frame(frame(pdpt_phys), flags);
    pml4[256].set_frame(frame(pdpt_phys), flags);

    // PDPT[i] → PD[i] (覆盖第 i 个 1 GiB)
    let pdpt = unsafe { &mut *(pdpt_phys as *mut PageTable) };
    for (i, &pd_p) in pd_phys.iter().enumerate() {
        if pd_p != 0 {
            pdpt[i].set_frame(frame(pd_p), flags);
        }
    }

    // PD[i][j] → 2 MiB 大页 (物理地址 i*1GiB + j*2MiB)
    for (i, &pd_p) in pd_phys.iter().enumerate() {
        if pd_p == 0 {
            continue;
        }
        let pd = unsafe { &mut *(pd_p as *mut PageTable) };
        for j in 0..512usize {
            let phys = (i as u64) * 0x4000_0000 + (j as u64) * 0x20_0000;
            pd[j].set_addr(PhysAddr::new(phys), flags | PageTableFlags::HUGE_PAGE);
        }
    }

    PhysAddr::new(pml4_phys)
}

/// 加载新页表到 CR3。
fn load_cr3(pml4_phys: PhysAddr) {
    let (_, flags) = Cr3::read();
    unsafe {
        Cr3::write(PhysFrame::containing_address(pml4_phys), flags);
    }
}

/// 映射内核堆到高位虚拟地址并初始化全局分配器。
fn init_heap(pml4_phys: PhysAddr) {
    // 通过 offset 映射访问 PML4, 构造 OffsetPageTable
    let pml4_virt = (PHYS_OFFSET + pml4_phys.as_u64()) as *mut PageTable;
    let mut mapper = unsafe { OffsetPageTable::new(&mut *pml4_virt, VirtAddr::new(PHYS_OFFSET)) };

    let heap_start = VirtAddr::new(HEAP_START);
    let heap_end = heap_start + HEAP_SIZE as u64 - 1u64;
    let start_page = Page::<Size4KiB>::containing_address(heap_start);
    let end_page = Page::<Size4KiB>::containing_address(heap_end);

    let mut allocator = KernelFrameAllocator;
    for page in Page::range_inclusive(start_page, end_page) {
        let frame = allocator.allocate_frame().expect("allocate heap frame");
        let flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE;
        unsafe {
            mapper
                .map_to(page, frame, flags, &mut allocator)
                .expect("map heap page")
                .flush();
        }
    }

    unsafe {
        let heap_bottom = HEAP_START as *mut u8;
        ALLOCATOR.lock().init(heap_bottom, HEAP_SIZE);
    }
}

/// 物理地址 → PhysFrame (Size4KiB) 辅助。
fn frame(addr: u64) -> PhysFrame<Size4KiB> {
    PhysFrame::containing_address(PhysAddr::new(addr))
}

/// 内核堆起始虚拟地址 (供日志等查询)。
pub fn heap_start() -> usize {
    HEAP_START as usize
}

/// 内核堆大小 (字节)。
pub fn heap_size() -> usize {
    HEAP_SIZE
}

/// 在指定域的页表中, 把用户虚拟地址 `vaddr` 映射到物理帧 `paddr` (USER 权限)。
///
/// 用户空间使用独立的 PML4 条目 (P4[1], 基址见 `USER_SPACE_BASE`), 不干扰内核的
/// 恒等/offset 映射。中间页表 (PDPT/PD/PT) 缺失时自动分配并清零。
pub fn map_user_page(domain_id: u64, vaddr: u64, paddr: u64) {
    let pml4 = crate::domain::pml4_of(domain_id);
    // 通过 offset 映射访问目标域的 PML4。
    let pml4_virt = (PHYS_OFFSET + pml4) as *mut PageTable;
    let mut mapper = unsafe { OffsetPageTable::new(&mut *pml4_virt, VirtAddr::new(PHYS_OFFSET)) };

    let page = Page::<Size4KiB>::containing_address(VirtAddr::new(vaddr));
    let frame = PhysFrame::containing_address(PhysAddr::new(paddr));
    let flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::USER_ACCESSIBLE;

    let mut allocator = KernelFrameAllocator;
    unsafe {
        mapper
            .map_to(page, frame, flags, &mut allocator)
            .expect("map_user_page: map failed")
            .flush();
    }
}

/// 把物理 MMIO 区域 `paddr` (页对齐) 映射到指定域的 `vaddr` (USER + 非缓存)。
///
/// 与 `map_user_page` 的区别在于额外置 `NO_CACHE` (PCD), 避免 CPU 缓存
/// 设备寄存器读写。用于 NVMe 等 MMIO 设备 BAR 的映射。
pub fn map_mmio(domain_id: u64, vaddr: u64, paddr: u64) {
    let pml4 = crate::domain::pml4_of(domain_id);
    let pml4_virt = (PHYS_OFFSET + pml4) as *mut PageTable;
    let mut mapper = unsafe { OffsetPageTable::new(&mut *pml4_virt, VirtAddr::new(PHYS_OFFSET)) };

    let page = Page::<Size4KiB>::containing_address(VirtAddr::new(vaddr));
    let frame = PhysFrame::containing_address(PhysAddr::new(paddr));
    let flags = PageTableFlags::PRESENT
        | PageTableFlags::WRITABLE
        | PageTableFlags::USER_ACCESSIBLE
        | PageTableFlags::NO_CACHE;

    let mut allocator = KernelFrameAllocator;
    unsafe {
        mapper
            .map_to(page, frame, flags, &mut allocator)
            .expect("map_mmio: map failed")
            .flush();
    }
}

/// 遍历指定域的 4 级页表, 把用户虚拟地址 `vaddr` 反查为物理地址。
///
/// 用户空间映射使用 4 KiB 页 (由 `map_user_page` 建立), 故按 PML4 → PDPT →
/// PD → PT 逐级解析; 兼容 2 MiB 大页 (返回大页基址 + 页内偏移)。
pub fn resolve_user_page(domain_id: u64, vaddr: u64) -> Option<u64> {
    let pml4 = crate::domain::pml4_of(domain_id);
    let pml4_virt = (PHYS_OFFSET + pml4) as *mut PageTable;
    let pml4 = unsafe { &*pml4_virt };

    let p4 = ((vaddr >> 39) & 0x1FF) as usize;
    let p3 = ((vaddr >> 30) & 0x1FF) as usize;
    let p2 = ((vaddr >> 21) & 0x1FF) as usize;
    let p1 = ((vaddr >> 12) & 0x1FF) as usize;

    let pml4e = &pml4[p4];
    if !pml4e.flags().contains(PageTableFlags::PRESENT) {
        return None;
    }
    let pdpt = unsafe { &*((PHYS_OFFSET + pml4e.addr().as_u64()) as *mut PageTable) };

    let pdpte = &pdpt[p3];
    if !pdpte.flags().contains(PageTableFlags::PRESENT) {
        return None;
    }
    let pd = unsafe { &*((PHYS_OFFSET + pdpte.addr().as_u64()) as *mut PageTable) };

    let pde = &pd[p2];
    if !pde.flags().contains(PageTableFlags::PRESENT) {
        return None;
    }
    if pde.flags().contains(PageTableFlags::HUGE_PAGE) {
        return Some(pde.addr().as_u64() + (vaddr & 0x1F_FFFF));
    }
    let pt = unsafe { &*((PHYS_OFFSET + pde.addr().as_u64()) as *mut PageTable) };

    let pte = &pt[p1];
    if !pte.flags().contains(PageTableFlags::PRESENT) {
        return None;
    }
    Some(pte.addr().as_u64() + (vaddr & 0xFFF))
}

/// 解除指定域中 `vaddr` 的用户页映射, 返回被解映射的物理帧地址。
///
/// 调用方负责在返回的帧上做引用计数 / 释放处理。
pub fn unmap_user_page(domain_id: u64, vaddr: u64) -> Option<u64> {
    let pml4 = crate::domain::pml4_of(domain_id);
    let pml4_virt = (PHYS_OFFSET + pml4) as *mut PageTable;
    let mut mapper = unsafe { OffsetPageTable::new(&mut *pml4_virt, VirtAddr::new(PHYS_OFFSET)) };

    let page = Page::<Size4KiB>::containing_address(VirtAddr::new(vaddr));
    let (frame, flush) = mapper.unmap(page).ok()?;
    let paddr = frame.start_address().as_u64();
    flush.flush();
    Some(paddr)
}
