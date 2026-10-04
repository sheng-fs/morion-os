//! 保护域 (Domain) — 地址空间隔离的基础抽象
//!
//! 微内核核心原语 · 第 3 小步。
//!
//! 每个域拥有独立的页表 (PML4)。创建时复制当前内核页表的非空条目,
//! 从而共享内核空间映射 (恒等 + offset + 内核堆), 保证内核代码在所有
//! 域中均可运行; 域私有的用户空间映射将在后续步骤建立。
//!
//! 域 id 是**各全局表的下标** (`cap`/`ipc`/`pager` 是 `Vec`, `irq::ANY_MASK`
//! 是 `[u64; 64]`), 因此 `destroy` 之后槽位必须**复用** —— 否则反复创建/销毁
//! 会让 id 单调增长, 迟早越界。

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

/// 全局域表 (按 id 索引; `None` = 已销毁的空槽, 该 id 可被复用)。
static DOMAINS: Mutex<Vec<Option<Domain>>> = Mutex::new(Vec::new());

/// 引导期域数: 域 id `0..BOOT_DOMAINS` 是引导期建的长期服务域
/// (sender/receiver/pager/echo/kbd/block/fat32/app/shell/mount/tmpfs/mfs/ext2/exfat/init/gfx_srv/net_srv/virtio_blk_srv/ahci_srv/xhci_srv/iso9660_srv/netstack_srv/e1000e_srv/httpd_srv/e1000_srv/wifi_srv)。
///
/// 它们的槽位**始终被占用**, 所以 `slot_for` 永远不会把运行时新域分配到这些 id 上 ——
/// 「id < `BOOT_DOMAINS` 即引导域」是一条稳定不变量。这些域**永不自动销毁** (退出即回收
/// 的白名单); 运行时经 `SYS_SPAWN_ELF` 建的域则在最后一个任务退出后自动回收。
///
/// 注意「永不自动销毁」不等于「实例永不退出」: 白名单域里的任务退出后域还留着 (槽位
/// 仍占用), 由监督者 [`crate::syscall`] 的 `SYS_SPAWN_ELF_AT` 用 [`reset`] 原地重启
/// (E3c) —— 见 `user/srv/src/init.rs`。
pub const BOOT_DOMAINS: u64 = 26;

/// 该域是否是引导期服务域 (白名单: 退出时不自动销毁)。
pub fn is_boot(id: u64) -> bool {
    id < BOOT_DOMAINS
}

/// 待回收域队列 (退出即回收的**延迟**机制, 见 `request_destroy`)。
static PENDING: Mutex<Vec<u64>> = Mutex::new(Vec::new());

/// 为新域挑一个槽位: 优先复用已销毁的空槽, 否则在表尾追加。
///
/// 纯策略函数, 与"建页表"这件硬件动作分开, 便于单测 (见本文末尾的测试)。
fn slot_for(domains: &[Option<Domain>]) -> usize {
    domains
        .iter()
        .position(|slot| slot.is_none())
        .unwrap_or(domains.len())
}

/// 创建一个新保护域, 返回其 id (优先复用已销毁的槽位)。
pub fn create() -> u64 {
    let mut domains = DOMAINS.lock();
    let id = slot_for(&domains) as u64;
    let domain = Domain::new(id);
    match domains.get_mut(id as usize) {
        Some(slot) => *slot = Some(domain),
        None => domains.push(Some(domain)),
    }
    id
}

/// 查询指定域的 PML4 物理地址。
pub fn pml4_of(id: u64) -> u64 {
    DOMAINS.lock()[id as usize]
        .as_ref()
        .expect("pml4_of: 域不存在或已被销毁")
        .pml4
}

/// 该域当前是否存活 (槽位非空)。
pub fn is_alive(id: u64) -> bool {
    DOMAINS
        .lock()
        .get(id as usize)
        .map(|slot| slot.is_some())
        .unwrap_or(false)
}

/// 存活域数 (自测取证用: 销毁之后应回到基线)。
pub fn alive_count() -> usize {
    DOMAINS.lock().iter().filter(|slot| slot.is_some()).count()
}

/// 销毁一个域: 回收它的用户地址空间与**全部**域相关内核状态, 槽位归还以便复用。
///
/// 顺序是有讲究的 —— **先摘掉状态, 再释放页与帧**:
///   1. 从域表摘除自己 (之后任何走 `pml4_of` 的路径都不会再碰到它),
///      同时取回 PML4 物理地址;
///   2. 释放用户地址空间: 逐页按 `release_user_frame` 的记账规则归还, 再回收
///      PDPT/PD/PT 页表帧, 最后释放 PML4 帧;
///   3. 清掉各子系统里属于它的行: 能力 + 句柄、邮箱、分页器、中断注册;
///   4. 摘除并终止它的全部任务, 并唤醒正在等它的任务 (否则对端永久挂死)。
///
/// 返回 `false` = 该域不存在 (或已销毁)。
///
/// **不允许自我销毁**: 调用者必须是**别的域** —— 销毁自己会拆掉正在使用的内核栈与
/// 页表。门禁留在 syscall 侧 (`SYS_DOMAIN_DESTROY`: 目标的分页器 == 调用者),
/// 而 `exec::spawn_elf` 正是把"加载者"登记为分页器的, 所以"谁加载谁负责"天然成立。
pub fn destroy(id: u64) -> bool {
    // 1. 摘除域表槽位 (取回 pml4, 供后面释放用)。
    let pml4 = {
        let mut domains = DOMAINS.lock();
        match domains.get_mut(id as usize).and_then(|slot| slot.take()) {
            Some(domain) => domain.pml4,
            None => return false,
        }
    };
    // 2. 地址空间 (逐页记账 + 页表帧 + PML4 帧)。
    crate::memory::paging::free_user_space(pml4);
    frame_allocator::free_frame(pml4);
    // 3. 各子系统的按域状态。
    crate::cap::remove_domain(id);
    crate::ipc::remove_domain(id);
    crate::pager::remove_domain(id);
    crate::irq::remove_domain(id);
    // 4. 任务 (含唤醒等待者)。
    crate::scheduler::remove_domain(id);
    true
}

/// 清空一个域的**用户地址空间**, 但保留域本身 (id / PML4 帧 / 分页器注册 / 能力表)。
///
/// 用于「同域重启」(`SYS_SPAWN_ELF_AT`, E3c): 上一个实例已退出, 但域还在 (引导期服务域
/// 永不自动销毁), 它的用户页表与镜像页仍挂着 —— 不清掉就无法把新实例映射进同一棵树
/// (`map_user_page` 会撞 `PageAlreadyMapped`)。清完之后 PML4 依旧是本域的根, 内核条目
/// (恒等 / offset / 内核堆) 原样保留, 故接着映射新镜像即可。
///
/// 返回 `false` = 该域不存在。
pub fn reset(id: u64) -> bool {
    let pml4 = {
        let domains = DOMAINS.lock();
        match domains.get(id as usize).and_then(|slot| slot.as_ref()) {
            Some(domain) => domain.pml4,
            None => return false,
        }
    };
    crate::memory::paging::free_user_space(pml4);
    true
}

/// 请求销毁一个域 —— **延迟执行** (退出即回收的入口, 由 `SYS_EXIT` 路径调用)。
///
/// 为什么延迟: 退出时调用任务仍跑在**自己的内核栈与该域的页表 (CR3)** 上, 就地销毁
/// 会拆掉正在使用的栈与地址空间。故这里只把域号登记进队列, 由 [`reclaim_pending`]
/// 在**别的任务**的上下文里真正销毁。
///
/// 引导期服务域 (`is_boot`) 是白名单, 直接忽略 —— 它们由内核在引导期长期持有。
pub fn request_destroy(id: u64) {
    if is_boot(id) {
        return;
    }
    PENDING.lock().push(id);
}

/// 执行所有挂起的域销毁。
///
/// 只可在**不在目标域上**的上下文调用 —— 目前由时钟 `tick` 调用: 那时跑在被打断任务的
/// 栈与地址空间上, 而挂起域已经没有可运行任务 (其最后一个任务已终止), 故二者必不相同。
/// 一轮把队列清空 (`destroy` 里会再取各类锁, 故逐个弹出、不在持锁时销毁)。
pub fn reclaim_pending() {
    loop {
        let id = { PENDING.lock().pop() };
        match id {
            Some(id) => {
                destroy(id);
            }
            None => break,
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn slot(id: u64, pml4: u64) -> Option<Domain> {
        Some(Domain { id, pml4 })
    }

    /// 建域挑槽位的策略: 空表追加; 有空槽就**复用** (域 id 是各全局表的下标,
    /// 不复用会单调增长并越界 `irq::ANY_MASK` 那类定长表)。
    ///
    /// 只测纯策略 —— `Domain::new` 要读 CR3 并分配真实帧, 在宿主单测里跑不了;
    /// "建/销之后帧与域号都回到基线"由 QEMU 里的端到端自测 (FS-28) 取证。
    #[test]
    fn slot_policy_appends_then_reuses_holes() {
        // 空表 → 0
        assert_eq!(slot_for(&[]), 0);

        // 表尾追加: 0..3 都活着 → 新域用 3
        let full = [slot(0, 0x1000), slot(1, 0x2000), slot(2, 0x3000)];
        assert_eq!(slot_for(&full), 3);

        // 中间出现空槽 (域 1 被销毁) → **复用 1**, 而不是继续往后加
        let holey = [slot(0, 0x1000), None, slot(2, 0x3000)];
        assert_eq!(slot_for(&holey), 1);
        assert_eq!(slot_for(&holey[..1]), 1); // 只剩域 0 存活 → 复用槽 1
    }

    /// 退出即回收的白名单判据: 引导期服务域 (0..13) 永不自动销毁, 运行时域 (≥14) 才可以。
    #[test]
    fn boot_domains_are_whitelisted() {
        for id in 0..BOOT_DOMAINS {
            assert!(is_boot(id), "引导域 {id} 应在白名单内");
        }
        assert!(!is_boot(BOOT_DOMAINS), "运行时域不应在白名单内");
        assert!(!is_boot(BOOT_DOMAINS + 1));
    }
}
