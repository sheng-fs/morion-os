//! 运行时加载可执行文件并启动 —— `SYS_SPAWN_ELF` 的落点
//!
//! 一条完整链路: 解析校验 (`elf::parse`) → 建新域 → 逐段映射 (含 `.bss` 补零) →
//! 映射用户栈 → `spawn_user` 起一个 Ring 3 任务。新域只拿到**访问文件系统所需的最小
//! 能力**（`SendTo` 挂载层 / MFS + `MapInto` MFS, 见 [`spawn_elf`]），不授予 Mmio / Irq /
//! Fb / Spawn 等特权能力; 且它的分页器登记为 **调用者** —— 加载者就是这个程序的 loader
//! （后续缺页交给它决定怎么补）。
//!
//! 与引导期的 [`spawn_elf_at`] 相比, 这里加载的是**任意镜像**而不是编译期嵌入的
//! 那一份, 并且每个程序拿到自己的域/地址空间, 不再共享"同一份镜像 + 域 id 分流"。

use crate::elf;
use crate::memory::frame_allocator::{self, FRAME_SIZE};
use crate::memory::paging::{self, map_user_page, UserPagePerm, USER_STACK_PAGES, USER_STACK_TOP};

/// 一次加载允许占用的最大物理页数（防止一个坏镜像把内存吃光）。
pub const MAX_IMAGE_PAGES: u64 = 4096; // 16 MiB

/// 引导期服务域号（与 `kernel/src/main.rs` 的建域顺序一致）: 运行期加载的程序默认需要
/// 访问挂载层与文件服务 —— 否则它连「打开一个文件」都发不出去（`SYS_CALL` 需 `SendTo`）。
const MOUNT_SRV_DOMAIN: u64 = 9;
const MFS_SRV_DOMAIN: u64 = 11;

/// 加载 `image` 里的 ELF 并启动它; 成功返回新域 id。
///
/// `loader` 是发起加载的域, 同时被登记为新域的分页器 (也就是"谁加载谁负责"里的负责人:
/// `SYS_DOMAIN_DESTROY` 只允许分页器销毁该域)。任一环节失败返回 `None`, 并且
/// **已经把建出来的半成品域销毁掉**（地址空间与各全局表行都不留）。
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
    crate::cap::add_domain(domain);
    crate::ipc::add_domain(domain);
    crate::pager::add_domain(domain, loader);

    // 运行期程序默认要能用文件系统: 授予到挂载层 (路由) 与 MFS (调用) 的 `SendTo`,
    // 以及把结果 / 写缓冲页映射进 MFS 的 `MapInto`。不给 Mmio / Irq / Fb / Spawn 等
    // 特权能力 —— 这是**最小可用**面 (04b 低权自测的前提, 也修了"运行时程序无法访问
    // 任何服务"这个缺口)。
    crate::cap::grant(domain, crate::cap::Capability::SendTo(MOUNT_SRV_DOMAIN));
    crate::cap::grant(domain, crate::cap::Capability::SendTo(MFS_SRV_DOMAIN));
    crate::cap::grant(domain, crate::cap::Capability::MapInto(MFS_SRV_DOMAIN));

    // 映射镜像与用户栈, 再起任务。任一步失败都要**销毁这个半成品域** ——
    // 域回收 (E2b 地基) 已经就位, 失败路径再漏域漏页就说不过去了。
    let built = map_image(image, &elf, domain).is_some()
        && crate::scheduler::try_spawn_user(elf.entry, USER_STACK_TOP, domain);
    if !built {
        crate::domain::destroy(domain);
        return None;
    }
    Some(domain)
}

/// 把镜像载入一个**已存在**的域并起一个 Ring 3 任务 (引导期建服务用), 成功返回 `true`。
///
/// 与 [`spawn_elf`] 的差别: **不建域、不登记各全局表** —— 引导期的服务域由内核按固定域号
/// 先建好并完成授权 (`cap/ipc/pager::init`), 这里只做"解析 → 映射镜像与栈 → 起任务"。
/// 失败 (镜像非法 / 超预算 / 映射或起任务失败) 返回 `false`; 引导期不做失败回滚 ——
/// 那意味着一个编译期内嵌的镜像坏了, 属于构建问题, 调用方打印 `[FAILED]` 即止。
pub fn spawn_elf_at(domain: u64, image: &[u8]) -> bool {
    let Some(elf) = elf::parse(image) else {
        return false;
    };

    // 预算页数: 各段按页向上取整之和 + 栈 (与 `spawn_elf` 同一口径)。
    let mut pages: u64 = USER_STACK_PAGES;
    for seg in elf.segments() {
        let Some(seg_pages) = (seg.memsz / FRAME_SIZE as u64).checked_add(1) else {
            return false;
        };
        let Some(total) = pages.checked_add(seg_pages) else {
            return false;
        };
        pages = total;
    }
    if pages > MAX_IMAGE_PAGES {
        return false;
    }

    map_image(image, &elf, domain).is_some()
        && crate::scheduler::try_spawn_user(elf.entry, USER_STACK_TOP, domain)
}

/// 把 `elf` 的各段与用户栈映射进 `domain`; 失败返回 `None` (调用方负责销毁域)。
///
/// 分两遍: 先按「页权限并集」建映射, 再拷内容。之所以要先建完再拷, 是因为相邻段常共享
/// 边界页 —— 若边拷边建, 后一段可能要求与已建映射不同的权限; 先按并集建好, 就不需要
/// 事后改权限 (页表项改标志在 `x86_64` 的 `Mapper` 上没有对应入口)。
///
/// 页权限取该页上**所有段的并集**; 并集同时含 W 与 X 时**拒绝加载** (W^X) ——
/// `elf::parse` 已拒绝单个 W+X 段, 这里再挡住"RX 段与 RW 段共享一页"的情形。
fn map_image(image: &[u8], elf: &elf::Image, domain: u64) -> Option<()> {
    // 第一遍: 逐段逐页建映射 (一页只映射一次, 已映射的复用其物理帧)。
    for &seg in elf.segments() {
        let first = seg.vaddr & !(FRAME_SIZE as u64 - 1);
        let end = seg.vaddr + seg.memsz;
        let mut page_vaddr = first;
        while page_vaddr < end {
            if paging::resolve_user_page(domain, page_vaddr).is_none() {
                let perm = page_perm(elf, page_vaddr)?;
                let p = frame_allocator::allocate_frame()?;
                // 清零: `.bss` 与段末尾的填充都依赖"新页为 0", 而分配器不保证内容。
                // 物理地址 < 4 GiB 在恒等映射内, 可直接按虚拟地址写。
                unsafe {
                    core::ptr::write_bytes(p as *mut u8, 0, FRAME_SIZE);
                }
                map_user_page(domain, page_vaddr, p, perm);
            }
            page_vaddr += FRAME_SIZE as u64;
        }
    }

    // 第二遍: 把段内容写进已映射的物理帧 (内核经恒等映射写, 不受用户页权限限制)。
    for &seg in elf.segments() {
        let first = seg.vaddr & !(FRAME_SIZE as u64 - 1);
        let end = seg.vaddr + seg.memsz;
        let mut page_vaddr = first;
        while page_vaddr < end {
            let paddr = paging::resolve_user_page(domain, page_vaddr)?;
            copy_segment_page(image, seg, page_vaddr, paddr);
            page_vaddr += FRAME_SIZE as u64;
        }
    }

    // 用户栈: 与引导期同一布局（所有程序共用一套链接地址）。数据页一律 RW + NX。
    let stack_base = USER_STACK_TOP - USER_STACK_PAGES * FRAME_SIZE as u64;
    for i in 0..USER_STACK_PAGES {
        let paddr = frame_allocator::allocate_frame()?;
        unsafe {
            core::ptr::write_bytes(paddr as *mut u8, 0, FRAME_SIZE);
        }
        map_user_page(
            domain,
            stack_base + i * FRAME_SIZE as u64,
            paddr,
            UserPagePerm::ReadWrite,
        );
    }
    Some(())
}

/// 页 `page_vaddr` 的权限 = 覆盖它的所有段的并集。
///
/// 并集同时可写、可执行时返回 `None`: 页级 W^X 无法满足, 调用方据此拒绝加载
/// (不静默降级成 RWX, 也不猜测"哪个更该保留")。
fn page_perm(elf: &elf::Image, page_vaddr: u64) -> Option<UserPagePerm> {
    let mut writable = false;
    let mut executable = false;
    for seg in elf.segments() {
        let start = seg.vaddr & !(FRAME_SIZE as u64 - 1);
        let end = seg.vaddr + seg.memsz;
        if page_vaddr >= start && page_vaddr < end {
            writable |= seg.flags & elf::PF_W != 0;
            executable |= seg.flags & elf::PF_X != 0;
        }
    }
    match (writable, executable) {
        (true, true) => None,
        (_, true) => Some(UserPagePerm::ReadExecute),
        (true, false) => Some(UserPagePerm::ReadWrite),
        (false, false) => Some(UserPagePerm::ReadOnly),
    }
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
