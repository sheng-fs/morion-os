//! 域 14 — init (监督者服务, E3c / E3c 后续)
//!
//! 引导期服务域由内核加载**一次**，之后没人管：任务退出后域还在 (它是引导域白名单，
//! 不会被"退出即回收")，于是那个服务就永远没了。init 补上这一环 —— 它持
//! `Capability::Spawn`，周期巡检一批"长期驻留"的服务域 (`SYS_DOMAIN_ALIVE`)，发现实例
//! 不在了就把它**原地重启**：域号不变，于是所有按域号寻址的地方 (libvfs 里写死的
//! `FAT32_DOMAIN=6`、shell 直接 `SendTo(5)`…) 都不受影响。
//!
//! # 两个镜像来源 (E3c 后续)
//!
//! 重启优先用**引导模块内存镜像** (`SYS_SPAWN_ELF_MODULE`): 引导器把服务 ELF 读进
//! `LOADER_DATA` 页、经 `BootInfo` 模块表交给内核 (E3b)，与内核同生命周期、**不依赖磁盘**。
//! 这条路径让 init 能重启**文件服务本身** (fat32_srv / mfs_srv) —— 若只能用盘上镜像，
//! "读盘要靠文件服务、文件服务死了没法自救"就是个死结。
//!
//! 内存镜像不可用 (旧引导器没打包该服务) 时才回退到盘上镜像
//! (`/system/services/<name>.elf`, 走 `SYS_SPAWN_ELF_AT`)，并打印来源以便区分。
//!
//! # 为什么是轮询而不是内核回调
//!
//! 内核里加"退出通知"要么引入回调注册表，要么让内核认识"服务"这个纯用户态概念 —— 与
//! E2b 之后"内核只提供机制"的方向相反。轮询的代价是一个 `SYS_SLEEP(40ms)` 加每个被监督
//! 域一次 `SYS_DOMAIN_ALIVE`，可以忽略 (块层统计 `irq_cmds`/`poll_cmds` 不受任何影响)。
//!
//! # 监督范围
//!
//! 监督**长期驻留、且镜像能自举**的服务: pager / echo / kbd / fat32_srv / mount_srv /
//! tmpfs_srv / mfs_srv / ext2_srv / exfat_srv。刻意不在列的两类:
//!
//! - **block_srv (域 5)**: 内核为它映射了 NVMe 配置页与 DMA 帧，`domain::reset` 会把那些
//!   映射连同**物理帧**一起还给帧分配器，甚至把 BAR0 的 MMIO 地址当成 RAM 交出去 ——
//!   重启它等于先破坏内核侧的设备状态。真要监督它，得先让 reset 跳过内核保留映射。
//! - **sender / receiver / app / shell**: 它们按设计会**正常退出** (演示/自测跑完就返回)，
//!   监督它们等于无休止重启。

use morion::exec;
use morion::syscall::*;

/// 被监督的服务: `(域号, 盘上镜像名)`。
///
/// 域号是 ABI (见内核对域布局的注释)，与 `kernel/src/main.rs` 建域顺序一致。盘上镜像名只
/// 在内存镜像不可用时用作回退路径。
const SUPERVISED: [(u64, &str); 9] = [
    (2, "pager"),
    (3, "echo"),
    (4, "kbd"),
    (6, "fat32_srv"),
    (9, "mount_srv"),
    (10, "tmpfs_srv"),
    (11, "mfs_srv"),
    (12, "ext2_srv"),
    (13, "exfat_srv"),
];

/// 巡检周期 (ms)。
const POLL_MS: u64 = 40;

/// 盘上服务目录 (FAT32 根卷)。
const SERVICE_DIR: &str = "/system/services/";

/// 监督循环: 每轮检查一遍被监督域，缺谁补谁。
pub fn run() {
    println(
        "init: supervising pager/echo/kbd/fat32_srv/mount_srv/tmpfs_srv/mfs_srv/ext2_srv/exfat_srv",
    );
    let mut restarts = 0u64;
    loop {
        for (domain, name) in SUPERVISED {
            if sys_domain_alive(domain) != 0 {
                continue;
            }
            // 域号必须原样回来 (内核只允许"原地重启") —— 不相等说明约定被破坏，宁可报错。
            let (revived, from_disk) = restart(domain, name);
            if revived != domain {
                print("init: restart ");
                print(name);
                println(" FAILED");
                continue;
            }
            restarts += 1;
            print("init: restarted ");
            print(name);
            print(" (domain ");
            print_u64(domain);
            print(", total ");
            print_u64(restarts);
            print(if from_disk {
                ", from disk)"
            } else {
                ", from memory)"
            });
            println("");
        }
        sys_sleep(POLL_MS);
    }
}

/// 重启 `domain` 上的 `name` 服务: 先试引导模块内存镜像，不行再回退盘上镜像。
///
/// 返回 `(实际域号, 是否走的磁盘)` —— 均失败时域号为 `u64::MAX`。
fn restart(domain: u64, name: &str) -> (u64, bool) {
    let from_memory = sys_spawn_elf_module(domain);
    if from_memory == domain {
        return (domain, false);
    }
    let mut buf = [0u8; 64];
    let path = service_path(&mut buf, name);
    match exec::spawn_file_at(path, domain) {
        Some(d) if d == domain => (domain, true),
        _ => (u64::MAX, false),
    }
}

/// 拼出 `/system/services/<name>.elf` (用户态没有格式化库，手工拼最省事)。
fn service_path<'a>(buf: &'a mut [u8; 64], name: &str) -> &'a str {
    let prefix = SERVICE_DIR.as_bytes();
    buf[..prefix.len()].copy_from_slice(prefix);
    let mut n = prefix.len();
    buf[n..n + name.len()].copy_from_slice(name.as_bytes());
    n += name.len();
    buf[n..n + 4].copy_from_slice(b".elf");
    n += 4;
    unsafe { core::str::from_utf8_unchecked(&buf[..n]) }
}
