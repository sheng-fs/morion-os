//! 可执行文件加载（libmorion）：从文件系统读一个程序镜像，交给内核启动
//!
//! 一条链路：`vfs::open` 打开路径 → 分块读进本域内存（经一页中转）→ 整段镜像交给
//! `SYS_SPAWN_ELF`（内核全量校验 → 建新域 → 映射 → 起任务）。
//!
//! # 为什么经"中转页"而不是直接读进暂存区
//!
//! 文件服务是把数据写进**调用方指定的那一页**（同地址共享映射），所以读的目标页必须
//! 共享给该服务。若让每个暂存页都共享出去，一个几百 KB 的程序就要占几十个内核共享帧
//! 槽位（`frame_allocator` 里只有 64 个）。这里只用**一页**反复当中转，再在本域内
//! `memcpy` 到暂存区 —— 多一次内存拷贝，换共享帧表只占 1 个槽位。
//!
//! # 调用前提
//!
//! 本域需持有 `Capability::Spawn`（造进程）与对目标文件服务的 `MapInto`（共享中转页）。

use crate::syscall::{sys_alloc_page, sys_share_page, sys_spawn_elf, sys_virt_to_phys};
use crate::vfs;

/// 镜像暂存区虚拟地址。所有程序共用同一套链接布局（见 `user/linker.ld`），故可用同一地址：
/// 取 `USER_BASE + 2 MiB` —— 在程序镜像（自 `USER_BASE` 起，随代码增长）与文件服务缓冲
/// （`+0x10_0000..+0x16_2000`）之上、用户栈（`+0x3F_9000` 起）之下。
///
/// ⚠️ 这是**客户端程序**（shell / app / 将来的 init）里的空闲区间。fat32_srv 自己在
/// `+0x20_0000..+0x21_0000` 有整簇缓冲，故 `spawn_file` 的调用方不能是文件服务。
pub const ELF_STAGE: u64 = 0x0000_0080_0020_0000;

/// 暂存区页数上限（1 MiB），与内核 `syscall::MAX_ELF_LEN` 一致。
pub const ELF_STAGE_PAGES: u64 = 256;

/// 读文件用的中转页（紧邻暂存区下方一页）。
const BOUNCE: u64 = ELF_STAGE - 4096;

/// 已经从本域共享过中转页的文件服务域（按域 id 置位的位图）。
///
/// 对同一 (页, 域) 重复 `share_page` 会让内核 `map_user_page` 撞到已映射页而 panic,
/// 故必须记住做过哪些。域 id 是 0 起的小整数（当前 14 个域），一个 u64 位图足够；
/// 用位图而不是数组是为了避开对 `static mut` 取引用的 lints。程序是单任务模型，无需并发保护。
static mut BOUNCE_SHARED: u64 = 0;

/// 从文件系统加载并启动一个可执行文件，返回新域 id；`None` = 失败（路径打不开、
/// 镜像非法/超限、或内核拒绝）。
pub fn spawn_file(path: &str) -> Option<u64> {
    let fd = vfs::open(path);
    if fd == u64::MAX {
        return None;
    }

    // 中转页：分配一次 + 共享给**持有该 fd 的那个**文件服务（只共享它，不多要能力）。
    if !ensure_page(BOUNCE) || !share_bounce(vfs::fd_domain(fd)) {
        vfs::close(fd);
        return None;
    }

    let cap = ELF_STAGE_PAGES * 4096;
    let mut len = 0u64;
    loop {
        if len >= cap {
            // 到达上限：再探一个字节，还有内容就说明镜像超限（宁可明确失败，不做静默截断）。
            if vfs::read_into(fd, len, 1, BOUNCE) > 0 {
                vfs::close(fd);
                return None;
            }
            break;
        }
        if !ensure_page(ELF_STAGE + len) {
            vfs::close(fd);
            return None;
        }
        let n = vfs::read_into(fd, len, 4096, BOUNCE);
        if n == u64::MAX {
            vfs::close(fd);
            return None;
        }
        if n == 0 {
            break; // EOF
        }
        unsafe {
            core::ptr::copy_nonoverlapping(
                BOUNCE as *const u8,
                (ELF_STAGE + len) as *mut u8,
                n as usize,
            );
        }
        len += n;
    }
    vfs::close(fd);

    if len == 0 {
        return None;
    }
    let image = unsafe { core::slice::from_raw_parts(ELF_STAGE as *const u8, len as usize) };
    match sys_spawn_elf(image) {
        u64::MAX => None,
        domain => Some(domain),
    }
}

/// 确保 `va` 已映射：已映射（同一进程里上一次 `spawn_file` 留下的）就直接复用 ——
/// 对已映射页再 `alloc_page` 会让内核 `map_user_page` panic。
fn ensure_page(va: u64) -> bool {
    if sys_virt_to_phys(va) != 0 {
        return true;
    }
    sys_alloc_page(va) == 1
}

/// 把中转页共享给 `domain`（同一进程里对同一域只做一次）。
fn share_bounce(domain: u64) -> bool {
    // 域 id 超出位图宽度就不记（当前域数远小于 64；真到那天这里要换成位数组）。
    let tracked = domain < 64;
    unsafe {
        if tracked && BOUNCE_SHARED & (1u64 << domain) != 0 {
            return true;
        }
    }
    if sys_share_page(BOUNCE, domain) != 1 {
        return false;
    }
    if tracked {
        unsafe {
            BOUNCE_SHARED |= 1u64 << domain;
        }
    }
    true
}
