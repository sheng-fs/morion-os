//! 虚拟内存管理 (阶段三) — 4 级页表 + offset 映射 + 内核堆
//!
//! 建立方式:
//!   1. 手动构造初始页表 (2 MiB 大页), 将物理内存前 32 GiB 同时映射到恒等映射
//!      (P4[0], 虚拟地址 == 物理地址, 供固件交来的物理地址直接读) 与 offset 映射
//!      (P4[256], 虚拟地址 = PHYS_OFFSET + 物理地址, 供页表自身访问)。
//!      覆盖范围必须 ≥ 帧分配器可管理的上限: 否则一旦帧分配器把页表帧/用户帧分到
//!      4 GiB 以上, offset 映射访问页表本身就会缺页 —— 真机上 (QEMU 2 GiB 不会复现)
//!      表现为启动到 paging::init 后立刻三重故障复位。
//!   2. 帧缓冲另经 `FB_VIRT_BASE` 的 ioremap 窗口映射 (与 Linux efifb/simplefb 同理):
//!      固件给的帧缓冲物理地址可能落到 32 GiB 之外, 不能依赖恒等映射。
//!   3. 加载 CR3
//!   4. 用 OffsetPageTable 把内核堆映射到高位虚拟地址, 并初始化全局分配器

#[cfg(target_os = "none")]
use linked_list_allocator::LockedHeap;
use x86_64::registers::control::Cr3;
use x86_64::structures::paging::page_table::PageTableEntry;
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

/// 用户栈底 —— 栈最下面一页的起始地址 (`USER_STACK_TOP` 之下 `USER_STACK_PAGES` 页)。
pub const USER_STACK_BOTTOM: u64 = USER_STACK_TOP - USER_STACK_PAGES * 0x1000;

/// 用户栈**守护页** —— 栈底再向下一页, 加载器**不映射**它。
///
/// 栈溢出越过 [`USER_STACK_BOTTOM`] 时先落到这一页: 未映射 → 缺页异常 (交给分页器 /
/// 终止进程), 而不是静默踩进相邻的段或堆映射。ELF 加载侧 (`elf::parse`) 拒绝任何
/// 覆盖本页的段, 保证它始终是空洞。
pub const USER_STACK_GUARD_PAGE: u64 = USER_STACK_BOTTOM - 0x1000;

/// 内核堆起始虚拟地址 (未使用的上半区地址)
const HEAP_START: u64 = 0x4444_4444_0000;
/// 内核堆大小 (至少容纳 7 个 32 KiB 内核栈 + 调度器/IPC/Cap/分页器/分配器元数据)
///   7 × 32 KiB = 224 KiB 仅栈; 加上分配器头 + 容器扩容 + 域/任务结构, 含运行时创建的
///   域/任务后每任务仍要 32 KiB 内核栈 (任务表上限 32 → 约 1 MiB 仅栈), 故留 4 MiB。
const HEAP_SIZE: usize = 4 * 1024 * 1024; // 4 MiB

/// 直接映射 (恒等 + offset) 覆盖的物理内存上限。
///
/// 与 `frame_allocator::MAX_MANAGED_FRAMES` 保持一致 (32 GiB): 帧分配器能交出的
/// 任何物理帧, 都必须能经 offset 映射被页表代码访问。原先固定 4 GiB 时, 大内存
/// 真机上页表帧一旦落在 4 GiB 以上, `map_to` 访问页表就会 #PF → #DF → 三重故障。
const MANAGED_MEMORY: u64 = 32 * 1024 * 1024 * 1024;

/// 直接映射所需的 PD 数量 (每个 PD 覆盖 1 GiB)。
const MANAGED_PDS: usize = (MANAGED_MEMORY / (512 * 0x20_0000)) as usize;

/// 帧缓冲 ioremap 窗口的虚拟基址 (P4[384], 与恒等 / offset / 内核堆 / 用户空间均不冲突)。
pub const FB_VIRT_BASE: u64 = 0xFFFF_C000_0000_0000;
/// ioremap 窗口大小上限 (1 GiB, 足够 8K×4K×32bpp 的帧缓冲)。
const FB_WINDOW_SIZE: u64 = 512 * 0x20_0000;

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

/// 初始化分页 (建立页表 + 加载 CR3 + 初始化内核堆)。
///
/// 返回帧缓冲 ioremap 窗口的虚拟基址 (帧缓冲可用时); 调用方据此把显示指针
/// 从"固件物理地址"改到窗口地址 —— 与 Linux 在 `efifb/simplefb` 里 `ioremap`
/// 帧缓冲后重设 `screen_base` 是同一件事。
pub fn init(info: &crate::bootinfo::BootInfo) -> Option<u64> {
    enable_nx();
    let pml4_phys = setup_page_tables();
    let fb_bytes = info.fb_height as u64 * info.fb_stride as u64 * (info.fb_bpp as u64 / 8);
    let fb_virt = map_framebuffer(pml4_phys, info.fb_addr, fb_bytes);
    load_cr3(pml4_phys);
    // 先切换显示指针再做后续映射: 堆的页表映射一旦 panic, 现场打印必须已经走
    // 新页表里确定存在的 ioremap 窗口, 而不是固件物理地址。
    if let Some(virt) = fb_virt {
        crate::video::rebind(virt);
    }
    #[cfg(target_os = "none")]
    init_heap(pml4_phys);
    fb_virt
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
#[derive(Clone, Copy)]
struct BootPageTable([u64; 512]);

static mut BOOT_PML4: BootPageTable = BootPageTable([0; 512]);
static mut BOOT_PDPT: BootPageTable = BootPageTable([0; 512]);
static mut BOOT_PDS: [BootPageTable; MANAGED_PDS] = [BootPageTable([0; 512]); MANAGED_PDS];

/// 帧缓冲 ioremap 窗口的 PDPT / PD (最多 1 GiB, 2 MiB 大页)。
static mut FB_PDPT: BootPageTable = BootPageTable([0; 512]);
static mut FB_PD: BootPageTable = BootPageTable([0; 512]);

/// 手动构造初始页表, 返回 PML4 的物理地址。
///
/// 此时 CPU 仍运行在 UEFI 的恒等映射下, 物理地址可直接作为虚拟地址访问。
fn setup_page_tables() -> PhysAddr {
    // 启动页表位于 .bss (镜像内, 已被帧分配器保留), 不再向分配器申请。
    let pml4_phys = core::ptr::addr_of!(BOOT_PML4) as u64;
    let pdpt_phys = core::ptr::addr_of!(BOOT_PDPT) as u64;
    let pd_count = MANAGED_PDS;
    let pds_base = core::ptr::addr_of!(BOOT_PDS) as *const BootPageTable;
    let mut pd_phys = [0u64; MANAGED_PDS];
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

/// 把固件帧缓冲映射到 [`FB_VIRT_BASE`] 窗口 (2 MiB 大页, 首尾按 2 MiB 对齐覆盖)。
///
/// 返回帧缓冲**首字节**在窗口中的虚拟地址 (含未对齐偏移); 参数异常或区间超出窗口
/// 时返回 `None` —— 调用方保留原物理地址指针 (仅当它落在恒等映射范围内才安全)。
///
/// 与 Linux `efifb`/`simplefb` 的 `screen_base = ioremap(fb_phys, size)` 同理:
/// 帧缓冲的物理地址由固件决定, 可能远高于内核直接映射的上限, 不能拿它当虚拟地址用。
fn map_framebuffer(pml4_phys: PhysAddr, fb_addr: u64, fb_len: u64) -> Option<u64> {
    const PAGE: u64 = 0x20_0000;
    if fb_addr == 0 || fb_len == 0 {
        return None;
    }
    let aligned = fb_addr & !(PAGE - 1);
    let offset = fb_addr - aligned;
    let span = offset.checked_add(fb_len)?;
    let pages = span.div_ceil(PAGE);
    if pages * PAGE > FB_WINDOW_SIZE {
        return None;
    }

    let pdpt_phys = core::ptr::addr_of!(FB_PDPT) as u64;
    let pd_phys = core::ptr::addr_of!(FB_PD) as u64;
    let flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE;

    unsafe {
        core::ptr::write_bytes(pdpt_phys as *mut u8, 0, 4096);
        core::ptr::write_bytes(pd_phys as *mut u8, 0, 4096);
    }

    let p4_index = ((FB_VIRT_BASE >> 39) & 0x1FF) as usize;
    let pml4 = unsafe { &mut *(pml4_phys.as_u64() as *mut PageTable) };
    pml4[p4_index].set_frame(frame(pdpt_phys), flags);

    let pdpt = unsafe { &mut *(pdpt_phys as *mut PageTable) };
    pdpt[0].set_frame(frame(pd_phys), flags);

    let pd = unsafe { &mut *(pd_phys as *mut PageTable) };
    for i in 0..pages as usize {
        let phys = aligned + i as u64 * PAGE;
        pd[i].set_addr(PhysAddr::new(phys), flags | PageTableFlags::HUGE_PAGE);
    }

    Some(FB_VIRT_BASE + offset)
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

/// 该物理区间是否被内核的**恒等映射**覆盖（前 `MANAGED_MEMORY` 字节, 32 GiB）。
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

/// 用户页映射的失败原因。映射路径**绝不 panic** —— 这些错误由调用方 (syscall 层)
/// 转换成错误码, 或者在内核内部作为"这一页没映射上"处理。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MapError {
    /// 目标虚拟地址**已有映射** (重复映射)。不静默覆盖: 覆盖会让两个虚拟地址背后的
    /// 引用计数失真, 且旧帧再没人回收。
    AlreadyMapped,
    /// 上级页表项是 2 MiB / 1 GiB 大页, 不能再往下建 4 KiB 映射。
    ParentHugePage,
    /// 页表帧分配失败 (内存耗尽)。
    OutOfMemory,
}

/// 本次映射调用**新建**的中间页表清单 (父表物理地址 + 表项下标)。
///
/// 建到一半失败时按逆序摘除并归还: 否则每次失败的映射都白丢 1..3 个页表帧
/// (用户态可以反复触发, 是一条缓慢的物理内存耗尽路径)。
struct CreatedTables {
    slots: [Option<(u64, usize)>; 3],
    len: usize,
}

impl CreatedTables {
    fn new() -> Self {
        Self {
            slots: [None; 3],
            len: 0,
        }
    }

    fn record(&mut self, parent_phys: u64, index: usize) {
        if self.len < self.slots.len() {
            self.slots[self.len] = Some((parent_phys, index));
            self.len += 1;
        }
    }

    /// 撤销本次调用新建的全部中间页表 (逆序: 先摘父表项, 再归还那帧)。
    fn rollback(self) {
        for i in (0..self.len).rev() {
            let Some((parent_phys, index)) = self.slots[i] else {
                continue;
            };
            let parent = unsafe { &mut *((PHYS_OFFSET + parent_phys) as *mut PageTable) };
            let child = parent[index].addr().as_u64();
            parent[index].set_unused();
            frame_allocator::free_frame(child);
        }
    }
}

/// 在已定位到叶页表的表项上建立映射: 已占用时返回 [`MapError::AlreadyMapped`],
/// 而不是像 `Mapper::map_to` 那样把"已映射"当异常路径处理。
///
/// 抽成纯函数 (只碰一条表项) 以便宿主单测覆盖重复映射。
fn install_leaf(
    entry: &mut PageTableEntry,
    frame: PhysFrame<Size4KiB>,
    flags: PageTableFlags,
) -> Result<(), MapError> {
    if !entry.is_unused() {
        return Err(MapError::AlreadyMapped);
    }
    entry.set_frame(frame, flags);
    Ok(())
}

/// 读取一条**非空**中间页表项指向的下一级页表物理地址; 大页项拒绝继续下钻。
fn child_table_addr(entry: &PageTableEntry) -> Result<u64, MapError> {
    if entry.flags().contains(PageTableFlags::HUGE_PAGE) {
        return Err(MapError::ParentHugePage);
    }
    Ok(entry.addr().as_u64())
}

/// 确保 `vaddr` 的 PDPT / PD / PT 存在, 返回**叶页表**的物理地址。
///
/// 新建的中间表记进 `CreatedTables`, 由调用方在后续失败时回收。
fn ensure_tables(
    pml4_phys: u64,
    vaddr: u64,
    parent_flags: PageTableFlags,
) -> Result<(u64, CreatedTables), MapError> {
    let indices = [
        ((vaddr >> 39) & 0x1FF) as usize,
        ((vaddr >> 30) & 0x1FF) as usize,
        ((vaddr >> 21) & 0x1FF) as usize,
    ];
    let mut created = CreatedTables::new();
    let mut table_phys = pml4_phys;
    for &index in &indices {
        let table = unsafe { &mut *((PHYS_OFFSET + table_phys) as *mut PageTable) };
        if table[index].is_unused() {
            let Some(next_phys) = frame_allocator::allocate_frame() else {
                created.rollback();
                return Err(MapError::OutOfMemory);
            };
            // 新页表帧必须整页清零, 否则残留数据会被当成有效表项。
            unsafe {
                core::ptr::write_bytes((PHYS_OFFSET + next_phys) as *mut u8, 0, 4096);
            }
            table[index].set_addr(PhysAddr::new(next_phys), parent_flags);
            created.record(table_phys, index);
            table_phys = next_phys;
        } else {
            match child_table_addr(&table[index]) {
                Ok(next_phys) => {
                    // 与 `Mapper::map_to` 的行为保持一致: 已存在的中间表项要把本次的
                    // 父表标志**并进去**。同一 PDPT/PD 下既有 RX 段又有 RW 段, 少了
                    // WRITABLE 会让用户态的写变成 #PF (err=7, present 但只读)。
                    let entry = &mut table[index];
                    if !entry.flags().contains(parent_flags) {
                        entry.set_flags(entry.flags() | parent_flags);
                    }
                    table_phys = next_phys;
                }
                Err(e) => {
                    created.rollback();
                    return Err(e);
                }
            }
        }
    }
    Ok((table_phys, created))
}

/// 映射的实现: 先补齐中间页表, 再以"已映射即错误"的方式装叶项。
fn map_page_with_flags(
    domain_id: u64,
    vaddr: u64,
    paddr: u64,
    flags: PageTableFlags,
) -> Result<(), MapError> {
    let pml4_phys = crate::domain::pml4_of(domain_id);
    // 与 x86_64 crate `Mapper::map_to` 的口径一致: 中间表项须带同样的 PRESENT /
    // WRITABLE / USER_ACCESSIBLE 位, 否则用户态走不到叶页。
    let parent_flags = flags
        & (PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::USER_ACCESSIBLE);

    let (pt_phys, created) = ensure_tables(pml4_phys, vaddr, parent_flags)?;
    let pt = unsafe { &mut *((PHYS_OFFSET + pt_phys) as *mut PageTable) };
    let index = ((vaddr >> 12) & 0x1FF) as usize;
    let frame = PhysFrame::containing_address(PhysAddr::new(paddr));

    match install_leaf(&mut pt[index], frame, flags) {
        Ok(()) => {
            x86_64::instructions::tlb::flush(VirtAddr::new(vaddr));
            // 这些中间表已被页表层级引用, 不再是"半成品", 不能回收。
            Ok(())
        }
        Err(e) => {
            created.rollback();
            Err(e)
        }
    }
}

/// 在指定域的页表中, 把用户虚拟地址 `vaddr` 映射到物理帧 `paddr` (USER 权限, 按 `perm`)。
///
/// 用户空间使用独立的 PML4 条目 (P4[1], 基址见 `USER_SPACE_BASE`), 不干扰内核的
/// 恒等/offset 映射。中间页表 (PDPT/PD/PT) 缺失时自动分配并清零。
///
/// **不 panic**: 已映射 / 大页父项 / 内存耗尽都返回错误; syscall 路径用这个入口。
pub fn try_map_user_page(
    domain_id: u64,
    vaddr: u64,
    paddr: u64,
    perm: UserPagePerm,
) -> Result<(), MapError> {
    map_page_with_flags(domain_id, vaddr, paddr, flags_for(perm))
}

/// 把物理 MMIO 区域 `paddr` (页对齐) 映射到指定域的 `vaddr` (USER + 非缓存)。
///
/// 与 `try_map_user_page` 的区别在于额外置 `NO_CACHE` (PCD), 避免 CPU 缓存
/// 设备寄存器读写。用于 NVMe 等 MMIO 设备 BAR 的映射。同样**不 panic**。
pub fn try_map_mmio(domain_id: u64, vaddr: u64, paddr: u64) -> Result<(), MapError> {
    let flags = PageTableFlags::PRESENT
        | PageTableFlags::WRITABLE
        | PageTableFlags::USER_ACCESSIBLE
        | PageTableFlags::NO_CACHE
        | PageTableFlags::NO_EXECUTE;
    map_page_with_flags(domain_id, vaddr, paddr, flags)
}

/// 内核内部 (ELF 加载 / 设备授权 / 网络缓冲) 的映射入口: 失败即"这页没映射上"。
///
/// 这些调用点的地址与目标帧都由内核自己决定, 重复映射意味着加载器状态错乱;
/// 但**仍然不能 panic** (内核停机由用户态可控), 故失败静默丢弃 —— 后续访问会
/// 以缺页的形式暴露出来。syscall 路径一律走 [`try_map_user_page`]。
pub fn map_user_page(domain_id: u64, vaddr: u64, paddr: u64, perm: UserPagePerm) {
    let _ = try_map_user_page(domain_id, vaddr, paddr, perm);
}

/// 同 [`map_user_page`], 用于 MMIO 映射。
pub fn map_mmio(domain_id: u64, vaddr: u64, paddr: u64) {
    let _ = try_map_mmio(domain_id, vaddr, paddr);
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
    use super::{
        child_table_addr, flags_for, install_leaf, is_identity_mapped, is_user_address, MapError,
        UserPagePerm, USER_SPACE_BASE, USER_STACK_BOTTOM, USER_STACK_GUARD_PAGE, USER_STACK_PAGES,
        USER_STACK_TOP,
    };
    use x86_64::structures::paging::{PageTable, PageTableFlags, PhysFrame};
    use x86_64::PhysAddr;

    /// 引导模块可达性判定 (E3b): 恒等映射覆盖帧分配器可管理的全部物理内存 (32 GiB),
    /// 且地址加法回绕要判否。
    #[test]
    fn identity_map_covers_managed_memory() {
        assert!(is_identity_mapped(0x1000, 0x1000));
        assert!(is_identity_mapped(0xFFFF_F000, 0x1000));
        assert!(is_identity_mapped(0x1_0000_0000, 0x1000)); // 4 GiB 之上也在覆盖内
        assert!(is_identity_mapped(0x7_FFFF_F000, 0x1000)); // 恰好到 32 GiB 边界
        assert!(!is_identity_mapped(0x8_0000_0000, 0x1000)); // 起于 32 GiB 之上
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

    /// 栈守护页布局: 它在栈底正下方一页, 与 8 个栈页首尾相接且本身仍是用户地址
    /// (加载器不映射它, 于是栈溢出必然触发缺页而不是踩进相邻映射)。
    #[test]
    fn stack_guard_page_sits_below_stack() {
        assert_eq!(
            USER_STACK_BOTTOM,
            USER_STACK_TOP - USER_STACK_PAGES * 0x1000
        );
        assert_eq!(USER_STACK_BOTTOM - USER_STACK_GUARD_PAGE, 0x1000);
        assert!(USER_STACK_GUARD_PAGE & 0xFFF == 0, "守护页须页对齐");
        // 守护页与整个栈都在用户空间 (否则校验/映射路径的地址口径就不一致了)。
        assert!(is_user_address(USER_STACK_GUARD_PAGE));
        assert!(is_user_address(USER_STACK_TOP - 1));
    }

    /// P0 回归: 同一虚拟地址重复映射必须返回 `Err(AlreadyMapped)`, **绝不 panic**,
    /// 也不能静默覆盖掉原映射 (覆盖会让物理帧引用计数失真)。
    #[test]
    fn duplicate_mapping_is_an_error_not_a_panic() {
        let mut pt = PageTable::new();
        let flags = flags_for(UserPagePerm::ReadWrite);
        let first = PhysFrame::containing_address(PhysAddr::new(0x10_0000));

        assert_eq!(install_leaf(&mut pt[0], first, flags), Ok(()));
        assert_eq!(
            install_leaf(&mut pt[0], first, flags),
            Err(MapError::AlreadyMapped),
            "重复映射必须返回错误"
        );
        // 每次用不同的物理帧重复映射, 结果同样是错误, 且原映射保持不变。
        let second = PhysFrame::containing_address(PhysAddr::new(0x20_0000));
        assert_eq!(install_leaf(&mut pt[0], second, flags), Err(MapError::AlreadyMapped));
        assert_eq!(pt[0].addr().as_u64(), 0x10_0000, "原映射不得被覆盖");

        // 不同槽位仍可正常映射。
        assert_eq!(install_leaf(&mut pt[1], second, flags), Ok(()));
        assert_eq!(pt[1].addr().as_u64(), 0x20_0000);
    }

    /// 上级是大页时不能再下钻建 4 KiB 映射 —— 返回错误而不是 `ParentEntryHugePage` panic。
    #[test]
    fn huge_parent_entry_is_rejected() {
        let mut table = PageTable::new();
        let huge = PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::HUGE_PAGE;
        table[3].set_addr(PhysAddr::new(0x40_0000), huge);
        assert_eq!(
            child_table_addr(&table[3]),
            Err(MapError::ParentHugePage)
        );

        // 普通表项正常返回下一级表地址。
        let mut table = PageTable::new();
        table[4].set_addr(
            PhysAddr::new(0x30_0000),
            PageTableFlags::PRESENT | PageTableFlags::WRITABLE,
        );
        assert_eq!(child_table_addr(&table[4]), Ok(0x30_0000));
    }
}
