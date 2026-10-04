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
//! tmpfs_srv / mfs_srv / ext2_srv / exfat_srv / gfx_srv / net_srv。刻意不在列的两类:
//!
//! - **block_srv (域 5)**: 内核为它映射了 NVMe 配置页与 DMA 帧，`domain::reset` 会把那些
//!   映射连同**物理帧**一起还给帧分配器，甚至把 BAR0 的 MMIO 地址当成 RAM 交出去 ——
//!   重启它等于先破坏内核侧的设备状态。真要监督它，得先让 reset 跳过内核保留映射。
//! - **sender / receiver / app / shell**: 它们按设计会**正常退出** (演示/自测跑完就返回)，
//!   监督它们等于无休止重启。
//!
//! **gfx_srv (域 15)** 原也因 reset 隐患排除在外，现已纳入 —— 它带来两个配套前提: ① 帧缓冲
//! 被登记为**内核保留区间** (内核 `frame_allocator` 的 `pin_range`)，`domain::reset` 不会再把
//! 它当普通帧释放; ② 客户端库 ([`morion::gfx`]) 在服务重启后会**重建共享会话** (重发
//! `SYS_SHARE_PAGE`)，且 `ipc::call` 在目标域无存活任务时**失败返回**而非永久挂起 ——
//! 三者缺一，重启后的屏幕要么涂花、要么陷入崩溃循环、要么把客户端卡死。

use morion::exec;
use morion::syscall::*;

/// 被监督的服务: `(域号, 盘上镜像名)`。
///
/// 域号是 ABI (见内核对域布局的注释)，与 `kernel/src/main.rs` 建域顺序一致。盘上镜像名只
/// 在内存镜像不可用时用作回退路径。
const SUPERVISED: [(u64, &str); 14] = [
    (2, "pager"),
    (3, "echo"),
    (4, "kbd"),
    (6, "fat32_srv"),
    (9, "mount_srv"),
    (10, "tmpfs_srv"),
    (11, "mfs_srv"),
    (12, "ext2_srv"),
    (13, "exfat_srv"),
    (15, "gfx_srv"),
    (16, "net_srv"),
    (21, "netstack_srv"),
    (22, "e1000e_srv"),
    (23, "httpd_srv"),
];

/// 巡检周期 (ms)。
const POLL_MS: u64 = 40;

/// 盘上服务目录 (FAT32 根卷)。
const SERVICE_DIR: &str = "/system/services/";

/// 最小权限策略 (② 能力审计): 高价值凭证的**允许持有者**白名单。
///
/// 只钉这四类"独占 / 危险"凭证 —— `SendTo` / `MapInto` / `Irq` 是常规流通能力, 百来条,
/// 设全局上限只会变成噪音。判据来源: 内核 `main.rs` 引导期 `cap::grant` 的实际签发
/// (域号见那里的域布局注释)。任何**不在**白名单里的持有都记一次违规。
const POLICY: [(u64, &[u64]); 4] = [
    // 造进程: 仅 app(7) / shell(8) / init(14) —— 其余域一概不该有。
    (CAP_KIND_SPAWN, &[7, 8, 14]),
    // MMIO: 仅设备驱动 block(5) / net(16) / vblk(17) / ahci(18) / xhci(19) / e1000e(22)。
    (CAP_KIND_MMIO, &[5, 16, 17, 18, 19, 22]),
    // 帧缓冲: 全局唯一凭证, 仅 gfx_srv(15)。
    (CAP_KIND_FB, &[15]),
    // I/O 端口: 仅 block(5, IDE PIT) / mfs(11) / exfat(13) (CMOS RTC)。
    (CAP_KIND_IO_PORT, &[5, 11, 13]),
];

/// 正向要求: 这些域**必须**持有这些能力 (缺了说明授权遗漏或审计读取失灵)。
///
/// 有它"审计通过"才不是空话 —— 若 `SYS_CAP_AUDIT` 读不到任何能力 (门禁拒了 / 编码坏了),
/// 这几条会立刻把它暴露成 `MISSING`。选 `Spawn` 是因为它**无条件**签发 (不依赖是否探测到
/// 某台 PCI 设备), 故在任意测试配置下都成立。
const REQUIRED: [(u64, u64); 3] = [
    (7, CAP_KIND_SPAWN),  // app
    (8, CAP_KIND_SPAWN),  // shell
    (14, CAP_KIND_SPAWN), // init (本域)
];

/// 每个被监督域「上一轮已发现它没有存活任务」的标记（两连击去抖，见 [`run`]）。
static mut PENDING: [bool; SUPERVISED.len()] = [false; SUPERVISED.len()];

/// 监督循环: 每轮检查一遍被监督域，缺谁补谁。
///
/// **两连击去抖**: 一次「没有存活任务」只记一笔，下一轮仍是同样结果才重启。
/// 这样做的两个理由:
///  * 单次采样可能正踩在服务刚退出/刚起步的边界上，连看两轮更稳；
///  * 「死亡」这个状态因此**至少持续一个巡检周期** (`POLL_MS`)，轮询式的观察者
///    (如自测 FS-29 断言"它一度没有存活任务") 才可能稳定看到它 —— 否则从内存镜像
///    重启只要一两个 tick，窗口短到轮询方根本采样不到。
pub fn run() {
    println(
        "init: supervising pager/echo/kbd/fat32_srv/mount_srv/tmpfs_srv/mfs_srv/ext2_srv/exfat_srv/gfx_srv/net_srv",
    );
    // 引导期能力审计 (②): 授权已在内核 `main.rs` 全部签发完毕, 这里按最小权限策略核对一遍,
    // 把"内核给了谁什么"落成可复核、可回归断言的启动日志 (marker `cap-audit:`)。
    audit();
    let mut restarts = 0u64;
    loop {
        for (i, &(domain, name)) in SUPERVISED.iter().enumerate() {
            if sys_domain_alive(domain) != 0 {
                unsafe {
                    PENDING[i] = false;
                }
                continue;
            }
            // 两连击: 第一次发现"没有存活任务"只记一笔, 下一轮仍然如此才重启。
            if !unsafe { PENDING[i] } {
                unsafe {
                    PENDING[i] = true;
                }
                continue;
            }
            unsafe {
                PENDING[i] = false;
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

// ---------------------------------------------------------------------------
// 能力审计 / 策略引擎 (②)
// ---------------------------------------------------------------------------

/// 引导期能力审计: 遍历全部存活域的能力槽, 按 [`POLICY`] / [`REQUIRED`] 核对最小权限。
///
/// 由持 `Capability::Spawn` 的监督者调用 (内核 `SYS_CAP_AUDIT` 的门禁)。只读、一次性:
/// 内核侧授权在 `main.rs` 已全部签发, 这里把它们摊开成启动日志里可复核、可回归断言的一段
/// (marker `cap-audit:`)。审计不通过只报告、不阻断启动 —— 让系统照常起来, 把问题留在日志。
fn audit() {
    let domains = sys_domain_count();
    let mut caps = 0u64;
    let mut violations = 0u64;
    let mut d = 0;
    while d < domains {
        let mut slot = 0;
        loop {
            let v = sys_cap_audit(d, slot);
            if v == u64::MAX {
                break; // 槽越界: 该域槽表到底
            }
            if v != 0 {
                caps += 1;
                let kind = (v >> 56).wrapping_sub(1);
                if !holder_allowed(d, kind) {
                    violations += 1;
                    print("cap-audit: VIOLATION dom=");
                    print_u64(d);
                    print(" cap=");
                    print(cap_name(kind));
                    println("");
                }
            }
            slot += 1;
        }
        d += 1;
    }
    // 正向核对: 该有的能力缺了, 同样是策略被破坏 (也挡住"审计读不到任何能力"的空通过)。
    for &(dom, kind) in REQUIRED.iter() {
        if !holds(dom, kind) {
            violations += 1;
            print("cap-audit: MISSING dom=");
            print_u64(dom);
            print(" cap=");
            print(cap_name(kind));
            println("");
        }
    }
    print("cap-audit: domains=");
    print_u64(domains);
    print(" caps=");
    print_u64(caps);
    print(" violations=");
    print_u64(violations);
    println("");
    if violations == 0 {
        println("cap-audit: OK (least-privilege policy holds)");
    } else {
        println("cap-audit: FAILED (capability policy violated)");
    }
}

/// `domain` 是否被允许持有 `kind` 类能力 (不在 [`POLICY`] 里的种类一律放行)。
fn holder_allowed(domain: u64, kind: u64) -> bool {
    match POLICY.iter().find(|(k, _)| *k == kind) {
        Some((_, allow)) => allow.contains(&domain),
        None => true,
    }
}

/// `domain` 是否持有 `kind` 类能力 (任一槽命中即可)。
fn holds(domain: u64, kind: u64) -> bool {
    let mut slot = 0;
    loop {
        let v = sys_cap_audit(domain, slot);
        if v == u64::MAX {
            return false; // 表尾
        }
        if v != 0 && (v >> 56).wrapping_sub(1) == kind {
            return true;
        }
        slot += 1;
    }
}

/// 能力种类名 (仅用于审计日志); 与内核 `cap::CAP_KIND_*` 一致。
fn cap_name(kind: u64) -> &'static str {
    match kind {
        CAP_KIND_SEND_TO => "SendTo",
        CAP_KIND_MAP_INTO => "MapInto",
        CAP_KIND_IRQ => "Irq",
        CAP_KIND_MMIO => "Mmio",
        CAP_KIND_SPAWN => "Spawn",
        CAP_KIND_FB => "Fb",
        CAP_KIND_IO_PORT => "IoPort",
        _ => "Unknown",
    }
}
