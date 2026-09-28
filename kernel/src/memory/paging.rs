//! 虚拟内存管理 (阶段三) — 4 级页表 + offset 映射 + 内核堆
//!
//! 建立方式:
//!   1. 手动构造初始页表 (2 MiB 大页), 将物理内存前 4 GiB 同时映射到:
//!        - 恒等映射 (P4[0])  : 虚拟地址 == 物理地址, 供现有代码 / 帧缓冲使用
//!        - offset 映射 (P4[256]): 虚拟地址 = PHYS_OFFSET + 物理地址, 供页表自身访问
//!   2. 加载 CR3
//!   3. 用 OffsetPageTable 把内核堆映射到高位虚拟地址, 并初始化全局分配器

#[cfg(target_os = "none")]
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

/// 用户空间结束地址 (**开区间**), 即 P4[1] 的上界 —— 越界就等于跳到内核/未映射区域。
pub const USER_SPACE_END: u64 = USER_SPACE_BASE + 0x0000_0080_0000_0000;

/// 用户栈顶虚拟地址。所有用户程序共用同一套链接地址与固定布局 (见 `user/linker.ld`),
/// 故栈位置也是全局常量 —— 引导期加载 (`exec::spawn_elf_at`) 与运行时 ELF 加载
/// (`exec::spawn_elf`) 必须一致, 因此放在这里作为唯一来源。
pub const USER_STACK_TOP: u64 = USER_SPACE_BASE + 0x40_1000;

/// 用户栈页数 (自 `USER_STACK_TOP` 向下增长)。
pub const USER_STACK_PAGES: u64 = 8;

/// 内核堆起始虚拟地址 (未使用的上半区地址)
const HEAP_START: u64 = 0x4444_4444_0000;
/// 内核堆大小 (至少容纳 7 个 32 KiB 内核栈 + 调度器/IPC/Cap/分页器/分配器元数据)
///   7 × 32 KiB = 224 KiB 仅栈; 加上分配器头 + 容器扩容 + 域/任务结构, 含运行时创建的
///   域/任务后每任务仍要 32 KiB 内核栈 (任务表上限 32 → 约 1 MiB 仅栈), 故留 4 MiB。
const HEAP_SIZE: usize = 4 * 1024 * 1024; // 4 MiB

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

/// 全局内核堆分配器 (仅内核目标; host 单测由 libtest 的 std 分配器承担)。
#[cfg(target_os = "none")]
#[global_allocator]
static ALLOCATOR: LockedHeap = LockedHeap::empty();

/// 初始化分页 (建立页表 + 加载 CR3 + 初始化内核堆)
pub fn init() {
    enable_nx();
    let pml4_phys = setup_page_tables();
    load_cr3(pml4_phys);
    #[cfg(target_os = "none")]
    init_heap(pml4_phys);
}

/// 开启 `EFER.NXE` —— 页表项 NX (不可执行) 位生效的前提, W^X 依赖它。
///
/// 先查 CPUID 是否支持 NX: 长模式下普遍支持, 但不支持时 W^X 无法强制 ——
/// 明确告警而不是静默地把"以为不可执行"当成安全属性。
fn enable_nx() {
    use x86_64::registers::model_specific::{Efer, EferFlags};
    // CPUID.8000_0001H:EDX bit 20 = NX (No-Execute) 支持。
    let supported = core::arch::x86_64::__cpuid(0x8000_0001).edx & (1 << 20) != 0;
    if !supported {
        crate::video::println("[WARN] CPU 不支持 NX, 用户页 W^X 无法强制");
        return;
    }
    unsafe {
        Efer::update(|e| e.insert(EferFlags::NO_EXECUTE_ENABLE));
    }
}

/// 4 KiB 对齐的页表存储。放在内核镜像的 `.bss` 中, 由链接器保留 (帧分配器不会
/// 复用镜像内的帧), 因此启动页表与堆/栈/引导器页表都不可能互相覆盖。
///
/// 历史上启动页表是从帧分配器「镜像尾部相邻帧」里取的, 一旦镜像大小变化使该帧
/// 与内核栈顶或仍在使用的引导器页表重合, 就会在 `paging::init` 处 triple fault,
/// 且时有时无。改为静态存储后该类问题不再可能发生。
#[repr(C, align(4096))]
struct BootPageTable([u64; 512]);

static mut BOOT_PML4: BootPageTable = BootPageTable([0; 512]);
static mut BOOT_PDPT: BootPageTable = BootPageTable([0; 512]);
static mut BOOT_PDS: [BootPageTable; 4] = [
    BootPageTable([0; 512]),
    BootPageTable([0; 512]),
    BootPageTable([0; 512]),
    BootPageTable([0; 512]),
];

/// 手动构造初始页表, 返回 PML4 的物理地址。
///
/// 此时 CPU 仍运行在 UEFI 的恒等映射下, 物理地址可直接作为虚拟地址访问。
fn setup_page_tables() -> PhysAddr {
    // 启动页表位于 .bss (镜像内, 已被帧分配器保留), 不再向分配器申请。
    let pml4_phys = core::ptr::addr_of!(BOOT_PML4) as u64;
    let pdpt_phys = core::ptr::addr_of!(BOOT_PDPT) as u64;
    let pd_count = (MANAGED_MEMORY / (512 * 0x20_0000)) as usize; // 4 GiB → 4 个 PD
    let pds_base = core::ptr::addr_of!(BOOT_PDS) as *const BootPageTable;
    let mut pd_phys = [0u64; 4];
    for (i, slot) in pd_phys.iter_mut().enumerate().take(pd_count) {
        *slot = pds_base.wrapping_add(i) as u64;
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
#[cfg(target_os = "none")]
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

/// 该物理区间是否被内核的**恒等映射**覆盖（前 `MANAGED_MEMORY` 字节）。
///
/// 引导器交来的服务 ELF 镜像（E3b）以物理地址给出，内核靠恒等映射按物理地址读它；
/// 落在覆盖范围之外的区间内核够不着，只能判失败而不是拿着地址去读。
pub fn is_identity_mapped(addr: u64, len: u64) -> bool {
    matches!(addr.checked_add(len), Some(end) if end <= MANAGED_MEMORY)
}

/// 判断虚拟地址是否属于用户空间 (P4[1], 即 `USER_SPACE_BASE` 起的 512 GiB)。
///
/// 用于系统调用信任边界: 拒绝用户态传入的内核地址 (恒等 P4[0] / offset P4[256] /
/// 内核堆 P4[136] 等) 被 `resolve_user_page` / `map_user_page` 解析或重映射,
/// 否则会因在 2 MiB 大页之上映射 4 KiB 页而触发 `ParentEntryHugePage`, 导致内核 panic。
pub fn is_user_address(vaddr: u64) -> bool {
    ((vaddr >> 39) & 0x1FF) == 1
}

/// 用户页权限。**W^X**: 任何一页都不同时可写、可执行 —— 只有 [`UserPagePerm::ReadExecute`]
/// 是可执行的, 它必然不可写; 另外两种一律置 NX (依赖 `EFER.NXE`, 见 `enable_nx`)。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum UserPagePerm {
    /// 只读、不可执行 (ELF 的只读数据段)。
    ReadOnly,
    /// 可读写、不可执行 (数据段 / 用户栈 / 共享缓冲 / 匿名页 —— 默认权限)。
    ReadWrite,
    /// 只读、可执行 (ELF 的代码段)。
    ReadExecute,
}

/// 页权限 → 页表项标志 (W^X 的唯一落点, 单测覆盖)。
fn flags_for(perm: UserPagePerm) -> PageTableFlags {
    let base = PageTableFlags::PRESENT | PageTableFlags::USER_ACCESSIBLE;
    match perm {
        // 可执行: 不置 NX; 同时绝不置 WRITABLE。
        UserPagePerm::ReadExecute => base,
        UserPagePerm::ReadWrite => base | PageTableFlags::WRITABLE | PageTableFlags::NO_EXECUTE,
        UserPagePerm::ReadOnly => base | PageTableFlags::NO_EXECUTE,
    }
}

/// 在指定域的页表中, 把用户虚拟地址 `vaddr` 映射到物理帧 `paddr` (USER 权限, 按 `perm`)。
///
/// 用户空间使用独立的 PML4 条目 (P4[1], 基址见 `USER_SPACE_BASE`), 不干扰内核的
/// 恒等/offset 映射。中间页表 (PDPT/PD/PT) 缺失时自动分配并清零。
pub fn map_user_page(domain_id: u64, vaddr: u64, paddr: u64, perm: UserPagePerm) {
    let pml4 = crate::domain::pml4_of(domain_id);
    // 通过 offset 映射访问目标域的 PML4。
    let pml4_virt = (PHYS_OFFSET + pml4) as *mut PageTable;
    let mut mapper = unsafe { OffsetPageTable::new(&mut *pml4_virt, VirtAddr::new(PHYS_OFFSET)) };

    let page = Page::<Size4KiB>::containing_address(VirtAddr::new(vaddr));
    let frame = PhysFrame::containing_address(PhysAddr::new(paddr));
    let flags = flags_for(perm);

    let mut allocator = KernelFrameAllocator;
    unsafe {
        let result = mapper.map_to(page, frame, flags, &mut allocator);
        if let Err(ref e) = result {
            let p4 = ((vaddr >> 39) & 0x1FF) as usize;
            let p3 = ((vaddr >> 30) & 0x1FF) as usize;
            let p2 = ((vaddr >> 21) & 0x1FF) as usize;
            let p1 = ((vaddr >> 12) & 0x1FF) as usize;
            panic!(
                "map_user_page: map failed {:?} dom={} vaddr=0x{:x} paddr=0x{:x} p4/p3/p2/p1={}/{}/{}/{}",
                e, domain_id, vaddr, paddr, p4, p3, p2, p1
            );
        }
        result.unwrap().flush();
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
        | PageTableFlags::NO_CACHE
        | PageTableFlags::NO_EXECUTE;

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

/// 释放一个域**全部用户空间**: 逐页归还物理帧, 再回收各级页表帧与 PML4 自身。
///
/// 只遍历 P4[1] (用户空间) —— 其余 PML4 条目是内核映射 (恒等 / offset / 内核堆),
/// 为所有域共享, 不能动 (见 `domain::Domain::new` 里对 P4[1] 的跳过)。
///
/// 逐页归还走 `frame_allocator::release_user_frame` (记账规则见那里): 登记过引用计数的
/// 帧按计数递减、归零才释放, 未登记的 (镜像页 / 栈帧 / 页表帧) 视为本域独占直接释放。
/// 不回收的话, 每 `run` 一次就漏掉一整个地址空间 —— 这正是 E2b 要补上的那一条。
///
/// 调用者须是**别的域** (不能拆自己正在用的页表), 门禁在 `domain::destroy` 的调用方。
pub fn free_user_space(pml4_phys: u64) {
    let p4_index = ((USER_SPACE_BASE >> 39) & 0x1FF) as usize;
    let frame_size = frame_allocator::FRAME_SIZE as u64;
    let pml4 = unsafe { &mut *((PHYS_OFFSET + pml4_phys) as *mut PageTable) };
    if pml4[p4_index].is_unused() {
        return;
    }
    let pdpt_phys = pml4[p4_index].addr().as_u64();

    for p3 in 0..512 {
        let pdpt = unsafe { &mut *((PHYS_OFFSET + pdpt_phys) as *mut PageTable) };
        if pdpt[p3].is_unused() {
            continue;
        }
        let pd_phys = pdpt[p3].addr().as_u64();
        for p2 in 0..512 {
            let pd = unsafe { &mut *((PHYS_OFFSET + pd_phys) as *mut PageTable) };
            if pd[p2].is_unused() {
                continue;
            }
            if pd[p2].flags().contains(PageTableFlags::HUGE_PAGE) {
                // 2 MiB 大页: 没有下一级页表, 直接归还它覆盖的 512 个 4 KiB 帧。
                let base = pd[p2].addr().as_u64();
                for i in 0..512u64 {
                    frame_allocator::release_user_frame(base + i * frame_size);
                }
                pd[p2].set_unused();
                continue;
            }
            let pt_phys = pd[p2].addr().as_u64();
            for p1 in 0..512 {
                let pt = unsafe { &mut *((PHYS_OFFSET + pt_phys) as *mut PageTable) };
                if pt[p1].is_unused() {
                    continue;
                }
                frame_allocator::release_user_frame(pt[p1].addr().as_u64());
                pt[p1].set_unused();
            }
            frame_allocator::free_frame(pt_phys);
            pd[p2].set_unused();
        }
        frame_allocator::free_frame(pd_phys);
        pdpt[p3].set_unused();
    }
    frame_allocator::free_frame(pdpt_phys);
    pml4[p4_index].set_unused();
}

#[cfg(test)]
mod tests {
    use super::{flags_for, is_identity_mapped, is_user_address, UserPagePerm, USER_SPACE_BASE};
    use x86_64::structures::paging::PageTableFlags;

    /// 引导模块可达性判定 (E3b): 恒等映射只覆盖前 4 GiB, 且地址加法回绕要判否。
    #[test]
    fn identity_map_covers_first_4gib_only() {
        assert!(is_identity_mapped(0x1000, 0x1000));
        assert!(is_identity_mapped(0xFFFF_F000, 0x1000)); // 恰好到 4 GiB 边界
        assert!(!is_identity_mapped(0x1_0000_0000, 0x1000)); // 起于 4 GiB 之上
        assert!(!is_identity_mapped(u64::MAX - 8, 0x1000)); // 加法回绕
    }

    /// W^X 契约: 可执行的页一律不可写, 可写的页一律不可执行。
    #[test]
    fn wx_invariant_holds_for_every_perm() {
        for perm in [
            UserPagePerm::ReadOnly,
            UserPagePerm::ReadWrite,
            UserPagePerm::ReadExecute,
        ] {
            let f = flags_for(perm);
            assert!(f.contains(PageTableFlags::PRESENT), "{perm:?} 必须 present");
            assert!(
                f.contains(PageTableFlags::USER_ACCESSIBLE),
                "{perm:?} 必须 USER"
            );
            let writable = f.contains(PageTableFlags::WRITABLE);
            let executable = !f.contains(PageTableFlags::NO_EXECUTE);
            assert!(
                !(writable && executable),
                "{perm:?} 违反 W^X: 同时可写可执行"
            );
        }
    }

    /// 三种权限各自的期望位 (防止 W^X 之外的位也悄悄变了)。
    #[test]
    fn flags_match_expected_permission_bits() {
        let ro = flags_for(UserPagePerm::ReadOnly);
        assert!(!ro.contains(PageTableFlags::WRITABLE));
        assert!(ro.contains(PageTableFlags::NO_EXECUTE));

        let rw = flags_for(UserPagePerm::ReadWrite);
        assert!(rw.contains(PageTableFlags::WRITABLE));
        assert!(rw.contains(PageTableFlags::NO_EXECUTE));

        let rx = flags_for(UserPagePerm::ReadExecute);
        assert!(!rx.contains(PageTableFlags::WRITABLE));
        assert!(!rx.contains(PageTableFlags::NO_EXECUTE));
    }

    /// 编码安全契约: 仅 P4[1] (用户空间) 放行, 其余一律拒绝。
    #[test]
    fn is_user_address_accepts_p4_1_only() {
        // 拒绝: 零地址 / 恒等映射 / 内核堆 / offset 映射 / 高半区。
        assert!(!is_user_address(0));
        assert!(!is_user_address(0x0000_0000_0010_0000)); // 恒等映射 (P4[0])
        assert!(!is_user_address(0x4444_4444_0000)); // 内核堆 (P4[136])
        assert!(!is_user_address(0xFFFF_8000_0000_0000)); // offset 映射 (P4[256])
        assert!(!is_user_address(0xFFFF_FFFF_FFFF_F000)); // 高半区 (P4[511])

        // 放行: USER_SPACE_BASE 起的 P4[1] 512 GiB 范围。
        assert!(is_user_address(USER_SPACE_BASE));
        assert!(is_user_address(USER_SPACE_BASE + 0x3000));
        assert!(is_user_address(0x0000_00FF_FFFF_FFFF)); // P4[1] 末尾
    }
}
