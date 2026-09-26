//! Morion OS 用户态测试程序 (Ring 3)
//!
//! 由内核在运行时加载到用户空间基址 USER_SPACE_BASE, 经 `switch_to_user`
//! 首次切入 Ring 3。入口 `_start` 必须位于镜像最前端 (offset 0)。

#![no_std]
#![no_main]
// 部分预留 syscall 与演示代码暂未使用, 故允许 dead_code。
#![allow(dead_code)]

mod syscall;
mod vfs;

use syscall::{
    print, print_u64, println, sys_alloc_page, sys_backspace, sys_call, sys_call_payload,
    sys_cap_issue, sys_cap_lookup, sys_cap_send, sys_clear, sys_handle_send, sys_map_anon,
    sys_page_fault_reply, sys_port_in16, sys_port_in8, sys_port_out16, sys_port_out8, sys_readline,
    sys_recv, sys_recv_msg, sys_register_irq, sys_reply, sys_scroll_down, sys_scroll_up, sys_send,
    sys_share_page, sys_term_left, sys_term_put, sys_term_right, sys_unmap, sys_virt_to_phys,
    CAP_KIND_IRQ, CAP_KIND_MAP_INTO, CAP_KIND_SEND_TO, PAYLOAD_LEN,
};

/// 各服务域 id (与内核 `main.rs` 创建顺序一致)。
///   0 sender / 1 receiver / 2 pager / 3 echo / 4 kbd
///   5 block_srv / 6 fat32_srv / 7 app / 8 shell
///   9 mount_srv / 10 tmpfs_srv
const BLOCK_DOMAIN: u64 = 5;
const APP_DOMAIN: u64 = 7;

/// 用户态固定数据区基址 (与内核约定): `USER_BASE + 8 MiB`。
/// 布置在程序镜像 (自 `USER_BASE` 起, 随代码增长) 与用户栈 (`USER_BASE + 4 MiB`)
/// 之上, 避免镜像增长后踩到这些固定映射。子区域:
///   `+0x80_0000` = sender/receiver 共享页; `+0x81_0000` = NVMe 配置页 (同内核 nvme.rs)。
const USER_DATA_BASE: u64 = 0x0000_0080_0080_0000;
/// sender/receiver 共享页虚拟地址 (双方约定同一地址)。
const SHARED_PAGE: u64 = USER_DATA_BASE;

/// 用户程序入口 — 内核已设好用户栈 (rsp) 与用户参数 (rdi=域 id),
/// 此处按所属域 id 分流到不同角色后退出。
#[link_section = ".text._start"]
#[no_mangle]
pub extern "C" fn _start(domain_id: u64) -> ! {
    match domain_id {
        0 => sender_main(),
        1 => receiver_main(),
        2 => pager_main(),
        3 => echo_main(),
        4 => kbd_main(),
        5 => block_main(),
        6 => fat32_main(),
        7 => app_main(),
        8 => shell_main(),
        9 => mount_main(),
        10 => tmpfs_main(),
        11 => mfs_main(),
        12 => ext2_main(),
        13 => exfat_main(),
        _ => {}
    }
    syscall::sys_exit();
}

/// 「能力随 IPC 传递」自测: 句柄指向的**不透明**对象标识 —— 内核不解释它的含义,
/// 只负责在移交时原样搬运 (真实的文件 fd 也是这么打包的: `(服务域 << 32) | fd`)。
const CAP_TOKEN: u64 = 0x5A5A_1234_5678_9ABC;
/// `sender` 通知 `receiver`「新句柄已就绪」的 tag 基数: 低 8 位放句柄索引。
const CAP_HANDLE_TAG: u64 = 0xCA00;

/// 域 0 — 发送者: 持有 SendTo(1) + MapInto(1) + SendTo(3) 能力, **不持有** SendTo(2)。
/// 成功路径保持静默 (避免刷屏), 仅失败时打印。
fn sender_main() {
    // 共享内存演示: 申请一页 → 写入 → 共享给域 1 → IPC 通知。
    let page = SHARED_PAGE;
    if sys_alloc_page(page) != 1 {
        println("sender: alloc_page FAILED");
        return;
    }
    let msg: &str = "HELLO SHARED";
    unsafe {
        core::ptr::copy_nonoverlapping(msg.as_ptr(), page as *mut u8, msg.len());
    }
    if sys_share_page(page, 1) != 1 {
        println("sender: share_page DENIED");
    }
    // IPC 通知 receiver 读取 (复用现有 SendTo 能力)。
    sys_send(1, 777);

    // 解除本域映射: 引用计数 2 -> 1, 帧不释放 (receiver 仍持有)。
    if sys_unmap(page) != 1 {
        println("sender: unmap FAILED");
    }

    // 按需分页演示: 访问一个从未映射的用户空间地址触发缺页 (由 pager 域补零帧)。
    // 必须是 canonical 用户空间地址 (P4[1])。
    let fault_addr = 0x81_0000_0000u64;
    let _ = unsafe { core::ptr::read_volatile(fault_addr as *const u64) };

    // ---- 能力随 IPC 传递 (M-C1) ----
    // 负例一: 委派自己**没有**的能力必须被拒 —— sender 持有 SendTo(1)/SendTo(3),
    // 但从不持有 SendTo(2)。没有的能力给不出去 (无放大), 这是能力模型的根。
    if sys_cap_send(1, CAP_KIND_SEND_TO, 2) != 0 {
        println("sender: delegate unheld SendTo(2) NOT denied FAILED");
    }
    // 负例二: 连「往不可达域塞能力」都不允许 —— 没有 SendTo(2), 目标域校验先失败。
    if sys_cap_send(2, CAP_KIND_SEND_TO, 3) != 0 {
        println("sender: cap_send to unreachable domain FAILED");
    }
    // 负例三: 参数非法的能力 (Irq 的 arg 超出 u8) 必须被解码层拒绝。
    if sys_cap_send(1, CAP_KIND_IRQ, 0x1234) != 0 {
        println("sender: bad cap arg NOT rejected FAILED");
    }

    // 句柄移交: 为一个不透明对象签发句柄, 再把它**移入** receiver 域。
    let h = sys_cap_issue(CAP_TOKEN);
    if h == u64::MAX {
        println("sender: cap_issue FAILED");
        return;
    }
    let moved = sys_handle_send(1, h);
    if moved == u64::MAX {
        println("sender: handle_send FAILED");
        return;
    }
    // 移动语义: 移走之后**本域的句柄必须立即失效** (能力同一时刻只属于一个域)。
    if sys_cap_lookup(h) != u64::MAX {
        println("sender: handle still valid after move FAILED");
    }

    // 能力委派: 把 SendTo(3) 复制给 receiver —— 它因此获得与 echo 通信的能力,
    // 而启动期没有任何静态授权让它能这么做 (receiver 的能力槽是空的)。
    if sys_cap_send(1, CAP_KIND_SEND_TO, 3) != 1 {
        println("sender: delegate SendTo(3) FAILED");
    }
    // 同一项能力重复委派必须幂等 (不再占新槽): 连做两次都应成功。
    if sys_cap_send(1, CAP_KIND_MAP_INTO, 1) != 1 || sys_cap_send(1, CAP_KIND_MAP_INTO, 1) != 1 {
        println("sender: duplicate delegation NOT idempotent FAILED");
    }

    // 通知 receiver 去核验 (句柄索引编进 tag 低 8 位)。
    // 必须检查返回值: 这条通知若没送出去, receiver 会**永久阻塞在第二次 recv**
    // 而不报任何错 —— 是本次自测里唯一的静默挂死路径。
    if sys_send(1, CAP_HANDLE_TAG | moved) != 1 {
        println("sender: capability transfer notification FAILED");
    }

    // 同步 IPC call/reply 演示: 调用 echo 服务 (域 3)。
    let _ = sys_call(3, 0xABCD);
}

/// 域 1 — 接收者: 经 IPC 收到通知后, 直接从共享页读取数据, 再解除映射;
/// 最后核验 sender 交过来的句柄与能力。
fn receiver_main() {
    // 反面对照 (关键): receiver 启动时**持有零个能力**, 此刻调用 echo 必须失败。
    // 后面同样的调用在拿到委派来的 SendTo(3) 之后必须成功 —— 一负一正,
    // 才能证明「能力确实是靠这次传递获得的」而不是本来就有的。
    if sys_call(3, 0x1234) != u64::MAX {
        println("receiver: call echo WITHOUT capability unexpectedly OK FAILED");
    }

    // 等 sender 通知共享页就绪。
    let tag = sys_recv();
    if tag != 777 {
        println("receiver: unexpected first tag FAILED");
    }

    // 直接读共享页 (零拷贝, 数据未经 IPC 传递)。
    let page = SHARED_PAGE;
    let _ = unsafe { core::slice::from_raw_parts(page as *const u8, 12) };

    // 解除映射: 引用计数 1 -> 0, 真正释放帧。
    if sys_unmap(page) != 1 {
        println("receiver: unmap FAILED");
    }

    // ---- 能力随 IPC 传递 (M-C1): 核验收到的句柄与能力 ----
    let tag = sys_recv();
    if tag & !0xFF != CAP_HANDLE_TAG {
        println("receiver: capability transfer notification FAILED");
        return;
    }
    let h = tag & 0xFF;
    // 收到的句柄必须能查出 sender 当初放进去的那个不透明对象。
    if sys_cap_lookup(h) != CAP_TOKEN {
        println("receiver: transferred handle lookup FAILED");
    }
    // 用委派来的 SendTo(3) 直接调 echo —— 这正是本函数开头做不到的那件事。
    if sys_call(3, 0xCAFE) != 0xCAFF {
        println("receiver: call echo via delegated capability FAILED");
    }
    // 负例: receiver 依然没有 SendTo(2), 不能往 pager 塞能力。
    if sys_cap_send(2, CAP_KIND_SEND_TO, 3) != 0 {
        println("receiver: cap_send to unreachable domain FAILED");
    }

    // 成功标记: 本段其余步骤都「静默通过」, 但「静默」无法区分「跑过并通过」与
    // 「根本没跑到」(例如阻塞在上面的 recv)。故这一行**必须打印** —— 它是本原语
    // 被真正执行过的唯一正面证据。上面 8 条负例/正例中任意一条失败都会另打 FAILED。
    println("receiver: capability passing OK (handle moved + SendTo(3) delegated)");
}

/// IPC 消息 (与内核 `ipc::Message` 布局一致: 24 字节头 + `PAYLOAD_LEN`)。
#[repr(C)]
#[allow(dead_code)]
struct Message {
    from: u64,
    to: u64,
    tag: u64,
    payload: [u8; PAYLOAD_LEN],
}

/// 缺页信息 (与内核 `pager::PageFaultInfo` 布局一致, 24 字节)。
#[repr(C)]
#[derive(Clone, Copy)]
struct PageFaultInfo {
    fault_domain: u64,
    fault_addr: u64,
    error_code: u64,
}

/// 域 2 — 分页器: 经通用 IPC 阻塞接收缺页消息, 映射匿名零帧并回复。
fn pager_main() {
    loop {
        let mut msg = Message {
            from: 0,
            to: 0,
            tag: 0,
            payload: [0; PAYLOAD_LEN],
        };
        sys_recv_msg(&mut msg as *mut Message as *mut u8);

        // 从 payload 前 24 字节解出缺页信息。
        let info: PageFaultInfo =
            unsafe { core::ptr::read_unaligned(msg.payload.as_ptr() as *const PageFaultInfo) };

        if sys_map_anon(info.fault_domain, info.fault_addr) != 1 {
            println("pager: map_anon FAILED");
        }

        sys_page_fault_reply();
    }
}

/// 域 3 — echo 服务: 同步 IPC 演示, 循环 `recv` → `reply` (回显 tag + 1)。
fn echo_main() {
    loop {
        let tag = sys_recv();
        // 回复当前调用者 (回复目标由内核在 recv 时记录)。
        if sys_reply(tag + 1) != 1 {
            println("echo: reply FAILED");
        }
    }
}

/// scancode set 1 基础键 → (无 shift, 有 shift) 字节；`0` 表示非字符键。
/// 索引即 scancode (0..0x60)。修饰键 (shift/ctrl/alt/caps) 与扩展键不在此列。
const KEYMAP: [(u8, u8); 0x60] = {
    let mut m = [(0u8, 0u8); 0x60];
    m[0x02] = (b'1', b'!');
    m[0x03] = (b'2', b'@');
    m[0x04] = (b'3', b'#');
    m[0x05] = (b'4', b'$');
    m[0x06] = (b'5', b'%');
    m[0x07] = (b'6', b'^');
    m[0x08] = (b'7', b'&');
    m[0x09] = (b'8', b'*');
    m[0x0A] = (b'9', b'(');
    m[0x0B] = (b'0', b')');
    m[0x0C] = (b'-', b'_');
    m[0x0D] = (b'=', b'+');
    m[0x0F] = (b'\t', b'\t');
    m[0x10] = (b'q', b'Q');
    m[0x11] = (b'w', b'W');
    m[0x12] = (b'e', b'E');
    m[0x13] = (b'r', b'R');
    m[0x14] = (b't', b'T');
    m[0x15] = (b'y', b'Y');
    m[0x16] = (b'u', b'U');
    m[0x17] = (b'i', b'I');
    m[0x18] = (b'o', b'O');
    m[0x19] = (b'p', b'P');
    m[0x1A] = (b'[', b'{');
    m[0x1B] = (b']', b'}');
    m[0x1E] = (b'a', b'A');
    m[0x1F] = (b's', b'S');
    m[0x20] = (b'd', b'D');
    m[0x21] = (b'f', b'F');
    m[0x22] = (b'g', b'G');
    m[0x23] = (b'h', b'H');
    m[0x24] = (b'j', b'J');
    m[0x25] = (b'k', b'K');
    m[0x26] = (b'l', b'L');
    m[0x27] = (b';', b':');
    m[0x28] = (b'\'', b'"');
    m[0x29] = (b'`', b'~');
    m[0x2B] = (b'\\', b'|');
    m[0x2C] = (b'z', b'Z');
    m[0x2D] = (b'x', b'X');
    m[0x2E] = (b'c', b'C');
    m[0x2F] = (b'v', b'V');
    m[0x30] = (b'b', b'B');
    m[0x31] = (b'n', b'N');
    m[0x32] = (b'm', b'M');
    m[0x33] = (b',', b'<');
    m[0x34] = (b'.', b'>');
    m[0x35] = (b'/', b'?');
    m[0x39] = (b' ', b' ');
    m
};

/// 查询 scancode 对应的字符字节 (按 shift 状态)；非字符键返回 `None`。
fn key_char(sc: u8, shift: bool) -> Option<u8> {
    let i = sc as usize;
    if i >= KEYMAP.len() {
        return None;
    }
    let (base, shifted) = KEYMAP[i];
    let c = if shift { shifted } else { base };
    if c == 0 {
        None
    } else {
        Some(c)
    }
}

/// 域 4 — 用户态键盘驱动: 注册接收 IRQ1, 循环接收 scancode 并解码成字符回显。
/// 方向键 (E0 前缀) 滚动控制台历史, 其余键位按 shift 状态输出对应字符。
fn kbd_main() {
    if sys_register_irq(1) != 1 {
        println("kbd: register irq1 FAILED");
        return;
    }

    let mut ext = false;
    let mut shift = false;
    loop {
        let sc = sys_recv() as u8;

        // E0 扩展前缀: 标记后续字节为扩展键码。
        if sc == 0xE0 {
            ext = true;
            continue;
        }
        // 释放码 (bit7 置位): 只处理按下码; shift 释放时清除状态。
        if sc & 0x80 != 0 {
            let base = sc & 0x7F;
            if base == 0x2A || base == 0x36 {
                shift = false;
            }
            ext = false;
            continue;
        }

        // 扩展按下码 (方向键等)。
        if ext {
            ext = false;
            match sc {
                0x48 => {
                    sys_scroll_up(); // ↑ 滚动历史
                }
                0x50 => {
                    sys_scroll_down(); // ↓ 滚动历史
                }
                0x4B => {
                    sys_term_left(); // ← 光标左移
                }
                0x4D => {
                    sys_term_right(); // → 光标右移
                }
                0x1C => {
                    sys_term_put(b'\n'); // 数字键盘 Enter = E0 0x1C, 同样提交当前行
                }
                _ => {}
            }
            continue;
        }

        // 普通按下码。
        match sc {
            0x2A | 0x36 => shift = true, // 左右 shift 按下
            0x0E => {
                sys_backspace(); // 退格
            }
            0x1C => {
                sys_term_put(b'\n'); // 回车
            }
            _ => {
                if let Some(c) = key_char(sc, shift) {
                    sys_term_put(c);
                }
            }
        }
    }
}

// ===========================================================================
// 域 5 — NVMe 块设备驱动服务 (文件系统阶段 1)
// ===========================================================================

/// 内核映射到本域的配置结构虚拟地址 (见 kernel/src/nvme.rs, 属 USER_DATA_BASE 区)。
const NVME_CFG_VADDR: u64 = USER_DATA_BASE + 0x1_0000;
/// 配置结构 magic 校验值 (与内核一致)。
const NVME_CONFIG_MAGIC: u64 = 0x004E_564D_454F_5321;

/// NVMe 配置结构 (与内核 `nvme::NvmeConfig` 布局完全一致)。
#[repr(C)]
#[derive(Clone, Copy)]
#[allow(dead_code)]
struct NvmeConfig {
    magic: u64,
    bar0_paddr: u64,
    mmio_vaddr: u64,
    asq_paddr: u64,
    acq_paddr: u64,
    isq_paddr: u64,
    icq_paddr: u64,
    data_paddr: u64,
    asq_vaddr: u64,
    acq_vaddr: u64,
    isq_vaddr: u64,
    icq_vaddr: u64,
    data_vaddr: u64,
    admin_qdepth: u16,
    io_qdepth: u16,
    page_size: u32,
}

// NVMe 控制器寄存器偏移 (相对 BAR0, 见 NVMe 规范)。
const REG_CAP: u64 = 0x00;
const REG_VS: u64 = 0x08;
const REG_CC: u64 = 0x14;
const REG_CSTS: u64 = 0x1C;
const REG_AQA: u64 = 0x24;
const REG_ASQ: u64 = 0x28;
const REG_ACQ: u64 = 0x30;
const DOORBELL_BASE: u64 = 0x1000;

// Admin / I/O 命令操作码。
const OP_CREATE_IO_SQ: u8 = 0x01;
// NVM I/O 命令 Write (0x01)。与 Admin 的 Create I/O SQ 同码, 但用于 I/O 队列。
const OP_WRITE: u8 = 0x01;
const OP_READ: u8 = 0x02;
const OP_CREATE_IO_CQ: u8 = 0x05;
const OP_IDENTIFY: u8 = 0x06;

/// 提交队列条目 (SQE, 64 字节)。
#[repr(C)]
#[derive(Clone, Copy)]
struct Sqe {
    opcode: u8,
    flags: u8, // FUSE/PSDT (PRP 时恒 0)
    cid: u16,
    nsid: u32,
    _rsvd1: u32,
    _rsvd2: u32,
    mptr: u64,
    prp1: u64,
    prp2: u64,
    cdw10: u32,
    cdw11: u32,
    cdw12: u32,
    cdw13: u32,
    cdw14: u32,
    cdw15: u32,
}

impl Sqe {
    fn zero() -> Self {
        Sqe {
            opcode: 0,
            flags: 0,
            cid: 0,
            nsid: 0,
            _rsvd1: 0,
            _rsvd2: 0,
            mptr: 0,
            prp1: 0,
            prp2: 0,
            cdw10: 0,
            cdw11: 0,
            cdw12: 0,
            cdw13: 0,
            cdw14: 0,
            cdw15: 0,
        }
    }
}

/// 完成队列条目 (CQE, 16 字节)。
#[repr(C)]
#[derive(Clone, Copy)]
struct Cqe {
    dw0: u32,
    dw1: u32,
    sqhd: u16,
    sqid: u16,
    cid: u16, // bytes 12-13: command id (与 Linux/QEMU 布局一致)
    sf: u16,  // bytes 14-15: status field, bit0 = phase, bit1.. = status code
}

/// 易失 MMIO 读/写 (寄存器映射为非缓存, 必须用 volatile)。
fn rd32(addr: u64) -> u32 {
    unsafe { core::ptr::read_volatile(addr as *const u32) }
}
fn rd64(addr: u64) -> u64 {
    unsafe { core::ptr::read_volatile(addr as *const u64) }
}
fn wr32(addr: u64, val: u32) {
    unsafe { core::ptr::write_volatile(addr as *mut u32, val) }
}
fn wr64(addr: u64, val: u64) {
    unsafe { core::ptr::write_volatile(addr as *mut u64, val) }
}

/// 向指定队列提交一条命令并轮询其完成。返回状态码是否为 0 (成功)。
///
/// `sq_vaddr`/`cq_vaddr` 为队列内存虚拟地址, `sq_doorbell`/`cq_doorbell`
/// 为门铃寄存器虚拟地址 (含 stride), `qdepth` 为队列深度。
#[allow(clippy::too_many_arguments)]
fn submit_wait(
    sq_vaddr: u64,
    cq_vaddr: u64,
    sq_doorbell: u64,
    cq_doorbell: u64,
    qdepth: u32,
    mmio: u64,
    sqe: Sqe,
    tail: &mut u32,
    head: &mut u32,
    phase: &mut u32,
) -> bool {
    let idx = (*tail % qdepth) as u64;
    unsafe {
        core::ptr::write_volatile((sq_vaddr + idx * 64) as *mut Sqe, sqe);
    }
    *tail = (*tail + 1) % qdepth;
    wr32(sq_doorbell, *tail);

    // 轮询完成队列。QEMU 用 timer 异步投递 CQE, 需要其主循环运行才会 post;
    // 而 guest 在 KVM 里纯轮询不会触发 VM exit, 主循环被阻塞。故每次迭代读一次
    // CSTS (MMIO) 强制 VM exit, 让 QEMU 主循环有机会 post CQE。
    for _ in 0..NVME_POLL_LIMIT {
        let idx = (*head % qdepth) as u64;
        let cqe: Cqe = unsafe { core::ptr::read_volatile((cq_vaddr + idx * 16) as *const Cqe) };
        if (cqe.sf & 1) as u32 == *phase {
            *head = (*head + 1) % qdepth;
            if *head == 0 {
                *phase ^= 1;
            }
            wr32(cq_doorbell, *head);
            let sc = cqe.sf >> 1;
            if sc != 0 {
                print("nvme: CQE fail sc=");
                print_u64(sc as u64);
                print(" cid=");
                print_u64(cqe.cid as u64);
                print(" sqid=");
                print_u64(cqe.sqid as u64);
                print(" op=");
                print_u64(sqe.opcode as u64);
                print(" cdw10=");
                print_u64(sqe.cdw10 as u64);
                print(" cdw11=");
                print_u64(sqe.cdw11 as u64);
                print(" nlb-1=");
                print_u64(sqe.cdw12 as u64);
                print(" nsid=");
                print_u64(sqe.nsid as u64);
                print(" prp1=");
                print_u64(sqe.prp1);
                println("");
            }
            return sc == 0;
        }
        // 读 CSTS 触发 VM exit (无副作用, 只读状态寄存器)。
        let _ = rd32(mmio + REG_CSTS);
    }
    // 轮询超时: 打印队列状态与命令 id, 便于定位。
    print("nvme: CQE timeout head=");
    print_u64(*head as u64);
    print(" tail=");
    print_u64(*tail as u64);
    print(" phase=");
    print_u64(*phase as u64);
    print("sqe_cid=");
    print_u64(sqe.cid as u64);
    println("");
    // 调试: 转储原始 CQ (槽 0/1) 与 SQ (槽 0/1) 的 16 字节, 判断是「从未投递」还是「phase 不符」。
    let d0 = unsafe { core::ptr::read_volatile(cq_vaddr as *const u64) };
    let d1 = unsafe { core::ptr::read_volatile((cq_vaddr + 8) as *const u64) };
    let d2 = unsafe { core::ptr::read_volatile((cq_vaddr + 16) as *const u64) };
    let d3 = unsafe { core::ptr::read_volatile((cq_vaddr + 24) as *const u64) };
    print("    CQ0 lo=");
    print_u64(d0);
    print(" hi=");
    print_u64(d1);
    println("");
    print("    CQ1 lo=");
    print_u64(d2);
    print(" hi=");
    print_u64(d3);
    println("");
    let s0 = unsafe { core::ptr::read_volatile(sq_vaddr as *const u64) };
    let s1 = unsafe { core::ptr::read_volatile((sq_vaddr + 64) as *const u64) };
    print("    SQ0 dw0=");
    print_u64(s0);
    print(" SQ1 dw0=");
    print_u64(s1);
    println("");
    false
}

/// 轮询 CQE 的迭代上限。
///
/// 每次迭代读一次 CSTS 强制 VM exit, 让 QEMU 主循环有机会投递 CQE; 上限按
/// 「最多等约几秒」设定 —— 预算过紧时, 宿主机负载稍高 (如 MFS 大量 COW 写)
/// 就会误判超时, 使一次正常的块读写失败。
const NVME_POLL_LIMIT: u32 = 1_000_000;
/// 逻辑扇区大小 (与块层约定一致)。
const NVME_SECTOR_SIZE: usize = 512;
/// 页大小 (NVMe 的 PRP 粒度)。
const NVME_PAGE_SIZE: usize = 4096;
/// 单条 NVMe 命令的扇区上限 (256 扇区 = 128 KiB, 与 `BlockReq.count` 约定一致)。
/// 更大的请求由 block_srv 拆成多条命令。
const NVME_MAX_SECTORS: u16 = 256;
/// block_srv 私有的 PRP 表页虚拟地址 (> 2 页的传输共用一页表项空间)。
/// 与 `VOL_SCRATCH_VADDR` 同理: 必须落在所有共享缓冲段之上 (见该常量处的地址分区表)。
const PRP_LIST_VADDR: u64 = 0x0000_0080_0016_1000;

/// 经 NVMe I/O 队列读/写 `count` 个扇区到 `buf` (页对齐的用户页)。
///
/// `buf` 为调用方共享给本域的缓冲页虚拟地址, 已映射; 先经 `sys_virt_to_phys`
/// 反查物理地址作为 NVMe DMA 的 PRP1。
///
/// **多页传输**: 单次命令可覆盖多页 (最多 `NVME_MAX_SECTORS` 扇区 = 128 KiB), 按 NVMe
/// 规范组织 PRP —— 1 页只用 PRP1; 2 页时 PRP2 直接指向第 2 页; 超过 2 页时 PRP2 指向
/// 本域私有的 **PRP 表页**, 表项依次是第 2..N 页的物理地址 (末项可指向半页, 长度由
/// NLB 决定)。因为要让每页单独反查物理地址, 调用方的缓冲必须是**逐页映射**的连续
/// 虚拟区间 (页式共享天然满足); 多页时首地址必须页对齐。
#[allow(clippy::too_many_arguments)]
fn nvme_rw_sectors(
    opcode: u8,
    nsid: u32,
    cfg: &NvmeConfig,
    mmio: u64,
    isq_doorbell: u64,
    icq_doorbell: u64,
    lba: u32,
    count: u16,
    buf: *mut u8,
    tail: &mut u32,
    head: &mut u32,
    phase: &mut u32,
) -> bool {
    if count == 0 || count > NVME_MAX_SECTORS {
        return false;
    }
    let bytes = count as usize * NVME_SECTOR_SIZE;
    let pages = bytes.div_ceil(NVME_PAGE_SIZE);
    let paddr = sys_virt_to_phys(buf as u64);
    if paddr == 0 {
        return false;
    }
    let mut prp2: u64 = 0;
    if pages > 1 {
        // 多页传输: PRP1 必须是页地址, 其余页由 PRP2 (或 PRP 表) 描述。
        if !paddr.is_multiple_of(NVME_PAGE_SIZE as u64) {
            return false;
        }
        if pages == 2 {
            let p = sys_virt_to_phys(buf as u64 + NVME_PAGE_SIZE as u64);
            if p == 0 {
                return false;
            }
            prp2 = p;
        } else {
            let lp = sys_virt_to_phys(PRP_LIST_VADDR);
            if lp == 0 {
                return false;
            }
            let list = PRP_LIST_VADDR as *mut u64;
            let mut i = 1usize;
            while i < pages {
                let p = sys_virt_to_phys(buf as u64 + (i * NVME_PAGE_SIZE) as u64);
                if p == 0 || !p.is_multiple_of(NVME_PAGE_SIZE as u64) {
                    return false;
                }
                unsafe {
                    core::ptr::write_volatile(list.add(i - 1), p);
                }
                i += 1;
            }
            prp2 = lp;
        }
    }
    let mut sqe = Sqe::zero();
    sqe.opcode = opcode;
    sqe.cid = 5;
    sqe.nsid = nsid;
    sqe.prp1 = paddr;
    sqe.prp2 = prp2;
    sqe.cdw10 = lba;
    sqe.cdw11 = 0; // SLBA 高 32 位 = 0
    sqe.cdw12 = (count as u32) - 1; // NLB (0-based)
    submit_wait(
        cfg.isq_vaddr,
        cfg.icq_vaddr,
        isq_doorbell,
        icq_doorbell,
        cfg.io_qdepth as u32,
        mmio,
        sqe,
        tail,
        head,
        phase,
    )
}

/// NVMe 枚举到的 namespace 上限 (Identify CNS=2 返回的 NSID 列表)。
const NVME_MAX_NS: usize = 8;
/// 枚举到的 namespace id 列表 (升序) 与数量, 由 `nvme_main` 初始化时填充。
/// 卷层据此逐盘扫描分区表。
static mut NVME_NSIDS: [u32; NVME_MAX_NS] = [0; NVME_MAX_NS];
static mut NVME_NS_COUNT: usize = 0;

/// 每个 namespace 的容量 (扇区数), 由 Identify Namespace (CNS=0) 的 NSZE 取得;
/// 0 = 未知 (该盘不支持查询 / 查询失败)。与 `NVME_NSIDS` 下标一一对应。
///
/// 为什么需要它: 卷层对「无分区表的整盘」记 `sectors = 0`(容量未知), 而 MFS 首次
/// 格式化要按**卷的真实几何**决定文件系统大小 —— 少了 NSZE 就只能退回写死的默认值,
/// 在一整块新盘上会格式出一个小得离谱的文件系统。
static mut NVME_NS_SECTORS: [u32; NVME_MAX_NS] = [0; NVME_MAX_NS];

/// 取 namespace `nsid` 的容量 (扇区数)。
///
/// Identify Namespace (CNS=0) 的返回数据里偏移 0 是 NSZE (u64), 在 512 B 逻辑块下
/// 即扇区数。走 **Admin 队列**: 该命令只在初始化阶段按盘各发一次, 不该占用 I/O 队列。
/// 返回值超出 u32 的容量 (≥ 2 TiB) 截断 —— 卷层的 `sectors` 本就是 u32。
#[allow(clippy::too_many_arguments)] // 与 vol_scan_namespace 等一样, 需透传队列上下文
fn nvme_ns_sectors(
    nsid: u32,
    cfg: &NvmeConfig,
    mmio: u64,
    asq_doorbell: u64,
    acq_doorbell: u64,
    tail: &mut u32,
    head: &mut u32,
    phase: &mut u32,
) -> u32 {
    let mut sqe = Sqe::zero();
    sqe.opcode = OP_IDENTIFY;
    sqe.cid = 0x100 + nsid as u16; // 与初始化阶段已用的 cid 1..4 错开
    sqe.nsid = nsid;
    sqe.prp1 = cfg.data_paddr;
    sqe.cdw10 = 0x00; // CNS=0 = Identify Namespace
    if !submit_wait(
        cfg.asq_vaddr,
        cfg.acq_vaddr,
        asq_doorbell,
        acq_doorbell,
        cfg.admin_qdepth as u32,
        mmio,
        sqe,
        tail,
        head,
        phase,
    ) {
        return 0;
    }
    let nsze = rd64(cfg.data_vaddr);
    if nsze > u32::MAX as u64 {
        u32::MAX
    } else {
        nsze as u32
    }
}

/// 查已缓存的 namespace 容量 (扇区数); 0 = 未知。
fn nvme_ns_sectors_of(nsid: u32) -> u32 {
    let n = unsafe { NVME_NS_COUNT };
    let mut i = 0usize;
    while i < n {
        if unsafe { NVME_NSIDS[i] } == nsid {
            return unsafe { NVME_NS_SECTORS[i] };
        }
        i += 1;
    }
    0
}

/// 域 5 — NVMe 驱动服务: 复位控制器 → Admin 队列 → Identify → I/O 队列 → 块服务。
fn nvme_main() {
    let cfg = unsafe { core::ptr::read_volatile(NVME_CFG_VADDR as *const NvmeConfig) };
    if cfg.magic != NVME_CONFIG_MAGIC {
        println("nvme: bad config magic, aborting");
        return;
    }
    let mmio = cfg.mmio_vaddr;

    // 1. 读 CAP, 计算门铃 stride (DSTRD 在 CAP 的 bits 32:35, stride = 4 << DSTRD 字节)。
    let cap = rd64(mmio + REG_CAP);
    let dstrd = ((cap >> 32) & 0xF) as u64;
    let stride = 4u64 << dstrd;

    // 2. 禁用控制器 (CC.EN=0), 等 CSTS.RDY 清零。
    wr32(mmio + REG_CC, 0);
    let mut timeout = 0u64;
    while rd32(mmio + REG_CSTS) & 1 != 0 {
        timeout += 1;
        if timeout > 1_000_000 {
            println("nvme: timeout waiting CSTS.RDY=0");
            return;
        }
    }

    // 3. 配置 Admin 队列属性 + 基地址。
    let qsize = (cfg.admin_qdepth as u32 - 1) & 0xFFF;
    wr32(mmio + REG_AQA, qsize | (qsize << 16));
    wr64(mmio + REG_ASQ, cfg.asq_paddr);
    wr64(mmio + REG_ACQ, cfg.acq_paddr);

    // 4. 使能控制器 (IOSQES=6 => 64B @bits16-19, IOCQES=4 => 16B @bits20-23, MPS=0 => 4KB)。
    //    与 Linux include/linux/nvme.h 一致: NVME_CC_IOSQES = 6<<16, NVME_CC_IOCQES = 4<<20。
    wr32(mmio + REG_CC, 1 | (6 << 16) | (4 << 20));

    // 5. 等 CSTS.RDY 置位。
    timeout = 0;
    while rd32(mmio + REG_CSTS) & 1 == 0 {
        timeout += 1;
        if timeout > 1_000_000 {
            println("nvme: timeout waiting CSTS.RDY=1");
            return;
        }
    }

    // Admin 队列门铃 (SQ0 tail / CQ0 head)。
    let asq_doorbell = mmio + DOORBELL_BASE;
    let acq_doorbell = mmio + DOORBELL_BASE + stride;
    let mut admin_tail: u32 = 0;
    let mut admin_head: u32 = 0;
    // CQ phase tag 首条为 1 (NVMe 规范 / QEMU NVMe 行为), 故初始期望 phase=1,
    // 避免把被清零的 CQ 槽 (phase=0) 误判为已完成的 CQE。
    let mut admin_phase: u32 = 1;

    // 6. Identify Controller (CNS=1) → 打印型号 / 序列号。
    let mut sqe = Sqe::zero();
    sqe.opcode = OP_IDENTIFY;
    sqe.cid = 1;
    sqe.prp1 = cfg.data_paddr;
    sqe.cdw10 = 0x01;
    if !submit_wait(
        cfg.asq_vaddr,
        cfg.acq_vaddr,
        asq_doorbell,
        acq_doorbell,
        cfg.admin_qdepth as u32,
        mmio,
        sqe,
        &mut admin_tail,
        &mut admin_head,
        &mut admin_phase,
    ) {
        println("nvme: Identify Controller FAILED");
        return;
    }

    // 6b. 从 Identify Controller 取 NN (Number of Namespaces, 数据偏移 516)。
    //     QEMU 把各 `nvme-ns` 连续编号为 1..NN, 故 namespace 列表即 1..=NN。
    //     (不用 Identify CNS=2: 实测部分 QEMU 版本返回的列表不完整。)
    //     注意必须在此处读: 紧接着的 CNS=0 会覆盖同一数据页。
    let nn = rd32(cfg.data_vaddr + 516) as usize;
    {
        let n = if nn > NVME_MAX_NS { NVME_MAX_NS } else { nn };
        let mut i = 0usize;
        while i < n {
            unsafe {
                NVME_NSIDS[i] = (i + 1) as u32;
            }
            i += 1;
        }
        unsafe {
            NVME_NS_COUNT = n;
        }
    }

    // 7. 若 NN 不可用 (为 0), 回退按当前 QEMU 配置假定 namespace 为 1..=3。
    //    必须在取 NSZE 之前: 下一步要按 namespace 列表逐个查询容量。
    if unsafe { NVME_NS_COUNT } == 0 {
        unsafe {
            NVME_NSIDS[0] = 1;
            NVME_NSIDS[1] = 2;
            NVME_NSIDS[2] = 3;
            NVME_NS_COUNT = 3;
        }
    }

    // 7b. 逐个 namespace 取容量 (Identify Namespace CNS=0 → NSZE)。
    //     卷层用它给「无分区表的整盘」填 sectors —— 此前那一栏恒为 0 (容量未知),
    //     于是文件系统只能按写死的默认值格式化, 一整块盘会被格成迷你分区。
    {
        let n = unsafe { NVME_NS_COUNT };
        let mut i = 0usize;
        while i < n {
            let nsid = unsafe { NVME_NSIDS[i] };
            let sec = nvme_ns_sectors(
                nsid,
                &cfg,
                mmio,
                asq_doorbell,
                acq_doorbell,
                &mut admin_tail,
                &mut admin_head,
                &mut admin_phase,
            );
            unsafe {
                NVME_NS_SECTORS[i] = sec;
            }
            i += 1;
        }
    }

    // 8. Create I/O Completion Queue (qid=1)。
    sqe = Sqe::zero();
    sqe.opcode = OP_CREATE_IO_CQ;
    sqe.cid = 3;
    sqe.prp1 = cfg.icq_paddr;
    sqe.cdw10 = 1 | ((cfg.io_qdepth as u32 - 1) << 16);
    sqe.cdw11 = 1; // PC=1 (物理连续)
    if !submit_wait(
        cfg.asq_vaddr,
        cfg.acq_vaddr,
        asq_doorbell,
        acq_doorbell,
        cfg.admin_qdepth as u32,
        mmio,
        sqe,
        &mut admin_tail,
        &mut admin_head,
        &mut admin_phase,
    ) {
        println("nvme: Create I/O CQ FAILED");
        return;
    }

    // 9. Create I/O Submission Queue (qid=1, 关联 CQ1)。
    sqe = Sqe::zero();
    sqe.opcode = OP_CREATE_IO_SQ;
    sqe.cid = 4;
    sqe.prp1 = cfg.isq_paddr;
    sqe.cdw10 = 1 | ((cfg.io_qdepth as u32 - 1) << 16);
    sqe.cdw11 = 1 | (1 << 16); // PC=1, CQID=1
    if !submit_wait(
        cfg.asq_vaddr,
        cfg.acq_vaddr,
        asq_doorbell,
        acq_doorbell,
        cfg.admin_qdepth as u32,
        mmio,
        sqe,
        &mut admin_tail,
        &mut admin_head,
        &mut admin_phase,
    ) {
        println("nvme: Create I/O SQ FAILED");
        return;
    }
    // 10. 进入块设备服务循环: 经 IPC 接收 BlockReq, 用 NVMe I/O 队列读扇区。
    let isq_doorbell = mmio + DOORBELL_BASE + 2 * stride;
    let icq_doorbell = mmio + DOORBELL_BASE + 3 * stride;
    let mut io_tail: u32 = 0;
    let mut io_head: u32 = 0;
    // 与 Admin 队列同理: 首条 completion 的 phase tag 为 1。
    let mut io_phase: u32 = 1;

    // 10b. 卷层初始化: 分配扫描缓冲页, 逐 namespace 解析分区表并登记卷。
    //      此后 `dev` 一律是「卷号」, 实际 I/O 用 (vol.nsid, vol.start_lba + lba)。
    if sys_alloc_page(VOL_SCRATCH_VADDR) != 1 {
        println("nvme: alloc vol scratch FAILED");
        return;
    }
    // 多页 DMA 的 PRP 表页 (> 2 页的传输用它描述第 2..N 页)。
    if sys_alloc_page(PRP_LIST_VADDR) != 1 {
        println("nvme: alloc prp list FAILED");
        return;
    }
    let scratch = VOL_SCRATCH_VADDR as *mut u8;
    vol_reset();
    let nsn = unsafe { NVME_NS_COUNT };
    let mut nsi = 0usize;
    while nsi < nsn {
        let nsid = unsafe { NVME_NSIDS[nsi] };
        vol_scan_namespace(
            nsid,
            &cfg,
            mmio,
            isq_doorbell,
            icq_doorbell,
            scratch,
            &mut io_tail,
            &mut io_head,
            &mut io_phase,
        );
        nsi += 1;
    }
    // 卷表 (启动诊断, 也是 `mkfs.mfs <卷号>` 的卷号来源)。
    vol_print_table();

    loop {
        let mut msg = Message {
            from: 0,
            to: 0,
            tag: 0,
            payload: [0; PAYLOAD_LEN],
        };
        sys_recv_msg(&mut msg as *mut Message as *mut u8);

        if msg.tag != BLOCK_REQ_TAG {
            print("nvme: unexpected tag=");
            print_u64(msg.tag);
            println("");
            sys_reply(0);
            continue;
        }
        let req: BlockReq =
            unsafe { core::ptr::read_unaligned(msg.payload.as_ptr() as *const BlockReq) };

        // `dev` 是「卷号」(卷层已把分区起始 LBA 合并进卷描述)。
        let dev = (req.op >> 8) as usize;
        let opcode_low = (req.op & 0xFF) as u8;
        match opcode_low {
            BLOCK_OP_READ | BLOCK_OP_WRITE => {
                let vol = match vol_get(dev) {
                    Some(v) => v,
                    None => {
                        sys_reply(0);
                        continue;
                    }
                };
                let opcode = if opcode_low == BLOCK_OP_READ {
                    OP_READ
                } else {
                    OP_WRITE
                };
                let base_lba = vol.start_lba.saturating_add(req.lba as u32);
                let total = req.count;
                if total == 0 {
                    sys_reply(0);
                    continue;
                }
                // 超过单条命令上限 (256 扇区 = 128 KiB) 的请求按命令上限切分:
                // 每段起始都落在页边界上 (256 * 512 = 128 KiB = 32 页), 故
                // 除最后一段外每段的缓冲区都是页对齐的。
                let mut done: u64 = 0;
                let mut ok = true;
                while done < total {
                    let chunk = (total - done).min(NVME_MAX_SECTORS as u64) as u16;
                    let seg_buf = (req.buf as *mut u8).wrapping_add((done * 512) as usize);
                    if !nvme_rw_sectors(
                        opcode,
                        vol.nsid,
                        &cfg,
                        mmio,
                        isq_doorbell,
                        icq_doorbell,
                        base_lba.saturating_add(done as u32),
                        chunk,
                        seg_buf,
                        &mut io_tail,
                        &mut io_head,
                        &mut io_phase,
                    ) {
                        ok = false;
                        break;
                    }
                    done += chunk as u64;
                }
                sys_reply(if ok { 1 } else { 0 });
            }
            BLOCK_OP_LIST_VOLUMES => {
                // 把卷描述符数组写进调用方共享页, 回复卷数。
                let dst = req.buf as *mut u8;
                let max = req.count as usize;
                let n = core::cmp::min(max, unsafe { VOL_COUNT });
                let dsize = core::mem::size_of::<VolumeDesc>();
                let mut i = 0usize;
                while i < n {
                    let v = unsafe { VOLUMES[i] };
                    let d = VolumeDesc {
                        id: i as u32,
                        nsid: v.nsid,
                        start_lba: v.start_lba,
                        sectors: v.sectors,
                        kind: v.kind,
                        _pad: 0,
                    };
                    unsafe {
                        core::ptr::write_unaligned(dst.add(i * dsize) as *mut VolumeDesc, d);
                    }
                    i += 1;
                }
                sys_reply(n as u64);
            }
            _ => {
                sys_reply(0);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 域 5 — 磁盘驱动服务 (IDE PIO, 文件系统阶段 1)
// ---------------------------------------------------------------------------
// Legacy IDE (PATA) primary 通道 I/O 端口, 用 ATA PIO 命令读扇区, 不依赖
// DMA / MMIO / MSI-X / PCI bus master, 是最简单的块设备访问路径。
const IDE_DATA: u16 = 0x1F0; // 16 位数据寄存器 (读/写)
const IDE_SECT_CNT: u16 = 0x1F2; // 扇区数
const IDE_LBA_LO: u16 = 0x1F3; // LBA 位 0-7
const IDE_LBA_MID: u16 = 0x1F4; // LBA 位 8-15
const IDE_LBA_HI: u16 = 0x1F5; // LBA 位 16-23
const IDE_DRIVE: u16 = 0x1F6; // 驱动器/磁头 (bit6=1 LBA 模式, bit7=1 master)
const IDE_STATUS: u16 = 0x1F7; // 状态 (读) / 命令 (写)
const IDE_CMD: u16 = 0x1F7; // 命令寄存器 (写)

/// ATA 命令: READ SECTORS (28 位 LBA)。
const ATA_READ_SECTORS: u8 = 0x20;
/// ATA 命令: WRITE SECTORS (28 位 LBA)。
const ATA_WRITE_SECTORS: u8 = 0x30;

/// 状态寄存器位。
const ATA_BSY: u8 = 0x80; // busy
const ATA_DRDY: u8 = 0x40; // drive ready
const ATA_DRQ: u8 = 0x08; // data request
const ATA_ERR: u8 = 0x01; // error

/// 从 I/O 端口读一个 16 位小端字并写入缓冲区 (避开对齐要求)。
unsafe fn read_sector_word(buf: *mut u8, i: usize) {
    let w = sys_port_in16(IDE_DATA);
    core::ptr::write_unaligned(buf.add(i * 2) as *mut u16, w);
}

/// 从缓冲区读一个 16 位小端字并写入 I/O 端口 (避开对齐要求)。
unsafe fn write_sector_word(buf: *const u8, i: usize) {
    let w = core::ptr::read_unaligned(buf.add(i * 2) as *const u16);
    sys_port_out16(IDE_DATA, w);
}

/// 读 LBA 起 `count` 个扇区到 `buf` (28 位 LBA, PIO 模式)。`count` 取值 1..=256
/// (写入 SECT_CNT 时 256 自动回绕为 0)。`buf` 需至少 `count * 512` 字节。成功返回 true。
fn read_sectors(lba: u32, count: u16, buf: *mut u8) -> bool {
    if count == 0 || count > 256 {
        return false;
    }

    // 1. 等控制器就绪: BSY 清零且 DRDY 置位。
    let mut ready = false;
    for _ in 0..100_000 {
        let s = sys_port_in8(IDE_STATUS);
        if s & ATA_BSY == 0 && s & ATA_DRDY != 0 {
            ready = true;
            break;
        }
    }
    if !ready {
        return false;
    }

    // 2. 写扇区数与 LBA 参数 (28 位 LBA)。
    sys_port_out8(IDE_SECT_CNT, (count & 0xFF) as u8); // 256 -> 0
    sys_port_out8(IDE_LBA_LO, (lba & 0xFF) as u8);
    sys_port_out8(IDE_LBA_MID, ((lba >> 8) & 0xFF) as u8);
    sys_port_out8(IDE_LBA_HI, ((lba >> 16) & 0xFF) as u8);
    sys_port_out8(IDE_DRIVE, 0xE0 | ((lba >> 24) & 0x0F) as u8); // master + LBA
    sys_port_out8(IDE_CMD, ATA_READ_SECTORS);

    // 3. 逐扇区等待 DRQ 后读 256 个 16 位字 (512 字节)。
    for i in 0..count as usize {
        let mut got_drq = false;
        for _ in 0..100_000 {
            let s = sys_port_in8(IDE_STATUS);
            if s & ATA_BSY != 0 {
                continue;
            }
            if s & ATA_ERR != 0 {
                return false;
            }
            if s & ATA_DRQ != 0 {
                got_drq = true;
                break;
            }
        }
        if !got_drq {
            return false;
        }
        let sector = unsafe { buf.add(i * 512) };
        for w in 0..256usize {
            unsafe { read_sector_word(sector, w) };
        }
    }
    true
}

/// 写 LBA 起 `count` 个扇区 (`buf` 为数据源, 28 位 LBA, PIO 模式)。
/// `count` 取值 1..=256 (写入 SECT_CNT 时 256 自动回绕为 0)。成功返回 true。
fn write_sectors(lba: u32, count: u16, buf: *mut u8) -> bool {
    if count == 0 || count > 256 {
        return false;
    }

    // 1. 等控制器就绪: BSY 清零且 DRDY 置位。
    let mut ready = false;
    for _ in 0..100_000 {
        let s = sys_port_in8(IDE_STATUS);
        if s & ATA_BSY == 0 && s & ATA_DRDY != 0 {
            ready = true;
            break;
        }
    }
    if !ready {
        return false;
    }

    // 2. 写扇区数与 LBA 参数 (28 位 LBA)。
    sys_port_out8(IDE_SECT_CNT, (count & 0xFF) as u8); // 256 -> 0
    sys_port_out8(IDE_LBA_LO, (lba & 0xFF) as u8);
    sys_port_out8(IDE_LBA_MID, ((lba >> 8) & 0xFF) as u8);
    sys_port_out8(IDE_LBA_HI, ((lba >> 16) & 0xFF) as u8);
    sys_port_out8(IDE_DRIVE, 0xE0 | ((lba >> 24) & 0x0F) as u8); // master + LBA
    sys_port_out8(IDE_CMD, ATA_WRITE_SECTORS);

    // 3. 逐扇区等待 DRQ 后写 256 个 16 位字 (512 字节)。
    for i in 0..count as usize {
        let mut got_drq = false;
        for _ in 0..100_000 {
            let s = sys_port_in8(IDE_STATUS);
            if s & ATA_BSY != 0 {
                continue;
            }
            if s & ATA_ERR != 0 {
                return false;
            }
            if s & ATA_DRQ != 0 {
                got_drq = true;
                break;
            }
        }
        if !got_drq {
            return false;
        }
        let sector = unsafe { buf.add(i * 512) };
        for w in 0..256usize {
            unsafe { write_sector_word(sector, w) };
        }
    }
    true
}

/// 块设备请求 tag (block_srv 据此识别读/写请求)。
const BLOCK_REQ_TAG: u64 = 0x424C_4F43; // "BLOC"

/// 块设备请求 (序列化进 IPC payload, 32 字节, 与内核 `PAYLOAD_LEN` 一致)。
/// `buf` 为数据缓冲页虚拟地址, 须已由调用方共享映射进 block_srv 地址空间。
///
/// `op` 打包了**卷号**与操作码: 低 8 位 = 操作码 (0 读 / 1 写 / 2 查询卷表),
/// 高位 = 卷号。卷号由 block_srv 的卷层分配 (扫描各 namespace 的 MBR/GPT 分区表;
/// 无分区表的 namespace 视为一个整盘卷)。之所以打包而非新增字段, 是因为 payload
/// 恰好 32 字节, 已无空位。
#[repr(C)]
#[derive(Clone, Copy)]
struct BlockReq {
    op: u64,    // (volume << 8) | opcode
    lba: u64,   // 卷内起始扇区号 (块层会加上分区偏移)
    count: u64, // 扇区数 (>0; 超过单条 NVMe 命令上限时由 block_srv 自动切分)
    buf: u64,   // 数据缓冲页虚拟地址; opcode=2 时为卷描述符输出页
}

/// fat32_srv 的**默认卷**号, 启动时由 `vol_claim` 认领 (见 `fat32_main`)。
/// 回退值 0 对应 `build/nvme.img` (namespace 1)。
static mut FAT_VOL: u64 = 0;

/// 本服务**当前请求**落在的卷号 (M1b 多卷挂载)。
///
/// 除默认卷 `FAT_VOL` 外, 本服务还会为额外挂载的 FAT 卷 (`/usb<卷号>`) 服务: 路径
/// 类请求的 tag 高位带卷编码, fd 类请求由 fd 里绑定的卷决定 (见 `fd_lookup`)。
/// 分派时把解出的卷写进这个「当前卷寄存器」, 于是所有既有的读写扇区调用无需改签名
/// 就自动落在正确的卷上 —— 服务是**单任务串行**处理请求的, 不存在并发覆盖问题。
static mut FAT_CUR_VOL: u64 = 0;

/// 经 IPC 请求 block_srv 读 `count` 个扇区到 `buf`。成功返回 true。
///
/// fat32_srv 通过它间接访问块设备, 而非直接触碰 IDE 端口; IDE PIO 逻辑
/// 收拢在 block_srv 内, 符合微内核「驱动服务化」的解耦。
fn block_read(lba: u32, count: u16, buf: *mut u8) -> bool {
    block_read_dev(unsafe { FAT_CUR_VOL }, lba, count, buf)
}

/// 经 IPC 请求 block_srv 从 `buf` 写 `count` 个扇区到磁盘。成功返回 true。
fn block_write(lba: u32, count: u16, buf: *mut u8) -> bool {
    block_write_dev(unsafe { FAT_CUR_VOL }, lba, count, buf)
}

/// 带卷号的读 (卷号由卷层分配, 0 = 第一个卷)。
fn block_read_dev(dev: u64, lba: u32, count: u16, buf: *mut u8) -> bool {
    let req = BlockReq {
        op: dev << 8,
        lba: lba as u64,
        count: count as u64,
        buf: buf as u64,
    };
    let payload = unsafe {
        core::slice::from_raw_parts(
            &req as *const BlockReq as *const u8,
            core::mem::size_of::<BlockReq>(),
        )
    };
    sys_call_payload(BLOCK_DOMAIN, BLOCK_REQ_TAG, payload) == 1
}

/// 带卷号的写 (卷号由卷层分配, 0 = 第一个卷)。
fn block_write_dev(dev: u64, lba: u32, count: u16, buf: *mut u8) -> bool {
    let req = BlockReq {
        op: (dev << 8) | 1,
        lba: lba as u64,
        count: count as u64,
        buf: buf as u64,
    };
    let payload = unsafe {
        core::slice::from_raw_parts(
            &req as *const BlockReq as *const u8,
            core::mem::size_of::<BlockReq>(),
        )
    };
    sys_call_payload(BLOCK_DOMAIN, BLOCK_REQ_TAG, payload) == 1
}

/// 读一个 16 位小端无符号整数 (引导扇区字段)。
fn read_u16(ptr: *const u8) -> u16 {
    unsafe {
        let lo = core::ptr::read_volatile(ptr) as u16;
        let hi = core::ptr::read_volatile(ptr.add(1)) as u16;
        lo | (hi << 8)
    }
}

/// 读一个 32 位小端无符号整数 (引导扇区 / FSInfo 字段)。
fn read_u32(ptr: *const u8) -> u32 {
    unsafe {
        let b0 = core::ptr::read_volatile(ptr) as u32;
        let b1 = core::ptr::read_volatile(ptr.add(1)) as u32;
        let b2 = core::ptr::read_volatile(ptr.add(2)) as u32;
        let b3 = core::ptr::read_volatile(ptr.add(3)) as u32;
        b0 | (b1 << 8) | (b2 << 16) | (b3 << 24)
    }
}

/// 写一个 16 位小端无符号整数 (目录项字段)。
fn write_u16(ptr: *mut u8, v: u16) {
    unsafe {
        core::ptr::write_volatile(ptr, (v & 0xFF) as u8);
        core::ptr::write_volatile(ptr.add(1), (v >> 8) as u8);
    }
}

/// 写一个 32 位小端无符号整数 (目录项 / FAT 表项)。
fn write_u32(ptr: *mut u8, v: u32) {
    unsafe {
        core::ptr::write_volatile(ptr, (v & 0xFF) as u8);
        core::ptr::write_volatile(ptr.add(1), ((v >> 8) & 0xFF) as u8);
        core::ptr::write_volatile(ptr.add(2), ((v >> 16) & 0xFF) as u8);
        core::ptr::write_volatile(ptr.add(3), ((v >> 24) & 0xFF) as u8);
    }
}

/// 从共享页 `buf` 起读一条 NUL 结尾的路径, 返回其字节切片。
///
/// 上限 `PAYLOAD_LEN - 1` —— 与客户端 `vfs` 写入共享页时的截断长度一致, 服务端不会
/// 越过它去扫描整页。页内无 NUL 时按上限截断。
fn page_path(buf: u64) -> &'static [u8] {
    let p = buf as *const u8;
    let mut n = 0usize;
    while n < PAYLOAD_LEN - 1 && unsafe { *p.add(n) } != 0 {
        n += 1;
    }
    unsafe { core::slice::from_raw_parts(p, n) }
}

/// 解析 `PathReq` (STAT / CHMOD 共用): 返回 (共享页地址, 页内路径)。
///
/// 路径走共享页而 payload 只带附加参数与缓冲地址 —— 单条 payload 装不下路径 +
/// 地址, 且结果页要按客户端指定 (app 与 shell 的地址不同)。
fn parse_path_req(payload: *const u8) -> (u64, &'static str) {
    let req: vfs::PathReq = unsafe { core::ptr::read_unaligned(payload as *const vfs::PathReq) };
    let p = page_path(req.buf);
    (req.buf, unsafe { core::str::from_utf8_unchecked(p) })
}

/// 从 `TwoPathReq` 取出调用方共享页里的两条路径 (`src\0dst`), 交给 `f`。
///
/// 长度必须自洽且落在单条 IPC 可达范围内, 否则视为坏请求 (不信任客户端给的长度)。
/// `f` 是泛型而非 `fn` 指针: 软链接还要带一个 owner 参数, 用闭包捕获比再多传一层更直接。
fn with_two_paths<F: FnOnce(&str, &str) -> u64>(payload: *const u8, f: F) -> u64 {
    let req: vfs::TwoPathReq =
        unsafe { core::ptr::read_unaligned(payload as *const vfs::TwoPathReq) };
    let total = req.a_len as usize + 1 + req.b_len as usize;
    if total > PAYLOAD_LEN || req.buf == 0 {
        return u64::MAX;
    }
    unsafe {
        let p = req.buf as *const u8;
        let a = core::str::from_utf8_unchecked(core::slice::from_raw_parts(p, req.a_len as usize));
        let b = core::str::from_utf8_unchecked(core::slice::from_raw_parts(
            p.add(req.a_len as usize + 1),
            req.b_len as usize,
        ));
        f(a, b)
    }
}

/// 清零 `len` 字节。
fn zero_bytes(ptr: *mut u8, len: usize) {
    for i in 0..len {
        unsafe {
            core::ptr::write_volatile(ptr.add(i), 0u8);
        }
    }
}

// ---------------------------------------------------------------------------
// FAT32 解析与目录遍历
// ---------------------------------------------------------------------------

/// FAT32 BPB 关键布局参数 (从引导扇区解析)。
struct Fat32Bpb {
    bytes_per_sector: u16,
    sectors_per_cluster: u8,
    reserved_sectors: u16,
    num_fats: u8,
    fat_size: u32,      // 单个 FAT 占用的扇区数
    total_sectors: u32, // BPB_TotSec32 (分区总扇区数)
    root_cluster: u32,
}

impl Fat32Bpb {
    /// 从 LBA 0 (引导扇区) 解析 BPB。
    fn parse(sector0: *const u8) -> Self {
        unsafe {
            Fat32Bpb {
                bytes_per_sector: read_u16(sector0.add(11)),
                sectors_per_cluster: *sector0.add(13),
                reserved_sectors: read_u16(sector0.add(14)),
                num_fats: *sector0.add(16),
                fat_size: read_u32(sector0.add(36)),
                total_sectors: read_u32(sector0.add(32)),
                root_cluster: read_u32(sector0.add(44)),
            }
        }
    }

    /// 每簇字节数。
    fn cluster_bytes(&self) -> u32 {
        self.sectors_per_cluster as u32 * self.bytes_per_sector as u32
    }

    /// 第一个 FAT 区的起始扇区。
    fn fat_start_sector(&self) -> u32 {
        self.reserved_sectors as u32
    }

    /// 数据区 (簇 2) 的起始扇区。
    fn data_start_sector(&self) -> u32 {
        self.reserved_sectors as u32 + self.num_fats as u32 * self.fat_size
    }

    /// 簇号 -> 起始扇区号 (簇 2 是数据区第一个簇)。
    fn cluster_to_sector(&self, cluster: u32) -> u32 {
        self.data_start_sector() + (cluster - 2) * self.sectors_per_cluster as u32
    }

    /// 数据区总簇数 (簇号有效范围 2..=total_clusters+1)。
    fn total_clusters(&self) -> u32 {
        let data_sectors = self.total_sectors - self.data_start_sector();
        data_sectors / self.sectors_per_cluster as u32
    }
}

/// 目录项属性位。
const ATTR_DIRECTORY: u8 = 0x10;
const ATTR_VOLUME_ID: u8 = 0x08;
const ATTR_LONG_NAME: u8 = 0x0F;

/// 读 FAT 表中 `cluster` 指向的下一个簇号 (高 4 位保留, 屏蔽为 28 位)。
fn read_fat_entry(bpb: &Fat32Bpb, cluster: u32, fat_buf: *mut u8) -> u32 {
    let byte_offset = cluster * 4;
    let sector = bpb.fat_start_sector() + byte_offset / 512;
    let index = (byte_offset % 512) / 4;
    block_read(sector, 1, fat_buf);
    read_u32(unsafe { fat_buf.add(index as usize * 4) }) & 0x0FFF_FFFF
}

/// 把簇 `cluster` 的整簇内容读入 `buf` (至少一簇大小)。
fn read_cluster(bpb: &Fat32Bpb, cluster: u32, buf: *mut u8) -> bool {
    let sector = bpb.cluster_to_sector(cluster);
    block_read(sector, bpb.sectors_per_cluster as u16, buf)
}

/// 打印字符串, 不可打印字节 (除换行) 替换为 '.'。用于安全显示文件内容,
/// 避免二进制文件 (如 NVRAM 变量) 中的控制字节扰乱屏幕输出。
fn print_sanitized(s: &str) {
    for &b in s.as_bytes() {
        let c = if b == b'\n' || (0x20..=0x7E).contains(&b) {
            b
        } else {
            b'.'
        };
        let byte = [c];
        print(unsafe { core::str::from_utf8_unchecked(&byte) });
    }
}

/// 从文件 (首簇 `start_cluster`, 大小 `file_size`) 的 `offset` 处读最多 `count`
/// 字节到 `out` (须 >= min(count, file_size-offset) 字节)。返回实际读取字节数;
/// 读盘失败返回 u64::MAX。
// 参数较多但都是读文件所需的几何信息与裸缓冲, 拆分结构体会牵动所有调用点。
#[allow(clippy::too_many_arguments)]
fn read_file_range(
    bpb: &Fat32Bpb,
    start_cluster: u32,
    file_size: u32,
    offset: u32,
    count: u32,
    fat_buf: *mut u8,
    file_buf: *mut u8,
    out: *mut u8,
) -> u64 {
    if offset >= file_size {
        return 0;
    }
    let want = core::cmp::min(count, file_size - offset) as usize;
    let cluster_bytes = bpb.cluster_bytes() as usize;
    let mut cluster = start_cluster;
    let mut absolute = 0usize; // 当前簇首字节在文件内的偏移
    let mut copied = 0usize;

    // 1. 跳过 offset 之前的整簇。
    while absolute + cluster_bytes <= offset as usize {
        absolute += cluster_bytes;
        let next = read_fat_entry(bpb, cluster, fat_buf);
        if next >= 0x0FFF_FFF8 {
            return 0;
        }
        cluster = next;
    }

    // 2. 逐簇拷贝与 [offset, offset+want) 重叠的部分。
    while copied < want {
        if !read_cluster(bpb, cluster, file_buf) {
            return u64::MAX;
        }
        let skip = (offset as usize).saturating_sub(absolute);
        let take = core::cmp::min(want - copied, cluster_bytes - skip);
        unsafe {
            core::ptr::copy_nonoverlapping(file_buf.add(skip), out.add(copied), take);
        }
        copied += take;
        absolute += cluster_bytes;

        if copied < want {
            let next = read_fat_entry(bpb, cluster, fat_buf);
            if next >= 0x0FFF_FFF8 {
                break; // FAT 链提前结束: 数据不完整, 返回已读部分
            }
            cluster = next;
        }
    }
    copied as u64
}

// ---------------------------------------------------------------------------
// FAT32 写支持 (覆盖 + 扩展)
// ---------------------------------------------------------------------------

/// FAT32 链结束标记 (28 位)。
const FAT_EOC: u32 = 0x0FFF_FFFF;

/// 把 `cluster` 对应的 FAT 表项写为 `value` (低 28 位), 并同步写所有 FAT 副本。
fn write_fat_entry(bpb: &Fat32Bpb, cluster: u32, value: u32, fat_buf: *mut u8) -> bool {
    let byte_offset = cluster * 4;
    let sector_off = byte_offset / 512;
    let index = (byte_offset % 512) / 4;

    // 读第一个 FAT 的对应扇区, 改 4 字节槽位 (保留高 4 位保留位)。
    if !block_read(bpb.fat_start_sector() + sector_off, 1, fat_buf) {
        return false;
    }
    let slot = unsafe { fat_buf.add(index as usize * 4) };
    let old = read_u32(slot);
    write_u32(slot, (old & 0xF000_0000) | (value & 0x0FFF_FFFF));

    // 同步写所有 FAT 副本。
    for k in 0..bpb.num_fats as u32 {
        let sector = bpb.fat_start_sector() + k * bpb.fat_size + sector_off;
        if !block_write(sector, 1, fat_buf) {
            return false;
        }
    }
    true
}

/// 把整簇内容 `buf` (至少一簇大小) 写入簇 `cluster`。
fn write_cluster(bpb: &Fat32Bpb, cluster: u32, buf: *mut u8) -> bool {
    let sector = bpb.cluster_to_sector(cluster);
    block_write(sector, bpb.sectors_per_cluster as u16, buf)
}

/// fat32 的簇分配游标 (与 exFAT 的 `EXFAT_ALLOC_HINT` 同义)。
///
/// `find_free_cluster` 从它起向后扫描, 找到后推进到下一簇 —— 避免每次分配都从簇 2
/// 重新扫整张 FAT。没有它, 写一个 100000 字节的文件 (512 B 簇 = 196 簇) 会让
/// 分配变成 O(n²) 次 FAT 读, 实测整套自测被拖到 8 分钟以上还跑不完。
static mut FAT_ALLOC_HINT: u32 = 2;

/// 扫描 FAT 表找第一个空闲簇 (表项 == 0), 无空闲返回 None。
/// 从 `FAT_ALLOC_HINT` 起向后扫描, 扫到表尾回绕到 2, 回到起点仍无空闲则返回 None。
fn find_free_cluster(bpb: &Fat32Bpb, fat_buf: *mut u8) -> Option<u32> {
    let total = bpb.total_clusters();
    let start = unsafe { FAT_ALLOC_HINT }.max(2);
    let mut cluster = start;
    loop {
        if read_fat_entry(bpb, cluster, fat_buf) == 0 {
            unsafe {
                FAT_ALLOC_HINT = if cluster + 1 > total + 1 {
                    2
                } else {
                    cluster + 1
                };
            }
            return Some(cluster);
        }
        cluster += 1;
        if cluster > total + 1 {
            cluster = 2;
        }
        if cluster == start {
            return None; // 扫完一圈仍无空闲
        }
    }
}

/// 把目录项 (父目录簇 `dir_cluster`, 簇内字节偏移 `entry_offset`) 的
/// 首簇号与文件大小写回磁盘。
fn update_dir_entry(
    bpb: &Fat32Bpb,
    dir_cluster: u32,
    entry_offset: u32,
    start_cluster: u32,
    file_size: u32,
    dir_buf: *mut u8,
) -> bool {
    let sector = bpb.cluster_to_sector(dir_cluster) + entry_offset / 512;
    let off = (entry_offset % 512) as usize;
    if !block_read(sector, 1, dir_buf) {
        return false;
    }
    let e = unsafe { dir_buf.add(off) };
    write_u16(unsafe { e.add(20) }, (start_cluster >> 16) as u16);
    write_u16(unsafe { e.add(26) }, (start_cluster & 0xFFFF) as u16);
    write_u32(unsafe { e.add(28) }, file_size);
    block_write(sector, 1, dir_buf)
}

/// 释放从 `start_cluster` 开始的簇链: 沿 FAT 链把每个簇表项清零 (标记为空闲),
/// 直到遇到链结束标记或 0。带链长上限, 防止 FAT 损坏导致的死循环。返回是否完整释放。
fn free_cluster_chain(bpb: &Fat32Bpb, start_cluster: u32, fat_buf: *mut u8) -> bool {
    let mut cluster = start_cluster;
    let mut steps = 0u32;
    while cluster != 0 && cluster < 0x0FFF_FFF8 {
        if steps >= 4096 {
            return false; // 链异常过长
        }
        let next = read_fat_entry(bpb, cluster, fat_buf);
        if !write_fat_entry(bpb, cluster, 0, fat_buf) {
            return false;
        }
        cluster = next;
        steps += 1;
    }
    true
}

/// 写一条完整 32 字节目录项到父目录簇 `dir_cluster` 的 `entry_offset` (簇内字节
/// 偏移) 处。写入短名 `sn` (11 字节)、属性 `attr`、首簇号 `start_cluster` 与大小
/// `file_size`; 时间戳字段暂置 0 (FAT 允许 0, 表示未指定)。返回是否成功。
// 参数较多但都是写一条目录项所需的字段与裸缓冲, 拆分结构体会牵动所有调用点。
#[allow(clippy::too_many_arguments)]
fn write_dir_entry(
    bpb: &Fat32Bpb,
    dir_cluster: u32,
    entry_offset: u32,
    sn: &[u8; 11],
    attr: u8,
    start_cluster: u32,
    file_size: u32,
    dir_buf: *mut u8,
) -> bool {
    let sector = bpb.cluster_to_sector(dir_cluster) + entry_offset / 512;
    let off = (entry_offset % 512) as usize;
    if !block_read(sector, 1, dir_buf) {
        return false;
    }
    let e = unsafe { dir_buf.add(off) };
    for (i, &b) in sn.iter().enumerate() {
        unsafe {
            core::ptr::write_volatile(e.add(i), b);
        }
    }
    unsafe {
        core::ptr::write_volatile(e.add(11), attr);
    }
    zero_bytes(unsafe { e.add(12) }, 8); // 保留 + 创建/访问时间戳置 0
    write_u16(unsafe { e.add(20) }, (start_cluster >> 16) as u16);
    zero_bytes(unsafe { e.add(22) }, 4); // 修改时间/日期置 0
    write_u16(unsafe { e.add(26) }, (start_cluster & 0xFFFF) as u16);
    write_u32(unsafe { e.add(28) }, file_size);
    block_write(sector, 1, dir_buf)
}

/// 在目录 `dir_cluster` 中找一个空闲的 32 字节目录项槽位。
/// 优先复用已删除项 (首字节 0xE5), 否则使用空项 (0x00); 若目录簇全满且链已到
/// 末尾, 分配一个新簇挂到链尾并清零, 返回其首个槽位。
/// 返回 (entry_offset, cluster): 槽位在 `cluster` 簇内的字节偏移。
fn find_dir_slot(
    bpb: &Fat32Bpb,
    dir_cluster: u32,
    dir_buf: *mut u8,
    fat_buf: *mut u8,
) -> Option<(u32, u32)> {
    let entries_per_cluster = bpb.cluster_bytes() as usize / 32;
    let mut cluster = dir_cluster;

    loop {
        if !read_cluster(bpb, cluster, dir_buf) {
            return None;
        }
        for i in 0..entries_per_cluster {
            let entry = unsafe { dir_buf.add(i * 32) };
            let first = unsafe { *entry };
            if first == 0xE5 || first == 0x00 {
                return Some(((i * 32) as u32, cluster));
            }
        }
        // 当前簇满, 尝试下一个目录簇。
        let next = read_fat_entry(bpb, cluster, fat_buf);
        if next >= 0x0FFF_FFF8 {
            // 链尾: 分配新簇挂接并清零。
            let free = find_free_cluster(bpb, fat_buf)?;
            if !write_fat_entry(bpb, cluster, free, fat_buf) {
                return None;
            }
            if !write_fat_entry(bpb, free, FAT_EOC, fat_buf) {
                return None;
            }
            zero_bytes(dir_buf, bpb.cluster_bytes() as usize);
            if !write_cluster(bpb, free, dir_buf) {
                return None;
            }
            return Some((0, free));
        }
        cluster = next;
    }
}

/// 删除父目录簇 `dir_cluster` 中 `entry_offset` 处的条目 (纯底层原语, 不区分
/// 文件/目录): 首字节置 0xE5 标记删除, 并释放其簇链。是否允许删目录由上层
/// (unlink / rmdir 服务分支) 决定。返回是否成功。
fn unlink_entry(
    bpb: &Fat32Bpb,
    dir_cluster: u32,
    entry_offset: u32,
    start_cluster: u32,
    dir_buf: *mut u8,
    fat_buf: *mut u8,
) -> bool {
    // 先释放簇链, 再标记删除, 避免失败后残留半删除状态。
    if start_cluster != 0 && !free_cluster_chain(bpb, start_cluster, fat_buf) {
        return false;
    }
    let sector = bpb.cluster_to_sector(dir_cluster) + entry_offset / 512;
    let off = (entry_offset % 512) as usize;
    if !block_read(sector, 1, dir_buf) {
        return false;
    }
    unsafe {
        core::ptr::write_volatile(dir_buf.add(off), 0xE5);
    }
    block_write(sector, 1, dir_buf)
}

/// 从 `src` (至少 `count` 字节) 取数据, 覆盖/扩展文件 `node`, 从 `offset` 起写
/// `count` 字节。必要时分配新簇并更新 FAT 链与目录项。返回写入字节数, 失败返回
/// u64::MAX; 成功时同步更新 `node` 的 start_cluster / file_size。
// 参数较多但都是写文件所需的裸缓冲与几何信息, 拆分结构体会牵动所有调用点。
#[allow(clippy::too_many_arguments)]
fn write_file_range(
    bpb: &Fat32Bpb,
    node: &mut OpenNode,
    offset: u32,
    count: u32,
    fat_buf: *mut u8,
    file_buf: *mut u8,
    dir_buf: *mut u8,
    src: *const u8,
) -> u64 {
    if count == 0 {
        return 0;
    }
    let cluster_bytes = bpb.cluster_bytes();
    let end = offset.saturating_add(count);
    let new_size = node.file_size.max(end);

    // 1. 沿 FAT 链收集现有簇。
    const MAX_CHAIN: usize = 256;
    let mut chain = [0u32; MAX_CHAIN];
    let mut chain_len = 0usize;
    let mut cluster = node.start_cluster;
    while cluster != 0 && cluster < 0x0FFF_FFF8 {
        if chain_len >= MAX_CHAIN {
            return u64::MAX;
        }
        chain[chain_len] = cluster;
        chain_len += 1;
        cluster = read_fat_entry(bpb, cluster, fat_buf);
        if cluster == 0 {
            break; // 链中途损坏, 停止收集
        }
    }
    let old_clusters = chain_len;

    // 2. 计算所需簇数, 不足则分配新簇并挂到链尾。
    let needed = if new_size == 0 {
        0
    } else {
        (new_size as u64).div_ceil(cluster_bytes as u64) as usize
    };
    if needed > MAX_CHAIN {
        return u64::MAX;
    }
    while chain_len < needed {
        let free = match find_free_cluster(bpb, fat_buf) {
            Some(c) => c,
            None => return u64::MAX,
        };
        if chain_len > 0 && !write_fat_entry(bpb, chain[chain_len - 1], free, fat_buf) {
            return u64::MAX;
        }
        if !write_fat_entry(bpb, free, FAT_EOC, fat_buf) {
            return u64::MAX;
        }
        chain[chain_len] = free;
        chain_len += 1;
    }
    let new_start = if needed > 0 { chain[0] } else { 0 };

    // 3. 逐簇写入数据 (覆盖 + 稀疏间隙清零 + 扩展尾清零)。
    for (i, &c) in chain.iter().enumerate().take(needed) {
        let cs = (i as u32) * cluster_bytes;
        let ce = cs + cluster_bytes;

        // 整个簇落在写区间内 → 直接从 src 拷贝整簇。
        if cs >= offset && ce <= end {
            unsafe {
                core::ptr::copy_nonoverlapping(
                    src.add((cs - offset) as usize),
                    file_buf,
                    cluster_bytes as usize,
                );
            }
            if !write_cluster(bpb, c, file_buf) {
                return u64::MAX;
            }
            continue;
        }

        let write_start = cs.max(offset);
        let write_end = ce.min(end);
        let has_write = write_start < write_end;

        // 完全在旧数据区且与写区间无交集 → 保持原样。
        if i < old_clusters && !has_write && ce <= node.file_size {
            continue;
        }

        // 读-改-写: 先取旧内容或清零 (新分配的簇)。
        if i < old_clusters {
            if !read_cluster(bpb, c, file_buf) {
                return u64::MAX;
            }
        } else {
            zero_bytes(file_buf, cluster_bytes as usize);
        }

        // 旧文件末尾与 offset 之间的空隙清零 (稀疏扩展)。
        if offset > node.file_size {
            let gap_start = cs.max(node.file_size);
            let gap_end = ce.min(offset);
            if gap_start < gap_end {
                zero_bytes(
                    unsafe { file_buf.add((gap_start - cs) as usize) },
                    (gap_end - gap_start) as usize,
                );
            }
        }

        // 覆盖写入区间。
        if has_write {
            unsafe {
                core::ptr::copy_nonoverlapping(
                    src.add((write_start - offset) as usize),
                    file_buf.add((write_start - cs) as usize),
                    (write_end - write_start) as usize,
                );
            }
        }

        // 末簇超出 new_size 的部分清零 (仅扩展时)。
        if new_size > node.file_size && i + 1 == needed && new_size < ce {
            zero_bytes(
                unsafe { file_buf.add((new_size - cs) as usize) },
                (ce - new_size) as usize,
            );
        }

        if !write_cluster(bpb, c, file_buf) {
            return u64::MAX;
        }
    }

    // 4. 目录项与节点描述符同步 (首簇号可能因从空文件分配而改变)。
    if (node.start_cluster != new_start || node.file_size != new_size)
        && !update_dir_entry(
            bpb,
            node.dir_cluster,
            node.entry_offset,
            new_start,
            new_size,
            dir_buf,
        )
    {
        return u64::MAX;
    }
    node.start_cluster = new_start;
    node.file_size = new_size;
    count as u64
}

// ---------------------------------------------------------------------------
// VFAT 长文件名 (LFN)
// ---------------------------------------------------------------------------
// 长名由若干个 32 字节的 LFN 目录项 (attr = 0x0F) 承载, 紧挨在对应的短名项之前,
// 每项存 13 个 UTF-16LE 码元, 并在磁盘上**逆序**排列 (逻辑最后一段在前, 该段
// order 字节带 0x40 标志; 紧邻短名项的是逻辑第 1 段)。
//
// LFN 项布局: [0] order(bit6=末段, 低 6 位为 1 起的段号) [1..11] 5 个码元
//             [11] attr=0x0F [12] type=0 [13] 短名校验和
//             [14..26] 6 个码元 [26..28] 首簇=0 [28..32] 2 个码元

/// LFN 最多段数 (255 码元 / 13 每段, 向上取整)。
const MAX_LFN_ENTRIES: usize = 20;
/// 每段 LFN 存的码元数。
const LFN_CHARS_PER_ENTRY: usize = 13;
/// 长名码元上限 (含结尾 0x0000)。
const LFN_MAX_UNITS: usize = MAX_LFN_ENTRIES * LFN_CHARS_PER_ENTRY + 1;

/// VFAT 长名累加器: 按 LFN 项的段号把 UTF-16 码元填回它在长名中的位置。
#[derive(Clone, Copy)]
struct LfnBuf {
    units: [u16; LFN_MAX_UNITS],
    /// 已填充到的最大码元数 (可能包含结尾的 0x0000)。
    filled: usize,
    /// 当前这一段 LFN 组是否可信 (由带 0x40 标志的项开启)。
    active: bool,
    /// 本组 LFN 项记录的短名校验和 (LFN 项偏移 13; 组内每项都相同)。
    cksum: u8,
}

impl LfnBuf {
    const fn new() -> Self {
        LfnBuf {
            units: [0; LFN_MAX_UNITS],
            filled: 0,
            active: false,
            cksum: 0,
        }
    }

    /// 丢弃当前累积的长名 (遇到删除项 / 卷标 / 短名项之后调用)。
    fn reset(&mut self) {
        self.filled = 0;
        self.active = false;
        self.cksum = 0;
    }

    /// 消化一个 LFN 项, 把其中的码元放到长名中的正确位置。
    fn push(&mut self, entry: *const u8) {
        let order = unsafe { *entry };
        let seq = (order & 0x3F) as usize;
        if order & 0x40 != 0 {
            // 带 0x40 的是逻辑最后一段, 也是磁盘上最先出现的一项 → 新的一组开始。
            self.filled = 0;
            self.active = true;
        }
        if !self.active || seq == 0 || seq > MAX_LFN_ENTRIES {
            return;
        }
        // 短名校验和记在 LFN 项自己的偏移 13 上 (不是短名项)。
        self.cksum = unsafe { *entry.add(13) };
        let base = (seq - 1) * LFN_CHARS_PER_ENTRY;
        // 13 个码元在项内分三段: 5 个 (偏移 1) + 6 个 (偏移 14) + 2 个 (偏移 28)。
        let mut k = 0usize;
        for &start in &[1usize, 14, 28] {
            let seg_len = if start == 1 {
                5
            } else if start == 14 {
                6
            } else {
                2
            };
            for c in 0..seg_len {
                let idx = base + k;
                if idx >= LFN_MAX_UNITS {
                    return;
                }
                self.units[idx] = read_u16(unsafe { entry.add(start + c * 2) });
                k += 1;
            }
        }
        let end = base + LFN_CHARS_PER_ENTRY;
        if end > self.filled {
            self.filled = end.min(LFN_MAX_UNITS);
        }
    }

    /// 取有效长名的码元 (截到第一个 0x0000 结尾); 无效时返回空切片。
    fn name(&self) -> &[u16] {
        if !self.active {
            return &[];
        }
        let end = self.units[..self.filled]
            .iter()
            .position(|&u| u == 0)
            .unwrap_or(self.filled);
        &self.units[..end]
    }

    /// 长名所属的短名项校验和是否与本组 LFN 项记录的一致。
    ///
    /// 校验和按 VFAT 规范对 11 字节短名逐字节计算: 先循环右移一位, 再加上该字节
    /// (`sum = rot_right1(sum) + b`, 按 u8 回绕)。不匹配说明这组 LFN 项不属于该
    /// 短名项 (残留 / 损坏), 此时应退回短名而不是沿用错的长名。
    fn checksum_ok(&self, short_entry: *const u8) -> bool {
        let mut sum: u8 = 0;
        for i in 0..11 {
            let b = unsafe { *short_entry.add(i) };
            sum = sum.rotate_right(1).wrapping_add(b);
        }
        sum == self.cksum
    }
}

/// 把 UTF-16 码元转成 UTF-8 并写入 `out`, 最多 `out.len()` 字节 (只截整字符)。
/// 返回写入字节数。
fn utf16_to_utf8(units: &[u16], out: &mut [u8; vfs::DIR_LONG_MAX]) -> usize {
    let mut n = 0usize;
    for &u in units {
        // BMP 之外 (代理对) 的码元按替换字符处理, 控制字符跳过。
        let cp = if (0xD800..0xE000).contains(&u) {
            u32::from('?')
        } else {
            u32::from(u)
        };
        let mut buf = [0u8; 4];
        let encoded = match char::from_u32(cp) {
            Some(c) => c.encode_utf8(&mut buf).len(),
            None => 0,
        };
        if encoded == 0 {
            continue;
        }
        if n + encoded > out.len() {
            break; // 截断: 放不下的字符直接丢弃
        }
        out[n..n + encoded].copy_from_slice(&buf[..encoded]);
        n += encoded;
    }
    n
}

/// 判断长名码元是否与查询名 `query` (ASCII) 相等 (大小写不敏感)。
fn lfn_name_eq(units: &[u16], query: &[u8]) -> bool {
    if units.len() != query.len() {
        return false;
    }
    for (i, &u) in units.iter().enumerate() {
        if u > 0x7F {
            return false; // 非 ASCII 码元无法与 ASCII 查询匹配
        }
        if ascii_upper(u as u8) != ascii_upper(query[i]) {
            return false;
        }
    }
    true
}

/// 把目录 `dir_cluster` 的条目以结构化 `vfs::DirEntry` 数组写入 `out`,
/// 返回写入字节数 (= 条目数 × size_of::<DirEntry>()); 失败返回 u64::MAX。
///
/// VFAT: 逐个拼接短名项之前的 LFN 项, 得到长名 (校验和不符则退回短名);
/// 条目数按结果页容量 `vfs::RESULT_MAX_ENTRIES` 截断, 不会越界写客户端缓冲页。
fn readdir_into(
    bpb: &Fat32Bpb,
    dir_cluster: u32,
    dir_buf: *mut u8,
    fat_buf: *mut u8,
    out: *mut u8,
) -> u64 {
    let entries_per_cluster = bpb.cluster_bytes() as usize / 32;
    let mut cluster = dir_cluster;
    let mut count = 0usize;
    let dst = out as *mut vfs::DirEntry;
    let mut lfn = LfnBuf::new();

    loop {
        if !read_cluster(bpb, cluster, dir_buf) {
            return u64::MAX;
        }
        for i in 0..entries_per_cluster {
            let entry = unsafe { dir_buf.add(i * 32) };
            let first = unsafe { *entry };
            if first == 0x00 {
                return (count * core::mem::size_of::<vfs::DirEntry>()) as u64; // 目录结束
            }
            if first == 0xE5 {
                lfn.reset(); // 已删除: 其 LFN 组也一并作废
                continue;
            }
            let attr = unsafe { *entry.add(11) };
            if attr & ATTR_LONG_NAME == ATTR_LONG_NAME {
                lfn.push(entry); // 长文件名项: 累积, 等短名项到来时使用
                continue;
            }
            if attr & ATTR_VOLUME_ID != 0 {
                lfn.reset(); // 卷标
                continue;
            }

            let is_dir = attr & ATTR_DIRECTORY != 0;
            // "." / ".." 以 '.' 开头, 跳过 (其 LFN 组同样作废)。
            if is_dir && first == b'.' {
                lfn.reset();
                continue;
            }

            // 结果页只有一页: 放不下就停在这里 (继续扫描只会越界写)。
            if count + 1 > vfs::RESULT_MAX_ENTRIES {
                return (count * core::mem::size_of::<vfs::DirEntry>()) as u64;
            }

            let file_size = read_u32(unsafe { entry.add(28) });
            let mut de = vfs::DirEntry::short([0; 11], file_size as u64, is_dir as u32);
            unsafe {
                core::ptr::copy_nonoverlapping(entry, de.name.as_mut_ptr(), 11);
            }
            // 长名: 仅当 LFN 校验和与这个短名项匹配时才采用 (否则可能张冠李戴)。
            if lfn.active && lfn.checksum_ok(entry) {
                let n = utf16_to_utf8(lfn.name(), &mut de.long);
                de.long_len = n as u8;
            }
            lfn.reset();
            unsafe {
                core::ptr::write_unaligned(dst.add(count), de);
            }
            count += 1;
        }

        // 跨簇: 读下一个目录簇。
        let next = read_fat_entry(bpb, cluster, fat_buf);
        if next >= 0x0FFF_FFF8 {
            return (count * core::mem::size_of::<vfs::DirEntry>()) as u64;
        }
        cluster = next;
    }
}

// ---------------------------------------------------------------------------
// 正斜杠路径解析 (最小闭环: 短名 8.3 查找 + 子目录递归)
// ---------------------------------------------------------------------------

/// 目录项解析结果 (路径查找用)。
struct DirEntryInfo {
    start_cluster: u32,
    file_size: u32,
    attr: u8,
    /// 目录项所在父目录簇 (写回 file_size / start_cluster 用)。
    dir_cluster: u32,
    /// 目录项在父目录簇内的字节偏移 (32 字节对齐)。
    entry_offset: u32,
}

/// ASCII 大写 (仅处理 a-z)。
fn ascii_upper(c: u8) -> u8 {
    if c.is_ascii_lowercase() {
        c - 0x20
    } else {
        c
    }
}

/// 把路径段 (如 "hello.txt" / "dir1") 转成 FAT 8.3 短名 (11 字节, 大写 + 空格填充)。
/// 扩展名按最后一个 '.' 分隔; 主名 > 8 或扩展名 > 3 视为不合法, 返回 None。
fn short_name_from_query(name: &[u8]) -> Option<[u8; 11]> {
    let mut dot = None;
    for (i, &c) in name.iter().enumerate() {
        if c == b'.' {
            dot = Some(i);
        }
    }
    let (base, ext): (&[u8], &[u8]) = match dot {
        Some(d) => (&name[..d], &name[d + 1..]),
        None => (name, &[]),
    };
    if base.len() > 8 || ext.len() > 3 {
        return None;
    }
    let mut sn = [b' '; 11];
    for (i, &c) in base.iter().enumerate() {
        sn[i] = ascii_upper(c);
    }
    for (i, &c) in ext.iter().enumerate() {
        sn[8 + i] = ascii_upper(c);
    }
    Some(sn)
}

/// 比较目录项 11 字节短名与目标短名 (严格相等, FAT 存储为大写)。
fn entry_name_matches(entry: *const u8, sn: &[u8; 11]) -> bool {
    unsafe {
        for (i, &b) in sn.iter().enumerate() {
            if *entry.add(i) != b {
                return false;
            }
        }
    }
    true
}

/// 从短名项取出首簇 / 大小 / 属性, 组装查找结果。
fn entry_info(entry: *const u8, cluster: u32, i: usize) -> DirEntryInfo {
    let cluster_hi = read_u16(unsafe { entry.add(20) }) as u32;
    let cluster_lo = read_u16(unsafe { entry.add(26) }) as u32;
    DirEntryInfo {
        start_cluster: (cluster_hi << 16) | cluster_lo,
        file_size: read_u32(unsafe { entry.add(28) }),
        attr: unsafe { *entry.add(11) },
        dir_cluster: cluster,
        entry_offset: (i * 32) as u32,
    }
}

/// 按名字在目录 `dir_cluster` 中查找条目, 两种匹配方式共用一次扫描:
///
/// 1. **8.3 短名** (`sn`, 11 字节严格比较) —— 优先, 命中立即返回;
/// 2. **VFAT 长名** (与 `query` 做 ASCII 大小写不敏感比较) —— 只在没有短名命中时
///    采用, 且必须 LFN 校验和与所属短名项一致, 否则可能是残留的孤儿 LFN 项。
///
/// `sn` 为 None 表示调用方给的名字本身就不是合法 8.3 (例如含长名) 只按长名找。
/// 支持跨簇目录; 目录正常结束返回已找到的长名命中 (可能为 None)。
fn find_entry_named(
    bpb: &Fat32Bpb,
    dir_cluster: u32,
    sn: Option<&[u8; 11]>,
    query: &[u8],
    dir_buf: *mut u8,
    fat_buf: *mut u8,
) -> Option<DirEntryInfo> {
    let entries_per_cluster = bpb.cluster_bytes() as usize / 32;
    let mut cluster = dir_cluster;
    let mut lfn = LfnBuf::new();
    // 长名命中先记下, 但继续扫描: 短名命中优先级更高。
    let mut long_hit: Option<DirEntryInfo> = None;

    loop {
        if !read_cluster(bpb, cluster, dir_buf) {
            return long_hit;
        }
        for i in 0..entries_per_cluster {
            let entry = unsafe { dir_buf.add(i * 32) };
            let first = unsafe { *entry };
            if first == 0x00 {
                return long_hit; // 目录结束
            }
            if first == 0xE5 {
                lfn.reset();
                continue;
            }
            let attr = unsafe { *entry.add(11) };
            if attr & ATTR_LONG_NAME == ATTR_LONG_NAME {
                lfn.push(entry);
                continue;
            }
            if attr & ATTR_VOLUME_ID != 0 {
                lfn.reset();
                continue;
            }
            let is_dir = attr & ATTR_DIRECTORY != 0;
            let is_dot = is_dir && first == b'.';
            let long_match =
                !is_dot && lfn.active && lfn.checksum_ok(entry) && lfn_name_eq(lfn.name(), query);
            lfn.reset();

            if let Some(sn) = sn {
                if !is_dot && entry_name_matches(entry, sn) {
                    return Some(entry_info(entry, cluster, i));
                }
            }
            if long_match && long_hit.is_none() {
                long_hit = Some(entry_info(entry, cluster, i));
            }
        }
        // 跨簇: 读下一个目录簇。
        let next = read_fat_entry(bpb, cluster, fat_buf);
        if next >= 0x0FFF_FFF8 {
            return long_hit;
        }
        cluster = next;
    }
}

/// 按名字查找目录条目 (短名优先, 回退到 VFAT 长名)。
fn find_entry(
    bpb: &Fat32Bpb,
    dir_cluster: u32,
    name: &[u8],
    dir_buf: *mut u8,
    fat_buf: *mut u8,
) -> Option<DirEntryInfo> {
    let sn = short_name_from_query(name);
    find_entry_named(bpb, dir_cluster, sn.as_ref(), name, dir_buf, fat_buf)
}

/// 在目录 `dir_cluster` 中按 11 字节短名 `sn` 直接查找条目 (支持跨簇目录)。
/// 命中返回首簇/大小/属性, 未命中或读盘失败返回 None。
///
/// 用于创建/删除等**已知短名**的场景 (这些操作只写 8.3 短名, 不涉及长名)。
fn find_entry_sn(
    bpb: &Fat32Bpb,
    dir_cluster: u32,
    sn: &[u8; 11],
    dir_buf: *mut u8,
    fat_buf: *mut u8,
) -> Option<DirEntryInfo> {
    let entries_per_cluster = bpb.cluster_bytes() as usize / 32;
    let mut cluster = dir_cluster;

    loop {
        if !read_cluster(bpb, cluster, dir_buf) {
            return None;
        }
        for i in 0..entries_per_cluster {
            let entry = unsafe { dir_buf.add(i * 32) };
            let first = unsafe { *entry };
            if first == 0x00 {
                return None; // 目录结束
            }
            if first == 0xE5 {
                continue; // 已删除
            }
            let attr = unsafe { *entry.add(11) };
            if attr & ATTR_LONG_NAME == ATTR_LONG_NAME {
                continue; // 长文件名项
            }
            if attr & ATTR_VOLUME_ID != 0 {
                continue; // 卷标
            }
            if !entry_name_matches(entry, sn) {
                continue;
            }
            return Some(entry_info(entry, cluster, i));
        }
        // 跨簇: 读下一个目录簇。
        let next = read_fat_entry(bpb, cluster, fat_buf);
        if next >= 0x0FFF_FFF8 {
            return None;
        }
        cluster = next;
    }
}

/// 按正斜杠路径 (如 "/dir1/nested.txt") 从根目录解析到最终条目。
/// 忽略空段 (连续 '/' 或前导 '/'), 中间段必须是目录, 末段返回条目。
fn resolve_path(
    bpb: &Fat32Bpb,
    path: &str,
    dir_buf: *mut u8,
    fat_buf: *mut u8,
) -> Option<DirEntryInfo> {
    let bytes = path.as_bytes();
    let mut cur_cluster = bpb.root_cluster;
    let mut i = 0usize;

    while i < bytes.len() {
        let mut j = i;
        while j < bytes.len() && bytes[j] != b'/' {
            j += 1;
        }
        let seg = &bytes[i..j];
        if !seg.is_empty() {
            let info = find_entry(bpb, cur_cluster, seg, dir_buf, fat_buf)?;
            // 判断 seg 之后是否还有非空段。
            let mut k = j;
            while k < bytes.len() && bytes[k] == b'/' {
                k += 1;
            }
            if k >= bytes.len() {
                return Some(info); // 末段
            }
            if info.attr & ATTR_DIRECTORY == 0 {
                return None; // 中间段不是目录
            }
            cur_cluster = info.start_cluster;
        }
        i = j + 1;
    }
    None
}

/// 解析 open 路径: 空路径或单个 "/" 视为根目录, 其余按正斜杠路径解析。
fn resolve_open_path(
    bpb: &Fat32Bpb,
    path: &str,
    dir_buf: *mut u8,
    fat_buf: *mut u8,
) -> Option<DirEntryInfo> {
    let trimmed = path.trim_matches('/');
    if trimmed.is_empty() {
        return Some(DirEntryInfo {
            start_cluster: bpb.root_cluster,
            file_size: 0,
            attr: ATTR_DIRECTORY,
            dir_cluster: bpb.root_cluster,
            entry_offset: 0,
        });
    }
    resolve_path(bpb, path, dir_buf, fat_buf)
}

/// 解析"创建/删除"类路径: 最后一个非空段作为目标名 (转成 8.3 短名), 其余段必须
/// 是已存在的目录。返回 (父目录簇, 目标短名); 空路径 / 单个 "/" / 中间段非目录 /
/// 目标名非法时返回 None。
fn resolve_parent(
    bpb: &Fat32Bpb,
    path: &str,
    dir_buf: *mut u8,
    fat_buf: *mut u8,
) -> Option<(u32, [u8; 11])> {
    let bytes = path.as_bytes();
    let mut cur_cluster = bpb.root_cluster;
    let mut i = 0usize;

    while i < bytes.len() {
        let mut j = i;
        while j < bytes.len() && bytes[j] != b'/' {
            j += 1;
        }
        let seg = &bytes[i..j];

        // 跳过斜杠, 判断 seg 之后是否还有非空段。
        let mut k = j;
        while k < bytes.len() && bytes[k] == b'/' {
            k += 1;
        }

        if !seg.is_empty() {
            if k >= bytes.len() {
                // seg 是最后一个非空段 → 目标名。
                let sn = short_name_from_query(seg)?;
                return Some((cur_cluster, sn));
            }
            // 中间段必须是目录。
            let info = find_entry(bpb, cur_cluster, seg, dir_buf, fat_buf)?;
            if info.attr & ATTR_DIRECTORY == 0 {
                return None;
            }
            cur_cluster = info.start_cluster;
        }
        i = j + 1;
    }
    None
}

/// 判断目录 `dir_cluster` 是否为空 (只含 `.` 与 `..`, 或更少)。跨簇遍历, 跳过
/// 已删除项 / 长文件名 / 卷标; 遇到第 3 个有效条目即非空。读盘失败视为非空 (安全)。
fn dir_is_empty(bpb: &Fat32Bpb, dir_cluster: u32, dir_buf: *mut u8, fat_buf: *mut u8) -> bool {
    let entries_per_cluster = bpb.cluster_bytes() as usize / 32;
    let mut cluster = dir_cluster;
    let mut valid = 0u32;

    loop {
        if !read_cluster(bpb, cluster, dir_buf) {
            return false;
        }
        for i in 0..entries_per_cluster {
            let entry = unsafe { dir_buf.add(i * 32) };
            let first = unsafe { *entry };
            if first == 0x00 {
                return valid <= 2; // 目录结束
            }
            if first == 0xE5 {
                continue; // 已删除
            }
            let attr = unsafe { *entry.add(11) };
            if attr & ATTR_LONG_NAME == ATTR_LONG_NAME {
                continue; // 长文件名项
            }
            if attr & ATTR_VOLUME_ID != 0 {
                continue; // 卷标
            }
            valid += 1;
            if valid > 2 {
                return false; // 第 3 个有效条目 → 非空
            }
        }
        // 跨簇。
        let next = read_fat_entry(bpb, cluster, fat_buf);
        if next >= 0x0FFF_FFF8 {
            return valid <= 2;
        }
        cluster = next;
    }
}

// ---------------------------------------------------------------------------
// 卷层: 把各 namespace 的分区表解析为「卷」, `dev` = 卷号
// ---------------------------------------------------------------------------
// 真实磁盘通常是「分区表 + 分区」, 而文件系统只认「卷」。本层在 block_srv 内完成:
//   - 扫描每个 namespace: 有 MBR/GPT 分区表 → 每个非空分区各成一个卷;
//     没有分区表 → **整个 namespace 视为一个卷** (向后兼容现有三张整盘镜像);
//   - 按卷首签名探测文件系统类型;
//   - 对上层把 `BlockReq.op` 高位定义为「卷号」(取代原先的 namespace 号),
//     实际 I/O 用 `(vol.nsid, vol.start_lba + lba)` 下发。
//
// 向后兼容: 现有三张整盘镜像 (无分区表) 各成一个卷, 卷号 0/1/2 与旧 dev 0/1/2 一致。

/// 卷表上限。
const VOL_MAX: usize = 16;
/// 卷扫描临时缓冲页: block_srv 自有 (不共享给任何域)。
///
/// ⚠️ 用户态固定虚拟地址分区 (从 `USER_BASE + 1 MiB` 起, 见各服务顶部注释):
///   0x10_0000..0x10_FFFF  fat32 / app·shell 共享页 / mfs / ext2 块缓冲
///   0x11_0000..0x11_3FFF  mfs GC 遍历 / inode 表 / 索引块 / GC 表块
///   0x11_4000..0x15_3FFF  exFAT 集群缓冲 (**按簇大小最多 64 页**, 故预留整段)
///   0x15_4000           exFAT 位图窗口
///   0x15_5000           exFAT upcase 窗口
///   0x15_6000           exFAT 单页暂存
///   0x16_0000           block_srv 卷扫描页 (本页)   ← 必须在 exFAT 预留段之上
///   0x16_1000           block_srv PRP 表页
/// 新增固定地址时务必对照本表 —— 一旦与别人的共享页重叠, 「同地址共享」会因为
/// 目标域的该地址已被映射而直接触发内核 panic (`map_user_page: PageAlreadyMapped`)。
const VOL_SCRATCH_VADDR: u64 = 0x0000_0080_0016_0000;

/// 文件系统类型 (按卷首签名探测)。
const VOL_KIND_UNKNOWN: u32 = 0;
const VOL_KIND_FAT: u32 = 1; // FAT12/16/32 (引导扇区尾 0x55AA)
const VOL_KIND_EXFAT: u32 = 2;
const VOL_KIND_MFS: u32 = 3;
const VOL_KIND_EXT2: u32 = 4;

/// 块请求操作码 (`BlockReq.op` 低 8 位)。
const BLOCK_OP_READ: u8 = 0;
const BLOCK_OP_WRITE: u8 = 1;
/// 查询卷表: 把 `VolumeDesc` 数组写进 `buf`, 回复卷数。
const BLOCK_OP_LIST_VOLUMES: u8 = 2;

/// 一个卷: 落在某 namespace 上的 [start_lba, start_lba+sectors) 区间。
/// `sectors == 0` 表示**容量未知** (Identify Namespace 没取到, 例如 IDE PIO 回退路径),
/// 此时「卷从 start_lba 起一直到盘尾」, 但没人知道盘尾在哪 —— 需要容量的上层 (MFS
/// 首次格式化) 必须按默认值兜底, 不能把 0 当成「零长度卷」。
#[derive(Clone, Copy)]
struct Volume {
    nsid: u32,
    start_lba: u32,
    sectors: u32,
    kind: u32,
}
const VOL_EMPTY: Volume = Volume {
    nsid: 0,
    start_lba: 0,
    sectors: 0,
    kind: VOL_KIND_UNKNOWN,
};
static mut VOLUMES: [Volume; VOL_MAX] = [VOL_EMPTY; VOL_MAX];
static mut VOL_COUNT: usize = 0;

/// 卷描述符 (经「卷列表」IPC 写给调用方共享页, `repr(C)` 固定布局)。
#[repr(C)]
#[derive(Clone, Copy)]
struct VolumeDesc {
    id: u32,
    nsid: u32,
    start_lba: u32,
    sectors: u32,
    kind: u32,
    _pad: u32,
}

fn vol_push(nsid: u32, start_lba: u32, sectors: u32, kind: u32) {
    let n = unsafe { VOL_COUNT };
    if n >= VOL_MAX {
        return;
    }
    unsafe {
        VOLUMES[n] = Volume {
            nsid,
            start_lba,
            sectors,
            kind,
        };
        VOL_COUNT = n + 1;
    }
}

fn vol_get(i: usize) -> Option<Volume> {
    if i >= unsafe { VOL_COUNT } {
        None
    } else {
        Some(unsafe { VOLUMES[i] })
    }
}

fn vol_reset() {
    unsafe {
        VOL_COUNT = 0;
    }
}

/// 卷类型的可读名 (与 `VOL_KIND_*` 对应), 仅供卷表打印。
fn vol_kind_name(kind: u32) -> &'static str {
    match kind {
        VOL_KIND_FAT => "fat32",
        VOL_KIND_EXFAT => "exfat",
        VOL_KIND_MFS => "mfs",
        VOL_KIND_EXT2 => "ext2",
        _ => "unknown",
    }
}

/// 打印卷表 (每卷一行 `vol: <卷号> nsid=<n> lba=<n> sectors=<n> kind=<名>`)。
///
/// 启动诊断, 也是 `mkfs.mfs <卷号>` 唯一的信息来源: 卷号由卷层在启动时按扫描顺序
/// 分配, 不打印出来就只能靠猜。`kind=unknown` 的卷是没格式化的 (可被 mkfs 接受)。
fn vol_print_table() {
    let n = unsafe { VOL_COUNT };
    let mut i = 0usize;
    while i < n {
        let v = match vol_get(i) {
            Some(v) => v,
            None => break,
        };
        print("vol: ");
        print_u64(i as u64);
        print(" nsid=");
        print_u64(v.nsid as u64);
        print(" lba=");
        print_u64(v.start_lba as u64);
        print(" sectors=");
        print_u64(v.sectors as u64);
        print(" kind=");
        println(vol_kind_name(v.kind));
        i += 1;
    }
}

/// 按卷首若干字节的签名判断文件系统类型。
///
/// 判据 (先强特征后弱特征): exFAT `"EXFAT   "` (偏移 3) → MFS magic `"MFS0".."MFS9"`
/// (偏移 0) → ext2 magic `0xEF53` (偏移 1080) → FAT 引导扇区尾 `0x55AA` (偏移 510)。
fn vol_detect_kind(buf: *const u8) -> u32 {
    let mut exfat = true;
    for (i, &c) in b"EXFAT   ".iter().enumerate() {
        if unsafe { *buf.add(3 + i) } != c {
            exfat = false;
            break;
        }
    }
    if exfat {
        return VOL_KIND_EXFAT;
    }
    let m = read_u32(buf);
    // "MFS0".."MFS9" (magic 低字节是版本号; 版本不符由挂载路径自行重格式化)。
    if m & 0xFFFF_FF00 == 0x4D46_5300 && (0x30..=0x39).contains(&(m & 0xFF)) {
        return VOL_KIND_MFS;
    }
    if read_u16(unsafe { buf.add(1080) }) == 0xEF53 {
        return VOL_KIND_EXT2;
    }
    let lo = unsafe { *buf.add(510) };
    let hi = unsafe { *buf.add(511) };
    if lo == 0x55 && hi == 0xAA {
        return VOL_KIND_FAT;
    }
    VOL_KIND_UNKNOWN
}

/// 读卷首 8 扇区 (4096 字节) 到 `scratch` 以探测类型。
#[allow(clippy::too_many_arguments)]
fn vol_probe_kind(
    nsid: u32,
    start_lba: u32,
    cfg: &NvmeConfig,
    mmio: u64,
    isq_doorbell: u64,
    icq_doorbell: u64,
    scratch: *mut u8,
    tail: &mut u32,
    head: &mut u32,
    phase: &mut u32,
) -> u32 {
    if !nvme_rw_sectors(
        OP_READ,
        nsid,
        cfg,
        mmio,
        isq_doorbell,
        icq_doorbell,
        start_lba,
        8,
        scratch,
        tail,
        head,
        phase,
    ) {
        return VOL_KIND_UNKNOWN;
    }
    vol_detect_kind(scratch)
}

/// 解析 GPT 分区表 (仅在检测到保护性 MBR 时进入): 逐个分区项登记卷。
#[allow(clippy::too_many_arguments)]
fn vol_scan_gpt(
    nsid: u32,
    cfg: &NvmeConfig,
    mmio: u64,
    isq_doorbell: u64,
    icq_doorbell: u64,
    scratch: *mut u8,
    tail: &mut u32,
    head: &mut u32,
    phase: &mut u32,
) {
    // LBA 1 = GPT 头。
    if !nvme_rw_sectors(
        OP_READ,
        nsid,
        cfg,
        mmio,
        isq_doorbell,
        icq_doorbell,
        1,
        1,
        scratch,
        tail,
        head,
        phase,
    ) {
        return;
    }
    let sig = unsafe { core::slice::from_raw_parts(scratch, 8) };
    if sig != b"EFI PART" {
        return;
    }
    let entry_lba = read_u64(unsafe { scratch.add(0x48) });
    let num = read_u32(unsafe { scratch.add(0x50) });
    let esize = read_u32(unsafe { scratch.add(0x54) });
    // 规范要求分区项大小为 128 的倍数; 上限保护避免异常镜像导致长循环。
    if esize < 128 || num == 0 || num > 128 || entry_lba > u32::MAX as u64 {
        return;
    }
    let mut idx = 0u32;
    while idx < num {
        let byte_off = idx as u64 * esize as u64;
        let sec = entry_lba + byte_off / 512;
        let within = (byte_off % 512) as usize;
        if sec > u32::MAX as u64 || within + esize as usize > 512 {
            // 项跨扇区 (非标准 esize): 跳过, 不冒险解析。
            idx += 1;
            continue;
        }
        if !nvme_rw_sectors(
            OP_READ,
            nsid,
            cfg,
            mmio,
            isq_doorbell,
            icq_doorbell,
            sec as u32,
            1,
            scratch,
            tail,
            head,
            phase,
        ) {
            return;
        }
        let e = unsafe { scratch.add(within) };
        // type_guid 全 0 = 未使用项。
        let mut used = false;
        let mut k = 0usize;
        while k < 16 {
            if unsafe { *e.add(k) } != 0 {
                used = true;
                break;
            }
            k += 1;
        }
        if used {
            let first = read_u64(unsafe { e.add(0x20) });
            let last = read_u64(unsafe { e.add(0x28) });
            if last >= first && first <= u32::MAX as u64 {
                let sectors = (last - first + 1).min(u32::MAX as u64) as u32;
                let kind = vol_probe_kind(
                    nsid,
                    first as u32,
                    cfg,
                    mmio,
                    isq_doorbell,
                    icq_doorbell,
                    scratch,
                    tail,
                    head,
                    phase,
                );
                vol_push(nsid, first as u32, sectors, kind);
            }
        }
        idx += 1;
    }
}

/// 扫描一个 namespace: 有分区表则逐分区登记卷, 否则整盘一个卷。
#[allow(clippy::too_many_arguments)]
fn vol_scan_namespace(
    nsid: u32,
    cfg: &NvmeConfig,
    mmio: u64,
    isq_doorbell: u64,
    icq_doorbell: u64,
    scratch: *mut u8,
    tail: &mut u32,
    head: &mut u32,
    phase: &mut u32,
) {
    // LBA 0: 判断 MBR / GPT / 无分区表。
    if !nvme_rw_sectors(
        OP_READ,
        nsid,
        cfg,
        mmio,
        isq_doorbell,
        icq_doorbell,
        0,
        1,
        scratch,
        tail,
        head,
        phase,
    ) {
        return;
    }
    let boot_sig = unsafe { *scratch.add(510) } == 0x55 && unsafe { *scratch.add(511) } == 0xAA;
    let entries = unsafe { scratch.add(446) };
    // 保护性 MBR (分区项 0 类型 = 0xEE) → 转 GPT。
    if boot_sig && unsafe { *entries.add(4) } == 0xEE {
        vol_scan_gpt(
            nsid,
            cfg,
            mmio,
            isq_doorbell,
            icq_doorbell,
            scratch,
            tail,
            head,
            phase,
        );
        return;
    }
    // 普通 MBR: 4 个 16 字节主分区项 (boot@0, type@4, lba_start@8, sectors@12)。
    // 先把所有项摘出来再逐个探测: 探测文件系统类型会读盘并覆写 `scratch`
    // (即分区表所在缓冲), 边读表边探测会把后续分区项毁掉。
    //
    // ⚠️ 必须校验项的合法性, 不能只看「type != 0 且 count != 0」: 本函数也用于
    // **本身就是分区**的卷 (真机 U 盘的分区用 `-drive file=/dev/sdXN` 接入)。此时
    // 扇区 0 是该分区的 VBR 而不是 MBR, 而 0x55AA 之外 446..509 那段在真实 FAT32
    // 引导代码里是**非零 ASCII**, 会被误读成 4 条主分区项, 于是真正的文件系统卷
    // 永远登记不上、只登记出 4 个指向非法 LBA 的假卷 (读它们直接 `LBA Out of Range`)。
    // 合法主分区项的启动标志只能是 0x00/0x80, 且 LBA 起点不可能为 0 (扇区 0 是 MBR 本身)。
    let mut parts: [(u32, u32); 4] = [(0, 0); 4];
    let mut npart = 0usize;
    let mut i = 0usize;
    while i < 4 {
        let e = unsafe { entries.add(i * 16) };
        let bootflag = unsafe { *e.add(0) };
        let ptype = unsafe { *e.add(4) };
        let start = read_u32(unsafe { e.add(8) });
        let count = read_u32(unsafe { e.add(12) });
        let plausible = (bootflag == 0x00 || bootflag == 0x80) && start != 0;
        if boot_sig && ptype != 0 && count != 0 && plausible {
            parts[npart] = (start, count);
            npart += 1;
        }
        i += 1;
    }
    if npart == 0 {
        // 无分区表 → 整盘一个卷。容量取 Identify Namespace 的 NSZE(在 `vol_probe_kind`
        // 覆盖 scratch 之前查好); 取不到则仍记 0 = 容量未知, 由上层按默认值兜底。
        let sectors = nvme_ns_sectors_of(nsid);
        let kind = vol_probe_kind(
            nsid,
            0,
            cfg,
            mmio,
            isq_doorbell,
            icq_doorbell,
            scratch,
            tail,
            head,
            phase,
        );
        vol_push(nsid, 0, sectors, kind);
        return;
    }
    let mut k = 0usize;
    while k < npart {
        let (start, count) = parts[k];
        let kind = vol_probe_kind(
            nsid,
            start,
            cfg,
            mmio,
            isq_doorbell,
            icq_doorbell,
            scratch,
            tail,
            head,
            phase,
        );
        vol_push(nsid, start, count, kind);
        k += 1;
    }
}

/// 查询卷表到调用方共享页 `buf`, 返回卷数 (失败 0)。
fn block_list_volumes(buf: *mut u8, max: u32) -> u64 {
    let req = BlockReq {
        op: (BLOCK_OP_LIST_VOLUMES as u64),
        lba: 0,
        count: max as u64,
        buf: buf as u64,
    };
    let payload = unsafe {
        core::slice::from_raw_parts(
            &req as *const BlockReq as *const u8,
            core::mem::size_of::<BlockReq>(),
        )
    };
    sys_call_payload(BLOCK_DOMAIN, BLOCK_REQ_TAG, payload)
}

/// 在卷描述符数组中查找第一个 `kind` 匹配的卷号。
fn vol_find_kind(list: *const u8, count: u64, kind: u32) -> Option<u32> {
    let esize = core::mem::size_of::<VolumeDesc>();
    let mut i = 0u64;
    while i < count {
        let d =
            unsafe { core::ptr::read_unaligned(list.add(i as usize * esize) as *const VolumeDesc) };
        if d.kind == kind {
            return Some(d.id);
        }
        i += 1;
    }
    None
}

/// 取卷描述符数组中第 `i` 个描述符 (供自测读取卷表)。
fn vol_desc(list: *const u8, i: usize) -> VolumeDesc {
    let esize = core::mem::size_of::<VolumeDesc>();
    unsafe { core::ptr::read_unaligned(list.add(i * esize) as *const VolumeDesc) }
}

/// 文件服务启动时「认领」自己要用的卷号。
///
/// 先在卷表里找第一个 `kind` 匹配的卷; 找不到则回退 `fallback` (约定卷号)。
/// 需要回退的原因: 空白 MFS 盘没有 magic, 必须先格式化才能被探测到。
/// `scratch` 必须是本域**已共享给 block_srv** 的缓冲页 (卷描述符经它回传)。
///
/// MFS **不用**这个函数: 一块盘上可以有多块 MFS 卷, 「第一个」不够用 —— 它按主卷
/// 序号认领 (见 `mfs_vol_claim`)。
fn vol_claim(scratch: *mut u8, max: u32, kind: u32, fallback: u64) -> u64 {
    let n = block_list_volumes(scratch, max);
    if n == 0 || n == u64::MAX {
        return fallback;
    }
    match vol_find_kind(scratch, n, kind) {
        Some(v) => v as u64,
        None => fallback,
    }
}

/// MFS 服务的卷认领 (取代通用 `vol_claim`)。
///
/// MFS 与其它文件系统不同: 一台机器上可能有多块 MFS 卷, 而**只有一块**该挂到 `/mfs`
/// —— 以前取「卷表里第一个」, 于是 `/mfs` 落在哪块卷上只由扫描顺序决定, 显式
/// `mkfs.mfs` 过谁毫无影响, 既不可控也无法解释。改为按**主卷序号**认领:
///
///   ① 序号最大且 > 0 的 MFS 卷 —— 序号由 `mkfs.mfs` 置成「现有最大 + 1」, 故它就是
///      「最近一次显式格式化过的卷」(见 `MFS_SB_PRIMARY`);
///   ② 都是 0 (老卷 / 只被首次挂载自动格式化过) 时退回卷表里第一个 MFS 卷;
///   ③ 一个 MFS 卷都没有时回退约定卷号 (空白盘没有 magic, 必须先格式化才会被探测到)。
fn mfs_vol_claim(scratch: *mut u8, max: u32) -> u64 {
    let n = block_list_volumes(scratch, max);
    if n == 0 || n == u64::MAX {
        return MFS_VOL_FALLBACK;
    }
    let esize = core::mem::size_of::<VolumeDesc>();
    let mut best: Option<(u64, u32)> = None;
    let mut first: Option<u32> = None;
    let mut i = 0u64;
    while i < n {
        let d = unsafe {
            core::ptr::read_unaligned(scratch.add(i as usize * esize) as *const VolumeDesc)
        };
        if d.kind == VOL_KIND_MFS {
            if first.is_none() {
                first = Some(d.id);
            }
            let serial = mfs_primary_of_vol(d.id as u64);
            if serial > 0 {
                match best {
                    Some((s, _)) if s >= serial => {}
                    _ => best = Some((serial, d.id)),
                }
            }
        }
        i += 1;
    }
    match best {
        Some((_, v)) => v as u64,
        None => first.map_or(MFS_VOL_FALLBACK, |v| v as u64),
    }
}

/// 卷表里卷号 `vol` 的描述符; 查不到返回 `None`。
///
/// `scratch` 必须是本域**已共享给 block_srv** 的缓冲页 (卷描述符经它回传)。
fn vol_find_desc(scratch: *mut u8, vol: u64) -> Option<VolumeDesc> {
    let n = block_list_volumes(scratch, VOL_MAX as u32);
    if n == 0 || n == u64::MAX {
        return None;
    }
    let esize = core::mem::size_of::<VolumeDesc>();
    let mut i = 0u64;
    while i < n {
        let d = unsafe {
            core::ptr::read_unaligned(scratch.add(i as usize * esize) as *const VolumeDesc)
        };
        if d.id as u64 == vol {
            return Some(d);
        }
        i += 1;
    }
    None
}

/// 卷表里卷号 `vol` 的文件系统类型 (查不到则返回 `VOL_KIND_UNKNOWN`)。
fn vol_kind_of(scratch: *mut u8, vol: u64) -> u32 {
    vol_find_desc(scratch, vol).map_or(VOL_KIND_UNKNOWN, |d| d.kind)
}

/// 卷表里卷号 `vol` 的容量 (扇区数); 0 = 未知。
fn vol_sectors(scratch: *mut u8, vol: u64) -> u32 {
    vol_find_desc(scratch, vol).map_or(0, |d| d.sectors)
}

/// 把本服务**默认卷之外**的同类卷挂到 `/usb<卷号>` (M1b 多卷挂载)。
///
/// `scratch` 必须是本域**已共享给 block_srv** 的缓冲页 (卷描述符经它回传), 且调用
/// 时机须在服务自己的元数据解析**之后** —— 它会覆盖该页内容。
/// 真实多盘/多分区机器上, 各文件服务借此把自己那一类的其余卷也提供给客户端, 而不是
/// 只认领「第一个匹配卷」。
fn mount_extra_volumes(scratch: *mut u8, kind: u32, primary: u64, domain: u64) {
    let n = block_list_volumes(scratch, VOL_MAX as u32);
    if n == 0 || n == u64::MAX {
        return;
    }
    let esize = core::mem::size_of::<VolumeDesc>();
    let mut i = 0u64;
    while i < n {
        let d = unsafe {
            core::ptr::read_unaligned(scratch.add(i as usize * esize) as *const VolumeDesc)
        };
        if d.kind == kind && d.id as u64 != primary {
            vfs::mount_vol(domain, d.id as u64);
        }
        i += 1;
    }
}

/// 域 5 — 块设备服务: 优先 NVMe (若内核已配置), 否则 IDE PIO 回退。
///
/// 接收 `BlockReq` (op/lba/count/buf), 读扇区写入调用方共享的缓冲页,
/// 回复状态 tag (1=成功, 0=失败)。数据经共享页零拷贝回传, IPC 仅传控制信息。
fn block_main() {
    let magic = unsafe { core::ptr::read_volatile(NVME_CFG_VADDR as *const u64) };
    if magic == NVME_CONFIG_MAGIC {
        nvme_main();
    } else {
        ide_block_main();
    }
}

/// 域 5 — IDE PIO 块设备服务 (无 NVMe 控制器时回退)。
///
/// M1 范围: IDE 单盘仅登记「整盘一个卷」(不扫描分区表), 故 `dev == 0` 的行为与
/// 引入卷层之前完全一致, `dev > 0` 一律失败。IDE 盘的分区扫描留待后续。
fn ide_block_main() {
    vol_reset();
    vol_push(0, 0, 0, VOL_KIND_UNKNOWN);
    // 卷表 (启动诊断): IDE 回退路径只有一个整盘卷, 容量未知 (sectors=0)。
    vol_print_table();
    loop {
        let mut msg = Message {
            from: 0,
            to: 0,
            tag: 0,
            payload: [0; PAYLOAD_LEN],
        };
        sys_recv_msg(&mut msg as *mut Message as *mut u8);

        if msg.tag != BLOCK_REQ_TAG {
            sys_reply(0);
            continue;
        }

        // 从 payload 解出块设备请求。
        let req: BlockReq =
            unsafe { core::ptr::read_unaligned(msg.payload.as_ptr() as *const BlockReq) };

        // `dev` 是卷号 (IDE 只有一个整盘卷, 即 dev 0)。
        let dev = (req.op >> 8) as usize;
        let opcode = (req.op & 0xFF) as u8;
        match opcode {
            BLOCK_OP_READ | BLOCK_OP_WRITE => {
                let vol = match vol_get(dev) {
                    Some(v) => v,
                    None => {
                        sys_reply(0);
                        continue;
                    }
                };
                let lba = vol.start_lba.saturating_add(req.lba as u32);
                let ok = if opcode == BLOCK_OP_READ {
                    read_sectors(lba, req.count as u16, req.buf as *mut u8)
                } else {
                    write_sectors(lba, req.count as u16, req.buf as *mut u8)
                };
                sys_reply(if ok { 1 } else { 0 });
            }
            BLOCK_OP_LIST_VOLUMES => {
                let dst = req.buf as *mut u8;
                let n = if req.count as usize >= 1 {
                    1usize
                } else {
                    0usize
                };
                if n == 1 {
                    let d = VolumeDesc {
                        id: 0,
                        nsid: 0,
                        start_lba: 0,
                        sectors: 0,
                        kind: VOL_KIND_UNKNOWN,
                        _pad: 0,
                    };
                    unsafe {
                        core::ptr::write_unaligned(dst as *mut VolumeDesc, d);
                    }
                }
                sys_reply(n as u64);
            }
            _ => {
                sys_reply(0);
            }
        }
    }
}

/// fat32_srv 打开节点描述符表 (静态, 单任务独占访问, 无需锁)。
const MAX_FD: usize = 16;

#[derive(Clone, Copy)]
struct OpenNode {
    is_dir: bool,
    start_cluster: u32,
    file_size: u32,
    /// 目录项所在父目录簇 (文件写回用)。
    dir_cluster: u32,
    /// 目录项在父目录簇内的字节偏移 (文件写回用)。
    entry_offset: u32,
    /// 打开时绑定的卷号 (M1b 多卷挂载: 同一服务可同时服务默认卷与额外卷)。
    vol: u64,
}

static mut FD_TABLE: [Option<OpenNode>; MAX_FD] = [None; MAX_FD];

/// 分配一个空闲 fd 槽位, 返回 fd (0..MAX_FD), 表满返回 u64::MAX。
fn fd_alloc(
    is_dir: bool,
    start_cluster: u32,
    file_size: u32,
    dir_cluster: u32,
    entry_offset: u32,
    vol: u64,
) -> u64 {
    unsafe {
        let base = core::ptr::addr_of_mut!(FD_TABLE).cast::<Option<OpenNode>>();
        for i in 0..MAX_FD {
            let slot = base.add(i);
            if (*slot).is_none() {
                *slot = Some(OpenNode {
                    is_dir,
                    start_cluster,
                    file_size,
                    dir_cluster,
                    entry_offset,
                    vol,
                });
                return i as u64;
            }
        }
    }
    u64::MAX
}

/// 查询 fd 对应的节点描述符, 并把「当前卷寄存器」切到该 fd 绑定的卷。
///
/// fd 类请求 (READ/WRITE/READDIR) 的 payload 里没有卷号 —— 卷在 `open` 时就固定
/// 绑到了 fd 上, 这里顺带切换, 使后续读写自动落在同一卷 (与路径类请求的 tag 卷编码
/// 等价, 见 `FAT_CUR_VOL`)。
fn fd_lookup(fd: u32) -> Option<OpenNode> {
    if (fd as usize) >= MAX_FD {
        return None;
    }
    let node = unsafe {
        *core::ptr::addr_of!(FD_TABLE)
            .cast::<Option<OpenNode>>()
            .add(fd as usize)
    };
    if let Some(n) = node {
        unsafe {
            FAT_CUR_VOL = n.vol;
        }
    }
    node
}

/// 更新 fd 对应节点的首簇号与文件大小 (写操作后同步)。
fn fd_update(fd: u32, start_cluster: u32, file_size: u32) {
    if (fd as usize) >= MAX_FD {
        return;
    }
    unsafe {
        let slot = core::ptr::addr_of_mut!(FD_TABLE)
            .cast::<Option<OpenNode>>()
            .add(fd as usize);
        if let Some(mut node) = *slot {
            node.start_cluster = start_cluster;
            node.file_size = file_size;
            *slot = Some(node);
        }
    }
}

/// 释放 fd, 成功返回 1, 失败返回 0。
fn fd_free(fd: u32) -> u64 {
    if (fd as usize) >= MAX_FD {
        return 0;
    }
    unsafe {
        let slot = core::ptr::addr_of_mut!(FD_TABLE)
            .cast::<Option<OpenNode>>()
            .add(fd as usize);
        if (*slot).is_some() {
            *slot = None;
            1
        } else {
            0
        }
    }
}

/// 各卷几何 (根簇 / FAT 起址 / 簇大小) 不同, 这里记住 BPB 当前属于哪个卷。
/// 请求落在别的卷上时重新解析该卷的 BPB (见 `fat_load_bpb`)。
static mut FAT_BPB_VOL: u64 = u64::MAX;

/// fat32_srv 的**整簇缓冲**虚拟地址 (M1b: 大簇支持)。
///
/// 整簇读写 (目录簇扫描 / 文件数据暂存) 都在这里进行, 按 FAT32 的**最大簇**预留:
/// BPB 的 `SecPerClus` 只有 1 字节且必须是 2 的幂, 故簇最大 128 扇区 = 64 KiB。
/// 地址取 2 MiB 偏移处: 避开 1 MiB 处的小缓冲段 (`0x10_xxxx`) 与用户栈 (`0x3F_9000`),
/// 也避开 exFAT 的集群缓冲段 (`0x11_4000..0x15_3FFF`) —— 两个服务都会把自己的缓冲
/// **同地址共享给 block_srv**, 地址撞上会让 block_srv 侧触发内核
/// `map_user_page: PageAlreadyMapped` panic。
const FAT32_CLU_VADDR: u64 = 0x0000_0080_0020_0000;
/// 整簇缓冲页数 (64 KiB 上限簇 = 16 页)。
const FAT32_CLU_PAGES: u64 = 16;
/// FAT32 允许的最大簇字节数 (`SecPerClus` ≤ 128 扇区 × 512 B)。
const FAT32_MAX_CLUSTER_BYTES: u32 = 128 * 512;

/// 载入卷 `vol` 的 BPB 到 `out`, 并把 `FAT_BPB_VOL` 标成 `vol`。
///
/// 调用者须已把 `FAT_CUR_VOL` 指向 `vol`。几何不合法 (非 512B 扇区 / 簇为 0 /
/// 簇超过整簇缓冲) 时返回 false —— 请求按失败回复, 不拿错几何去读盘。
fn fat_load_bpb(vol: u64, bpb_buf: *mut u8, out: &mut Fat32Bpb) -> bool {
    if !block_read(0, 1, bpb_buf) {
        return false;
    }
    // 引导扇区签名 (偏移 510 = 0x55, 511 = 0xAA)。
    if read_u16(unsafe { bpb_buf.add(510) } as *const u8) != 0xAA55 {
        return false;
    }
    let b = Fat32Bpb::parse(bpb_buf as *const u8);
    let cb = b.cluster_bytes();
    if b.bytes_per_sector != 512 || cb == 0 || cb > FAT32_MAX_CLUSTER_BYTES {
        return false;
    }
    *out = b;
    unsafe {
        FAT_BPB_VOL = vol;
        // 换卷后分配游标归位 (各卷簇数不同, 沿用旧卷的 hint 可能越过新卷簇数)。
        FAT_ALLOC_HINT = 2;
    }
    true
}

/// 域 6 — FAT32 文件服务 (fat32_srv): 经 IPC 请求 block_srv 读扇区,
/// 解析 BPB / FAT / 目录 / 路径, 提供 open/read/readdir/close。
fn fat32_main() {
    // 小缓冲 (各 1 页): BPB / FAT 扇区 —— 放 1 MiB 处, 与 app·shell 的共享页同段。
    let bpb_buf = 0x0000_0080_0010_2000u64;
    let fat_buf = 0x0000_0080_0010_1000u64;
    // 整簇缓冲 (最多 16 页 = 64 KiB 簇): 目录簇扫描与文件数据暂存共用同一块。
    let clu_buf = FAT32_CLU_VADDR;
    let dir_buf = clu_buf;
    let file_buf = clu_buf;

    if sys_alloc_page(bpb_buf) != 1 || sys_alloc_page(fat_buf) != 1 {
        println("fat32: alloc buffer FAILED");
        return;
    }

    // 把缓冲页共享给 block_srv (同地址映射), 使其能直接写入读到的扇区数据。
    if sys_share_page(bpb_buf, BLOCK_DOMAIN) != 1 || sys_share_page(fat_buf, BLOCK_DOMAIN) != 1 {
        println("fat32: share buffer FAILED");
        return;
    }

    // 整簇缓冲逐页分配 + 同地址共享 (M1b: 大簇支持, 见 `FAT32_CLU_VADDR`)。
    for i in 0..FAT32_CLU_PAGES {
        let p = clu_buf + i * 4096;
        if sys_alloc_page(p) != 1 || sys_share_page(p, BLOCK_DOMAIN) != 1 {
            println("fat32: alloc/share cluster buffer FAILED");
            return;
        }
    }

    // 认领卷: 第一个 FAT 签名的卷; 无分区表的整盘镜像即卷 0 (回退值)。
    unsafe {
        FAT_VOL = vol_claim(bpb_buf as *mut u8, 16, VOL_KIND_FAT, 0);
        // 启动期 (读 BPB / FS-1 自测) 的读写都落在默认卷上。
        FAT_CUR_VOL = FAT_VOL;
    }

    // 经 block_srv 读 LBA 0 并解析 BPB。
    if !block_read(0, 1, bpb_buf as *mut u8) {
        println("fat32: read LBA 0 FAILED");
        return;
    }
    // 引导签名校验 (offset 510 = 0x55, 511 = 0xAA)。
    let sig = read_u16((bpb_buf + 510) as *const u8);
    if sig != 0xAA55 {
        println("fat32: not a boot sector");
        return;
    }
    let mut bpb = Fat32Bpb::parse(bpb_buf as *const u8);
    unsafe {
        FAT_BPB_VOL = FAT_VOL;
    }
    // 几何校验: 整簇缓冲按 64 KiB 上限预留, 更大的簇 (以及非 512B 扇区) 直接拒绝,
    // 绝不用「按小块缓冲算出的偏移」去读盘。
    let cbytes = bpb.cluster_bytes();
    if bpb.bytes_per_sector != 512 || cbytes == 0 || cbytes > FAT32_MAX_CLUSTER_BYTES {
        print("fat32: unsupported geometry bps=");
        print_u64(bpb.bytes_per_sector as u64);
        print(" cluster=");
        print_u64(cbytes as u64);
        println("");
        return;
    }

    // FS-1 自测: 验证元数据原语 (find_dir_slot / write_dir_entry / find_entry /
    // unlink_entry / free_cluster_chain) 的建项 → 命中 → 删项 → 释放簇全链路。
    {
        let sn = match short_name_from_query(b"FS1TEST") {
            Some(s) => s,
            None => {
                println("fat32: FS1 self-test invalid short name");
                return;
            }
        };
        let free = match find_free_cluster(&bpb, fat_buf as *mut u8) {
            Some(c) => c,
            None => {
                println("fat32: FS1 self-test no free cluster");
                return;
            }
        };
        if !write_fat_entry(&bpb, free, FAT_EOC, fat_buf as *mut u8) {
            println("fat32: FS1 self-test write FAT failed");
            return;
        }
        let (off, dc) = match find_dir_slot(
            &bpb,
            bpb.root_cluster,
            dir_buf as *mut u8,
            fat_buf as *mut u8,
        ) {
            Some(x) => x,
            None => {
                println("fat32: FS1 self-test find_dir_slot failed");
                return;
            }
        };
        if !write_dir_entry(&bpb, dc, off, &sn, 0, free, 0, dir_buf as *mut u8) {
            println("fat32: FS1 self-test write_dir_entry failed");
            return;
        }
        let found = find_entry(
            &bpb,
            bpb.root_cluster,
            b"FS1TEST",
            dir_buf as *mut u8,
            fat_buf as *mut u8,
        );
        let info = match found {
            Some(i) if i.start_cluster == free => i,
            _ => {
                println("fat32: FS1 self-test find_entry verify FAILED");
                return;
            }
        };
        if !unlink_entry(
            &bpb,
            info.dir_cluster,
            info.entry_offset,
            info.start_cluster,
            dir_buf as *mut u8,
            fat_buf as *mut u8,
        ) {
            println("fat32: FS1 self-test unlink_entry FAILED");
            return;
        }
        if find_entry(
            &bpb,
            bpb.root_cluster,
            b"FS1TEST",
            dir_buf as *mut u8,
            fat_buf as *mut u8,
        )
        .is_some()
        {
            println("fat32: FS1 self-test entry still present FAILED");
            return;
        }
        if read_fat_entry(&bpb, free, fat_buf as *mut u8) != 0 {
            println("fat32: FS1 self-test cluster not freed FAILED");
            return;
        }
    }

    // M1b: 把**额外**的 FAT 卷 (如分区盘上的第二个 FAT 分区、真 U 盘) 挂到 `/usb<卷号>`。
    // 用 `fat_buf` 暂存卷描述符 —— 元数据已解析完毕, 该页此刻是空闲暂存。
    mount_extra_volumes(
        fat_buf as *mut u8,
        VOL_KIND_FAT,
        unsafe { FAT_VOL },
        vfs::FAT32_DOMAIN,
    );

    // 服务循环: 经 IPC 提供 open / read / readdir / close (见 vfs.rs 协议)。
    loop {
        let mut msg = Message {
            from: 0,
            to: 0,
            tag: 0,
            payload: [0; PAYLOAD_LEN],
        };
        sys_recv_msg(&mut msg as *mut Message as *mut u8);

        let tag = vfs::tag_body(msg.tag);
        // tag 高位携带卷编码 (M1b): 路径类请求由它决定目标卷; fd 类请求由 fd 绑定的卷
        // 决定 (fd 是这些请求 payload 的首字段), 先探一次 fd, 使两类请求都对。
        let mut vol = vfs::vol_from_enc(vfs::tag_vol(msg.tag), unsafe { FAT_VOL });
        if matches!(
            tag,
            vfs::VFS_READ_TAG | vfs::VFS_WRITE_TAG | vfs::VFS_READDIR_TAG
        ) {
            if let Some(n) = fd_lookup(read_u32(msg.payload.as_ptr())) {
                vol = n.vol;
            }
        }
        unsafe {
            FAT_CUR_VOL = vol;
        }
        // 卷切换: 各 FAT 卷的根簇 / FAT 起址 / 簇大小都不同, 必须重新解析该卷的 BPB,
        // 否则会拿上一个卷的几何去算扇区号, 读到完全错误的位置。
        if unsafe { FAT_BPB_VOL } != vol && !fat_load_bpb(vol, bpb_buf as *mut u8, &mut bpb) {
            sys_reply(u64::MAX);
            continue;
        }
        match tag {
            vfs::VFS_OPEN_TAG => {
                let path_len = msg
                    .payload
                    .iter()
                    .position(|&b| b == 0)
                    .unwrap_or(PAYLOAD_LEN);
                let path = unsafe { core::str::from_utf8_unchecked(&msg.payload[..path_len]) };
                let fd = match resolve_open_path(&bpb, path, dir_buf as *mut u8, fat_buf as *mut u8)
                {
                    Some(info) => fd_alloc(
                        info.attr & ATTR_DIRECTORY != 0,
                        info.start_cluster,
                        info.file_size,
                        info.dir_cluster,
                        info.entry_offset,
                        vol,
                    ),
                    None => u64::MAX,
                };
                sys_reply(fd);
            }
            vfs::VFS_READ_TAG => {
                let req: vfs::ReadReq = unsafe {
                    core::ptr::read_unaligned(msg.payload.as_ptr() as *const vfs::ReadReq)
                };
                // 协议 offset 是 u64, 但 FAT32 的文件大小字段只有 32 位: 偏移一旦超出
                // u32 就不可能落在合法数据上, 直接判失败 (而不是截断成一个错的偏移)。
                if req.offset > u32::MAX as u64 {
                    sys_reply(u64::MAX);
                    continue;
                }
                let offset = req.offset as u32;
                let n = match fd_lookup(req.fd) {
                    Some(node) if !node.is_dir => read_file_range(
                        &bpb,
                        node.start_cluster,
                        node.file_size,
                        offset,
                        req.count,
                        fat_buf as *mut u8,
                        file_buf as *mut u8,
                        req.buf as *mut u8,
                    ),
                    _ => u64::MAX,
                };
                sys_reply(n);
            }
            vfs::VFS_WRITE_TAG => {
                let req: vfs::WriteReq = unsafe {
                    core::ptr::read_unaligned(msg.payload.as_ptr() as *const vfs::WriteReq)
                };
                if req.offset > u32::MAX as u64 {
                    sys_reply(u64::MAX);
                    continue;
                }
                let offset = req.offset as u32;
                let n = match fd_lookup(req.fd) {
                    Some(mut node) if !node.is_dir => {
                        let written = write_file_range(
                            &bpb,
                            &mut node,
                            offset,
                            req.count,
                            fat_buf as *mut u8,
                            file_buf as *mut u8,
                            dir_buf as *mut u8,
                            req.buf as *const u8,
                        );
                        if written != u64::MAX {
                            fd_update(req.fd, node.start_cluster, node.file_size);
                        }
                        written
                    }
                    _ => u64::MAX,
                };
                sys_reply(n);
            }
            vfs::VFS_READDIR_TAG => {
                let req: vfs::DirReq = unsafe {
                    core::ptr::read_unaligned(msg.payload.as_ptr() as *const vfs::DirReq)
                };
                let n = match fd_lookup(req.fd) {
                    Some(node) if node.is_dir => readdir_into(
                        &bpb,
                        node.start_cluster,
                        dir_buf as *mut u8,
                        fat_buf as *mut u8,
                        req.buf as *mut u8,
                    ),
                    _ => u64::MAX,
                };
                sys_reply(n);
            }
            vfs::VFS_CLOSE_TAG => {
                let fd = read_u32(msg.payload.as_ptr());
                sys_reply(fd_free(fd));
            }
            vfs::VFS_CREAT_TAG => {
                let path_len = msg
                    .payload
                    .iter()
                    .position(|&b| b == 0)
                    .unwrap_or(PAYLOAD_LEN);
                let path = unsafe { core::str::from_utf8_unchecked(&msg.payload[..path_len]) };
                let fd = match resolve_parent(&bpb, path, dir_buf as *mut u8, fat_buf as *mut u8) {
                    Some((parent, sn)) => match find_entry_sn(
                        &bpb,
                        parent,
                        &sn,
                        dir_buf as *mut u8,
                        fat_buf as *mut u8,
                    ) {
                        Some(info) if info.attr & ATTR_DIRECTORY == 0 => fd_alloc(
                            false,
                            info.start_cluster,
                            info.file_size,
                            info.dir_cluster,
                            info.entry_offset,
                            vol,
                        ),
                        Some(_) => u64::MAX, // 已存在目录
                        None => {
                            // 创建空文件 (首簇 0, 大小 0, 不预分配簇)。
                            match find_dir_slot(
                                &bpb,
                                parent,
                                dir_buf as *mut u8,
                                fat_buf as *mut u8,
                            ) {
                                Some((off, dc))
                                    if write_dir_entry(
                                        &bpb,
                                        dc,
                                        off,
                                        &sn,
                                        0,
                                        0,
                                        0,
                                        dir_buf as *mut u8,
                                    ) =>
                                {
                                    fd_alloc(false, 0, 0, dc, off, vol)
                                }
                                _ => u64::MAX,
                            }
                        }
                    },
                    None => u64::MAX,
                };
                sys_reply(fd);
            }
            vfs::VFS_MKDIR_TAG => {
                let path_len = msg
                    .payload
                    .iter()
                    .position(|&b| b == 0)
                    .unwrap_or(PAYLOAD_LEN);
                let path = unsafe { core::str::from_utf8_unchecked(&msg.payload[..path_len]) };
                let r = match resolve_parent(&bpb, path, dir_buf as *mut u8, fat_buf as *mut u8) {
                    Some((parent, sn)) => {
                        if find_entry_sn(&bpb, parent, &sn, dir_buf as *mut u8, fat_buf as *mut u8)
                            .is_some()
                        {
                            u64::MAX // 已存在
                        } else {
                            match find_free_cluster(&bpb, fat_buf as *mut u8) {
                                Some(free) => {
                                    if !write_fat_entry(&bpb, free, FAT_EOC, fat_buf as *mut u8) {
                                        u64::MAX
                                    } else {
                                        // 清零整簇, 写入 . 与 .. 目录项。
                                        zero_bytes(
                                            dir_buf as *mut u8,
                                            bpb.cluster_bytes() as usize,
                                        );
                                        if !write_cluster(&bpb, free, dir_buf as *mut u8) {
                                            u64::MAX
                                        } else {
                                            let dot = *b".          ";
                                            let dotdot = *b"..         ";
                                            if !write_dir_entry(
                                                &bpb,
                                                free,
                                                0,
                                                &dot,
                                                ATTR_DIRECTORY,
                                                free,
                                                0,
                                                dir_buf as *mut u8,
                                            ) || !write_dir_entry(
                                                &bpb,
                                                free,
                                                32,
                                                &dotdot,
                                                ATTR_DIRECTORY,
                                                parent,
                                                0,
                                                dir_buf as *mut u8,
                                            ) {
                                                u64::MAX
                                            } else {
                                                // 在父目录登记新目录项。
                                                match find_dir_slot(
                                                    &bpb,
                                                    parent,
                                                    dir_buf as *mut u8,
                                                    fat_buf as *mut u8,
                                                ) {
                                                    Some((off, dc))
                                                        if write_dir_entry(
                                                            &bpb,
                                                            dc,
                                                            off,
                                                            &sn,
                                                            ATTR_DIRECTORY,
                                                            free,
                                                            0,
                                                            dir_buf as *mut u8,
                                                        ) =>
                                                    {
                                                        1
                                                    }
                                                    _ => u64::MAX,
                                                }
                                            }
                                        }
                                    }
                                }
                                None => u64::MAX,
                            }
                        }
                    }
                    None => u64::MAX,
                };
                sys_reply(r);
            }
            vfs::VFS_UNLINK_TAG => {
                let path_len = msg
                    .payload
                    .iter()
                    .position(|&b| b == 0)
                    .unwrap_or(PAYLOAD_LEN);
                let path = unsafe { core::str::from_utf8_unchecked(&msg.payload[..path_len]) };
                let r = match resolve_parent(&bpb, path, dir_buf as *mut u8, fat_buf as *mut u8) {
                    Some((parent, sn)) => match find_entry_sn(
                        &bpb,
                        parent,
                        &sn,
                        dir_buf as *mut u8,
                        fat_buf as *mut u8,
                    ) {
                        Some(info)
                            if info.attr & ATTR_DIRECTORY == 0
                                && unlink_entry(
                                    &bpb,
                                    info.dir_cluster,
                                    info.entry_offset,
                                    info.start_cluster,
                                    dir_buf as *mut u8,
                                    fat_buf as *mut u8,
                                ) =>
                        {
                            1
                        }
                        _ => u64::MAX, // 不存在或目录
                    },
                    None => u64::MAX,
                };
                sys_reply(r);
            }
            vfs::VFS_RMDIR_TAG => {
                let path_len = msg
                    .payload
                    .iter()
                    .position(|&b| b == 0)
                    .unwrap_or(PAYLOAD_LEN);
                let path = unsafe { core::str::from_utf8_unchecked(&msg.payload[..path_len]) };
                let r = match resolve_parent(&bpb, path, dir_buf as *mut u8, fat_buf as *mut u8) {
                    Some((parent, sn)) => match find_entry_sn(
                        &bpb,
                        parent,
                        &sn,
                        dir_buf as *mut u8,
                        fat_buf as *mut u8,
                    ) {
                        Some(info)
                            if info.attr & ATTR_DIRECTORY != 0
                                && dir_is_empty(
                                    &bpb,
                                    info.start_cluster,
                                    dir_buf as *mut u8,
                                    fat_buf as *mut u8,
                                )
                                && unlink_entry(
                                    &bpb,
                                    info.dir_cluster,
                                    info.entry_offset,
                                    info.start_cluster,
                                    dir_buf as *mut u8,
                                    fat_buf as *mut u8,
                                ) =>
                        {
                            1
                        }
                        _ => u64::MAX, // 不存在或文件
                    },
                    None => u64::MAX,
                };
                sys_reply(r);
            }
            vfs::VFS_STAT_TAG => {
                let (buf, path) = parse_path_req(msg.payload.as_ptr());
                let n = match resolve_open_path(&bpb, path, dir_buf as *mut u8, fat_buf as *mut u8)
                {
                    Some(info) => {
                        let is_dir = u32::from(info.attr & ATTR_DIRECTORY != 0);
                        let st = vfs::Stat::plain(info.file_size as u64, is_dir);
                        unsafe {
                            core::ptr::write_unaligned(buf as *mut vfs::Stat, st);
                        }
                        core::mem::size_of::<vfs::Stat>() as u64
                    }
                    None => u64::MAX,
                };
                sys_reply(n);
            }
            _ => {
                sys_reply(u64::MAX);
            }
        }
    }
}

/// 大块写测试缓冲 (4KB, 跨簇扩展验证用)。
static mut BIG_WRITE_BUF: [u8; 4096] = [0u8; 4096];

/// FS-11 辅助: 从 `off` 起写 `pages` 个 4 KiB 页, 第 i 页整页填 `tag + i`。
fn fs11_write_pages(fd: u64, off: u32, pages: u32, tag: u8) -> bool {
    let mut i = 0u32;
    while i < pages {
        let want = tag.wrapping_add(i as u8);
        let n = unsafe {
            let buf = core::slice::from_raw_parts_mut(
                core::ptr::addr_of_mut!(BIG_WRITE_BUF) as *mut u8,
                4096,
            );
            buf.fill(want);
            let slice =
                core::slice::from_raw_parts(core::ptr::addr_of!(BIG_WRITE_BUF) as *const u8, 4096);
            vfs::write(fd, (off + i * 4096) as u64, slice)
        };
        if n != 4096 {
            return false;
        }
        i += 1;
    }
    true
}

/// FS-11 辅助: 读回 `pages` 页并逐字节比对 (期望与 `fs11_write_pages` 一致)。
fn fs11_verify_pages(fd: u64, off: u32, pages: u32, tag: u8) -> bool {
    let mut i = 0u32;
    while i < pages {
        let n = vfs::read(fd, (off + i * 4096) as u64, 4096);
        if n != 4096 {
            return false;
        }
        let want = tag.wrapping_add(i as u8);
        let got = unsafe { core::slice::from_raw_parts(vfs::RESULT_BUF as *const u8, 4096) };
        if got.iter().any(|&b| b != want) {
            return false;
        }
        i += 1;
    }
    true
}

/// FS-12 辅助: 生成 16 字节长名 `LONGFILE_nnn.TXT` (nnn = 3 位十进制)。
///
/// 16 字节日志式短名 + 8 字节条目头 = 24 字节/项 —— 目录节点块 (4080 字节条目区)
/// 恰好只容 170 项, 故第 171 项起必然落进扩展目录块 (M4 要验证的路径)。
fn fs12_name(i: u32, out: &mut [u8]) {
    out[..9].copy_from_slice(b"LONGFILE_");
    out[9] = b'0' + ((i / 100) % 10) as u8;
    out[10] = b'0' + ((i / 10) % 10) as u8;
    out[11] = b'0' + (i % 10) as u8;
    out[12..16].copy_from_slice(b".TXT");
}

/// FS-13 辅助: 取 `path` 的元数据 (`stat` 结果落在 `RESULT_BUF`)。
fn fs13_stat(path: &str) -> Option<vfs::Stat> {
    if vfs::stat(path) != core::mem::size_of::<vfs::Stat>() as u64 {
        return None;
    }
    Some(unsafe { core::ptr::read_unaligned(vfs::RESULT_BUF as *const vfs::Stat) })
}

/// FS-13 辅助: 从 `mode` 取**权限位** (低 12 位)。
///
/// M5c 起 `mode` 的高 4 位是节点类型 (见 `vfs::MODE_FTYPE_*`), 自测比较权限时统一用它
/// 剥掉类型位, 否则「0o644」这类断言会被类型位顶掉。
fn fs13_perm(mode: u16) -> u16 {
    mode & !vfs::MODE_FTYPE_MASK
}

/// FS-13 辅助: 读 `fd` 的 [off, off+count) 并确认整段为 0 (稀疏区验证用)。
fn fs13_all_zero(fd: u64, off: u32, count: u32) -> bool {
    if vfs::read(fd, off as u64, count) != count as u64 {
        return false;
    }
    let got = unsafe { core::slice::from_raw_parts(vfs::RESULT_BUF as *const u8, count as usize) };
    got.iter().all(|&b| b == 0)
}

/// FS-19 辅助: 在目录 `dir` 的 readdir 结果里按**长名**取条目, 用于检查类型位与 size。
fn fs19_entry(dir: &str, name: &str) -> Option<vfs::DirEntry> {
    let fd = vfs::open(dir);
    if fd == u64::MAX {
        return None;
    }
    let n = vfs::readdir(fd);
    vfs::close(fd);
    if n == u64::MAX {
        return None;
    }
    let entry_size = core::mem::size_of::<vfs::DirEntry>();
    let count = n as usize / entry_size;
    let list =
        unsafe { core::slice::from_raw_parts(vfs::RESULT_BUF as *const vfs::DirEntry, count) };
    let q = name.as_bytes();
    let mut found = None;
    for de in list {
        let llen = de.long_len as usize;
        if llen == q.len() && &de.long[..llen] == q {
            found = Some(*de);
        }
    }
    found
}

/// FS-20 辅助: `readlink` 并逐字节比对目标串。
fn fs20_readlink_is(path: &str, want: &str) -> bool {
    let n = vfs::readlink(path);
    if n == u64::MAX || n as usize != want.len() {
        return false;
    }
    let got = unsafe { core::slice::from_raw_parts(vfs::RESULT_BUF as *const u8, n as usize) };
    got == want.as_bytes()
}

/// FS-20 辅助: 取 `path` **自身** (不跟随软链接) 的元数据。
fn fs20_lstat(path: &str) -> Option<vfs::Stat> {
    if vfs::lstat(path) != core::mem::size_of::<vfs::Stat>() as u64 {
        return None;
    }
    Some(unsafe { core::ptr::read_unaligned(vfs::RESULT_BUF as *const vfs::Stat) })
}

/// 域 7 — 测试应用: 经 libvfs 走通 read/write/readdir + FS-2/FS-3 全链路自测。
/// 成功路径完全静默 (只保留失败信息), 避免刷屏打断 shell 提示符。
fn app_main() {
    // 分配结果页并共享给各文件服务 (同地址映射), 供其写入文件/目录内容。
    if sys_alloc_page(vfs::RESULT_BUF) != 1 {
        println("app: alloc result buf FAILED");
        return;
    }
    if sys_share_page(vfs::RESULT_BUF, vfs::FAT32_DOMAIN) != 1
        || sys_share_page(vfs::RESULT_BUF, vfs::TMPFS_DOMAIN) != 1
        || sys_share_page(vfs::RESULT_BUF, vfs::MFS_DOMAIN) != 1
        || sys_share_page(vfs::RESULT_BUF, vfs::EXT2_DOMAIN) != 1
        || sys_share_page(vfs::RESULT_BUF, vfs::EXFAT_DOMAIN) != 1
        || sys_share_page(vfs::RESULT_BUF, BLOCK_DOMAIN) != 1
    {
        println("app: share result buf FAILED");
        return;
    }

    // 1. open -> read -> close: 读整个文件。
    let fd = vfs::open("/HELLO.TXT");
    if fd == u64::MAX {
        println("app: open \"/HELLO.TXT\" -> NOT FOUND");
        return;
    }
    if vfs::read(fd, 0, 4096) == u64::MAX {
        println("app: read \"/HELLO.TXT\" -> FAILED");
    }
    vfs::close(fd);

    // 2. 分配并共享写缓冲页, 供各文件服务读取要写入的数据。
    if sys_alloc_page(vfs::WRITE_BUF) != 1 {
        println("app: alloc write buf FAILED");
        return;
    }
    if sys_share_page(vfs::WRITE_BUF, vfs::FAT32_DOMAIN) != 1
        || sys_share_page(vfs::WRITE_BUF, vfs::TMPFS_DOMAIN) != 1
        || sys_share_page(vfs::WRITE_BUF, vfs::MFS_DOMAIN) != 1
        || sys_share_page(vfs::WRITE_BUF, vfs::EXFAT_DOMAIN) != 1
    {
        println("app: share write buf FAILED");
        return;
    }

    // 3. write -> read: 覆盖并扩展 /HELLO.TXT, 再读回验证。
    let wfd = vfs::open("/HELLO.TXT");
    if wfd == u64::MAX {
        println("app: open \"/HELLO.TXT\" for write -> NOT FOUND");
        return;
    }
    let data = b"OVERWRITTEN BY MORION OS WRITE TEST 0123456789";
    if vfs::write(wfd, 0, data) == u64::MAX {
        println("app: write \"/HELLO.TXT\" -> FAILED");
    }
    if vfs::read(wfd, 0, 4096) == u64::MAX {
        println("app: read back -> FAILED");
    }
    vfs::close(wfd);

    // 3b. 跨簇扩展: 写满一页 4096 字节, 强制分配多个簇, 再读回逐字节校验。
    unsafe {
        let buf = core::slice::from_raw_parts_mut(
            core::ptr::addr_of_mut!(BIG_WRITE_BUF) as *mut u8,
            4096,
        );
        for (i, slot) in buf.iter_mut().enumerate() {
            *slot = b'A' + (i % 26) as u8;
        }
    }
    let bfd = vfs::open("/HELLO.TXT");
    if bfd == u64::MAX {
        println("app: open \"/HELLO.TXT\" for big write -> NOT FOUND");
        return;
    }
    let bwrite = unsafe {
        let slice =
            core::slice::from_raw_parts(core::ptr::addr_of!(BIG_WRITE_BUF) as *const u8, 4096);
        vfs::write(bfd, 0, slice)
    };
    let bread = vfs::read(bfd, 0, 4096);
    if bwrite != 4096 || bread != 4096 {
        println("app: big write/read FAILED");
    } else {
        let bcontent =
            unsafe { core::slice::from_raw_parts(vfs::RESULT_BUF as *const u8, bread as usize) };
        let mut ok = true;
        for (i, &b) in bcontent.iter().enumerate() {
            if b != (b'A' + (i % 26) as u8) {
                ok = false;
                break;
            }
        }
        if !ok {
            println("app: big write verify MISMATCH");
        }
    }
    vfs::close(bfd);

    // 4. open("/") -> readdir -> close: 列出根目录。
    let dfd = vfs::open("/");
    if dfd == u64::MAX {
        println("app: open \"/\" -> FAILED");
        return;
    }
    if vfs::readdir(dfd) == u64::MAX {
        println("app: readdir \"/\" -> FAILED");
    }
    vfs::close(dfd);

    // 5. FS-2 自测: mkdir → stat → creat → write → stat → unlink → rmdir 全链路。
    if vfs::mkdir("/DIRT") != 1 {
        println("app: FS2 mkdir \"/DIRT\" FAILED");
        return;
    }
    if vfs::stat("/DIRT") == u64::MAX {
        println("app: FS2 stat \"/DIRT\" FAILED");
        return;
    }
    {
        let dstat = unsafe { *(vfs::RESULT_BUF as *const vfs::Stat) };
        if dstat.is_dir != 1 {
            println("app: FS2 stat \"/DIRT\" not dir FAILED");
            return;
        }
    }

    let cfd = vfs::creat("/DIRT/NEWFILE");
    if cfd == u64::MAX {
        println("app: FS2 creat \"/DIRT/NEWFILE\" FAILED");
        return;
    }
    if vfs::write(cfd, 0, b"hello") != 5 {
        println("app: FS2 write \"/DIRT/NEWFILE\" FAILED");
        return;
    }
    vfs::close(cfd);

    if vfs::stat("/DIRT/NEWFILE") == u64::MAX {
        println("app: FS2 stat \"/DIRT/NEWFILE\" FAILED");
        return;
    }
    {
        let fstat = unsafe { *(vfs::RESULT_BUF as *const vfs::Stat) };
        if fstat.size != 5 || fstat.is_dir != 0 {
            println("app: FS2 stat \"/DIRT/NEWFILE\" size/dir FAILED");
            return;
        }
    }

    if vfs::unlink("/DIRT/NEWFILE") != 1 {
        println("app: FS2 unlink \"/DIRT/NEWFILE\" FAILED");
        return;
    }
    if vfs::rmdir("/DIRT") != 1 {
        println("app: FS2 rmdir \"/DIRT\" FAILED");
        return;
    }
    if vfs::stat("/DIRT") != u64::MAX {
        println("app: FS2 stat \"/DIRT\" still exists FAILED");
        return;
    }

    // 6. FS-3 自测: mkdir/touch/rm 后 readdir 验证目录结构变化。
    //    与 FS-2 (用 stat 校验) 不同, 这里用 readdir 校验父/子目录的可见性变化。
    if vfs::mkdir("/FS3DIR") != 1 {
        println("app: FS3 mkdir \"/FS3DIR\" FAILED");
        return;
    }
    let rfd = vfs::open("/");
    if rfd == u64::MAX || !readdir_has(rfd, "FS3DIR", true) {
        println("app: FS3 readdir \"/\" missing FS3DIR FAILED");
        return;
    }
    vfs::close(rfd);

    // touch: 创建空文件后立即关闭。
    let tfd = vfs::creat("/FS3DIR/TOUCH.TXT");
    if tfd == u64::MAX {
        println("app: FS3 creat \"/FS3DIR/TOUCH.TXT\" FAILED");
        return;
    }
    vfs::close(tfd);

    let d1 = vfs::open("/FS3DIR");
    if d1 == u64::MAX || !readdir_has(d1, "TOUCH.TXT", false) {
        println("app: FS3 readdir \"/FS3DIR\" missing TOUCH.TXT FAILED");
        return;
    }
    vfs::close(d1);

    // rm: 删除文件后 readdir 应不再可见。
    if vfs::unlink("/FS3DIR/TOUCH.TXT") != 1 {
        println("app: FS3 unlink \"/FS3DIR/TOUCH.TXT\" FAILED");
        return;
    }
    let d2 = vfs::open("/FS3DIR");
    if d2 == u64::MAX || readdir_has(d2, "TOUCH.TXT", false) {
        println("app: FS3 readdir \"/FS3DIR\" still has TOUCH.TXT FAILED");
        return;
    }
    vfs::close(d2);

    // rmdir: 删除空目录后根目录 readdir 应不再可见。
    if vfs::rmdir("/FS3DIR") != 1 {
        println("app: FS3 rmdir \"/FS3DIR\" FAILED");
        return;
    }
    let rfd2 = vfs::open("/");
    if rfd2 == u64::MAX || readdir_has(rfd2, "FS3DIR", true) {
        println("app: FS3 readdir \"/\" still has FS3DIR FAILED");
        return;
    }
    vfs::close(rfd2);

    // 7. FS-4 自测 (阶段 C2): 挂载层把 `/tmp/**` 路由到 tmpfs_srv, 其余仍走 fat32。
    //    走通 mkdir → readdir → creat → write → read → unlink → rmdir 全链路。
    let tfd_root = vfs::open("/tmp");
    if tfd_root == u64::MAX {
        println("app: FS4 open /tmp FAILED (mount routing)");
        return;
    }
    vfs::close(tfd_root);

    if vfs::mkdir("/tmp/D1") != 1 {
        println("app: FS4 mkdir /tmp/D1 FAILED");
        return;
    }
    let tdir = vfs::open("/tmp");
    if tdir == u64::MAX || !readdir_has(tdir, "D1", true) {
        println("app: FS4 readdir /tmp missing D1 FAILED");
        return;
    }
    vfs::close(tdir);

    let tf = vfs::creat("/tmp/D1/F1");
    if tf == u64::MAX {
        println("app: FS4 creat /tmp/D1/F1 FAILED");
        return;
    }
    if vfs::write(tf, 0, b"tmpfs!") != 6 {
        println("app: FS4 write FAILED");
        return;
    }
    if vfs::read(tf, 0, 64) != 6 {
        println("app: FS4 read FAILED");
        return;
    }
    {
        let got = unsafe { core::slice::from_raw_parts(vfs::RESULT_BUF as *const u8, 6) };
        if got != b"tmpfs!" {
            println("app: FS4 read content MISMATCH");
            return;
        }
    }
    vfs::close(tf);

    if vfs::unlink("/tmp/D1/F1") != 1 {
        println("app: FS4 unlink /tmp/D1/F1 FAILED");
        return;
    }
    if vfs::rmdir("/tmp/D1") != 1 {
        println("app: FS4 rmdir /tmp/D1 FAILED");
        return;
    }

    // 路由正确性: 同一调用序列在 `/` 下仍由 fat32 服务 (HELLO.TXT 可见)。
    let rootfd = vfs::open("/");
    if rootfd == u64::MAX || !readdir_has(rootfd, "HELLO.TXT", false) {
        println("app: FS4 readdir / missing HELLO.TXT FAILED (routing)");
        return;
    }
    vfs::close(rootfd);

    // 8. FS-5 自测 (阶段 C3): MorionFS (块设备后端, 挂载 /mfs)。
    //    - 空白 mfs.img 首次挂载 -> mfs_srv 自动格式化;
    //    - mkdir/creat/write/read 走通 (每块 CRC32 校验);
    //    - 快照: 记录根 -> 覆盖文件 -> 回滚 -> 旧内容恢复 (验证 COW 语义);
    //    - 持久化: 留下 /mfs/PERSIST.TXT, 二次启动读回即证明数据已落盘。

    // 持久化检查 (第二次及以后启动)。
    let pfd = vfs::open("/mfs/PERSIST.TXT");
    if pfd != u64::MAX {
        let n = vfs::read(pfd, 0, 64);
        vfs::close(pfd);
        if n != 6 {
            println("app: FS5 persist read FAILED");
            return;
        }
        let got = unsafe { core::slice::from_raw_parts(vfs::RESULT_BUF as *const u8, 6) };
        if got != b"MFS-OK" {
            println("app: FS5 persist content MISMATCH");
            return;
        }
    }

    // 幂等准备: 上一轮若在本段清理之前提前返回 (例如快照失败), `/mfs/D` 会残留在盘上;
    // 这里先清掉残留, 否则本次 `mkdir` 会因为目录已存在而失败, 把上一次的失败传染到本次。
    vfs::unlink("/mfs/D/F");
    vfs::rmdir("/mfs/D");

    if vfs::mkdir("/mfs/D") != 1 {
        println("app: FS5 mkdir /mfs/D FAILED");
        return;
    }
    let mroot = vfs::open("/mfs");
    if mroot == u64::MAX || !readdir_has(mroot, "D", true) {
        println("app: FS5 readdir /mfs missing D FAILED");
        return;
    }
    vfs::close(mroot);

    let mf = vfs::creat("/mfs/D/F");
    if mf == u64::MAX || vfs::write(mf, 0, b"MFS-V1") != 6 {
        println("app: FS5 create/write /mfs/D/F FAILED");
        return;
    }
    vfs::close(mf);

    // 快照当前状态 (D/F = "MFS-V1")。
    let snap = vfs::mfs_snapshot();
    if snap == u64::MAX {
        println("app: FS5 snapshot FAILED");
        return;
    }

    // 覆盖为 V2。
    let mf2 = vfs::open("/mfs/D/F");
    if mf2 == u64::MAX || vfs::write(mf2, 0, b"MFS-V2") != 6 {
        println("app: FS5 overwrite V2 FAILED");
        return;
    }
    vfs::close(mf2);

    // 回滚到快照: COW 未覆盖旧块, 故快照树仍完好, 应读回 V1。
    if vfs::mfs_snapshot_restore(snap as u32) != 1 {
        println("app: FS5 snapshot restore FAILED");
        return;
    }
    let mf3 = vfs::open("/mfs/D/F");
    if mf3 == u64::MAX {
        println("app: FS5 reopen after restore FAILED");
        return;
    }
    let rn = vfs::read(mf3, 0, 64);
    vfs::close(mf3);
    if rn != 6 {
        println("app: FS5 read after restore FAILED");
        return;
    }
    {
        let got = unsafe { core::slice::from_raw_parts(vfs::RESULT_BUF as *const u8, 6) };
        if got != b"MFS-V1" {
            println("app: FS5 snapshot did not preserve old content FAILED");
            return;
        }
    }

    // 清理 (COW 只增不回收: 旧块保留给快照, 逻辑上删除即可)。
    if vfs::unlink("/mfs/D/F") != 1 {
        println("app: FS5 unlink FAILED");
        return;
    }
    if vfs::rmdir("/mfs/D") != 1 {
        println("app: FS5 rmdir FAILED");
        return;
    }

    // 持久化标记 (仅首次创建; 二次启动由上面的检查读回)。
    if vfs::open("/mfs/PERSIST.TXT") == u64::MAX {
        let pfd2 = vfs::creat("/mfs/PERSIST.TXT");
        if pfd2 == u64::MAX || vfs::write(pfd2, 0, b"MFS-OK") != 6 {
            println("app: FS5 create persist marker FAILED");
            return;
        }
        vfs::close(pfd2);
    }

    // 9. FS-6 自测 (阶段 C3): 运行时挂载 —— 挂载表不再只由编译期常量决定。
    //    - `/mnt0` 未挂载时应不可路由 (open 失败);
    //    - `MNTA` 空前缀 -> mount_srv 自动分配 `/mnt0` 给 mfs_srv;
    //    - 同一服务经新挂载点可达 (证明最长前缀匹配 + 前缀剥离都对);
    //    - `MNTD` 卸载后 `/mnt0` 又不可路由, 而 `/mfs` 不受影响。
    if vfs::open("/mnt0") != u64::MAX {
        println("app: FS6 /mnt0 reachable before mount FAILED");
        return;
    }
    if vfs::mount("", vfs::MFS_DOMAIN) == u64::MAX {
        println("app: FS6 runtime mount (auto) FAILED");
        return;
    }
    let a0 = vfs::open("/mnt0");
    if a0 == u64::MAX {
        println("app: FS6 open /mnt0 after mount FAILED");
        return;
    }
    vfs::close(a0);

    if vfs::umount("/mnt0") != 1 {
        println("app: FS6 umount /mnt0 FAILED");
        return;
    }
    if vfs::open("/mnt0") != u64::MAX {
        println("app: FS6 /mnt0 still routed after umount FAILED");
        return;
    }
    let mfs_still = vfs::open("/mfs");
    if mfs_still == u64::MAX {
        println("app: FS6 /mfs broken by umount FAILED");
        return;
    }
    vfs::close(mfs_still);

    // 句柄生命周期: 反复 open/close 不应耗尽内核句柄槽 (close 会撤销句柄,
    // 槽位复用)。循环次数 > 内核 HANDLE_SLOTS (32), 泄漏即在此暴露。
    for _ in 0..40 {
        let f = vfs::open("/mfs");
        if f == u64::MAX {
            println("app: FS6 capability handle slot leak FAILED");
            return;
        }
        vfs::close(f);
    }

    // 10. FS-7 自测 (阶段 C3): ext2 只读兼容 (挂载既有 Linux 分区)。
    //     - 根目录可达且列出宿主预置的 HELLO.TXT / SUBDIR (长名字段 = ext2 名字);
    //     - 读文件内容与宿主预置一致;
    //     - 子目录递归可达;
    //     - 大小写不敏感回退 (ext2 本身大小写敏感, 便于交互才加这一层);
    //     - 只读: 创建文件必须被拒; 不存在的路径必须失败。
    let efd = vfs::open("/ext2");
    if efd == u64::MAX {
        println("app: FS7 open /ext2 FAILED");
        return;
    }
    if !readdir_has_long(efd, "HELLO.TXT", false) || !readdir_has_long(efd, "SUBDIR", true) {
        println("app: FS7 ext2 root listing FAILED");
        vfs::close(efd);
        return;
    }
    vfs::close(efd);

    let hfd = vfs::open("/ext2/HELLO.TXT");
    if hfd == u64::MAX {
        println("app: FS7 open /ext2/HELLO.TXT FAILED");
        return;
    }
    let hn = vfs::read(hfd, 0, 4096);
    vfs::close(hfd);
    if hn == u64::MAX {
        println("app: FS7 read /ext2/HELLO.TXT FAILED");
        return;
    }
    {
        let want = b"Hello from ext2!\n";
        let got = unsafe { core::slice::from_raw_parts(vfs::RESULT_BUF as *const u8, hn as usize) };
        if got.len() < want.len() || &got[..want.len()] != want {
            println("app: FS7 /ext2/HELLO.TXT content MISMATCH");
            return;
        }
    }

    // stat 也走 ext2 服务 (app 的结果页已共享给 ext2 域)。
    if vfs::stat("/ext2/HELLO.TXT") == u64::MAX {
        println("app: FS7 stat /ext2/HELLO.TXT FAILED");
        return;
    }
    {
        let st = unsafe { *(vfs::RESULT_BUF as *const vfs::Stat) };
        if st.is_dir != 0 || st.size == 0 {
            println("app: FS7 stat /ext2/HELLO.TXT wrong metadata FAILED");
            return;
        }
    }
    if vfs::stat("/ext2/SUBDIR") == u64::MAX {
        println("app: FS7 stat /ext2/SUBDIR FAILED");
        return;
    }
    {
        let st = unsafe { *(vfs::RESULT_BUF as *const vfs::Stat) };
        if st.is_dir != 1 {
            println("app: FS7 stat /ext2/SUBDIR not dir FAILED");
            return;
        }
    }

    let sfd = vfs::open("/ext2/SUBDIR");
    if sfd == u64::MAX {
        println("app: FS7 open /ext2/SUBDIR FAILED");
        return;
    }
    if !readdir_has_long(sfd, "NESTED.TXT", false) {
        println("app: FS7 /ext2/SUBDIR listing FAILED");
        vfs::close(sfd);
        return;
    }
    vfs::close(sfd);

    let nfd = vfs::open("/ext2/SUBDIR/NESTED.TXT");
    if nfd == u64::MAX {
        println("app: FS7 open /ext2/SUBDIR/NESTED.TXT FAILED");
        return;
    }
    let nn = vfs::read(nfd, 0, 4096);
    vfs::close(nfd);
    if nn == u64::MAX || nn < 3 {
        println("app: FS7 read /ext2/SUBDIR/NESTED.TXT FAILED");
        return;
    }

    // 大小写不敏感回退: 盘上名字是大写, 小写路径也应命中。
    let cfd7 = vfs::open("/ext2/hello.txt");
    if cfd7 == u64::MAX {
        println("app: FS7 case-insensitive fallback FAILED");
        return;
    }
    vfs::close(cfd7);

    // 只读: 创建文件必须被拒 (ext2_srv 对写类 tag 一律回 u64::MAX)。
    if vfs::creat("/ext2/NEW.TXT") != u64::MAX {
        println("app: FS7 creat on read-only ext2 NOT rejected FAILED");
        return;
    }
    if vfs::open("/ext2/NOPE.TXT") != u64::MAX {
        println("app: FS7 missing path NOT rejected FAILED");
        return;
    }

    // 11. FS-8 自测 (阶段 C3): VFAT 长名 —— 读取 + 条目长名字段 + 按长名打开。
    let rfd8 = vfs::open("/");
    if rfd8 == u64::MAX {
        println("app: FS8 open / FAILED");
        return;
    }
    if !readdir_has_long(rfd8, "Long File Name.txt", false) {
        println("app: FS8 long name missing from readdir FAILED");
        vfs::close(rfd8);
        return;
    }
    vfs::close(rfd8);

    let lfd = vfs::open("/Long File Name.txt");
    if lfd == u64::MAX {
        println("app: FS8 open by long name FAILED");
        return;
    }
    let ln = vfs::read(lfd, 0, 4096);
    vfs::close(lfd);
    if ln == u64::MAX {
        println("app: FS8 read by long name FAILED");
        return;
    }
    {
        let want = b"long name read via VFAT LFN!\n";
        let got = unsafe { core::slice::from_raw_parts(vfs::RESULT_BUF as *const u8, ln as usize) };
        if got != want {
            println("app: FS8 long name content MISMATCH");
            return;
        }
    }
    // 长名匹配对大小写不敏感 (VFAT LFN 语义)。
    let lc = vfs::open("/long file name.txt");
    if lc == u64::MAX {
        println("app: FS8 case-insensitive long name open FAILED");
        return;
    }
    vfs::close(lc);

    // 12. FS-9 自测 (阶段 D/M1): 卷层 —— block_srv 解析 MBR 分区表并按卷首签名探测 FS 类型。
    //     - 向后兼容: 卷 0 = ns1 整盘 FAT、卷 2 = ns3 整盘 ext2 (start_lba 均为 0);
    //     - 新增的第 4 张盘有 MBR 两个主分区, 必须分别被识别为 FAT(@2048) 与 ext2(@34816)。
    let nvol = block_list_volumes(vfs::RESULT_BUF as *mut u8, 16);
    if nvol < 6 {
        println("app: FS9 volume count < 6 FAILED");
        return;
    }
    {
        // exFAT 盘 (nsid 5) 必须被卷层按签名识别为 EXFAT。
        let mut exfat_vol = false;
        let mut i = 0u64;
        while i < nvol {
            let d = vol_desc(vfs::RESULT_BUF as *const u8, i as usize);
            if d.kind == VOL_KIND_EXFAT && d.nsid == 5 && d.start_lba == 0 {
                exfat_vol = true;
            }
            i += 1;
        }
        if !exfat_vol {
            println("app: FS9 exFAT volume missing FAILED");
            return;
        }
    }
    {
        let v0 = vol_desc(vfs::RESULT_BUF as *const u8, 0);
        if v0.kind != VOL_KIND_FAT || v0.nsid != 1 || v0.start_lba != 0 {
            println("app: FS9 vol0 (whole-disk FAT32) FAILED");
            return;
        }
        let v2 = vol_desc(vfs::RESULT_BUF as *const u8, 2);
        if v2.kind != VOL_KIND_EXT2 || v2.nsid != 3 || v2.start_lba != 0 {
            println("app: FS9 vol2 (whole-disk ext2) FAILED");
            return;
        }
    }
    {
        // 分区盘 (nsid 4) 的两个分区。
        let mut fat_part = false;
        let mut ext2_part = false;
        let mut i = 0u64;
        while i < nvol {
            let d = vol_desc(vfs::RESULT_BUF as *const u8, i as usize);
            if d.nsid == 4 {
                if d.kind == VOL_KIND_FAT && d.start_lba == 2048 {
                    fat_part = true;
                }
                if d.kind == VOL_KIND_EXT2 && d.start_lba == 34816 {
                    ext2_part = true;
                }
            }
            i += 1;
        }
        if !fat_part || !ext2_part {
            println("app: FS9 MBR partition scan FAILED");
        }
    }

    // 13. FS-10 自测 (阶段 D/M2): MFS v2 空闲位图 + 空间回收 (GC)。
    //     - 反复覆盖同一文件时 COW 持续吃新块 -> 空闲块下降;
    //     - 删除后 GC 按可达性重建位图 -> 这些垃圾块被归还;
    //     - 快照根也是可达根: GC 不得回收快照仍引用的历史版本。
    let st0 = vfs::mfs_stat();
    if st0 == u64::MAX {
        println("app: FS10 mfs_stat FAILED");
        return;
    }
    let free0 = st0 & 0xFFFF_FFFF;
    if (st0 >> 32) == 0 || free0 == 0 {
        println("app: FS10 stat sanity FAILED");
        return;
    }

    // 64 KiB 文件覆盖写 4 轮 (每轮 COW 出 32 个数据块 + inode + 目录链)。
    let gfd = vfs::creat("/mfs/GC.BIN");
    if gfd == u64::MAX {
        println("app: FS10 creat /mfs/GC.BIN FAILED");
        return;
    }
    let chunk = unsafe {
        core::slice::from_raw_parts_mut(core::ptr::addr_of_mut!(BIG_WRITE_BUF) as *mut u8, 4096)
    };
    for round in 0..4u32 {
        for (i, slot) in chunk.iter_mut().enumerate() {
            *slot = b'0' + (round as u8) + (i % 8) as u8;
        }
        let mut off = 0u32;
        while off < 65536 {
            let n = unsafe {
                let slice = core::slice::from_raw_parts(
                    core::ptr::addr_of!(BIG_WRITE_BUF) as *const u8,
                    4096,
                );
                vfs::write(gfd, off as u64, slice)
            };
            if n != 4096 {
                println("app: FS10 chunked write FAILED");
                vfs::close(gfd);
                return;
            }
            off += 4096;
        }
    }
    vfs::close(gfd);

    let st1 = vfs::mfs_stat();
    if st1 == u64::MAX {
        println("app: FS10 stat after overwrite FAILED");
        return;
    }
    let free1 = st1 & 0xFFFF_FFFF;
    if free1 + 100 > free0 {
        println("app: FS10 COW did not consume space FAILED");
        return;
    }

    // 删除 + 回收: 该文件的数据块 / inode / 中间目录块都应被归还。
    if vfs::unlink("/mfs/GC.BIN") != 1 {
        println("app: FS10 unlink /mfs/GC.BIN FAILED");
        return;
    }
    let freed = vfs::mfs_gc();
    if freed == u64::MAX || freed < 100 {
        println("app: FS10 GC did not reclaim space FAILED");
        return;
    }

    // 快照安全: 快照之后覆盖写, GC 仍保留快照引用的旧版本, 回滚后内容应为旧值。
    let sfd10 = vfs::creat("/mfs/GCS.BIN");
    if sfd10 == u64::MAX || vfs::write(sfd10, 0, b"SNAP-A") != 6 {
        println("app: FS10 create snapshot file FAILED");
        return;
    }
    vfs::close(sfd10);
    let snap10 = vfs::mfs_snapshot();
    if snap10 == u64::MAX {
        println("app: FS10 snapshot FAILED");
        return;
    }
    let s2fd = vfs::open("/mfs/GCS.BIN");
    if s2fd == u64::MAX || vfs::write(s2fd, 0, b"SNAP-B") != 6 {
        println("app: FS10 overwrite after snapshot FAILED");
        return;
    }
    vfs::close(s2fd);
    if vfs::mfs_gc() == u64::MAX {
        println("app: FS10 GC with snapshot FAILED");
        return;
    }
    // 分配扰动: 若 GC 误释放了快照引用的块, 这里的分配会立刻把它覆盖, 从而在下面
    // 的回滚读回中暴露 (否则可能侥幸读到还没被复用的旧内容)。
    let cfd10 = vfs::creat("/mfs/CHURN.BIN");
    if cfd10 == u64::MAX {
        println("app: FS10 creat churn file FAILED");
        return;
    }
    let mut coff = 0u32;
    while coff < 32768 {
        let n = unsafe {
            let slice =
                core::slice::from_raw_parts(core::ptr::addr_of!(BIG_WRITE_BUF) as *const u8, 4096);
            vfs::write(cfd10, coff as u64, slice)
        };
        if n != 4096 {
            println("app: FS10 churn write FAILED");
            vfs::close(cfd10);
            return;
        }
        coff += 4096;
    }
    vfs::close(cfd10);
    if vfs::mfs_snapshot_restore(snap10 as u32) != 1 {
        println("app: FS10 restore after GC FAILED");
        return;
    }
    let r3 = vfs::open("/mfs/GCS.BIN");
    if r3 == u64::MAX {
        println("app: FS10 reopen after restore+GC FAILED");
        return;
    }
    let rn3 = vfs::read(r3, 0, 64);
    vfs::close(r3);
    if rn3 != 6 {
        println("app: FS10 read after restore+GC FAILED");
        return;
    }
    {
        let got = unsafe { core::slice::from_raw_parts(vfs::RESULT_BUF as *const u8, 6) };
        if got != b"SNAP-A" {
            println("app: FS10 GC broke snapshot history FAILED");
            return;
        }
    }

    // 清理 (旧版本仍被快照钉住, 回收不掉的块有限且随快照淘汰自然释放)。
    if vfs::unlink("/mfs/GCS.BIN") != 1 {
        println("app: FS10 cleanup unlink FAILED");
        return;
    }
    if vfs::mfs_gc() == u64::MAX {
        println("app: FS10 final GC FAILED");
        return;
    }

    // 14. FS-11 自测 (阶段 D/M3): MFS v2 大文件 —— 直接 / 一级 / 二级间接块映射。
    //     - 起点放在直接区末尾, 连写 5 页必然从直接区跨进一级间接区;
    //     - 再在二级间接区写一页, 文件大小突破旧的 ≈4 MiB 上限;
    //     - GC 必须把间接块及其指向的数据块都当作可达, 否则稀疏大文件读回会失败。
    let bfd11 = vfs::creat("/mfs/BIG.BIN");
    if bfd11 == u64::MAX {
        println("app: FS11 creat /mfs/BIG.BIN FAILED");
        return;
    }
    let cross_off = (MFS_FILE_DIRECT - 3) as u32 * MFS_DATA_CAP as u32;
    let l2_off = (MFS_FILE_DIRECT + MFS_IND_CAP) as u32 * MFS_DATA_CAP as u32;
    if !fs11_write_pages(bfd11, cross_off, 5, 0x40) {
        println("app: FS11 cross-boundary write FAILED");
        vfs::close(bfd11);
        return;
    }
    if !fs11_write_pages(bfd11, l2_off, 1, 0x80) {
        println("app: FS11 second-level write FAILED");
        vfs::close(bfd11);
        return;
    }
    // 大小必须超过 4 MiB (直接区上限) —— 这正是 M3 要突破的边界。
    if vfs::stat("/mfs/BIG.BIN") != core::mem::size_of::<vfs::Stat>() as u64 {
        println("app: FS11 stat FAILED");
        vfs::close(bfd11);
        return;
    }
    let size11 = unsafe { core::ptr::read_unaligned(vfs::RESULT_BUF as *const vfs::Stat) }.size;
    if size11 <= 4 * 1024 * 1024 {
        println("app: FS11 size still capped at 4 MiB FAILED");
        vfs::close(bfd11);
        return;
    }
    if !fs11_verify_pages(bfd11, cross_off, 5, 0x40) || !fs11_verify_pages(bfd11, l2_off, 1, 0x80) {
        println("app: FS11 read back FAILED");
        vfs::close(bfd11);
        return;
    }
    // 回收 + 分配扰动: GC 若误回收间接块/数据块, 扰动会把它们覆盖, 读回即暴露。
    if vfs::mfs_gc() == u64::MAX {
        println("app: FS11 GC FAILED");
        vfs::close(bfd11);
        return;
    }
    let chfd11 = vfs::creat("/mfs/CHURN3.BIN");
    if chfd11 == u64::MAX || !fs11_write_pages(chfd11, 0, 8, 0xC0) {
        println("app: FS11 churn FAILED");
        vfs::close(bfd11);
        return;
    }
    vfs::close(chfd11);
    if !fs11_verify_pages(bfd11, cross_off, 5, 0x40) || !fs11_verify_pages(bfd11, l2_off, 1, 0x80) {
        println("app: FS11 read back after GC FAILED");
        vfs::close(bfd11);
        return;
    }
    vfs::close(bfd11);

    // 清理: 删除大文件与扰动文件, 回收它们的数据块与间接块。
    if vfs::unlink("/mfs/BIG.BIN") != 1 || vfs::unlink("/mfs/CHURN3.BIN") != 1 {
        println("app: FS11 cleanup unlink FAILED");
        return;
    }
    if vfs::mfs_gc() == u64::MAX {
        println("app: FS11 final GC FAILED");
    }

    // 15. FS-12 自测 (阶段 D/M4): MFS v2 目录与长名。
    //     - 单目录 200 项、名长 16 字节: 节点块只容 170 项, 其余必进扩展目录块;
    //     - 长名 (57 字节) 全程走 IPC payload: 建 / 查 / 读 / readdir 回传;
    //     - 深目录 20 级 (旧上限 12), 验证目录嵌套不再受结构限制;
    //     - GC + 分配扰动后逐项读回: 目录扩展块/索引块若被误回收, 扰动会覆盖它们。
    const FS12_DIR: &str = "/mfs/DIR12";
    const FS12_FILES: u32 = 200;
    const FS12_LONG: &str = "/mfs/LONGFILE_WITH_A_REALLY_LONG_NAME_0123456789ABCDEFGHIJ.TXT";
    // 幂等准备: 上一轮若在本段清理之前提前返回 (失败 / 被中断), 残留在盘上的目录与文件
    // 会让本次 `mkdir` 因「目录已存在」而失败, 把上一次的失败传染到本次 —— 与 FS-5 的
    // 处理相同 (MFS 是持久卷, 自测必须能反复重跑)。
    {
        let mut cb = [0u8; 64];
        let nb = FS12_DIR.len() + 1;
        cb[..FS12_DIR.len()].copy_from_slice(FS12_DIR.as_bytes());
        cb[FS12_DIR.len()] = b'/';
        let mut k = 0u32;
        while k < FS12_FILES {
            fs12_name(k, &mut cb[nb..nb + 16]);
            let s = unsafe { core::str::from_utf8_unchecked(&cb[..nb + 16]) };
            vfs::unlink(s);
            k += 1;
        }
        vfs::unlink("/mfs/CHURN4.BIN");
        vfs::unlink(FS12_LONG);
        vfs::rmdir(FS12_DIR);
        // 深目录 (18 级, 名字按 lvl % 26 循环) 自叶向上删。
        let mut db = [0u8; 64];
        db[..8].copy_from_slice(b"/mfs/D12");
        let mut dl0 = 8usize;
        let mut lvl0 = 0u32;
        while lvl0 < 18 {
            db[dl0] = b'/';
            db[dl0 + 1] = b'a' + (lvl0 % 26) as u8;
            dl0 += 2;
            lvl0 += 1;
        }
        db[dl0] = b'/';
        db[dl0 + 1] = b'H';
        let hp0 = unsafe { core::str::from_utf8_unchecked(&db[..dl0 + 2]) };
        vfs::unlink(hp0);
        let mut d0 = dl0;
        while d0 > 8 {
            let s = unsafe { core::str::from_utf8_unchecked(&db[..d0]) };
            vfs::rmdir(s);
            d0 -= 2;
        }
        vfs::rmdir("/mfs/D12");
    }
    if vfs::mkdir(FS12_DIR) != 1 {
        println("app: FS12 mkdir /mfs/DIR12 FAILED");
        return;
    }
    let mut pbuf = [0u8; 64];
    let nbase = FS12_DIR.len() + 1;
    pbuf[..FS12_DIR.len()].copy_from_slice(FS12_DIR.as_bytes());
    pbuf[FS12_DIR.len()] = b'/';
    // 建 200 个 16 字节名文件, 每个写入唯一字节; 中途回收以压住 COW 垃圾水位。
    let mut i = 0u32;
    while i < FS12_FILES {
        fs12_name(i, &mut pbuf[nbase..nbase + 16]);
        let s = unsafe { core::str::from_utf8_unchecked(&pbuf[..nbase + 16]) };
        let fd = vfs::creat(s);
        if fd == u64::MAX {
            println("app: FS12 creat FAILED");
            return;
        }
        let n = unsafe {
            let buf = core::slice::from_raw_parts_mut(
                core::ptr::addr_of_mut!(BIG_WRITE_BUF) as *mut u8,
                4096,
            );
            buf.fill(i as u8);
            let slice =
                core::slice::from_raw_parts(core::ptr::addr_of!(BIG_WRITE_BUF) as *const u8, 4096);
            vfs::write(fd, 0, slice)
        };
        vfs::close(fd);
        if n != 4096 {
            println("app: FS12 write FAILED");
            return;
        }
        i += 1;
        if i.is_multiple_of(64) && vfs::mfs_gc() == u64::MAX {
            println("app: FS12 mid GC FAILED");
            return;
        }
    }
    // 逐项按名查回 (第 171 项起的查找必须穿过扩展目录块) 并校验内容。
    i = 0;
    while i < FS12_FILES {
        fs12_name(i, &mut pbuf[nbase..nbase + 16]);
        let s = unsafe { core::str::from_utf8_unchecked(&pbuf[..nbase + 16]) };
        let fd = vfs::open(s);
        if fd == u64::MAX {
            println("app: FS12 open FAILED");
            return;
        }
        let n = vfs::read(fd, 0, 512);
        vfs::close(fd);
        let got = unsafe { core::slice::from_raw_parts(vfs::RESULT_BUF as *const u8, 512) };
        if n != 512 || got.iter().any(|&b| b != i as u8) {
            println("app: FS12 read back FAILED");
            return;
        }
        i += 1;
    }
    // readdir: 一页放不下 200 项, 必须正好写满 RESULT_MAX_ENTRIES 条且长名回传完整。
    let dfd = vfs::open(FS12_DIR);
    if dfd == u64::MAX {
        println("app: FS12 open dir FAILED");
        return;
    }
    let dn = vfs::readdir(dfd);
    vfs::close(dfd);
    if dn == u64::MAX {
        println("app: FS12 readdir FAILED");
        return;
    }
    let dents = dn as usize / core::mem::size_of::<vfs::DirEntry>();
    if dents != vfs::RESULT_MAX_ENTRIES {
        println("app: FS12 readdir count FAILED");
        return;
    }
    let drr =
        unsafe { core::slice::from_raw_parts(vfs::RESULT_BUF as *const vfs::DirEntry, dents) };
    for de in drr.iter() {
        if de.long_len as usize != 16 {
            println("app: FS12 readdir long name FAILED");
            return;
        }
    }
    // 长名 (57 字节) 端到端: 建 → 关 → 按全名重开 → 读回 → readdir 原样回传。
    let lfd = vfs::creat(FS12_LONG);
    if lfd == u64::MAX {
        println("app: FS12 creat long name FAILED");
        return;
    }
    let ln = unsafe {
        let buf = core::slice::from_raw_parts_mut(
            core::ptr::addr_of_mut!(BIG_WRITE_BUF) as *mut u8,
            4096,
        );
        buf.fill(0xA5);
        let slice =
            core::slice::from_raw_parts(core::ptr::addr_of!(BIG_WRITE_BUF) as *const u8, 4096);
        vfs::write(lfd, 0, slice)
    };
    vfs::close(lfd);
    if ln != 4096 {
        println("app: FS12 long name write FAILED");
        return;
    }
    let lfd2 = vfs::open(FS12_LONG);
    if lfd2 == u64::MAX {
        println("app: FS12 long name reopen FAILED");
        return;
    }
    let lrn = vfs::read(lfd2, 0, 4096);
    vfs::close(lfd2);
    let lgot = unsafe { core::slice::from_raw_parts(vfs::RESULT_BUF as *const u8, 4096) };
    if lrn != 4096 || lgot.iter().any(|&b| b != 0xA5) {
        println("app: FS12 long name read FAILED");
        return;
    }
    let mfd = vfs::open("/mfs");
    if mfd == u64::MAX {
        println("app: FS12 open /mfs FAILED");
        return;
    }
    let mn = vfs::readdir(mfd);
    vfs::close(mfd);
    if mn == u64::MAX {
        println("app: FS12 readdir /mfs FAILED");
        return;
    }
    let ments = mn as usize / core::mem::size_of::<vfs::DirEntry>();
    let marr =
        unsafe { core::slice::from_raw_parts(vfs::RESULT_BUF as *const vfs::DirEntry, ments) };
    let lwant = &FS12_LONG.as_bytes()[5..]; // 去掉 "/mfs/" 前缀
    if !marr
        .iter()
        .any(|de| de.long_len as usize == lwant.len() && &de.long[..lwant.len()] == lwant)
    {
        println("app: FS12 readdir long name FAILED");
        return;
    }
    // 深目录: /mfs/D12 起逐级建 18 个单字母目录 -> 叶子深度 20 (旧上限 12)。
    if vfs::mkdir("/mfs/D12") != 1 {
        println("app: FS12 mkdir D12 FAILED");
        return;
    }
    let mut dp = [0u8; 64];
    dp[..8].copy_from_slice(b"/mfs/D12");
    let mut dl = 8usize;
    let mut lvl = 0u32;
    while lvl < 18 {
        dp[dl] = b'/';
        dp[dl + 1] = b'a' + (lvl % 26) as u8;
        dl += 2;
        let s = unsafe { core::str::from_utf8_unchecked(&dp[..dl]) };
        if vfs::mkdir(s) != 1 {
            println("app: FS12 deep mkdir FAILED");
            return;
        }
        lvl += 1;
    }
    dp[dl] = b'/';
    dp[dl + 1] = b'H';
    let hfile = dl + 2;
    let hpath = unsafe { core::str::from_utf8_unchecked(&dp[..hfile]) };
    let hfd = vfs::creat(hpath);
    if hfd == u64::MAX {
        println("app: FS12 deep creat FAILED");
        return;
    }
    let hn = unsafe {
        let buf = core::slice::from_raw_parts_mut(
            core::ptr::addr_of_mut!(BIG_WRITE_BUF) as *mut u8,
            4096,
        );
        buf.fill(0x5A);
        let slice =
            core::slice::from_raw_parts(core::ptr::addr_of!(BIG_WRITE_BUF) as *const u8, 4096);
        vfs::write(hfd, 0, slice)
    };
    vfs::close(hfd);
    if hn != 4096 {
        println("app: FS12 deep write FAILED");
        return;
    }
    let hfd2 = vfs::open(hpath);
    if hfd2 == u64::MAX {
        println("app: FS12 deep reopen FAILED");
        return;
    }
    let hrn = vfs::read(hfd2, 0, 4096);
    vfs::close(hfd2);
    let hgot = unsafe { core::slice::from_raw_parts(vfs::RESULT_BUF as *const u8, 4096) };
    if hrn != 4096 || hgot.iter().any(|&b| b != 0x5A) {
        println("app: FS12 deep read FAILED");
        return;
    }
    // 回收 + 分配扰动: GC 若漏标目录扩展块/索引块, 扰动会覆盖它们, 抽样读回即暴露。
    if vfs::mfs_gc() == u64::MAX {
        println("app: FS12 GC FAILED");
        return;
    }
    let chfd = vfs::creat("/mfs/CHURN4.BIN");
    if chfd == u64::MAX || !fs11_write_pages(chfd, 0, 8, 0xE0) {
        println("app: FS12 churn FAILED");
        return;
    }
    vfs::close(chfd);
    i = 0;
    while i < FS12_FILES {
        if i.is_multiple_of(17) {
            fs12_name(i, &mut pbuf[nbase..nbase + 16]);
            let s = unsafe { core::str::from_utf8_unchecked(&pbuf[..nbase + 16]) };
            let fd = vfs::open(s);
            if fd == u64::MAX {
                println("app: FS12 reopen after GC FAILED");
                return;
            }
            let n = vfs::read(fd, 0, 512);
            vfs::close(fd);
            let got = unsafe { core::slice::from_raw_parts(vfs::RESULT_BUF as *const u8, 512) };
            if n != 512 || got.iter().any(|&b| b != i as u8) {
                println("app: FS12 content after GC FAILED");
                return;
            }
        }
        i += 1;
    }
    // 清理: 深目录自叶向上删, 再删长名文件、200 项与扰动文件。
    if vfs::unlink(hpath) != 1 {
        println("app: FS12 cleanup deep file FAILED");
        return;
    }
    let mut d = dl;
    while d > 8 {
        let s = unsafe { core::str::from_utf8_unchecked(&dp[..d]) };
        if vfs::rmdir(s) != 1 {
            println("app: FS12 cleanup deep rmdir FAILED");
            return;
        }
        d -= 2;
    }
    if vfs::rmdir("/mfs/D12") != 1 {
        println("app: FS12 cleanup D12 FAILED");
        return;
    }
    if vfs::unlink(FS12_LONG) != 1 {
        println("app: FS12 cleanup long name FAILED");
        return;
    }
    i = 0;
    while i < FS12_FILES {
        fs12_name(i, &mut pbuf[nbase..nbase + 16]);
        let s = unsafe { core::str::from_utf8_unchecked(&pbuf[..nbase + 16]) };
        if vfs::unlink(s) != 1 {
            println("app: FS12 cleanup unlink FAILED");
            return;
        }
        i += 1;
    }
    if vfs::rmdir(FS12_DIR) != 1 || vfs::unlink("/mfs/CHURN4.BIN") != 1 {
        println("app: FS12 cleanup FAILED");
        return;
    }
    if vfs::mfs_gc() == u64::MAX {
        println("app: FS12 final GC FAILED");
    }

    // 16. FS-13 自测 (阶段 D/M5): MFS v2 节点元数据 + rename + truncate。
    //     - 时间戳来自 CMOS RTC, 必须落在合理区间 (而不是 0 或垃圾值);
    //     - chmod 改 mode 后经 GC 仍保持 (元数据随 inode 走 COW);
    //     - truncate 截短释放尾部块, 扩展为稀疏 (读回 0);
    //     - rename 跨目录 + 覆盖已存在文件 + 拒绝把目录移进自己的子孙;
    //     - 全程混入 GC 与分配扰动, 验证元数据不影响可达性判定。
    const FS13_DIR: &str = "/mfs/M5";
    const FS13_SUB: &str = "/mfs/M5/SUB";
    const FS13_FILE: &str = "/mfs/M5/A.TXT";
    const FS13_MOVED: &str = "/mfs/M5/SUB/B.TXT";
    const FS13_DST: &str = "/mfs/M5/C.TXT";
    /// 2020-01-01T00:00:00Z —— RTC 只要正常就应大于它。
    const FS13_EPOCH_FLOOR: u64 = 1_577_836_800;

    // 幂等准备: `/mfs` 是持久卷, 上一轮若在自测中途被打断 (或被 kill), 这些对象会留在
    // 卷上, 让下面的 `mkdir` 因「已存在」失败并传染后续所有步骤。先清成干净状态。
    // 顺序: 先摘文件, 再自底向上删目录 (目录非空删不掉)。
    vfs::unlink("/mfs/M5/SUB2/B.TXT");
    vfs::unlink("/mfs/M5/SUB/B.TXT");
    vfs::unlink("/mfs/M5/C.TXT");
    vfs::unlink("/mfs/M5/A.TXT");
    vfs::rmdir("/mfs/M5/SUB2/INNER");
    vfs::rmdir("/mfs/M5/SUB2");
    vfs::rmdir("/mfs/M5/SUB");
    vfs::rmdir("/mfs/M5");

    if vfs::mkdir(FS13_DIR) != 1 || vfs::mkdir(FS13_SUB) != 1 {
        println("app: FS13 mkdir FAILED");
        return;
    }
    let fd13 = vfs::creat(FS13_FILE);
    if fd13 == u64::MAX || !fs11_write_pages(fd13, 0, 3, 0x50) {
        println("app: FS13 write FAILED");
        return;
    }
    vfs::close(fd13);

    // 元数据: 大小 / 属主 / 权限 / 链接数 / 时间戳。
    let st13 = match fs13_stat(FS13_FILE) {
        Some(s) => s,
        None => {
            println("app: FS13 stat FAILED");
            return;
        }
    };
    if st13.size != 3 * 4096 {
        println("app: FS13 size FAILED");
        return;
    }
    if st13.owner != APP_DOMAIN as u16 {
        println("app: FS13 owner FAILED");
        return;
    }
    if fs13_perm(st13.mode) != 0o644 || st13.nlink != 1 || st13.is_dir != 0 {
        println("app: FS13 default meta FAILED");
        return;
    }
    if st13.mtime < FS13_EPOCH_FLOOR || st13.ctime < FS13_EPOCH_FLOOR {
        println("app: FS13 RTC timestamp FAILED");
        return;
    }
    if st13.atime == 0 {
        println("app: FS13 atime FAILED");
        return;
    }

    // chmod 后 mode 必须改; 再经一次 GC 仍保持。
    if vfs::chmod(FS13_FILE, 0o600) != 1 {
        println("app: FS13 chmod FAILED");
        return;
    }
    if fs13_stat(FS13_FILE).map(|s| fs13_perm(s.mode)) != Some(0o600) {
        println("app: FS13 chmod readback FAILED");
        return;
    }
    if vfs::mfs_gc() == u64::MAX {
        println("app: FS13 GC after chmod FAILED");
        return;
    }
    let st13b = match fs13_stat(FS13_FILE) {
        Some(s) => s,
        None => {
            println("app: FS13 stat after GC FAILED");
            return;
        }
    };
    if fs13_perm(st13b.mode) != 0o600 || st13b.ctime < FS13_EPOCH_FLOOR {
        println("app: FS13 meta lost after GC FAILED");
        return;
    }

    // truncate 截短: 12 KiB -> 4 KiB。保留页内容不变, 越过新末尾读到 0 字节。
    let fd13 = vfs::open(FS13_FILE);
    if fd13 == u64::MAX || vfs::truncate(fd13, 4096) != 1 {
        println("app: FS13 truncate down FAILED");
        vfs::close(fd13);
        return;
    }
    if !fs11_verify_pages(fd13, 0, 1, 0x50) {
        println("app: FS13 kept page FAILED");
        vfs::close(fd13);
        return;
    }
    if vfs::read(fd13, 4096, 512) != 0 {
        println("app: FS13 read past new end FAILED");
        vfs::close(fd13);
        return;
    }
    // truncate 扩展: 4 KiB -> 8 KiB, 新区域必须是稀疏的 0。
    if vfs::truncate(fd13, 8192) != 1 {
        println("app: FS13 truncate up FAILED");
        vfs::close(fd13);
        return;
    }
    vfs::close(fd13);
    let st13c = match fs13_stat(FS13_FILE) {
        Some(s) => s,
        None => {
            println("app: FS13 stat after truncate FAILED");
            return;
        }
    };
    if st13c.size != 8192 {
        println("app: FS13 sparse size FAILED");
        return;
    }
    let fd13 = vfs::open(FS13_FILE);
    if fd13 == u64::MAX || !fs13_all_zero(fd13, 4096, 4096) {
        println("app: FS13 sparse zeros FAILED");
        vfs::close(fd13);
        return;
    }
    vfs::close(fd13);
    // 截到 0 再写一页, 给后面的 rename 测试准备可辨识内容。
    let fd13 = vfs::open(FS13_FILE);
    if fd13 == u64::MAX || vfs::truncate(fd13, 0) != 1 || !fs11_write_pages(fd13, 0, 1, 0x60) {
        println("app: FS13 truncate zero FAILED");
        vfs::close(fd13);
        return;
    }
    vfs::close(fd13);

    // rename 跨目录: /mfs/M5/A.TXT -> /mfs/M5/SUB/B.TXT (移动的 inode 保持原 mode)。
    if vfs::rename(FS13_FILE, FS13_MOVED) != 1 {
        println("app: FS13 rename FAILED");
        return;
    }
    if vfs::open(FS13_FILE) != u64::MAX {
        println("app: FS13 old name still there FAILED");
        return;
    }
    let moved = vfs::open(FS13_MOVED);
    if moved == u64::MAX {
        println("app: FS13 rename target missing FAILED");
        return;
    }
    if !fs11_verify_pages(moved, 0, 1, 0x60) {
        println("app: FS13 renamed content FAILED");
        vfs::close(moved);
        return;
    }
    vfs::close(moved);
    if fs13_stat(FS13_MOVED).map(|s| (fs13_perm(s.mode), s.size)) != Some((0o600, 4096)) {
        println("app: FS13 renamed meta FAILED");
        return;
    }

    // rename 目录 + 拒绝把目录移进自己的子孙。
    if vfs::rename(FS13_SUB, "/mfs/M5/SUB2") != 1 {
        println("app: FS13 rename dir FAILED");
        return;
    }
    if vfs::open("/mfs/M5/SUB2/B.TXT") == u64::MAX {
        println("app: FS13 renamed dir child FAILED");
        return;
    }
    if vfs::mkdir("/mfs/M5/SUB2/INNER") != 1 {
        println("app: FS13 mkdir inner FAILED");
        return;
    }
    if vfs::rename("/mfs/M5/SUB2", "/mfs/M5/SUB2/INNER/X") != u64::MAX {
        println("app: FS13 dir-into-own-descendant allowed FAILED");
        return;
    }

    // rename 覆盖已存在文件: 目标内容应变成源的 (0x60), 源名字消失。
    let dstfd = vfs::creat(FS13_DST);
    if dstfd == u64::MAX || !fs11_write_pages(dstfd, 0, 1, 0x70) {
        println("app: FS13 overwrite prep FAILED");
        return;
    }
    vfs::close(dstfd);
    if vfs::rename("/mfs/M5/SUB2/B.TXT", FS13_DST) != 1 {
        println("app: FS13 overwrite rename FAILED");
        return;
    }
    if vfs::open("/mfs/M5/SUB2/B.TXT") != u64::MAX {
        println("app: FS13 overwrite src still there FAILED");
        return;
    }
    let ovw = vfs::open(FS13_DST);
    if ovw == u64::MAX {
        println("app: FS13 overwrite target missing FAILED");
        return;
    }
    let ok_ovw = fs11_verify_pages(ovw, 0, 1, 0x60);
    vfs::close(ovw);
    if !ok_ovw {
        println("app: FS13 overwrite content FAILED");
        return;
    }

    // readdir 长格式所需字段: 目录条目的 mode / mtime 也要有值。
    let dfd13 = vfs::open(FS13_DIR);
    if dfd13 == u64::MAX {
        println("app: FS13 open dir FAILED");
        return;
    }
    let dn13 = vfs::readdir(dfd13);
    vfs::close(dfd13);
    if dn13 == u64::MAX {
        println("app: FS13 readdir FAILED");
        return;
    }
    let dent13 = dn13 as usize / core::mem::size_of::<vfs::DirEntry>();
    let dlist13 =
        unsafe { core::slice::from_raw_parts(vfs::RESULT_BUF as *const vfs::DirEntry, dent13) };
    if dent13 != 2
        || !dlist13.iter().all(|de| {
            fs13_perm(de.mode) != 0 && de.mtime >= FS13_EPOCH_FLOOR && de.owner == APP_DOMAIN as u16
        })
    {
        println("app: FS13 readdir meta FAILED");
        return;
    }

    // GC + 分配扰动后重读: 元数据与内容都不受影响。
    if vfs::mfs_gc() == u64::MAX {
        println("app: FS13 GC FAILED");
        return;
    }
    let chfd13 = vfs::creat("/mfs/CHURN5.BIN");
    if chfd13 == u64::MAX || !fs11_write_pages(chfd13, 0, 8, 0xA0) {
        println("app: FS13 churn FAILED");
        return;
    }
    vfs::close(chfd13);
    let after = vfs::open(FS13_DST);
    if after == u64::MAX {
        println("app: FS13 reopen after GC FAILED");
        return;
    }
    let ok_after = fs11_verify_pages(after, 0, 1, 0x60);
    vfs::close(after);
    if !ok_after || fs13_stat(FS13_DST).map(|s| fs13_perm(s.mode)) != Some(0o600) {
        println("app: FS13 content after GC FAILED");
        return;
    }

    // 清理: 自叶向上删目录, 再删文件与扰动文件, 最后回收。
    if vfs::rmdir("/mfs/M5/SUB2/INNER") != 1
        || vfs::rmdir("/mfs/M5/SUB2") != 1
        || vfs::unlink(FS13_DST) != 1
        || vfs::rmdir(FS13_DIR) != 1
        || vfs::unlink("/mfs/CHURN5.BIN") != 1
    {
        println("app: FS13 cleanup FAILED");
        return;
    }
    if vfs::mfs_gc() == u64::MAX {
        println("app: FS13 final GC FAILED");
    }

    // 17. FS-14 自测 (阶段 D/M5b): inode 号间接层 + 硬链接。
    //     核心断言只有一条: **从任一个名字改写文件, 另一个名字立刻看到新内容** ——
    //     旧实现 (目录项直接存块号) 在这一步会把两个名字的内容写分叉。
    //     另外覆盖 nlink 计数、摘掉一个名字后数据仍在、目录不可链接、GC 后一致性。
    const FS14_A: &str = "/mfs/L1.TXT";
    const FS14_B: &str = "/mfs/L2.TXT";
    const FS14_C: &str = "/mfs/L3.TXT";
    const FS14_D: &str = "/mfs/L4.TXT";
    const FS14_DIR: &str = "/mfs/DIR14";

    let fd = vfs::creat(FS14_A);
    if fd == u64::MAX || !fs11_write_pages(fd, 0, 1, 0x30) {
        println("app: FS14 create FAILED");
        return;
    }
    vfs::close(fd);
    // 建第二个名字。
    if vfs::link(FS14_A, FS14_B) != 1 {
        println("app: FS14 link FAILED");
        return;
    }
    // 两个名字必须看到同一份元数据 (同一个 inode)。
    let sa = match fs13_stat(FS14_A) {
        Some(s) => s,
        None => {
            println("app: FS14 stat A FAILED");
            return;
        }
    };
    let sb = match fs13_stat(FS14_B) {
        Some(s) => s,
        None => {
            println("app: FS14 stat B FAILED");
            return;
        }
    };
    if sa.nlink != 2 || sb.nlink != 2 || sa.size != 4096 || fs13_perm(sa.mode) != 0o644 {
        println("app: FS14 nlink FAILED");
        return;
    }
    if (sa.mtime, sa.owner) != (sb.mtime, sb.owner) {
        println("app: FS14 shared inode FAILED");
        return;
    }
    // 从 L2 改写, L1 必须看到新内容 (硬链接的关键语义)。
    let fd = vfs::open(FS14_B);
    if fd == u64::MAX || !fs11_write_pages(fd, 0, 1, 0x40) {
        println("app: FS14 write via B FAILED");
        vfs::close(fd);
        return;
    }
    vfs::close(fd);
    let fd = vfs::open(FS14_A);
    if fd == u64::MAX {
        println("app: FS14 reopen A FAILED");
        return;
    }
    let ok = fs11_verify_pages(fd, 0, 1, 0x40);
    vfs::close(fd);
    if !ok {
        println("app: FS14 write-through-link FAILED");
        return;
    }
    // GC + 分配扰动后两个名字仍指向同一份数据。
    if vfs::mfs_gc() == u64::MAX {
        println("app: FS14 GC FAILED");
        return;
    }
    let churn = vfs::creat("/mfs/CHURN6.BIN");
    if churn == u64::MAX || !fs11_write_pages(churn, 0, 8, 0xB0) {
        println("app: FS14 churn FAILED");
        return;
    }
    vfs::close(churn);
    let fd = vfs::open(FS14_B);
    if fd == u64::MAX {
        println("app: FS14 reopen B after GC FAILED");
        return;
    }
    let ok = fs11_verify_pages(fd, 0, 1, 0x40);
    vfs::close(fd);
    if !ok || fs13_stat(FS14_A).map(|s| s.nlink) != Some(2) {
        println("app: FS14 after GC FAILED");
        return;
    }
    // 摘掉一个名字: 数据必须还在 (nlink 减到 1)。
    if vfs::unlink(FS14_B) != 1 {
        println("app: FS14 unlink B FAILED");
        return;
    }
    if vfs::open(FS14_B) != u64::MAX {
        println("app: FS14 B still there FAILED");
        return;
    }
    let fd = vfs::open(FS14_A);
    if fd == u64::MAX {
        println("app: FS14 A lost after unlink B FAILED");
        return;
    }
    let ok = fs11_verify_pages(fd, 0, 1, 0x40);
    vfs::close(fd);
    if !ok || fs13_stat(FS14_A).map(|s| s.nlink) != Some(1) {
        println("app: FS14 survive unlink FAILED");
        return;
    }
    // 再链一次, 这次摘掉"原来的名字", 剩余名字仍然可用; 顺带验证改名后仍可读。
    if vfs::link(FS14_A, FS14_C) != 1 || vfs::unlink(FS14_A) != 1 {
        println("app: FS14 relink FAILED");
        return;
    }
    if vfs::rename(FS14_C, FS14_D) != 1 {
        println("app: FS14 rename linked FAILED");
        return;
    }
    let fd = vfs::open(FS14_D);
    if fd == u64::MAX {
        println("app: FS14 D missing FAILED");
        return;
    }
    let ok = fs11_verify_pages(fd, 0, 1, 0x40);
    vfs::close(fd);
    if !ok {
        println("app: FS14 content after rename FAILED");
        return;
    }
    // 负向: 目录不能硬链接; 目标已存在 / 源不存在都必须失败。
    if vfs::mkdir(FS14_DIR) != 1 {
        println("app: FS14 mkdir FAILED");
        return;
    }
    if vfs::link(FS14_DIR, "/mfs/DIR14B") != u64::MAX {
        println("app: FS14 dir link allowed FAILED");
        return;
    }
    if vfs::link(FS14_D, FS14_D) != u64::MAX {
        println("app: FS14 link onto existing allowed FAILED");
        return;
    }
    if vfs::link("/mfs/NOPE14", "/mfs/NOPE14B") != u64::MAX {
        println("app: FS14 link missing src allowed FAILED");
        return;
    }
    // 清理: 最后一个名字摘掉后 inode 槽释放, 块由 GC 回收。
    if vfs::unlink(FS14_D) != 1 || vfs::rmdir(FS14_DIR) != 1 || vfs::unlink("/mfs/CHURN6.BIN") != 1
    {
        println("app: FS14 cleanup FAILED");
        return;
    }
    if vfs::mfs_gc() == u64::MAX {
        println("app: FS14 final GC FAILED");
    }

    // 18. FS-15 自测 (阶段 D/M6a): exFAT 只读兼容。
    //     宿主 `mkfs.exfat` 预格式化的卷 (nsid 5) 自动挂载于 /usb, 服务必须完成:
    //     引导扇区 + boot checksum 校验 → 系统项 (0x81/0x82) 解析 → 位图/upcase 载入。
    //     这里只要求列举成功 (系统项 / 卷标不会被当成文件) 与类型正确;
    //     「卷已清空」的强校验放在 FS-16 收尾 (那里刚删掉自己创建的对象)。
    let efd15 = vfs::open("/usb");
    if efd15 == u64::MAX {
        println("app: FS15 open /usb FAILED");
        return;
    }
    let rn15 = vfs::readdir(efd15);
    vfs::close(efd15);
    if rn15 == u64::MAX {
        println("app: FS15 readdir /usb FAILED");
        return;
    }
    if !(rn15 as usize).is_multiple_of(core::mem::size_of::<vfs::DirEntry>()) {
        println("app: FS15 listing not whole entries FAILED");
        return;
    }
    {
        if vfs::stat("/usb") == u64::MAX {
            println("app: FS15 stat /usb FAILED");
            return;
        }
        let st = unsafe { core::ptr::read_unaligned(vfs::RESULT_BUF as *const vfs::Stat) };
        if st.is_dir != 1 {
            println("app: FS15 /usb not dir FAILED");
            return;
        }
    }
    if vfs::open("/usb/NOPE.TXT") != u64::MAX {
        println("app: FS15 missing path NOT rejected FAILED");
    }

    // 19. FS-16 自测 (阶段 D/M6b): exFAT 读写。
    //     覆盖 creat/write/读回/stat/mkdir/readdir/rmdir(非空拒绝)/truncate(缩+扩)/unlink
    //     全链路, 并在末尾把卷清空 —— 下一次启动的 FS-15 因此仍看到空卷。
    //     写入 100000 字节: 4 KiB 簇下跨 25 簇、32 KiB 簇下跨 4 簇, 都能验证簇链扩展;
    //     建 45 个文件以验证目录块扩容 (4 KiB 簇下 135 条目 > 单簇 128 项)。
    {
        // 幂等: 先清掉上次可能残留的自测对象。
        vfs::unlink("/usb/D16/A.TXT");
        vfs::rmdir("/usb/D16");
        vfs::unlink("/usb/FS16.TXT");
        let pat = |i: usize| (i % 251) as u8;

        // --- 写 100000 字节 ---
        let fd = vfs::creat("/usb/FS16.TXT");
        if fd == u64::MAX {
            println("app: FS16 creat FAILED");
            return;
        }
        let mut buf = [0u8; 4096];
        let total = 100_000usize;
        let mut written = 0usize;
        while written < total {
            let n = (total - written).min(buf.len());
            let mut i = 0usize;
            while i < n {
                buf[i] = pat(written + i);
                i += 1;
            }
            if vfs::write(fd, written as u64, &buf[..n]) != n as u64 {
                println("app: FS16 write FAILED");
                return;
            }
            written += n;
        }
        vfs::close(fd);
        if vfs::stat("/usb/FS16.TXT") == u64::MAX {
            println("app: FS16 stat FAILED");
            return;
        }
        let st = unsafe { core::ptr::read_unaligned(vfs::RESULT_BUF as *const vfs::Stat) };
        if st.size as usize != total {
            println("app: FS16 size mismatch FAILED");
            return;
        }
        let rfd = vfs::open("/usb/FS16.TXT");
        if rfd == u64::MAX {
            println("app: FS16 reopen FAILED");
            return;
        }
        let mut off = 0usize;
        while off < total {
            let n = (total - off).min(4096);
            if vfs::read(rfd, off as u64, n as u32) != n as u64 {
                println("app: FS16 read back FAILED");
                return;
            }
            let r = unsafe { core::slice::from_raw_parts(vfs::RESULT_BUF as *const u8, n) };
            let mut i = 0usize;
            while i < n {
                if r[i] != pat(off + i) {
                    println("app: FS16 content mismatch FAILED");
                    return;
                }
                i += 1;
            }
            off += n;
        }

        // --- 截短到 1000 (保留首簇前缀, 释放第二簇) ---
        if vfs::truncate(rfd, 1000) == u64::MAX {
            println("app: FS16 truncate down FAILED");
            return;
        }
        if vfs::read(rfd, 900, 200) != 100 {
            println("app: FS16 read after shrink FAILED");
            return;
        }
        {
            let r = unsafe { core::slice::from_raw_parts(vfs::RESULT_BUF as *const u8, 100) };
            let mut i = 0usize;
            while i < 100 {
                if r[i] != pat(900 + i) {
                    println("app: FS16 shrink kept prefix FAILED");
                    return;
                }
                i += 1;
            }
        }
        // --- 扩展回 100000 (新区必须读到 0, exFAT 无稀疏) ---
        if vfs::truncate(rfd, 100_000) == u64::MAX {
            println("app: FS16 truncate up FAILED");
            return;
        }
        if vfs::read(rfd, 1000, 200) != 200 {
            println("app: FS16 read after grow FAILED");
            return;
        }
        {
            let r = unsafe { core::slice::from_raw_parts(vfs::RESULT_BUF as *const u8, 200) };
            let mut i = 0usize;
            while i < 200 {
                if r[i] != 0 {
                    println("app: FS16 grow not zero FAILED");
                    return;
                }
                i += 1;
            }
        }
        vfs::close(rfd);

        // --- 目录: 建 / 列举 / 非空 rmdir 被拒 / unlink 目录被拒 ---
        if vfs::mkdir("/usb/D16") == u64::MAX {
            println("app: FS16 mkdir FAILED");
            return;
        }
        let dfd = vfs::open("/usb/D16");
        if dfd == u64::MAX {
            println("app: FS16 open dir FAILED");
            return;
        }
        if vfs::readdir(dfd) != 0 {
            println("app: FS16 new dir not empty FAILED");
            return;
        }
        vfs::close(dfd);
        let afd = vfs::creat("/usb/D16/A.TXT");
        if afd == u64::MAX || vfs::write(afd, 0, b"hi") != 2 {
            println("app: FS16 write in dir FAILED");
            return;
        }
        vfs::close(afd);
        let rfd2 = vfs::open("/usb/D16/A.TXT");
        if rfd2 == u64::MAX || vfs::read(rfd2, 0, 16) != 2 {
            println("app: FS16 read in dir FAILED");
            return;
        }
        vfs::close(rfd2);
        if vfs::rmdir("/usb/D16") != u64::MAX {
            println("app: FS16 rmdir non-empty NOT rejected FAILED");
            return;
        }
        if vfs::unlink("/usb/D16") != u64::MAX {
            println("app: FS16 unlink dir NOT rejected FAILED");
            return;
        }

        // --- 目录扩容: 45 个 5 字符名 → 135 个条目 > 单簇 128 项 ---
        let mut k = 0usize;
        while k < 45 {
            let (a, b) = (b'0' + (k / 10) as u8, b'0' + (k % 10) as u8);
            let mut nm = [0u8; 16];
            nm[..5].copy_from_slice(b"/usb/");
            nm[5] = b'X';
            nm[6] = a;
            nm[7] = b;
            nm[8] = b'.';
            nm[9] = b'T';
            nm[10] = b'X';
            nm[11] = b'T';
            let p = unsafe { core::str::from_utf8_unchecked(&nm[..12]) };
            let f = vfs::creat(p);
            if f == u64::MAX {
                println("app: FS16 bulk creat FAILED");
                return;
            }
            vfs::close(f);
            k += 1;
        }
        let dfd2 = vfs::open("/usb");
        let n2 = vfs::readdir(dfd2);
        vfs::close(dfd2);
        // 45 个文件 + 1 个目录远超一页条目上限, 必须正好写满结果页。
        if n2 == u64::MAX
            || n2 as usize / core::mem::size_of::<vfs::DirEntry>() != vfs::RESULT_MAX_ENTRIES
        {
            println("app: FS16 bulk listing FAILED");
            return;
        }

        // --- 清理 (先摘名字再核对空卷) ---
        if vfs::unlink("/usb/D16/A.TXT") == u64::MAX || vfs::rmdir("/usb/D16") == u64::MAX {
            println("app: FS16 cleanup dir FAILED");
            return;
        }
        let mut k = 0usize;
        while k < 45 {
            let (a, b) = (b'0' + (k / 10) as u8, b'0' + (k % 10) as u8);
            let mut nm = [0u8; 16];
            nm[..5].copy_from_slice(b"/usb/");
            nm[5] = b'X';
            nm[6] = a;
            nm[7] = b;
            nm[8] = b'.';
            nm[9] = b'T';
            nm[10] = b'X';
            nm[11] = b'T';
            let p = unsafe { core::str::from_utf8_unchecked(&nm[..12]) };
            if vfs::unlink(p) == u64::MAX {
                println("app: FS16 bulk unlink FAILED");
                return;
            }
            k += 1;
        }
        if vfs::unlink("/usb/FS16.TXT") == u64::MAX {
            println("app: FS16 cleanup file FAILED");
            return;
        }
        let efd = vfs::open("/usb");
        let left = vfs::readdir(efd);
        vfs::close(efd);
        if left != 0 {
            println("app: FS16 cleanup left entries FAILED");
            return;
        }
    }

    // 20. FS-17 自测 (阶段 D/M1b): **额外卷**自动挂载。
    //     分区测试盘 (nsid 4) 上的 FAT32 / ext2 分区都不是「第一个匹配卷」, 卷层按
    //     M1b 把它们作为额外卷挂到 `/usb<卷号>`, 由同一个文件服务**按卷切换几何**来
    //     服务。这里验证:
    //       - 卷表里能查到这两个分区的卷号, 并据此拼出挂载点;
    //       - FAT32 分区可打开目录, 且能读出宿主预置的 PART1.TXT;
    //       - ext2 分区可读出 PART2.TXT;
    //     全程只读: 不向额外卷写任何数据。
    {
        let nvol = block_list_volumes(vfs::RESULT_BUF as *mut u8, 16);
        if nvol == u64::MAX {
            println("app: FS17 list volumes FAILED");
            return;
        }
        let mut fat_vol = u64::MAX;
        let mut ext_vol = u64::MAX;
        let mut i = 0u64;
        while i < nvol {
            let d = vol_desc(vfs::RESULT_BUF as *const u8, i as usize);
            if d.nsid == 4 && d.start_lba == 2048 && d.kind == VOL_KIND_FAT {
                fat_vol = d.id as u64;
            }
            if d.nsid == 4 && d.start_lba == 34816 && d.kind == VOL_KIND_EXT2 {
                ext_vol = d.id as u64;
            }
            i += 1;
        }
        if fat_vol == u64::MAX || ext_vol == u64::MAX {
            println("app: FS17 partition volumes missing FAILED");
            return;
        }

        // 额外卷的挂载点 = `/usb<卷号>` (与 mount_srv 的命名规则一致)。
        let mut base = [0u8; 8];
        base[..4].copy_from_slice(b"/usb");
        let mut path = [0u8; 32];
        path[..4].copy_from_slice(b"/usb");

        // --- FAT32 分区: 目录可打开, 且能读出宿主 mcopy 预置的 PART1.TXT ---
        let dn = dec_to_str(fat_vol, &mut base[4..]);
        let root = unsafe { core::str::from_utf8_unchecked(&base[..4 + dn]) };
        let dfd = vfs::open(root);
        if dfd == u64::MAX {
            println("app: FS17 open extra FAT volume FAILED");
            return;
        }
        vfs::close(dfd);
        path[4..4 + dn].copy_from_slice(&base[4..4 + dn]);
        path[4 + dn..4 + dn + 10].copy_from_slice(b"/PART1.TXT");
        let fpath = unsafe { core::str::from_utf8_unchecked(&path[..4 + dn + 10]) };
        let f = vfs::open(fpath);
        if f == u64::MAX {
            println("app: FS17 open PART1.TXT on extra FAT volume FAILED");
            return;
        }
        let n = vfs::read(f, 0, 4096);
        vfs::close(f);
        if n == u64::MAX || n < 10 {
            println("app: FS17 read PART1.TXT FAILED");
            return;
        }
        {
            let head = unsafe { core::slice::from_raw_parts(vfs::RESULT_BUF as *const u8, 10) };
            if head != &b"partition "[..] {
                println("app: FS17 PART1.TXT content FAILED");
                return;
            }
        }

        // --- ext2 分区: 同样读一个宿主预置的文件 ---
        let dn2 = dec_to_str(ext_vol, &mut base[4..]);
        path = [0u8; 32];
        path[..4].copy_from_slice(b"/usb");
        path[4..4 + dn2].copy_from_slice(&base[4..4 + dn2]);
        path[4 + dn2..4 + dn2 + 10].copy_from_slice(b"/PART2.TXT");
        let epath = unsafe { core::str::from_utf8_unchecked(&path[..4 + dn2 + 10]) };
        let e = vfs::open(epath);
        if e == u64::MAX {
            println("app: FS17 open PART2.TXT on extra ext2 volume FAILED");
            return;
        }
        let en = vfs::read(e, 0, 4096);
        vfs::close(e);
        if en == u64::MAX || en < 10 {
            println("app: FS17 read PART2.TXT FAILED");
            return;
        }
        {
            let head = unsafe { core::slice::from_raw_parts(vfs::RESULT_BUF as *const u8, 10) };
            if head != &b"partition "[..] {
                println("app: FS17 PART2.TXT content FAILED");
                return;
            }
        }
    }

    // 21. FS-18 自测: fat32 大簇写路径 (跨簇写入 + 读回 + 删除释放)。
    //     fat32 没有 truncate, 故重点覆盖 WRITE 触发簇链扩展、逐簇读回、UNLINK 释放
    //     簇链这三条「目录项增删 / 簇分配 / FAT 链维护」的写路径。写入 100000 字节:
    //     512 B 簇下跨 196 簇、32 KiB 簇下跨 4 簇 —— 无论哪种几何都会真实跨簇。
    //     这一条是 fat32 写路径里**唯一**的大文件用例, 且能在 `NVME_CLU=64` 造出的
    //     32 KiB 簇镜像上验证 M1b 大簇写路径。
    {
        // 幂等: 清掉上一轮被中断可能残留的对象。
        vfs::unlink("/FS18.BIN");
        let pat = |i: usize| (i % 251) as u8;
        let total = 100_000usize;

        let fd = vfs::creat("/FS18.BIN");
        if fd == u64::MAX {
            println("app: FS18 creat FAILED");
            return;
        }
        let mut buf = [0u8; 4096];
        let mut written = 0usize;
        while written < total {
            let n = (total - written).min(buf.len());
            let mut i = 0usize;
            while i < n {
                buf[i] = pat(written + i);
                i += 1;
            }
            if vfs::write(fd, written as u64, &buf[..n]) != n as u64 {
                println("app: FS18 write FAILED");
                return;
            }
            written += n;
        }
        vfs::close(fd);

        if vfs::stat("/FS18.BIN") == u64::MAX {
            println("app: FS18 stat FAILED");
            return;
        }
        {
            let st = unsafe { core::ptr::read_unaligned(vfs::RESULT_BUF as *const vfs::Stat) };
            if st.size as usize != total {
                println("app: FS18 size mismatch FAILED");
                return;
            }
        }

        let rfd = vfs::open("/FS18.BIN");
        if rfd == u64::MAX {
            println("app: FS18 reopen FAILED");
            return;
        }
        let mut off = 0usize;
        while off < total {
            let n = (total - off).min(4096);
            if vfs::read(rfd, off as u64, n as u32) != n as u64 {
                println("app: FS18 read back FAILED");
                return;
            }
            let r = unsafe { core::slice::from_raw_parts(vfs::RESULT_BUF as *const u8, n) };
            let mut i = 0usize;
            while i < n {
                if r[i] != pat(off + i) {
                    println("app: FS18 content mismatch FAILED");
                    return;
                }
                i += 1;
            }
            off += n;
        }
        vfs::close(rfd);

        if vfs::unlink("/FS18.BIN") != 1 {
            println("app: FS18 unlink FAILED");
            return;
        }
        if vfs::stat("/FS18.BIN") != u64::MAX {
            println("app: FS18 still present after unlink FAILED");
            return;
        }
    }

    // 22. FS-19 自测 (阶段 D/M5c): 软链接 —— 新节点类型 MFSL + 解析跟随 + 限深防环。
    //     全在 `/mfs` 下做 (软链接只有 MFS 支持)。三类断言:
    //       a) 跟随: 读链接等于读目标 (绝对目标 / 同目录相对目标 / 带 `..` 的相对目标 /
    //          路径**中间**分量是目录链接); `stat` 链接跟随到目标类型 (不是 "symbolic
    //          link"); readdir 里它才是链接 (mode 高位 = LINK, size = 目标串长度)。
    //       b) 不跟随: `rm`/`mv`/`rmdir` 作用于链接自身 —— 摘掉链接后目标内容必须还在,
    //          且 `rmdir <指向目录的链接>` 必须失败 (它不是目录条目)。
    //       c) 防环: 互相指向 / 自指的链接在解析时报错, 不挂死、不无限展开。
    {
        const F19_T: &str = "/mfs/S19T.TXT";
        const F19_D: &str = "/mfs/D19";
        const F19_F: &str = "/mfs/D19/F.TXT";
        // 幂等: /mfs 是持久卷, 清掉上一轮被中断可能残留的对象 (软链接用 unlink 摘)。
        // 顺序: 先摘链接再删目录, 否则目录非空删不掉。
        for p in [
            "/mfs/S19L1",
            "/mfs/S19L2",
            "/mfs/S19L2R",
            "/mfs/D19/S19L3",
            "/mfs/S19LD",
            "/mfs/S19L5",
            "/mfs/C19A",
            "/mfs/C19B",
            "/mfs/C19C",
        ] {
            vfs::unlink(p);
        }
        vfs::unlink(F19_F);
        vfs::rmdir(F19_D);
        vfs::unlink(F19_T);

        // 目标文件 (1 页, 内容标记 0x50)。
        let fd = vfs::creat(F19_T);
        if fd == u64::MAX || !fs11_write_pages(fd, 0, 1, 0x50) {
            println("app: FS19 create target FAILED");
            return;
        }
        vfs::close(fd);

        // (a1) 绝对目标: 读链接 == 读目标。
        if vfs::symlink("/mfs/S19T.TXT", "/mfs/S19L1") != 1 {
            println("app: FS19 symlink abs FAILED");
            return;
        }
        let lfd = vfs::open("/mfs/S19L1");
        if lfd == u64::MAX {
            println("app: FS19 open through abs link FAILED");
            return;
        }
        let ok = fs11_verify_pages(lfd, 0, 1, 0x50);
        vfs::close(lfd);
        if !ok {
            println("app: FS19 read through abs link FAILED");
            return;
        }

        // (a2) stat 跟随: 报的是**目标**的类型与大小, 不是链接。
        match fs13_stat("/mfs/S19L1") {
            Some(st)
                if st.size == 4096
                    && st.is_dir == 0
                    && st.mode & vfs::MODE_FTYPE_MASK == vfs::MODE_FTYPE_FILE => {}
            _ => {
                println("app: FS19 stat through link FAILED");
                return;
            }
        }

        // (a3) readdir 看到的是**链接本身**: 类型位 = LINK, size = 目标串长度。
        //      存下的是服务命名空间里的目标: 客户端已把挂载前缀 `/mfs` 剥掉,
        //      "/mfs/S19T.TXT" -> "/S19T.TXT" (9 字节)。
        match fs19_entry("/mfs", "S19L1") {
            Some(de)
                if de.mode & vfs::MODE_FTYPE_MASK == vfs::MODE_FTYPE_LINK
                    && de.is_dir == 0
                    && de.size == 9 => {}
            _ => {
                println("app: FS19 readdir link entry FAILED");
                return;
            }
        }

        // (a4) 同目录相对目标 ("S19T.TXT" 相对链接所在目录 /mfs)。
        if vfs::symlink("S19T.TXT", "/mfs/S19L2") != 1 {
            println("app: FS19 symlink rel FAILED");
            return;
        }
        let lfd = vfs::open("/mfs/S19L2");
        if lfd == u64::MAX {
            println("app: FS19 open through rel link FAILED");
            return;
        }
        let ok = fs11_verify_pages(lfd, 0, 1, 0x50);
        vfs::close(lfd);
        if !ok {
            println("app: FS19 read through rel link FAILED");
            return;
        }

        // (a5) 带 `..` 的相对目标: 链接在 /mfs/D19 里, 目标 "../D19/F.TXT"。
        //      相对基准是**链接所在目录** (/mfs/D19), 展开后为 /mfs/D19/../D19/F.TXT,
        //      必须重新规范化成 /mfs/D19/F.TXT 才找得到。
        if vfs::mkdir(F19_D) != 1 {
            println("app: FS19 mkdir D19 FAILED");
            return;
        }
        let fd = vfs::creat(F19_F);
        if fd == u64::MAX || !fs11_write_pages(fd, 0, 1, 0x60) {
            println("app: FS19 create D19/F FAILED");
            return;
        }
        vfs::close(fd);
        if vfs::symlink("../D19/F.TXT", "/mfs/D19/S19L3") != 1 {
            println("app: FS19 symlink dotdot FAILED");
            return;
        }
        let lfd = vfs::open("/mfs/D19/S19L3");
        if lfd == u64::MAX {
            println("app: FS19 open through dotdot link FAILED");
            return;
        }
        let ok = fs11_verify_pages(lfd, 0, 1, 0x60);
        vfs::close(lfd);
        if !ok {
            println("app: FS19 dotdot link target FAILED");
            return;
        }

        // (a6) **中间分量**是目录链接: /mfs/S19LD -> /mfs/D19, 打开 /mfs/S19LD/F.TXT。
        if vfs::symlink("/mfs/D19", "/mfs/S19LD") != 1 {
            println("app: FS19 symlink dir FAILED");
            return;
        }
        let lfd = vfs::open("/mfs/S19LD/F.TXT");
        if lfd == u64::MAX {
            println("app: FS19 open via dir link FAILED");
            return;
        }
        let ok = fs11_verify_pages(lfd, 0, 1, 0x60);
        vfs::close(lfd);
        if !ok {
            println("app: FS19 dir-link traversal FAILED");
            return;
        }

        // (b1) rmdir 一个**指向目录的链接**必须失败 (它不是目录条目, 不能跟随)。
        if vfs::rmdir("/mfs/S19LD") != u64::MAX {
            println("app: FS19 rmdir dir-link should FAIL");
            return;
        }
        // (b2) rm 摘掉链接; 目标目录与其中的文件都不受影响。
        if vfs::unlink("/mfs/S19LD") != 1 {
            println("app: FS19 unlink dir-link FAILED");
            return;
        }
        if vfs::stat("/mfs/D19/F.TXT") == u64::MAX {
            println("app: FS19 target removed by link unlink FAILED");
            return;
        }

        // (b3) 摘掉链接后, 目标文件内容必须原封不动。
        if vfs::unlink("/mfs/S19L1") != 1 {
            println("app: FS19 unlink link FAILED");
            return;
        }
        let tfd = vfs::open(F19_T);
        if tfd == u64::MAX {
            println("app: FS19 target gone after unlink FAILED");
            return;
        }
        let ok = fs11_verify_pages(tfd, 0, 1, 0x50);
        vfs::close(tfd);
        if !ok {
            println("app: FS19 unlink touched target FAILED");
            return;
        }
        // 链接已不存在: 经它打开必须失败。
        if vfs::open("/mfs/S19L1") != u64::MAX {
            println("app: FS19 link still resolvable FAILED");
            return;
        }

        // (b4) mv 移动的是链接自身 (不跟随): 改名后仍是链接, 且仍能读到目标。
        if vfs::rename("/mfs/S19L2", "/mfs/S19L2R") != 1 {
            println("app: FS19 rename link FAILED");
            return;
        }
        match fs19_entry("/mfs", "S19L2R") {
            Some(de) if de.mode & vfs::MODE_FTYPE_MASK == vfs::MODE_FTYPE_LINK => {}
            _ => {
                println("app: FS19 link type lost after rename FAILED");
                return;
            }
        }
        let lfd = vfs::open("/mfs/S19L2R");
        if lfd == u64::MAX {
            println("app: FS19 open renamed link FAILED");
            return;
        }
        let ok = fs11_verify_pages(lfd, 0, 1, 0x50);
        vfs::close(lfd);
        if !ok {
            println("app: FS19 renamed link target FAILED");
            return;
        }

        // (c1) 悬空链接: 建得出来, 但解析 (open) 失败; 条目本身仍在 (不能再建同名)。
        if vfs::symlink("/mfs/S19NOPE.TXT", "/mfs/S19L5") != 1 {
            println("app: FS19 symlink dangling FAILED");
            return;
        }
        if vfs::open("/mfs/S19L5") != u64::MAX {
            println("app: FS19 dangling link should FAIL to open");
            return;
        }
        if vfs::mkdir("/mfs/S19L5") != u64::MAX {
            println("app: FS19 dangling link name should be taken");
            return;
        }
        if vfs::unlink("/mfs/S19L5") != 1 {
            println("app: FS19 unlink dangling link FAILED");
            return;
        }

        // (c2) 互相指向 C19A <-> C19B: 解析必须失败 (限深), 且不能挂死。
        if vfs::symlink("/mfs/C19B", "/mfs/C19A") != 1
            || vfs::symlink("/mfs/C19A", "/mfs/C19B") != 1
        {
            println("app: FS19 symlink cycle setup FAILED");
            return;
        }
        if vfs::open("/mfs/C19A") != u64::MAX || vfs::open("/mfs/C19B") != u64::MAX {
            println("app: FS19 link cycle should FAIL to resolve");
            return;
        }
        // (c3) 自指链接。
        if vfs::symlink("/mfs/C19C", "/mfs/C19C") != 1 {
            println("app: FS19 symlink self FAILED");
            return;
        }
        if vfs::open("/mfs/C19C") != u64::MAX {
            println("app: FS19 self link should FAIL to resolve");
            return;
        }

        // 清理 (幂等准备里同样的清单)。
        vfs::unlink("/mfs/S19L2R");
        vfs::unlink("/mfs/D19/S19L3");
        vfs::unlink("/mfs/C19A");
        vfs::unlink("/mfs/C19B");
        vfs::unlink("/mfs/C19C");
        vfs::unlink(F19_F);
        vfs::rmdir(F19_D);
        vfs::unlink(F19_T);
    }

    // 23. FS-20 自测 (阶段 D/M5c 配套): `readlink` + `lstat`。
    //     补上 M5c 落地时留下的两个缺口: 界面看不到链接指向哪里 (只有 readlink 能看),
    //     以及悬空链接根本无法 stat (`stat` 一律跟随)。
    {
        const F20_T: &str = "/mfs/S20T.TXT";
        const F20_L1: &str = "/mfs/S20L1";
        const F20_L2: &str = "/mfs/S20L2";
        const F20_L3: &str = "/mfs/S20L3";
        const F20_L5: &str = "/mfs/S20L5";
        // 幂等准备。
        for p in [F20_L1, F20_L2, F20_L3, F20_L5, "/mfs/S20L4"] {
            vfs::unlink(p);
        }
        vfs::unlink(F20_T);

        let fd = vfs::creat(F20_T);
        if fd == u64::MAX || !fs11_write_pages(fd, 0, 1, 0x70) {
            println("app: FS20 create target FAILED");
            return;
        }
        vfs::close(fd);

        // (a) readlink 与 `ln -s` 的输入**逐字节往返**: 绝对目标原样回来
        //     (服务端存的是剥掉挂载前缀的 `/S20T.TXT`, 客户端把前缀加回去)。
        if vfs::symlink("/mfs/S20T.TXT", F20_L1) != 1 {
            println("app: FS20 symlink abs FAILED");
            return;
        }
        if !fs20_readlink_is(F20_L1, "/mfs/S20T.TXT") {
            println("app: FS20 readlink abs FAILED");
            return;
        }
        // (b) 相对目标**不加**前缀 (它相对链接所在目录, 与服务命名空间无关)。
        if vfs::symlink("S20T.TXT", F20_L2) != 1 {
            println("app: FS20 symlink rel FAILED");
            return;
        }
        if !fs20_readlink_is(F20_L2, "S20T.TXT") {
            println("app: FS20 readlink rel FAILED");
            return;
        }
        // (c) 带 `..` 的相对目标同样原样返回。
        if vfs::symlink("../S20T.TXT", F20_L5) != 1 {
            println("app: FS20 symlink dotdot FAILED");
            return;
        }
        if !fs20_readlink_is(F20_L5, "../S20T.TXT") {
            println("app: FS20 readlink dotdot FAILED");
            return;
        }

        // (d) lstat 看**链接自身**: 类型位 = LINK, size = 目标串长度 (存的是 `/S20T.TXT`
        //     —— 9 字节, 挂载前缀已剥掉); 同一路径的 stat 则跟随到目标 (普通文件 / 4096)。
        match fs20_lstat(F20_L1) {
            Some(st)
                if st.mode & vfs::MODE_FTYPE_MASK == vfs::MODE_FTYPE_LINK
                    && st.is_dir == 0
                    && st.size == 9 => {}
            _ => {
                println("app: FS20 lstat link FAILED");
                return;
            }
        }
        match fs13_stat(F20_L1) {
            Some(st)
                if st.mode & vfs::MODE_FTYPE_MASK == vfs::MODE_FTYPE_FILE && st.size == 4096 => {}
            _ => {
                println("app: FS20 stat-through-link FAILED");
                return;
            }
        }

        // (e) 悬空链接: `stat` 必然失败 (跟随不到), 而 `lstat` / `readlink` 都正常 ——
        //     这正是 `lstat` 存在的意义。
        if vfs::symlink("/mfs/S20NOPE.TXT", F20_L3) != 1 {
            println("app: FS20 symlink dangling FAILED");
            return;
        }
        if vfs::stat(F20_L3) != u64::MAX {
            println("app: FS20 stat dangling should FAIL");
            return;
        }
        if !fs20_readlink_is(F20_L3, "/mfs/S20NOPE.TXT") {
            println("app: FS20 readlink dangling FAILED");
            return;
        }
        match fs20_lstat(F20_L3) {
            // "/S20NOPE.TXT" = 12 字节。
            Some(st) if st.mode & vfs::MODE_FTYPE_MASK == vfs::MODE_FTYPE_LINK && st.size == 12 => {
            }
            _ => {
                println("app: FS20 lstat dangling FAILED");
                return;
            }
        }

        // (f) 非软链接上 readlink 必须失败 (普通文件 / 目录都不行)。
        if vfs::readlink(F20_T) != u64::MAX || vfs::readlink("/mfs") != u64::MAX {
            println("app: FS20 readlink non-link should FAIL");
            return;
        }
        // (g) lstat 对普通文件 / 目录与 stat 等价。
        match fs20_lstat(F20_T) {
            Some(st)
                if st.mode & vfs::MODE_FTYPE_MASK == vfs::MODE_FTYPE_FILE && st.size == 4096 => {}
            _ => {
                println("app: FS20 lstat file FAILED");
                return;
            }
        }
        match fs20_lstat("/mfs") {
            Some(st) if st.is_dir == 1 && st.size == 0 => {}
            _ => {
                println("app: FS20 lstat dir FAILED");
                return;
            }
        }

        // (h) 跨文件系统的绝对目标**拒绝创建**: 服务端解析不到别的挂载点, 与其留个
        //     静默悬空链接, 不如当场失败 (这正是 readlink 能安全加回前缀的前提 --
        //     绝对目标必然是同一个挂载点内的)。
        if vfs::symlink("/usb/ANY.TXT", "/mfs/S20L4") != u64::MAX
            || vfs::symlink("/tmp/ANY.TXT", "/mfs/S20L4") != u64::MAX
            || vfs::symlink("/nosuchmount/A.TXT", "/mfs/S20L4") != u64::MAX
        {
            println("app: FS20 cross-fs symlink should FAIL");
            return;
        }

        // 清理。
        vfs::unlink(F20_L1);
        vfs::unlink(F20_L2);
        vfs::unlink(F20_L3);
        vfs::unlink(F20_L5);
        vfs::unlink(F20_T);
    }
    // 24. FS-21 自测 (阶段 D/M7): 文件系统**铺满卷** —— 格式化尺寸按卷几何定。
    //     盯的是一个极易回退的默认值: 早先 `mfs_format` 无论卷多大都写死 4096 块
    //     (16 MiB), 于是整块新盘也只会格式出 16 MiB。断言只有一条关系式:
    //       MFS 总块数 × 8 扇区/块 == 它所在卷的 sectors
    //     它同时证明两件事: (a) Identify Namespace 的 NSZE 真填进了卷表 —— 整盘卷
    //     此前 `sectors` 恒为 0(容量未知); (b) 格式化确实按卷几何取尺寸。
    //     MFS7 起位图已外置到独立数据块 (见 FS-23(a)), 上界提到 ≈127.25 GiB
    //     (`MFS_MAX_BLOCKS`), 故 256 MiB 测试卷不再触发 clamp —— 这条等号在测试卷
    //     尺寸下恒成立。
    {
        let usage = vfs::mfs_stat();
        if usage == u64::MAX {
            println("app: FS21 mfs_stat FAILED");
            return;
        }
        let total = usage >> 32;
        let free = usage & 0xFFFF_FFFF;
        if total == 0 || free > total {
            println("app: FS21 usage sanity FAILED");
            return;
        }
        // 卷表里定位 MFS 那张盘 (整盘卷: mfs.img 是 nsid=2, start_lba=0)。
        let n = block_list_volumes(vfs::RESULT_BUF as *mut u8, 16);
        if n == 0 || n == u64::MAX {
            println("app: FS21 list volumes FAILED");
            return;
        }
        let mut i = 0u64;
        let mut found = false;
        while i < n {
            let d = vol_desc(vfs::RESULT_BUF as *const u8, i as usize);
            if d.nsid == 2 && d.start_lba == 0 {
                found = true;
                if d.sectors == 0 {
                    println("app: FS21 whole-disk volume still reports no capacity FAILED");
                    return;
                }
                if total * 8 != d.sectors as u64 {
                    println("app: FS21 fs does not fill its volume FAILED");
                    return;
                }
            }
            i += 1;
        }
        if !found {
            println("app: FS21 mfs disk missing from volume table FAILED");
        }
    }

    // 25. FS-22 自测 (S2): 显式格式化 (`mkfs.mfs`) + 额外 MFS 卷挂载与按卷服务。
    //     新增的空白盘 (nsid 6) 启动时卷层探测为 unknown —— 正是真盘上「刚买一块盘」的样子。
    //     验证四件事:
    //       (a) 护栏: 对 FAT / ext2 / 不存在的卷号调 mkfs 必须被拒 (绝不吞别人的分区);
    //       (b) 格式化空白卷成功, 该卷作为额外卷挂到 `/usb<卷号>` 后可独立读写;
    //       (c) 格式化**别的**卷之后, 主卷 `/mfs` 的数据仍完好 —— 证明 mfs_srv 把内存态
    //           (位图 / inode 表 / 快照) 正确重建回了主卷, 而不是继续用新卷的位图;
    //       (d) 在额外卷与主卷之间交替读写, 两边内容都不串 —— 证明按请求切卷生效。
    {
        let nvol = block_list_volumes(vfs::RESULT_BUF as *mut u8, 16);
        if nvol == u64::MAX || nvol < 6 {
            println("app: FS22 list volumes FAILED");
            return;
        }
        let mut fat_vol = u64::MAX;
        let mut ext2_vol = u64::MAX;
        let mut spare_vol = u64::MAX;
        let mut i = 0u64;
        while i < nvol {
            let d = vol_desc(vfs::RESULT_BUF as *const u8, i as usize);
            if d.nsid == 1 && d.kind == VOL_KIND_FAT {
                fat_vol = d.id as u64;
            }
            if d.nsid == 3 && d.kind == VOL_KIND_EXT2 {
                ext2_vol = d.id as u64;
            }
            if d.nsid == 6 {
                // 首次启动是空白 (unknown); 若保留上一轮的盘则是已格式化的 MFS。
                if d.kind != VOL_KIND_UNKNOWN && d.kind != VOL_KIND_MFS {
                    println("app: FS22 spare volume has unexpected kind FAILED");
                    return;
                }
                spare_vol = d.id as u64;
            }
            i += 1;
        }
        if fat_vol == u64::MAX || ext2_vol == u64::MAX || spare_vol == u64::MAX {
            println("app: FS22 test volumes missing FAILED");
            return;
        }

        // (a) 护栏: 别人的分区与不存在的卷号都不允许格式化。
        if vfs::mfs_mkfs(fat_vol) != u64::MAX {
            println("app: FS22 mkfs on FAT volume NOT refused FAILED");
            return;
        }
        if vfs::mfs_mkfs(ext2_vol) != u64::MAX {
            println("app: FS22 mkfs on ext2 volume NOT refused FAILED");
            return;
        }
        if vfs::mfs_mkfs(4242) != u64::MAX {
            println("app: FS22 mkfs on nonexistent volume NOT refused FAILED");
            return;
        }

        // (c) 先在主卷落一个标记, 格式化完别的卷后它必须还在。
        let keep = "/mfs/FS22KEEP.TXT";
        let kfd = vfs::creat(keep);
        if kfd == u64::MAX || vfs::write(kfd, 0, b"KEEP") != 4 {
            println("app: FS22 write marker on primary FAILED");
            return;
        }
        vfs::close(kfd);

        // (b) 格式化空白卷; 成功后 mfs_srv 会把它挂到 `/usb<卷号>`。
        // 回复是落盘后的主卷序号 (>0) —— 格式化同时把这块卷标记为主卷, 但那要**下次
        // 启动**才生效 (FS-24 会专门盯这条链路), 本次运行 `/mfs` 仍是原主卷。
        if vfs::mfs_mkfs(spare_vol) == u64::MAX {
            println("app: FS22 mkfs on blank volume FAILED");
            return;
        }

        // 拼出额外卷的挂载点 `/usb<卷号>` 与新卷上的目标路径。
        let mut pbuf = [0u8; 32];
        pbuf[..4].copy_from_slice(b"/usb");
        let rl = 4 + dec_to_str(spare_vol, &mut pbuf[4..]);
        let suffix = b"/NEW.TXT";
        pbuf[rl..rl + suffix.len()].copy_from_slice(suffix);
        let newpath = unsafe { core::str::from_utf8_unchecked(&pbuf[..rl + suffix.len()]) };

        let nfd = vfs::creat(newpath);
        if nfd == u64::MAX || vfs::write(nfd, 0, b"SPARE") != 5 {
            println("app: FS22 write on new volume FAILED");
            return;
        }
        vfs::close(nfd);

        // (d) 额外卷 -> 主卷 -> 额外卷 交替读, 各自内容不能串。
        let nfd = vfs::open(newpath);
        if nfd == u64::MAX || vfs::read(nfd, 0, 5) != 5 {
            println("app: FS22 reopen on new volume FAILED");
            return;
        }
        vfs::close(nfd);
        {
            let got = unsafe { core::slice::from_raw_parts(vfs::RESULT_BUF as *const u8, 5) };
            if got != b"SPARE" {
                println("app: FS22 new volume content mismatch FAILED");
                return;
            }
        }

        // (c) 主卷标记仍完好。
        let kfd = vfs::open(keep);
        if kfd == u64::MAX || vfs::read(kfd, 0, 4) != 4 {
            println("app: FS22 primary volume LOST after mkfs FAILED");
            return;
        }
        vfs::close(kfd);
        {
            let got = unsafe { core::slice::from_raw_parts(vfs::RESULT_BUF as *const u8, 4) };
            if got != b"KEEP" {
                println("app: FS22 primary volume content mismatch FAILED");
                return;
            }
        }
    }

    // 26. FS-23(a) 自测 (S3a): 位图外置后的**多块位图**容量路径。
    //     MFS7 把空闲位图从超级块里挪出来 (独立位图数据块 + 位图头块), 容量上限从
    //     内联位图的 30656 块 (≈119 MiB) 提到 `MFS_MAX_BLOCKS` = 1018 × 32768
    //     ≈ 127.25 GiB。一个 4 KiB 位图数据块覆盖 32768 块 (= 128 MiB), 故卷超过
    //     128 MiB 时 bb ≥ 2 —— 这正是本自测要走的路径 (默认测试卷 256 MiB → bb = 2)。
    //     断言 (只盯几何关系, 不真写满 128 MiB 数据: IPC 往返代价不可接受):
    //       (a) MFS 总块数 × 8 扇区/块 == 该卷 sectors —— 格式化按卷几何定尺寸仍成立;
    //       (b) 总块数 > 一个位图数据块的覆盖范围 (32768), 即 bb ≥ 2 —— 多块位图确实在用。
    //     多块位图的**读回 / CRC 校验 / 重建**由每次挂载校验 (逐 chunk 比对 CRC32) 与
    //     GC 全量重建覆盖: FS-12 每轮做 3 次全卷 GC, 会把所有 chunk 置脏并整体落盘。
    {
        // `mfs_stat` 报的是**当前卷**, 而 FS-22 刚在额外卷上折腾过 —— 先对主卷做一次
        // 操作把服务的内存态锚回主卷, 免得量到的是一块小卷。
        let probe = vfs::creat("/mfs/FS23PROBE.TXT");
        if probe == u64::MAX {
            println("app: FS23 pin primary volume FAILED");
            return;
        }
        vfs::close(probe);
        let usage = vfs::mfs_stat();
        if usage == u64::MAX {
            println("app: FS23 mfs_stat FAILED");
            return;
        }
        let total = usage >> 32;
        // 主卷必须跨过单个位图数据块的覆盖范围 (32768 块) -> bb ≥ 2, 多块位图在用。
        if total <= 32768 {
            println("app: FS23 primary volume too small for multi-chunk bitmap FAILED");
            return;
        }
        let n = block_list_volumes(vfs::RESULT_BUF as *mut u8, 16);
        if n == 0 || n == u64::MAX {
            println("app: FS23 list volumes FAILED");
            return;
        }
        let mut i = 0u64;
        let mut found = false;
        while i < n {
            let d = vol_desc(vfs::RESULT_BUF as *const u8, i as usize);
            if d.nsid == 2 && d.start_lba == 0 {
                found = true;
                if total * 8 != d.sectors as u64 {
                    println("app: FS23 fs does not fill its volume FAILED");
                    return;
                }
            }
            i += 1;
        }
        if !found {
            println("app: FS23 mfs disk missing from volume table FAILED");
        }
    }

    // 27. FS-23(b) 自测 (S3b): 单文件 >4 GiB (稀疏) —— u64 offset 端到端 + 三级间接块。
    //     在 /mfs 上建文件并 truncate 到 5 GiB + 12345 字节 (稀疏扩展, 不真写数据, 代价可
    //     接受); 再在**跨过 4 GiB 边界**的偏移 (4 GiB + 4 KiB) 写 16 字节已知内容, 同偏移
    //     读回逐字节校验。二级间接区的字节上限约 3.98 GiB
    //     ((1005 + 1022 + 1022²) × 4088), 故该偏移必然落在**三级间接区** —— 这条断言同时
    //     覆盖「u64 offset 端到端」与「三级间接块被真正使用」。
    const FS23B_OFF: u64 = 4 * 1024 * 1024 * 1024 + 4096;
    const FS23B_SIZE: u64 = 5 * 1024 * 1024 * 1024 + 12345;
    const FS23B_PAT: [u8; 16] = [0xA5; 16];
    {
        let fd = vfs::creat("/mfs/FS23BIG.BIN");
        if fd == u64::MAX {
            println("app: FS23 creat big file FAILED");
            return;
        }
        if vfs::truncate(fd, FS23B_SIZE) != 1 {
            println("app: FS23 truncate to 5GiB FAILED");
            vfs::close(fd);
            return;
        }
        // size 必须如实报出 5 GiB + 12345 (u64, 而不是被截窄到 32 位的值)。
        match fs13_stat("/mfs/FS23BIG.BIN") {
            Some(s) if s.size == FS23B_SIZE => {}
            _ => {
                println("app: FS23 big file size FAILED");
                vfs::close(fd);
                return;
            }
        }
        // 起始处仍是空洞: 整页读回必须全 0 (稀疏扩展不分配块)。
        if !fs13_all_zero(fd, 0, 4096) {
            println("app: FS23 head hole FAILED");
            vfs::close(fd);
            return;
        }
        // 跨 4 GiB 边界写 16 字节 (count ≤ 一页), 同偏移读回逐字节校验。
        if vfs::write(fd, FS23B_OFF, &FS23B_PAT) != 16 {
            println("app: FS23 write past 4GiB FAILED");
            vfs::close(fd);
            return;
        }
        if vfs::read(fd, FS23B_OFF, 16) != 16 {
            println("app: FS23 read past 4GiB FAILED");
            vfs::close(fd);
            return;
        }
        {
            let got = unsafe { core::slice::from_raw_parts(vfs::RESULT_BUF as *const u8, 16) };
            if got != FS23B_PAT {
                println("app: FS23 past-4GiB content mismatch FAILED");
                vfs::close(fd);
                return;
            }
        }
        vfs::close(fd);
    }

    // 28. FS-23(c) 自测 (S3b): 三级块在重新打开与 GC 后仍可达。
    //     重新 open 后读同一偏移内容一致 -> size/指针已正确持久化, 不依赖内存态;
    //     再调 MFS 显式 GC 后复读一致 -> GC 的可达性标记正确覆盖了 MFI3 (漏标会把三级块
    //     当垃圾回收并重新分配出去, 这条读取就会失败或读到错内容)。
    {
        let fd = vfs::open("/mfs/FS23BIG.BIN");
        if fd == u64::MAX {
            println("app: FS23 reopen FAILED");
            return;
        }
        if vfs::read(fd, FS23B_OFF, 16) != 16 {
            println("app: FS23 read after reopen FAILED");
            vfs::close(fd);
            return;
        }
        {
            let got = unsafe { core::slice::from_raw_parts(vfs::RESULT_BUF as *const u8, 16) };
            if got != FS23B_PAT {
                println("app: FS23 content lost after reopen FAILED");
                vfs::close(fd);
                return;
            }
        }
        vfs::close(fd);
        if vfs::mfs_gc() == u64::MAX {
            println("app: FS23 GC FAILED");
            return;
        }
        let fd = vfs::open("/mfs/FS23BIG.BIN");
        if fd == u64::MAX {
            println("app: FS23 reopen after GC FAILED");
            return;
        }
        if vfs::read(fd, FS23B_OFF, 16) != 16 {
            println("app: FS23 read after GC FAILED");
            vfs::close(fd);
            return;
        }
        {
            let got = unsafe { core::slice::from_raw_parts(vfs::RESULT_BUF as *const u8, 16) };
            if got != FS23B_PAT {
                println("app: FS23 content lost after GC FAILED");
                vfs::close(fd);
                return;
            }
        }
        vfs::close(fd);
        // 清理: 删掉这个 5 GiB 稀疏文件, 避免污染后续回归与下一轮持久卷。
        if vfs::unlink("/mfs/FS23BIG.BIN") != 1 {
            println("app: FS23 cleanup unlink FAILED");
            return;
        }
    }

    // 29. FS-24 自测 (S2 补齐): **主卷切换的持久化标记**。
    //     `mkfs.mfs <卷号>` 把该卷标记为主卷 (超级块 `+256` 的序号 = 现有最大 + 1),
    //     从此**下次启动** `/mfs` 认领的就是它 —— 与卷表扫描顺序无关 (M8 未覆盖项的收口)。
    //     会话内不能重启, 故把这条链路拆成三个可观测环节:
    //       (a) mkfs 回复的序号是**从盘上回读**的 (>0) —— 标记确实写进超级块且能再读出;
    //       (b) 再次 mkfs 同一卷, 序号严格变大 —— 这就是「最近 mkfs 的卷胜出」的判据;
    //       (c) 两次 mkfs 之间对该卷做一次**普通写提交**, 序号仍继续变大 —— 证明标记在
    //           普通提交里被保留 (`mfs_load_state` 回读 + `mfs_build_super` 回写)。
    //           若标记被一次普通写盘抹成 0, (b)(c) 的序号会掉回 1, 断言立刻失败。
    //       (d) 全程主卷 `/mfs` 照常可读 —— 给别的卷换标不扰动正在服务的卷。
    {
        let nvol = block_list_volumes(vfs::RESULT_BUF as *mut u8, 16);
        if nvol == u64::MAX {
            println("app: FS24 list volumes FAILED");
            return;
        }
        let mut spare_vol = u64::MAX;
        let mut i = 0u64;
        while i < nvol {
            let d = vol_desc(vfs::RESULT_BUF as *const u8, i as usize);
            if d.nsid == 6 {
                spare_vol = d.id as u64;
            }
            i += 1;
        }
        if spare_vol == u64::MAX {
            println("app: FS24 spare volume missing FAILED");
            return;
        }

        // (a) 序号来自盘上回读 (FS-22 已格式化过一次, 这里再格一次同样应拿到正序号)。
        let s1 = vfs::mfs_mkfs(spare_vol);
        if s1 == u64::MAX || s1 == 0 {
            println("app: FS24 primary mark not on disk FAILED");
            return;
        }
        // (b) 同一卷再格式化: 序号必须严格变大。
        let s2 = vfs::mfs_mkfs(spare_vol);
        if s2 == u64::MAX || s2 <= s1 {
            println("app: FS24 primary serial not increasing FAILED");
            return;
        }

        // (c) 对该卷做一次普通写提交 (走 /usb<卷号> 挂载点, 会切到该卷并提交超级块)。
        let mut pbuf = [0u8; 32];
        pbuf[..4].copy_from_slice(b"/usb");
        let rl = 4 + dec_to_str(spare_vol, &mut pbuf[4..]);
        let suffix = b"/FS24.TXT";
        pbuf[rl..rl + suffix.len()].copy_from_slice(suffix);
        let path = unsafe { core::str::from_utf8_unchecked(&pbuf[..rl + suffix.len()]) };
        let fd = vfs::creat(path);
        if fd == u64::MAX || vfs::write(fd, 0, b"M") != 1 {
            println("app: FS24 write on marked volume FAILED");
            return;
        }
        vfs::close(fd);

        let s3 = vfs::mfs_mkfs(spare_vol);
        if s3 == u64::MAX || s3 <= s2 {
            println("app: FS24 primary mark lost across commit FAILED");
            return;
        }

        // (d) 主卷未被扰动 (FS-23(a) 落下的探针文件仍在)。
        let fd = vfs::open("/mfs/FS23PROBE.TXT");
        if fd == u64::MAX {
            println("app: FS24 primary volume damaged by mkfs FAILED");
            return;
        }
        vfs::close(fd);
    }
    println("app: SELFTEST DONE");
}

// ===========================================================================
// 域 8 — Shell (命令行解释器)
// ===========================================================================
// SH-3 阶段: 在基础命令之上补齐路径支持 (cwd + 相对路径) 与写命令
// cd / pwd / mkdir / rm / touch。
// ls / cat / cd 等经 libvfs 调用 fat32_srv; shell 使用独立的 `SHELL_RESULT_BUF`
// 缓冲页, 与 app 域的结果页地址不同, 避免在 fat32_srv 地址空间内互相覆盖。
//
// 约定: 提示符必须以换行结束 (println), 使内核输入行缓冲在用户输入前为空,
// 这样 SYS_READLINE 取回的行只含用户键入的字符, 不含提示符。

/// shell 读入单行的最大长度。
const SHELL_LINE_MAX: usize = 128;
/// 当前工作目录最大长度。
const CWD_MAX: usize = 128;
/// 解析后绝对路径的静态缓冲大小。
const PATH_MAX: usize = 256;

/// shell 运行时状态: 当前工作目录 (绝对路径, 以 '/' 开头)。
struct ShellState {
    cwd: [u8; CWD_MAX],
    cwd_len: usize,
}

impl ShellState {
    fn cwd_str(&self) -> &str {
        unsafe { core::str::from_utf8_unchecked(&self.cwd[..self.cwd_len]) }
    }
    fn set_cwd(&mut self, path: &str) {
        let n = path.len().min(CWD_MAX);
        self.cwd[..n].copy_from_slice(&path.as_bytes()[..n]);
        self.cwd_len = n;
    }
}

/// 解析结果静态缓冲 (单域单线程; 调用方须在下次解析前消费返回的字符串)。
static mut PATH_BUF: [u8; PATH_MAX] = [0u8; PATH_MAX];

/// 把 `arg` 相对 `cwd` 解析为归一化的绝对路径, 写入 `out`, 返回长度。
/// `arg` 以 '/' 开头时按绝对路径处理; 处理 "." / ".." 与重复 '/'。越界返回 None。
fn join_path(cwd: &str, arg: &str, out: &mut [u8]) -> Option<usize> {
    // 1. 拼接 raw: 绝对参数直接使用, 否则 cwd + '/' + arg。
    let mut raw = [0u8; CWD_MAX + SHELL_LINE_MAX];
    let mut rlen = 0usize;
    if !arg.starts_with('/') {
        let c = cwd.as_bytes();
        let n = c.len().min(CWD_MAX);
        raw[..n].copy_from_slice(&c[..n]);
        rlen += n;
        if rlen == 0 || raw[rlen - 1] != b'/' {
            if rlen >= raw.len() {
                return None;
            }
            raw[rlen] = b'/';
            rlen += 1;
        }
    }
    let a = arg.as_bytes();
    if rlen + a.len() > raw.len() {
        return None;
    }
    raw[rlen..rlen + a.len()].copy_from_slice(a);
    rlen += a.len();

    // 2. 逐段归一化, 结果以 '/' 开头。
    if out.is_empty() {
        return None;
    }
    let mut n = 0usize;
    out[n] = b'/';
    n += 1;
    for seg in raw[..rlen].split(|&b| b == b'/') {
        if seg.is_empty() || seg == b"." {
            continue;
        }
        if seg == b".." {
            // 回退一级 (已在根时不动)。
            if n > 1 {
                let mut i = n - 1;
                let mut found = false;
                while i >= 1 {
                    if out[i] == b'/' {
                        n = i;
                        found = true;
                        break;
                    }
                    i -= 1;
                }
                if !found {
                    n = 1;
                }
            }
            continue;
        }
        if n > 1 {
            if n >= out.len() {
                return None;
            }
            out[n] = b'/';
            n += 1;
        }
        if n + seg.len() > out.len() {
            return None;
        }
        out[n..n + seg.len()].copy_from_slice(seg);
        n += seg.len();
    }
    Some(n)
}

/// 把 `arg` 解析为相对当前工作目录的绝对路径字符串 (写入静态 `PATH_BUF`)。
fn resolve_in_cwd(cwd: &str, arg: &str) -> Option<&'static str> {
    unsafe {
        let buf = &mut *core::ptr::addr_of_mut!(PATH_BUF);
        let n = join_path(cwd, arg, buf)?;
        Some(core::str::from_utf8_unchecked(&buf[..n]))
    }
}

/// 域 8 — Shell: 分配结果页 → 循环「提示符 → 读行 → 执行命令」。
fn shell_main() {
    // 分配 shell 专用结果页并共享给各文件服务 (同地址映射); ls/cat 的结果写在此页。
    if sys_alloc_page(vfs::SHELL_RESULT_BUF) != 1 {
        println("shell: alloc result buf FAILED");
        return;
    }
    if sys_share_page(vfs::SHELL_RESULT_BUF, vfs::FAT32_DOMAIN) != 1
        || sys_share_page(vfs::SHELL_RESULT_BUF, vfs::TMPFS_DOMAIN) != 1
        || sys_share_page(vfs::SHELL_RESULT_BUF, vfs::MFS_DOMAIN) != 1
        || sys_share_page(vfs::SHELL_RESULT_BUF, vfs::EXT2_DOMAIN) != 1
        || sys_share_page(vfs::SHELL_RESULT_BUF, vfs::EXFAT_DOMAIN) != 1
    {
        println("shell: share result buf FAILED");
        return;
    }
    println("shell: type 'help' for commands");

    // 初始工作目录为根。
    let mut st = ShellState {
        cwd: [0; CWD_MAX],
        cwd_len: 1,
    };
    st.cwd[0] = b'/';

    let mut line = [0u8; SHELL_LINE_MAX];
    loop {
        // 行内提示符: 与用户输入同处一行, 形如正常终端 `[user@host cwd]$ `。
        print("[morion@morion ");
        print(st.cwd_str());
        print("]$ ");
        syscall::flush();

        let n = sys_readline(&mut line);
        if n == u64::MAX {
            println("shell: readline FAILED");
            return;
        }
        shell_exec(&mut st, &line[..n as usize]);
    }
}

/// 解析并执行一行命令 (命令与参数以首个空格分隔)。
fn shell_exec(st: &mut ShellState, line: &[u8]) {
    let s = unsafe { core::str::from_utf8_unchecked(line) };
    let s = s.trim();
    if s.is_empty() {
        return;
    }
    let (cmd, arg) = match s.find(' ') {
        Some(i) => (&s[..i], s[i + 1..].trim()),
        None => (s, ""),
    };

    match cmd {
        "help" => {
            println("commands:");
            println("  help           show this help");
            println("  echo <text>    print text");
            println("  pwd            print working directory");
            println("  ls [-l] [path] list directory (-l: long form)");
            println("  cat <file>     print file content");
            println("  cd [path]      change directory (default: /)");
            println("  mkdir <path>   create directory");
            println("  touch <file>   create empty file");
            println("  rm <path>      remove file / empty directory");
            println("  mv <src> <dst> rename / move (same filesystem)");
            println("  ln <src> <dst> hard link an existing file (same filesystem)");
            println("  ln -s <target> <name>  symbolic link (target kept verbatim; MFS only)");
            println("  chmod <mode> <path>  set permission bits (octal, display-only)");
            println("  truncate <file> <size>  resize a file (sparse on grow)");
            println("  stat <path>    show metadata (mode / owner / links / times)");
            println("  lstat <path>   like stat but on the link itself (no follow)");
            println("  readlink <link>  print a symbolic link's target (no follow)");
            println("  mkfs.mfs <vol>   create a MorionFS filesystem on a volume (ERASES it)");
            println("  clear          clear screen");
            println("  (mounts: / = fat32, /tmp = tmpfs, /mfs = MorionFS, /ext2 = ext2 ro, /usb = exFAT)");
            println(
                "  (extra volumes auto-mounted as /usb<N>, N = volume id in the boot volume list)",
            );
        }
        "echo" => println(arg),
        "pwd" => println(st.cwd_str()),
        "ls" => shell_ls(st, if arg.is_empty() { "." } else { arg }),
        "cat" => shell_cat(st, arg),
        "cd" => shell_cd(st, if arg.is_empty() { "/" } else { arg }),
        "mkdir" => shell_mkdir(st, arg),
        "touch" => shell_touch(st, arg),
        "rm" => shell_rm(st, arg),
        "mv" => shell_mv(st, arg),
        "ln" => shell_ln(st, arg),
        "chmod" => shell_chmod(st, arg),
        "truncate" => shell_truncate(st, arg),
        "stat" => shell_stat(st, arg, false),
        "lstat" => shell_stat(st, arg, true),
        "readlink" => shell_readlink(st, arg),
        "mkfs.mfs" => shell_mkfs(arg),
        "clear" => {
            sys_clear();
        }
        _ => {
            print("shell: unknown command: ");
            println(cmd);
        }
    }
}

/// `ls [-l] [path]` — 列出目录条目; `-l` 时附权限 / 属主 / 链接数 / 时间。
fn shell_ls(st: &ShellState, arg: &str) {
    // `-l` 解析: 只支持这一个开关, 其余部分当路径。
    let (long, rest) = match arg.strip_prefix("-l") {
        Some(r) => (true, r.trim()),
        None => (false, arg),
    };
    let arg = if rest.is_empty() { "." } else { rest };
    let path = match resolve_in_cwd(st.cwd_str(), arg) {
        Some(p) => p,
        None => {
            println("ls: path too long");
            return;
        }
    };
    let fd = vfs::open(path);
    if fd == u64::MAX {
        print("ls: cannot open ");
        println(path);
        return;
    }
    let n = vfs::readdir_into(fd, vfs::SHELL_RESULT_BUF);
    if n == u64::MAX {
        print("ls: not a directory: ");
        println(path);
        vfs::close(fd);
        return;
    }
    let entry_size = core::mem::size_of::<vfs::DirEntry>();
    let count = n as usize / entry_size;
    let list = unsafe {
        core::slice::from_raw_parts(vfs::SHELL_RESULT_BUF as *const vfs::DirEntry, count)
    };
    for de in list {
        if long {
            print_mode(de.mode, de.is_dir != 0);
            print(" owner=");
            print_u64(de.owner as u64);
            print(" links=");
            print_u64(de.nlink as u64);
            print(" size=");
            print_u64_pad(de.size, 8);
            print("  ");
            print_time(de.mtime);
            print("  ");
            print_entry_name(de);
            println("");
        } else {
            print(entry_kind_label(de));
            print_entry_name(de);
            if de.is_dir == 0 {
                print("  size=");
                print_u64(de.size);
            }
            println("");
        }
    }
    vfs::close(fd);
}

/// `chmod <mode> <path>` — 修改权限位 (八进制, 低 12 位有效)。
///
/// 权限位当前只存储与显示, 不参与访问判定 (系统还没有多用户概念)。
fn shell_chmod(st: &ShellState, arg: &str) {
    let (mode_s, path_s) = match arg.find(' ') {
        Some(i) => (&arg[..i], arg[i + 1..].trim()),
        None => {
            println("chmod: usage: chmod <octal-mode> <path>");
            return;
        }
    };
    if path_s.is_empty() {
        println("chmod: usage: chmod <octal-mode> <path>");
        return;
    }
    let mode = match parse_octal(mode_s) {
        Some(m) => m,
        None => {
            print("chmod: bad mode: ");
            println(mode_s);
            return;
        }
    };
    let path = match resolve_in_cwd(st.cwd_str(), path_s) {
        Some(p) => p,
        None => {
            println("chmod: path too long");
            return;
        }
    };
    if vfs::chmod_into(path, mode, vfs::SHELL_RESULT_BUF) == 1 {
        print("chmod: mode=");
        print_u64(mode as u64);
        print(" ");
        println(path);
    } else {
        print("chmod: failed: ");
        println(path);
    }
}

/// `mv <src> <dst>` — 重命名 / 移动 (同一次请求内跨目录; 不支持跨文件系统)。
fn shell_mv(st: &ShellState, arg: &str) {
    let (src_s, dst_s) = match arg.find(' ') {
        Some(i) => (&arg[..i], arg[i + 1..].trim()),
        None => {
            println("mv: usage: mv <src> <dst>");
            return;
        }
    };
    if dst_s.is_empty() {
        println("mv: usage: mv <src> <dst>");
        return;
    }
    let src = match resolve_in_cwd(st.cwd_str(), src_s) {
        Some(p) => p,
        None => {
            println("mv: source path too long");
            return;
        }
    };
    let dst = match resolve_in_cwd(st.cwd_str(), dst_s) {
        Some(p) => p,
        None => {
            println("mv: target path too long");
            return;
        }
    };
    if vfs::rename_into(src, dst, vfs::SHELL_RESULT_BUF) == 1 {
        print("mv: ");
        print(src);
        print(" -> ");
        println(dst);
    } else {
        print("mv: failed (cross-fs, bad target, or directory loop): ");
        println(src);
    }
}

/// `ln [-s] <target> <name>` — 硬链接 (`ln`) 或软链接 (`ln -s`, 阶段 D/M5c)。
///
/// 硬链接: 两个名字共享同一个 inode, 从任一个名字改写内容, 另一个名字都会看到。
/// 软链接: 存的是**目标路径字符串**, `target` 原样落盘 —— 相对路径相对链接所在目录,
/// 允许悬空 (目标可以之后才创建)。`rm` 一个软链接只摘掉链接, 不动目标。
fn shell_ln(st: &ShellState, arg: &str) {
    let (sym, rest) = match arg.strip_prefix("-s") {
        Some(r) => (true, r.trim()),
        None => (false, arg),
    };
    let (src_s, dst_s) = match rest.find(' ') {
        Some(i) => (&rest[..i], rest[i + 1..].trim()),
        None => {
            println(if sym {
                "ln: usage: ln -s <target> <link-name>"
            } else {
                "ln: usage: ln <existing-file> <new-name>"
            });
            return;
        }
    };
    if dst_s.is_empty() {
        println(if sym {
            "ln: usage: ln -s <target> <link-name>"
        } else {
            "ln: usage: ln <existing-file> <new-name>"
        });
        return;
    }
    let dst = match resolve_in_cwd(st.cwd_str(), dst_s) {
        Some(p) => p,
        None => {
            println("ln: target path too long");
            return;
        }
    };
    if sym {
        // 目标**不做路径解析**: 它只是一段要存进链接节点的字符串。
        if vfs::symlink_into(src_s, dst, vfs::SHELL_RESULT_BUF) == 1 {
            print("ln -s: ");
            print(dst);
            print(" -> ");
            println(src_s);
        } else {
            print("ln -s: failed (name exists, target empty/too long, or no MFS): ");
            println(dst);
        }
        return;
    }
    let src = match resolve_in_cwd(st.cwd_str(), src_s) {
        Some(p) => p,
        None => {
            println("ln: source path too long");
            return;
        }
    };
    if vfs::link_into(src, dst, vfs::SHELL_RESULT_BUF) == 1 {
        print("ln: ");
        print(dst);
        print(" -> ");
        println(src);
    } else {
        print("ln: failed (needs an existing file, new name, same fs): ");
        println(src);
    }
}

/// `truncate <path> <size>` — 把文件截断/扩展到指定字节数。
fn shell_truncate(st: &ShellState, arg: &str) {
    let (path_s, size_s) = match arg.find(' ') {
        Some(i) => (&arg[..i], arg[i + 1..].trim()),
        None => {
            println("truncate: usage: truncate <path> <size>");
            return;
        }
    };
    // size 现在是 u64: 不再把上限卡在 4 GiB, 与协议 / MFS 的能力对齐。
    let size = match parse_dec(size_s) {
        Some(v) => v,
        _ => {
            print("truncate: bad size: ");
            println(size_s);
            return;
        }
    };
    let path = match resolve_in_cwd(st.cwd_str(), path_s) {
        Some(p) => p,
        None => {
            println("truncate: path too long");
            return;
        }
    };
    let fd = vfs::open(path);
    if fd == u64::MAX {
        print("truncate: cannot open ");
        println(path);
        return;
    }
    let r = vfs::truncate(fd, size);
    vfs::close(fd);
    if r == 1 {
        print("truncate: size=");
        print_u64(size);
        print(" ");
        println(path);
    } else {
        print("truncate: failed (directory or bad file): ");
        println(path);
    }
}

/// `stat <path>` / `lstat <path>` — 打印单条路径的完整元数据。
///
/// `no_follow = false` (`stat`) 跟随末段软链接 (悬空链接会失败, 与 Unix `stat` 一致);
/// `no_follow = true` (`lstat`) 作用于链接自身 —— 类型显示为 `symbolic link`、
/// `Size` 是**目标串长度**, 悬空链接也照样看得到。
fn shell_stat(st: &ShellState, arg: &str, no_follow: bool) {
    if arg.is_empty() {
        println("stat: missing operand");
        return;
    }
    let path = match resolve_in_cwd(st.cwd_str(), arg) {
        Some(p) => p,
        None => {
            println("stat: path too long");
            return;
        }
    };
    let want = core::mem::size_of::<vfs::Stat>() as u64;
    let got = if no_follow {
        vfs::lstat_into(path, vfs::SHELL_RESULT_BUF)
    } else {
        vfs::stat_into(path, vfs::SHELL_RESULT_BUF)
    };
    if got != want {
        print(if no_follow {
            "lstat: cannot stat "
        } else {
            "stat: cannot stat "
        });
        println(path);
        return;
    }
    let st = unsafe { core::ptr::read_unaligned(vfs::SHELL_RESULT_BUF as *const vfs::Stat) };
    print("  File: ");
    println(path);
    print("  Type: ");
    println(entry_type_name(st.mode, st.is_dir != 0));
    print("  Mode: ");
    print_mode(st.mode, st.is_dir != 0);
    print("  Owner: domain ");
    print_u64(st.owner as u64);
    print("  Links: ");
    print_u64(st.nlink as u64);
    print("  Size:  ");
    print_u64(st.size);
    print("  Modify: ");
    print_time(st.mtime);
    print("  Change: ");
    print_time(st.ctime);
}

/// `readlink <link>` — 打印软链接**自身**的目标串 (不跟随)。
///
/// 输出的正是当初 `ln -s` 写的那个路径: 服务端存的是服务命名空间里的形式, 客户端
/// `vfs::readlink_into` 会把挂载前缀加回去 (`/a` -> `/mfs/a`)。
fn shell_readlink(st: &ShellState, arg: &str) {
    if arg.is_empty() {
        println("readlink: missing operand");
        return;
    }
    let path = match resolve_in_cwd(st.cwd_str(), arg) {
        Some(p) => p,
        None => {
            println("readlink: path too long");
            return;
        }
    };
    let n = vfs::readlink_into(path, vfs::SHELL_RESULT_BUF);
    if n == u64::MAX || n == 0 {
        print("readlink: not a symbolic link: ");
        println(path);
        return;
    }
    let raw =
        unsafe { core::slice::from_raw_parts(vfs::SHELL_RESULT_BUF as *const u8, n as usize) };
    let target = unsafe { core::str::from_utf8_unchecked(raw) };
    print_sanitized(target);
    println("");
}

/// `mkfs.mfs <卷号>` — 在指定卷上创建 MorionFS 文件系统 (**擦除**该卷现有内容)。
///
/// 卷号来自块服务启动时打印的卷表 (`vol: <卷号> nsid=... kind=...`)。护栏不在这里
/// 而在服务端: mfs_srv 只接受 `kind=mfs` (重新格式化) 或 `kind=unknown` (未格式化)
/// 的卷, FAT / exFAT / ext2 一律拒绝 —— 命令与自测走同一条路径, 判定只有一处。
///
/// 格式化同时把这卷标记为**主卷**: **下次启动** `/mfs` 就是它 (本次运行的挂载不变)。
fn shell_mkfs(arg: &str) {
    let vol = match parse_dec(arg) {
        Some(v) => v,
        None => {
            println(
                "mkfs.mfs: usage: mkfs.mfs <volume-id>   (see the 'vol:' lines in the boot log)",
            );
            return;
        }
    };
    let serial = vfs::mfs_mkfs(vol);
    if serial != u64::MAX {
        print("mkfs.mfs: volume ");
        print_u64(vol);
        print(" formatted, marked primary (serial ");
        print_u64(serial);
        println(") -> /mfs after next boot; other volumes mount at /usb<volume-id>");
    } else {
        print("mkfs.mfs: refused volume ");
        print_u64(vol);
        println(" (not blank, not MFS, or no such volume)");
    }
}

/// 按 8 / 10 进制解析无符号整数 (不带前缀, 空串/非法字符返回 None)。
fn parse_octal(s: &str) -> Option<u32> {
    parse_radix(s, 8)
}
fn parse_dec(s: &str) -> Option<u64> {
    parse_radix(s, 10).map(|v| v as u64)
}
fn parse_radix(s: &str, radix: u32) -> Option<u32> {
    if s.is_empty() {
        return None;
    }
    let mut v: u32 = 0;
    for c in s.bytes() {
        let d = match c {
            b'0'..=b'9' => (c - b'0') as u32,
            b'a'..=b'f' => (c - b'a') as u32 + 10,
            _ => return None,
        };
        if d >= radix {
            return None;
        }
        v = v.checked_mul(radix)?.checked_add(d)?;
    }
    Some(v)
}

/// 打印 `ls -l` / `stat` 用的类型+权限字符串 (10 字符)。
///
/// 首字符优先取 `mode` 高 4 位的类型位 (MFS 会填, 能区分出软链接 `l`); 没有类型位
/// 的文件服务 (fat32 / ext2 / exFAT / tmpfs) 回退到 `is_dir`。
fn print_mode(mode: u16, is_dir: bool) {
    const RWX: [u8; 9] = *b"rwxrwxrwx";
    let mut out = [b'-'; 10];
    out[0] = match mode & vfs::MODE_FTYPE_MASK {
        vfs::MODE_FTYPE_DIR => b'd',
        vfs::MODE_FTYPE_LINK => b'l',
        vfs::MODE_FTYPE_FILE => b'-',
        _ => {
            if is_dir {
                b'd'
            } else {
                b'-'
            }
        }
    };
    for (i, slot) in out.iter_mut().enumerate().skip(1) {
        let bit = 9 - i; // i=1 -> bit8 (owner r) ... i=9 -> bit0 (other x)
        if mode & (1 << bit) != 0 {
            *slot = RWX[i - 1];
        }
    }
    print(unsafe { core::str::from_utf8_unchecked(&out) });
}

/// 条目的类型短标签 (供 `ls` 非长格式显示)。
fn entry_kind_label(de: &vfs::DirEntry) -> &'static str {
    match de.mode & vfs::MODE_FTYPE_MASK {
        vfs::MODE_FTYPE_DIR => "[DIR]  ",
        vfs::MODE_FTYPE_LINK => "[LINK] ",
        _ => {
            if de.is_dir != 0 {
                "[DIR]  "
            } else {
                "[FILE] "
            }
        }
    }
}

/// `stat` 的 Type 行文本。
fn entry_type_name(mode: u16, is_dir: bool) -> &'static str {
    match mode & vfs::MODE_FTYPE_MASK {
        vfs::MODE_FTYPE_DIR => "directory",
        vfs::MODE_FTYPE_LINK => "symbolic link",
        vfs::MODE_FTYPE_FILE => "regular file",
        _ => {
            if is_dir {
                "directory"
            } else {
                "regular file"
            }
        }
    }
}

/// 打印无符号整数, 不足 `width` 位左侧补 '0'。
fn print_u64_pad(v: u64, width: usize) {
    let mut buf = [b'0'; 24];
    let mut i = buf.len();
    let mut x = v;
    loop {
        i -= 1;
        buf[i] = b'0' + (x % 10) as u8;
        x /= 10;
        if x == 0 {
            break;
        }
    }
    while buf.len() - i < width && i > 0 {
        i -= 1;
        buf[i] = b'0';
    }
    print(unsafe { core::str::from_utf8_unchecked(&buf[i..]) });
}

/// 打印 Unix 秒 (UTC) 为 `YYYY-MM-DD HH:MM`; 0 表示"未知", 打印占位符。
fn print_time(secs: u64) {
    if secs == 0 {
        print("(unknown)");
        return;
    }
    let days = (secs / 86400) as i64;
    let rem = secs % 86400;
    let (y, m, d) = civil_from_days(days);
    print_u64_pad(y as u64, 4);
    print("-");
    print_u64_pad(m as u64, 2);
    print("-");
    print_u64_pad(d as u64, 2);
    print(" ");
    print_u64_pad(rem / 3600, 2);
    print(":");
    print_u64_pad((rem % 3600) / 60, 2);
}

/// 距 1970-01-01 的天数 → 民用历 (年/月/日), `days_from_civil` 的逆运算。
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// `cat <file>` — 打印文件内容 (不可打印字节替换为 '.')。
fn shell_cat(st: &ShellState, arg: &str) {
    if arg.is_empty() {
        println("cat: missing file operand");
        return;
    }
    let path = match resolve_in_cwd(st.cwd_str(), arg) {
        Some(p) => p,
        None => {
            println("cat: path too long");
            return;
        }
    };
    let fd = vfs::open(path);
    if fd == u64::MAX {
        print("cat: cannot open ");
        println(path);
        return;
    }
    let n = vfs::read_into(fd, 0, 4096, vfs::SHELL_RESULT_BUF);
    if n == u64::MAX {
        println("cat: read failed (is it a directory?)");
    } else {
        let content =
            unsafe { core::slice::from_raw_parts(vfs::SHELL_RESULT_BUF as *const u8, n as usize) };
        let s = unsafe { core::str::from_utf8_unchecked(content) };
        print_sanitized(s);
        println("");
    }
    vfs::close(fd);
}

/// `cd [path]` — 切换工作目录 (须为已存在目录); 无参数回到根目录。
fn shell_cd(st: &mut ShellState, arg: &str) {
    let path = match resolve_in_cwd(st.cwd_str(), arg) {
        Some(p) => p,
        None => {
            println("cd: path too long");
            return;
        }
    };
    let fd = vfs::open(path);
    if fd == u64::MAX {
        print("cd: no such directory: ");
        println(path);
        return;
    }
    let n = vfs::readdir_into(fd, vfs::SHELL_RESULT_BUF);
    vfs::close(fd);
    if n == u64::MAX {
        print("cd: not a directory: ");
        println(path);
        return;
    }
    st.set_cwd(path);
}

/// `mkdir <path>` — 创建目录。
fn shell_mkdir(st: &ShellState, arg: &str) {
    if arg.is_empty() {
        println("mkdir: missing operand");
        return;
    }
    let path = match resolve_in_cwd(st.cwd_str(), arg) {
        Some(p) => p,
        None => {
            println("mkdir: path too long");
            return;
        }
    };
    if vfs::mkdir(path) == 1 {
        print("mkdir: created ");
        println(path);
    } else {
        print("mkdir: failed (exists or bad parent): ");
        println(path);
    }
}

/// `touch <file>` — 创建空文件 (已存在则视为打开, 不报错)。
fn shell_touch(st: &ShellState, arg: &str) {
    if arg.is_empty() {
        println("touch: missing operand");
        return;
    }
    let path = match resolve_in_cwd(st.cwd_str(), arg) {
        Some(p) => p,
        None => {
            println("touch: path too long");
            return;
        }
    };
    let fd = vfs::creat(path);
    if fd == u64::MAX {
        print("touch: failed (bad parent?): ");
        println(path);
    } else {
        vfs::close(fd);
        print("touch: created ");
        println(path);
    }
}

/// `rm <path>` — 删除文件; 文件删除失败时尝试按空目录删除。
fn shell_rm(st: &ShellState, arg: &str) {
    if arg.is_empty() {
        println("rm: missing operand");
        return;
    }
    let path = match resolve_in_cwd(st.cwd_str(), arg) {
        Some(p) => p,
        None => {
            println("rm: path too long");
            return;
        }
    };
    if vfs::unlink(path) == 1 {
        print("rm: removed ");
        println(path);
    } else if vfs::rmdir(path) == 1 {
        print("rm: removed directory ");
        println(path);
    } else {
        print("rm: failed (not found, or directory not empty): ");
        println(path);
    }
}

/// 列出目录 fd 的条目, 判断是否存在短名为 `name` 且类型匹配 `want_dir` 的条目。
/// readdir 失败或未命中返回 false。
fn readdir_has(fd: u64, name: &str, want_dir: bool) -> bool {
    let sn = match short_name_from_query(name.as_bytes()) {
        Some(sn) => sn,
        None => return false,
    };
    let n = vfs::readdir(fd);
    if n == u64::MAX {
        return false;
    }
    let entry_size = core::mem::size_of::<vfs::DirEntry>();
    let count = n as usize / entry_size;
    let list =
        unsafe { core::slice::from_raw_parts(vfs::RESULT_BUF as *const vfs::DirEntry, count) };
    for de in list {
        if de.name == sn && (de.is_dir != 0) == want_dir {
            return true;
        }
    }
    false
}

/// 在 readdir 结果中按**长名字段**查找条目 (VFAT LFN / ext2 名字)。
fn readdir_has_long(fd: u64, name: &str, want_dir: bool) -> bool {
    let n = vfs::readdir(fd);
    if n == u64::MAX {
        return false;
    }
    let entry_size = core::mem::size_of::<vfs::DirEntry>();
    let count = n as usize / entry_size;
    let list =
        unsafe { core::slice::from_raw_parts(vfs::RESULT_BUF as *const vfs::DirEntry, count) };
    let q = name.as_bytes();
    for de in list {
        let llen = de.long_len as usize;
        if llen == q.len() && &de.long[..llen] == q && (de.is_dir != 0) == want_dir {
            return true;
        }
    }
    false
}

/// 打印长名 (UTF-8): ASCII 原样输出, 一个非 ASCII 字符折成一个 '?'。
///
/// 内核字体只有 ASCII 0x20..0x7E, 非 ASCII 字节不会被绘制 (会在屏幕上留空隙),
/// 故在用户态先折成 '?' 再送出去, 保证名字长度与视觉都对得上。
fn print_long_name(name: &[u8]) {
    let mut out = [0u8; vfs::DIR_LONG_MAX];
    let mut n = 0usize;
    let mut i = 0usize;
    while i < name.len() && n < out.len() {
        let b = name[i];
        if b < 0x80 {
            out[n] = if (0x20..0x7F).contains(&b) { b } else { b'?' };
            n += 1;
            i += 1;
        } else {
            // 整个 UTF-8 序列折成一个 '?' (跳过后续 10xxxxxx 续字节)。
            out[n] = b'?';
            n += 1;
            i += 1;
            while i < name.len() && name[i] & 0xC0 == 0x80 {
                i += 1;
            }
        }
    }
    print(unsafe { core::str::from_utf8_unchecked(&out[..n]) });
}

/// 打印目录条目名: 有条目长名 (VFAT / ext2) 就用长名, 否则退回 8.3 短名。
fn print_entry_name(de: &vfs::DirEntry) {
    if de.long_len > 0 {
        print_long_name(&de.long[..de.long_len as usize]);
    } else {
        print_83_name(&de.name);
    }
}

/// 把 8.3 短名格式化为 "主名[.扩展]" 并打印 (去尾随空格)。
fn print_83_name(name: &[u8; 11]) {
    let mut out = [0u8; 13];
    let mut n = 0usize;

    let base = &name[..8];
    let ext = &name[8..11];

    let mut end = 8;
    while end > 0 && base[end - 1] == b' ' {
        end -= 1;
    }
    out[n..n + end].copy_from_slice(&base[..end]);
    n += end;

    if ext[0] != b' ' {
        out[n] = b'.';
        n += 1;
        let mut eend = 3;
        while eend > 0 && ext[eend - 1] == b' ' {
            eend -= 1;
        }
        out[n..n + eend].copy_from_slice(&ext[..eend]);
        n += eend;
    }

    let s = unsafe { core::str::from_utf8_unchecked(&out[..n]) };
    print(s);
}

// ===========================================================================
// 域 9 — mount_srv (挂载管理服务)
// ===========================================================================
// docs/architecture.md「挂载与统一目录树」: 由用户态挂载服务维护全局命名空间,
// 把各文件服务的目录树拼成一个逻辑树。libvfs 在发起请求前查询本服务, 由 VFS
// 库据此完成路由 —— 应用只看到单一根 `/`。
//
// 查询回复打包为 `(服务域 << 32) | 挂载点前缀长度`: libvfs 依此去掉挂载点前缀,
// 得到发给目标文件服务的子路径。

// 挂载表是**运行时可变的**: 启动时写入引导用的默认项 (三个编译期已知的核心
// 文件服务), 之后任何服务都能通过 `MNTA` / `MNTD` 在运行时挂载 / 卸载, 无需
// 重新编译 (阶段 C3)。

/// 挂载表容量 (含自动分配出来的空闲挂载点)。
///
/// 8 个槽在「多卷挂载」(M1b) 下不够用: 引导默认项 5 个 + 每个文件服务上报的
/// 额外卷各占一个 (`/usb<卷号>`), 插一块带两个分区的 U 盘就撑满了 —— 表满时
/// 连 `MNTA` 自动分配的 `/mnt<N>` 都拿不到槽位 (FS-6 因此失败)。
const MOUNT_MAX: usize = 16;
/// 挂载点前缀最大长度 (含前导 '/', 不含结尾 NUL)。
const MOUNT_PREFIX_MAX: usize = 24;

/// 一条挂载记录: 挂载点前缀 → 文件服务域 (可选绑定一个卷)。
#[derive(Clone, Copy)]
struct MountEntry {
    used: bool,
    /// 前缀长度 (不含结尾 NUL)。
    plen: u8,
    /// 卷编码: 0 = 该服务的默认卷; 否则 = block_srv 卷号 + 1 (M1b 多卷挂载)。
    vol_enc: u32,
    domain: u64,
    prefix: [u8; MOUNT_PREFIX_MAX],
}

const MOUNT_EMPTY: MountEntry = MountEntry {
    used: false,
    plen: 0,
    vol_enc: 0,
    domain: 0,
    prefix: [0; MOUNT_PREFIX_MAX],
};

static mut MOUNTS: [MountEntry; MOUNT_MAX] = [MOUNT_EMPTY; MOUNT_MAX];

fn mount_at(i: usize) -> &'static MountEntry {
    unsafe { &*core::ptr::addr_of!(MOUNTS).cast::<MountEntry>().add(i) }
}

fn mount_at_mut(i: usize) -> &'static mut MountEntry {
    unsafe { &mut *core::ptr::addr_of_mut!(MOUNTS).cast::<MountEntry>().add(i) }
}

/// 取该记录的挂载点前缀字符串。
fn mount_prefix_of(e: &MountEntry) -> &str {
    unsafe { core::str::from_utf8_unchecked(&e.prefix[..e.plen as usize]) }
}

/// 判断 `path` 是否落在挂载点 `prefix` 下; 是则返回前缀长度。
/// 匹配须落在组件边界: `/tmpfoo` 不匹配 `/tmp`。
fn mount_prefix_match(path: &str, prefix: &str) -> Option<usize> {
    let p = path.as_bytes();
    if p.first() != Some(&b'/') {
        return None;
    }
    if prefix == "/" {
        return Some(1);
    }
    let q = prefix.as_bytes();
    if p.len() < q.len() || &p[..q.len()] != q {
        return None;
    }
    // 路径恰为挂载点, 或挂载点后紧跟 '/', 才算命中。
    if p.len() == q.len() || p[q.len()] == b'/' {
        Some(q.len())
    } else {
        None
    }
}

/// 精确查找挂载点 `prefix`, 返回槽位下标。
fn mount_find(prefix: &str) -> Option<usize> {
    for i in 0..MOUNT_MAX {
        let e = mount_at(i);
        if e.used && mount_prefix_of(e) == prefix {
            return Some(i);
        }
    }
    None
}

/// 写入一条挂载记录, 成功返回挂载槽位号 (1 起)。
///
/// `vol_enc` 为 0 表示「该服务的默认卷」(服务自己认领的那一个); 非 0 表示把该
/// 挂载点绑定到 `vol_enc - 1` 号卷 (M1b 多卷挂载), 该编码随每次请求下发。
///
/// 拒绝: 空前缀 / 非绝对路径 / 前缀过长 / 域号为 0 / 该前缀已被占用
/// (重复挂载同一前缀需先 `MNTD`, 避免静默改写别人的命名空间)。
fn mount_add(prefix: &str, domain: u64, vol_enc: u32) -> u64 {
    let b = prefix.as_bytes();
    if b.is_empty() || b[0] != b'/' || b.len() >= MOUNT_PREFIX_MAX || domain == 0 {
        return u64::MAX;
    }
    if mount_find(prefix).is_some() {
        return u64::MAX;
    }
    for i in 0..MOUNT_MAX {
        let e = mount_at_mut(i);
        if !e.used {
            e.used = true;
            e.domain = domain;
            e.vol_enc = vol_enc;
            e.plen = b.len() as u8;
            e.prefix = [0; MOUNT_PREFIX_MAX];
            e.prefix[..b.len()].copy_from_slice(b);
            return (i + 1) as u64;
        }
    }
    u64::MAX
}

/// 自动分配挂载点: 取最小的未被占用的 `/mnt<N>`, 服务默认卷。
/// 成功返回挂载槽位号 (1 起)。
fn mount_auto(domain: u64) -> u64 {
    let mut buf = [0u8; MOUNT_PREFIX_MAX];
    buf[..4].copy_from_slice(b"/mnt");
    for n in 0..MOUNT_MAX {
        buf[4] = b'0' + n as u8;
        let name = unsafe { core::str::from_utf8_unchecked(&buf[..5]) };
        if mount_find(name).is_none() {
            return mount_add(name, domain, 0);
        }
    }
    u64::MAX
}

/// 自动分配「额外卷」挂载点: `/usb<卷号>`, 并绑定该卷 (M1b 多卷挂载)。
///
/// 命名直接用卷号, 故同一台机器上多个服务挂各自的额外卷也不会撞名, 且挂载点与
/// block_srv 卷表一一对应 (`/usb3` = 卷 3)。已经挂过同一卷时静默成功 (幂等)。
fn mount_auto_vol(domain: u64, vol: u64) -> u64 {
    let mut buf = [0u8; MOUNT_PREFIX_MAX];
    buf[..4].copy_from_slice(b"/usb");
    let digits = dec_to_str(vol, &mut buf[4..]);
    let name = unsafe { core::str::from_utf8_unchecked(&buf[..4 + digits]) };
    let r = if let Some(i) = mount_find(name) {
        // 同一前缀已挂上: 只在「同域同卷」时算成功 (重复上报幂等)。
        let e = mount_at(i);
        if e.domain == domain && e.vol_enc == vfs::enc_of_vol(vol) {
            (i + 1) as u64
        } else {
            u64::MAX
        }
    } else {
        mount_add(name, domain, vfs::enc_of_vol(vol))
    };
    // 启动期诊断 (与 `mfs-dbg` / `exfat-dbg` 同类): 记下额外卷挂到了哪个前缀。
    print("mount-dbg: ");
    print(name);
    print(" domain=");
    print_u64(domain);
    print(" slot=");
    print_u64(r);
    println("");
    r
}

/// 把 `v` 的十进制写法写进 `dst`, 返回写入的字节数 (不使用堆)。
fn dec_to_str(v: u64, dst: &mut [u8]) -> usize {
    let mut tmp = [0u8; 20];
    let mut n = 0;
    let mut x = v;
    loop {
        tmp[n] = b'0' + (x % 10) as u8;
        n += 1;
        x /= 10;
        if x == 0 || n == tmp.len() {
            break;
        }
    }
    let n = n.min(dst.len());
    for i in 0..n {
        dst[i] = tmp[n - 1 - i];
    }
    n
}

/// 卸载挂载点 `prefix`, 成功返回 1。
/// 根 `/` 不可卸载 (否则整个命名空间失去根)。
fn mount_del(prefix: &str) -> u64 {
    if prefix == "/" {
        return u64::MAX;
    }
    match mount_find(prefix) {
        Some(i) => {
            mount_at_mut(i).used = false;
            1
        }
        None => u64::MAX,
    }
}

/// 写入引导用的默认挂载: 三个编译期已知的核心文件服务。
/// 其余服务一律走运行时 `MNTA`。
fn mounts_init() {
    mount_add("/", vfs::FAT32_DOMAIN, 0);
    mount_add("/tmp", vfs::TMPFS_DOMAIN, 0);
    mount_add("/mfs", vfs::MFS_DOMAIN, 0);
    mount_add("/ext2", vfs::EXT2_DOMAIN, 0);
    mount_add("/usb", vfs::EXFAT_DOMAIN, 0);
}

/// 在挂载表中查最长匹配前缀, 返回 (服务域, 卷编码, 前缀长度)。
fn mount_resolve(path: &str) -> Option<(u64, u32, usize)> {
    let mut best: Option<(u64, u32, usize)> = None;
    for i in 0..MOUNT_MAX {
        let e = mount_at(i);
        if !e.used {
            continue;
        }
        if let Some(len) = mount_prefix_match(path, mount_prefix_of(e)) {
            if best.is_none_or(|(_, _, bl)| len > bl) {
                best = Some((e.domain, e.vol_enc, len));
            }
        }
    }
    best
}

/// 域 9 — 挂载服务: 处理路由查询 (`MNTQ`) 与运行时挂载 / 卸载 (`MNTA` / `MNTD`)。
fn mount_main() {
    mounts_init();
    let mut msg = Message {
        from: 0,
        to: 0,
        tag: 0,
        payload: [0; PAYLOAD_LEN],
    };
    loop {
        sys_recv_msg(&mut msg as *mut Message as *mut u8);
        match msg.tag {
            vfs::VFS_LOOKUP_TAG => {
                let len = msg
                    .payload
                    .iter()
                    .position(|&b| b == 0)
                    .unwrap_or(PAYLOAD_LEN);
                let path = unsafe { core::str::from_utf8_unchecked(&msg.payload[..len]) };
                // 回复布局: `[63:40] 卷编码 | [39:32] 服务域 | [31:0] 前缀长度`
                // (libvfs 按同布局解出, 见 `vfs::mount_lookup`)。
                let r = match mount_resolve(path) {
                    Some((domain, vol_enc, prefix_len)) => {
                        ((vol_enc as u64) << 40) | (domain << 32) | prefix_len as u64
                    }
                    None => u64::MAX,
                };
                sys_reply(r);
            }
            vfs::VFS_MOUNT_TAG => {
                // payload = MountReq { domain, prefix[24] }; 前缀为空则自动分配。
                let req = unsafe { &*(msg.payload.as_ptr() as *const vfs::MountReq) };
                let plen = req
                    .prefix
                    .iter()
                    .position(|&b| b == 0)
                    .unwrap_or(vfs::MOUNT_PREFIX_MAX);
                let r = if plen == 0 {
                    mount_auto(req.domain)
                } else {
                    let prefix = unsafe { core::str::from_utf8_unchecked(&req.prefix[..plen]) };
                    mount_add(prefix, req.domain, 0)
                };
                sys_reply(r);
            }
            vfs::VFS_MOUNT_VOL_TAG => {
                // payload = MountVolReq { domain, vol }: 把某服务的额外卷挂到 `/usb<卷号>`。
                let req = unsafe { &*(msg.payload.as_ptr() as *const vfs::MountVolReq) };
                sys_reply(mount_auto_vol(req.domain, req.vol));
            }
            vfs::VFS_UMOUNT_TAG => {
                let len = msg
                    .payload
                    .iter()
                    .position(|&b| b == 0)
                    .unwrap_or(PAYLOAD_LEN);
                let prefix = unsafe { core::str::from_utf8_unchecked(&msg.payload[..len]) };
                sys_reply(mount_del(prefix));
            }
            _ => {
                sys_reply(u64::MAX);
            }
        }
    }
}

// ===========================================================================
// 域 10 — tmpfs_srv (内存文件系统)
// ===========================================================================
// 阶段 C2: 纯内存文件系统, 挂载于 `/tmp`, 与 fat32_srv 共存构成统一目录树,
// 用于验证「多文件服务 + 挂载层路由」。
//
// 存储模型: 平铺节点表 (绝对路径 → 节点) + 字节区; 目录语义由「父路径」关系表达
// (节点 `/A/B` 的父为 `/A`, 根 `/` 预置)。名称统一转大写并限定为 8.3 短名, 与
// libvfs 的 `DirEntry` (11 字节定长) 及 FAT 的大小写不敏感语义保持一致。

/// 节点数量上限。
const TMP_MAX_NODES: usize = 32;
/// 规范化后路径的最大长度。
///
/// 取值 = 单条 IPC payload 的长度: 客户端把绝对路径编码进 payload, 故服务端路径
/// 缓冲按此上限即可覆盖任何合法请求 (tmpfs 与 MFS 的 fd 表都用它)。
const TMP_PATH_MAX: usize = PAYLOAD_LEN;
/// 文件数据区总容量 (字节)。
const TMP_DATA_CAP: usize = 32 * 1024;
/// 打开文件数上限。
const TMP_MAX_FD: usize = 16;

#[derive(Clone, Copy)]
struct TmpNode {
    used: bool,
    is_dir: bool,
    path_len: u8,
    /// 文件逻辑大小。
    size: u32,
    /// 数据区分配容量 (0 = 尚未分配)。
    cap: u32,
    /// 数据区起始偏移。
    data_off: u32,
    path: [u8; TMP_PATH_MAX],
}

const TMP_NODE_EMPTY: TmpNode = TmpNode {
    used: false,
    is_dir: false,
    path_len: 0,
    size: 0,
    cap: 0,
    data_off: 0,
    path: [0; TMP_PATH_MAX],
};

static mut TMP_NODES: [TmpNode; TMP_MAX_NODES] = [TMP_NODE_EMPTY; TMP_MAX_NODES];
static mut TMP_DATA: [u8; TMP_DATA_CAP] = [0; TMP_DATA_CAP];
static mut TMP_DATA_USED: usize = 0;

#[derive(Clone, Copy)]
struct TmpFd {
    used: bool,
    node: u32,
}

const TMP_FD_EMPTY: TmpFd = TmpFd {
    used: false,
    node: 0,
};

static mut TMP_FDS: [TmpFd; TMP_MAX_FD] = [TMP_FD_EMPTY; TMP_MAX_FD];

fn tmp_node_at(i: usize) -> &'static TmpNode {
    unsafe { &*core::ptr::addr_of!(TMP_NODES).cast::<TmpNode>().add(i) }
}

fn tmp_node_at_mut(i: usize) -> &'static mut TmpNode {
    unsafe { &mut *core::ptr::addr_of_mut!(TMP_NODES).cast::<TmpNode>().add(i) }
}

fn tmp_data_ptr(off: usize) -> *mut u8 {
    unsafe { core::ptr::addr_of_mut!(TMP_DATA).cast::<u8>().add(off) }
}

/// 把一个路径分量规整为 8.3 短名 (转大写)。非法 (空主名 / 主名>8 / 扩展>3 /
/// 多个 '.') 返回 None。
fn tmp_norm_component(seg: &[u8], out: &mut [u8]) -> Option<usize> {
    let mut dot: Option<usize> = None;
    for (i, &b) in seg.iter().enumerate() {
        if b == b'.' {
            if dot.is_some() {
                return None;
            }
            dot = Some(i);
        }
    }
    let (base, ext) = match dot {
        Some(i) => (&seg[..i], &seg[i + 1..]),
        None => (seg, &seg[seg.len()..]),
    };
    if base.is_empty() || base.len() > 8 || ext.len() > 3 {
        return None;
    }
    let mut n = 0usize;
    for &b in base {
        out[n] = b.to_ascii_uppercase();
        n += 1;
    }
    if !ext.is_empty() {
        out[n] = b'.';
        n += 1;
        for &b in ext {
            out[n] = b.to_ascii_uppercase();
            n += 1;
        }
    }
    Some(n)
}

/// 规范化绝对路径: 逐分量规整为 8.3 并转大写, 处理 "." / ".." 与重复 '/'。
/// 结果以 '/' 开头且无尾随 '/' (根为 "/")。返回长度。
fn tmp_normalize(path: &str, out: &mut [u8]) -> Option<usize> {
    let bytes = path.as_bytes();
    if bytes.first() != Some(&b'/') || out.is_empty() {
        return None;
    }
    let mut n = 0usize;
    out[n] = b'/';
    n += 1;
    let mut i = 1usize;
    while i < bytes.len() {
        if bytes[i] == b'/' {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && bytes[i] != b'/' {
            i += 1;
        }
        let seg = &bytes[start..i];
        if seg == b"." {
            continue;
        }
        if seg == b".." {
            if n > 1 {
                let mut k = n - 1;
                while k > 0 && out[k - 1] != b'/' {
                    k -= 1;
                }
                n = if k > 1 { k - 1 } else { 1 };
            }
            continue;
        }
        let mut comp = [0u8; 12];
        let clen = tmp_norm_component(seg, &mut comp)?;
        if n > 1 {
            if n + 1 > out.len() {
                return None;
            }
            out[n] = b'/';
            n += 1;
        }
        if n + clen > out.len() {
            return None;
        }
        out[n..n + clen].copy_from_slice(&comp[..clen]);
        n += clen;
    }
    Some(n)
}

/// 按规范化路径查找节点索引。
fn tmp_find(path: &[u8]) -> Option<usize> {
    for i in 0..TMP_MAX_NODES {
        let nd = tmp_node_at(i);
        if nd.used && &nd.path[..nd.path_len as usize] == path {
            return Some(i);
        }
    }
    None
}

/// 取路径的父路径 (写入 `out`), 返回长度。根 "/" 的父仍为 "/"。
fn tmp_parent(path: &[u8], out: &mut [u8]) -> usize {
    let mut n = path.len();
    while n > 1 && path[n - 1] != b'/' {
        n -= 1;
    }
    let mut m = n;
    while m > 1 && path[m - 1] == b'/' {
        m -= 1;
    }
    if m == 0 {
        m = 1;
    }
    out[..m].copy_from_slice(&path[..m]);
    m
}

/// 分配一个新节点 (路径已规范化), 表满返回 None。
fn tmp_alloc_node(path: &[u8], is_dir: bool) -> Option<usize> {
    if path.is_empty() || path.len() > TMP_PATH_MAX {
        return None;
    }
    for i in 0..TMP_MAX_NODES {
        let nd = tmp_node_at_mut(i);
        if !nd.used {
            nd.used = true;
            nd.is_dir = is_dir;
            nd.path_len = path.len() as u8;
            nd.size = 0;
            nd.cap = 0;
            nd.data_off = 0;
            nd.path[..path.len()].copy_from_slice(path);
            return Some(i);
        }
    }
    None
}

/// 目录 `dir` 是否为空 (无任何其它节点以它为父)。
fn tmp_dir_empty(dir: &[u8]) -> bool {
    let mut parent = [0u8; TMP_PATH_MAX];
    for i in 0..TMP_MAX_NODES {
        let nd = tmp_node_at(i);
        if !nd.used {
            continue;
        }
        let p = &nd.path[..nd.path_len as usize];
        if p == dir {
            continue;
        }
        let plen = tmp_parent(p, &mut parent);
        if &parent[..plen] == dir {
            return false;
        }
    }
    true
}

/// 取路径的最后分量, 填为 11 字节 8.3 短名 (主名 8 + 扩展 3, 空格填充)。
fn tmp_name_83(path: &[u8], out: &mut [u8; 11]) {
    *out = [b' '; 11];
    let mut k = path.len();
    while k > 1 && path[k - 1] != b'/' {
        k -= 1;
    }
    let seg = &path[k..];
    let mut dot = seg.len();
    for (i, &b) in seg.iter().enumerate() {
        if b == b'.' {
            dot = i;
            break;
        }
    }
    let base = &seg[..dot];
    let ext = if dot < seg.len() {
        &seg[dot + 1..]
    } else {
        &seg[seg.len()..]
    };
    let bn = base.len().min(8);
    out[..bn].copy_from_slice(&base[..bn]);
    let en = ext.len().min(3);
    out[8..8 + en].copy_from_slice(&ext[..en]);
}

/// 从数据区分配 `need` 字节 (只增不回收), 空间不足返回 None。
fn tmp_data_alloc(need: usize) -> Option<usize> {
    unsafe {
        let used = *core::ptr::addr_of!(TMP_DATA_USED);
        if used + need > TMP_DATA_CAP {
            return None;
        }
        *core::ptr::addr_of_mut!(TMP_DATA_USED) = used + need;
        Some(used)
    }
}

fn tmp_fd_alloc(node: usize) -> u64 {
    for i in 0..TMP_MAX_FD {
        unsafe {
            let slot = &mut *core::ptr::addr_of_mut!(TMP_FDS).cast::<TmpFd>().add(i);
            if !slot.used {
                slot.used = true;
                slot.node = node as u32;
                return i as u64;
            }
        }
    }
    u64::MAX
}

fn tmp_fd_node(fd: u32) -> Option<usize> {
    if fd as usize >= TMP_MAX_FD {
        return None;
    }
    unsafe {
        let slot = &*core::ptr::addr_of!(TMP_FDS)
            .cast::<TmpFd>()
            .add(fd as usize);
        if slot.used {
            Some(slot.node as usize)
        } else {
            None
        }
    }
}

fn tmp_fd_free(fd: u32) -> u64 {
    if fd as usize >= TMP_MAX_FD {
        return 0;
    }
    unsafe {
        let slot = &mut *core::ptr::addr_of_mut!(TMP_FDS)
            .cast::<TmpFd>()
            .add(fd as usize);
        if slot.used {
            slot.used = false;
            1
        } else {
            0
        }
    }
}

/// 域 10 — tmpfs 服务: 处理与 fat32_srv 相同的 VFS 协议 (open/read/write/...)。
fn tmpfs_main() {
    // 预置根目录节点 "/"。
    if tmp_alloc_node(b"/", true).is_none() {
        println("tmpfs: init root FAILED");
        return;
    }

    let mut msg = Message {
        from: 0,
        to: 0,
        tag: 0,
        payload: [0; PAYLOAD_LEN],
    };
    let mut canon = [0u8; TMP_PATH_MAX];
    loop {
        sys_recv_msg(&mut msg as *mut Message as *mut u8);
        // 卷编码 (tag 高位) 对本服务无意义 (tmpfs 没有卷概念), 分发前剥掉。
        match vfs::tag_body(msg.tag) {
            vfs::VFS_OPEN_TAG => {
                let len = msg
                    .payload
                    .iter()
                    .position(|&b| b == 0)
                    .unwrap_or(PAYLOAD_LEN);
                let path = unsafe { core::str::from_utf8_unchecked(&msg.payload[..len]) };
                let fd = match tmp_normalize(path, &mut canon) {
                    Some(n) => match tmp_find(&canon[..n]) {
                        Some(idx) => tmp_fd_alloc(idx),
                        None => u64::MAX,
                    },
                    None => u64::MAX,
                };
                sys_reply(fd);
            }
            vfs::VFS_READ_TAG => {
                let req: vfs::ReadReq = unsafe {
                    core::ptr::read_unaligned(msg.payload.as_ptr() as *const vfs::ReadReq)
                };
                // tmpfs 节点 size 是 u32 (存储 32 KiB), 协议 offset 超出 u32 直接失败。
                if req.offset > u32::MAX as u64 {
                    sys_reply(u64::MAX);
                    continue;
                }
                let offset = req.offset as u32;
                let n = match tmp_fd_node(req.fd) {
                    Some(idx) => {
                        let nd = tmp_node_at(idx);
                        if nd.is_dir {
                            u64::MAX
                        } else if offset >= nd.size {
                            0
                        } else {
                            let cnt = (req.count).min(nd.size - offset) as usize;
                            unsafe {
                                core::ptr::copy_nonoverlapping(
                                    tmp_data_ptr(nd.data_off as usize + offset as usize),
                                    req.buf as *mut u8,
                                    cnt,
                                );
                            }
                            cnt as u64
                        }
                    }
                    None => u64::MAX,
                };
                sys_reply(n);
            }
            vfs::VFS_WRITE_TAG => {
                let req: vfs::WriteReq = unsafe {
                    core::ptr::read_unaligned(msg.payload.as_ptr() as *const vfs::WriteReq)
                };
                if req.offset > u32::MAX as u64 {
                    sys_reply(u64::MAX);
                    continue;
                }
                let offset = req.offset as u32;
                let n = match tmp_fd_node(req.fd) {
                    Some(idx) if !tmp_node_at(idx).is_dir => {
                        let end = offset as usize + req.count as usize;
                        let (mut off, mut cap) = {
                            let nd = tmp_node_at(idx);
                            (nd.data_off as usize, nd.cap as usize)
                        };
                        if end > cap {
                            if let Some(o) = tmp_data_alloc(end) {
                                if cap > 0 {
                                    unsafe {
                                        core::ptr::copy_nonoverlapping(
                                            tmp_data_ptr(off),
                                            tmp_data_ptr(o),
                                            cap,
                                        );
                                    }
                                }
                                off = o;
                                cap = end;
                            }
                        }
                        if cap < end {
                            u64::MAX // 数据区已满
                        } else {
                            unsafe {
                                core::ptr::copy_nonoverlapping(
                                    req.buf as *const u8,
                                    tmp_data_ptr(off + offset as usize),
                                    req.count as usize,
                                );
                            }
                            let nd = tmp_node_at_mut(idx);
                            nd.data_off = off as u32;
                            nd.cap = cap as u32;
                            if end as u32 > nd.size {
                                nd.size = end as u32;
                            }
                            req.count as u64
                        }
                    }
                    _ => u64::MAX,
                };
                sys_reply(n);
            }
            vfs::VFS_READDIR_TAG => {
                let req: vfs::DirReq = unsafe {
                    core::ptr::read_unaligned(msg.payload.as_ptr() as *const vfs::DirReq)
                };
                let n = match tmp_fd_node(req.fd) {
                    Some(idx) if tmp_node_at(idx).is_dir => {
                        let mut dir = [0u8; TMP_PATH_MAX];
                        let dlen = {
                            let nd = tmp_node_at(idx);
                            let d = nd.path_len as usize;
                            dir[..d].copy_from_slice(&nd.path[..d]);
                            d
                        };
                        let entry_size = core::mem::size_of::<vfs::DirEntry>();
                        let out = req.buf as *mut vfs::DirEntry;
                        let mut parent = [0u8; TMP_PATH_MAX];
                        let mut count = 0usize;
                        for i in 0..TMP_MAX_NODES {
                            let child = tmp_node_at(i);
                            if !child.used {
                                continue;
                            }
                            let cp = &child.path[..child.path_len as usize];
                            if cp == &dir[..dlen] {
                                continue; // 自身
                            }
                            let plen = tmp_parent(cp, &mut parent);
                            if plen != dlen || parent[..plen] != dir[..dlen] {
                                continue;
                            }
                            // 结果页只有一页: 放不下就停在已写入的条目上。
                            if count + 1 > vfs::RESULT_MAX_ENTRIES {
                                break;
                            }
                            let mut e = vfs::DirEntry::short(
                                [0u8; 11],
                                if child.is_dir { 0 } else { child.size as u64 },
                                if child.is_dir { 1 } else { 0 },
                            );
                            tmp_name_83(cp, &mut e.name);
                            unsafe {
                                core::ptr::write_unaligned(out.add(count), e);
                            }
                            count += 1;
                        }
                        (count * entry_size) as u64
                    }
                    _ => u64::MAX,
                };
                sys_reply(n);
            }
            vfs::VFS_CLOSE_TAG => {
                let fd = read_u32(msg.payload.as_ptr());
                sys_reply(tmp_fd_free(fd));
            }
            vfs::VFS_CREAT_TAG => {
                let len = msg
                    .payload
                    .iter()
                    .position(|&b| b == 0)
                    .unwrap_or(PAYLOAD_LEN);
                let path = unsafe { core::str::from_utf8_unchecked(&msg.payload[..len]) };
                let fd = match tmp_normalize(path, &mut canon) {
                    Some(n) => {
                        if let Some(idx) = tmp_find(&canon[..n]) {
                            if tmp_node_at(idx).is_dir {
                                u64::MAX
                            } else {
                                tmp_fd_alloc(idx)
                            }
                        } else {
                            let mut parent = [0u8; TMP_PATH_MAX];
                            let plen = tmp_parent(&canon[..n], &mut parent);
                            match tmp_find(&parent[..plen]) {
                                Some(pidx) if tmp_node_at(pidx).is_dir => {
                                    match tmp_alloc_node(&canon[..n], false) {
                                        Some(idx) => tmp_fd_alloc(idx),
                                        None => u64::MAX,
                                    }
                                }
                                _ => u64::MAX,
                            }
                        }
                    }
                    None => u64::MAX,
                };
                sys_reply(fd);
            }
            vfs::VFS_MKDIR_TAG => {
                let len = msg
                    .payload
                    .iter()
                    .position(|&b| b == 0)
                    .unwrap_or(PAYLOAD_LEN);
                let path = unsafe { core::str::from_utf8_unchecked(&msg.payload[..len]) };
                let r = match tmp_normalize(path, &mut canon) {
                    Some(n) if n > 1 && tmp_find(&canon[..n]).is_none() => {
                        let mut parent = [0u8; TMP_PATH_MAX];
                        let plen = tmp_parent(&canon[..n], &mut parent);
                        match tmp_find(&parent[..plen]) {
                            Some(pidx)
                                if tmp_node_at(pidx).is_dir
                                    && tmp_alloc_node(&canon[..n], true).is_some() =>
                            {
                                1
                            }
                            _ => u64::MAX,
                        }
                    }
                    _ => u64::MAX,
                };
                sys_reply(r);
            }
            vfs::VFS_UNLINK_TAG => {
                let len = msg
                    .payload
                    .iter()
                    .position(|&b| b == 0)
                    .unwrap_or(PAYLOAD_LEN);
                let path = unsafe { core::str::from_utf8_unchecked(&msg.payload[..len]) };
                let r = match tmp_normalize(path, &mut canon) {
                    Some(n) => match tmp_find(&canon[..n]) {
                        Some(idx) if !tmp_node_at(idx).is_dir => {
                            tmp_node_at_mut(idx).used = false;
                            1
                        }
                        _ => u64::MAX,
                    },
                    None => u64::MAX,
                };
                sys_reply(r);
            }
            vfs::VFS_RMDIR_TAG => {
                let len = msg
                    .payload
                    .iter()
                    .position(|&b| b == 0)
                    .unwrap_or(PAYLOAD_LEN);
                let path = unsafe { core::str::from_utf8_unchecked(&msg.payload[..len]) };
                let r = match tmp_normalize(path, &mut canon) {
                    Some(n) if n > 1 => match tmp_find(&canon[..n]) {
                        Some(idx) if tmp_node_at(idx).is_dir && tmp_dir_empty(&canon[..n]) => {
                            tmp_node_at_mut(idx).used = false;
                            1
                        }
                        _ => u64::MAX,
                    },
                    _ => u64::MAX,
                };
                sys_reply(r);
            }
            vfs::VFS_STAT_TAG => {
                let (buf, path) = parse_path_req(msg.payload.as_ptr());
                let n = match tmp_normalize(path, &mut canon) {
                    Some(n) => match tmp_find(&canon[..n]) {
                        Some(idx) => {
                            let nd = tmp_node_at(idx);
                            let st = vfs::Stat::plain(nd.size as u64, u32::from(nd.is_dir));
                            unsafe {
                                core::ptr::write_unaligned(buf as *mut vfs::Stat, st);
                            }
                            core::mem::size_of::<vfs::Stat>() as u64
                        }
                        None => u64::MAX,
                    },
                    None => u64::MAX,
                };
                sys_reply(n);
            }
            _ => {
                sys_reply(u64::MAX);
            }
        }
    }
}

// ===========================================================================
// 域 11 — mfs_srv (MorionFS, 块设备后端)
// ===========================================================================
// MorionFS (MFS) 是原创的原生文件系统, 相对 fat32/tmpfs 的差异化设计:
//
//   * 块校验: 每个 4 KiB 块带 8 字节头 (magic + CRC32), 读时校验, 损坏即拒绝,
//     避免静默损坏被上层当成正常内容。
//   * 写时复制 (COW): 任何修改都分配**新块**写入, 旧块原地保留; 从叶子一路复制
//     父目录、祖父目录直到根, 最后写超级块 —— 天然产生不可变的历史版本。
//   * 快照: 超级块内保存 {generation, root_block, alloc_next}; 因 COW 从不覆盖旧块,
//     快照创建后其目录树始终有效, 回滚只需把根指回快照的根。
//   * 超级块 A/B 双副本 + generation: 交替写入, 挂载时取 CRC 有效且代际更高者,
//     掉电只损坏一份仍可挂载。
//   * 自动格式化: 两份超级块都无效 (空白盘) 时, 首次挂载即格式化。
//
// 一对一节点: 每个 4 KiB 块 = 一个节点 (目录/文件) 或一个数据块。
// 块 0/1 = 超级块 A/B; 块 2 起为 COW 分配区 (只增不回收, 空间保留给快照)。
//
// 统一块布局: [ magic u32 | crc32 u32 | payload 4088 ]
//   目录 payload: nentries u32 | pad u32 | entries[170] { name[16] | block u32 | type u32 }
//   文件 payload: size u64 | nblocks u32 | pad u32 | 直接指针 + 一/二/三级间接指针
//   数据 payload: 文件字节 (每块最多 4088 字节)
//
// 名称沿用 8.3 短名 (转大写), 与 libvfs 的 DirEntry ABI 及 shell 显示一致。

const MFS_BLOCK: usize = 4096;
const MFS_SECTORS_PER_BLOCK: u16 = (MFS_BLOCK / 512) as u16;
const MFS_HDR: usize = 8;
const MFS_PAYLOAD: usize = MFS_BLOCK - MFS_HDR;

/// 卷号回退值: MFS 空白盘没有 magic, 卷层探测不到时按约定认领卷 1
/// (对应 Makefile 的 `build/mfs.img`, 即 namespace 2)。
const MFS_VOL_FALLBACK: u64 = 1;
/// MFS 服务**主卷**号 (挂在 `/mfs`), 启动时由 `vol_claim` 认领 (见 `mfs_main`)。
static mut MFS_VOL: u64 = MFS_VOL_FALLBACK;
/// 主卷的容量 (扇区数, 启动时从卷表取); 0 = 未知。
/// 首次格式化按它决定文件系统大小 —— 不再假设「盘就是 16 MiB」。
static mut MFS_VOL_SECTORS: u32 = 0;
/// **当前卷**号 (M1b 多卷挂载): 本服务此刻正在服务的卷。
///
/// MFS 的内存态 (位图 / inode 表 / 快照 / 各游标) 只有一份, 对应**一个**卷, 故
/// 只能串行服务多卷: 请求带的卷号与当前卷不同时, 先把新卷的超级块载回内存态
/// (见 `mfs_switch_vol`) —— 之所以能这样切, 是因为每次改动都会随超级块写盘,
/// 请求边界上盘上状态总是自洽的。
static mut MFS_CUR_VOL: u64 = MFS_VOL_FALLBACK;
/// 当前卷的容量 (扇区数); 0 = 未知。格式化尺寸与挂载时的容量校验都按它算。
static mut MFS_CUR_SECTORS: u32 = 0;
/// 卷容量未知时的兜底总块数 (16 MiB / 4 KiB), 与 Makefile 的默认 `MFS_MIB=16` 对应。
/// 仅用于首次格式化; 之后以超级块记录的值为准。
const MFS_DEFAULT_TOTAL_BLOCKS: u32 = 4096;
/// 首次格式化的**下限** (256 KiB): 卷再小也得放得下两份超级块 + 根目录 + inode 表。
/// 低于这个数直接格式化会得到一个连元数据都装不下的文件系统。
const MFS_MIN_TOTAL_BLOCKS: u32 = 64;
/// 超级块副本数 (块 0 / 块 1)。
const MFS_SB_COPIES: u32 = 2;
/// 超级块内快照表容量。
const MFS_MAX_SNAP: usize = 8;

/// 超级块 magic: "MFS8"（文件 size 改 u64 + 三级间接块 → 单文件上限与卷容量同量级）。
///
/// magic 携带布局修订: `MFS1` = 纯 COW, `MFS2` = +空闲位图, `MFS3` = +文件间接块,
/// `MFS4` = +变长目录项/多块目录, `MFS5` = +节点元数据（目录块头部 8 → 48）,
/// `MFS6` = +inode 表（目录项改存 inode 号，块号经表映射）,
/// `MFS7` = +位图外置（位图改为独立数据块 + 头块，超级块 payload 不再内联位图）,
/// `MFS8` = +文件 size u64 +三级间接块（缩直接区 4 字节腾出 ind3 槽位，元数据偏移不变）。
/// 旧修订缺少新布局所需的字段/语义, 挂载时一律视为无效 -> 自动重新格式化
/// (卷层仍同时认各修订, 见 `vol_detect_kind`)。
const MFS_MAGIC_SUPER: u32 = 0x4D46_5338; // "MFS8"
/// 超级块内的格式版本 (magic 之外的二次校验)。
const MFS_VERSION: u32 = 8;
/// 位图头块 magic: "MFBH"（记录 gen / 总块数 / 位图数据块数 + 各数据块 CRC32）。
const MFS_MAGIC_BMPHDR: u32 = 0x4D46_4248; // "MFBH"
/// 目录块 (base 节点或扩展块): ext + 元数据 + 变长条目区。
const MFS_MAGIC_DIR: u32 = 0x4D46_4449; // "MFDI"
/// 目录扩展索引块 (槽位全是扩展目录块指针)。
const MFS_MAGIC_DIDX: u32 = 0x4D46_5849; // "MFXI"
const MFS_MAGIC_FILE: u32 = 0x4D46_464C; // "MFFL"
/// 软链接节点 (M5c): 目标路径内联存在节点 payload 里, 不占数据块。
const MFS_MAGIC_LINK: u32 = 0x4D46_534C; // "MFSL"
const MFS_MAGIC_DATA: u32 = 0x4D46_4441; // "MFDA"
/// 一级间接块 (槽位全是数据块指针)。
const MFS_MAGIC_IND: u32 = 0x4D46_494E; // "MFIN"
/// 二级间接块 (槽位全是一级间接块指针)。
const MFS_MAGIC_IND2: u32 = 0x4D46_4932; // "MFI2"
/// 三级间接块 (槽位全是二级间接块指针)。
const MFS_MAGIC_IND3: u32 = 0x4D46_4933; // "MFI3"

// 节点元数据: 文件与目录布局相同, 只是所在偏移不同 (目录紧跟 ext 之后, 文件在 inode
// 尾部保留区)。三种时间都是 Unix 秒 (由 CMOS RTC 提供)。
//
//   +0 mode(u16) / +2 owner(u16) / +4 nlink(u32)
//   +8 mtime(u64) / +16 ctime(u64) / +24 atime(u64) / +32 reserved(u64)
//
// `owner` 记创建者域 id (没有多用户概念, 故不做 uid/gid); `mode` 只存储与显示,
// **不做强制检查** (见 docs/roadmap-fs.md M5)。`atime` 不随读更新 —— 否则每次读都要
// COW 整个 inode 并上溯到根, 读路径会退化成写路径。
const MFS_META_LEN: usize = 40;
const MFS_META_MODE: usize = 0;
const MFS_META_OWNER: usize = 2;
const MFS_META_NLINK: usize = 4;
const MFS_META_MTIME: usize = 8;
const MFS_META_CTIME: usize = 16;
const MFS_META_ATIME: usize = 24;

/// 新建目录的默认权限 (rwxr-xr-x)。
const MFS_MODE_DIR: u16 = 0o755;
/// 新建文件的默认权限 (rw-r--r--)。
const MFS_MODE_FILE: u16 = 0o644;
/// 新建软链接的默认权限 (rwxrwxrwx; 与 Unix 一致, 链接自身的权限位无意义)。
const MFS_MODE_LINK: u16 = 0o777;
/// 权限位掩码 (只保留低 12 位: setuid/setgid/sticky + rwxrwxrwx)。
const MFS_MODE_MASK: u16 = 0o7777;

// `mode` 的高 4 位是**节点类型** (与 ext2 `i_mode` 的 S_IFMT 同构), 低 12 位是权限。
//
// 为什么要它: 目录/文件之外多了软链接, 而 `vfs::Stat` / `vfs::DirEntry` 只有
// `is_dir` 一个类型信号 —— 光看它区分不出「普通文件」与「软链接」。把类型编码进
// 已经存在的 `mode` 字段即可让 `ls -l` / `stat` 显示 `l`, 不必改协议结构体。
// 非 MFS 的文件服务不填类型位 (mode 只有权限), 客户端按 `is_dir` 回退显示。
const MFS_FTYPE_MASK: u16 = 0xF000;
const MFS_FTYPE_FILE: u16 = 0x8000;
const MFS_FTYPE_DIR: u16 = 0x4000;
const MFS_FTYPE_LINK: u16 = 0xA000;

// 目录块 payload 布局: +0 ext(扩展索引块号, 0 = 无) / +4 pad / +8 元数据(40) / +48 起条目区。
//
// 条目按 rec_len 串联 (ext2 风格), 每项 4 字节对齐:
//   +0 block(u32) / +4 type(u8) / +5 name_len(u8) / +6 rec_len(u16) / +8 name
// `name_len == 0` 表示空槽; 空槽与"条目的尾部余量"都可被后续插入复用, 删除时把
// 释放的长度并给前一项以回收碎片。名字上限 255 字节 (name_len 是 u8)。
const MFS_DIR_HDR: usize = 8 + MFS_META_LEN;
/// 条目区字节数。
const MFS_DIR_AREA: usize = MFS_PAYLOAD - MFS_DIR_HDR;
/// 条目头长度。
const MFS_DIR_ENT_HDR: usize = 8;
/// 最小条目长度 (头 + 1 字节名字, 向上取 4 的倍数)。
const MFS_DIR_ENT_MIN: usize = 12;
/// 名字长度上限 (磁盘格式能力; 端到端受 IPC payload 限制, 见 docs/roadmap-fs.md M4)。
const MFS_NAME_MAX: usize = 255;
/// 扩展索引块的槽位数 (整个 payload 都是扩展目录块指针)。
const MFS_DIR_SLOTS: usize = MFS_PAYLOAD / 4;

// 文件块 payload 布局 (MFS8):
//   +0 size(u64) / +8 nblocks(u32) / +12 pad(u32) / +16 起 1005 个直接块指针
//   / 其后 一级 + 二级 + 三级间接指针 / 末尾 40 字节保留 (预留给元数据)。
//
// 逻辑块索引 (`bi`) 到物理块的映射分四段: 直接区 -> 一级 -> 二级 -> **三级**间接区;
// 每段容量见下。合计 `MFS_FILE_MAX_BLOCKS` 已远超位图能描述的块数, 故单文件上限实际
// 等于整卷可用块数 (即单文件与卷容量同量级; MFS7 起位图外置, ≈127.25 GiB)。
/// inode 内的直接块指针数 (直接区覆盖 ≈3.9 MiB, 小文件不产生额外 I/O)。
///
/// 由 1008 缩到 1005: size 由 u32 变 u64 (+4 字节)、新增 4 字节 pad (+4) 各让出
/// 一个槽位, 再把 `MFS_FILE_IND3_OFF` 需要的 4 字节腾出来 —— 合计缩掉 3 个指针
/// (12 字节), 使三个间接指针仍结束于 4056, 元数据偏移保持不变。
const MFS_FILE_DIRECT: usize = 1005;
/// 文件大小 (u64) 在块内的偏移。
const MFS_FILE_SIZE_OFF: usize = MFS_HDR;
/// 已分配逻辑块数 (u32) 在块内的偏移。
const MFS_FILE_NBLOCKS_OFF: usize = MFS_HDR + 8;
/// 直接块指针区在块内的起点。
const MFS_FILE_DIRECT_OFF: usize = MFS_HDR + 16;
/// 一级间接块指针在 inode 中的偏移。
const MFS_FILE_IND1_OFF: usize = MFS_FILE_DIRECT_OFF + MFS_FILE_DIRECT * 4;
/// 二级间接块指针在 inode 中的偏移。
const MFS_FILE_IND2_OFF: usize = MFS_FILE_IND1_OFF + 4;
/// 三级间接块指针在 inode 中的偏移 (缩直接区腾出的槽位)。
const MFS_FILE_IND3_OFF: usize = MFS_FILE_IND2_OFF + 4;
/// inode 保留区起点 (留给元数据: 时间戳 / 权限 / 链接数); 必须仍为 4056。
const MFS_FILE_RESERVED_OFF: usize = MFS_FILE_IND3_OFF + 4;
/// inode 保留区字节数。
const MFS_FILE_RESERVED: usize = MFS_BLOCK - MFS_FILE_RESERVED_OFF;

// 软链接块 payload 布局 (M5c) —— **沿用文件布局**, 这样所有元数据读写函数用
// `is_dir = false` 就能直接作用于软链接, 不必给它们再加一种节点类型分支:
//
//   +0 size(u64)  ← 复用文件的大小字段, 这里存**目标路径字节数** (`lstat` 的 size)
//   +8 起         ← 目标路径字节 (UTF-8, 无结尾 NUL), 见 `MFS_LINK_TARGET_OFF`
//                    (紧接 u64 size 之后, 顺带覆盖未用的 nblocks / 直接指针区)
//   ...
//   +MFS_FILE_RESERVED_OFF 起 40 字节元数据 (与文件同偏移)
//
// 目标内联在节点里 (fast symlink), 不占数据块 —— 软链接不参与硬链接, nlink 恒为 1。
/// 软链接目标路径在节点 payload 里的起始偏移 (紧接 u64 size 之后)。
const MFS_LINK_TARGET_OFF: usize = MFS_HDR + 8;
/// 软链接目标路径长度上限 (实际还受单条 IPC 路径长度约束, 见 `MFS_PATH_MAX`)。
const MFS_LINK_MAX: usize = MFS_FILE_RESERVED_OFF - MFS_LINK_TARGET_OFF;
/// 布局自检: 直接指针区 + 三个间接指针 + 保留区正好铺满一个 4 KiB 块。
const _: () = assert!(MFS_FILE_RESERVED >= 40);
/// 每个间接块的指针槽数 (整个 payload 都是指针)。
const MFS_IND_CAP: usize = MFS_PAYLOAD / 4;
/// 单个文件的逻辑块上限 = 直接 + 一级 + 二级 + 三级容量。
///
/// 三级槽数 (1022³ ≈ 1.07e9) 在 usize 上算, 避免中间量溢出。
const MFS_FILE_MAX_BLOCKS: usize = MFS_FILE_DIRECT
    + MFS_IND_CAP
    + MFS_IND_CAP * MFS_IND_CAP
    + MFS_IND_CAP * MFS_IND_CAP * MFS_IND_CAP;
/// 单个数据块可存放的文件字节数。
const MFS_DATA_CAP: usize = MFS_PAYLOAD;
// 路径解析链最大深度 / 打开文件上限。
// 深度上限按「单条 IPC 路径最长 95 字节、最短分量 1 字符 + '/'」估算 (≈47 级), 取 48;
// M4 起目录可任意嵌套 (无额外结构限制), 限制只来自路径编码长度。
const MFS_MAX_DEPTH: usize = 48;
/// 解析路径的工作缓冲大小 (与单条 IPC 路径上限一致)。
///
/// 跟随软链接时会把「目标 + 剩余分量」重新组装成一条路径再解析, 组装结果也受这个
/// 上限约束 —— 超长则解析失败 (返回 None), 而不是截断成一条错路径。
const MFS_PATH_MAX: usize = TMP_PATH_MAX;
/// 一条路径上最多跟随多少个软链接 (防环 + 限制展开长度)。
///
/// 环 (a→b→a) 会在这里被截住并返回"解析失败", 而不是无限展开; 正常的软链接链远
/// 短于这个值。
const MFS_SYMLINK_MAX_DEPTH: u32 = 16;
const MFS_MAX_FD: usize = 16;

// 节点类型 (目录项 type 字段)。
const MFS_TYPE_FILE: u32 = 1;
const MFS_TYPE_DIR: u32 = 2;
/// 软链接 (M5c)。目录项只按这个类型区分, 具体目标在节点 payload 里。
const MFS_TYPE_LINK: u32 = 3;

// MFS6: 目录项存 **inode 号** 而不是块号, inode 号到块号的映射由一棵独立的 COW 树
// 提供 (索引块 -> 表块 -> 槽位)。这样多个目录项可以指向同一个 inode (硬链接), 而
// 修改 inode 只需更新它那一个表槽 —— 所有链接自动看到新内容。
//
//   inode 表索引块 (MFIX): payload 全是表块指针, 第 k 项 = 第 k 个表块 (0 = 未分配)
//   inode 表块 (MFIT):     payload 全是对象块指针, 第 j 项 = ino = k*SLOTS + j 的块号
//
// ino 0 保留为「无效 / 空闲」, 根目录固定在 ino 1 (它永不改名/删除, 故无需在超级块里
// 存根号)。inode 更新 = COW 对象块 + COW 表块 + COW 索引块 + 写超级块。
const MFS_MAGIC_ITAB: u32 = 0x4D46_4954; // "MFIT" inode 表块
const MFS_MAGIC_ITABX: u32 = 0x4D46_4958; // "MFIX" inode 表索引块
/// inode 表块 / 索引块的槽位数 (整个 payload 都是 u32 指针)。
const MFS_ITAB_SLOTS: usize = MFS_PAYLOAD / 4;
/// inode 号上限 (索引块 × 表块 × 每块槽数)。
const MFS_INO_MAX: u32 = (MFS_ITAB_SLOTS * MFS_ITAB_SLOTS) as u32;
/// 根目录固定的 inode 号。
const MFS_ROOT_INO: u32 = 1;

// 超级块 payload 布局 (除下列区段外均为保留):
//   +0 version / +4 block_size / +8 total_blocks / +12 ino_count / +16 alloc_hint
//   +20 snap_count / +24 gen(u64) / +32 itab_root / +36 ino_hint
//   / +48 快照表(8 × 24B) / +256 起预留
// (MFS7 起空闲位图已移出超级块, +256 那片区域留空保留 —— 见位图头块布局;
//  其中 `MFS_SB_PRIMARY` 从 +256 起占了 8 字节, 其余仍保留。)
const MFS_SB_BLOCK_SIZE: usize = 4;
const MFS_SB_TOTAL: usize = 8;
const MFS_SB_INO_COUNT: usize = 12;
const MFS_SB_ALLOC_HINT: usize = 16;
const MFS_SB_SNAP_COUNT: usize = 20;
const MFS_SB_GEN: usize = 24;
/// inode 表索引块 (MFIX) 块号。
const MFS_SB_ITAB: usize = 32;
/// inode 分配游标 (下次从这里开始找空槽)。
const MFS_SB_INO_HINT: usize = 36;
/// 快照表起点。
const MFS_SB_SNAPS: usize = 48;
/// 单条快照记录字节数: gen(u64) + itab_root/ino_hint/alloc_hint/reserved (4 × u32)。
const MFS_SNAP_REC: usize = 24;
/// **主卷序号** (u64, 0 = 不是主卷) —— `MFS7` 起 payload `+256` 整片是保留区, 这里取头 8 字节。
///
/// 一台机器上可以有多块 MFS 卷; 谁当 `/mfs` 以前只由**卷表扫描顺序**决定, 显式
/// `mkfs.mfs` 过谁完全不影响 —— 这既不可控也无法解释。改为: `mkfs.mfs` 把目标卷的
/// 序号置成「现有最大 + 1」(见 `mfs_mkfs_volume`), 认领时取**序号最大**的 MFS 卷
/// (见 `mfs_vol_claim`)。于是「最近一次显式格式化的卷」稳定地就是下次启动的主卷。
///
/// 序号只增不清: 不需要回写别的卷就能表达「我更新」, 单主卷由「最大者胜出」保证。
/// 全为 0 (老卷 / 只被首次挂载自动格式化过) 时退回「第一个 MFS 卷」的旧行为。
const MFS_SB_PRIMARY: usize = 256;
// 位图头块 payload 布局 (MFS7): +0 gen(u64) / +8 total_blocks(u32) / +12 data_blocks(u32)
//   / +16 CRC32 数组 (每个位图数据块一项)。
/// 位图头块 payload: 代际 (必须与对应超级块副本的 gen 一致)。
const MFS_BMPH_GEN: usize = 0;
/// 位图头块 payload: 总块数 (必须与超级块一致)。
const MFS_BMPH_TOTAL: usize = 8;
/// 位图头块 payload: 位图数据块数 bb (必须等于该卷的 bb)。
const MFS_BMPH_DATA_BLOCKS: usize = 12;
/// 位图头块 payload: CRC32 数组起点。
const MFS_BMPH_CRC: usize = 16;

// MFS7 盘上布局 (位图外置):
//   块 0/1        = 超级块 A/B
//   块 2/3        = 位图头 A/B (magic "MFBH")
//   块 4..4+bb    = 位图数据副本 A (裸 4096 字节, 无块头)
//   块 4+bb..4+2bb = 位图数据副本 B
//   MFS_DATA_START = 4 + 2*bb; 它之前的块一律强制标记占用, 不参与分配。
/// 一个位图数据块覆盖的块数 (4096 字节 × 8 位 = 32768 块 = 128 MiB)。
const MFS_BMP_BLOCK_SPAN: u32 = (MFS_BLOCK * 8) as u32;
/// 位图数据块数上限 (= 位图头块 payload 里 CRC 数组的容量)。
const MFS_MAX_BMP_DATA_BLOCKS: u32 = ((MFS_PAYLOAD - MFS_BMPH_CRC) / 4) as u32;
/// 位图能描述的最大块数 (1018 × 32768 = 33_357_824 块 ≈ 127.25 GiB)。
const MFS_MAX_BLOCKS: u32 = MFS_MAX_BMP_DATA_BLOCKS * MFS_BMP_BLOCK_SPAN;
/// 低水位分频: 空闲块 < `总块数 / MFS_GC_LOW_WATER_DIV` 时, 服务空闲即自动回收。
const MFS_GC_LOW_WATER_DIV: u32 = 16;
/// 回收遍历栈容量 (只压入目录块, 深度优先)。
const MFS_GC_STACK_MAX: usize = 4096;

/// MFS 块缓冲虚拟地址。
///
/// 必须放在程序镜像之外 (`USER_BASE` 起, 随代码增长) 且与其他固定区不重叠:
/// 这些页要以「同地址」共享给 block_srv 供其 DMA 写入, 若位于镜像内, 目标域
/// block_srv 自身的镜像会占住同一地址, 共享时 map_user_page 触发
/// PageAlreadyMapped。已占用区间: fat32 `+0x10_0000..0x10_4000`、
/// app/shell 共享缓冲 `+0x10_4000..0x10_8000`, 故取其后相邻 4 页。
const MFS_BUF_A_VADDR: u64 = 0x0000_0080_0010_8000;
const MFS_BUF_B_VADDR: u64 = 0x0000_0080_0010_9000;
const MFS_BUF_C_VADDR: u64 = 0x0000_0080_0010_A000;
const MFS_BUF_S_VADDR: u64 = 0x0000_0080_0010_B000;
/// GC 遍历块专用缓冲页。
///
/// GC 需要读任意块; 若复用 A/B/C/S 就必须在每个调用点单独证明它们空闲 (A/B/C 常
/// 持有 COW 在建内容, S 供超级块使用)。独立一页让 GC 与这些缓冲彻底解耦。取 ext2
/// 缓冲页之后、block_srv 卷扫描页之前的空位, 同样位于程序镜像之外。
const MFS_GC_VADDR: u64 = 0x0000_0080_0011_0000;
/// inode 表块缓存页 (单条目: 记住当前载入的表块索引, `mfs_ino_block` 用)。
const MFS_ITAB_VADDR: u64 = 0x0000_0080_0011_1000;
/// inode 表索引块 scratch 页 (只在 `mfs_itab_flush` 重建索引块时用)。
const MFS_ITABX_VADDR: u64 = 0x0000_0080_0011_2000;
/// GC 遍历时翻译 ino 用的表块缓冲页 (GC 的主体缓冲在 `MFS_GC_VADDR`, 两个不能共用)。
const MFS_GC_TAB_VADDR: u64 = 0x0000_0080_0011_3000;

// MFS7 位图窗口 (页数动态, 见 `mfs_win_ensure`)。
//
// 旧版把空闲位图 / GC 标记 / 本根已访问三个位图放在**编译期定长数组**里; 新容量上限
// 需要 ≈4.17 MB/窗口, 静态放不下 —— 改为按卷容量逐页分配的窗口。三个窗口都**只增不缩**
// (卷变小也不 sys_unmap: 窗口页共享给了 block_srv, 回收要走引用计数, 不值当)。
/// 主空闲位图窗口: 第 k 页 ↔ 该副本第 k 个位图数据块, 直接作 `block_read/write_dev`
/// 的缓冲 (免拷贝)。**必须**共享给 block_srv 供其 DMA 写入。
const MFS_BMP_VADDR: u64 = 0x0000_0080_0100_0000;
/// GC 可达标记窗口 (服务私有, 不共享)。
const MFS_MARK_VADDR: u64 = 0x0000_0080_0140_0000;
/// GC「本根已访问」窗口 (服务私有, 不共享)。
const MFS_SEEN_VADDR: u64 = 0x0000_0080_0180_0000;
/// 位图头块缓冲页 (单页)。同样共享给 block_srv —— 头块经它读入 / 写出。
const MFS_BMPH_VADDR: u64 = 0x0000_0080_0016_2000;

fn mfs_a() -> *mut u8 {
    MFS_BUF_A_VADDR as *mut u8
}
fn mfs_b() -> *mut u8 {
    MFS_BUF_B_VADDR as *mut u8
}
fn mfs_c() -> *mut u8 {
    MFS_BUF_C_VADDR as *mut u8
}
fn mfs_s() -> *mut u8 {
    MFS_BUF_S_VADDR as *mut u8
}
fn mfs_gc_buf() -> *mut u8 {
    MFS_GC_VADDR as *mut u8
}
fn mfs_itab_buf() -> *mut u8 {
    MFS_ITAB_VADDR as *mut u8
}
fn mfs_itabx_buf() -> *mut u8 {
    MFS_ITABX_VADDR as *mut u8
}
fn mfs_gc_tab_buf() -> *mut u8 {
    MFS_GC_TAB_VADDR as *mut u8
}
fn mfs_bmph_buf() -> *mut u8 {
    MFS_BMPH_VADDR as *mut u8
}

// 超级块/分配状态 (内存镜像, 与磁盘副本同步)。
/// inode 表索引块号 (根目录恒为 `MFS_ROOT_INO`)。
static mut MFS_ITAB: u32 = 0;
/// 已分配的 inode 数 (供 `MSST` 之类的统计)。
static mut MFS_INO_COUNT: u32 = 0;
/// inode 分配游标: 下次从这里开始找空槽。
static mut MFS_INO_HINT: u32 = MFS_ROOT_INO;
/// inode 表块缓存的当前表块索引 (u32::MAX = 未载入)。
static mut MFS_ITAB_CACHE_IDX: u32 = u32::MAX;
/// 分配游标 (超级块 payload +16): 下次分配优先从此块开始扫描。
static mut MFS_ALLOC_NEXT: u32 = 0;
static mut MFS_TOTAL_BLOCKS: u32 = 0;
static mut MFS_GEN: u64 = 0;
static mut MFS_SNAP_COUNT: usize = 0;
/// **当前卷**的主卷序号 (超级块 `MFS_SB_PRIMARY`; 0 = 不是主卷)。
///
/// 必须随每次提交一并写回: 否则一次普通的写盘就会把标记抹成 0, 下次启动主卷就丢了。
static mut MFS_PRIMARY_SERIAL: u64 = 0;
/// 空闲块数 (由位图窗口派生, 由 `mfs_bmp_set/clear` 增量维护)。
static mut MFS_FREE_BLOCKS: u32 = 0;
/// 三个位图窗口当前已分配的页数 (只增不缩, 见各窗口 VADDR 处的说明)。
static mut MFS_BMP_PAGES: u32 = 0;
static mut MFS_MARK_PAGES: u32 = 0;
static mut MFS_SEEN_PAGES: u32 = 0;
/// 位图数据块数 bb (当前卷; 0 = 未挂载)。
static mut MFS_BMP_DATA_BLOCKS: u32 = 0;
/// 位图数据块**脏位图**的字节数 (每位对应一个位图数据块)。
const MFS_BMP_DIRTY_BYTES: usize = (MFS_MAX_BMP_DATA_BLOCKS as usize).div_ceil(8);
/// 位图数据块的脏位图: 提交时只把变动过的区间落盘 (见 `mfs_bmp_flush`)。
static mut MFS_BMP_DIRTY: [u8; MFS_BMP_DIRTY_BYTES] = [0; MFS_BMP_DIRTY_BYTES];
/// 常驻 CRC32 数组 (每个位图数据块一项): 落盘进位图头块, 加载时逐块比对。
static mut MFS_BMP_CRC: [u32; MFS_MAX_BMP_DATA_BLOCKS as usize] =
    [0; MFS_MAX_BMP_DATA_BLOCKS as usize];
/// GC 的「本根已访问」位图 (每换一个可达根就清零; 现居 `MFS_SEEN_VADDR` 窗口)。
///
/// 必须与可达位图分开: 同一个目录块可能同时被当前树与某快照引用, 而块内条目的
/// ino 在不同根的表下会翻译成**不同**的对象块 —— 所以每个根都得重新遍历一遍。
/// 若拿可达位图当访问集去重, 快照那次就会被跳过, 快照引用的旧块漏标而被回收
/// (M5 之前目录项直接存块号, 不存在这个差异; 引入 inode 表后必须分开)。
/// GC 遍历栈 (待展开的目录块)。
static mut MFS_GC_STACK: [u32; MFS_GC_STACK_MAX] = [0; MFS_GC_STACK_MAX];
/// GC 遍历栈顶指针 (跨函数传递, 故用静态量)。
static mut MFS_GC_SP: usize = 0;

/// 快照记录: 根恒为 `MFS_ROOT_INO`, 故只需记 inode 表索引块 + 两个分配游标 + 代际
/// —— 有这四项就能完整重建当时的目录树与 inode 映射 (表块本身被 GC 视为快照可达)。
#[derive(Clone, Copy)]
struct MfsSnap {
    gen: u64,
    itab: u32,
    ino_hint: u32,
    alloc_next: u32,
}
impl MfsSnap {
    const EMPTY: MfsSnap = MfsSnap {
        gen: 0,
        itab: 0,
        ino_hint: 0,
        alloc_next: 0,
    };
}
static mut MFS_SNAPS: [MfsSnap; MFS_MAX_SNAP] = [MfsSnap::EMPTY; MFS_MAX_SNAP];

fn mfs_snap(i: usize) -> MfsSnap {
    unsafe { *core::ptr::addr_of!(MFS_SNAPS).cast::<MfsSnap>().add(i) }
}
fn mfs_set_snap(i: usize, s: MfsSnap) {
    unsafe {
        *core::ptr::addr_of_mut!(MFS_SNAPS).cast::<MfsSnap>().add(i) = s;
    }
}

/// 路径解析结果的叶子条目位置 (目录项的 ino 与条目所在块/偏移), 供删除 / 改名使用。
///
/// MFS6 起目录项存 ino, 父目录条目在对象更新时**不变**, 因此不再需要「从叶到根的
/// 完整回写链」—— 这就是 inode 间接层带来的简化: 改一个对象只需 COW 它自己 + 它的
/// 表槽, 与目录深度无关。
#[derive(Clone, Copy)]
struct MfsLoc {
    /// 条目所属目录的 inode 号 (COW 回写的锚点)。
    dir_ino: u32,
    /// 条目实际所在块 (目录节点本身, 或某个扩展目录块)。
    blk: u32,
    /// 块内偏移。
    off: usize,
}
const MFS_LOC_EMPTY: MfsLoc = MfsLoc {
    dir_ino: 0,
    blk: 0,
    off: 0,
};
/// 最近一次 `mfs_resolve` 命中的叶子条目位置 (调用方据此改/删条目)。
static mut MFS_LEAF: MfsLoc = MFS_LOC_EMPTY;

/// 打开文件描述符 (按路径而非 inode 记录: COW 后 inode 块会变, 每次操作重新解析
/// 路径即可始终指向最新版本, 避免句柄失效)。
#[derive(Clone, Copy)]
struct MfsFd {
    used: bool,
    is_dir: bool,
    path_len: u8,
    /// 打开时本服务服务的卷号 (M1b 多卷挂载: fd 类请求靠它找回该卷, 见服务循环)。
    vol: u64,
    path: [u8; TMP_PATH_MAX],
}
const MFS_FD_EMPTY: MfsFd = MfsFd {
    used: false,
    is_dir: false,
    path_len: 0,
    vol: 0,
    path: [0; TMP_PATH_MAX],
};
static mut MFS_FDS: [MfsFd; MFS_MAX_FD] = [MFS_FD_EMPTY; MFS_MAX_FD];

// ---------------------------------------------------------------------------
// 基础工具
// ---------------------------------------------------------------------------

fn mfs_at(buf: *const u8, off: usize) -> *const u8 {
    unsafe { buf.add(off) }
}
fn mfs_atm(buf: *mut u8, off: usize) -> *mut u8 {
    unsafe { buf.add(off) }
}
fn read_u64(ptr: *const u8) -> u64 {
    (read_u32(ptr) as u64) | ((read_u32(unsafe { ptr.add(4) }) as u64) << 32)
}
fn write_u64(ptr: *mut u8, v: u64) {
    write_u32(ptr, (v & 0xFFFF_FFFF) as u32);
    write_u32(unsafe { ptr.add(4) }, (v >> 32) as u32);
}

/// CRC-32 (IEEE 802.3, 多项式 0xEDB88320, 反射)。
fn mfs_crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ 0xEDB8_8320;
            } else {
                crc >>= 1;
            }
        }
    }
    !crc
}

/// 计算并写入块头 CRC (payload 已填好)。
fn mfs_seal(buf: *mut u8, magic: u32) {
    write_u32(buf, magic);
    let crc = {
        let p = unsafe { core::slice::from_raw_parts(mfs_at(buf, MFS_HDR), MFS_PAYLOAD) };
        mfs_crc32(p)
    };
    write_u32(mfs_atm(buf, 4), crc);
}

/// 校验块头 magic 与 CRC。
fn mfs_ok(buf: *const u8, magic: u32) -> bool {
    if read_u32(buf) != magic {
        return false;
    }
    let stored = read_u32(mfs_at(buf, 4));
    let p = unsafe { core::slice::from_raw_parts(mfs_at(buf, MFS_HDR), MFS_PAYLOAD) };
    mfs_crc32(p) == stored
}

fn mfs_read_blk(block_no: u32, dst: *mut u8) -> bool {
    block_read_dev(
        unsafe { MFS_CUR_VOL },
        block_no * MFS_SECTORS_PER_BLOCK as u32,
        MFS_SECTORS_PER_BLOCK,
        dst,
    )
}
fn mfs_write_blk(block_no: u32, src: *const u8) -> bool {
    block_write_dev(
        unsafe { MFS_CUR_VOL },
        block_no * MFS_SECTORS_PER_BLOCK as u32,
        MFS_SECTORS_PER_BLOCK,
        src as *mut u8,
    )
}

/// 读**任意卷** `vol` 超级块里的主卷序号; 0 = 非主卷 / 不是 MFS / 两份都读不出。
///
/// 直接用 `block_read_dev` 指定卷号, 与本服务的「当前卷」无关 —— 认领主卷要把**所有**
/// MFS 卷扫一遍, 不能靠切内存态 (那会把正在服务的卷换掉)。两份副本取较大者: 一次提交
/// 把两份写成同一序号, 崩溃在中途时落后的那份序号必不大于新的, 取大即取新。
///
/// 缓冲借用位图头块页 (`mfs_bmph_buf`) —— 该页只在 `mfs_bmp_flush` 内部使用, 而本函数
/// 只从启动认领与 `mkfs` **之前**调用, 与提交路径不重叠。
fn mfs_primary_of_vol(vol: u64) -> u64 {
    let buf = mfs_bmph_buf();
    let mut best = 0u64;
    for copy in 0..MFS_SB_COPIES {
        let lba = copy * MFS_SECTORS_PER_BLOCK as u32;
        if !block_read_dev(vol, lba, MFS_SECTORS_PER_BLOCK, buf) {
            continue;
        }
        if !mfs_ok(buf, MFS_MAGIC_SUPER) {
            continue;
        }
        let p = MFS_HDR;
        if read_u32(mfs_at(buf, p)) != MFS_VERSION {
            continue;
        }
        best = best.max(read_u64(mfs_at(buf, p + MFS_SB_PRIMARY)));
    }
    best
}

/// 目标卷 `target` 应得的主卷序号: 现有**所有** MFS 卷与 `target` 自身的最大序号 + 1。
///
/// 只加 1、不回写别的卷 —— 序号只增, 「最大者胜出」由认领端 (`mfs_vol_claim`) 保证。
/// 重复格式化同一块卷会让它继续胜出 (序号同样 +1), 正是「最近 mkfs 过的卷当主卷」。
fn mfs_next_primary_serial(scratch: *mut u8, target: u64) -> u64 {
    // 目标卷此刻的 kind 可能还是卷表里的旧值 (卷表在启动时就冻结了, 之后 mkfs 出来的
    // 卷在表里仍是 unknown), 故先按卷号直接读一次, 与卷表无关。
    let mut max = mfs_primary_of_vol(target);
    let n = block_list_volumes(scratch, VOL_MAX as u32);
    if n != 0 && n != u64::MAX {
        let esize = core::mem::size_of::<VolumeDesc>();
        let mut i = 0u64;
        while i < n {
            let d = unsafe {
                core::ptr::read_unaligned(scratch.add(i as usize * esize) as *const VolumeDesc)
            };
            if d.kind == VOL_KIND_MFS && d.id as u64 != target {
                max = max.max(mfs_primary_of_vol(d.id as u64));
            }
            i += 1;
        }
    }
    max + 1
}

// ---------------------------------------------------------------------------
// 空闲位图 (块分配 / 空间回收)
// ---------------------------------------------------------------------------
// MFS2 用一块常驻位图记录块占用 (1 = 占用, 0 = 空闲), 位图本身随超级块 COW
// 交替写入两份副本, 故 CRC 一并保护。分配不再「只增不减」: 旧块的可达性由 GC
// 判定, 不可达的老版本块会被归还给空闲池。

fn mfs_bmp_byte(i: usize) -> *mut u8 {
    (MFS_BMP_VADDR as *mut u8).wrapping_add(i)
}
fn mfs_mark_byte(i: usize) -> *mut u8 {
    (MFS_MARK_VADDR as *mut u8).wrapping_add(i)
}
fn mfs_seen_byte(i: usize) -> *mut u8 {
    (MFS_SEEN_VADDR as *mut u8).wrapping_add(i)
}
/// 位图需覆盖的字节数 (按当前卷总块数动态算)。
fn mfs_bmp_bytes() -> usize {
    (unsafe { MFS_TOTAL_BLOCKS } as usize).div_ceil(8)
}
/// 把块 `b` 所在的位图数据块标脏 (提交时只落盘变动过的区间)。
fn mfs_bmp_touch(b: u32) {
    let c = (b / MFS_BMP_BLOCK_SPAN) as usize;
    if c < MFS_MAX_BMP_DATA_BLOCKS as usize {
        unsafe {
            *core::ptr::addr_of_mut!(MFS_BMP_DIRTY)
                .cast::<u8>()
                .add(c >> 3) |= 1u8 << (c & 7);
        }
    }
}
/// 清空整个标记窗口 (GC 每轮的起点)。
fn mfs_mark_clear_all() {
    for i in 0..mfs_bmp_bytes() {
        unsafe {
            *mfs_mark_byte(i) = 0;
        }
    }
}
/// 清空整个「本根已访问」窗口 (每换一个可达根都要清)。
fn mfs_seen_clear_all() {
    for i in 0..mfs_bmp_bytes() {
        unsafe {
            *mfs_seen_byte(i) = 0;
        }
    }
}
/// 本根是否已访问过块 `b` (去重与防环)。
fn mfs_seen_get(b: u32) -> bool {
    if b >= unsafe { MFS_TOTAL_BLOCKS } {
        return false;
    }
    let i = b as usize;
    unsafe { *mfs_seen_byte(i >> 3) & (1u8 << (i & 7)) != 0 }
}
fn mfs_seen_set(b: u32) {
    if b < unsafe { MFS_TOTAL_BLOCKS } {
        let i = b as usize;
        unsafe {
            *mfs_seen_byte(i >> 3) |= 1u8 << (i & 7);
        }
    }
}
fn mfs_stack_slot(i: usize) -> *mut u32 {
    unsafe { core::ptr::addr_of_mut!(MFS_GC_STACK).cast::<u32>().add(i) }
}

fn mfs_bmp_get(b: u32) -> bool {
    if b >= unsafe { MFS_TOTAL_BLOCKS } {
        return false;
    }
    let i = b as usize;
    unsafe { *mfs_bmp_byte(i >> 3) & (1u8 << (i & 7)) != 0 }
}
fn mfs_bmp_set(b: u32) {
    if b >= unsafe { MFS_TOTAL_BLOCKS } {
        return;
    }
    let i = b as usize;
    let p = mfs_bmp_byte(i >> 3);
    unsafe {
        if *p & (1u8 << (i & 7)) == 0 {
            *p |= 1u8 << (i & 7);
            MFS_FREE_BLOCKS = MFS_FREE_BLOCKS.saturating_sub(1);
            mfs_bmp_touch(b);
        }
    }
}
fn mfs_bmp_clear(b: u32) {
    if b >= unsafe { MFS_TOTAL_BLOCKS } {
        return;
    }
    let i = b as usize;
    let p = mfs_bmp_byte(i >> 3);
    unsafe {
        if *p & (1u8 << (i & 7)) != 0 {
            *p &= !(1u8 << (i & 7));
            MFS_FREE_BLOCKS += 1;
            mfs_bmp_touch(b);
        }
    }
}
/// 按位图重算空闲块数 (挂载校验用; 位图是唯一依据, 不信任落盘计数)。
fn mfs_bmp_recount() -> u32 {
    let total = unsafe { MFS_TOTAL_BLOCKS } as usize;
    let mut used = 0usize;
    for b in 0..total {
        if mfs_bmp_get(b as u32) {
            used += 1;
        }
    }
    let free = (total - used) as u32;
    unsafe {
        MFS_FREE_BLOCKS = free;
    }
    free
}

/// 把一个 inode 表索引块及其引用的所有表块强制标为占用。
///
/// 挂载时用它保护元数据: 当前表与每张快照的表都是可达根的元数据, 一旦被当作空闲
/// 分配出去, 对应根的翻译就会失效 (GC 随之失败)。读失败 (块不在盘上 / CRC 坏) 时
/// 只保留索引块本身占位 —— 交给后续 GC 判定, 不在这里阻塞挂载。
fn mfs_mark_itab_meta(itab: u32) {
    if itab == 0 {
        return;
    }
    mfs_bmp_set(itab);
    let x = mfs_itabx_buf();
    if !mfs_read_blk(itab, x) || !mfs_ok(x, MFS_MAGIC_ITABX) {
        return;
    }
    for k in 0..MFS_ITAB_SLOTS {
        let t = read_u32(mfs_at(x, MFS_HDR + k * 4));
        if t != 0 {
            mfs_bmp_set(t);
        }
    }
}

/// 位图数据块数 `bb = ceil(total / 32768)` (一个数据块 = 4096 字节位图 = 32768 块)。
fn mfs_bb_for(total: u32) -> u32 {
    total.div_ceil(MFS_BMP_BLOCK_SPAN)
}

/// 数据区起点 (块 0/1 超级块 + 块 2/3 位图头 + 两副本位图数据); 之前的块一律强制占用。
fn mfs_data_start() -> u32 {
    4 + 2 * mfs_bb_for(unsafe { MFS_TOTAL_BLOCKS })
}

/// 从位图中取一个空闲块, **不**触发回收; 空间耗尽返回 None。
///
/// 这里不做 GC: 调用点常在一次 COW 操作中间 (已分配但尚未被根引用的块), GC 会把
/// 它们误判成垃圾。回收改在服务空闲时分派前统一触发 (见 `mfs_maybe_gc`)。
fn mfs_alloc_block() -> Option<u32> {
    let total = unsafe { MFS_TOTAL_BLOCKS };
    let start = mfs_data_start();
    if total <= start {
        return None;
    }
    // 先扫 [游标, 末尾), 再回头扫 [数据区起点, 游标), 避免每次都从头找。
    let hint = unsafe { MFS_ALLOC_NEXT }.clamp(start, total);
    let mut b = hint;
    while b < total {
        if !mfs_bmp_get(b) {
            return Some(mfs_bmp_take(b));
        }
        b += 1;
    }
    b = start;
    while b < hint {
        if !mfs_bmp_get(b) {
            return Some(mfs_bmp_take(b));
        }
        b += 1;
    }
    None
}

/// 占用块 `b` 并把分配游标推到其后。
fn mfs_bmp_take(b: u32) -> u32 {
    mfs_bmp_set(b);
    unsafe {
        MFS_ALLOC_NEXT = b + 1;
    }
    b
}

// ---------------------------------------------------------------------------
// 空间回收 (mark & sweep)
// ---------------------------------------------------------------------------

/// 在标记位图中置位 `b` (越界忽略)。
fn mfs_mark_set(b: u32, total: usize) {
    let i = b as usize;
    if i >= total {
        return;
    }
    unsafe {
        *mfs_mark_byte(i >> 3) |= 1u8 << (i & 7);
    }
}
fn mfs_mark_get(b: u32) -> bool {
    if b >= unsafe { MFS_TOTAL_BLOCKS } {
        return false;
    }
    let i = b as usize;
    unsafe { *mfs_mark_byte(i >> 3) & (1u8 << (i & 7)) != 0 }
}

/// GC 遍历栈顶指针的裸指针 (避免对 `static mut` 造 `&mut`, 那属于未定义行为)。
fn mfs_gc_sp_ptr() -> *mut usize {
    core::ptr::addr_of_mut!(MFS_GC_SP)
}
fn mfs_gc_sp_get() -> usize {
    unsafe { *mfs_gc_sp_ptr() }
}
fn mfs_gc_sp_set(v: usize) {
    unsafe {
        *mfs_gc_sp_ptr() = v;
    }
}

/// 把块 `b` 压入遍历栈并标记可达; 0 / 越界 / **本根已访问**的块直接跳过。栈满 false。
///
/// 去重用「本根已访问」位图而不是可达位图: 同一个块在不同根的表下会展开出不同的子树。
fn mfs_gc_push(b: u32, total: usize) -> bool {
    if b == 0 || b as usize >= total || mfs_seen_get(b) {
        return true;
    }
    let sp = mfs_gc_sp_get();
    if sp >= MFS_GC_STACK_MAX {
        return false;
    }
    unsafe {
        *mfs_stack_slot(sp) = b;
    }
    mfs_gc_sp_set(sp + 1);
    mfs_seen_set(b);
    mfs_mark_set(b, total);
    true
}

/// 载入某个可达根的 inode 表索引块到 X 缓冲 (GC 期间该缓冲专供此事), 并标记索引块
/// 与它引用的所有表块 —— 它们是元数据, 必须视为可达, 否则会被回收后重新分配出去。
fn mfs_gc_load_itab(itab: u32, total: usize) -> bool {
    if itab == 0 || itab as usize >= total {
        return false;
    }
    mfs_mark_set(itab, total);
    let x = mfs_itabx_buf();
    if !mfs_read_blk(itab, x) || !mfs_ok(x, MFS_MAGIC_ITABX) {
        return false;
    }
    for k in 0..MFS_ITAB_SLOTS {
        let t = read_u32(mfs_at(x, MFS_HDR + k * 4));
        if t == 0 {
            continue;
        }
        if t as usize >= total {
            return false;
        }
        mfs_mark_set(t, total);
    }
    true
}

/// GC 期间用「当前正在遍历的那个根的 inode 表」翻译 ino -> 块号。
///
/// 快照必须用它自己的表: 同一个 ino 在快照里指向的是当时的对象块, 用当前表翻译会
/// 把快照内容识别成最新版本, 从而漏标真正的历史块。
fn mfs_gc_ino_block(ino: u32) -> Option<u32> {
    if ino == 0 || ino >= MFS_INO_MAX {
        return None;
    }
    let (k, j) = mfs_ino_slot(ino);
    let x = mfs_itabx_buf();
    let t = read_u32(mfs_at(x, MFS_HDR + k as usize * 4));
    if t == 0 {
        return Some(0);
    }
    let tb = mfs_gc_tab_buf();
    if !mfs_read_blk(t, tb) || !mfs_ok(tb, MFS_MAGIC_ITAB) {
        return None;
    }
    Some(read_u32(mfs_at(tb, MFS_HDR + j * 4)))
}

/// 空间回收: 从当前根 + 所有快照根出发标记可达块, 其余块释放。
///
/// COW 只增不减时, 被新版本取代的旧块会一直占在位图里; 回收的唯一判据是**可达性**
/// —— 快照根同样算根, 因此快照仍引用的历史版本 (含它自己的 inode 表) 不会被回收
/// (回滚依旧可用)。遍历或写盘失败时不改动位图, 调用方看到的仍是一致的旧位图。
/// 返回本次回收的块数; 失败返回 `u64::MAX`。
fn mfs_gc() -> u64 {
    let total = unsafe { MFS_TOTAL_BLOCKS } as usize;
    if total == 0 || total > MFS_MAX_BLOCKS as usize {
        return u64::MAX;
    }
    mfs_mark_clear_all();
    // 元数据区 (超级块 / 位图头 / 位图数据块) 一律视为可达, 绝不回收。
    let meta_end = mfs_data_start();
    for b in 0..meta_end {
        mfs_mark_set(b, total);
    }
    // 可达根 = (当前 inode 表, 根 ino) + 每个快照的 (表, 根 ino); 根号恒为 1。
    let sn = unsafe { MFS_SNAP_COUNT };
    let mut roots = [(0u32, 0u32); MFS_MAX_SNAP + 1];
    roots[0] = (unsafe { MFS_ITAB }, MFS_ROOT_INO);
    for (i, slot) in roots.iter_mut().enumerate().take(sn + 1).skip(1) {
        slot.0 = mfs_snap(i - 1).itab;
        slot.1 = MFS_ROOT_INO;
    }
    for &(itab, root_ino) in roots.iter().take(sn + 1) {
        if !mfs_gc_load_itab(itab, total) {
            return u64::MAX;
        }
        // 换根: 清空「本根已访问」, 保证这棵树用**它自己的表**重新展开一遍。
        mfs_seen_clear_all();
        let rblk = match mfs_gc_ino_block(root_ino) {
            Some(b) if b != 0 => b,
            _ => return u64::MAX,
        };
        if !mfs_gc_push(rblk, total) {
            return u64::MAX;
        }
        if !mfs_gc_drain(total) {
            return u64::MAX;
        }
    }
    // 用标记位图重建空闲位图。
    let free_before = unsafe { MFS_FREE_BLOCKS };
    let mut used = 0u32;
    for b in 0..total {
        if mfs_mark_get(b as u32) {
            used += 1;
            mfs_bmp_set(b as u32);
        } else {
            mfs_bmp_clear(b as u32);
        }
    }
    unsafe {
        MFS_FREE_BLOCKS = total as u32 - used;
        MFS_ALLOC_NEXT = mfs_data_start();
    }
    if !mfs_bmp_flush() {
        return u64::MAX;
    }
    (total as u32 - used).saturating_sub(free_before) as u64
}

/// 深度优先展开遍历栈: 目录展开其子项 (条目存 ino, 需经当前根的表翻译),
/// 文件展开其数据块, 数据块是叶子。
fn mfs_gc_drain(total: usize) -> bool {
    let gb = mfs_gc_buf();
    while mfs_gc_sp_get() > 0 {
        let sp = mfs_gc_sp_get() - 1;
        mfs_gc_sp_set(sp);
        let b = unsafe { *mfs_stack_slot(sp) };
        if !mfs_read_blk(b, gb) {
            return false;
        }
        if mfs_ok(gb, MFS_MAGIC_DIR) {
            // 变长条目: 按 rec_len 逐个走, 有效条目的 ino 翻译成块后入栈展开;
            // 目录的扩展索引块同样可达 (它下面挂着扩展目录块)。
            let end = MFS_HDR + MFS_PAYLOAD;
            let mut off = MFS_HDR + MFS_DIR_HDR;
            while off + MFS_DIR_ENT_HDR <= end {
                if mfs_ent_name_len(gb, off) != 0 {
                    let child = match mfs_gc_ino_block(mfs_ent_ino(gb, off)) {
                        Some(x) => x,
                        None => return false,
                    };
                    if !mfs_gc_push(child, total) {
                        return false;
                    }
                }
                match mfs_ent_step(gb, off) {
                    Some(n) => off = n,
                    // 结构不可信: 放弃本次回收 (宁可漏回收, 也不能把在用的块发出去)。
                    None => return false,
                }
            }
            if !mfs_gc_push(mfs_dir_ext(gb), total) {
                return false;
            }
        } else if mfs_ok(gb, MFS_MAGIC_DIDX) {
            // 目录扩展索引块: 槽位全是扩展目录块。
            for i in 0..MFS_DIR_SLOTS {
                if !mfs_gc_push(read_u32(mfs_at(gb, MFS_HDR + i * 4)), total) {
                    return false;
                }
            }
        } else if mfs_ok(gb, MFS_MAGIC_FILE) {
            // 文件节点: 直接槽逐个标记 (未用槽恒为 0), 一/二/三级间接块入栈展开。
            // 不看 nblocks: 计数若损坏, 少标就会把仍在用的数据块回收掉。
            for i in 0..MFS_FILE_DIRECT {
                let c = mfs_file_direct(gb, i);
                if c != 0 {
                    mfs_mark_set(c, total);
                }
            }
            // ind3 必须一并入栈: 漏标会让三级块被当作垃圾回收, 进而损坏 >4 GiB 文件。
            if !mfs_gc_push(mfs_file_ind1(gb), total)
                || !mfs_gc_push(mfs_file_ind2(gb), total)
                || !mfs_gc_push(mfs_file_ind3(gb), total)
            {
                return false;
            }
        } else if mfs_ok(gb, MFS_MAGIC_IND) {
            // 一级间接块: 槽位全是数据块 (叶子)。
            for i in 0..MFS_IND_CAP {
                let c = mfs_ind_slot(gb, i);
                if c != 0 {
                    mfs_mark_set(c, total);
                }
            }
        } else if mfs_ok(gb, MFS_MAGIC_IND2) {
            // 二级间接块: 槽位全是一级间接块。
            for i in 0..MFS_IND_CAP {
                if !mfs_gc_push(mfs_ind_slot(gb, i), total) {
                    return false;
                }
            }
        } else if mfs_ok(gb, MFS_MAGIC_IND3) {
            // 三级间接块: 槽位全是二级间接块。
            for i in 0..MFS_IND_CAP {
                if !mfs_gc_push(mfs_ind_slot(gb, i), total) {
                    return false;
                }
            }
        } else if mfs_ok(gb, MFS_MAGIC_LINK) {
            // 软链接节点 (M5c): 目标内联在 payload 里, 不引用任何其它块 —— 到这一层
            // 就算展开完了。少了这一支会掉进下面的 else 被判成「盘上结构不可信」,
            // 于是**只要卷上存在软链接, 整次回收都会被放弃**。
        } else {
            // 可达块却既不是目录也不是文件节点: 说明盘上结构不可信, 放弃本次回收
            // (宁可漏回收, 也不能把仍被引用的块分配出去)。
            return false;
        }
    }
    true
}

/// 低水位回收: 空闲块少于 `总块数 / MFS_GC_LOW_WATER_DIV` 时回收一次。
///
/// 只在**没有在建 COW 操作**时分派请求之前调用 (见 `mfs_main` 主循环)。
fn mfs_maybe_gc() {
    let total = unsafe { MFS_TOTAL_BLOCKS };
    if total == 0 || unsafe { MFS_FREE_BLOCKS } > total / MFS_GC_LOW_WATER_DIV {
        return;
    }
    if mfs_gc() == u64::MAX {
        println("mfs: gc FAILED");
    }
}

/// 封装并写入一个新块, 返回块号 (COW 的基本操作)。
fn mfs_commit(buf: *mut u8, magic: u32) -> Option<u32> {
    mfs_seal(buf, magic);
    let b = mfs_alloc_block()?;
    if !mfs_write_blk(b, buf) {
        return None;
    }
    Some(b)
}

// ---------------------------------------------------------------------------
// inode 表 (ino -> 对象块号)
// ---------------------------------------------------------------------------
//
// 索引块 (MFIX) 有 `MFS_ITAB_SLOTS` 个槽, 第 k 个槽指向第 k 个表块 (MFIT); 表块有
// `MFS_ITAB_SLOTS` 个槽, 第 j 个槽指向 ino = k * SLOTS + j 的对象块。索引块内容在
// 内存里留一份完整镜像 (`MFS_ITAB_MEM`), 改动时整体 COW 成新索引块 —— 这样查表
// 不需要读索引块, 只有表块要读 (单条目缓存让连续 ino 只读一次)。
//
// 表块 / 索引块都是普通分配块, 故 GC 必须把它们当作可达块标记 (见 `mfs_gc`)。

/// 索引块内容的内存镜像 (slot k = 第 k 个表块号, 0 = 未分配)。
static mut MFS_ITAB_MEM: [u32; MFS_ITAB_SLOTS] = [0; MFS_ITAB_SLOTS];

/// ino 落在哪个表块的第几个槽。
fn mfs_ino_slot(ino: u32) -> (u32, usize) {
    let n = ino as usize;
    ((n / MFS_ITAB_SLOTS) as u32, n % MFS_ITAB_SLOTS)
}

fn mfs_itab_table(k: u32) -> u32 {
    if k as usize >= MFS_ITAB_SLOTS {
        return 0;
    }
    unsafe {
        *core::ptr::addr_of!(MFS_ITAB_MEM)
            .cast::<u32>()
            .add(k as usize)
    }
}
fn mfs_set_itab_table(k: u32, blk: u32) {
    if (k as usize) < MFS_ITAB_SLOTS {
        unsafe {
            *core::ptr::addr_of_mut!(MFS_ITAB_MEM)
                .cast::<u32>()
                .add(k as usize) = blk;
        }
    }
}

/// 读 ino 对应的对象块号 (0 = 未分配); 结构不可信 / 越界返回 None。
///
/// 单条目表块缓存: 连续 ino (create 分配、readdir 遍历) 通常落在同一表块, 只读一次。
fn mfs_ino_block(ino: u32) -> Option<u32> {
    if ino == 0 || ino >= MFS_INO_MAX {
        return None;
    }
    let (k, j) = mfs_ino_slot(ino);
    let t = mfs_itab_table(k);
    if t == 0 {
        return Some(0);
    }
    let buf = mfs_itab_buf();
    if unsafe { MFS_ITAB_CACHE_IDX } != k {
        if !mfs_read_blk(t, buf) || !mfs_ok(buf, MFS_MAGIC_ITAB) {
            return None;
        }
        unsafe {
            MFS_ITAB_CACHE_IDX = k;
        }
    }
    Some(read_u32(mfs_at(buf, MFS_HDR + j * 4)))
}

/// 把 `MFS_ITAB` 指向的索引块内容载入内存镜像, 并让表块缓存失效。
///
/// 挂载与快照回滚都要用它 —— 回滚会把 `MFS_ITAB` 换成旧索引块, 镜像必须跟着换。
fn mfs_itab_reload() -> bool {
    let x = mfs_itabx_buf();
    let itab = unsafe { MFS_ITAB };
    if itab == 0 || !mfs_read_blk(itab, x) || !mfs_ok(x, MFS_MAGIC_ITABX) {
        return false;
    }
    for k in 0..MFS_ITAB_SLOTS {
        mfs_set_itab_table(k as u32, read_u32(mfs_at(x, MFS_HDR + k * 4)));
    }
    unsafe {
        MFS_ITAB_CACHE_IDX = u32::MAX;
    }
    true
}

/// 把内存索引镜像 COW 成一个新的索引块 (每次表块变化后调用), 并写超级块。
fn mfs_itab_flush() -> bool {
    let x = mfs_itabx_buf();
    zero_bytes(x, MFS_BLOCK);
    mfs_seal(x, MFS_MAGIC_ITABX); // 先占位, 内容随后填 (下面重新封装)
    for k in 0..MFS_ITAB_SLOTS {
        write_u32(mfs_atm(x, MFS_HDR + k * 4), mfs_itab_table(k as u32));
    }
    let nb = match mfs_commit(x, MFS_MAGIC_ITABX) {
        Some(b) => b,
        None => return false,
    };
    unsafe {
        MFS_ITAB = nb;
    }
    mfs_bmp_flush()
}

/// 在卷 `vol` 上写入一个全新的 MFS 文件系统 (**擦除**该卷现有内容), 成功后把该卷
/// 挂到 `/usb<卷号>` 立即可用。成功返回该卷落盘后的**主卷序号** (>0), 失败 `u64::MAX`。
///
/// 护栏: 只接受「**已是 MFS**」或「**整盘无文件系统**」的卷 —— FAT / exFAT / ext2
/// 等别人的分区一律拒绝, 绝不自动吞掉。真盘上卷号认错时, 这里就是最后一道闸。
///
/// **主卷语义**: 格式化会把该卷标记为主卷 (序号 = 现有最大 + 1), 于是**下一次启动**
/// `/mfs` 就认领到它 —— 这就是「切换主卷」的手段。本次运行的主卷不变: 换主卷是要重启
/// 才生效的事, 在跑的会话里换挂载点会让所有已打开的路径句柄失效。
///
/// 格式化期间内存态被改写成新卷, 故结束后必须把**原卷**的状态重新载回来; 那里只用
/// `mfs_load_state` (只载入), 不会因原卷此刻读不出来而把它格式化掉。
fn mfs_mkfs_volume(vol: u64) -> u64 {
    let desc = match vol_find_desc(mfs_a(), vol) {
        Some(d) => d,
        None => {
            println("mfs: mkfs refused (no such volume)");
            return u64::MAX;
        }
    };
    if desc.kind != VOL_KIND_UNKNOWN && desc.kind != VOL_KIND_MFS {
        println("mfs: mkfs refused (volume holds another filesystem)");
        return u64::MAX;
    }
    let prev_vol = unsafe { MFS_CUR_VOL };
    let prev_sectors = unsafe { MFS_CUR_SECTORS };
    let serial = mfs_next_primary_serial(mfs_a(), vol);
    unsafe {
        MFS_CUR_VOL = vol;
        MFS_CUR_SECTORS = desc.sectors; // 格式化尺寸按目标卷的真实容量算 (M7)
        MFS_PRIMARY_SERIAL = serial; // 提交时随超级块写盘, 下次启动据此认领主卷
        MFS_LEAF = MFS_LOC_EMPTY;
    }
    let ok = mfs_format();
    // 切回原卷并重建它的内存态 (位图 / inode 表 / 快照 / 各游标 / 主卷序号): 格式化已经
    // 把内存态写成了新卷, 不重建的话后续对原卷的读写会用错的总块数与位图。
    unsafe {
        MFS_CUR_VOL = prev_vol;
        MFS_CUR_SECTORS = prev_sectors;
    }
    if !mfs_load_state() {
        println("mfs: reload state after mkfs FAILED");
        return u64::MAX;
    }
    if !ok {
        println("mfs: mkfs FAILED");
        return u64::MAX;
    }
    // 立刻可用: 非主卷挂到 `/usb<卷号>` (主卷已挂在 `/mfs`, 不重复挂)。
    if vol != unsafe { MFS_VOL } && vfs::mount_vol(vfs::MFS_DOMAIN, vol) == u64::MAX {
        println("mfs: mkfs OK but mount FAILED (mount table full?)");
    }
    // 回复**从盘上回读**的序号, 而不是刚才设进内存的那个: 前者证明「标记确实写进了
    // 超级块并能再读出来」, 后者只说明「我记得我设过」。回读为 0 说明没落盘。
    let landed = mfs_primary_of_vol(vol);
    if landed == 0 {
        println("mfs: mkfs OK but primary mark missing on disk");
        return u64::MAX;
    }
    landed
}

/// 设置 ino 的槽位 (`blk == 0` 表示释放该 ino)。
///
/// 写路径: 载入表块 → 改槽位 → COW 表块 → 更新索引镜像 → COW 索引块 → 写超级块。
/// 索引块内存镜像里保留的就是刚写出去的表块内容, 故缓存仍有效。
fn mfs_itab_set(ino: u32, blk: u32) -> bool {
    if ino == 0 || ino >= MFS_INO_MAX {
        return false;
    }
    let (k, j) = mfs_ino_slot(ino);
    let t = mfs_itab_table(k);
    let buf = mfs_itab_buf();
    if t == 0 {
        zero_bytes(buf, MFS_BLOCK); // 该表块首次使用
    } else if unsafe { MFS_ITAB_CACHE_IDX } != k
        && (!mfs_read_blk(t, buf) || !mfs_ok(buf, MFS_MAGIC_ITAB))
    {
        return false;
    }
    write_u32(mfs_atm(buf, MFS_HDR + j * 4), blk);
    let new_t = match mfs_commit(buf, MFS_MAGIC_ITAB) {
        Some(b) => b,
        None => return false,
    };
    unsafe {
        MFS_ITAB_CACHE_IDX = k;
    }
    mfs_set_itab_table(k, new_t);
    mfs_itab_flush()
}

/// 为一个**新建**对象分配空闲 ino, 并把它的槽位直接设为 `blk`。返回 ino。
fn mfs_ino_alloc_for(blk: u32) -> Option<u32> {
    let hint = unsafe { MFS_INO_HINT }.clamp(MFS_ROOT_INO + 1, MFS_INO_MAX - 1);
    let mut ino = hint;
    for _ in 0..MFS_INO_MAX {
        if ino >= MFS_INO_MAX {
            ino = MFS_ROOT_INO + 1; // 回绕; ino 1 留给根, 永不复用
        }
        if mfs_ino_block(ino) == Some(0) {
            if !mfs_itab_set(ino, blk) {
                return None;
            }
            unsafe {
                MFS_INO_HINT = if ino + 1 >= MFS_INO_MAX {
                    MFS_ROOT_INO + 1
                } else {
                    ino + 1
                };
                MFS_INO_COUNT += 1;
            }
            return Some(ino);
        }
        ino += 1;
    }
    None
}

/// 释放 ino: 清空它的槽位。对象块随即不可达, 由 GC 回收。
fn mfs_free_ino(ino: u32) -> bool {
    if ino <= MFS_ROOT_INO {
        return false; // 根不可释放
    }
    if !mfs_itab_set(ino, 0) {
        return false;
    }
    unsafe {
        MFS_INO_COUNT = MFS_INO_COUNT.saturating_sub(1);
    }
    true
}

/// COW 提交一个已有对象 (目录 / 文件节点) 并同步它的表槽 —— MFS6 写路径的统一出口。
///
/// 对象块换了位置, 而引用它的目录项存的是 ino (不变), 故**无需**回写父目录:
/// 这正是硬链接的多个名字能自动保持一致的原因, 也让写代价与目录深度无关。
fn mfs_commit_object(ino: u32, buf: *mut u8, magic: u32) -> Option<u32> {
    let nb = mfs_commit(buf, magic)?;
    if !mfs_itab_set(ino, nb) {
        return None;
    }
    Some(nb)
}

// ---------------------------------------------------------------------------
// 超级块 (A/B 双副本)
// ---------------------------------------------------------------------------

fn mfs_build_super(buf: *mut u8) {
    zero_bytes(buf, MFS_BLOCK);
    let p = MFS_HDR;
    write_u32(mfs_atm(buf, p), MFS_VERSION);
    write_u32(mfs_atm(buf, p + MFS_SB_BLOCK_SIZE), MFS_BLOCK as u32);
    write_u32(mfs_atm(buf, p + MFS_SB_TOTAL), unsafe { MFS_TOTAL_BLOCKS });
    write_u32(mfs_atm(buf, p + MFS_SB_INO_COUNT), unsafe { MFS_INO_COUNT });
    write_u32(mfs_atm(buf, p + MFS_SB_ALLOC_HINT), unsafe {
        MFS_ALLOC_NEXT
    });
    write_u32(
        mfs_atm(buf, p + MFS_SB_SNAP_COUNT),
        unsafe { MFS_SNAP_COUNT } as u32,
    );
    write_u64(mfs_atm(buf, p + MFS_SB_GEN), unsafe { MFS_GEN });
    write_u32(mfs_atm(buf, p + MFS_SB_ITAB), unsafe { MFS_ITAB });
    write_u32(mfs_atm(buf, p + MFS_SB_INO_HINT), unsafe { MFS_INO_HINT });
    for i in 0..unsafe { MFS_SNAP_COUNT } {
        let s = mfs_snap(i);
        let off = p + MFS_SB_SNAPS + i * MFS_SNAP_REC;
        write_u64(mfs_atm(buf, off), s.gen);
        write_u32(mfs_atm(buf, off + 8), s.itab);
        write_u32(mfs_atm(buf, off + 12), s.ino_hint);
        write_u32(mfs_atm(buf, off + 16), s.alloc_next);
    }
    // 主卷序号: 认领 `/mfs` 的依据 (见 `mfs_vol_claim`)。必须每次都写 —— 提交走的是
    // 「重写整块超级块」而不是原地改字段, 漏写就会把标记清掉。
    write_u64(mfs_atm(buf, p + MFS_SB_PRIMARY), unsafe {
        MFS_PRIMARY_SERIAL
    });
    // MFS7: 空闲位图不再内联在超级块里, 位图改由 `mfs_bmp_flush` 写入独立的位图
    // 数据块 + 头块。原内联区 (+256 起) 只留 `MFS_SB_PRIMARY` 一项, 其余为保留。
    mfs_seal(buf, MFS_MAGIC_SUPER);
}

/// 构建位图头块 (magic "MFBH"): gen / 总块数 / 位图数据块数 bb + 各数据块 CRC32。
fn mfs_build_bmp_header(buf: *mut u8) {
    zero_bytes(buf, MFS_BLOCK);
    let p = MFS_HDR;
    unsafe {
        write_u64(mfs_atm(buf, p + MFS_BMPH_GEN), MFS_GEN);
        write_u32(mfs_atm(buf, p + MFS_BMPH_TOTAL), MFS_TOTAL_BLOCKS);
        write_u32(mfs_atm(buf, p + MFS_BMPH_DATA_BLOCKS), MFS_BMP_DATA_BLOCKS);
        for i in 0..MFS_BMP_DATA_BLOCKS as usize {
            write_u32(
                mfs_atm(buf, p + MFS_BMPH_CRC + i * 4),
                *core::ptr::addr_of!(MFS_BMP_CRC).cast::<u32>().add(i),
            );
        }
    }
    mfs_seal(buf, MFS_MAGIC_BMPHDR);
}

/// 把常驻位图窗口按副本落盘 (只写脏区间), 再写头块 + 超级块 —— **唯一**提交出口。
///
/// 取代 MFS6 的 `mfs_write_super`。流程 (代际 +1 后):
///   1. 清空脏位图, 重新计算本次要写的区间的 CRC;
///   2. 对 copy ∈ {0,1} 依次: 写脏的位图数据块 → 写该副本头块 (gen/total/bb + 全量 CRC) →
///      写该副本超级块 (带新 gen);
///   3. 两份都写完后清 dirty。
///
/// 崩溃在任一步骤时, 另一份仍是**旧代但自洽**的 (gen 不同 → 加载取 gen 高者), 因此
/// 不需要额外的副本选择状态 (`MFS_SB_COPY` 已删)。
fn mfs_bmp_flush() -> bool {
    unsafe {
        MFS_GEN += 1;
    }
    let bb = unsafe { MFS_BMP_DATA_BLOCKS };
    let (a_start, b_start) = (4u32, 4 + bb);
    let head = mfs_bmph_buf();
    // 重新计算全部位图数据块的 CRC (脏区间之外的块内容未变, 但 CRC 数组整体落盘,
    // 故这里统一按窗口内容算, 保证数组与数据块始终一致)。
    for c in 0..bb as usize {
        let p = (MFS_BMP_VADDR as *const u8).wrapping_add(c * MFS_BLOCK);
        let crc = unsafe { mfs_crc32(core::slice::from_raw_parts(p, MFS_BLOCK)) };
        unsafe {
            *core::ptr::addr_of_mut!(MFS_BMP_CRC).cast::<u32>().add(c) = crc;
        }
    }
    for copy in 0..MFS_SB_COPIES {
        let data_start = if copy == 0 { a_start } else { b_start };
        // 位图数据块: 直接以窗口页作 I/O 缓冲 (免拷贝), 只写脏区间。
        for c in 0..bb as usize {
            let dirty = unsafe {
                *core::ptr::addr_of!(MFS_BMP_DIRTY).cast::<u8>().add(c >> 3) & (1u8 << (c & 7)) != 0
            };
            if !dirty {
                continue;
            }
            let p = (MFS_BMP_VADDR as *const u8).wrapping_add(c * MFS_BLOCK);
            if !mfs_write_blk(data_start + c as u32, p) {
                return false;
            }
        }
        // 头块 (gen/total/bb + 全量 CRC 数组)。
        mfs_build_bmp_header(head);
        if !mfs_write_blk(2 + copy, head) {
            return false;
        }
        // 超级块 (带新 gen)。
        let sb = mfs_s();
        mfs_build_super(sb);
        if !mfs_write_blk(copy, sb) {
            return false;
        }
    }
    for i in 0..MFS_BMP_DIRTY_BYTES {
        unsafe {
            *core::ptr::addr_of_mut!(MFS_BMP_DIRTY).cast::<u8>().add(i) = 0;
        }
    }
    true
}

/// 从盘上载入 MFS 内存态: 取两份超级块中 CRC 有效、版本匹配且代际更高者, 并据此
/// 重建位图 / inode 表镜像 / 快照表 / 各游标。两份都不可用返回 false。
///
/// **不**在这里格式化 —— 格式化是调用方的决定 (首次挂载可格式化, 但切卷时不行:
/// 那会把一块读不出来的盘直接抹掉, 里面可能是用户唯一的副本)。
fn mfs_load_state() -> bool {
    let mut found = false;
    let mut best_gen = 0u64;
    let mut bitmap_ok = false;
    for copy in 0..MFS_SB_COPIES {
        let buf = mfs_a();
        if !mfs_read_blk(copy, buf) || !mfs_ok(buf, MFS_MAGIC_SUPER) {
            continue;
        }
        let p = MFS_HDR;
        if read_u32(mfs_at(buf, p)) != MFS_VERSION {
            continue;
        }
        let gen = read_u64(mfs_at(buf, p + MFS_SB_GEN));
        // 取代际更高者; 同代时**优先已成功载入位图的那份**。MFS7 起一次提交把两份
        // 副本写成同一 gen, 若只按 `gen <= best_gen` 跳过, 先试的副本位图坏了就会
        // 白白触发重建 —— 即使另一份完好。同代时两份超级块内容一致, 重复采用同一份
        // 内存态无副作用。
        if found && (gen < best_gen || (gen == best_gen && bitmap_ok)) {
            continue;
        }
        let total = read_u32(mfs_at(buf, p + MFS_SB_TOTAL));
        let itab = read_u32(mfs_at(buf, p + MFS_SB_ITAB));
        if total == 0 || itab == 0 || total > MFS_MAX_BLOCKS {
            continue;
        }
        // 盘上记的总块数不能超过该卷的实际容量: 换过镜像 / 卷号认领错 / 卷被缩小过时,
        // 超出的块一律读不到 —— 与其让后续读写大面积失败, 不如在这里判该副本不可用
        // (两份都不可用就会走格式化, 那才是正确处置)。容量未知 (0) 时跳过这项检查。
        let vsectors = unsafe { MFS_CUR_SECTORS } as u64;
        if vsectors != 0 && total as u64 * MFS_SECTORS_PER_BLOCK as u64 > vsectors {
            continue;
        }
        // 索引块是 inode 表的根, 先验证它再采纳这份副本 (坏了就试另一份)。
        let x = mfs_itabx_buf();
        if !mfs_read_blk(itab, x) || !mfs_ok(x, MFS_MAGIC_ITABX) {
            continue;
        }
        let alloc = read_u32(mfs_at(buf, p + MFS_SB_ALLOC_HINT));
        let scount = (read_u32(mfs_at(buf, p + MFS_SB_SNAP_COUNT)) as usize).min(MFS_MAX_SNAP);
        let bb = mfs_bb_for(total);
        unsafe {
            MFS_TOTAL_BLOCKS = total;
            MFS_BMP_DATA_BLOCKS = bb;
            MFS_ALLOC_NEXT = alloc;
            MFS_GEN = gen;
            MFS_SNAP_COUNT = scount;
            MFS_ITAB = itab;
            MFS_INO_COUNT = read_u32(mfs_at(buf, p + MFS_SB_INO_COUNT));
            MFS_INO_HINT = read_u32(mfs_at(buf, p + MFS_SB_INO_HINT)).max(MFS_ROOT_INO + 1);
            // 主卷序号随内存态一起载入: 后续任何一次提交都会把它原样写回, 标记不会丢。
            MFS_PRIMARY_SERIAL = read_u64(mfs_at(buf, p + MFS_SB_PRIMARY));
            MFS_ITAB_CACHE_IDX = u32::MAX;
        }
        // 位图窗口必须容得下本卷的 bb 页 (首次挂载时 `mfs_main` 已按卷容量预算过)。
        if !mfs_win_ensure(bb) {
            continue;
        }
        // 索引块内容载入内存镜像。
        if !mfs_itab_reload() {
            continue;
        }
        for i in 0..scount {
            let off = p + MFS_SB_SNAPS + i * MFS_SNAP_REC;
            mfs_set_snap(
                i,
                MfsSnap {
                    gen: read_u64(mfs_at(buf, off)),
                    itab: read_u32(mfs_at(buf, off + 8)),
                    ino_hint: read_u32(mfs_at(buf, off + 12)),
                    alloc_next: read_u32(mfs_at(buf, off + 16)),
                },
            );
        }
        // 位图数据按副本独立存放: 校验该副本头块与逐块 CRC, 通过则读进窗口。失败时
        // 只是本轮 `bitmap_ok = false` —— 所有候选都失败才走下面的重建兜底。
        bitmap_ok = mfs_load_bitmap_copy(copy);
        best_gen = gen;
        found = true;
    }
    if !found {
        return false;
    }
    if bitmap_ok {
        // 位图是分配的唯一依据: 重算空闲块数, 并强制保留元数据区 (超级块 / 位图头 /
        // 位图数据) 与**所有可达根的 inode 表元数据** (当前表 + 每张快照的表) ——
        // 万一位图缺了这几位 (例如上次回收失败留下的陈旧位图), 立刻纠正, 绝不会把
        // 元数据块分配出去。
        mfs_bmp_recount();
        let meta_end = mfs_data_start();
        for b in 0..meta_end {
            mfs_bmp_set(b);
        }
        mfs_mark_itab_meta(unsafe { MFS_ITAB });
        for i in 0..unsafe { MFS_SNAP_COUNT } {
            mfs_mark_itab_meta(mfs_snap(i).itab);
        }
    } else if !mfs_rebuild_bitmap() {
        return false;
    }
    true
}

/// 载入某个副本的位图: 校验头块 (magic / gen / total / bb) 与逐块 CRC32。
///
/// 校验通过时把数据块读进 `MFS_BMP_VADDR` 窗口 (直接以窗口页作 I/O 缓冲, 免拷贝),
/// 并把各块 CRC 填进常驻 `MFS_BMP_CRC`; 任一不符立即返回 false。
/// 副本 `copy` 的位图数据位于 `4 + copy*bb` 起的连续 `bb` 个块, 头块在块 `2+copy`。
fn mfs_load_bitmap_copy(copy: u32) -> bool {
    let bb = unsafe { MFS_BMP_DATA_BLOCKS };
    let total = unsafe { MFS_TOTAL_BLOCKS };
    let head = mfs_bmph_buf();
    if !mfs_read_blk(2 + copy, head) || !mfs_ok(head, MFS_MAGIC_BMPHDR) {
        return false;
    }
    let hp = MFS_HDR;
    if read_u64(mfs_at(head, hp + MFS_BMPH_GEN)) != unsafe { MFS_GEN } {
        return false;
    }
    if read_u32(mfs_at(head, hp + MFS_BMPH_TOTAL)) != total {
        return false;
    }
    if read_u32(mfs_at(head, hp + MFS_BMPH_DATA_BLOCKS)) != bb {
        return false;
    }
    let data_start = 4 + copy * bb;
    for c in 0..bb as usize {
        let dp = (MFS_BMP_VADDR as *mut u8).wrapping_add(c * MFS_BLOCK);
        if !mfs_read_blk(data_start + c as u32, dp) {
            return false;
        }
        let crc = unsafe { mfs_crc32(core::slice::from_raw_parts(dp as *const u8, MFS_BLOCK)) };
        if crc != read_u32(mfs_at(head, hp + MFS_BMPH_CRC + c * 4)) {
            return false;
        }
        unsafe {
            *core::ptr::addr_of_mut!(MFS_BMP_CRC).cast::<u32>().add(c) = crc;
        }
    }
    true
}

/// 位图两份副本都不可用时的兜底: 先把位图整体置「占用」(FREE = 0 的安全态), 再跑
/// `mfs_gc()` 从可达根重建 —— **不格式化**。
///
/// 全置占用是安全方向: 即使 GC 中途失败, 最坏结果是「空间没被回收」, 绝不会把仍在
/// 使用的块当成空闲发出去。盘上目录树完好、只是位图坏了的情形正是靠这条路径救回。
fn mfs_rebuild_bitmap() -> bool {
    let bb = unsafe { MFS_BMP_DATA_BLOCKS } as usize;
    for i in 0..bb * MFS_BLOCK {
        unsafe {
            *mfs_bmp_byte(i) = 0xFF;
        }
    }
    // 位图全为占用 -> 空闲块数为 0, 交给 GC 重建后重算。
    unsafe {
        MFS_FREE_BLOCKS = 0;
    }
    mfs_gc() != u64::MAX
}

/// 确保三个位图窗口至少各 `pages` 页 (**只增不缩**)。新增的 BMP 窗口页要共享给
/// block_srv (位图数据块直接以窗口页作 DMA 缓冲)。失败返回 false。
fn mfs_win_ensure(pages: u32) -> bool {
    while unsafe { MFS_BMP_PAGES } < pages {
        let n = unsafe { MFS_BMP_PAGES } as u64;
        let va = MFS_BMP_VADDR + n * MFS_BLOCK as u64;
        if sys_alloc_page(va) != 1 || sys_share_page(va, BLOCK_DOMAIN) != 1 {
            return false;
        }
        unsafe {
            MFS_BMP_PAGES += 1;
        }
    }
    while unsafe { MFS_MARK_PAGES } < pages {
        let n = unsafe { MFS_MARK_PAGES } as u64;
        let va = MFS_MARK_VADDR + n * MFS_BLOCK as u64;
        if sys_alloc_page(va) != 1 {
            return false;
        }
        unsafe {
            MFS_MARK_PAGES += 1;
        }
    }
    while unsafe { MFS_SEEN_PAGES } < pages {
        let n = unsafe { MFS_SEEN_PAGES } as u64;
        let va = MFS_SEEN_VADDR + n * MFS_BLOCK as u64;
        if sys_alloc_page(va) != 1 {
            return false;
        }
        unsafe {
            MFS_SEEN_PAGES += 1;
        }
    }
    true
}

/// 切换当前卷到 `vol` (必须是**已格式化**的 MFS 卷): 更新卷号 / 容量后重新载入
/// 内存态。成功返回 true。
///
/// 失败时内存态已不可信 —— 调用方必须放弃本次请求 (见服务循环), 不能继续用旧卷的
/// 位图去写新卷。
fn mfs_switch_vol(vol: u64) -> bool {
    let sectors = vol_sectors(mfs_a(), vol);
    unsafe {
        MFS_CUR_VOL = vol;
        MFS_CUR_SECTORS = sectors;
        // 上一卷的叶子位置 (块号) 在新卷上没有意义, 清掉以免被误用。
        MFS_LEAF = MFS_LOC_EMPTY;
    }
    mfs_load_state()
}

/// 挂载: 能载入就载入, 否则格式化 (首次使用 / 旧格式升级)。
fn mfs_mount_or_format() -> bool {
    if mfs_load_state() {
        return true;
    }
    // 自动格式化**不**认领主卷: 主卷标记只由显式 `mkfs.mfs` 设置 (见 `mfs_mkfs_volume`)。
    // 这里清掉可能残留在内存态里的上一卷序号, 免得把别的卷的标记写进这块新盘。
    unsafe {
        MFS_PRIMARY_SERIAL = 0;
    }
    mfs_format()
}

/// 首次格式化该用多少块: 按**卷的真实容量**算 (每块 4 KiB = 8 扇区), 夹在
/// [`MFS_MIN_TOTAL_BLOCKS`, `MFS_MAX_BLOCKS`] 之间; 容量未知时退回默认值。
///
/// 上界是硬约束: 位图头块的 CRC 数组只能放 `MFS_MAX_BMP_DATA_BLOCKS` 项, 每项覆盖
/// 32768 块 —— 即 `MFS_MAX_BLOCKS` (≈127.25 GiB)。更大的卷需要再加一层位图间接。
fn mfs_format_total_blocks() -> u32 {
    let sectors = unsafe { MFS_CUR_SECTORS } as u64;
    if sectors == 0 {
        return MFS_DEFAULT_TOTAL_BLOCKS;
    }
    let blocks = sectors / MFS_SECTORS_PER_BLOCK as u64;
    blocks.clamp(MFS_MIN_TOTAL_BLOCKS as u64, MFS_MAX_BLOCKS as u64) as u32
}

/// 首次格式化 (含旧格式升级): 清空位图 -> 占用元数据区 -> 建空根目录 (ino 1) ->
/// 建 inode 表 -> 提交 (写位图数据块 + 位图头块 + 超级块)。
fn mfs_format() -> bool {
    let total = mfs_format_total_blocks();
    unsafe {
        MFS_TOTAL_BLOCKS = total;
        MFS_BMP_DATA_BLOCKS = mfs_bb_for(total);
    }
    let bb = unsafe { MFS_BMP_DATA_BLOCKS } as usize;
    // 位图窗口按本卷的 bb 页铺开 (mfs_main 已按容量预算; 这里兜底「mkfs 到更大卷」)。
    if !mfs_win_ensure(bb as u32) {
        return false;
    }
    unsafe {
        MFS_ALLOC_NEXT = mfs_data_start();
        MFS_GEN = 0;
        MFS_SNAP_COUNT = 0;
        MFS_INO_COUNT = 0;
        MFS_INO_HINT = MFS_ROOT_INO + 1;
        MFS_ITAB = 0;
        MFS_ITAB_CACHE_IDX = u32::MAX;
        // 整个位图窗口清零 (含末尾余量), 让 CRC 可复现。
        for i in 0..bb * MFS_BLOCK {
            *mfs_bmp_byte(i) = 0;
        }
        MFS_FREE_BLOCKS = total;
    }
    // 位图内容整体重置: 所有数据块都必须落盘 (不能只靠脏位增量)。
    for c in 0..bb {
        unsafe {
            *core::ptr::addr_of_mut!(MFS_BMP_DIRTY)
                .cast::<u8>()
                .add(c >> 3) |= 1u8 << (c & 7);
        }
    }
    for k in 0..MFS_ITAB_SLOTS {
        mfs_set_itab_table(k as u32, 0); // 空索引镜像
    }
    // 元数据区 (超级块 0/1 + 位图头 2/3 + 两副本位图数据) 一律常驻占用。
    let meta_end = mfs_data_start();
    for b in 0..meta_end {
        mfs_bmp_set(b);
    }
    // 根目录节点块: 无扩展索引块 + 覆盖全区的单个大空槽 + 系统属主 (格式化时无调用者)。
    let buf = mfs_a();
    zero_bytes(buf, MFS_BLOCK);
    mfs_dir_init_empty(buf);
    mfs_init_meta(buf, true, MFS_FTYPE_DIR, 0, MFS_MODE_DIR);
    let root = match mfs_commit(buf, MFS_MAGIC_DIR) {
        Some(b) => b,
        None => return false,
    };
    // 先落一个空索引块, 再登记 ino 1 (登记本身会分配表块并重新 COW 索引块)。
    if !mfs_itab_flush() {
        return false;
    }
    if !mfs_itab_set(MFS_ROOT_INO, root) {
        return false;
    }
    unsafe {
        MFS_INO_COUNT = 1;
        MFS_INO_HINT = MFS_ROOT_INO + 1;
    }
    mfs_bmp_flush()
}

// ---------------------------------------------------------------------------
// 节点元数据 (mode / owner / nlink / 时间戳) 与时间源
// ---------------------------------------------------------------------------
//
// 元数据布局见 `MFS_META_*` 常量。文件节点放在 inode 尾部保留区, 目录节点紧跟
// `ext` 指针之后 —— 位置不同但结构相同, 故访问函数按 `is_dir` 选偏移。

/// 节点元数据在块内的偏移 (`is_dir` 决定文件 / 目录两种布局)。
fn mfs_meta_off(is_dir: bool) -> usize {
    if is_dir {
        MFS_HDR + 8
    } else {
        MFS_FILE_RESERVED_OFF
    }
}

fn mfs_get_mode(buf: *const u8, is_dir: bool) -> u16 {
    read_u16(mfs_at(buf, mfs_meta_off(is_dir) + MFS_META_MODE))
}
/// 设置权限位 (低 12 位), **保留**高 4 位的节点类型 —— 类型随节点固定, `chmod` 不该
/// 把文件改成目录 (或被软链接的形式掩盖)。
fn mfs_set_mode(buf: *mut u8, is_dir: bool, v: u16) {
    let off = mfs_meta_off(is_dir) + MFS_META_MODE;
    let ty = read_u16(mfs_at(buf, off)) & MFS_FTYPE_MASK;
    write_u16(mfs_atm(buf, off), ty | (v & MFS_MODE_MASK));
}
fn mfs_get_owner(buf: *const u8, is_dir: bool) -> u16 {
    read_u16(mfs_at(buf, mfs_meta_off(is_dir) + MFS_META_OWNER))
}
fn mfs_set_owner(buf: *mut u8, is_dir: bool, v: u16) {
    write_u16(mfs_atm(buf, mfs_meta_off(is_dir) + MFS_META_OWNER), v);
}
fn mfs_get_nlink(buf: *const u8, is_dir: bool) -> u32 {
    read_u32(mfs_at(buf, mfs_meta_off(is_dir) + MFS_META_NLINK))
}
fn mfs_set_nlink(buf: *mut u8, is_dir: bool, v: u32) {
    write_u32(mfs_atm(buf, mfs_meta_off(is_dir) + MFS_META_NLINK), v);
}
fn mfs_get_mtime(buf: *const u8, is_dir: bool) -> u64 {
    read_u64(mfs_at(buf, mfs_meta_off(is_dir) + MFS_META_MTIME))
}
fn mfs_set_mtime(buf: *mut u8, is_dir: bool, v: u64) {
    write_u64(mfs_atm(buf, mfs_meta_off(is_dir) + MFS_META_MTIME), v);
}
fn mfs_get_ctime(buf: *const u8, is_dir: bool) -> u64 {
    read_u64(mfs_at(buf, mfs_meta_off(is_dir) + MFS_META_CTIME))
}
fn mfs_set_ctime(buf: *mut u8, is_dir: bool, v: u64) {
    write_u64(mfs_atm(buf, mfs_meta_off(is_dir) + MFS_META_CTIME), v);
}
fn mfs_get_atime(buf: *const u8, is_dir: bool) -> u64 {
    read_u64(mfs_at(buf, mfs_meta_off(is_dir) + MFS_META_ATIME))
}
fn mfs_set_atime(buf: *mut u8, is_dir: bool, v: u64) {
    write_u64(mfs_atm(buf, mfs_meta_off(is_dir) + MFS_META_ATIME), v);
}

/// 初始化一个新建节点的元数据 (nlink = 1, 三个时间同刻)。
///
/// `ftype` 是 `mode` 高 4 位的节点类型 (文件 / 目录 / 软链接); 节点刚清零过, 类型位
/// 必须在这里写入 —— `mfs_set_mode` 是"保留类型"的, 零值下它只能写权限。
fn mfs_init_meta(buf: *mut u8, is_dir: bool, ftype: u16, owner: u16, mode: u16) {
    let now = mfs_now();
    let off = mfs_meta_off(is_dir) + MFS_META_MODE;
    write_u16(
        mfs_atm(buf, off),
        (ftype & MFS_FTYPE_MASK) | (mode & MFS_MODE_MASK),
    );
    mfs_set_owner(buf, is_dir, owner);
    mfs_set_nlink(buf, is_dir, 1);
    mfs_set_mtime(buf, is_dir, now);
    mfs_set_ctime(buf, is_dir, now);
    mfs_set_atime(buf, is_dir, now);
}

/// 内容或元数据变更后刷新 mtime / ctime。
fn mfs_touch(buf: *mut u8, is_dir: bool) {
    let now = mfs_now();
    mfs_set_mtime(buf, is_dir, now);
    mfs_set_ctime(buf, is_dir, now);
}
/// 仅刷新 ctime (权限等元数据变更)。
fn mfs_touch_ctime(buf: *mut u8, is_dir: bool) {
    mfs_set_ctime(buf, is_dir, mfs_now());
}

// --- 时间源: CMOS RTC ---
//
// 内核没有时间系统调用, 但 `SYS_PORT_IN8/OUT8` 已开放, 故用户态直接读 CMOS RTC
// (端口 0x70 选寄存器 / 0x71 读写)。读失败或时间明显不合理时返回 0 —— 元数据里
// 0 表示"时间未知", 不阻塞任何操作。

const CMOS_IDX: u16 = 0x70;
const CMOS_DAT: u16 = 0x71;
/// RTC 寄存器号。
const CMOS_SEC: u8 = 0x00;
const CMOS_MIN: u8 = 0x02;
const CMOS_HOUR: u8 = 0x04;
const CMOS_DAY: u8 = 0x07;
const CMOS_MON: u8 = 0x08;
const CMOS_YEAR: u8 = 0x09;
/// 状态寄存器 A (bit7 = update in progress) / B (bit2 = 二进制, bit1 = 24 小时制)。
const CMOS_STAT_A: u8 = 0x0A;
const CMOS_STAT_B: u8 = 0x0B;

fn cmos_read(reg: u8) -> u8 {
    sys_port_out8(CMOS_IDX, reg);
    sys_port_in8(CMOS_DAT)
}

/// BCD → 二进制 (状态寄存器 B 的 bit2 为 0 时 RTC 用 BCD 编码)。
fn cmos_bcd(v: u8) -> u8 {
    (v & 0x0F) + ((v >> 4) * 10)
}

/// 民用历 (年/月/日) → 距 1970-01-01 的天数 (Howard Hinnant 的 days_from_civil)。
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146097 + doe - 719468
}

/// 读 CMOS RTC 得到当前 Unix 秒 (UTC); 读取失败或字段不合理返回 0。
///
/// 连读两次并要求一致: RTC 更新周期内读到的字段可能跨秒, 两次相同才认为稳定。
fn mfs_now() -> u64 {
    let mut attempt = 0;
    while attempt < 4 {
        attempt += 1;
        let a = cmos_rtc_snapshot();
        let b = cmos_rtc_snapshot();
        let (sa, sb) = match (a, b) {
            (Some(x), Some(y)) => (x, y),
            _ => return 0,
        };
        if sa == sb {
            return sa;
        }
    }
    0
}

/// 单次读取 RTC 并转成 Unix 秒; 等 update-in-progress 清零后读, 字段不合法返回 None。
fn cmos_rtc_snapshot() -> Option<u64> {
    let mut guard = 0u32;
    while cmos_read(CMOS_STAT_A) & 0x80 != 0 {
        guard += 1;
        if guard > 1_000_000 {
            return None;
        }
    }
    let stat_b = cmos_read(CMOS_STAT_B);
    let binary = stat_b & 0x04 != 0;
    let h24 = stat_b & 0x02 != 0;
    let mut sec = cmos_read(CMOS_SEC);
    let mut min = cmos_read(CMOS_MIN);
    let mut hour = cmos_read(CMOS_HOUR);
    let mut day = cmos_read(CMOS_DAY);
    let mut mon = cmos_read(CMOS_MON);
    let mut year = cmos_read(CMOS_YEAR);
    if !binary {
        // 12 小时制时 bit7 是 PM 标志, 必须在 BCD 转换前摘掉。
        let pm = !h24 && (hour & 0x80) != 0;
        hour &= 0x7F;
        sec = cmos_bcd(sec);
        min = cmos_bcd(min);
        hour = cmos_bcd(hour);
        day = cmos_bcd(day);
        mon = cmos_bcd(mon);
        year = cmos_bcd(year);
        if pm && hour < 12 {
            hour += 12;
        }
    } else if !h24 && (hour & 0x80) != 0 {
        hour = ((hour & 0x7F) + 12) % 24;
    }
    // 两位数年份: 按 70..99 → 19xx, 00..69 → 20xx 归一。
    let full_year = if year >= 70 {
        1900 + year as i64
    } else {
        2000 + year as i64
    };
    if !(1..=12).contains(&mon) || !(1..=31).contains(&day) || hour > 23 || min > 59 || sec > 60 {
        return None;
    }
    let days = days_from_civil(full_year, mon as i64, day as i64);
    let secs = days * 86400 + hour as i64 * 3600 + min as i64 * 60 + sec as i64;
    if secs < 0 {
        return None;
    }
    Some(secs as u64)
}

// ---------------------------------------------------------------------------
// 目录 / 文件节点访问
// ---------------------------------------------------------------------------

/// 目录块的扩展索引块号 (0 = 无扩展)。
fn mfs_dir_ext(buf: *const u8) -> u32 {
    read_u32(mfs_at(buf, MFS_HDR))
}
fn mfs_dir_set_ext(buf: *mut u8, b: u32) {
    write_u32(mfs_atm(buf, MFS_HDR), b);
}
/// 把目录块初始化成「空目录」(无扩展 + 一个覆盖全区的大空槽)。
fn mfs_dir_init_empty(buf: *mut u8) {
    mfs_dir_set_ext(buf, 0);
    write_u32(mfs_atm(buf, MFS_HDR + 4), 0); // pad
    mfs_ent_clear(buf, MFS_HDR + MFS_DIR_HDR, MFS_DIR_AREA);
}

// --- 变长条目 (`off` = 条目在块缓冲内、相对块首的绝对偏移) ---
//
// 名字不再以 NUL 结尾: 长度由 `name_len` 给出, 条目按 `rec_len` 串联。

fn mfs_ent_rec_len(buf: *const u8, off: usize) -> usize {
    read_u16(mfs_at(buf, off + 6)) as usize
}
fn mfs_ent_name_len(buf: *const u8, off: usize) -> usize {
    unsafe { *mfs_at(buf, off + 5) as usize }
}
/// 条目指向的 **inode 号** (MFS6 起该字段不再是块号, 需经 inode 表翻译才能得到块)。
fn mfs_ent_ino(buf: *const u8, off: usize) -> u32 {
    read_u32(mfs_at(buf, off))
}
fn mfs_ent_type(buf: *const u8, off: usize) -> u32 {
    unsafe { *mfs_at(buf, off + 4) as u32 }
}
/// 条目名字是否等于 `comp`。
fn mfs_ent_name_eq(buf: *const u8, off: usize, comp: &[u8]) -> bool {
    let n = mfs_ent_name_len(buf, off);
    if n == 0 || n != comp.len() {
        return false;
    }
    for (i, &c) in comp.iter().enumerate() {
        if unsafe { *mfs_at(buf, off + MFS_DIR_ENT_HDR + i) } != c {
            return false;
        }
    }
    true
}
/// 名字长度 → 条目所需字节数 (4 字节对齐)。
fn mfs_ent_need(name_len: usize) -> usize {
    (MFS_DIR_ENT_HDR + name_len + 3) & !3
}
/// 写入一个条目 (`rec_len` 必须 >= `mfs_ent_need(comp.len())`)。
fn mfs_ent_fill(buf: *mut u8, off: usize, comp: &[u8], child: u32, typ: u32, rec_len: usize) {
    write_u32(mfs_atm(buf, off), child);
    unsafe {
        *mfs_atm(buf, off + 4) = typ as u8;
        *mfs_atm(buf, off + 5) = comp.len() as u8;
    }
    write_u16(mfs_atm(buf, off + 6), rec_len as u16);
    for (i, &c) in comp.iter().enumerate() {
        unsafe {
            *mfs_atm(buf, off + MFS_DIR_ENT_HDR + i) = c;
        }
    }
}
/// 把一段区域写成空槽 (`name_len == 0`)。
fn mfs_ent_clear(buf: *mut u8, off: usize, rec_len: usize) {
    write_u32(mfs_atm(buf, off), 0);
    unsafe {
        *mfs_atm(buf, off + 4) = 0;
        *mfs_atm(buf, off + 5) = 0;
    }
    write_u16(mfs_atm(buf, off + 6), rec_len as u16);
}
/// 按 `rec_len` 走到下一个条目; 结构损坏 (rec_len 太小 / 越界) 返回 None。
///
/// 走到区尾会返回恰好等于区尾偏移的值, 由调用方的 `off + MFS_DIR_ENT_HDR <= end`
/// 循环条件负责收尾 —— 这样「正常结束」与「损坏」不会混淆 (GC 需要区分二者)。
fn mfs_ent_step(buf: *const u8, off: usize) -> Option<usize> {
    let rl = mfs_ent_rec_len(buf, off);
    let end = MFS_HDR + MFS_PAYLOAD;
    if rl < MFS_DIR_ENT_MIN || off + rl > end {
        return None;
    }
    Some(off + rl)
}
/// 在一个目录块里扫描名字, 命中返回条目偏移。
fn mfs_dir_scan(buf: *const u8, comp: &[u8]) -> Option<usize> {
    let end = MFS_HDR + MFS_PAYLOAD;
    let mut off = MFS_HDR + MFS_DIR_HDR;
    while off + MFS_DIR_ENT_HDR <= end {
        if mfs_ent_name_eq(buf, off, comp) {
            return Some(off);
        }
        off = mfs_ent_step(buf, off)?;
    }
    None
}
/// 找可容纳 `need` 字节的插入点, 返回 `(条目前偏移, 该条目 rec_len, 该条目已用长度)`。
///
/// 空槽 (`name_len == 0`, used = 0) 与「有效条目 rec_len 里多出来的余量」都算可用空间
/// (ext2 first-fit)。余量来自删除时把空出长度并给了前一条目。
fn mfs_dir_slot(buf: *const u8, need: usize) -> Option<(usize, usize, usize)> {
    let end = MFS_HDR + MFS_PAYLOAD;
    let mut off = MFS_HDR + MFS_DIR_HDR;
    while off + MFS_DIR_ENT_HDR <= end {
        let rl = mfs_ent_rec_len(buf, off);
        if rl < MFS_DIR_ENT_MIN || off + rl > end {
            return None;
        }
        let nl = mfs_ent_name_len(buf, off);
        let used = if nl == 0 { 0 } else { mfs_ent_need(nl) };
        if rl >= used + need {
            return Some((off, rl, used));
        }
        off += rl;
    }
    None
}
/// 把条目放进 `(off, rl, used)`: 从 `off + used` 起占用, 余量切成空槽。
///
/// 若 `used > 0` (切的是某条有效条目的余量), 必须先把该条目的 rec_len 缩回 `used`,
/// 否则它的 rec_len 会越过新条目, 串联链就跳过了新条目 (插入成功却查不到)。
fn mfs_ent_place(
    buf: *mut u8,
    off: usize,
    rl: usize,
    used: usize,
    comp: &[u8],
    child: u32,
    typ: u32,
) {
    let need = mfs_ent_need(comp.len());
    let avail = rl - used;
    let take = if avail - need >= MFS_DIR_ENT_MIN {
        need
    } else {
        avail
    };
    let eoff = off + used;
    if used > 0 {
        write_u16(mfs_atm(buf, off + 6), used as u16);
    }
    mfs_ent_fill(buf, eoff, comp, child, typ, take);
    if take < avail {
        mfs_ent_clear(buf, eoff + take, avail - take);
    }
}
/// 目录块里是否存在有效条目。
fn mfs_dir_has_entry(buf: *const u8) -> bool {
    let end = MFS_HDR + MFS_PAYLOAD;
    let mut off = MFS_HDR + MFS_DIR_HDR;
    while off + MFS_DIR_ENT_HDR <= end {
        if mfs_ent_name_len(buf, off) != 0 {
            return true;
        }
        match mfs_ent_step(buf, off) {
            Some(n) => off = n,
            None => return false,
        }
    }
    false
}

/// 在目录 (`dir_ino`, 含扩展块) 中查找条目。用 A(节点)/B(索引块)/C(扩展块) 缓冲。
fn mfs_dir_lookup(dir_ino: u32, comp: &[u8]) -> Option<MfsLoc> {
    let base = mfs_ino_block(dir_ino)?;
    if base == 0 {
        return None;
    }
    let a = mfs_a();
    if !mfs_read_blk(base, a) || !mfs_ok(a, MFS_MAGIC_DIR) {
        return None;
    }
    if let Some(off) = mfs_dir_scan(a, comp) {
        return Some(MfsLoc {
            dir_ino,
            blk: base,
            off,
        });
    }
    let ext = mfs_dir_ext(a);
    if ext == 0 {
        return None;
    }
    let b = mfs_b();
    if !mfs_read_blk(ext, b) || !mfs_ok(b, MFS_MAGIC_DIDX) {
        return None;
    }
    let c = mfs_c();
    for i in 0..MFS_DIR_SLOTS {
        let blk = read_u32(mfs_at(b, MFS_HDR + i * 4));
        if blk == 0 {
            continue;
        }
        if !mfs_read_blk(blk, c) || !mfs_ok(c, MFS_MAGIC_DIR) {
            continue;
        }
        if let Some(off) = mfs_dir_scan(c, comp) {
            return Some(MfsLoc { dir_ino, blk, off });
        }
    }
    None
}

/// 把条目所在块读入 C 缓冲 (供修改)。
fn mfs_dir_load_loc(loc: &MfsLoc) -> bool {
    let c = mfs_c();
    mfs_read_blk(loc.blk, c) && mfs_ok(c, MFS_MAGIC_DIR)
}

/// 把已修改的条目所在块 (C) 写回。返回是否成功。
///
/// 条目就在目录节点块里 → 直接经 `mfs_commit_object` 提交 (换块 + 同步表槽);
/// 否则要 COW 扩展块 + 更新索引块槽位 + 回写节点块。层数固定 3 层 (不是沿链表级联),
/// 故修改代价与目录大小无关, 也与目录深度无关。
///
/// 目录内容被改动 → 顺带刷新其 mtime/ctime (节点块本来就要 COW, 不产生额外块)。
fn mfs_dir_store_loc(loc: &MfsLoc) -> bool {
    if Some(loc.blk) == mfs_ino_block(loc.dir_ino) {
        mfs_touch(mfs_c(), true);
        return mfs_commit_object(loc.dir_ino, mfs_c(), MFS_MAGIC_DIR).is_some();
    }
    let new_blk = match mfs_commit(mfs_c(), MFS_MAGIC_DIR) {
        Some(b) => b,
        None => return false,
    };
    let base = match mfs_ino_block(loc.dir_ino) {
        Some(b) if b != 0 => b,
        _ => return false,
    };
    let a = mfs_a();
    if !mfs_read_blk(base, a) || !mfs_ok(a, MFS_MAGIC_DIR) {
        return false;
    }
    mfs_touch(a, true);
    let idx = mfs_dir_ext(a);
    if idx == 0 {
        return false;
    }
    let b = mfs_b();
    if !mfs_read_blk(idx, b) || !mfs_ok(b, MFS_MAGIC_DIDX) {
        return false;
    }
    let mut found = false;
    for i in 0..MFS_DIR_SLOTS {
        if read_u32(mfs_at(b, MFS_HDR + i * 4)) == loc.blk {
            write_u32(mfs_atm(b, MFS_HDR + i * 4), new_blk);
            found = true;
            break;
        }
    }
    if !found {
        return false;
    }
    let new_idx = match mfs_commit(b, MFS_MAGIC_DIDX) {
        Some(x) => x,
        None => return false,
    };
    mfs_dir_set_ext(a, new_idx);
    mfs_commit_object(loc.dir_ino, a, MFS_MAGIC_DIR).is_some()
}

/// 目录是否为空 (无任何有效条目)。
fn mfs_dir_is_empty(dir_ino: u32) -> Option<bool> {
    let base = mfs_ino_block(dir_ino)?;
    if base == 0 {
        return None;
    }
    let a = mfs_a();
    if !mfs_read_blk(base, a) || !mfs_ok(a, MFS_MAGIC_DIR) {
        return None;
    }
    if mfs_dir_has_entry(a) {
        return Some(false);
    }
    let ext = mfs_dir_ext(a);
    if ext == 0 {
        return Some(true);
    }
    let b = mfs_b();
    if !mfs_read_blk(ext, b) || !mfs_ok(b, MFS_MAGIC_DIDX) {
        return None;
    }
    let c = mfs_c();
    for i in 0..MFS_DIR_SLOTS {
        let blk = read_u32(mfs_at(b, MFS_HDR + i * 4));
        if blk == 0 {
            continue;
        }
        if !mfs_read_blk(blk, c) || !mfs_ok(c, MFS_MAGIC_DIR) {
            continue;
        }
        if mfs_dir_has_entry(c) {
            return Some(false);
        }
    }
    Some(true)
}

/// 在目录 `dir_ino` 中插入条目 `comp -> child_ino`, 返回是否成功。
///
/// 内部完成扩展块 / 索引块的 COW, 最后经 `mfs_commit_object` 换掉目录节点块并同步
/// 它的 inode 表槽 —— 父目录条目存的是 ino, 故**不需要**回写任何祖先。
fn mfs_dir_insert(dir_ino: u32, comp: &[u8], child_ino: u32, typ: u32) -> bool {
    if comp.is_empty() || comp.len() > MFS_NAME_MAX {
        return false;
    }
    let need = mfs_ent_need(comp.len());
    let base = match mfs_ino_block(dir_ino) {
        Some(b) if b != 0 => b,
        _ => return false,
    };
    let a = mfs_a();
    if !mfs_read_blk(base, a) || !mfs_ok(a, MFS_MAGIC_DIR) {
        return false;
    }
    mfs_touch(a, true);
    // 1) 节点块内还有空间。
    if let Some((off, rl, used)) = mfs_dir_slot(a, need) {
        mfs_ent_place(a, off, rl, used, comp, child_ino, typ);
        return mfs_commit_object(dir_ino, a, MFS_MAGIC_DIR).is_some();
    }
    // 2) 取索引块 (不存在则物化一个空索引块并在 A 中登记)。
    let b = mfs_b();
    let mut idx = mfs_dir_ext(a);
    if idx == 0 {
        zero_bytes(b, MFS_BLOCK);
        idx = match mfs_commit(b, MFS_MAGIC_DIDX) {
            Some(x) => x,
            None => return false,
        };
        mfs_dir_set_ext(a, idx);
    } else if !mfs_read_blk(idx, b) || !mfs_ok(b, MFS_MAGIC_DIDX) {
        return false;
    }
    // 3) 已有扩展块里有空间。
    let c = mfs_c();
    for i in 0..MFS_DIR_SLOTS {
        let blk = read_u32(mfs_at(b, MFS_HDR + i * 4));
        if blk == 0 {
            continue;
        }
        if !mfs_read_blk(blk, c) || !mfs_ok(c, MFS_MAGIC_DIR) {
            continue;
        }
        let (off, rl, used) = match mfs_dir_slot(c, need) {
            Some(x) => x,
            None => continue,
        };
        mfs_ent_place(c, off, rl, used, comp, child_ino, typ);
        let new_ext = match mfs_commit(c, MFS_MAGIC_DIR) {
            Some(x) => x,
            None => return false,
        };
        write_u32(mfs_atm(b, MFS_HDR + i * 4), new_ext);
        let new_idx = match mfs_commit(b, MFS_MAGIC_DIDX) {
            Some(x) => x,
            None => return false,
        };
        mfs_dir_set_ext(a, new_idx);
        return mfs_commit_object(dir_ino, a, MFS_MAGIC_DIR).is_some();
    }
    // 4) 新增一个扩展块。
    zero_bytes(c, MFS_BLOCK);
    mfs_dir_init_empty(c);
    let (off, rl, used) = match mfs_dir_slot(c, need) {
        Some(x) => x,
        None => return false,
    };
    mfs_ent_place(c, off, rl, used, comp, child_ino, typ);
    let new_ext = match mfs_commit(c, MFS_MAGIC_DIR) {
        Some(x) => x,
        None => return false,
    };
    let mut placed = false;
    for i in 0..MFS_DIR_SLOTS {
        if read_u32(mfs_at(b, MFS_HDR + i * 4)) == 0 {
            write_u32(mfs_atm(b, MFS_HDR + i * 4), new_ext);
            placed = true;
            break;
        }
    }
    if !placed {
        return false; // 索引块满 (1022 个扩展块), 实际不可达
    }
    let new_idx = match mfs_commit(b, MFS_MAGIC_DIDX) {
        Some(x) => x,
        None => return false,
    };
    mfs_dir_set_ext(a, new_idx);
    mfs_commit_object(dir_ino, a, MFS_MAGIC_DIR).is_some()
}

/// 删除目录条目 (释放的长度并给前一项以回收碎片), 返回是否成功。
fn mfs_dir_delete(loc: &MfsLoc) -> bool {
    if !mfs_dir_load_loc(loc) {
        return false;
    }
    let c = mfs_c();
    let rl = mfs_ent_rec_len(c, loc.off);
    let end = MFS_HDR + MFS_PAYLOAD;
    let mut prev = None;
    let mut off = MFS_HDR + MFS_DIR_HDR;
    while off + MFS_DIR_ENT_HDR <= end && off < loc.off {
        let r = mfs_ent_rec_len(c, off);
        if r < MFS_DIR_ENT_MIN || off + r > end {
            return false;
        }
        prev = Some(off);
        off += r;
    }
    match prev {
        Some(p) => {
            let prl = mfs_ent_rec_len(c, p);
            write_u16(mfs_atm(c, p + 6), (prl + rl) as u16);
        }
        None => mfs_ent_clear(c, loc.off, rl),
    }
    mfs_dir_store_loc(loc)
}

fn mfs_file_size(buf: *const u8) -> u64 {
    read_u64(mfs_at(buf, MFS_FILE_SIZE_OFF))
}
fn mfs_file_set_size(buf: *mut u8, v: u64) {
    write_u64(mfs_atm(buf, MFS_FILE_SIZE_OFF), v);
}
fn mfs_file_nblocks(buf: *const u8) -> u32 {
    read_u32(mfs_at(buf, MFS_FILE_NBLOCKS_OFF))
}
fn mfs_file_set_nblocks(buf: *mut u8, v: u32) {
    write_u32(mfs_atm(buf, MFS_FILE_NBLOCKS_OFF), v);
}
/// 直接块指针 (i < `MFS_FILE_DIRECT`)。
fn mfs_file_direct(buf: *const u8, i: usize) -> u32 {
    read_u32(mfs_at(buf, MFS_FILE_DIRECT_OFF + i * 4))
}
fn mfs_file_set_direct(buf: *mut u8, i: usize, b: u32) {
    write_u32(mfs_atm(buf, MFS_FILE_DIRECT_OFF + i * 4), b);
}
fn mfs_file_ind1(buf: *const u8) -> u32 {
    read_u32(mfs_at(buf, MFS_FILE_IND1_OFF))
}
fn mfs_file_set_ind1(buf: *mut u8, b: u32) {
    write_u32(mfs_atm(buf, MFS_FILE_IND1_OFF), b);
}
fn mfs_file_ind2(buf: *const u8) -> u32 {
    read_u32(mfs_at(buf, MFS_FILE_IND2_OFF))
}
fn mfs_file_set_ind2(buf: *mut u8, b: u32) {
    write_u32(mfs_atm(buf, MFS_FILE_IND2_OFF), b);
}
fn mfs_file_ind3(buf: *const u8) -> u32 {
    read_u32(mfs_at(buf, MFS_FILE_IND3_OFF))
}
fn mfs_file_set_ind3(buf: *mut u8, b: u32) {
    write_u32(mfs_atm(buf, MFS_FILE_IND3_OFF), b);
}
/// 间接块内的第 `slot` 个指针。
fn mfs_ind_slot(buf: *const u8, slot: usize) -> u32 {
    read_u32(mfs_at(buf, MFS_HDR + slot * 4))
}
fn mfs_ind_set_slot(buf: *mut u8, slot: usize, b: u32) {
    write_u32(mfs_atm(buf, MFS_HDR + slot * 4), b);
}

// ---------------------------------------------------------------------------
// 逻辑块映射 (直接 -> 一级 -> 二级 -> 三级间接)
// ---------------------------------------------------------------------------

/// 逻辑块 `bi` 的映射位置。
///
/// `kind` 0 = inode 直接槽 (只用 `slot1`), 1 = 一级间接 (只用 `slot1`),
/// 2 = 二级间接 (`slot2` 定位一级块, `slot1` 定位块内槽位),
/// 3 = 三级间接 (`slot3` 定位二级块, `slot2` 定位一级块, `slot1` 定位块内槽位)。
#[derive(Clone, Copy)]
struct MfsMapPlan {
    kind: u32,
    slot3: usize,
    slot2: usize,
    slot1: usize,
}

/// 把逻辑块索引算成映射位置; 超出 `MFS_FILE_MAX_BLOCKS` 返回 None。
fn mfs_map_plan(bi: usize) -> Option<MfsMapPlan> {
    if bi < MFS_FILE_DIRECT {
        return Some(MfsMapPlan {
            kind: 0,
            slot3: 0,
            slot2: 0,
            slot1: bi,
        });
    }
    let idx = bi - MFS_FILE_DIRECT;
    if idx < MFS_IND_CAP {
        return Some(MfsMapPlan {
            kind: 1,
            slot3: 0,
            slot2: 0,
            slot1: idx,
        });
    }
    let idx = idx - MFS_IND_CAP;
    if idx < MFS_IND_CAP * MFS_IND_CAP {
        let slot2 = idx / MFS_IND_CAP;
        return Some(MfsMapPlan {
            kind: 2,
            slot3: 0,
            slot2,
            slot1: idx % MFS_IND_CAP,
        });
    }
    // 三级间接: idx3 先按二级块的容量 (CAP²) 切出 slot3, 余下再按一级块容量切。
    let idx3 = idx - MFS_IND_CAP * MFS_IND_CAP;
    let slot3 = idx3 / (MFS_IND_CAP * MFS_IND_CAP);
    if slot3 >= MFS_IND_CAP {
        return None;
    }
    let rest = idx3 % (MFS_IND_CAP * MFS_IND_CAP);
    Some(MfsMapPlan {
        kind: 3,
        slot3,
        slot2: rest / MFS_IND_CAP,
        slot1: rest % MFS_IND_CAP,
    })
}

/// 读路径: 逻辑块 `bi` 的物理块号 (0 = 空洞); 读盘/校验失败返回 None。
///
/// 用 B 缓冲承载间接块 (读路径 A = inode, C = 数据块, B 空闲); 每级读完立刻取走需要
/// 的槽值再复用, 故单缓冲即可逐级下降。
fn mfs_file_map(a: *const u8, bi: usize) -> Option<u32> {
    let p = mfs_map_plan(bi)?;
    if p.kind == 0 {
        return Some(mfs_file_direct(a, p.slot1));
    }
    let b = mfs_b();
    let l1 = if p.kind == 1 {
        mfs_file_ind1(a)
    } else if p.kind == 2 {
        let l2 = mfs_file_ind2(a);
        if l2 == 0 {
            return Some(0);
        }
        if !mfs_read_blk(l2, b) || !mfs_ok(b, MFS_MAGIC_IND2) {
            return None;
        }
        mfs_ind_slot(b, p.slot2)
    } else {
        // 三级: ind3 -> ind2 -> ind1, 任一指针为 0 即空洞。
        let l3 = mfs_file_ind3(a);
        if l3 == 0 {
            return Some(0);
        }
        if !mfs_read_blk(l3, b) || !mfs_ok(b, MFS_MAGIC_IND3) {
            return None;
        }
        let l2 = mfs_ind_slot(b, p.slot3);
        if l2 == 0 {
            return Some(0);
        }
        if !mfs_read_blk(l2, b) || !mfs_ok(b, MFS_MAGIC_IND2) {
            return None;
        }
        mfs_ind_slot(b, p.slot2)
    };
    if l1 == 0 {
        return Some(0);
    }
    if !mfs_read_blk(l1, b) || !mfs_ok(b, MFS_MAGIC_IND) {
        return None;
    }
    Some(mfs_ind_slot(b, p.slot1))
}

/// 写路径的「活动间接块」缓存。
///
/// 一次写调用常跨 1~2 个逻辑块且多落在同一个间接块内; 缓存它可避免对同一个间接块
/// 反复「读-改-COW」(间接块自身也是要写盘的 4 KiB 块)。缓冲同样借用 B:
/// 写数据阶段 A = inode、C = 数据块、B 归本缓存 (COW 上溯才用 B, 那时已 flush)。
#[derive(Clone, Copy)]
struct MfsIndCache {
    /// 0 = 未持有; 1 / 2 同 `MfsMapPlan::kind`。
    kind: u32,
    /// kind == 2 时: 该一级块在二级块中的槽位。
    slot2: usize,
    /// 缓冲内是否有未写回的修改。
    dirty: bool,
}
const MFS_IND_CACHE_EMPTY: MfsIndCache = MfsIndCache {
    kind: 0,
    slot2: 0,
    dirty: false,
};

/// 把缓存里的一级间接块 COW 出去并回写父级 (inode 的一级指针或二级块槽位)。
fn mfs_ind_flush(a: *mut u8, cache: &mut MfsIndCache) -> bool {
    if cache.kind == 0 || !cache.dirty {
        return true;
    }
    let b = mfs_b();
    let new_l1 = match mfs_commit(b, MFS_MAGIC_IND) {
        Some(x) => x,
        None => return false,
    };
    if cache.kind == 1 {
        mfs_file_set_ind1(a, new_l1);
    } else {
        // 二级: 读二级块 (B 已被 commit 占用, 一级内容已落盘, 可安全复用) -> 改槽 ->
        // COW 二级块 -> 回写 inode。二级块在加载该一级块时已保证存在。
        let l2 = mfs_file_ind2(a);
        if l2 == 0 || !mfs_read_blk(l2, b) || !mfs_ok(b, MFS_MAGIC_IND2) {
            return false;
        }
        mfs_ind_set_slot(b, cache.slot2, new_l1);
        let new_l2 = match mfs_commit(b, MFS_MAGIC_IND2) {
            Some(x) => x,
            None => return false,
        };
        mfs_file_set_ind2(a, new_l2);
    }
    cache.dirty = false;
    true
}

/// 让缓存持有覆盖 `bi` 的一级间接块 (必要时先 flush 旧组再加载新组)。
fn mfs_ind_load(a: *mut u8, bi: usize, cache: &mut MfsIndCache) -> bool {
    let p = match mfs_map_plan(bi) {
        Some(p) => p,
        None => return false,
    };
    if p.kind == 0 {
        // 直接槽不经间接块; 若缓存里还压着脏块则先落盘 (正常路径 bi 单调递增,
        // 先走直接区再走间接区, 不会碰到; 这里只为不给「静默丢改动」留口子)。
        if !mfs_ind_flush(a, cache) {
            return false;
        }
        cache.kind = 0;
        return true;
    }
    if cache.kind == p.kind && cache.slot2 == p.slot2 {
        return true;
    }
    if !mfs_ind_flush(a, cache) {
        return false;
    }
    let b = mfs_b();
    if p.kind == 1 {
        let l1 = mfs_file_ind1(a);
        if l1 == 0 {
            zero_bytes(b, MFS_BLOCK);
        } else if !mfs_read_blk(l1, b) || !mfs_ok(b, MFS_MAGIC_IND) {
            return false;
        }
    } else {
        // 二级: 二级块不存在就当场建一个空块 (flush 需要它存在), 再取其中的一级块。
        let mut l2 = mfs_file_ind2(a);
        if l2 == 0 {
            zero_bytes(b, MFS_BLOCK);
            l2 = match mfs_commit(b, MFS_MAGIC_IND2) {
                Some(x) => x,
                None => return false,
            };
            mfs_file_set_ind2(a, l2);
        } else if !mfs_read_blk(l2, b) || !mfs_ok(b, MFS_MAGIC_IND2) {
            return false;
        }
        let l1 = mfs_ind_slot(b, p.slot2);
        if l1 == 0 {
            zero_bytes(b, MFS_BLOCK);
        } else if !mfs_read_blk(l1, b) || !mfs_ok(b, MFS_MAGIC_IND) {
            return false;
        }
    }
    cache.kind = p.kind;
    cache.slot2 = p.slot2;
    cache.dirty = false;
    true
}

/// 写路径: 读逻辑块 `bi` 当前的物理块号 (0 = 空洞), 顺带把它的间接块载入缓存。
///
/// 三级间接块 (kind 3) 不由缓存承载: 先 flush 并清空缓存, 再走 `mfs_ind_peek3` ——
/// 否则缓存里的一级块内容会与三级链路共用的 B 缓冲相互覆盖。
fn mfs_ind_peek(a: *mut u8, bi: usize, cache: &mut MfsIndCache) -> Option<u32> {
    let p = mfs_map_plan(bi)?;
    if p.kind == 0 {
        return Some(mfs_file_direct(a, p.slot1));
    }
    if p.kind == 3 {
        if !mfs_ind_flush(a, cache) {
            return None;
        }
        *cache = MFS_IND_CACHE_EMPTY;
        return mfs_ind_peek3(a, &p);
    }
    if !mfs_ind_load(a, bi, cache) {
        return None;
    }
    Some(mfs_ind_slot(mfs_b(), p.slot1))
}

/// 写路径 (三级间接专用): 链式读取 ind3 -> ind2 -> ind1, 返回 ind1 里 `slot1` 的数据
/// 块号 (0 = 空洞)。
///
/// 三级只服务 >4 GiB 文件, 为保持缓存实现简单而走**无缓存链路** (常规文件不受影响);
/// 每级读完立刻取走所需槽值, 全程只用 B 一页缓冲。
fn mfs_ind_peek3(a: *mut u8, plan: &MfsMapPlan) -> Option<u32> {
    let b = mfs_b();
    let l3 = mfs_file_ind3(a);
    if l3 == 0 {
        return Some(0);
    }
    if !mfs_read_blk(l3, b) || !mfs_ok(b, MFS_MAGIC_IND3) {
        return None;
    }
    let l2 = mfs_ind_slot(b, plan.slot3);
    if l2 == 0 {
        return Some(0);
    }
    if !mfs_read_blk(l2, b) || !mfs_ok(b, MFS_MAGIC_IND2) {
        return None;
    }
    let l1 = mfs_ind_slot(b, plan.slot2);
    if l1 == 0 {
        return Some(0);
    }
    if !mfs_read_blk(l1, b) || !mfs_ok(b, MFS_MAGIC_IND) {
        return None;
    }
    Some(mfs_ind_slot(b, plan.slot1))
}

/// 写路径 (三级间接专用): 把 `bi` 指向 `db`, 自下而上逐级「读-改-COW」回写 ——
/// COW ind1 -> 写回父 ind2 的 `slot2` -> COW ind2 -> 写回 ind3 的 `slot3` -> COW ind3
/// -> 写回 inode 的 ind3 指针。中间级不存在时当场建空块 (与 `mfs_ind_load` 同策略)。
///
/// 全程只用 B 一页缓冲: 每级校验/建好之后立刻取走所需槽值, 再复用该页写下一级。
fn mfs_ind_link3(a: *mut u8, plan: &MfsMapPlan, db: u32) -> bool {
    let b = mfs_b();
    // 自上而下取出三级、二级、一级块号 (只读, 读完即取走槽值)。
    let l3 = mfs_file_ind3(a);
    let l2 = if l3 == 0 {
        0
    } else {
        if !mfs_read_blk(l3, b) || !mfs_ok(b, MFS_MAGIC_IND3) {
            return false;
        }
        mfs_ind_slot(b, plan.slot3)
    };
    let l1 = if l2 == 0 {
        0
    } else {
        if !mfs_read_blk(l2, b) || !mfs_ok(b, MFS_MAGIC_IND2) {
            return false;
        }
        mfs_ind_slot(b, plan.slot2)
    };
    // 一级: 载入 (不存在则清零当空块) -> 改 slot1 -> COW。
    if l1 == 0 {
        zero_bytes(b, MFS_BLOCK);
    } else if !mfs_read_blk(l1, b) || !mfs_ok(b, MFS_MAGIC_IND) {
        return false;
    }
    mfs_ind_set_slot(b, plan.slot1, db);
    let new_l1 = match mfs_commit(b, MFS_MAGIC_IND) {
        Some(x) => x,
        None => return false,
    };
    // 二级: 载入 -> 改 slot2 指向新一级块 -> COW。
    if l2 == 0 {
        zero_bytes(b, MFS_BLOCK);
    } else if !mfs_read_blk(l2, b) || !mfs_ok(b, MFS_MAGIC_IND2) {
        return false;
    }
    mfs_ind_set_slot(b, plan.slot2, new_l1);
    let new_l2 = match mfs_commit(b, MFS_MAGIC_IND2) {
        Some(x) => x,
        None => return false,
    };
    // 三级: 载入 -> 改 slot3 指向新二级块 -> COW -> 回写 inode 指针。
    if l3 == 0 {
        zero_bytes(b, MFS_BLOCK);
    } else if !mfs_read_blk(l3, b) || !mfs_ok(b, MFS_MAGIC_IND3) {
        return false;
    }
    mfs_ind_set_slot(b, plan.slot3, new_l2);
    let new_l3 = match mfs_commit(b, MFS_MAGIC_IND3) {
        Some(x) => x,
        None => return false,
    };
    mfs_file_set_ind3(a, new_l3);
    true
}

/// 写路径: 把逻辑块 `bi` 指向 `db` (直接槽写 inode; 三级块走无缓存链路; 否则写进缓存
/// 中的一级间接块)。
///
/// 调用前必须对同一个 `bi` 调过 `mfs_ind_peek` (保证目标组已载入缓存; 三级块除外)。
fn mfs_ind_link(a: *mut u8, bi: usize, db: u32, cache: &mut MfsIndCache) -> bool {
    let p = match mfs_map_plan(bi) {
        Some(p) => p,
        None => return false,
    };
    if p.kind == 0 {
        mfs_file_set_direct(a, p.slot1, db);
        return true;
    }
    if p.kind == 3 {
        return mfs_ind_link3(a, &p, db);
    }
    if cache.kind != p.kind || cache.slot2 != p.slot2 {
        return false;
    }
    mfs_ind_set_slot(mfs_b(), p.slot1, db);
    cache.dirty = true;
    true
}

/// 把存储名 (dotted 大写) 转成 11 字节 FAT 8.3 短名 (主名 8 + 扩展 3, 空格填充)。
///
/// MFS 名字不再以 NUL 结尾 (变长条目由 `name_len` 给出长度), 故这里收切片。
fn mfs_name_to_fat(name: &[u8], out: &mut [u8; 11]) {
    *out = [b' '; 11];
    let len = name.len().min(MFS_NAME_MAX);
    let seg = &name[..len];
    let mut dot = len;
    for (i, &c) in seg.iter().enumerate() {
        if c == b'.' {
            dot = i;
            break;
        }
    }
    let base = &seg[..dot];
    let ext = if dot < len {
        &seg[dot + 1..len]
    } else {
        &seg[len..len]
    };
    let bn = base.len().min(8);
    out[..bn].copy_from_slice(&base[..bn]);
    let en = ext.len().min(3);
    out[8..8 + en].copy_from_slice(&ext[..en]);
}

// ---------------------------------------------------------------------------
// 路径解析 + COW 上溯
// ---------------------------------------------------------------------------

/// 规范化 MFS 绝对路径: 处理 "." / ".." 与重复 '/', 分量**原样保留**。
///
/// 与 tmpfs 的 8.3 规整 (`tmp_normalize`) 不同: MFS v2 起名字是长度 ≤ `MFS_NAME_MAX`
/// 的任意字节串, 大小写敏感、按字节精确匹配 (与 ext2 一致), 不做大写化或截断。
/// 端到端长度另受单条 IPC 路径 (payload) 限制。返回长度; 越界/空分量返回 None。
fn mfs_normalize(path: &str, out: &mut [u8]) -> Option<usize> {
    let bytes = path.as_bytes();
    if bytes.first() != Some(&b'/') || out.is_empty() {
        return None;
    }
    out[0] = b'/';
    let mut n = 1usize;
    let mut i = 1usize;
    while i < bytes.len() {
        if bytes[i] == b'/' {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && bytes[i] != b'/' {
            i += 1;
        }
        let seg = &bytes[start..i];
        if seg == b"." {
            continue;
        }
        if seg == b".." {
            // 回退一级 (已在根时不动)。
            if n > 1 {
                let mut k = n - 1;
                while k > 0 && out[k - 1] != b'/' {
                    k -= 1;
                }
                n = if k > 1 { k - 1 } else { 1 };
            }
            continue;
        }
        if seg.len() > MFS_NAME_MAX {
            return None;
        }
        let sep = usize::from(n > 1);
        if n + sep + seg.len() > out.len() {
            return None;
        }
        if sep == 1 {
            out[n] = b'/';
            n += 1;
        }
        out[n..n + seg.len()].copy_from_slice(seg);
        n += seg.len();
    }
    Some(n)
}

/// 读软链接节点 `ino` 的目标路径到 `out`, 返回目标字节数 (不含 NUL)。
///
/// 目标**原样**存储在节点里 (以 '/' 开头 = 绝对路径, 否则相对链接所在目录); 这里也
/// 原样取出 —— 规范化与「绝对/相对」的判断都留给解析方。
fn mfs_link_target(ino: u32, out: &mut [u8]) -> Option<usize> {
    let blk = mfs_ino_block(ino)?;
    if blk == 0 {
        return None;
    }
    let a = mfs_a();
    if !mfs_read_blk(blk, a) || !mfs_ok(a, MFS_MAGIC_LINK) {
        return None;
    }
    let tlen = mfs_file_size(a) as usize;
    if tlen == 0 || tlen > MFS_LINK_MAX || tlen > out.len() {
        return None;
    }
    unsafe {
        core::ptr::copy_nonoverlapping(mfs_at(a, MFS_LINK_TARGET_OFF), out.as_mut_ptr(), tlen);
    }
    Some(tlen)
}

/// 解析规范化绝对路径, 返回叶子节点的 **inode 号**, 沿途**跟随软链接**。
///
/// 顺带把叶子条目位置记进 `MFS_LEAF` —— 需要改/删这个条目的调用方 (rename / unlink)
/// 直接用它。MFS6 起解析不再需要沿途记录整条链: 目录项存 ino, 改对象不影响祖先。
fn mfs_resolve(canon: &[u8]) -> Option<u32> {
    mfs_resolve_ex(canon, true)
}

/// 同 `mfs_resolve`, 但**不跟随最后一段**上的软链接。
///
/// `unlink` / `rmdir` / `rename` 作用在条目本身 (删除、移动的是链接而不是它的目标),
/// 用这个版本; 路径中间分量上的软链接仍然跟随 —— `/a/link/b` 必须走进 link 指到的
/// 那个目录才能找到 b。
fn mfs_resolve_no_follow(canon: &[u8]) -> Option<u32> {
    mfs_resolve_ex(canon, false)
}

/// 解析主体。`follow_leaf` = 最后一段是软链接时是否跟随。
///
/// 跟随的实现是「就地展开 + 整条重走」: 把路径里那个链接分量替换成它的目标 (绝对目标
/// 直接用, 相对目标接到链接所在目录之后), 保留其后的剩余分量, 重新规范化后从头再走
/// 一遍。不接着展开点往下走, 是因为目标里的 `..` 可能吃掉展开点**之前**的目录, 分量
/// 位置会整体变化; 从头重走只是多几次目录查找, 换来逻辑简单可靠。展开次数由
/// `MFS_SYMLINK_MAX_DEPTH` 兜底, 因此链接成环只会解析失败, 不会无限展开。
fn mfs_resolve_ex(canon: &[u8], follow_leaf: bool) -> Option<u32> {
    unsafe {
        MFS_LEAF = MFS_LOC_EMPTY;
    }
    if canon.len() > MFS_PATH_MAX {
        return None;
    }
    // 工作缓冲: 链接展开会就地重写整条路径。
    let mut buf = [0u8; MFS_PATH_MAX];
    let mut len = canon.len();
    buf[..len].copy_from_slice(canon);
    if len == 1 {
        return Some(MFS_ROOT_INO); // 根
    }
    let mut links = 0u32;
    loop {
        let mut i = 1usize;
        let mut cur = MFS_ROOT_INO;
        // 本轮走到的软链接分量 (需展开): (分量起点, 分量终点, 链接 ino)。
        let mut expand: Option<(usize, usize, u32)> = None;
        while i < len {
            let start = i;
            while i < len && buf[i] != b'/' {
                i += 1;
            }
            let comp_end = i;
            let comp_len = comp_end - start;
            if comp_len == 0 || comp_len > MFS_NAME_MAX {
                return None;
            }
            let loc = mfs_dir_lookup(cur, &buf[start..comp_end])?;
            // 条目可能落在扩展块里, 上一步的 C 缓冲已被覆盖 —— 重新读条目所在块。
            let (child_ino, is_link) = {
                let c = mfs_c();
                if !mfs_read_blk(loc.blk, c) || !mfs_ok(c, MFS_MAGIC_DIR) {
                    return None;
                }
                (
                    mfs_ent_ino(c, loc.off),
                    mfs_ent_type(c, loc.off) == MFS_TYPE_LINK,
                )
            };
            if child_ino == 0 {
                return None;
            }
            let is_leaf = comp_end >= len;
            if is_link && (!is_leaf || follow_leaf) {
                expand = Some((start, comp_end, child_ino));
                break;
            }
            unsafe {
                MFS_LEAF = loc;
            }
            cur = child_ino;
            if is_leaf {
                return Some(cur);
            }
            i = comp_end + 1; // 跳过 '/'
        }
        // 本轮没碰到软链接却走到了这里 —— 说明分量没走完 (规范化后的路径不该出现)。
        let (start, comp_end, link_ino) = expand?;
        links += 1;
        if links > MFS_SYMLINK_MAX_DEPTH {
            return None; // 链接成环 / 链过长
        }
        // 组装新路径: [父目录前缀] + 目标 + [剩余分量] (绝对目标不带父前缀)。
        let mut target = [0u8; MFS_LINK_MAX];
        let tlen = mfs_link_target(link_ino, &mut target)?;
        let parent = if target[0] == b'/' { 0 } else { start };
        let rest = &buf[comp_end..len]; // 以 '/' 开头, 或为空
        let mut merged = [0u8; MFS_PATH_MAX];
        let mut n = 0usize;
        if parent > 0 {
            merged[..parent].copy_from_slice(&buf[..parent]);
            n = parent;
        }
        if n + tlen + rest.len() > MFS_PATH_MAX {
            return None; // 展开后超长: 直接失败, 不截断成一条错路径
        }
        merged[n..n + tlen].copy_from_slice(&target[..tlen]);
        n += tlen;
        merged[n..n + rest.len()].copy_from_slice(rest);
        n += rest.len();
        // 目标里可能带 '.' / '..' / 重复 '/', 重新规范化后再整条重走。
        let mut canon2 = [0u8; MFS_PATH_MAX];
        let cn = mfs_normalize(
            unsafe { core::str::from_utf8_unchecked(&merged[..n]) },
            &mut canon2,
        )?;
        len = cn;
        buf[..len].copy_from_slice(&canon2[..len]);
        if len == 1 {
            return Some(MFS_ROOT_INO);
        }
    }
}

/// 读取文件 `ino` 的 [offset, offset+count) 区间到 `dst`, 返回读取字节数。
///
/// **空洞按 0 返回**: 逻辑块未分配 (`db == 0`) 或超出 `nblocks` 时, 该段视为稀疏空洞,
/// 填 0 后继续 —— 若在这里 break, 稀疏文件 (truncate 扩展出来的区段) 会读成短读。
fn mfs_read_file(ino: u32, offset: u64, count: u32, dst: *mut u8) -> u64 {
    let a = mfs_a();
    let block = match mfs_ino_block(ino) {
        Some(b) if b != 0 => b,
        _ => return u64::MAX,
    };
    if !mfs_read_blk(block, a) || !mfs_ok(a, MFS_MAGIC_FILE) {
        return u64::MAX;
    }
    let size = mfs_file_size(a);
    if offset >= size {
        return 0;
    }
    let end = core::cmp::min(offset + count as u64, size);
    let n = (end - offset) as u32;
    let c = mfs_c();
    let mut done = 0u32;
    while done < n {
        let pos = offset + done as u64;
        let bi = (pos as usize) / MFS_DATA_CAP;
        let boff = (pos as usize) % MFS_DATA_CAP;
        let chunk = core::cmp::min(MFS_DATA_CAP - boff, (n - done) as usize);
        let db = if bi >= mfs_file_nblocks(a) as usize {
            0 // 超出已分配的逻辑块数 -> 空洞
        } else {
            match mfs_file_map(a, bi) {
                Some(x) => x,
                None => return u64::MAX,
            }
        };
        if db == 0 {
            unsafe {
                core::ptr::write_bytes(dst.add(done as usize), 0, chunk);
            }
            done += chunk as u32;
            continue;
        }
        if !mfs_read_blk(db, c) || !mfs_ok(c, MFS_MAGIC_DATA) {
            return u64::MAX;
        }
        unsafe {
            core::ptr::copy_nonoverlapping(
                mfs_at(c, MFS_HDR + boff),
                dst.add(done as usize),
                chunk,
            );
        }
        done += chunk as u32;
    }
    done as u64
}

/// 写文件 `ino` 的 [offset, offset+count) 区间, COW 数据块 (必要时含一/二/三级间接块) +
/// 文件节点, 再同步它在 inode 表里的槽位。返回写入字节数, 失败 `u64::MAX`。
fn mfs_write_file(ino: u32, offset: u64, count: u32, src: *const u8) -> u64 {
    let a = mfs_a();
    let block = match mfs_ino_block(ino) {
        Some(b) if b != 0 => b,
        _ => return u64::MAX,
    };
    if !mfs_read_blk(block, a) || !mfs_ok(a, MFS_MAGIC_FILE) {
        return u64::MAX;
    }
    let old_size = mfs_file_size(a);
    let mut nblocks = mfs_file_nblocks(a) as usize;
    let c = mfs_c();
    // 结束位置按 64 位算: 文件上限已与卷容量同量级, 只需挡住超出结构可寻址范围的写入。
    let end = offset + count as u64;
    let max_bytes = MFS_FILE_MAX_BLOCKS as u64 * MFS_DATA_CAP as u64;
    if end > max_bytes {
        return u64::MAX;
    }
    let mut cache = MFS_IND_CACHE_EMPTY;
    let mut done = 0u32;
    while done < count {
        let pos = offset + done as u64;
        let bi = (pos as usize) / MFS_DATA_CAP;
        let boff = (pos as usize) % MFS_DATA_CAP;
        let chunk = core::cmp::min(MFS_DATA_CAP - boff, (count - done) as usize);
        if bi >= MFS_FILE_MAX_BLOCKS {
            return u64::MAX;
        }
        // 读旧数据块 (存在则复制, 否则清零); 顺带把该块所属的一级间接块载入缓存。
        let old_db = match mfs_ind_peek(a, bi, &mut cache) {
            Some(x) => x,
            None => return u64::MAX,
        };
        if old_db != 0 {
            if !mfs_read_blk(old_db, c) || !mfs_ok(c, MFS_MAGIC_DATA) {
                return u64::MAX;
            }
        } else {
            zero_bytes(c, MFS_BLOCK);
        }
        unsafe {
            core::ptr::copy_nonoverlapping(
                src.add(done as usize),
                mfs_atm(c, MFS_HDR + boff),
                chunk,
            );
        }
        let new_db = match mfs_commit(c, MFS_MAGIC_DATA) {
            Some(b) => b,
            None => return u64::MAX,
        };
        if !mfs_ind_link(a, bi, new_db, &mut cache) {
            return u64::MAX;
        }
        // 追加块: 只需抬高逻辑块数 (未分配槽位恒为 0, 空洞无需显式填写)。
        if bi >= nblocks {
            nblocks = bi + 1;
            mfs_file_set_nblocks(a, nblocks as u32);
        }
        done += chunk as u32;
    }
    // 收尾: 把缓存里最后一个间接块 COW 落盘并回写 inode 指针, 再提交新 inode。
    if !mfs_ind_flush(a, &mut cache) {
        return u64::MAX;
    }
    if end > old_size {
        mfs_file_set_size(a, end);
    }
    mfs_touch(a, false); // 内容变更 -> mtime / ctime
    if mfs_commit_object(ino, a, MFS_MAGIC_FILE).is_none() {
        return u64::MAX;
    }
    count as u64
}

/// 清掉逻辑块 `bi` 的映射 (直接槽或间接块槽位置 0), 返回是否成功。
///
/// 三级块经无缓存链路 (`mfs_ind_peek3` / `mfs_ind_link3`) 处理; 一/二级仍走缓存。
/// 只清指针、不回收块 —— 块由 GC 按可达性回收, 故这里无需关心"谁还在用"。
fn mfs_unmap_block(a: *mut u8, bi: usize, cache: &mut MfsIndCache) -> bool {
    let old = match mfs_ind_peek(a, bi, cache) {
        Some(x) => x,
        None => return false,
    };
    if old == 0 {
        return true;
    }
    mfs_ind_link(a, bi, 0, cache)
}

/// 把文件 `ino` 的长度改到 `new_size` 字节, 成功返回 1, 失败 `u64::MAX`。
///
/// - `new_size < 现有长度`: 截短。保留前 `keep = ceil(new_size / MFS_DATA_CAP)` 个逻辑块,
///   其余映射一律清 0。整段不再需要时直接丢一/二/三级指针 (块本体交给 GC), 只有部分保留
///   的那一段才逐槽清理。
/// - `new_size > 现有长度`: 稀疏扩展 —— 只抬高 `size`, 不分配块 (未写过的区间读回 0)。
fn mfs_truncate(ino: u32, new_size: u64) -> u64 {
    let a = mfs_a();
    let block = match mfs_ino_block(ino) {
        Some(b) if b != 0 => b,
        _ => return u64::MAX,
    };
    if !mfs_read_blk(block, a) || !mfs_ok(a, MFS_MAGIC_FILE) {
        return u64::MAX;
    }
    let old_size = mfs_file_size(a);
    if new_size == old_size {
        return 1;
    }
    if new_size > old_size {
        mfs_file_set_size(a, new_size);
        mfs_touch(a, false);
        return if mfs_commit_object(ino, a, MFS_MAGIC_FILE).is_some() {
            1
        } else {
            u64::MAX
        };
    }
    // 截短: 保留前 `keep` 个逻辑块。
    let keep = (new_size as usize).div_ceil(MFS_DATA_CAP);
    let old_nb = mfs_file_nblocks(a) as usize;
    let dir_end = MFS_FILE_DIRECT;
    let ind1_end = dir_end + MFS_IND_CAP;
    let ind2_end = ind1_end + MFS_IND_CAP * MFS_IND_CAP;
    let mut cache = MFS_IND_CACHE_EMPTY;
    // 1) 直接区尾部逐槽清零。
    let mut bi = keep;
    while bi < dir_end && bi < old_nb {
        mfs_file_set_direct(a, bi, 0);
        bi += 1;
    }
    // 2) 一级间接区: 整组保留 / 整组丢弃 / 部分保留。
    if old_nb > dir_end {
        if keep <= dir_end {
            mfs_file_set_ind1(a, 0);
        } else {
            let mut b = keep.max(dir_end);
            while b < ind1_end && b < old_nb {
                if !mfs_unmap_block(a, b, &mut cache) {
                    return u64::MAX;
                }
                b += 1;
            }
        }
    }
    // 3) 二级间接区同理 (循环上界到 ind2_end, 三级区留给下一步)。
    if old_nb > ind1_end {
        if keep <= ind1_end {
            mfs_file_set_ind2(a, 0);
        } else {
            let mut b = keep.max(ind1_end);
            while b < ind2_end && b < old_nb {
                if !mfs_unmap_block(a, b, &mut cache) {
                    return u64::MAX;
                }
                b += 1;
            }
        }
    }
    // 4) 三级间接区: 不在保留范围内就整段丢指针; 否则逐槽清理 (走无缓存链路)。
    if old_nb > ind2_end {
        if keep <= ind2_end {
            mfs_file_set_ind3(a, 0);
        } else {
            let mut b = keep.max(ind2_end);
            while b < old_nb {
                if !mfs_unmap_block(a, b, &mut cache) {
                    return u64::MAX;
                }
                b += 1;
            }
        }
    }
    if !mfs_ind_flush(a, &mut cache) {
        return u64::MAX;
    }
    // 5) 最后一个保留块若只用到一半, 把尾部清零 —— 否则日后扩展回来会读出截断前的旧数据
    //    (Linux ftruncate 同样会清掉部分块的尾部, 这里与之一致; 只在非块对齐时付一次块 COW)。
    let tail = new_size as usize % MFS_DATA_CAP;
    if keep > 0 && tail != 0 {
        let last = keep - 1;
        let old_db = match mfs_ind_peek(a, last, &mut cache) {
            Some(x) => x,
            None => return u64::MAX,
        };
        if old_db != 0 {
            let c = mfs_c();
            if !mfs_read_blk(old_db, c) || !mfs_ok(c, MFS_MAGIC_DATA) {
                return u64::MAX;
            }
            zero_bytes(mfs_atm(c, MFS_HDR + tail), MFS_DATA_CAP - tail);
            let new_db = match mfs_commit(c, MFS_MAGIC_DATA) {
                Some(b) => b,
                None => return u64::MAX,
            };
            if !mfs_ind_link(a, last, new_db, &mut cache) {
                return u64::MAX;
            }
            if !mfs_ind_flush(a, &mut cache) {
                return u64::MAX;
            }
        }
    }
    mfs_file_set_size(a, new_size);
    mfs_file_set_nblocks(a, keep as u32);
    mfs_touch(a, false);
    if mfs_commit_object(ino, a, MFS_MAGIC_FILE).is_none() {
        return u64::MAX;
    }
    1
}

/// 把 `src` 重命名 / 移动到 `dst` (可跨目录, 必须在同一文件服务内), 成功返回 1。
///
/// 顺序刻意做成「先建新名 → 再删旧名」而不是反过来:
/// 前者中途最多是同一 inode 被两个名字引用 (无害), 后者会有一小段"inode 不可达"的
/// 窗口 —— 万一后续插入失败, 文件就真的丢了 (块会被下一次 GC 回收)。
///
/// 目标已存在时的语义: 都是文件 → 覆盖; 目标是非空目录 / 类型不匹配 → 拒绝。
/// 移动目录时拒绝把它移进自己的子孙 (会形成环)。
fn mfs_rename(src: &str, dst: &str) -> u64 {
    let mut sc = [0u8; TMP_PATH_MAX];
    let mut dc = [0u8; TMP_PATH_MAX];
    let sn = match mfs_normalize(src, &mut sc) {
        Some(n) => n,
        None => return u64::MAX,
    };
    let dn = match mfs_normalize(dst, &mut dc) {
        Some(n) => n,
        None => return u64::MAX,
    };
    // 根既不能被移动也不能被覆盖。
    if sn == 1 || dn == 1 {
        return u64::MAX;
    }
    // 同一路径: 无事可做 (POSIX 里也算成功)。
    if sn == dn && sc[..sn] == dc[..dn] {
        return 1;
    }
    // 源必须存在。**不跟随末段软链接**: `mv link dst` 移动的是链接本身。
    let s_ino = match mfs_resolve_no_follow(&sc[..sn]) {
        Some(x) => x,
        None => return u64::MAX,
    };
    let sblock = match mfs_ino_block(s_ino) {
        Some(b) if b != 0 => b,
        _ => return u64::MAX,
    };
    // 类型取自节点魔数: 条目类型要与它一致, 否则改名会把软链接变成普通文件。
    let s_typ = match mfs_node_type(sblock) {
        Some(t) => t,
        None => return u64::MAX,
    };
    let s_is_dir = s_typ == MFS_TYPE_DIR;
    // 目录不能移进自己的子孙 (否则目录树成环, 解析会绕圈)。
    if s_is_dir && dn > sn && dc[..sn] == sc[..sn] && dc[sn] == b'/' {
        return u64::MAX;
    }
    // 目标父路径与末分量。
    let mut split = dn;
    while split > 1 && dc[split - 1] != b'/' {
        split -= 1;
    }
    let dparent_end = if split > 1 { split - 1 } else { 1 };
    let dcomp = &dc[split..dn];
    if dcomp.is_empty() {
        return u64::MAX;
    }
    // 目标若已存在: 校验类型与空目录约束, 并在插入前删掉旧条目。
    // 同样**不跟随**: 目标是一个悬空软链接时它「已存在」, 必须按已存在处理, 否则
    // 会往目录里插出两条同名的条目。
    if let Some(d_ino) = mfs_resolve_no_follow(&dc[..dn]) {
        let dblk = match mfs_ino_block(d_ino) {
            Some(b) if b != 0 => b,
            _ => return u64::MAX,
        };
        let d_typ = match mfs_node_type(dblk) {
            Some(t) => t,
            None => return u64::MAX,
        };
        if d_typ != s_typ {
            return u64::MAX; // 类型不匹配 (文件 / 目录 / 软链接之间)
        }
        if d_typ == MFS_TYPE_DIR && mfs_dir_is_empty(d_ino) != Some(true) {
            return u64::MAX; // 目标目录非空
        }
        // 刚解析完目标, `MFS_LEAF` 就是指向它的那条条目。
        let dloc = unsafe { MFS_LEAF };
        if !mfs_dir_delete(&dloc) {
            return u64::MAX;
        }
        if !mfs_free_ino(d_ino) {
            return u64::MAX;
        }
    }
    // 1) 先在新位置建条目 (同一个 ino, 此时可能短暂存在两个名字)。
    let dparent_ino = match mfs_resolve(&dc[..dparent_end]) {
        Some(x) => x,
        None => return u64::MAX,
    };
    if !mfs_dir_insert(dparent_ino, dcomp, s_ino, s_typ) {
        return u64::MAX;
    }
    // 2) 再删掉旧名字 (解析一次以取得条目位置, 上一步的改动不影响 ino)。
    if mfs_resolve_no_follow(&sc[..sn]).is_none() {
        return u64::MAX;
    }
    let sloc = unsafe { MFS_LEAF };
    if !mfs_dir_delete(&sloc) {
        return u64::MAX;
    }
    1
}

/// 修改 `path` 的权限位 (低 12 位), 成功返回 1。
///
/// 权限位只存储与显示, 不参与访问判定 —— 当前系统没有多用户概念 (见 roadmap M5)。
fn mfs_chmod(path: &str, mode: u16) -> u64 {
    let mut canon = [0u8; TMP_PATH_MAX];
    let n = match mfs_normalize(path, &mut canon) {
        Some(n) => n,
        None => return u64::MAX,
    };
    let ino = match mfs_resolve(&canon[..n]) {
        Some(x) => x,
        None => return u64::MAX,
    };
    let block = match mfs_ino_block(ino) {
        Some(b) if b != 0 => b,
        _ => return u64::MAX,
    };
    let a = mfs_a();
    if !mfs_read_blk(block, a) {
        return u64::MAX;
    }
    let (is_dir, magic) = if mfs_ok(a, MFS_MAGIC_DIR) {
        (true, MFS_MAGIC_DIR)
    } else if mfs_ok(a, MFS_MAGIC_FILE) {
        (false, MFS_MAGIC_FILE)
    } else {
        return u64::MAX;
    };
    mfs_set_mode(a, is_dir, mode);
    mfs_touch_ctime(a, is_dir);
    if mfs_commit_object(ino, a, magic).is_none() {
        return u64::MAX;
    }
    1
}

/// 把一个目录块里的所有有效条目写成 `DirEntry`, 累加到 `count` (上限 `RESULT_MAX_ENTRIES`)。
///
/// 读子节点 inode 用 S 缓冲, 以免覆盖 A(节点)/B(索引)/C(扩展块) 三块目录状态。
fn mfs_dir_emit(buf: *const u8, out: *mut vfs::DirEntry, count: &mut usize) {
    let end = MFS_HDR + MFS_PAYLOAD;
    let mut off = MFS_HDR + MFS_DIR_HDR;
    while off + MFS_DIR_ENT_HDR <= end {
        if *count >= vfs::RESULT_MAX_ENTRIES {
            return;
        }
        let nl = mfs_ent_name_len(buf, off);
        if nl != 0 {
            let typ = mfs_ent_type(buf, off);
            let is_dir = typ == MFS_TYPE_DIR;
            let mut de = vfs::DirEntry::short([0u8; 11], 0, u32::from(is_dir));
            let name =
                unsafe { core::slice::from_raw_parts(mfs_at(buf, off + MFS_DIR_ENT_HDR), nl) };
            mfs_name_to_fat(name, &mut de.name);
            // MFS 名字直接以字节串存储, 原样回传 (截到 IPC 可达长度)。
            let ln = nl.min(vfs::DIR_LONG_MAX);
            de.long[..ln].copy_from_slice(&name[..ln]);
            de.long_len = ln as u8;
            // 读子节点 inode 补齐元数据 (用 S 缓冲, 不碰 A/B/C 的目录状态);
            // `ls -l` 因此只需一次 readdir, 不必逐条目 stat。条目存的是 ino,
            // 故先经 inode 表翻译成块号 (表块走单条目缓存, 连续 ino 只读一次)。
            let s = mfs_s();
            let child = mfs_ino_block(mfs_ent_ino(buf, off)).unwrap_or_default();
            if child != 0 && mfs_read_blk(child, s) {
                // 软链接也带出 size (= 目标路径长度, 同 `lstat`); 类型靠 `mode` 高位区分。
                if !is_dir && (mfs_ok(s, MFS_MAGIC_FILE) || mfs_ok(s, MFS_MAGIC_LINK)) {
                    de.size = mfs_file_size(s);
                }
                de.mode = mfs_get_mode(s, is_dir);
                de.owner = mfs_get_owner(s, is_dir);
                de.nlink = mfs_get_nlink(s, is_dir);
                de.mtime = mfs_get_mtime(s, is_dir);
            }
            unsafe {
                core::ptr::write_unaligned(out.add(*count), de);
            }
            *count += 1;
        }
        off = match mfs_ent_step(buf, off) {
            Some(n) => n,
            None => return,
        };
    }
}

/// 列出目录 `ino` 的条目, 写入 `out` (DirEntry 数组), 返回写入字节数。
///
/// 目录条目可能散在目录节点块与若干扩展块里 (M4), 按「节点块 → 索引块槽位顺序」遍历。
fn mfs_readdir(ino: u32, out: *mut vfs::DirEntry) -> u64 {
    let a = mfs_a();
    let block = match mfs_ino_block(ino) {
        Some(b) if b != 0 => b,
        _ => return u64::MAX,
    };
    if !mfs_read_blk(block, a) || !mfs_ok(a, MFS_MAGIC_DIR) {
        return u64::MAX;
    }
    // 结果页只有一页: 放不下就停在已写入的条目上 (mfs_dir_emit 内部即按上限截断)。
    let mut count = 0usize;
    mfs_dir_emit(a, out, &mut count);
    let ext = mfs_dir_ext(a);
    if ext != 0 && count < vfs::RESULT_MAX_ENTRIES {
        let b = mfs_b();
        if mfs_read_blk(ext, b) && mfs_ok(b, MFS_MAGIC_DIDX) {
            let c = mfs_c();
            for i in 0..MFS_DIR_SLOTS {
                if count >= vfs::RESULT_MAX_ENTRIES {
                    break;
                }
                let blk = read_u32(mfs_at(b, MFS_HDR + i * 4));
                if blk == 0 {
                    continue;
                }
                if !mfs_read_blk(blk, c) || !mfs_ok(c, MFS_MAGIC_DIR) {
                    continue;
                }
                mfs_dir_emit(c, out, &mut count);
            }
        }
    }
    (count * core::mem::size_of::<vfs::DirEntry>()) as u64
}

// ---------------------------------------------------------------------------
// fd 表 (按路径)
// ---------------------------------------------------------------------------

fn mfs_fd_alloc(path: &[u8], is_dir: bool) -> u64 {
    for i in 0..MFS_MAX_FD {
        unsafe {
            let s = &mut *core::ptr::addr_of_mut!(MFS_FDS).cast::<MfsFd>().add(i);
            if !s.used {
                s.used = true;
                s.is_dir = is_dir;
                s.path_len = path.len() as u8;
                // 绑定分配时所在的卷: 之后这个 fd 上的请求可能落在别的卷被处理
                // (服务循环按请求切卷), 靠它把请求拉回本 fd 所属的那一卷。
                s.vol = MFS_CUR_VOL;
                s.path = [0; TMP_PATH_MAX];
                s.path[..path.len()].copy_from_slice(path);
                return i as u64;
            }
        }
    }
    u64::MAX
}
fn mfs_fd_get(fd: u32) -> Option<MfsFd> {
    if fd as usize >= MFS_MAX_FD {
        return None;
    }
    unsafe {
        let s = &*core::ptr::addr_of!(MFS_FDS)
            .cast::<MfsFd>()
            .add(fd as usize);
        if s.used {
            Some(*s)
        } else {
            None
        }
    }
}
fn mfs_fd_free(fd: u32) -> u64 {
    if fd as usize >= MFS_MAX_FD {
        return 0;
    }
    unsafe {
        let s = &mut *core::ptr::addr_of_mut!(MFS_FDS)
            .cast::<MfsFd>()
            .add(fd as usize);
        if s.used {
            s.used = false;
            1
        } else {
            0
        }
    }
}

// ---------------------------------------------------------------------------
// 服务循环
// ---------------------------------------------------------------------------

/// 域 11 — MFS 服务: 处理 VFS 协议 + MFS 快照 / 空间回收操作。
fn mfs_main() {
    // 先为块缓冲分配页 (固定虚拟地址, 避开程序镜像 / 用户栈 / 数据区),
    // 再把它们共享给 block_srv (它按 req.buf 写入读到的扇区), 之后才能做块 I/O。
    if sys_alloc_page(mfs_a() as u64) != 1
        || sys_alloc_page(mfs_b() as u64) != 1
        || sys_alloc_page(mfs_c() as u64) != 1
        || sys_alloc_page(mfs_s() as u64) != 1
        || sys_alloc_page(mfs_gc_buf() as u64) != 1
        || sys_alloc_page(mfs_itab_buf() as u64) != 1
        || sys_alloc_page(mfs_itabx_buf() as u64) != 1
        || sys_alloc_page(mfs_gc_tab_buf() as u64) != 1
        || sys_alloc_page(mfs_bmph_buf() as u64) != 1
    {
        println("mfs: alloc block buffers FAILED");
        return;
    }
    // 这些页都要「同地址」共享给 block_srv: 它按我们给的地址做 DMA 写入, 未共享的
    // 地址在目标域里无效 -> NVMe 命令会直接被判为非法字段。
    if sys_share_page(mfs_a() as u64, BLOCK_DOMAIN) != 1
        || sys_share_page(mfs_b() as u64, BLOCK_DOMAIN) != 1
        || sys_share_page(mfs_c() as u64, BLOCK_DOMAIN) != 1
        || sys_share_page(mfs_s() as u64, BLOCK_DOMAIN) != 1
        || sys_share_page(mfs_gc_buf() as u64, BLOCK_DOMAIN) != 1
        || sys_share_page(mfs_itab_buf() as u64, BLOCK_DOMAIN) != 1
        || sys_share_page(mfs_itabx_buf() as u64, BLOCK_DOMAIN) != 1
        || sys_share_page(mfs_gc_tab_buf() as u64, BLOCK_DOMAIN) != 1
        || sys_share_page(mfs_bmph_buf() as u64, BLOCK_DOMAIN) != 1
    {
        println("mfs: share block buffers FAILED");
        return;
    }
    // 认领卷: 优先「主卷序号最大」的 MFS 卷 (= 最近一次 `mkfs.mfs` 过的那块), 其次
    // 第一个 MFS 卷; 空白盘没有 magic, 回退到约定卷号 1 (见 `mfs_vol_claim`)。
    unsafe {
        MFS_VOL = mfs_vol_claim(mfs_a(), 16);
        // 卷容量 (扇区数): 首次格式化按它决定文件系统大小; 也用于校验盘上记录的总
        // 块数没超出卷的实际容量。0 = 未知 (IDE 回退等), 上层按默认值兜底。
        MFS_VOL_SECTORS = vol_sectors(mfs_a(), MFS_VOL);
        // 服务起点就是主卷: 之后只有请求明确要求别的卷时才切。
        MFS_CUR_VOL = MFS_VOL;
        MFS_CUR_SECTORS = MFS_VOL_SECTORS;
    }
    // 位图窗口必须在**挂载前**铺开 (挂载路径要直接拿窗口页作位图 I/O 缓冲)。此刻盘上
    // 的 bb 还读不到, 故按**卷容量**预算上界: 每 32768 块占一个 4 KiB 位图数据块,
    // 即窗口页数 = ceil(卷块数 / 32768)。只增不缩: 之后切到更大卷由 `mfs_win_ensure` 补页。
    {
        let secs = unsafe { MFS_CUR_SECTORS } as u64;
        let vol_blocks = if secs == 0 {
            MFS_DEFAULT_TOTAL_BLOCKS as u64
        } else {
            secs / MFS_SECTORS_PER_BLOCK as u64
        };
        let blocks = vol_blocks.clamp(MFS_MIN_TOTAL_BLOCKS as u64, MFS_MAX_BLOCKS as u64);
        if !mfs_win_ensure(mfs_bb_for(blocks as u32)) {
            println("mfs: alloc bitmap windows FAILED");
            return;
        }
    }
    // 安全护栏: 只允许挂载「已是 MFS」或「整盘无文件系统 (UNKNOWN, 需格式化)」的卷。
    // 卷号回退一旦算错 (例如接了真 U 盘、换了镜像布局), 自动格式化会把别人的分区
    // 直接写掉 —— 这里宁可让服务不挂载 (上层会看到 FAILED), 也绝不动非 MFS 卷。
    let kind = vol_kind_of(mfs_a(), unsafe { MFS_VOL });
    if kind != VOL_KIND_UNKNOWN && kind != VOL_KIND_MFS {
        print("mfs: refuse to format non-MFS volume vol=");
        print_u64(unsafe { MFS_VOL });
        print(" kind=");
        print_u64(kind as u64);
        println("");
        return;
    }
    if !mfs_mount_or_format() {
        println("mfs: mount/format FAILED");
        return;
    }
    print("mfs-dbg: vol=");
    print_u64(unsafe { MFS_VOL });
    print(" total=");
    print_u64(unsafe { MFS_TOTAL_BLOCKS } as u64);
    print(" free=");
    print_u64(unsafe { MFS_FREE_BLOCKS } as u64);
    print(" gen=");
    print_u64(unsafe { MFS_GEN });
    print(" snap=");
    print_u64(unsafe { MFS_SNAP_COUNT } as u64);
    // 卷容量 (扇区数): `total * 8` 应等于它 —— 不等说明文件系统没铺满卷 (或卷被换过)。
    print(" volsec=");
    print_u64(unsafe { MFS_VOL_SECTORS } as u64);
    println("");

    // M1b: 把**额外**的 MFS 卷 (真盘上可以有多块) 挂到 `/usb<卷号>`。用 A 页暂存卷
    // 描述符 —— 超级块已解析完毕, 该页此刻只是块缓冲, 内容不留用。
    // 只认卷层探测为 MFS 的卷: 空白的额外卷不会被自动格式化 (要显式 `mkfs.mfs`)。
    mount_extra_volumes(mfs_a(), VOL_KIND_MFS, unsafe { MFS_VOL }, vfs::MFS_DOMAIN);

    let mut msg = Message {
        from: 0,
        to: 0,
        tag: 0,
        payload: [0; PAYLOAD_LEN],
    };
    let mut canon = [0u8; TMP_PATH_MAX];
    loop {
        sys_recv_msg(&mut msg as *mut Message as *mut u8);
        // 请求间隙无在建 COW, 是唯一安全的回收时机: 空闲块偏少就先整理一次。
        mfs_maybe_gc();
        let tag = vfs::tag_body(msg.tag);
        // 卷编码 (tag 高位, M1b) 决定路径类请求落在哪个卷; fd 类请求的卷由 fd 自己
        // 绑定 (fd 是这些请求 payload 的首字段), 先探一次 fd, 使两类请求都对。
        // 关 fd 不动卷 (只改内存里的 fd 表), 故不参与。
        let mut vol = vfs::vol_from_enc(vfs::tag_vol(msg.tag), unsafe { MFS_VOL });
        if matches!(
            tag,
            vfs::VFS_READ_TAG | vfs::VFS_WRITE_TAG | vfs::VFS_READDIR_TAG | vfs::VFS_TRUNCATE_TAG
        ) {
            if let Some(fd) = mfs_fd_get(read_u32(msg.payload.as_ptr())) {
                vol = fd.vol;
            }
        }
        // 换卷: 内存态只有一份, 必须先把新卷的超级块载回来 (位图 / inode 表 / 快照)。
        // 载不回来就放弃本次请求 —— 继续用旧卷的位图去写新卷会毁数据。
        if vol != unsafe { MFS_CUR_VOL } && !mfs_switch_vol(vol) {
            sys_reply(u64::MAX);
            continue;
        }
        match tag {
            vfs::VFS_OPEN_TAG => {
                let len = msg
                    .payload
                    .iter()
                    .position(|&b| b == 0)
                    .unwrap_or(PAYLOAD_LEN);
                let path = unsafe { core::str::from_utf8_unchecked(&msg.payload[..len]) };
                let fd = match mfs_normalize(path, &mut canon) {
                    Some(n) => match mfs_resolve(&canon[..n]).and_then(mfs_ino_block) {
                        Some(blk) if blk != 0 => mfs_fd_alloc(&canon[..n], mfs_is_dir(blk)),
                        _ => u64::MAX,
                    },
                    None => u64::MAX,
                };
                sys_reply(fd);
            }
            vfs::VFS_READ_TAG => {
                let req: vfs::ReadReq = unsafe {
                    core::ptr::read_unaligned(msg.payload.as_ptr() as *const vfs::ReadReq)
                };
                let n = match mfs_fd_get(req.fd) {
                    Some(fd) if !fd.is_dir => {
                        let p = &fd.path[..fd.path_len as usize];
                        match mfs_resolve(p) {
                            Some(ino) => {
                                mfs_read_file(ino, req.offset, req.count, req.buf as *mut u8)
                            }
                            None => u64::MAX,
                        }
                    }
                    _ => u64::MAX,
                };
                sys_reply(n);
            }
            vfs::VFS_WRITE_TAG => {
                let req: vfs::WriteReq = unsafe {
                    core::ptr::read_unaligned(msg.payload.as_ptr() as *const vfs::WriteReq)
                };
                let n = match mfs_fd_get(req.fd) {
                    Some(fd) if !fd.is_dir => {
                        // 复制路径 (解析会复用 canon/缓冲, 避免借用冲突)。
                        let mut p = [0u8; TMP_PATH_MAX];
                        let plen = fd.path_len as usize;
                        p[..plen].copy_from_slice(&fd.path[..plen]);
                        match mfs_resolve(&p[..plen]) {
                            Some(ino) => {
                                mfs_write_file(ino, req.offset, req.count, req.buf as *const u8)
                            }
                            None => u64::MAX,
                        }
                    }
                    _ => u64::MAX,
                };
                sys_reply(n);
            }
            vfs::VFS_READDIR_TAG => {
                let req: vfs::DirReq = unsafe {
                    core::ptr::read_unaligned(msg.payload.as_ptr() as *const vfs::DirReq)
                };
                let n = match mfs_fd_get(req.fd) {
                    Some(fd) if fd.is_dir => {
                        let mut p = [0u8; TMP_PATH_MAX];
                        let plen = fd.path_len as usize;
                        p[..plen].copy_from_slice(&fd.path[..plen]);
                        match mfs_resolve(&p[..plen]) {
                            Some(ino) => mfs_readdir(ino, req.buf as *mut vfs::DirEntry),
                            None => u64::MAX,
                        }
                    }
                    _ => u64::MAX,
                };
                sys_reply(n);
            }
            vfs::VFS_CLOSE_TAG => {
                let fd = read_u32(msg.payload.as_ptr());
                sys_reply(mfs_fd_free(fd));
            }
            vfs::VFS_CREAT_TAG | vfs::VFS_MKDIR_TAG => {
                let is_dir = tag == vfs::VFS_MKDIR_TAG;
                let len = msg
                    .payload
                    .iter()
                    .position(|&b| b == 0)
                    .unwrap_or(PAYLOAD_LEN);
                let path = unsafe { core::str::from_utf8_unchecked(&msg.payload[..len]) };
                // 创建者域 id 作为 owner 记进元数据 (fire-and-forget 显示用)。
                let fd = mfs_create(path, is_dir, msg.from as u16);
                sys_reply(fd);
            }
            vfs::VFS_UNLINK_TAG | vfs::VFS_RMDIR_TAG => {
                let want_dir = tag == vfs::VFS_RMDIR_TAG;
                let len = msg
                    .payload
                    .iter()
                    .position(|&b| b == 0)
                    .unwrap_or(PAYLOAD_LEN);
                let path = unsafe { core::str::from_utf8_unchecked(&msg.payload[..len]) };
                sys_reply(mfs_remove(path, want_dir));
            }
            vfs::VFS_TRUNCATE_TAG => {
                let req: vfs::TruncateReq = unsafe {
                    core::ptr::read_unaligned(msg.payload.as_ptr() as *const vfs::TruncateReq)
                };
                let n = match mfs_fd_get(req.fd) {
                    Some(fd) if !fd.is_dir => {
                        let mut p = [0u8; TMP_PATH_MAX];
                        let plen = fd.path_len as usize;
                        p[..plen].copy_from_slice(&fd.path[..plen]);
                        match mfs_resolve(&p[..plen]) {
                            Some(ino) => mfs_truncate(ino, req.size),
                            None => u64::MAX,
                        }
                    }
                    _ => u64::MAX,
                };
                sys_reply(n);
            }
            vfs::VFS_RENAME_TAG => {
                sys_reply(with_two_paths(msg.payload.as_ptr(), mfs_rename));
            }
            vfs::VFS_LINK_TAG => {
                sys_reply(with_two_paths(msg.payload.as_ptr(), mfs_link));
            }
            // 软链接 (M5c): 两条路径 = (目标, 链接自身)。目标的**原样**存储, 故只有
            // 链接自身的路径经挂载层路由 (见 `vfs::symlink_into`)。
            vfs::VFS_SYMLINK_TAG => {
                let owner = msg.from as u16;
                sys_reply(with_two_paths(msg.payload.as_ptr(), |t, l| {
                    mfs_symlink(t, l, owner)
                }));
            }
            vfs::VFS_CHMOD_TAG => {
                let req: vfs::PathReq = unsafe {
                    core::ptr::read_unaligned(msg.payload.as_ptr() as *const vfs::PathReq)
                };
                let path = unsafe { core::str::from_utf8_unchecked(page_path(req.buf)) };
                sys_reply(mfs_chmod(path, req.aux as u16));
            }
            vfs::VFS_STAT_TAG => {
                let (buf, path) = parse_path_req(msg.payload.as_ptr());
                let n = match mfs_normalize(path, &mut canon) {
                    Some(nn) => match mfs_resolve(&canon[..nn]) {
                        Some(ino) => {
                            let blk = mfs_ino_block(ino).unwrap_or_default();
                            mfs_stat_into(blk, buf)
                        }
                        None => u64::MAX,
                    },
                    None => u64::MAX,
                };
                sys_reply(n);
            }
            // 软链接配套 (M5c): 读**链接自身**的目标串 (不跟随)。
            vfs::VFS_READLINK_TAG => {
                let (buf, path) = parse_path_req(msg.payload.as_ptr());
                let n = match mfs_normalize(path, &mut canon) {
                    Some(nn) => mfs_readlink(&canon[..nn], buf),
                    None => u64::MAX,
                };
                sys_reply(n);
            }
            // 取**链接自身**的元数据 (不跟随末段): 与 STAT 只差「末段是否跟随」。
            vfs::VFS_LSTAT_TAG => {
                let (buf, path) = parse_path_req(msg.payload.as_ptr());
                let n = match mfs_normalize(path, &mut canon) {
                    Some(nn) => match mfs_resolve_no_follow(&canon[..nn]) {
                        Some(ino) => mfs_stat_into(mfs_ino_block(ino).unwrap_or_default(), buf),
                        None => u64::MAX,
                    },
                    None => u64::MAX,
                };
                sys_reply(n);
            }
            vfs::MFS_SNAP_TAG => {
                // 快照表是**环形**: 满 (MFS_MAX_SNAP) 时先淘汰最旧一条, 为新快照腾位,
                // 而不是直接失败 —— 表跨启动持久化在超级块里, 否则第 9 次起就再也建不出
                // 快照。被淘汰快照指向的旧块仍由 COW 永久保留, 只是不再有快照记录引用;
                // 淘汰后所有快照索引整体前移一位, 旧的索引随即失效。
                unsafe {
                    if MFS_SNAP_COUNT >= MFS_MAX_SNAP {
                        for i in 1..MFS_MAX_SNAP {
                            mfs_set_snap(i - 1, mfs_snap(i));
                        }
                        MFS_SNAP_COUNT = MFS_MAX_SNAP - 1;
                    }
                }
                let idx = unsafe { MFS_SNAP_COUNT };
                mfs_set_snap(
                    idx,
                    MfsSnap {
                        gen: unsafe { MFS_GEN },
                        itab: unsafe { MFS_ITAB },
                        ino_hint: unsafe { MFS_INO_HINT },
                        alloc_next: unsafe { MFS_ALLOC_NEXT },
                    },
                );
                unsafe {
                    MFS_SNAP_COUNT = idx + 1;
                }
                let r = if mfs_bmp_flush() {
                    idx as u64
                } else {
                    u64::MAX
                };
                sys_reply(r);
            }
            vfs::MFS_SNAPLIST_TAG => {
                let buf = read_u64(msg.payload.as_ptr()) as *mut u8;
                let n = unsafe { MFS_SNAP_COUNT };
                for i in 0..n {
                    let s = mfs_snap(i);
                    let dst = unsafe { buf.add(i * vfs::SNAP_REC_LEN) };
                    write_u64(dst, s.gen);
                    write_u32(unsafe { dst.add(8) }, s.itab);
                    write_u32(unsafe { dst.add(12) }, s.ino_hint);
                    write_u32(unsafe { dst.add(16) }, s.alloc_next);
                }
                sys_reply((n * vfs::SNAP_REC_LEN) as u64);
            }
            vfs::MFS_SNAPRESTORE_TAG => {
                let idx = read_u32(msg.payload.as_ptr()) as usize;
                let r = if idx < unsafe { MFS_SNAP_COUNT } {
                    let s = mfs_snap(idx);
                    unsafe {
                        MFS_ITAB = s.itab;
                        MFS_INO_HINT = s.ino_hint;
                        MFS_ALLOC_NEXT = s.alloc_next;
                        MFS_GEN = s.gen;
                    }
                    // 索引镜像必须跟着换成快照那一版, 否则 ino 会翻译到回滚后的对象上。
                    if !mfs_itab_reload() {
                        u64::MAX
                    } else if mfs_bmp_flush() {
                        1
                    } else {
                        u64::MAX
                    }
                } else {
                    u64::MAX
                };
                sys_reply(r);
            }
            vfs::MFS_GC_TAG => {
                let freed = mfs_gc();
                if freed == u64::MAX {
                    println("mfs: gc FAILED");
                    sys_reply(u64::MAX);
                } else {
                    sys_reply(freed);
                }
            }
            vfs::MFS_STAT_TAG => {
                let total = unsafe { MFS_TOTAL_BLOCKS } as u64;
                let free = unsafe { MFS_FREE_BLOCKS } as u64;
                sys_reply((total << 32) | free);
            }
            // 显式格式化入口 (S2): 在指定卷上建 MFS。按卷号寻址, 不经挂载路由。
            vfs::VFS_MKFS_TAG => {
                let vol = read_u64(msg.payload.as_ptr());
                sys_reply(mfs_mkfs_volume(vol));
            }
            _ => {
                sys_reply(u64::MAX);
            }
        }
    }
}

/// 读取 `block` 是否为目录节点。
fn mfs_is_dir(block: u32) -> bool {
    let a = mfs_a();
    mfs_read_blk(block, a) && mfs_ok(a, MFS_MAGIC_DIR)
}

/// 读节点块, 按魔数返回它的类型 (`MFS_TYPE_*`); 魔数不认识时返回 None。
fn mfs_node_type(block: u32) -> Option<u32> {
    let a = mfs_a();
    if !mfs_read_blk(block, a) {
        return None;
    }
    if mfs_ok(a, MFS_MAGIC_DIR) {
        Some(MFS_TYPE_DIR)
    } else if mfs_ok(a, MFS_MAGIC_FILE) {
        Some(MFS_TYPE_FILE)
    } else if mfs_ok(a, MFS_MAGIC_LINK) {
        Some(MFS_TYPE_LINK)
    } else {
        None
    }
}

/// 读软链接 `path` **自身**的目标串到共享页 `buf`, 返回字节数 (不含 NUL)。
///
/// **不跟随**: 只对「末段是软链接」的路径有效, 普通文件 / 目录一律失败 (与 `readlink(2)`
/// 一致 —— 它不会跟着链接往下走)。目标串是**服务命名空间**里的路径, 形如 `/a`;
/// 把它还原成用户命名空间 (`/mfs/a`) 是客户端的事 (见 `vfs::readlink_into`)。
fn mfs_readlink(canon: &[u8], buf: u64) -> u64 {
    let ino = match mfs_resolve_no_follow(canon) {
        Some(x) => x,
        None => return u64::MAX,
    };
    // 类型用**条目**判断 (与 rm/mv 同口径): 刚解析完, `MFS_LEAF` 指向它的那条条目。
    let loc = unsafe { MFS_LEAF };
    if loc.dir_ino == 0 || !mfs_dir_load_loc(&loc) {
        return u64::MAX;
    }
    if mfs_ent_type(mfs_c(), loc.off) != MFS_TYPE_LINK {
        return u64::MAX;
    }
    let mut tmp = [0u8; MFS_LINK_MAX];
    let n = match mfs_link_target(ino, &mut tmp) {
        Some(n) => n,
        None => return u64::MAX,
    };
    // 单条 IPC 能回的路径上限; 创建时已受限, 这里兜底。
    let n = n.min(PAYLOAD_LEN - 1);
    unsafe {
        core::ptr::copy_nonoverlapping(tmp.as_ptr(), buf as *mut u8, n);
        *(buf as *mut u8).add(n) = 0;
    }
    n as u64
}

/// 把节点 `block` 的大小与元数据写成 `vfs::Stat` 到共享页 `buf`, 返回写入字节数。
fn mfs_stat_into(block: u32, buf: u64) -> u64 {
    let a = mfs_a();
    if !mfs_read_blk(block, a) {
        return u64::MAX;
    }
    // 类型信息不必单列一个字段: 它已在 `mode` 的高 4 位里 (见 `MFS_FTYPE_*`)。
    let (size, is_dir) = if mfs_ok(a, MFS_MAGIC_FILE) {
        (mfs_file_size(a), false)
    } else if mfs_ok(a, MFS_MAGIC_DIR) {
        (0, true)
    } else if mfs_ok(a, MFS_MAGIC_LINK) {
        // 软链接的 size 是目标路径字节数 (与 Unix `lstat` 一致)。
        (mfs_file_size(a), false)
    } else {
        return u64::MAX;
    };
    let st = vfs::Stat {
        size,
        is_dir: u32::from(is_dir),
        mode: mfs_get_mode(a, is_dir),
        owner: mfs_get_owner(a, is_dir),
        nlink: mfs_get_nlink(a, is_dir),
        mtime: mfs_get_mtime(a, is_dir),
        ctime: mfs_get_ctime(a, is_dir),
        atime: mfs_get_atime(a, is_dir),
    };
    unsafe {
        core::ptr::write_unaligned(buf as *mut vfs::Stat, st);
    }
    core::mem::size_of::<vfs::Stat>() as u64
}

/// 创建文件 (`is_dir=false`) 或目录 (`is_dir=true`)。成功返回 fd。
///
/// `owner` = 发起请求的域 id, 只记进元数据供 `ls -l` 显示 (不做权限检查)。
fn mfs_create(path: &str, is_dir: bool, owner: u16) -> u64 {
    let mut canon = [0u8; TMP_PATH_MAX];
    let n = match mfs_normalize(path, &mut canon) {
        Some(n) => n,
        None => return u64::MAX,
    };
    // 已存在: 文件直接打开; 目录按类型匹配 (creat 遇目录 / mkdir 遇任何已有项都失败)。
    if let Some(x) = mfs_resolve(&canon[..n]) {
        if is_dir {
            return u64::MAX;
        }
        let blk = match mfs_ino_block(x) {
            Some(b) if b != 0 => b,
            _ => return u64::MAX,
        };
        if mfs_is_dir(blk) {
            return u64::MAX;
        }
        return mfs_fd_alloc(&canon[..n], false);
    }
    // 父目录路径与末分量。
    let mut split = n;
    while split > 1 && canon[split - 1] != b'/' {
        split -= 1;
    }
    let parent_end = if split > 1 { split - 1 } else { 1 };
    let comp = &canon[split..n];
    if comp.is_empty() {
        return u64::MAX;
    }
    let parent_ino = match mfs_resolve(&canon[..parent_end]) {
        Some(x) => x,
        None => return u64::MAX,
    };
    // 名称查重 (也顺带校验父目录可解析)。
    if mfs_dir_lookup(parent_ino, comp).is_some() {
        return u64::MAX;
    }
    // 新建空节点 (用 C, 避免覆盖 A 中的父目录)。
    let c = mfs_c();
    zero_bytes(c, MFS_BLOCK);
    let mode = if is_dir { MFS_MODE_DIR } else { MFS_MODE_FILE };
    let ftype = if is_dir {
        MFS_FTYPE_DIR
    } else {
        MFS_FTYPE_FILE
    };
    if is_dir {
        mfs_dir_init_empty(c);
    } else {
        mfs_file_set_size(c, 0);
        mfs_file_set_nblocks(c, 0);
    }
    mfs_init_meta(c, is_dir, ftype, owner, mode);
    let magic = if is_dir {
        MFS_MAGIC_DIR
    } else {
        MFS_MAGIC_FILE
    };
    // 先落对象块, 再为它登记一个 ino (登记时把表槽直接指向该块)。顺序反过来会先占
    // 一个空槽却还不知道块号, 需要写两次表。
    let obj = match mfs_commit(c, magic) {
        Some(b) => b,
        None => return u64::MAX,
    };
    let child_ino = match mfs_ino_alloc_for(obj) {
        Some(i) => i,
        None => return u64::MAX,
    };
    let typ = if is_dir { MFS_TYPE_DIR } else { MFS_TYPE_FILE };
    if !mfs_dir_insert(parent_ino, comp, child_ino, typ) {
        return u64::MAX;
    }
    if is_dir {
        1
    } else {
        mfs_fd_alloc(&canon[..n], false)
    }
}

/// 在 `linkpath` 建一个指向 `target` 的软链接 (M5c)。成功返回 1。
///
/// `target` **原样存储**: 不规范化、不做长度以外的校验, 也**不要求它存在** (允许悬空
/// 链接 —— 目标可以是之后才创建的文件)。绝对/相对在**解析时**判断: 以 '/' 开头 =
/// 从服务这棵树的根开始, 否则相对链接**所在目录**。
///
/// 注意绝对目标是**服务自己命名空间**里的路径, 不含挂载前缀 —— 客户端侧的
/// `vfs::symlink_into` 已经把同挂载点内的前缀剥掉了 (`/mfs/a` -> `/a`); 服务端
/// 只看得见自己这棵子树, 无从知道挂载点叫什么。
///
/// 软链接节点沿用文件布局 (目标内联在 payload 里, 不占数据块), 故元数据用
/// `is_dir = false` 读写; 它不参与硬链接, `nlink` 恒为 1。
fn mfs_symlink(target: &str, linkpath: &str, owner: u16) -> u64 {
    let tb = target.as_bytes();
    if tb.is_empty() || tb.len() > MFS_LINK_MAX {
        return u64::MAX;
    }
    let mut canon = [0u8; MFS_PATH_MAX];
    let n = match mfs_normalize(linkpath, &mut canon) {
        Some(n) => n,
        None => return u64::MAX,
    };
    if n == 1 {
        return u64::MAX; // 不能把根变成软链接
    }
    // 已存在同名的任何条目都拒绝 (不覆盖) —— 含既有软链接 (含悬空的)。
    if mfs_resolve_no_follow(&canon[..n]).is_some() {
        return u64::MAX;
    }
    let mut split = n;
    while split > 1 && canon[split - 1] != b'/' {
        split -= 1;
    }
    let parent_end = if split > 1 { split - 1 } else { 1 };
    let comp = &canon[split..n];
    if comp.is_empty() {
        return u64::MAX;
    }
    let parent_ino = match mfs_resolve(&canon[..parent_end]) {
        Some(x) => x,
        None => return u64::MAX,
    };
    if mfs_dir_lookup(parent_ino, comp).is_some() {
        return u64::MAX;
    }
    // 目标内联进节点 (复用文件布局的 size 字段存目标长度)。
    let c = mfs_c();
    zero_bytes(c, MFS_BLOCK);
    mfs_file_set_size(c, tb.len() as u64);
    mfs_file_set_nblocks(c, 0);
    unsafe {
        core::ptr::copy_nonoverlapping(tb.as_ptr(), mfs_atm(c, MFS_LINK_TARGET_OFF), tb.len());
    }
    mfs_init_meta(c, false, MFS_FTYPE_LINK, owner, MFS_MODE_LINK);
    let obj = match mfs_commit(c, MFS_MAGIC_LINK) {
        Some(b) => b,
        None => return u64::MAX,
    };
    let child_ino = match mfs_ino_alloc_for(obj) {
        Some(i) => i,
        None => return u64::MAX,
    };
    if !mfs_dir_insert(parent_ino, comp, child_ino, MFS_TYPE_LINK) {
        return u64::MAX;
    }
    1
}

/// 为已存在的文件 `src` 在 `dst` 再加一个名字 (硬链接)。成功返回 1。
///
/// 目录不允许硬链接 (会形成环)。有了 inode 表, 两个名字共享同一个 ino, 之后从任一个
/// 名字改写文件, 另一个名字都会看到新内容 —— 这正是 inode 间接层的核心收益:
/// 改对象只动它自己的表槽, 与"有多少个名字引用它"无关。
fn mfs_link(src: &str, dst: &str) -> u64 {
    let mut sc = [0u8; TMP_PATH_MAX];
    let mut dc = [0u8; TMP_PATH_MAX];
    let sn = match mfs_normalize(src, &mut sc) {
        Some(n) => n,
        None => return u64::MAX,
    };
    let dn = match mfs_normalize(dst, &mut dc) {
        Some(n) => n,
        None => return u64::MAX,
    };
    if sn == 1 || dn == 1 {
        return u64::MAX;
    }
    let s_ino = match mfs_resolve(&sc[..sn]) {
        Some(x) => x,
        None => return u64::MAX,
    };
    let sblk = match mfs_ino_block(s_ino) {
        Some(b) if b != 0 => b,
        _ => return u64::MAX,
    };
    if mfs_is_dir(sblk) {
        return u64::MAX; // 目录不能硬链接
    }
    // 目标必须不存在 (不覆盖)。
    if mfs_resolve(&dc[..dn]).is_some() {
        return u64::MAX;
    }
    let mut split = dn;
    while split > 1 && dc[split - 1] != b'/' {
        split -= 1;
    }
    let dparent_end = if split > 1 { split - 1 } else { 1 };
    let dcomp = &dc[split..dn];
    if dcomp.is_empty() {
        return u64::MAX;
    }
    let dparent_ino = match mfs_resolve(&dc[..dparent_end]) {
        Some(x) => x,
        None => return u64::MAX,
    };
    // 先建新名字再抬链接数: 反过来会留一段「计数已加但名字还不存在」的窗口, 崩溃后
    // 计数偏高 (只影响显示, 不丢数据)。
    if !mfs_dir_insert(dparent_ino, dcomp, s_ino, MFS_TYPE_FILE) {
        return u64::MAX;
    }
    let sblk = match mfs_ino_block(s_ino) {
        Some(b) if b != 0 => b,
        _ => return u64::MAX,
    };
    let a = mfs_a();
    if !mfs_read_blk(sblk, a) || !mfs_ok(a, MFS_MAGIC_FILE) {
        return u64::MAX;
    }
    let nlink = mfs_get_nlink(a, false).max(1);
    mfs_set_nlink(a, false, nlink + 1);
    mfs_touch_ctime(a, false);
    if mfs_commit_object(s_ino, a, MFS_MAGIC_FILE).is_none() {
        return u64::MAX;
    }
    1
}

/// 删除文件 (`want_dir=false`) 或空目录 (`want_dir=true`)。成功返回 1。
///
/// 文件是「摘名字」而不是「删对象」: 链接数减到 0 才释放它的 ino, 否则只是少了一个
/// 名字 (块交给 GC 按可达性回收)。
fn mfs_remove(path: &str, want_dir: bool) -> u64 {
    let mut canon = [0u8; TMP_PATH_MAX];
    let n = match mfs_normalize(path, &mut canon) {
        Some(n) => n,
        None => return u64::MAX,
    };
    if n == 1 {
        return u64::MAX; // 不允许删除根
    }
    let ino = match mfs_resolve_no_follow(&canon[..n]) {
        Some(x) => x,
        None => return u64::MAX,
    };
    // 刚解析完, `MFS_LEAF` 就是指向它的那条条目 (可能在扩展块里)。
    // 用**不跟随**的解析: `rm`/`rmdir` 删的是条目本身 —— `rm link` 摘掉的是软链接,
    // 绝不能跟着目标去删目标文件。
    let loc = unsafe { MFS_LEAF };
    if loc.dir_ino == 0 {
        return u64::MAX;
    }
    if !mfs_dir_load_loc(&loc) {
        return u64::MAX;
    }
    let typ = mfs_ent_type(mfs_c(), loc.off);
    let is_dir = typ == MFS_TYPE_DIR;
    if want_dir != is_dir {
        return u64::MAX;
    }
    if is_dir {
        // 目录必须为空 (条目可能散在扩展块里), 且目录不参与硬链接 -> 直接释放 ino。
        match mfs_dir_is_empty(ino) {
            Some(true) => {}
            _ => return u64::MAX,
        }
        if !mfs_dir_delete(&loc) || !mfs_free_ino(ino) {
            return u64::MAX;
        }
        return 1;
    }
    // 非目录 (普通文件 / 软链接): 先摘掉这个条目。
    if !mfs_dir_delete(&loc) {
        return u64::MAX;
    }
    if typ == MFS_TYPE_LINK {
        // 软链接不参与硬链接 (nlink 恒 1), 且它不占数据块 -> 直接释放 ino。
        return if mfs_free_ino(ino) { 1 } else { u64::MAX };
    }
    // 普通文件: 递减链接数; 还有别的名字就只更新计数, 否则释放 ino。
    let blk = match mfs_ino_block(ino) {
        Some(b) if b != 0 => b,
        _ => return u64::MAX,
    };
    let a = mfs_a();
    if !mfs_read_blk(blk, a) || !mfs_ok(a, MFS_MAGIC_FILE) {
        return u64::MAX;
    }
    let nlink = mfs_get_nlink(a, false);
    if nlink > 1 {
        mfs_set_nlink(a, false, nlink - 1);
        mfs_touch_ctime(a, false);
        if mfs_commit_object(ino, a, MFS_MAGIC_FILE).is_none() {
            return u64::MAX;
        }
    } else if !mfs_free_ino(ino) {
        return u64::MAX;
    }
    1
}

// ===========================================================================
// 域 12 — ext2 只读文件服务 (ext2_srv)
// ===========================================================================
// 阶段 C3: 挂载既有 Linux ext2 分区的只读兼容层。镜像由宿主 `mke2fs` 预格式化,
// 服务**不写盘、也不自动格式化** —— 超级块无效即挂载失败 (与 MFS 的「首挂载自动
// 格式化」相反: ext2 的定位就是读别人已有的分区)。
//
// 只实现读取所需的最小 ext2 子集:
//   - 超级块 (@1024, magic 0xEF53) → 块大小 / 每组块数 / 每组 inode 数 / inode 大小;
//   - 块组描述符表 → 缓存每组 inode 表起始块, 由 inode 号定位 inode;
//   - inode `block[15]` 的直接 / 一级间接 / 二级间接块映射 (三级不实现);
//   - 目录项 (`inode/rec_len/name_len/file_type/name`) 顺序遍历。
//
// ext2 名字是大小写敏感的字节串; 为便于交互, 精确匹配失败后再做一次 ASCII
// 大小写不敏感回退 (精确命中优先)。

/// 卷号回退值: 2 对应 `build/ext2.img` (namespace 3)。
const EXT2_VOL_FALLBACK: u64 = 2;
/// ext2 服务实际使用的卷号, 启动时由 `vol_claim` 认领 (见 `ext2_main`)。
static mut EXT2_VOL: u64 = EXT2_VOL_FALLBACK;

/// ext2 超级块 magic (超级块内偏移 0x38)。
const EXT2_MAGIC: u16 = 0xEF53;
/// 根目录 inode 号 (ext2 规范固定为 2)。
const EXT2_ROOT_INO: u32 = 2;

/// `i_mode` 的文件类型位 (高 4 位)。
const EXT2_S_IFMT: u16 = 0xF000;
const EXT2_S_IFDIR: u16 = 0x4000;

/// 目录项 `file_type` 值。
const EXT2_FT_DIR: u8 = 2;

/// inode `block[]` 布局: 前 12 个直接块, 其后依次是一 / 二 / 三级间接块。
const EXT2_DIRECT_BLOCKS: u32 = 12;
const EXT2_IND_BLOCK: usize = 12;
const EXT2_DIND_BLOCK: usize = 13;

/// 块组数上限 —— 决定 inode 表起始块缓存的大小 (`EXT2_MAX_GROUPS` × 4 字节)。
///
/// 取 4096: 1 KiB 块 + 默认 8192 块/组 = 每组 8 MiB, 故可覆盖到约 32 GiB 的卷;
/// 真实 U 盘 / 大分区 (M1b) 的块组数远超早期测试镜像 (16 MiB 只需 2 组), 上限过小
/// 会让挂载直接失败。
const EXT2_MAX_GROUPS: usize = 4096;
/// 打开文件上限。
const EXT2_MAX_FD: usize = 16;

/// ext2 块缓冲虚拟地址 (紧跟 MFS 缓冲页, 均位于程序镜像之外)。
///
/// 这些页要以「同地址」共享给 block_srv 供其 DMA 写入, 故必须避开程序镜像:
/// 所有域加载同一份用户镜像, 若地址落在镜像内, block_srv 自身镜像会占住该地址,
/// 共享时触发 PageAlreadyMapped。已占用: fat32 `+0x10_0000..0x10_4000`、
/// app/shell `+0x10_4000..0x10_8000`、MFS `+0x10_8000..0x10_C000`。
const EXT2_BUF_A_VADDR: u64 = 0x0000_0080_0010_C000;
const EXT2_BUF_B_VADDR: u64 = 0x0000_0080_0010_D000;
const EXT2_BUF_C_VADDR: u64 = 0x0000_0080_0010_E000;
const EXT2_BUF_D_VADDR: u64 = 0x0000_0080_0010_F000;

fn ext2_a() -> *mut u8 {
    EXT2_BUF_A_VADDR as *mut u8
}
fn ext2_b() -> *mut u8 {
    EXT2_BUF_B_VADDR as *mut u8
}
fn ext2_c() -> *mut u8 {
    EXT2_BUF_C_VADDR as *mut u8
}
fn ext2_d() -> *mut u8 {
    EXT2_BUF_D_VADDR as *mut u8
}

// 挂载后固定的卷参数 (内存镜像)。
static mut EXT2_BLOCK_SIZE: u32 = 0;
static mut EXT2_INODES_PER_GROUP: u32 = 0;
static mut EXT2_INODE_SIZE: u32 = 0;
static mut EXT2_GROUP_COUNT: u32 = 0;
static mut EXT2_INODE_TABLE: [u32; EXT2_MAX_GROUPS] = [0; EXT2_MAX_GROUPS];

/// ext2 inode 的读取所需字段。
#[derive(Clone, Copy)]
struct Ext2Inode {
    mode: u16,
    size: u32,
    block: [u32; 15],
}

/// 打开文件描述符 (只读: 记住 inode 号即可, 不需要路径)。
#[derive(Clone, Copy)]
struct Ext2Fd {
    used: bool,
    is_dir: bool,
    ino: u32,
    /// 打开时绑定的卷号 (M1b 多卷挂载)。
    vol: u64,
}
const EXT2_FD_EMPTY: Ext2Fd = Ext2Fd {
    used: false,
    is_dir: false,
    ino: 0,
    vol: 0,
};
static mut EXT2_FDS: [Ext2Fd; EXT2_MAX_FD] = [EXT2_FD_EMPTY; EXT2_MAX_FD];

/// 本服务**当前请求**落在的卷号 (M1b 多卷挂载; 见 fat32_srv 的 `FAT_CUR_VOL` 注释)。
static mut EXT2_CUR_VOL: u64 = 0;

/// 已解析的几何 (超级块 / 块组描述符) 属于哪个卷。各卷的块大小 / inode 表位置不同,
/// 请求落到别的卷上必须重新解析 (见 `ext2_mount`)。
static mut EXT2_GEO_VOL: u64 = u64::MAX;

/// 读一个 ext2 块 (块号 → LBA = 块号 × 每块扇区数)。
fn ext2_read_block(block_no: u32, dst: *mut u8) -> bool {
    let sectors = (unsafe { EXT2_BLOCK_SIZE } / 512) as u16;
    if sectors == 0 {
        return false;
    }
    block_read_dev(
        unsafe { EXT2_CUR_VOL },
        block_no * sectors as u32,
        sectors,
        dst,
    )
}

/// 由 inode 号读 inode: inode 表块读入 `buf`, 需要的字段拷进返回值。
///
/// inode 尺寸 (128 / 256) 整除块大小, 故 inode 不会跨块。
fn ext2_read_inode_buf(ino: u32, buf: *mut u8) -> Option<Ext2Inode> {
    if ino == 0 {
        return None;
    }
    let per_group = unsafe { EXT2_INODES_PER_GROUP };
    let inode_size = unsafe { EXT2_INODE_SIZE };
    let block_size = unsafe { EXT2_BLOCK_SIZE };
    let idx = ino - 1;
    let group = idx / per_group;
    if group >= unsafe { EXT2_GROUP_COUNT } {
        return None;
    }
    let in_table = unsafe { EXT2_INODE_TABLE[group as usize] };
    let byte_off = (idx % per_group) as u64 * inode_size as u64;
    let block_no = in_table as u64 + byte_off / block_size as u64;
    let within = (byte_off % block_size as u64) as usize;
    if within + inode_size as usize > block_size as usize || block_no > u32::MAX as u64 {
        return None;
    }
    if !ext2_read_block(block_no as u32, buf) {
        return None;
    }
    let p = unsafe { buf.add(within) };
    let mut inode = Ext2Inode {
        mode: read_u16(p),
        size: read_u32(unsafe { p.add(4) }),
        block: [0; 15],
    };
    let mut i = 0usize;
    while i < 15 {
        inode.block[i] = read_u32(unsafe { p.add(0x28 + i * 4) });
        i += 1;
    }
    Some(inode)
}
fn ext2_read_inode(ino: u32) -> Option<Ext2Inode> {
    ext2_read_inode_buf(ino, ext2_a())
}
/// 用 B 缓冲读 inode: 供 readdir 在 A 缓冲持有目录数据时使用 (避免互相覆盖)。
fn ext2_read_inode_b(ino: u32) -> Option<Ext2Inode> {
    ext2_read_inode_buf(ino, ext2_b())
}

/// 把 inode 的逻辑块号映射为物理块号 (空洞 / 越界返回 None)。
fn ext2_map_block(inode: &Ext2Inode, logical: u32) -> Option<u32> {
    let ptrs = unsafe { EXT2_BLOCK_SIZE } / 4;
    if ptrs == 0 {
        return None;
    }
    if logical < EXT2_DIRECT_BLOCKS {
        let b = inode.block[logical as usize];
        return if b == 0 { None } else { Some(b) };
    }
    let mut idx = logical - EXT2_DIRECT_BLOCKS;
    if idx < ptrs {
        let ind = inode.block[EXT2_IND_BLOCK];
        if ind == 0 {
            return None;
        }
        let buf = ext2_c();
        if !ext2_read_block(ind, buf) {
            return None;
        }
        let b = read_u32(unsafe { buf.add(idx as usize * 4) });
        return if b == 0 { None } else { Some(b) };
    }
    idx -= ptrs;
    if idx < ptrs * ptrs {
        let dind = inode.block[EXT2_DIND_BLOCK];
        if dind == 0 {
            return None;
        }
        let buf = ext2_c();
        if !ext2_read_block(dind, buf) {
            return None;
        }
        let first = read_u32(unsafe { buf.add((idx / ptrs) as usize * 4) });
        if first == 0 {
            return None;
        }
        let buf2 = ext2_d();
        if !ext2_read_block(first, buf2) {
            return None;
        }
        let b = read_u32(unsafe { buf2.add((idx % ptrs) as usize * 4) });
        return if b == 0 { None } else { Some(b) };
    }
    None // 三级间接不实现 (只读演示足够)
}

/// 名字比较; `ci = true` 时按 ASCII 大小写不敏感比较。
fn ext2_name_eq(a: &[u8], b: &[u8], ci: bool) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0usize;
    while i < a.len() {
        if a[i] == b[i] || (ci && ascii_upper(a[i]) == ascii_upper(b[i])) {
            i += 1;
            continue;
        }
        return false;
    }
    true
}

/// 在目录 inode 的数据块中顺序查找名字, 返回 (inode, file_type)。
fn ext2_dir_find(dir: &Ext2Inode, name: &[u8], ci: bool) -> Option<(u32, u8)> {
    let block_size = unsafe { EXT2_BLOCK_SIZE };
    let buf = ext2_a();
    let mut off = 0u32;
    while off < dir.size {
        let block = match ext2_map_block(dir, off / block_size) {
            Some(b) => b,
            None => {
                off += block_size;
                continue;
            }
        };
        if !ext2_read_block(block, buf) {
            return None;
        }
        let mut pos = 0usize;
        while pos + 8 <= block_size as usize {
            let e = unsafe { buf.add(pos) };
            let ino = read_u32(e);
            let rec_len = read_u16(unsafe { e.add(4) }) as usize;
            if rec_len < 8 || pos + rec_len > block_size as usize {
                break;
            }
            let name_len = unsafe { *e.add(6) } as usize;
            if ino != 0 && name_len == name.len() {
                let ename = unsafe { core::slice::from_raw_parts(e.add(8), name_len) };
                if ext2_name_eq(ename, name, ci) {
                    return Some((ino, unsafe { *e.add(7) }));
                }
            }
            pos += rec_len;
        }
        off += block_size;
    }
    None
}

/// 把绝对路径解析为 (inode 号, 是否目录)。路径必须以 '/' 开头。
fn ext2_resolve(path: &str) -> Option<(u32, bool)> {
    let bytes = path.as_bytes();
    if bytes.is_empty() || bytes[0] != b'/' {
        return None;
    }
    let mut ino = EXT2_ROOT_INO;
    let mut i = 1usize;
    while i < bytes.len() {
        let start = i;
        while i < bytes.len() && bytes[i] != b'/' {
            i += 1;
        }
        let comp = &bytes[start..i];
        if i < bytes.len() {
            i += 1; // 跳过分隔符
        }
        if comp.is_empty() || comp == b"." {
            continue;
        }
        let dir = ext2_read_inode(ino)?;
        if dir.mode & EXT2_S_IFMT != EXT2_S_IFDIR {
            return None;
        }
        let hit = match ext2_dir_find(&dir, comp, false) {
            Some(x) => Some(x),
            None => ext2_dir_find(&dir, comp, true),
        };
        ino = hit?.0;
    }
    let inode = ext2_read_inode(ino)?;
    Some((ino, inode.mode & EXT2_S_IFMT == EXT2_S_IFDIR))
}

/// 把 ext2 名字 (字节串) 拷进长名字段, 截断到容量且不切断 UTF-8 字符。
fn ext2_copy_long(name: &[u8], out: &mut [u8; vfs::DIR_LONG_MAX]) -> u8 {
    let mut n = name.len().min(vfs::DIR_LONG_MAX);
    // 截断点若落在字符中间 (续字节 10xxxxxx), 回退到该字符首字节之前。
    while n > 0 && n < name.len() && (name[n] & 0xC0) == 0x80 {
        n -= 1;
    }
    out[..n].copy_from_slice(&name[..n]);
    n as u8
}

/// 由 ext2 名字派生一个 8.3 形式的短名 (大写), 供无长名时的回退显示。
fn ext2_short_name(name: &[u8]) -> [u8; 11] {
    let mut out = [b' '; 11];
    if name == b"." || name == b".." {
        let mut i = 0usize;
        while i < name.len() && i < 11 {
            out[i] = name[i];
            i += 1;
        }
        return out;
    }
    // 主名/扩展名以最后一个 '.' 切分 (无 '.' 则整段都是主名)。
    let mut dot = name.len();
    let mut i = name.len();
    while i > 0 {
        i -= 1;
        if name[i] == b'.' {
            dot = i;
            break;
        }
    }
    let mut k = 0usize;
    let mut n = 0usize;
    while k < dot && n < 8 {
        out[n] = ascii_upper(name[k]);
        n += 1;
        k += 1;
    }
    let mut k = dot + 1;
    let mut m = 8usize;
    while k < name.len() && m < 11 {
        out[m] = ascii_upper(name[k]);
        m += 1;
        k += 1;
    }
    out
}

/// 构造一条目录条目记录 (需要文件大小时额外读一次目标 inode, 用 B 缓冲)。
fn ext2_make_entry(name: &[u8], ftype: u8, ino: u32) -> vfs::DirEntry {
    let short = ext2_short_name(name);
    let mut long = [0u8; vfs::DIR_LONG_MAX];
    let llen = ext2_copy_long(name, &mut long);
    let mut is_dir = ftype == EXT2_FT_DIR;
    let mut size = 0u64;
    if let Some(inode) = ext2_read_inode_b(ino) {
        // file_type 在旧版 ext2 可能为 0, 这时以 inode 的 mode 为准。
        is_dir = inode.mode & EXT2_S_IFMT == EXT2_S_IFDIR;
        if !is_dir {
            size = inode.size as u64;
        }
    }
    vfs::DirEntry::with_long(short, long, llen, size, if is_dir { 1 } else { 0 })
}

/// 读文件区间 [offset, offset+count) 到 `dst`, 返回实际读取字节数。
fn ext2_read_data(ino: u32, offset: u32, count: u32, dst: *mut u8) -> Option<u64> {
    let inode = ext2_read_inode(ino)?;
    if inode.mode & EXT2_S_IFMT == EXT2_S_IFDIR {
        return None;
    }
    if offset >= inode.size {
        return Some(0);
    }
    let n = count.min(inode.size - offset);
    let block_size = unsafe { EXT2_BLOCK_SIZE };
    let buf = ext2_b();
    let mut done = 0u32;
    while done < n {
        let pos = offset + done;
        let boff = (pos % block_size) as usize;
        let chunk = (block_size as usize - boff).min((n - done) as usize);
        match ext2_map_block(&inode, pos / block_size) {
            Some(block) => {
                if !ext2_read_block(block, buf) {
                    return None;
                }
                unsafe {
                    core::ptr::copy_nonoverlapping(buf.add(boff), dst.add(done as usize), chunk);
                }
            }
            // 稀疏文件: 空洞按零填充。
            None => zero_bytes(unsafe { dst.add(done as usize) }, chunk),
        }
        done += chunk as u32;
    }
    Some(n as u64)
}

/// 列出目录条目 (跳过 "." / ".."), 返回写入字节数。
fn ext2_readdir(ino: u32, dst: *mut vfs::DirEntry) -> Option<u64> {
    let dir = ext2_read_inode(ino)?;
    if dir.mode & EXT2_S_IFMT != EXT2_S_IFDIR {
        return None;
    }
    let block_size = unsafe { EXT2_BLOCK_SIZE };
    let buf = ext2_a();
    let entry_size = core::mem::size_of::<vfs::DirEntry>();
    let mut count = 0usize;
    let mut off = 0u32;
    while off < dir.size {
        let block = match ext2_map_block(&dir, off / block_size) {
            Some(b) => b,
            None => {
                off += block_size;
                continue;
            }
        };
        if !ext2_read_block(block, buf) {
            return None;
        }
        let mut pos = 0usize;
        while pos + 8 <= block_size as usize {
            let e = unsafe { buf.add(pos) };
            let eino = read_u32(e);
            let rec_len = read_u16(unsafe { e.add(4) }) as usize;
            if rec_len < 8 || pos + rec_len > block_size as usize {
                break;
            }
            let name_len = unsafe { *e.add(6) } as usize;
            if eino != 0 && name_len > 0 {
                let name = unsafe { core::slice::from_raw_parts(e.add(8), name_len) };
                // "." / ".." 不列出 (与 FAT32 / MFS 的 readdir 输出保持一致)。
                if name != b"." && name != b".." {
                    if count >= vfs::RESULT_MAX_ENTRIES {
                        return Some((count * entry_size) as u64);
                    }
                    let de = ext2_make_entry(name, unsafe { *e.add(7) }, eino);
                    unsafe {
                        core::ptr::write_unaligned(dst.add(count), de);
                    }
                    count += 1;
                }
            }
            pos += rec_len;
        }
        off += block_size;
    }
    Some((count * entry_size) as u64)
}

// ---------------------------------------------------------------------------
// 目录项构造与增删
// ---------------------------------------------------------------------------

/// Unix 秒 → exFAT 打包时间戳 (UTC); 返回 (时间戳, 10ms 增量)。
fn exfat_encode_time(unix: u64) -> (u32, u8) {
    if unix == 0 {
        return (0, 0);
    }
    let secs = unix as i64;
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    if !(1980..=2107).contains(&y) {
        return (0, 0);
    }
    let rem = secs.rem_euclid(86_400);
    let hour = rem / 3600;
    let min = (rem % 3600) / 60;
    let sec = rem % 60;
    let ts = (((y - 1980) as u32) << 25)
        | ((m as u32) << 21)
        | ((d as u32) << 16)
        | ((hour as u32) << 11)
        | ((min as u32) << 5)
        | ((sec / 2) as u32);
    (ts, ((sec % 2) * 100) as u8)
}

/// upcase 表 / 位图的「按需窗口」缓存状态。
///
/// 这两个表在大容量卷上可以很大 (位图可达 MB 级), 因此不整体载入, 而是
/// 每次把用到的那个 512B 扇区读进一页窗口; `SEC` 记录窗口当前装的是哪个扇区
/// (命中时无需再走 FAT 链换算 LBA)。
static mut EXFAT_UPC_WIN_SEC: usize = usize::MAX;
static mut EXFAT_UPC_WIN_LBA: u32 = 0;
static mut EXFAT_BMP_WIN_SEC: usize = usize::MAX;
static mut EXFAT_BMP_WIN_LBA: u32 = 0;
static mut EXFAT_BMP_DIRTY: bool = false;

/// 表 (`first` 起始的 FAT 链) 第 `sec` 个 512B 扇区的 LBA。
fn exfat_chain_sec_lba(first: u32, sec: usize) -> Option<u32> {
    let spc = unsafe { EXFAT_SECTORS_PER_CLUSTER } as usize;
    if spc == 0 {
        return None;
    }
    let cl = exfat_chain_nth(first, (sec / spc) as u32)?;
    Some(exfat_cluster_lba(cl) + (sec % spc) as u32)
}

/// upcase 表查询: 表只覆盖前 N 个码元, 超出者映射为自身。
fn exfat_upcase_unit(u: u16) -> u16 {
    let off = u as usize * 2;
    if (off + 2) as u32 > unsafe { EXFAT_UPCASE_BYTES } {
        return u;
    }
    let sec = off / EXFAT_SECTOR_SIZE as usize;
    if unsafe { EXFAT_UPC_WIN_SEC } != sec {
        let lba = match exfat_chain_sec_lba(unsafe { EXFAT_UPCASE_CLUSTER }, sec) {
            Some(l) => l,
            None => return u,
        };
        if !exfat_read_sectors(lba, 1, exfat_upc()) {
            return u;
        }
        unsafe {
            EXFAT_UPC_WIN_SEC = sec;
            EXFAT_UPC_WIN_LBA = lba;
        }
    }
    read_u16(exfat_at(exfat_upc(), off % EXFAT_SECTOR_SIZE as usize))
}

/// exFAT NameHash: 每个码元先对散列循环右移 1 位再累加其 upcase 值, 末尾再右移一次。
fn exfat_name_hash(units: &[u16], len: usize) -> u16 {
    let mut h: u16 = 0;
    let mut i = 0usize;
    while i < len {
        h = h.rotate_right(1).wrapping_add(exfat_upcase_unit(units[i]));
        i += 1;
    }
    h.rotate_right(1)
}

/// UTF-8 名字 → UTF-16 码元 (仅 BMP; 补充平面 (4 字节序列) 不支持)。
fn exfat_encode_name(name: &[u8], out: &mut [u16; 255]) -> Option<usize> {
    let mut n = 0usize;
    let mut i = 0usize;
    while i < name.len() {
        let b = name[i];
        let (cp, adv) = if b < 0x80 {
            (b as u32, 1)
        } else if b & 0xE0 == 0xC0 {
            if i + 1 >= name.len() {
                return None;
            }
            ((((b & 0x1F) as u32) << 6) | (name[i + 1] & 0x3F) as u32, 2)
        } else if b & 0xF0 == 0xE0 {
            if i + 2 >= name.len() {
                return None;
            }
            (
                (((b & 0x0F) as u32) << 12)
                    | (((name[i + 1] & 0x3F) as u32) << 6)
                    | (name[i + 2] & 0x3F) as u32,
                3,
            )
        } else {
            return None;
        };
        if cp > 0xFFFF || n >= 255 {
            return None;
        }
        out[n] = cp as u16;
        n += 1;
        i += adv;
    }
    if n == 0 {
        None
    } else {
        Some(n)
    }
}

/// 在 `out` 构造一组文件 entry set (`0x85` + `0xC0` + N×`0xC1`), 返回总条目数。
#[allow(clippy::too_many_arguments)]
fn exfat_build_set(
    out: *mut u8,
    name: &[u8],
    is_dir: bool,
    no_fat_chain: bool,
    first_cluster: u32,
    data_len: u64,
    valid_len: u64,
    mtime: u64,
) -> Option<usize> {
    let mut units = [0u16; 255];
    let nu = exfat_encode_name(name, &mut units)?;
    let name_entries = nu.div_ceil(EXFAT_NAME_UNITS_PER_ENTRY);
    let total = 2 + name_entries;
    let (ts, ten) = exfat_encode_time(mtime);
    zero_bytes(out, total * EXFAT_DIR_ENTRY);
    // 0x85 File
    unsafe {
        *out = EXFAT_TYPE_FILE;
        *out.add(1) = (1 + name_entries) as u8; // SecondaryCount = 0xC0 + N×0xC1
    }
    write_u16(
        exfat_atm(out, 4),
        if is_dir {
            EXFAT_ATTR_DIR
        } else {
            EXFAT_ATTR_ARCHIVE
        },
    );
    write_u32(exfat_atm(out, 8), ts); // CreateTimestamp
    write_u32(exfat_atm(out, 12), ts); // LastModifiedTimestamp
    write_u32(exfat_atm(out, 16), ts); // LastAccessedTimestamp
    unsafe {
        *out.add(20) = ten;
        *out.add(21) = ten;
    }
    // 0xC0 Stream Extension
    let stream = exfat_atm(out, EXFAT_DIR_ENTRY);
    unsafe {
        *stream = EXFAT_TYPE_STREAM;
        *stream.add(1) = EXFAT_SF_ALLOC | if no_fat_chain { EXFAT_SF_NOFATCHAIN } else { 0 };
        *stream.add(3) = nu as u8; // NameLength (UTF-16 码元数)
    }
    write_u16(exfat_atm(stream, 4), exfat_name_hash(&units, nu));
    write_u64(exfat_atm(stream, 8), valid_len);
    write_u32(exfat_atm(stream, 20), first_cluster);
    write_u64(exfat_atm(stream, 24), data_len);
    // N×0xC1 File Name
    let mut k = 0usize;
    while k < nu {
        let ent = exfat_atm(out, (2 + k / EXFAT_NAME_UNITS_PER_ENTRY) * EXFAT_DIR_ENTRY);
        unsafe {
            *ent = EXFAT_TYPE_NAME;
        }
        write_u16(
            exfat_atm(ent, 2 + (k % EXFAT_NAME_UNITS_PER_ENTRY) * 2),
            units[k],
        );
        k += 1;
    }
    let sum = exfat_set_checksum(out, (total * EXFAT_DIR_ENTRY) as u32);
    write_u16(exfat_atm(out, 2), sum);
    Some(total)
}

/// 目录条目组的磁盘位置: 第 `cluster` 簇内下标 `index`, 共 `1 + sec_count` 个条目。
#[derive(Clone, Copy)]
struct ExfatLoc {
    cluster: u32,
    index: usize,
    /// `0x85` 的 SecondaryCount (从属条目数)。
    sec_count: usize,
}

/// 在目录链中定位 `want` 的条目组 (需要簇与簇内下标, 便于原地改写)。
fn exfat_dir_locate(dir_first: u32, want: &[u8]) -> Option<ExfatLoc> {
    let cb = unsafe { EXFAT_CLUSTER_BYTES } as usize;
    if cb < EXFAT_DIR_ENTRY {
        return None;
    }
    let per = cb / EXFAT_DIR_ENTRY;
    let buf = exfat_clu();
    let mut cl = dir_first;
    let mut guard = 0u32;
    while cl >= 2 && guard <= unsafe { EXFAT_CLUSTER_COUNT } + 1 {
        if !exfat_read_cluster(cl, buf) {
            return None;
        }
        let mut i = 0usize;
        while i < per {
            let t = unsafe { *exfat_at(buf, i * EXFAT_DIR_ENTRY) };
            if t == EXFAT_TYPE_UNUSED {
                // `0x00` 只表示「本簇剩余条目未使用」; 目录链的后续簇仍可能有条目,
                // 故只跳过本簇余下部分, 不能整体停止 (否则会漏掉后面的簇)。
                break;
            }
            if t == EXFAT_TYPE_FILE {
                let sec = unsafe { *exfat_at(buf, i * EXFAT_DIR_ENTRY + 1) } as usize;
                if let Some(e) = exfat_parse_file_set(buf, i, per, sec) {
                    if exfat_name_eq(&e.name[..e.name_len as usize], want, true) {
                        return Some(ExfatLoc {
                            cluster: cl,
                            index: i,
                            sec_count: sec,
                        });
                    }
                }
                i += 1 + sec;
                continue;
            }
            i += 1;
        }
        let next = exfat_fat_get(cl)?;
        if next == EXFAT_FAT_FREE || exfat_is_eoc(next) {
            return None;
        }
        cl = next;
        guard += 1;
    }
    None
}

/// 在目录链中找一段可容纳 `need` 个连续条目的空位 (结尾 `0x00` 或已删除条目)。
///
/// 已删除条目 (in-use 位为 0) 可复用; 成组条目整体跳过, 避免把组中间当空位。
fn exfat_dir_find_slot(dir_first: u32, need: usize) -> Option<ExfatLoc> {
    let cb = unsafe { EXFAT_CLUSTER_BYTES } as usize;
    if cb < EXFAT_DIR_ENTRY || need == 0 {
        return None;
    }
    let per = cb / EXFAT_DIR_ENTRY;
    let buf = exfat_clu();
    let mut cl = dir_first;
    let mut guard = 0u32;
    while cl >= 2 && guard <= unsafe { EXFAT_CLUSTER_COUNT } + 1 {
        if !exfat_read_cluster(cl, buf) {
            return None;
        }
        let mut i = 0usize;
        while i < per {
            let t = unsafe { *exfat_at(buf, i * EXFAT_DIR_ENTRY) };
            if t != EXFAT_TYPE_UNUSED && t & 0x80 != 0 {
                if t == EXFAT_TYPE_FILE {
                    let sec = unsafe { *exfat_at(buf, i * EXFAT_DIR_ENTRY + 1) } as usize;
                    i += 1 + sec;
                } else {
                    i += 1;
                }
                continue;
            }
            let mut k = 0usize;
            while k < need && i + k < per {
                let tt = unsafe { *exfat_at(buf, (i + k) * EXFAT_DIR_ENTRY) };
                if tt != EXFAT_TYPE_UNUSED && tt & 0x80 != 0 {
                    break;
                }
                k += 1;
            }
            if k == need {
                return Some(ExfatLoc {
                    cluster: cl,
                    index: i,
                    sec_count: need - 1,
                });
            }
            i += 1;
        }
        let next = exfat_fat_get(cl)?;
        if next == EXFAT_FAT_FREE || exfat_is_eoc(next) {
            return None;
        }
        cl = next;
        guard += 1;
    }
    None
}

/// 目录链尾追加一个清零的簇 (目录放不下时扩容)。
///
/// 扩容前把链尾簇中尾部的未使用项 (`0x00`) 填成非 0 的「已删除」标记 ——
/// exFAT 规定 `0x00` 之后不得再出现非 0 项, 否则链上后续簇里的条目会被判为
/// 损坏 (宿主 `fsck.exfat` 直接报 `other entry follows unused entry`)。
fn exfat_dir_grow(dir_first: u32) -> Option<u32> {
    let cb = unsafe { EXFAT_CLUSTER_BYTES } as usize;
    if cb == 0 {
        return None;
    }
    let per = cb / EXFAT_DIR_ENTRY;
    // 1) 走到链尾簇。
    let mut cur = dir_first;
    let mut guard = 0u32;
    loop {
        let next = exfat_fat_get(cur)?;
        if next == EXFAT_FAT_FREE || exfat_is_eoc(next) {
            break;
        }
        cur = next;
        guard += 1;
        if guard > unsafe { EXFAT_CLUSTER_COUNT } + 1 {
            return None;
        }
    }
    // 2) 填掉尾部空位 (幂等: 已填过的簇不会再出现 0x00 尾部)。
    let buf = exfat_clu();
    if !exfat_read_cluster(cur, buf) {
        return None;
    }
    let mut i = 0usize;
    let mut need_fill = false;
    while i < per {
        if unsafe { *exfat_at(buf, i * EXFAT_DIR_ENTRY) } == EXFAT_TYPE_UNUSED {
            need_fill = true;
            break;
        }
        i += 1;
    }
    if need_fill {
        let mut k = i;
        while k < per {
            unsafe {
                *exfat_atm(buf, k * EXFAT_DIR_ENTRY) = EXFAT_FILLER;
            }
            k += 1;
        }
        if !exfat_write_cluster(cur, buf) {
            return None;
        }
    }
    // 3) 追加并链接一个清零的新簇。
    let cl = exfat_alloc_cluster()?;
    let z = exfat_clu();
    zero_bytes(z, cb);
    if !exfat_write_cluster(cl, z) {
        return None;
    }
    if !exfat_fat_set(cur, cl) {
        return None;
    }
    Some(cl)
}

/// 把一组条目写入 `loc` 指定的位置 (读-改-写所在簇)。
fn exfat_dir_put_set(loc: ExfatLoc, set: *const u8, total: usize) -> bool {
    let cb = unsafe { EXFAT_CLUSTER_BYTES } as usize;
    if cb == 0 || (loc.index + total) * EXFAT_DIR_ENTRY > cb {
        return false;
    }
    let buf = exfat_clu();
    if !exfat_read_cluster(loc.cluster, buf) {
        return false;
    }
    unsafe {
        core::ptr::copy_nonoverlapping(
            set,
            exfat_atm(buf, loc.index * EXFAT_DIR_ENTRY),
            total * EXFAT_DIR_ENTRY,
        );
    }
    exfat_write_cluster(loc.cluster, buf)
}

/// 把一组条目整体标记为已删除 (清 in-use 位); 不回收簇, 由调用方决定。
fn exfat_dir_del_set(loc: ExfatLoc) -> bool {
    let cb = unsafe { EXFAT_CLUSTER_BYTES } as usize;
    let total = 1 + loc.sec_count;
    if cb == 0 || (loc.index + total) * EXFAT_DIR_ENTRY > cb {
        return false;
    }
    let buf = exfat_clu();
    if !exfat_read_cluster(loc.cluster, buf) {
        return false;
    }
    for k in 0..total {
        unsafe {
            *exfat_atm(buf, (loc.index + k) * EXFAT_DIR_ENTRY) &= 0x7F;
        }
    }
    exfat_write_cluster(loc.cluster, buf)
}

/// 目录是否为空 (忽略系统项与卷标: 它们不是 `0x85` 条目)。
fn exfat_dir_is_empty(dir_first: u32) -> bool {
    let mut empty = true;
    exfat_dir_scan(dir_first, |_e| {
        empty = false;
        false
    });
    empty
}

/// 用新的元数据重写目录里 `name` 的 entry set (含 SetChecksum)。
fn exfat_rewrite_set(parent_first: u32, name: &[u8], e: &ExfatEntry) -> bool {
    let loc = match exfat_dir_locate(parent_first, name) {
        Some(l) => l,
        None => return false,
    };
    let mut set = [0u8; EXFAT_SET_MAX_BYTES];
    let total = match exfat_build_set(
        set.as_mut_ptr(),
        name,
        e.is_dir,
        e.no_fat_chain,
        e.first_cluster,
        e.size,
        e.valid_size,
        e.mtime,
    ) {
        Some(t) => t,
        None => return false,
    };
    if total != 1 + loc.sec_count {
        return false; // 名字编码长度变化会改变条目数, 不支持原地替换
    }
    exfat_dir_put_set(loc, set.as_ptr(), total)
}

/// 把绝对路径拆成 (父目录路径, 最后一段名字字节); 父目录为根时返回 `"/"`。
fn exfat_split_parent(path: &str) -> Option<(&str, &[u8])> {
    let b = path.as_bytes();
    if b.is_empty() || b[0] != b'/' || b.len() > TMP_PATH_MAX {
        return None;
    }
    let mut end = b.len();
    while end > 0 && b[end - 1] == b'/' {
        end -= 1;
    }
    if end <= 1 {
        return None; // 根或空路径: 不能创建/删除根
    }
    let mut i = end;
    while i > 0 && b[i - 1] != b'/' {
        i -= 1;
    }
    let name = &b[i..end];
    if name.is_empty() || name == b"." || name == b".." {
        return None;
    }
    let parent = if i <= 1 { "/" } else { &path[..i - 1] };
    Some((parent, name))
}

/// 创建文件 / 目录 (已存在且类型匹配则直接打开), 返回 fd。
fn exfat_create(path: &str, is_dir: bool, vol: u64) -> u64 {
    if path.is_empty() || path == "/" {
        return u64::MAX;
    }
    if let Some(e) = exfat_resolve(path) {
        return if e.is_dir == is_dir {
            exfat_fd_alloc(path, is_dir, vol)
        } else {
            u64::MAX
        };
    }
    let (pdir, name) = match exfat_split_parent(path) {
        Some(v) => v,
        None => return u64::MAX,
    };
    let parent = match exfat_resolve(pdir) {
        Some(e) if e.is_dir => e,
        _ => return u64::MAX,
    };
    // 目录先占一个清零的簇 (文件按需在写入时分配)。
    let mut first = 0u32;
    if is_dir {
        first = match exfat_alloc_cluster() {
            Some(cl) => cl,
            None => return u64::MAX,
        };
        let z = exfat_clu();
        zero_bytes(z, unsafe { EXFAT_CLUSTER_BYTES } as usize);
        if !exfat_write_cluster(first, z) {
            return u64::MAX;
        }
    }
    let mtime = mfs_now();
    let mut set = [0u8; EXFAT_SET_MAX_BYTES];
    let total = match exfat_build_set(set.as_mut_ptr(), name, is_dir, false, first, 0, 0, mtime) {
        Some(t) => t,
        None => return u64::MAX,
    };
    let loc = match exfat_dir_find_slot(parent.first_cluster, total) {
        Some(l) => l,
        None => {
            if exfat_dir_grow(parent.first_cluster).is_none() {
                return u64::MAX;
            }
            match exfat_dir_find_slot(parent.first_cluster, total) {
                Some(l) => l,
                None => return u64::MAX,
            }
        }
    };
    if !exfat_dir_put_set(loc, set.as_ptr(), total) {
        return u64::MAX;
    }
    exfat_fd_alloc(path, is_dir, vol)
}

/// 删除文件 (`want_dir = false`) 或空目录 (`want_dir = true`)。
fn exfat_remove(path: &str, want_dir: bool) -> u64 {
    let (pdir, name) = match exfat_split_parent(path) {
        Some(v) => v,
        None => return u64::MAX,
    };
    let parent = match exfat_resolve(pdir) {
        Some(e) if e.is_dir => e,
        _ => return u64::MAX,
    };
    let e = match exfat_dir_lookup(parent.first_cluster, name) {
        Some(e) => e,
        None => return u64::MAX,
    };
    if e.is_dir != want_dir {
        return u64::MAX;
    }
    if e.is_dir && !exfat_dir_is_empty(e.first_cluster) {
        return u64::MAX;
    }
    let loc = match exfat_dir_locate(parent.first_cluster, name) {
        Some(l) => l,
        None => return u64::MAX,
    };
    // 先摘名字 (此后对象不可达), 再释放簇。
    if !exfat_dir_del_set(loc) {
        return u64::MAX;
    }
    if e.first_cluster >= 2 {
        let n = exfat_entry_clusters(&e);
        if !exfat_free_chain(e.first_cluster, e.no_fat_chain, n) {
            return u64::MAX;
        }
    }
    1
}

/// 写文件区间 `[offset, offset+count)`; 需要时扩展簇链并更新 entry set。
fn exfat_write_file(
    e: &ExfatEntry,
    parent_first: u32,
    name: &[u8],
    offset: u32,
    count: u32,
    src: *const u8,
) -> Option<u64> {
    if e.is_dir || count == 0 {
        return Some(0);
    }
    let cb = unsafe { EXFAT_CLUSTER_BYTES };
    if cb == 0 {
        return None;
    }
    let end = offset as u64 + count as u64;
    let new_size = end.max(e.size);
    let need = new_size.div_ceil(cb as u64) as u32;
    let have = exfat_entry_clusters(e);
    let (first, no_chain, n) = exfat_grow_to(e.first_cluster, e.no_fat_chain, have, need)?;
    // 新增簇内容未定义, 先清零 (exFAT 无稀疏文件)。
    let mut idx = have.min(n);
    while idx < n {
        let cl = exfat_chain_nth(first, idx)?;
        let z = exfat_clu();
        zero_bytes(z, cb as usize);
        if !exfat_write_cluster(cl, z) {
            return None;
        }
        idx += 1;
    }
    let mut done = 0u32;
    let scratch = exfat_clu();
    while done < count {
        let pos = offset as u64 + done as u64;
        let cl = exfat_chain_nth(first, (pos / cb as u64) as u32)?;
        let boff = (pos % cb as u64) as usize;
        let chunk = (cb as usize - boff).min((count - done) as usize);
        if !exfat_read_cluster(cl, scratch) {
            return None;
        }
        unsafe {
            core::ptr::copy_nonoverlapping(src.add(done as usize), exfat_atm(scratch, boff), chunk);
        }
        if !exfat_write_cluster(cl, scratch) {
            return None;
        }
        done += chunk as u32;
    }
    let mut e2 = *e;
    e2.first_cluster = first;
    e2.no_fat_chain = no_chain;
    e2.size = new_size;
    e2.valid_size = e.valid_size.max(end).min(new_size);
    e2.mtime = mfs_now();
    if !exfat_rewrite_set(parent_first, name, &e2) {
        return None;
    }
    Some(count as u64)
}

/// 截断 / 扩展文件到 `size` 字节 (exFAT 无稀疏文件: 扩展会实际分配并清零簇)。
fn exfat_truncate(e: &ExfatEntry, parent_first: u32, name: &[u8], size: u32) -> Option<u64> {
    if e.is_dir {
        return None;
    }
    let cb = unsafe { EXFAT_CLUSTER_BYTES } as u64;
    if cb == 0 {
        return None;
    }
    let need = (size as u64).div_ceil(cb) as u32;
    let have = exfat_entry_clusters(e);
    // 截到 0: 释放整条链 (可能还是连续文件, 按 `have` 个簇处理)。
    if need == 0 {
        if !exfat_free_chain(e.first_cluster, e.no_fat_chain, have) {
            return None;
        }
        let mut e2 = *e;
        e2.first_cluster = 0;
        e2.no_fat_chain = false;
        e2.size = 0;
        e2.valid_size = 0;
        e2.mtime = mfs_now();
        return if exfat_rewrite_set(parent_first, name, &e2) {
            Some(0)
        } else {
            None
        };
    }
    let (first, no_chain, n) = exfat_grow_to(e.first_cluster, e.no_fat_chain, have, need)?;
    // 扩展: 新簇清零。
    let mut idx = have.min(n);
    while idx < n {
        let cl = exfat_chain_nth(first, idx)?;
        let z = exfat_clu();
        zero_bytes(z, cb as usize);
        if !exfat_write_cluster(cl, z) {
            return None;
        }
        idx += 1;
    }
    // 截短: 断链并释放尾部多余的簇。
    if n > need {
        let tail = exfat_chain_nth(first, need - 1)?;
        let next = exfat_fat_get(tail)?;
        if !exfat_fat_set(tail, EXFAT_FAT_EOC) {
            return None;
        }
        if next >= 2 && !exfat_is_eoc(next) && !exfat_free_chain(next, false, 0) {
            return None;
        }
    }
    let mut e2 = *e;
    e2.first_cluster = first;
    e2.no_fat_chain = no_chain;
    e2.size = size as u64;
    e2.valid_size = e.valid_size.min(size as u64);
    e2.mtime = mfs_now();
    if !exfat_rewrite_set(parent_first, name, &e2) {
        return None;
    }
    Some(size as u64)
}

/// 由 fd 保存的路径取出 (父目录簇, 名字长度); 名字拷进 `out`。写路径共用。
fn exfat_fd_parent(fd: &ExfatFd, out: &mut [u8; vfs::DIR_LONG_MAX]) -> Option<(u32, usize)> {
    let plen = fd.path_len as usize;
    if plen == 0 || plen > TMP_PATH_MAX {
        return None;
    }
    let path = unsafe { core::str::from_utf8_unchecked(&fd.path[..plen]) };
    let (pdir, name) = exfat_split_parent(path)?;
    if name.len() > vfs::DIR_LONG_MAX {
        return None;
    }
    let parent = exfat_resolve(pdir)?;
    if !parent.is_dir {
        return None;
    }
    out[..name.len()].copy_from_slice(name);
    Some((parent.first_cluster, name.len()))
}

// ---------------------------------------------------------------------------
// fd 表
// ---------------------------------------------------------------------------

fn ext2_fd_alloc(ino: u32, is_dir: bool, vol: u64) -> u64 {
    for i in 0..EXT2_MAX_FD {
        unsafe {
            let s = &mut *core::ptr::addr_of_mut!(EXT2_FDS).cast::<Ext2Fd>().add(i);
            if !s.used {
                s.used = true;
                s.is_dir = is_dir;
                s.ino = ino;
                s.vol = vol;
                return i as u64;
            }
        }
    }
    u64::MAX
}
/// 查 fd 并把「当前卷寄存器」切到该 fd 绑定的卷 (与路径类请求的 tag 卷编码等价)。
fn ext2_fd_get(fd: u32) -> Option<Ext2Fd> {
    if fd as usize >= EXT2_MAX_FD {
        return None;
    }
    unsafe {
        let s = &*core::ptr::addr_of!(EXT2_FDS)
            .cast::<Ext2Fd>()
            .add(fd as usize);
        if s.used {
            EXT2_CUR_VOL = s.vol;
            Some(*s)
        } else {
            None
        }
    }
}
fn ext2_fd_free(fd: u32) -> u64 {
    if fd as usize >= EXT2_MAX_FD {
        return 0;
    }
    unsafe {
        let s = &mut *core::ptr::addr_of_mut!(EXT2_FDS)
            .cast::<Ext2Fd>()
            .add(fd as usize);
        if s.used {
            s.used = false;
            1
        } else {
            0
        }
    }
}

// ---------------------------------------------------------------------------
// 挂载
// ---------------------------------------------------------------------------

/// 读取并校验超级块 + 块组描述符表; 成功即完成挂载 (只读, 不改盘)。
fn ext2_mount() -> bool {
    // 超级块固定在字节偏移 1024 (LBA 2), 前 1024 字节已含所需全部字段。
    let sb = ext2_a();
    if !block_read_dev(unsafe { EXT2_CUR_VOL }, 2, 2, sb) {
        return false;
    }
    if read_u16(unsafe { sb.add(0x38) }) != EXT2_MAGIC {
        return false;
    }
    let log_block_size = read_u32(unsafe { sb.add(0x18) });
    if log_block_size > 6 {
        return false; // 块大小上限 64 KiB
    }
    let block_size = 1024u32 << log_block_size;
    let blocks_count = read_u32(unsafe { sb.add(0x04) });
    let first_data_block = read_u32(unsafe { sb.add(0x14) });
    let blocks_per_group = read_u32(unsafe { sb.add(0x20) });
    let inodes_per_group = read_u32(unsafe { sb.add(0x28) });
    let rev_level = read_u32(unsafe { sb.add(0x4C) });
    let mut inode_size = 128u32;
    if rev_level != 0 {
        let s = read_u16(unsafe { sb.add(0x58) }) as u32;
        if s >= 128 {
            inode_size = s;
        }
    }
    if blocks_count == 0 || blocks_per_group == 0 || inodes_per_group == 0 {
        return false;
    }
    let groups = blocks_count.div_ceil(blocks_per_group);
    if groups == 0 || groups as usize > EXT2_MAX_GROUPS {
        return false;
    }
    // 卷参数必须先落盘到内存状态: 之后所有块 I/O 都要用 `EXT2_BLOCK_SIZE` 换算 LBA。
    unsafe {
        EXT2_BLOCK_SIZE = block_size;
        EXT2_INODES_PER_GROUP = inodes_per_group;
        EXT2_INODE_SIZE = inode_size;
        EXT2_GROUP_COUNT = groups;
    }
    // 块组描述符表紧随超级块所在块: 块号 = `s_first_data_block + 1`。
    let gdt_block = first_data_block + 1;
    let per_block = block_size / 32;
    if per_block == 0 {
        return false;
    }
    let gbuf = ext2_a();
    let mut g = 0u32;
    let mut blk = gdt_block;
    while g < groups {
        if !ext2_read_block(blk, gbuf) {
            return false;
        }
        let n = (groups - g).min(per_block);
        let mut k = 0u32;
        while k < n {
            let off = k as usize * 32 + 0x08; // bg_inode_table
            unsafe {
                EXT2_INODE_TABLE[(g + k) as usize] = read_u32(gbuf.add(off));
            }
            k += 1;
        }
        g += n;
        blk += 1;
    }
    // 根 inode 必须存在且是目录, 否则视为无效卷。
    let ok =
        matches!(ext2_read_inode(EXT2_ROOT_INO), Some(i) if i.mode & EXT2_S_IFMT == EXT2_S_IFDIR);
    if ok {
        // 记录「当前几何属于哪个卷」(M1b: 卷切换时据此判断要不要重新解析)。
        unsafe {
            EXT2_GEO_VOL = EXT2_CUR_VOL;
        }
    }
    ok
}

// ---------------------------------------------------------------------------
// 服务循环
// ---------------------------------------------------------------------------

/// 域 12 — ext2_srv: 只读服务 OPEN / READ / READDIR / STAT / CLOSE, 写操作一律拒绝。
fn ext2_main() {
    if sys_alloc_page(ext2_a() as u64) != 1
        || sys_alloc_page(ext2_b() as u64) != 1
        || sys_alloc_page(ext2_c() as u64) != 1
        || sys_alloc_page(ext2_d() as u64) != 1
    {
        println("ext2: alloc block buffers FAILED");
        return;
    }
    if sys_share_page(ext2_a() as u64, BLOCK_DOMAIN) != 1
        || sys_share_page(ext2_b() as u64, BLOCK_DOMAIN) != 1
        || sys_share_page(ext2_c() as u64, BLOCK_DOMAIN) != 1
        || sys_share_page(ext2_d() as u64, BLOCK_DOMAIN) != 1
    {
        println("ext2: share block buffers FAILED");
        return;
    }
    // 认领卷: 第一个 ext2 签名的卷; 无分区表的整盘镜像即卷 2 (回退值)。
    unsafe {
        EXT2_VOL = vol_claim(ext2_a(), 16, VOL_KIND_EXT2, EXT2_VOL_FALLBACK);
        EXT2_CUR_VOL = EXT2_VOL;
    }
    if !ext2_mount() {
        println("ext2: mount FAILED (not a valid ext2 volume)");
        return;
    }

    // M1b: 把**额外**的 ext2 卷挂到 `/usb<卷号>` (元数据已解析完, `ext2_a` 可作暂存)。
    mount_extra_volumes(
        ext2_a(),
        VOL_KIND_EXT2,
        unsafe { EXT2_VOL },
        vfs::EXT2_DOMAIN,
    );

    let mut msg = Message {
        from: 0,
        to: 0,
        tag: 0,
        payload: [0; PAYLOAD_LEN],
    };
    loop {
        sys_recv_msg(&mut msg as *mut Message as *mut u8);
        // 同 fat32_srv: tag 高位带卷编码 (M1b); fd 类请求的卷由 fd 绑定决定。
        let tag = vfs::tag_body(msg.tag);
        let mut vol = vfs::vol_from_enc(vfs::tag_vol(msg.tag), unsafe { EXT2_VOL });
        if matches!(tag, vfs::VFS_READ_TAG | vfs::VFS_READDIR_TAG) {
            if let Some(fd) = ext2_fd_get(read_u32(msg.payload.as_ptr())) {
                vol = fd.vol;
            }
        }
        unsafe {
            EXT2_CUR_VOL = vol;
        }
        // 卷切换: 各 ext2 卷的块大小 / inode 表位置不同, 必须重新解析该卷的超级块与
        // 块组描述符表 (只读, 不改盘), 否则会用上个卷的几何去换算块号。
        if unsafe { EXT2_GEO_VOL } != vol && !ext2_mount() {
            sys_reply(u64::MAX);
            continue;
        }
        match tag {
            vfs::VFS_OPEN_TAG => {
                let len = msg
                    .payload
                    .iter()
                    .position(|&b| b == 0)
                    .unwrap_or(PAYLOAD_LEN);
                let path = unsafe { core::str::from_utf8_unchecked(&msg.payload[..len]) };
                let fd = match ext2_resolve(path) {
                    Some((ino, is_dir)) => ext2_fd_alloc(ino, is_dir, vol),
                    None => u64::MAX,
                };
                sys_reply(fd);
            }
            vfs::VFS_READ_TAG => {
                let req: vfs::ReadReq = unsafe {
                    core::ptr::read_unaligned(msg.payload.as_ptr() as *const vfs::ReadReq)
                };
                // ext2 inode 的 size 是 u32: 协议 offset 超出 u32 直接失败。
                if req.offset > u32::MAX as u64 {
                    sys_reply(u64::MAX);
                    continue;
                }
                let n = match ext2_fd_get(req.fd) {
                    Some(fd) if !fd.is_dir => {
                        ext2_read_data(fd.ino, req.offset as u32, req.count, req.buf as *mut u8)
                            .unwrap_or(u64::MAX)
                    }
                    _ => u64::MAX,
                };
                sys_reply(n);
            }
            vfs::VFS_READDIR_TAG => {
                let req: vfs::DirReq = unsafe {
                    core::ptr::read_unaligned(msg.payload.as_ptr() as *const vfs::DirReq)
                };
                let n = match ext2_fd_get(req.fd) {
                    Some(fd) if fd.is_dir => {
                        ext2_readdir(fd.ino, req.buf as *mut vfs::DirEntry).unwrap_or(u64::MAX)
                    }
                    _ => u64::MAX,
                };
                sys_reply(n);
            }
            vfs::VFS_STAT_TAG => {
                let (buf, path) = parse_path_req(msg.payload.as_ptr());
                let n = match ext2_resolve(path) {
                    Some((ino, is_dir)) => match ext2_read_inode(ino) {
                        Some(inode) => {
                            // ext2 侧不做元数据映射 (M5 只覆盖 MFS), 大小按类型给。
                            let st = vfs::Stat::plain(
                                if is_dir { 0 } else { inode.size as u64 },
                                u32::from(is_dir),
                            );
                            unsafe {
                                core::ptr::write_unaligned(buf as *mut vfs::Stat, st);
                            }
                            core::mem::size_of::<vfs::Stat>() as u64
                        }
                        None => u64::MAX,
                    },
                    None => u64::MAX,
                };
                sys_reply(n);
            }
            vfs::VFS_CLOSE_TAG => {
                let fd = read_u32(msg.payload.as_ptr());
                sys_reply(ext2_fd_free(fd));
            }
            // 只读服务: WRITE / CREAT / MKDIR / UNLINK / RMDIR 及其它一律拒绝。
            _ => {
                sys_reply(u64::MAX);
            }
        }
    }
}

// ===========================================================================
// 域 13 — exFAT 读写文件服务 (exfat_srv)
// ===========================================================================
// 阶段 D/M6a: 挂载宿主 `mkfs.exfat` 预格式化的 exFAT 卷 (U 盘/分区) 的只读兼容。
// 服务**不**自动格式化 —— 与 ext2 同: 定位是读写别人已有的卷, 而非自建。
//
// 已实现的读取子集:
//   - 引导扇区 (sector 0) + 备份 (sector 12) 的 boot checksum 校验;
//   - FAT 链表 (每簇一个 u32; `0` = 空闲, `>= 0xFFFFFFF8` = 链尾);
//   - 集群堆映射 (cluster → LBA) 与目录 entry set 解析;
//   - 分配位图 (0x81) / upcase 表 (0x82) 整体载入内存;
//   - 文件读取 (含 `NoFatChain` 连续文件) 与目录遍历。
//
// 边界 (M6a): 只读; 只支持 `BytesPerSectorShift == 9` (512B 扇区, mkfs.exfat 与
// 常见 U 盘均如此), 更大扇区在挂载时拒绝; 位图 / upcase 表必须能整体装入缓存
// (本卷尺寸下足够)。M6b 再加分配位图分配与 entry set 增删。

/// 卷号回退值: 5 对应 `build/exfat.img` (namespace 5, 见 Makefile)。
const EXFAT_VOL_FALLBACK: u64 = 5;
/// exFAT 服务实际使用的卷号, 启动时由 `vol_claim` 认领。
static mut EXFAT_VOL: u64 = EXFAT_VOL_FALLBACK;

/// exFAT **集群缓冲**虚拟地址 (紧跟 MFS 的 `+0x11_0000..0x11_4000` 之后)。
///
/// 与其它块缓冲页同理: 必须位于程序镜像之外, 且以「同地址」共享给 block_srv
/// 供其 DMA 读写, 否则 NVMe 会回「非法字段」。
/// 尺寸按实际簇大小**动态分配** (`spc` 页), 上限 `EXFAT_MAX_CLUSTER_PAGES`
/// (256 KiB 簇); 因此后续固定窗口从该上限之上开始排布, 避免重叠。
const EXFAT_CLU_VADDR: u64 = 0x0000_0080_0011_4000;
/// 集群缓冲页数上限 (对应 `spc_shift <= 9`, 即 256 KiB 簇)。
const EXFAT_MAX_CLUSTER_PAGES: usize = 64;
/// 分配位图窗口 (一页 = 一个 512B 扇区 + 余量, 按需读入、按扇区回写)。
///
/// 位图不再整体载入内存 —— 大容量卷的位图可达数十 KB 甚至 MB。
const EXFAT_BMP_VADDR: u64 = 0x0000_0080_0015_4000;
/// upcase 表窗口 (一页, 按需读入; 只在生成 NameHash 时用到)。
const EXFAT_UPC_VADDR: u64 = 0x0000_0080_0015_5000;
/// 通用单页暂存: FAT 表项读改写、引导扇区、卷表扫描。
const EXFAT_PG_VADDR: u64 = 0x0000_0080_0015_6000;

fn exfat_clu() -> *mut u8 {
    EXFAT_CLU_VADDR as *mut u8
}
fn exfat_bmp() -> *mut u8 {
    EXFAT_BMP_VADDR as *mut u8
}
fn exfat_upc() -> *mut u8 {
    EXFAT_UPC_VADDR as *mut u8
}
fn exfat_pg() -> *mut u8 {
    EXFAT_PG_VADDR as *mut u8
}
fn exfat_at(buf: *const u8, off: usize) -> *const u8 {
    unsafe { buf.add(off) }
}
fn exfat_atm(buf: *mut u8, off: usize) -> *mut u8 {
    unsafe { buf.add(off) }
}

/// 目录项类型 (高位置 1 = 在用; 清位即「已删除」)。
const EXFAT_TYPE_UNUSED: u8 = 0x00; // 目录结尾
const EXFAT_TYPE_BITMAP: u8 = 0x81;
const EXFAT_TYPE_UPCASE: u8 = 0x82;
const EXFAT_TYPE_LABEL: u8 = 0x83;
const EXFAT_TYPE_FILE: u8 = 0x85;
const EXFAT_TYPE_STREAM: u8 = 0xC0;
const EXFAT_TYPE_NAME: u8 = 0xC1;
/// `FileAttributes` 的目录位。
const EXFAT_ATTR_DIR: u16 = 0x0010;
/// `GeneralSecondaryFlags` 的 `NoFatChain` 位 (1 = 该文件连续, 忽略 FAT 链)。
const EXFAT_SF_NOFATCHAIN: u8 = 0x02;
/// FAT 值: `0` = 空闲; `>= EXFAT_FAT_EOC_MIN` = 链尾。
const EXFAT_FAT_FREE: u32 = 0;
const EXFAT_FAT_EOC_MIN: u32 = 0xFFFF_FFF8;

/// 每个目录项 32 字节。
const EXFAT_DIR_ENTRY: usize = 32;
/// 打开文件上限。
const EXFAT_MAX_FD: usize = 16;
/// 仅支持 512 字节扇区 (`BytesPerSectorShift == 9`)。
const EXFAT_SECTOR_SIZE: u32 = 512;
/// 簇大小上限对应的 `SectorsPerClusterShift` (256 KiB 簇, 受集群缓冲页数约束)。
const EXFAT_MAX_SPC_SHIFT: u8 = 9;
/// 页大小 (逐页分配 / 共享的粒度)。
const EXFAT_PAGE_SIZE: usize = 4096;

// 挂载后固定的卷参数 (内存镜像)。
static mut EXFAT_SECTORS_PER_CLUSTER: u32 = 0;
static mut EXFAT_CLUSTER_BYTES: u32 = 0;
static mut EXFAT_FAT_OFFSET: u32 = 0; // 单位: 扇区
static mut EXFAT_FAT_LENGTH: u32 = 0; // 单位: 扇区
static mut EXFAT_HEAP_OFFSET: u32 = 0; // 单位: 扇区
static mut EXFAT_CLUSTER_COUNT: u32 = 0;
static mut EXFAT_ROOT_CLUSTER: u32 = 0;
static mut EXFAT_NUM_FATS: u32 = 1;
static mut EXFAT_BITMAP_CLUSTER: u32 = 0;
static mut EXFAT_BITMAP_BYTES: u32 = 0;
static mut EXFAT_UPCASE_CLUSTER: u32 = 0;
static mut EXFAT_UPCASE_BYTES: u32 = 0;

/// 目录 entry set 解析结果 (一个文件/目录的描述)。
#[derive(Clone, Copy)]
struct ExfatEntry {
    is_dir: bool,
    no_fat_chain: bool,
    first_cluster: u32,
    size: u64,
    valid_size: u64,
    mtime: u64,
    name_len: u8,
    name: [u8; vfs::DIR_LONG_MAX],
}
impl ExfatEntry {
    const EMPTY: ExfatEntry = ExfatEntry {
        is_dir: false,
        no_fat_chain: false,
        first_cluster: 0,
        size: 0,
        valid_size: 0,
        mtime: 0,
        name_len: 0,
        name: [0; vfs::DIR_LONG_MAX],
    };
}

/// 打开文件描述符: 记住规范化路径, 每次操作重新解析 (元数据不会变陈旧)。
#[derive(Clone, Copy)]
struct ExfatFd {
    used: bool,
    is_dir: bool,
    path_len: u8,
    path: [u8; TMP_PATH_MAX],
    /// 打开时绑定的卷号 (M1b 多卷挂载)。
    vol: u64,
}
const EXFAT_FD_EMPTY: ExfatFd = ExfatFd {
    used: false,
    is_dir: false,
    path_len: 0,
    path: [0; TMP_PATH_MAX],
    vol: 0,
};
static mut EXFAT_FDS: [ExfatFd; EXFAT_MAX_FD] = [EXFAT_FD_EMPTY; EXFAT_MAX_FD];

/// 本服务**当前请求**落在的卷号 (M1b 多卷挂载; 见 fat32_srv 的 `FAT_CUR_VOL` 注释)。
static mut EXFAT_CUR_VOL: u64 = 0;

/// 已解析的几何 (引导区 / FAT / 集群堆 / 位图 / upcase) 属于哪个卷。各卷的簇大小与
/// 各部分偏移都不同, 请求落到别的卷上必须重新挂载解析 (见 `exfat_mount`)。
static mut EXFAT_GEO_VOL: u64 = u64::MAX;
/// 集群缓冲**已分配并共享**的页数 (卷切换只需补分配差额, 见 `exfat_bufs_init`)。
static mut EXFAT_BUFS_PAGES: usize = 0;

// ---------------------------------------------------------------------------
// 块 I/O / FAT / 集群
// ---------------------------------------------------------------------------

/// 经 block_srv 读 `count` 个 512B 扇区到 `dst`。
fn exfat_read_sectors(lba: u32, count: u16, dst: *mut u8) -> bool {
    block_read_dev(unsafe { EXFAT_CUR_VOL }, lba, count, dst)
}

/// 集群 `cl` 的首个 512B 扇区号 (集群 2 是堆内第一簇)。
fn exfat_cluster_lba(cl: u32) -> u32 {
    unsafe { EXFAT_HEAP_OFFSET + (cl - 2) * EXFAT_SECTORS_PER_CLUSTER }
}

/// 读一个完整集群到 `dst` (`dst` 必须至少有 `spc` 页)。
fn exfat_read_cluster(cl: u32, dst: *mut u8) -> bool {
    if cl < 2 {
        return false;
    }
    let spc = unsafe { EXFAT_SECTORS_PER_CLUSTER };
    if spc == 0 || spc > u16::MAX as u32 {
        return false;
    }
    exfat_read_sectors(exfat_cluster_lba(cl), spc as u16, dst)
}

/// 读集群 `cl` 的 FAT 表项。FAT 表项 4 字节对齐, 必落在单个 512B 扇区内。
fn exfat_fat_get(cl: u32) -> Option<u32> {
    let byte = unsafe { EXFAT_FAT_OFFSET } as u64 * EXFAT_SECTOR_SIZE as u64 + cl as u64 * 4;
    let lba = (byte / EXFAT_SECTOR_SIZE as u64) as u32;
    let off = (byte % EXFAT_SECTOR_SIZE as u64) as usize;
    let buf = exfat_pg();
    if !exfat_read_sectors(lba, 1, buf) {
        return None;
    }
    Some(read_u32(exfat_at(buf, off)))
}

fn exfat_is_eoc(v: u32) -> bool {
    v >= EXFAT_FAT_EOC_MIN
}

/// 分配位图里集群 `cl` 是否已占用 (按需读入所在扇区)。
fn exfat_bitmap_get(cl: u32) -> bool {
    match exfat_bitmap_byte(cl) {
        Some(p) => unsafe { *p & (1u8 << ((cl - 2) & 7)) != 0 },
        None => false,
    }
}

/// 定位位图中集群 `cl` 对应的那个字节所在扇区, 返回窗口内的字节指针。
///
/// 窗口是**写回缓存**: 一旦装载了新扇区, 之前的脏扇区会先落盘。
fn exfat_bitmap_byte(cl: u32) -> Option<*mut u8> {
    if cl < 2 {
        return None;
    }
    let byte = (cl - 2) as usize / 8;
    if byte >= unsafe { EXFAT_BITMAP_BYTES } as usize {
        return None;
    }
    let sec = byte / EXFAT_SECTOR_SIZE as usize;
    let off = byte % EXFAT_SECTOR_SIZE as usize;
    if unsafe { EXFAT_BMP_WIN_SEC } != sec {
        if !exfat_bitmap_flush() {
            return None;
        }
        let lba = exfat_chain_sec_lba(unsafe { EXFAT_BITMAP_CLUSTER }, sec)?;
        if !exfat_read_sectors(lba, 1, exfat_bmp()) {
            return None;
        }
        unsafe {
            EXFAT_BMP_WIN_SEC = sec;
            EXFAT_BMP_WIN_LBA = lba;
        }
    }
    Some(exfat_atm(exfat_bmp(), off))
}

/// 只改内存位图 (1 = 占用), 标脏由 `exfat_bitmap_flush` 落盘。
fn exfat_bitmap_put(cl: u32, used: bool) -> bool {
    let mask = 1u8 << ((cl - 2) & 7);
    let p = match exfat_bitmap_byte(cl) {
        Some(p) => p,
        None => return false,
    };
    unsafe {
        if used {
            *p |= mask;
        } else {
            *p &= !mask;
        }
        EXFAT_BMP_DIRTY = true;
    }
    true
}

/// 把位图窗口里的脏扇区写回 (无脏数据时是空操作)。
fn exfat_bitmap_flush() -> bool {
    if !unsafe { EXFAT_BMP_DIRTY } {
        return true;
    }
    let lba = unsafe { EXFAT_BMP_WIN_LBA };
    if !exfat_write_sectors(lba, 1, exfat_bmp()) {
        return false;
    }
    unsafe { EXFAT_BMP_DIRTY = false };
    true
}

/// 文件逻辑簇号 `idx` 对应的物理簇 (连续文件直接相加, 否则沿 FAT 链走)。
fn exfat_cluster_at(e: &ExfatEntry, idx: u32) -> Option<u32> {
    if e.no_fat_chain {
        let cl = e.first_cluster.checked_add(idx)?;
        if cl < 2 || cl >= unsafe { EXFAT_CLUSTER_COUNT } + 2 {
            return None;
        }
        return Some(cl);
    }
    let mut cl = e.first_cluster;
    let mut i = 0u32;
    while i < idx {
        let next = exfat_fat_get(cl)?;
        if next == EXFAT_FAT_FREE || exfat_is_eoc(next) {
            return None;
        }
        cl = next;
        i += 1;
    }
    if cl < 2 {
        return None;
    }
    Some(cl)
}

// ---------------------------------------------------------------------------
// 引导扇区 / 目录项
// ---------------------------------------------------------------------------

/// boot checksum: 主引导区前 11 个扇区的滚动 32 位校验, 跳过 `VolumeFlags`
/// (106/107) 与 `PercentInUse` (112); 结果重复填入第 11 扇区。
fn exfat_verify_boot_checksum() -> bool {
    // 前 11 个 512B 扇区 = 5632 字节; 逐扇区读进单页暂存后滚动累加,
    // 不依赖「一次读多页」, 也不需要额外的连续两页缓冲。
    let buf = exfat_pg();
    let mut sum: u32 = 0;
    let mut sec: usize = 0;
    while sec < 11 {
        if !exfat_read_sectors(sec as u32, 1, buf) {
            return false;
        }
        let base = sec * EXFAT_SECTOR_SIZE as usize;
        let mut i = 0usize;
        while i < EXFAT_SECTOR_SIZE as usize {
            let off = base + i;
            if off != 106 && off != 107 && off != 112 {
                let byte = unsafe { *exfat_at(buf, i) } as u32;
                let rot: u32 = if sum & 1 != 0 { 0x8000_0000 } else { 0 };
                sum = rot.wrapping_add(sum >> 1).wrapping_add(byte);
            }
            i += 1;
        }
        sec += 1;
    }
    if !exfat_read_sectors(11, 1, buf) {
        return false;
    }
    read_u32(buf) == sum
}

/// entry set 的 16 位校验和: 覆盖整组 (count 字节), 跳过 SetChecksum 字段本身
/// (首项第 2/3 字节)。
fn exfat_set_checksum(e: *const u8, count: u32) -> u16 {
    let mut sum: u16 = 0;
    let mut i = 0u32;
    while i < count {
        if i != 2 && i != 3 {
            let byte = unsafe { *e.add(i as usize) } as u16;
            let rot: u16 = if sum & 1 != 0 { 0x8000 } else { 0 };
            sum = rot.wrapping_add(sum >> 1).wrapping_add(byte);
        }
        i += 1;
    }
    sum
}

/// UTF-16 码元 → UTF-8 (仅基本多文种平面; 代理对不处理)。
fn exfat_utf16_to_utf8(c: u16, out: &mut [u8; 3]) -> usize {
    if c < 0x80 {
        out[0] = c as u8;
        1
    } else if c < 0x800 {
        out[0] = 0xC0 | (c >> 6) as u8;
        out[1] = 0x80 | (c & 0x3F) as u8;
        2
    } else {
        out[0] = 0xE0 | (c >> 12) as u8;
        out[1] = 0x80 | ((c >> 6) & 0x3F) as u8;
        out[2] = 0x80 | (c & 0x3F) as u8;
        3
    }
}

/// 从首个 File Name 条目起收集 `units` 个 UTF-16 码元并转成 UTF-8。
///
/// `first` 是首个 0xC1 条目在 `buf` 中的**条目下标**; 每 15 个字符占一个条目。
fn exfat_read_name(
    buf: *const u8,
    first: usize,
    units: usize,
    out: &mut [u8; vfs::DIR_LONG_MAX],
) -> u8 {
    let mut n = 0usize;
    let mut k = 0usize;
    while k < units {
        let ent = k / 15;
        let pos = k % 15;
        let e = exfat_at(buf, (first + ent) * EXFAT_DIR_ENTRY);
        let c = read_u16(exfat_at(e, 2 + pos * 2));
        let mut tmp = [0u8; 3];
        let l = exfat_utf16_to_utf8(c, &mut tmp);
        let mut j = 0usize;
        while j < l {
            if n >= vfs::DIR_LONG_MAX {
                return n as u8;
            }
            out[n] = tmp[j];
            n += 1;
            j += 1;
        }
        k += 1;
    }
    n as u8
}

/// 解析 `first_index` 处的一组文件 entry set (0x85 + 0xC0 + N×0xC1)。
///
/// 校验 SetChecksum; 不完整 / 校验失败 / 跨簇边界一律返回 None (跳过该组)。
fn exfat_parse_file_set(
    buf: *const u8,
    first_index: usize,
    per: usize,
    sec_count: usize,
) -> Option<ExfatEntry> {
    if sec_count < 2 {
        return None;
    }
    let total = 1 + sec_count;
    if first_index + total > per {
        return None; // entry set 不跨簇边界
    }
    let file = exfat_at(buf, first_index * EXFAT_DIR_ENTRY);
    let stream = exfat_at(buf, (first_index + 1) * EXFAT_DIR_ENTRY);
    if unsafe { *stream } != EXFAT_TYPE_STREAM {
        return None;
    }
    let units = unsafe { *exfat_at(stream, 3) } as usize;
    if units == 0 || units > 255 {
        return None;
    }
    let name_entries = units.div_ceil(15);
    if 2 + name_entries > total {
        return None;
    }
    let stored = read_u16(exfat_at(file, 2));
    if stored != exfat_set_checksum(file, (total * EXFAT_DIR_ENTRY) as u32) {
        return None;
    }
    let mut e = ExfatEntry::EMPTY;
    let attrs = read_u16(exfat_at(file, 4));
    e.is_dir = attrs & EXFAT_ATTR_DIR != 0;
    let flags = unsafe { *exfat_at(stream, 1) };
    e.no_fat_chain = flags & EXFAT_SF_NOFATCHAIN != 0;
    e.first_cluster = read_u32(exfat_at(stream, 20));
    e.valid_size = read_u64(exfat_at(stream, 8));
    e.size = read_u64(exfat_at(stream, 24));
    e.mtime = exfat_decode_time(read_u32(exfat_at(file, 12)), unsafe { *exfat_at(file, 21) });
    let mut long = [0u8; vfs::DIR_LONG_MAX];
    e.name_len = exfat_read_name(buf, first_index + 2, units, &mut long);
    e.name = long;
    if e.name_len == 0 {
        return None;
    }
    Some(e)
}

/// exFAT 打包时间戳 → Unix 秒 (忽略 UTC 偏移字段, 按 UTC 处理)。
///
/// 位域: `[4:0]` 2 秒计数 / `[10:5]` 分 / `[15:11]` 时 / `[20:16]` 日 /
/// `[24:21]` 月 / `[31:25]` 年 - 1980; `ten_ms` 是 0~199 的 10ms 增量。
fn exfat_decode_time(ts: u32, ten_ms: u8) -> u64 {
    if ts == 0 {
        return 0;
    }
    let sec = (ts & 0x1F) as i64 * 2;
    let min = ((ts >> 5) & 0x3F) as i64;
    let hour = ((ts >> 11) & 0x1F) as i64;
    let day = ((ts >> 16) & 0x1F) as i64;
    let mon = ((ts >> 21) & 0x0F) as i64;
    let year = 1980 + ((ts >> 25) & 0x7F) as i64;
    if day == 0 || mon == 0 {
        return 0;
    }
    let secs = days_from_civil(year, mon, day) * 86_400
        + hour * 3600
        + min * 60
        + sec
        + (ten_ms as i64) / 100;
    if secs < 0 {
        0
    } else {
        secs as u64
    }
}

/// 遍历目录 `dir_first` 的所有文件 entry set; 回调返回 false 表示提前停止。
///
/// 目录本身是 FAT 链; 逐簇读取, 遇 `0x00` 条目即目录结束。
fn exfat_dir_scan<F: FnMut(&ExfatEntry) -> bool>(dir_first: u32, mut cb: F) -> bool {
    let cb_size = unsafe { EXFAT_CLUSTER_BYTES } as usize;
    if cb_size < EXFAT_DIR_ENTRY {
        return false;
    }
    let per = cb_size / EXFAT_DIR_ENTRY;
    let buf = exfat_clu();
    let mut cl = dir_first;
    let mut guard = 0u32;
    while cl >= 2 && guard <= unsafe { EXFAT_CLUSTER_COUNT } + 1 {
        if !exfat_read_cluster(cl, buf) {
            return false;
        }
        let mut i = 0usize;
        while i < per {
            let t = unsafe { *exfat_at(buf, i * EXFAT_DIR_ENTRY) };
            if t == EXFAT_TYPE_UNUSED {
                // `0x00` = 本簇剩余条目未使用; 目录链的后续簇仍要扫描
                // (否则「本簇放不下整组条目而留白」会遮住后面簇里的条目)。
                break;
            }
            if t == EXFAT_TYPE_FILE {
                let sec = unsafe { *exfat_at(buf, i * EXFAT_DIR_ENTRY + 1) } as usize;
                if let Some(e) = exfat_parse_file_set(buf, i, per, sec) {
                    if !cb(&e) {
                        return true;
                    }
                }
                i += 1 + sec;
                continue;
            }
            i += 1;
        }
        let next = match exfat_fat_get(cl) {
            Some(v) => v,
            None => return false,
        };
        if next == EXFAT_FAT_FREE || exfat_is_eoc(next) {
            break;
        }
        cl = next;
        guard += 1;
    }
    true
}

/// 名字比较; `ci = true` 按 ASCII 大小写不敏感。
fn exfat_name_eq(a: &[u8], b: &[u8], ci: bool) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0usize;
    while i < a.len() {
        if a[i] == b[i] || (ci && ascii_upper(a[i]) == ascii_upper(b[i])) {
            i += 1;
            continue;
        }
        return false;
    }
    true
}

/// 在目录 `dir_first` 中按名查找 (大小写不敏感)。
fn exfat_dir_lookup(dir_first: u32, want: &[u8]) -> Option<ExfatEntry> {
    let mut found: Option<ExfatEntry> = None;
    exfat_dir_scan(dir_first, |e| {
        if exfat_name_eq(&e.name[..e.name_len as usize], want, true) {
            found = Some(*e);
            false
        } else {
            true
        }
    });
    found
}

/// 把 entry set 转成 VFS 目录条目 (长名 + 8.3 后备 + 大小 / 时间)。
fn exfat_make_direntry(e: &ExfatEntry) -> vfs::DirEntry {
    let name = &e.name[..e.name_len as usize];
    let short = ext2_short_name(name);
    let mut long = [0u8; vfs::DIR_LONG_MAX];
    let llen = ext2_copy_long(name, &mut long);
    let size = if e.is_dir { 0 } else { e.size };
    let mut de = vfs::DirEntry::with_long(short, long, llen, size, u32::from(e.is_dir));
    de.mtime = e.mtime;
    de
}

// ---------------------------------------------------------------------------
// 路径解析 / 数据读取
// ---------------------------------------------------------------------------

/// 把绝对路径解析为 entry (根目录是特例: 没有 entry set, 直接返回根簇)。
fn exfat_resolve(path: &str) -> Option<ExfatEntry> {
    let bytes = path.as_bytes();
    if bytes.is_empty() || bytes[0] != b'/' {
        return None;
    }
    let mut cur = ExfatEntry::EMPTY;
    cur.is_dir = true;
    cur.first_cluster = unsafe { EXFAT_ROOT_CLUSTER };
    let mut i = 1usize;
    while i < bytes.len() {
        let start = i;
        while i < bytes.len() && bytes[i] != b'/' {
            i += 1;
        }
        let comp = &bytes[start..i];
        let had_sep = i < bytes.len();
        if had_sep {
            i += 1;
        }
        if comp.is_empty() || comp == b"." {
            continue;
        }
        if !cur.is_dir {
            return None;
        }
        cur = exfat_dir_lookup(cur.first_cluster, comp)?;
        if had_sep && !cur.is_dir {
            return None; // 中间分量必须是目录
        }
    }
    Some(cur)
}

/// 读文件区间 `[offset, offset+count)` 到 `dst`, 返回实际读取字节数。
///
/// 超过 `ValidDataLength` 的已分配区段按 0 读 (exFAT 的「有效数据长度」语义)。
fn exfat_read_file(e: &ExfatEntry, offset: u32, count: u32, dst: *mut u8) -> Option<u64> {
    if e.is_dir {
        return None;
    }
    if offset as u64 >= e.size {
        return Some(0);
    }
    let n = (count as u64).min(e.size - offset as u64) as u32;
    let cb = unsafe { EXFAT_CLUSTER_BYTES };
    if cb == 0 {
        return None;
    }
    let buf = exfat_clu();
    let mut done = 0u32;
    while done < n {
        let pos = offset + done;
        let boff = (pos % cb) as usize;
        let chunk = (cb as usize - boff).min((n - done) as usize);
        let cl = exfat_cluster_at(e, pos / cb).unwrap_or(0);
        if pos as u64 >= e.valid_size || cl < 2 {
            zero_bytes(unsafe { dst.add(done as usize) }, chunk);
        } else {
            if !exfat_read_cluster(cl, buf) {
                return None;
            }
            unsafe {
                core::ptr::copy_nonoverlapping(exfat_at(buf, boff), dst.add(done as usize), chunk);
            }
        }
        done += chunk as u32;
    }
    Some(n as u64)
}

/// 列出目录条目 (跳过系统项与卷标), 返回写入字节数。
fn exfat_readdir(dir_first: u32, dst: *mut vfs::DirEntry) -> Option<u64> {
    let entry_size = core::mem::size_of::<vfs::DirEntry>();
    let mut count = 0usize;
    let ok = exfat_dir_scan(dir_first, |e| {
        if count >= vfs::RESULT_MAX_ENTRIES {
            return false;
        }
        let de = exfat_make_direntry(e);
        unsafe {
            core::ptr::write_unaligned(dst.add(count), de);
        }
        count += 1;
        true
    });
    if !ok {
        return None;
    }
    Some((count * entry_size) as u64)
}

// ---------------------------------------------------------------------------
// 写路径 (M6b)
// ---------------------------------------------------------------------------
//
// 设计要点:
//   * 我们创建的文件/目录一律用 **FAT 链** (不置 `NoFatChain`); 改写既有的
//     连续文件时先把它转成 FAT 链 (补齐簇间链接), 之后只有一种寻簇方式。
//   * 顺序保证「不留下悬空引用」: 创建 = 先备好簇与数据, 最后写目录项;
//     删除 = 先摘目录项 (文件即刻不可达), 再释放簇。
//   * 位图与 FAT 是两份持久状态: 改位图后立即 `exfat_bitmap_flush`,
//     FAT 表项改动逐项落盘 (有第二份 FAT 时同步镜像)。

/// FAT 链尾标记 (mkfs.exfat 写成全 1)。
const EXFAT_FAT_EOC: u32 = 0xFFFF_FFFF;
/// `FileAttributes` 的归档位 (普通文件)。
const EXFAT_ATTR_ARCHIVE: u16 = 0x0020;
/// `GeneralSecondaryFlags` 的 `AllocationPossible` 位 (1 = 允许含分配)。
const EXFAT_SF_ALLOC: u8 = 0x01;
/// 目录尾部空位的填充值: 「已删除的良性次级项」(InUse=0, 非 0)。
///
/// exFAT 要求未使用项 (`0x00`) 之后不得再出现非 0 项, 故目录扩容时
/// 必须把链尾簇的空位「占掉」, 不能留 `0x00`。
const EXFAT_FILLER: u8 = 0x20;
/// entry set 缓冲上限: `0x85` + `0xC0` + 17 个 `0xC1` (255 / 15)。
const EXFAT_SET_MAX_ENTRIES: usize = 19;
const EXFAT_SET_MAX_BYTES: usize = EXFAT_SET_MAX_ENTRIES * EXFAT_DIR_ENTRY;
/// 每个 `0xC1` 承载的 UTF-16 码元数。
const EXFAT_NAME_UNITS_PER_ENTRY: usize = 15;
/// 分配游标: 从上次分配处继续找空闲簇, 避免每次都从第 2 簇线性扫描。
static mut EXFAT_ALLOC_HINT: u32 = 2;

/// 经 block_srv 写 `count` 个 512B 扇区 (源需页对齐; 超过块层单命令上限时自动切分)。
fn exfat_write_sectors(lba: u32, count: u16, src: *mut u8) -> bool {
    if count == 0 {
        return false;
    }
    block_write_dev(unsafe { EXFAT_CUR_VOL }, lba, count, src)
}

/// 写一个完整集群 (`src` 必须至少有 `spc` 页)。
fn exfat_write_cluster(cl: u32, src: *mut u8) -> bool {
    if cl < 2 {
        return false;
    }
    let spc = unsafe { EXFAT_SECTORS_PER_CLUSTER };
    if spc == 0 || spc > u16::MAX as u32 {
        return false;
    }
    exfat_write_sectors(exfat_cluster_lba(cl), spc as u16, src)
}

/// 写集群 `cl` 的 FAT 表项 (读-改-写所在扇区); 有第二份 FAT 时同步镜像。
///
/// FAT 表项 4 字节对齐, 必落在单个 512B 扇区内, 故每次只动一个扇区。
fn exfat_fat_set(cl: u32, val: u32) -> bool {
    let mut idx = 0u32;
    loop {
        let byte = (unsafe { EXFAT_FAT_OFFSET } as u64
            + idx as u64 * unsafe { EXFAT_FAT_LENGTH } as u64)
            * EXFAT_SECTOR_SIZE as u64
            + cl as u64 * 4;
        let lba = (byte / EXFAT_SECTOR_SIZE as u64) as u32;
        let off = (byte % EXFAT_SECTOR_SIZE as u64) as usize;
        let buf = exfat_pg();
        if !exfat_read_sectors(lba, 1, buf) {
            return false;
        }
        write_u32(exfat_atm(buf, off), val);
        if !exfat_write_sectors(lba, 1, buf) {
            return false;
        }
        idx += 1;
        if idx >= unsafe { EXFAT_NUM_FATS } {
            return true;
        }
    }
}

/// 分配一个空闲集群: 置位图 + FAT 置链尾 + 落盘 (位图与 FAT 保持一致)。
fn exfat_alloc_cluster() -> Option<u32> {
    let count = unsafe { EXFAT_CLUSTER_COUNT };
    if count == 0 {
        return None;
    }
    let start = unsafe { EXFAT_ALLOC_HINT };
    let mut i = 0u32;
    while i < count {
        let cl = 2 + ((start - 2 + i) % count);
        if !exfat_bitmap_get(cl) {
            if !exfat_bitmap_put(cl, true) || !exfat_fat_set(cl, EXFAT_FAT_EOC) {
                return None;
            }
            if !exfat_bitmap_flush() {
                return None;
            }
            unsafe {
                EXFAT_ALLOC_HINT = if cl + 1 < count + 2 { cl + 1 } else { 2 };
            }
            return Some(cl);
        }
        i += 1;
    }
    None
}

/// 释放整条簇链: 清位图 + FAT 归零; `no_fat_chain` 时按 `nclusters` 个连续簇处理。
fn exfat_free_chain(first: u32, no_fat_chain: bool, nclusters: u32) -> bool {
    if first < 2 {
        return true; // 无簇 (空文件)
    }
    let mut cl = first;
    let mut i = 0u32;
    let guard_max = unsafe { EXFAT_CLUSTER_COUNT } + 1;
    loop {
        if cl < 2 || i > guard_max {
            return false;
        }
        if !exfat_bitmap_put(cl, false) {
            return false;
        }
        if no_fat_chain {
            i += 1;
            if i >= nclusters {
                break;
            }
            cl += 1;
            continue;
        }
        let next = match exfat_fat_get(cl) {
            Some(v) => v,
            None => return false,
        };
        if !exfat_fat_set(cl, EXFAT_FAT_FREE) {
            return false;
        }
        if next == EXFAT_FAT_FREE || exfat_is_eoc(next) {
            break;
        }
        cl = next;
        i += 1;
    }
    exfat_bitmap_flush()
}

/// 沿 FAT 链走到第 `idx` 个簇 (0 基)。
fn exfat_chain_nth(first: u32, idx: u32) -> Option<u32> {
    if first < 2 {
        return None;
    }
    let mut cl = first;
    let mut i = 0u32;
    while i < idx {
        let next = exfat_fat_get(cl)?;
        if next == EXFAT_FAT_FREE || exfat_is_eoc(next) {
            return None;
        }
        cl = next;
        i += 1;
        if i > unsafe { EXFAT_CLUSTER_COUNT } + 1 {
            return None;
        }
    }
    Some(cl)
}

/// 把链扩展到至少 `needed` 个簇; 返回 (首簇, no_fat_chain, 现有簇数)。
///
/// 既有的连续 (`NoFatChain`) 文件会先补齐簇间 FAT 链接转成链式 —— 之后
/// 只有一种寻簇方式, 读写路径不必再分叉。
fn exfat_grow_to(
    first: u32,
    no_fat_chain: bool,
    have: u32,
    needed: u32,
) -> Option<(u32, bool, u32)> {
    if needed == 0 {
        return Some((0, false, 0));
    }
    let mut first = first;
    let mut have = have;
    if first < 2 || have == 0 {
        first = exfat_alloc_cluster()?;
        have = 1;
    }
    if no_fat_chain {
        let mut i = 0u32;
        while i + 1 < have {
            if !exfat_fat_set(first + i, first + i + 1) {
                return None;
            }
            i += 1;
        }
        if !exfat_fat_set(first + have - 1, EXFAT_FAT_EOC) {
            return None;
        }
    }
    // 从链尾继续分配。
    let mut tail = first;
    let mut n = 1u32;
    loop {
        let next = exfat_fat_get(tail)?;
        if next == EXFAT_FAT_FREE || exfat_is_eoc(next) {
            break;
        }
        tail = next;
        n += 1;
        if n > unsafe { EXFAT_CLUSTER_COUNT } + 1 {
            return None;
        }
    }
    while n < needed {
        let nc = exfat_alloc_cluster()?;
        if !exfat_fat_set(tail, nc) {
            return None;
        }
        tail = nc;
        n += 1;
    }
    Some((first, false, n))
}

/// 当前已分配簇数 (`no_fat_chain` 的连续文件由大小推算)。
fn exfat_entry_clusters(e: &ExfatEntry) -> u32 {
    let cb = unsafe { EXFAT_CLUSTER_BYTES } as u64;
    if cb == 0 {
        return 0;
    }
    if e.no_fat_chain {
        e.size.div_ceil(cb) as u32
    } else {
        let mut cl = e.first_cluster;
        let mut n = 0u32;
        let mut guard = 0u32;
        while cl >= 2 && guard <= unsafe { EXFAT_CLUSTER_COUNT } + 1 {
            n += 1;
            match exfat_fat_get(cl) {
                Some(v) if v != EXFAT_FAT_FREE && !exfat_is_eoc(v) => cl = v,
                _ => break,
            }
            guard += 1;
        }
        n
    }
}

// ---------------------------------------------------------------------------
// fd 表
// ---------------------------------------------------------------------------

fn exfat_fd_alloc(path: &str, is_dir: bool, vol: u64) -> u64 {
    if path.len() > TMP_PATH_MAX {
        return u64::MAX;
    }
    for i in 0..EXFAT_MAX_FD {
        unsafe {
            let s = &mut *core::ptr::addr_of_mut!(EXFAT_FDS).cast::<ExfatFd>().add(i);
            if !s.used {
                s.used = true;
                s.is_dir = is_dir;
                s.path_len = path.len() as u8;
                s.path = [0; TMP_PATH_MAX];
                s.path[..path.len()].copy_from_slice(path.as_bytes());
                s.vol = vol;
                return i as u64;
            }
        }
    }
    u64::MAX
}
/// 查 fd 并把「当前卷寄存器」切到该 fd 绑定的卷 (与路径类请求的 tag 卷编码等价)。
fn exfat_fd_get(fd: u32) -> Option<ExfatFd> {
    if fd as usize >= EXFAT_MAX_FD {
        return None;
    }
    unsafe {
        let s = &*core::ptr::addr_of!(EXFAT_FDS)
            .cast::<ExfatFd>()
            .add(fd as usize);
        if s.used {
            EXFAT_CUR_VOL = s.vol;
            Some(*s)
        } else {
            None
        }
    }
}
fn exfat_fd_free(fd: u32) -> u64 {
    if fd as usize >= EXFAT_MAX_FD {
        return 0;
    }
    unsafe {
        let s = &mut *core::ptr::addr_of_mut!(EXFAT_FDS)
            .cast::<ExfatFd>()
            .add(fd as usize);
        if s.used {
            s.used = false;
            1
        } else {
            0
        }
    }
}

// ---------------------------------------------------------------------------
// 挂载
// ---------------------------------------------------------------------------

/// 扫根目录的**系统项**: 记录分配位图 (0x81) / upcase 表 (0x82) 的簇与长度。
fn exfat_scan_system_entries() -> bool {
    let cb = unsafe { EXFAT_CLUSTER_BYTES } as usize;
    if cb < EXFAT_DIR_ENTRY {
        return false;
    }
    let per = cb / EXFAT_DIR_ENTRY;
    let buf = exfat_clu();
    let mut cl = unsafe { EXFAT_ROOT_CLUSTER };
    let mut guard = 0u32;
    let mut bitmap_found = false;
    let mut upcase_found = false;
    while cl >= 2 && guard <= unsafe { EXFAT_CLUSTER_COUNT } + 1 {
        if !exfat_read_cluster(cl, buf) {
            return false;
        }
        let mut i = 0usize;
        while i < per {
            let e = exfat_at(buf, i * EXFAT_DIR_ENTRY);
            let t = unsafe { *e };
            if t == EXFAT_TYPE_UNUSED {
                return bitmap_found && upcase_found;
            }
            if t & 0x80 != 0 {
                let first = read_u32(exfat_at(e, 20));
                let len = read_u64(exfat_at(e, 24));
                match t {
                    EXFAT_TYPE_BITMAP => {
                        // 位图按需读取 (只缓存一个扇区), 故只校验存在性与覆盖面。
                        if first < 2 || len == 0 || len * 8 < unsafe { EXFAT_CLUSTER_COUNT } as u64
                        {
                            return false;
                        }
                        unsafe {
                            EXFAT_BITMAP_CLUSTER = first;
                            EXFAT_BITMAP_BYTES = len as u32;
                        }
                        bitmap_found = true;
                    }
                    EXFAT_TYPE_UPCASE => {
                        if first < 2 || len == 0 {
                            return false;
                        }
                        unsafe {
                            EXFAT_UPCASE_CLUSTER = first;
                            EXFAT_UPCASE_BYTES = len as u32;
                        }
                        upcase_found = true;
                    }
                    EXFAT_TYPE_LABEL => {} // 卷标可选, 不参与挂载判定
                    _ => {}
                }
            }
            i += 1;
        }
        let next = match exfat_fat_get(cl) {
            Some(v) => v,
            None => return false,
        };
        if next == EXFAT_FAT_FREE || exfat_is_eoc(next) {
            break;
        }
        cl = next;
        guard += 1;
    }
    bitmap_found && upcase_found
}

/// 挂载: 解析引导扇区 + 校验 boot checksum + 读取位图/upcase 表。
///
/// 返回 0 表示成功; 非 0 是失败阶段编号 (供诊断打印定位)。
fn exfat_mount() -> u32 {
    let a = exfat_pg();
    if !exfat_read_sectors(0, 1, a) {
        return 1;
    }
    for (i, &c) in b"EXFAT   ".iter().enumerate() {
        if unsafe { *exfat_at(a, 3 + i) } != c {
            return 2;
        }
    }
    if read_u16(exfat_at(a, 510)) != 0xAA55 {
        return 3;
    }
    // 只支持 512B 扇区: 扇区更大时引导区 / 扇区换算都要另一套逻辑。
    if unsafe { *exfat_at(a, 108) } != 9 {
        return 4;
    }
    let spc_shift = unsafe { *exfat_at(a, 109) };
    if spc_shift == 0 || spc_shift > EXFAT_MAX_SPC_SHIFT {
        // 簇上限 = 集群缓冲页数 (256 KiB); 更大簇的卷当前不支持。
        return 5;
    }
    let num_fats = unsafe { *exfat_at(a, 110) } as u32;
    if num_fats == 0 || num_fats > 2 {
        return 6;
    }
    let fat_off = read_u32(exfat_at(a, 80));
    let fat_len = read_u32(exfat_at(a, 84));
    let heap_off = read_u32(exfat_at(a, 88));
    let clu_count = read_u32(exfat_at(a, 92));
    let root = read_u32(exfat_at(a, 96));
    if fat_len == 0 || clu_count == 0 || root < 2 || root >= clu_count + 2 {
        return 7;
    }
    unsafe {
        EXFAT_SECTORS_PER_CLUSTER = 1u32 << spc_shift;
        EXFAT_CLUSTER_BYTES = EXFAT_SECTORS_PER_CLUSTER * EXFAT_SECTOR_SIZE;
        EXFAT_FAT_OFFSET = fat_off;
        EXFAT_FAT_LENGTH = fat_len;
        EXFAT_HEAP_OFFSET = heap_off;
        EXFAT_CLUSTER_COUNT = clu_count;
        EXFAT_ROOT_CLUSTER = root;
        EXFAT_NUM_FATS = num_fats;
        // 分配游标是**每卷**状态: 换卷时必须复位, 否则会指到新卷的簇范围之外。
        EXFAT_ALLOC_HINT = 2;
    }
    // 簇大小已知, 现在按需分配集群缓冲 (簇字节数 / 页大小 页, 至少一页)。
    let clu_bytes = (1u32 << spc_shift) * EXFAT_SECTOR_SIZE;
    let pages = (clu_bytes as usize).div_ceil(EXFAT_PAGE_SIZE).max(1);
    if !exfat_bufs_init(pages) {
        return 13;
    }
    if !exfat_verify_boot_checksum() {
        return 8;
    }
    if !exfat_scan_system_entries() {
        return 9;
    }
    // 根目录所在簇必须被位图标为占用 —— 同时对「按需位图」做一次端到端校验。
    if !exfat_bitmap_get(root) {
        return 12;
    }
    // 记录「当前几何属于哪个卷」(M1b: 卷切换时据此判断要不要重新解析)。
    unsafe {
        EXFAT_GEO_VOL = EXFAT_CUR_VOL;
    }
    0
}

/// 按簇大小分配并共享 exFAT 的集群缓冲 (逐页 alloc + 同地址 share)。
///
/// 已经分配过的页**不能**再 alloc/share 一次 —— 同地址重复共享会让 block_srv 侧触发
/// 内核 `map_user_page: PageAlreadyMapped` panic。故卷切换 (M1b) 需要更大簇时, 只补
/// 分配「多出来的那几页」。
fn exfat_bufs_init(spc_pages: usize) -> bool {
    if spc_pages == 0 || spc_pages > EXFAT_MAX_CLUSTER_PAGES {
        return false;
    }
    let mut i = unsafe { EXFAT_BUFS_PAGES };
    while i < spc_pages {
        let va = EXFAT_CLU_VADDR + (i * EXFAT_PAGE_SIZE) as u64;
        if sys_alloc_page(va) != 1 || sys_share_page(va, BLOCK_DOMAIN) != 1 {
            return false;
        }
        i += 1;
    }
    unsafe {
        EXFAT_BUFS_PAGES = spc_pages.max(EXFAT_BUFS_PAGES);
    }
    true
}

/// 域 13 — exFAT 服务主循环 (M6a 只读 + M6b 读写)。
fn exfat_main() {
    // 固定缓冲: 单页暂存 + 位图窗口 + upcase 窗口。集群缓冲按实际簇大小
    // 在 `exfat_mount` 里动态分配 (逐页 alloc + 同地址 share 给 block_srv;
    // 漏了共享 NVMe 会直接回「非法字段」而写入静默失败)。
    for b in [exfat_pg(), exfat_bmp(), exfat_upc()] {
        if sys_alloc_page(b as u64) != 1 || sys_share_page(b as u64, BLOCK_DOMAIN) != 1 {
            println("exfat: alloc/share block buffers FAILED");
            return;
        }
    }
    // 认领卷: 第一个 exFAT 签名的卷; 无分区表的整盘镜像即卷 5 (回退值)。
    unsafe {
        EXFAT_VOL = vol_claim(exfat_pg(), 16, VOL_KIND_EXFAT, EXFAT_VOL_FALLBACK);
        EXFAT_CUR_VOL = EXFAT_VOL;
    }
    let stage = exfat_mount();
    if stage != 0 {
        print("exfat: mount FAILED vol=");
        print_u64(unsafe { EXFAT_VOL });
        print(" stage=");
        print_u64(stage as u64);
        println("");
        return;
    }
    print("exfat-dbg: vol=");
    print_u64(unsafe { EXFAT_VOL });
    print(" cluster=");
    print_u64(unsafe { EXFAT_CLUSTER_BYTES } as u64);
    print(" clusters=");
    print_u64(unsafe { EXFAT_CLUSTER_COUNT } as u64);
    print(" root=");
    print_u64(unsafe { EXFAT_ROOT_CLUSTER } as u64);
    print(" bitmap=");
    print_u64(unsafe { EXFAT_BITMAP_BYTES } as u64);
    print(" upcase=");
    print_u64(unsafe { EXFAT_UPCASE_BYTES } as u64);
    println("");

    // M1b: 把**额外**的 exFAT 卷挂到 `/usb<卷号>` (元数据已解析完, `exfat_pg` 可作暂存)。
    mount_extra_volumes(
        exfat_pg(),
        VOL_KIND_EXFAT,
        unsafe { EXFAT_VOL },
        vfs::EXFAT_DOMAIN,
    );

    let mut msg = Message {
        from: 0,
        to: 0,
        tag: 0,
        payload: [0; PAYLOAD_LEN],
    };
    loop {
        sys_recv_msg(&mut msg as *mut Message as *mut u8);
        // 同 fat32_srv: tag 高位带卷编码 (M1b); fd 类请求的卷由 fd 绑定决定。
        let tag = vfs::tag_body(msg.tag);
        let mut vol = vfs::vol_from_enc(vfs::tag_vol(msg.tag), unsafe { EXFAT_VOL });
        if matches!(
            tag,
            vfs::VFS_READ_TAG | vfs::VFS_WRITE_TAG | vfs::VFS_READDIR_TAG | vfs::VFS_TRUNCATE_TAG
        ) {
            if let Some(fd) = exfat_fd_get(read_u32(msg.payload.as_ptr())) {
                vol = fd.vol;
            }
        }
        unsafe {
            EXFAT_CUR_VOL = vol;
        }
        // 卷切换: 各 exFAT 卷的簇大小 / 区域偏移都不同, 必须按该卷重新解析 (会顺带把
        // 几何、集群缓冲、位图 / upcase 视图都切过去)。
        if unsafe { EXFAT_GEO_VOL } != vol && exfat_mount() != 0 {
            sys_reply(u64::MAX);
            continue;
        }
        match tag {
            vfs::VFS_OPEN_TAG => {
                let len = msg
                    .payload
                    .iter()
                    .position(|&b| b == 0)
                    .unwrap_or(PAYLOAD_LEN);
                let path = unsafe { core::str::from_utf8_unchecked(&msg.payload[..len]) };
                let fd = match exfat_resolve(path) {
                    Some(e) => exfat_fd_alloc(path, e.is_dir, vol),
                    None => u64::MAX,
                };
                sys_reply(fd);
            }
            vfs::VFS_READ_TAG => {
                let req: vfs::ReadReq = unsafe {
                    core::ptr::read_unaligned(msg.payload.as_ptr() as *const vfs::ReadReq)
                };
                // exFAT 侧的内部偏移/计数仍是 32 位: 协议 offset 超出 u32 直接失败。
                if req.offset > u32::MAX as u64 {
                    sys_reply(u64::MAX);
                    continue;
                }
                let n = match exfat_fd_get(req.fd) {
                    Some(fd) if !fd.is_dir => {
                        let path = unsafe {
                            core::str::from_utf8_unchecked(&fd.path[..fd.path_len as usize])
                        };
                        match exfat_resolve(path) {
                            Some(e) => exfat_read_file(
                                &e,
                                req.offset as u32,
                                req.count,
                                req.buf as *mut u8,
                            )
                            .unwrap_or(u64::MAX),
                            None => u64::MAX,
                        }
                    }
                    _ => u64::MAX,
                };
                sys_reply(n);
            }
            vfs::VFS_READDIR_TAG => {
                let req: vfs::DirReq = unsafe {
                    core::ptr::read_unaligned(msg.payload.as_ptr() as *const vfs::DirReq)
                };
                let n = match exfat_fd_get(req.fd) {
                    Some(fd) if fd.is_dir => {
                        let path = unsafe {
                            core::str::from_utf8_unchecked(&fd.path[..fd.path_len as usize])
                        };
                        match exfat_resolve(path) {
                            Some(e) => {
                                exfat_readdir(e.first_cluster, req.buf as *mut vfs::DirEntry)
                                    .unwrap_or(u64::MAX)
                            }
                            None => u64::MAX,
                        }
                    }
                    _ => u64::MAX,
                };
                sys_reply(n);
            }
            vfs::VFS_STAT_TAG => {
                let (buf, path) = parse_path_req(msg.payload.as_ptr());
                let n = match exfat_resolve(path) {
                    Some(e) => {
                        let st = vfs::Stat {
                            size: if e.is_dir { 0 } else { e.size },
                            is_dir: u32::from(e.is_dir),
                            mode: if e.is_dir { 0o755 } else { 0o644 },
                            owner: 0,
                            nlink: 1,
                            mtime: e.mtime,
                            ctime: e.mtime,
                            atime: e.mtime,
                        };
                        unsafe {
                            core::ptr::write_unaligned(buf as *mut vfs::Stat, st);
                        }
                        core::mem::size_of::<vfs::Stat>() as u64
                    }
                    None => u64::MAX,
                };
                sys_reply(n);
            }
            vfs::VFS_CLOSE_TAG => {
                let fd = read_u32(msg.payload.as_ptr());
                sys_reply(exfat_fd_free(fd));
            }
            vfs::VFS_CREAT_TAG | vfs::VFS_MKDIR_TAG => {
                let is_dir = tag == vfs::VFS_MKDIR_TAG;
                let len = msg
                    .payload
                    .iter()
                    .position(|&b| b == 0)
                    .unwrap_or(PAYLOAD_LEN);
                let path = unsafe { core::str::from_utf8_unchecked(&msg.payload[..len]) };
                sys_reply(exfat_create(path, is_dir, vol));
            }
            vfs::VFS_UNLINK_TAG | vfs::VFS_RMDIR_TAG => {
                let want_dir = tag == vfs::VFS_RMDIR_TAG;
                let len = msg
                    .payload
                    .iter()
                    .position(|&b| b == 0)
                    .unwrap_or(PAYLOAD_LEN);
                let path = unsafe { core::str::from_utf8_unchecked(&msg.payload[..len]) };
                sys_reply(exfat_remove(path, want_dir));
            }
            vfs::VFS_WRITE_TAG => {
                let req: vfs::WriteReq = unsafe {
                    core::ptr::read_unaligned(msg.payload.as_ptr() as *const vfs::WriteReq)
                };
                if req.offset > u32::MAX as u64 {
                    sys_reply(u64::MAX);
                    continue;
                }
                let n = match exfat_fd_get(req.fd) {
                    Some(fd) if !fd.is_dir => {
                        let mut nm = [0u8; vfs::DIR_LONG_MAX];
                        match exfat_fd_parent(&fd, &mut nm) {
                            Some((parent_first, nlen)) => {
                                let mut p = [0u8; TMP_PATH_MAX];
                                let plen = fd.path_len as usize;
                                p[..plen].copy_from_slice(&fd.path[..plen]);
                                let path = unsafe { core::str::from_utf8_unchecked(&p[..plen]) };
                                match exfat_resolve(path) {
                                    Some(e) => exfat_write_file(
                                        &e,
                                        parent_first,
                                        &nm[..nlen],
                                        req.offset as u32,
                                        req.count,
                                        req.buf as *const u8,
                                    )
                                    .unwrap_or(u64::MAX),
                                    None => u64::MAX,
                                }
                            }
                            None => u64::MAX,
                        }
                    }
                    _ => u64::MAX,
                };
                sys_reply(n);
            }
            vfs::VFS_TRUNCATE_TAG => {
                let req: vfs::TruncateReq = unsafe {
                    core::ptr::read_unaligned(msg.payload.as_ptr() as *const vfs::TruncateReq)
                };
                if req.size > u32::MAX as u64 {
                    sys_reply(u64::MAX);
                    continue;
                }
                let n = match exfat_fd_get(req.fd) {
                    Some(fd) if !fd.is_dir => {
                        let mut nm = [0u8; vfs::DIR_LONG_MAX];
                        match exfat_fd_parent(&fd, &mut nm) {
                            Some((parent_first, nlen)) => {
                                let mut p = [0u8; TMP_PATH_MAX];
                                let plen = fd.path_len as usize;
                                p[..plen].copy_from_slice(&fd.path[..plen]);
                                let path = unsafe { core::str::from_utf8_unchecked(&p[..plen]) };
                                match exfat_resolve(path) {
                                    Some(e) => exfat_truncate(
                                        &e,
                                        parent_first,
                                        &nm[..nlen],
                                        req.size as u32,
                                    )
                                    .unwrap_or(u64::MAX),
                                    None => u64::MAX,
                                }
                            }
                            None => u64::MAX,
                        }
                    }
                    _ => u64::MAX,
                };
                sys_reply(n);
            }
            // 其余未实现 tag (rename/chmod/link 等) 一律拒绝。
            _ => {
                sys_reply(u64::MAX);
            }
        }
    }
}

#[cfg(target_os = "none")]
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    // 用户态 panic: 无法恢复, 直接终止本任务。
    syscall::sys_exit();
}
