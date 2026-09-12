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
    print, print_u64, println, sys_alloc_page, sys_backspace, sys_call,
    sys_call_payload, sys_clear, sys_map_anon, sys_page_fault_reply, sys_port_in16, sys_port_in8,
    sys_port_out8, sys_port_out16, sys_readline, sys_recv, sys_recv_msg, sys_register_irq,
    sys_reply, sys_scroll_down, sys_scroll_up, sys_send, sys_share_page, sys_term_left,
    sys_term_put, sys_term_right, sys_unmap, sys_virt_to_phys,
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
        _ => {}
    }
    syscall::sys_exit();
}

/// 域 0 — 发送者: 持有 SendTo(1) + MapInto(1) 能力, 无 SendTo(2) 能力。
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

    // 同步 IPC call/reply 演示: 调用 echo 服务 (域 3)。
    let _ = sys_call(3, 0xABCD);
}

/// 域 1 — 接收者: 经 IPC 收到通知后, 直接从共享页读取数据, 再解除映射。
fn receiver_main() {
    // 等 sender 通知共享页就绪。
    let _ = sys_recv();

    // 直接读共享页 (零拷贝, 数据未经 IPC 传递)。
    let page = SHARED_PAGE;
    let _ = unsafe { core::slice::from_raw_parts(page as *const u8, 12) };

    // 解除映射: 引用计数 1 -> 0, 真正释放帧。
    if sys_unmap(page) != 1 {
        println("receiver: unmap FAILED");
    }
}

/// IPC 消息 (与内核 `ipc::Message` 布局一致, 56 字节)。
#[repr(C)]
#[allow(dead_code)]
struct Message {
    from: u64,
    to: u64,
    tag: u64,
    payload: [u8; 32],
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
            payload: [0; 32],
        };
        sys_recv_msg(&mut msg as *mut Message as *mut u8);

        // 从 payload 前 24 字节解出缺页信息。
        let info: PageFaultInfo = unsafe {
            core::ptr::read_unaligned(msg.payload.as_ptr() as *const PageFaultInfo)
        };

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
    if c == 0 { None } else { Some(c) }
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
    for _ in 0..10_000 {
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

/// 经 NVMe I/O 队列读 `count` 个扇区到 `buf` (页对齐的用户页)。
///
/// `buf` 为 fat32 共享给本域的缓冲页虚拟地址, 页对齐且已映射; 先经
/// `sys_virt_to_phys` 反查物理地址作为 NVMe DMA 的 PRP1。单次 READ 用单
/// PRP1 页, 最多 8 扇区 (4096 字节, 不跨页); 超过则返回 false (当前 fat32
/// 最多读一簇 8 扇区, 不会触及)。
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
    if count == 0 || count > 8 {
        return false;
    }
    let paddr = sys_virt_to_phys(buf as u64);
    if paddr == 0 {
        return false;
    }
    let mut sqe = Sqe::zero();
    sqe.opcode = opcode;
    sqe.cid = 5;
    sqe.nsid = nsid;
    sqe.prp1 = paddr;
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

    // 7. Identify Namespace (CNS=0, NSID=1) → 扇区总数。
    sqe = Sqe::zero();
    sqe.opcode = OP_IDENTIFY;
    sqe.cid = 2;
    sqe.nsid = 1;
    sqe.prp1 = cfg.data_paddr;
    sqe.cdw10 = 0x00;
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
        println("nvme: Identify Namespace FAILED");
        return;
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

    loop {
        let mut msg = Message {
            from: 0,
            to: 0,
            tag: 0,
            payload: [0; 32],
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

        // 解出设备号与操作码; 设备号映射为 namespace id (nsid = dev + 1)。
        let dev = (req.op >> 8) as u32;
        let opcode_low = (req.op & 0xFF) as u8;
        match opcode_low {
            0 | 1 => {
                let opcode = if opcode_low == 0 { OP_READ } else { OP_WRITE };
                let ok = nvme_rw_sectors(
                    opcode,
                    dev + 1,
                    &cfg,
                    mmio,
                    isq_doorbell,
                    icq_doorbell,
                    req.lba as u32,
                    req.count as u16,
                    req.buf as *mut u8,
                    &mut io_tail,
                    &mut io_head,
                    &mut io_phase,
                );
                sys_reply(if ok { 1 } else { 0 });
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
/// `op` 打包了设备号与操作码: 低 8 位 = 操作码 (0 读 / 1 写), 高位 = 设备号。
/// 设备号在 NVMe 下映射为 namespace id (`nsid = dev + 1`): 0 = FAT32 盘, 1 = MFS 盘。
/// 之所以打包而非新增字段, 是因为 payload 恰好 32 字节, 已无空位。
#[repr(C)]
#[derive(Clone, Copy)]
struct BlockReq {
    op: u64,    // (device << 8) | opcode
    lba: u64,   // 起始扇区号
    count: u64, // 扇区数 (1..=256)
    buf: u64,   // 数据缓冲页虚拟地址
}

/// 经 IPC 请求 block_srv 读 `count` 个扇区到 `buf`。成功返回 true。
///
/// fat32_srv 通过它间接访问块设备, 而非直接触碰 IDE 端口; IDE PIO 逻辑
/// 收拢在 block_srv 内, 符合微内核「驱动服务化」的解耦。
fn block_read(lba: u32, count: u16, buf: *mut u8) -> bool {
    block_read_dev(0, lba, count, buf)
}

/// 经 IPC 请求 block_srv 从 `buf` 写 `count` 个扇区到磁盘。成功返回 true。
fn block_write(lba: u32, count: u16, buf: *mut u8) -> bool {
    block_write_dev(0, lba, count, buf)
}

/// 带设备号的读 (dev 0 = FAT32 盘, dev 1 = MFS 盘)。
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

/// 带设备号的写 (dev 0 = FAT32 盘, dev 1 = MFS 盘)。
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
    fat_size: u32, // 单个 FAT 占用的扇区数
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

/// 扫描 FAT 表找第一个空闲簇 (表项 == 0), 无空闲返回 None。
fn find_free_cluster(bpb: &Fat32Bpb, fat_buf: *mut u8) -> Option<u32> {
    let total = bpb.total_clusters();
    (2..=(total + 1)).find(|&cluster| read_fat_entry(bpb, cluster, fat_buf) == 0)
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
        && !update_dir_entry(bpb, node.dir_cluster, node.entry_offset, new_start, new_size, dir_buf)
    {
        return u64::MAX;
    }
    node.start_cluster = new_start;
    node.file_size = new_size;
    count as u64
}

/// 把目录 `dir_cluster` 的条目以结构化 `vfs::DirEntry` 数组写入 `out`,
/// 返回写入字节数 (= 条目数 × size_of::<DirEntry>()); 失败返回 u64::MAX。
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
                continue; // 已删除
            }
            let attr = unsafe { *entry.add(11) };
            if attr & ATTR_LONG_NAME == ATTR_LONG_NAME {
                continue; // 长文件名项 (短名已够用)
            }
            if attr & ATTR_VOLUME_ID != 0 {
                continue; // 卷标
            }

            let is_dir = attr & ATTR_DIRECTORY != 0;
            // "." / ".." 以 '.' 开头, 跳过。
            if is_dir && first == b'.' {
                continue;
            }

            let file_size = read_u32(unsafe { entry.add(28) });
            let de = unsafe { &mut *dst.add(count) };
            unsafe {
                core::ptr::copy_nonoverlapping(entry, de.name.as_mut_ptr(), 11);
            }
            de.size = file_size;
            de.is_dir = is_dir as u32;
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

/// 在目录 `dir_cluster` 中按短名 `name` 查找条目 (支持跨簇目录)。
/// 命中返回首簇/大小/属性, 未命中或读盘失败返回 None。
fn find_entry(
    bpb: &Fat32Bpb,
    dir_cluster: u32,
    name: &[u8],
    dir_buf: *mut u8,
    fat_buf: *mut u8,
) -> Option<DirEntryInfo> {
    let sn = short_name_from_query(name)?;
    find_entry_sn(bpb, dir_cluster, &sn, dir_buf, fat_buf)
}

/// 在目录 `dir_cluster` 中按 11 字节短名 `sn` 直接查找条目 (支持跨簇目录)。
/// 命中返回首簇/大小/属性, 未命中或读盘失败返回 None。
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
            let cluster_hi = read_u16(unsafe { entry.add(20) }) as u32;
            let cluster_lo = read_u16(unsafe { entry.add(26) }) as u32;
            let start_cluster = (cluster_hi << 16) | cluster_lo;
            let file_size = read_u32(unsafe { entry.add(28) });
            return Some(DirEntryInfo {
                start_cluster,
                file_size,
                attr,
                dir_cluster: cluster,
                entry_offset: (i * 32) as u32,
            });
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
fn dir_is_empty(
    bpb: &Fat32Bpb,
    dir_cluster: u32,
    dir_buf: *mut u8,
    fat_buf: *mut u8,
) -> bool {
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
fn ide_block_main() {
    loop {
        let mut msg = Message {
            from: 0,
            to: 0,
            tag: 0,
            payload: [0; 32],
        };
        sys_recv_msg(&mut msg as *mut Message as *mut u8);

        if msg.tag != BLOCK_REQ_TAG {
            sys_reply(0);
            continue;
        }

        // 从 payload 解出块设备请求。
        let req: BlockReq =
            unsafe { core::ptr::read_unaligned(msg.payload.as_ptr() as *const BlockReq) };

        // IDE 回退路径只有一块盘, 仅支持设备 0。
        let dev = req.op >> 8;
        let opcode = (req.op & 0xFF) as u8;
        if dev != 0 {
            sys_reply(0);
            continue;
        }
        match opcode {
            0 => {
                // read: 读 count (1..=256) 个扇区到 req.buf 指向的共享页。
                let ok = read_sectors(req.lba as u32, req.count as u16, req.buf as *mut u8);
                sys_reply(if ok { 1 } else { 0 });
            }
            1 => {
                // write: 从 req.buf 指向的共享页写 count 个扇区到磁盘。
                let ok = write_sectors(req.lba as u32, req.count as u16, req.buf as *mut u8);
                sys_reply(if ok { 1 } else { 0 });
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
}

static mut FD_TABLE: [Option<OpenNode>; MAX_FD] = [None; MAX_FD];

/// 分配一个空闲 fd 槽位, 返回 fd (0..MAX_FD), 表满返回 u64::MAX。
fn fd_alloc(
    is_dir: bool,
    start_cluster: u32,
    file_size: u32,
    dir_cluster: u32,
    entry_offset: u32,
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
                });
                return i as u64;
            }
        }
    }
    u64::MAX
}

/// 查询 fd 对应的节点描述符。
fn fd_lookup(fd: u32) -> Option<OpenNode> {
    if (fd as usize) >= MAX_FD {
        return None;
    }
    unsafe { *core::ptr::addr_of!(FD_TABLE).cast::<Option<OpenNode>>().add(fd as usize) }
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

/// 域 6 — FAT32 文件服务 (fat32_srv): 经 IPC 请求 block_srv 读扇区,
/// 解析 BPB / FAT / 目录 / 路径, 提供 open/read/readdir/close。
fn fat32_main() {
    // 缓冲页: BPB / 目录簇 / FAT 扇区 / 文件内容。
    // 地址须避开程序镜像 (USER_BASE 起, 随代码增长)、用户栈 (USER_BASE + 4 MiB)
    // 与 USER_DATA_BASE 区 (共享页 / NVMe 配置等, USER_BASE + 8 MiB), 故放到 1MB 偏移处。
    let bpb_buf = 0x0000_0080_0010_2000u64;
    let dir_buf = 0x0000_0080_0010_3000u64;
    let fat_buf = 0x0000_0080_0010_1000u64;
    let file_buf = 0x0000_0080_0010_0000u64;

    if sys_alloc_page(bpb_buf) != 1
        || sys_alloc_page(dir_buf) != 1
        || sys_alloc_page(fat_buf) != 1
        || sys_alloc_page(file_buf) != 1
    {
        println("fat32: alloc buffer FAILED");
        return;
    }

    // 把缓冲页共享给 block_srv (同地址映射), 使其能直接写入读到的扇区数据。
    if sys_share_page(bpb_buf, BLOCK_DOMAIN) != 1
        || sys_share_page(dir_buf, BLOCK_DOMAIN) != 1
        || sys_share_page(fat_buf, BLOCK_DOMAIN) != 1
        || sys_share_page(file_buf, BLOCK_DOMAIN) != 1
    {
        println("fat32: share buffer FAILED");
        return;
    }

    // 经 block_srv 读 LBA 0 并解析 BPB。
    if !block_read(0, 1, bpb_buf as *mut u8) {
        println("fat32: read LBA 0 FAILED");
        return;
    }
    let bpb = Fat32Bpb::parse(bpb_buf as *const u8);

    // 引导签名校验 (offset 510 = 0x55, 511 = 0xAA)。
    let sig = read_u16((bpb_buf + 510) as *const u8);
    if sig != 0xAA55 {
        println("fat32: not a boot sector");
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

    // 服务循环: 经 IPC 提供 open / read / readdir / close (见 vfs.rs 协议)。
    loop {
        let mut msg = Message {
            from: 0,
            to: 0,
            tag: 0,
            payload: [0; 32],
        };
        sys_recv_msg(&mut msg as *mut Message as *mut u8);

        match msg.tag {
            vfs::VFS_OPEN_TAG => {
                let path_len = msg.payload.iter().position(|&b| b == 0).unwrap_or(32);
                let path = unsafe { core::str::from_utf8_unchecked(&msg.payload[..path_len]) };
                let fd = match resolve_open_path(&bpb, path, dir_buf as *mut u8, fat_buf as *mut u8)
                {
                    Some(info) => fd_alloc(
                        info.attr & ATTR_DIRECTORY != 0,
                        info.start_cluster,
                        info.file_size,
                        info.dir_cluster,
                        info.entry_offset,
                    ),
                    None => u64::MAX,
                };
                sys_reply(fd);
            }
            vfs::VFS_READ_TAG => {
                let req: vfs::ReadReq = unsafe {
                    core::ptr::read_unaligned(msg.payload.as_ptr() as *const vfs::ReadReq)
                };
                let n = match fd_lookup(req.fd) {
                    Some(node) if !node.is_dir => read_file_range(
                        &bpb,
                        node.start_cluster,
                        node.file_size,
                        req.offset,
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
                let n = match fd_lookup(req.fd) {
                    Some(mut node) if !node.is_dir => {
                        let written = write_file_range(
                            &bpb,
                            &mut node,
                            req.offset,
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
                let path_len = msg.payload.iter().position(|&b| b == 0).unwrap_or(32);
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
                        ),
                        Some(_) => u64::MAX, // 已存在目录
                        None => {
                            // 创建空文件 (首簇 0, 大小 0, 不预分配簇)。
                            match find_dir_slot(&bpb, parent, dir_buf as *mut u8, fat_buf as *mut u8)
                            {
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
                                    fd_alloc(false, 0, 0, dc, off)
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
                let path_len = msg.payload.iter().position(|&b| b == 0).unwrap_or(32);
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
                                                &bpb, free, 0, &dot, ATTR_DIRECTORY, free, 0,
                                                dir_buf as *mut u8,
                                            ) || !write_dir_entry(
                                                &bpb, free, 32, &dotdot, ATTR_DIRECTORY, parent, 0,
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
                                                            &bpb, dc, off, &sn, ATTR_DIRECTORY, free, 0,
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
                let path_len = msg.payload.iter().position(|&b| b == 0).unwrap_or(32);
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
                let path_len = msg.payload.iter().position(|&b| b == 0).unwrap_or(32);
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
                let path_len = msg.payload.iter().position(|&b| b == 0).unwrap_or(32);
                let path = unsafe { core::str::from_utf8_unchecked(&msg.payload[..path_len]) };
                let n = match resolve_open_path(&bpb, path, dir_buf as *mut u8, fat_buf as *mut u8)
                {
                    Some(info) => {
                        let st = vfs::Stat {
                            size: info.file_size,
                            is_dir: if info.attr & ATTR_DIRECTORY != 0 { 1 } else { 0 },
                        };
                        unsafe {
                            core::ptr::write_unaligned(vfs::RESULT_BUF as *mut vfs::Stat, st);
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
        let slice = core::slice::from_raw_parts(
            core::ptr::addr_of!(BIG_WRITE_BUF) as *const u8,
            4096,
        );
        vfs::write(bfd, 0, slice)
    };
    let bread = vfs::read(bfd, 0, 4096);
    if bwrite != 4096 || bread != 4096 {
        println("app: big write/read FAILED");
    } else {
        let bcontent = unsafe {
            core::slice::from_raw_parts(vfs::RESULT_BUF as *const u8, bread as usize)
        };
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
            println("  ls [path]      list directory (default: current)");
            println("  cat <file>     print file content");
            println("  cd [path]      change directory (default: /)");
            println("  mkdir <path>   create directory");
            println("  touch <file>   create empty file");
            println("  rm <path>      remove file / empty directory");
            println("  clear          clear screen");
            println("  (mounts: / = fat32, /tmp = tmpfs, /mfs = MorionFS)");
        }
        "echo" => println(arg),
        "pwd" => println(st.cwd_str()),
        "ls" => shell_ls(st, if arg.is_empty() { "." } else { arg }),
        "cat" => shell_cat(st, arg),
        "cd" => shell_cd(st, if arg.is_empty() { "/" } else { arg }),
        "mkdir" => shell_mkdir(st, arg),
        "touch" => shell_touch(st, arg),
        "rm" => shell_rm(st, arg),
        "clear" => {
            sys_clear();
        }
        _ => {
            print("shell: unknown command: ");
            println(cmd);
        }
    }
}

/// `ls [path]` — 列出目录条目 (8.3 短名 + 类型 + 文件大小)。
fn shell_ls(st: &ShellState, arg: &str) {
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
        if de.is_dir != 0 {
            print("[DIR]  ");
        } else {
            print("[FILE] ");
        }
        print_83_name(&de.name);
        if de.is_dir == 0 {
            print("  size=");
            print_u64(de.size as u64);
        }
        println("");
    }
    vfs::close(fd);
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
        let content = unsafe {
            core::slice::from_raw_parts(vfs::SHELL_RESULT_BUF as *const u8, n as usize)
        };
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
const MOUNT_MAX: usize = 8;
/// 挂载点前缀最大长度 (含前导 '/', 不含结尾 NUL)。
const MOUNT_PREFIX_MAX: usize = 24;

/// 一条挂载记录: 挂载点前缀 → 文件服务域。
#[derive(Clone, Copy)]
struct MountEntry {
    used: bool,
    /// 前缀长度 (不含结尾 NUL)。
    plen: u8,
    domain: u64,
    prefix: [u8; MOUNT_PREFIX_MAX],
}

const MOUNT_EMPTY: MountEntry = MountEntry {
    used: false,
    plen: 0,
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
/// 拒绝: 空前缀 / 非绝对路径 / 前缀过长 / 域号为 0 / 该前缀已被占用
/// (重复挂载同一前缀需先 `MNTD`, 避免静默改写别人的命名空间)。
fn mount_add(prefix: &str, domain: u64) -> u64 {
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
            e.plen = b.len() as u8;
            e.prefix = [0; MOUNT_PREFIX_MAX];
            e.prefix[..b.len()].copy_from_slice(b);
            return (i + 1) as u64;
        }
    }
    u64::MAX
}

/// 自动分配挂载点: 取最小的未被占用的 `/mnt<N>`。
/// 成功返回挂载槽位号 (1 起)。
fn mount_auto(domain: u64) -> u64 {
    let mut buf = [0u8; MOUNT_PREFIX_MAX];
    buf[..4].copy_from_slice(b"/mnt");
    for n in 0..MOUNT_MAX {
        buf[4] = b'0' + n as u8;
        let name = unsafe { core::str::from_utf8_unchecked(&buf[..5]) };
        if mount_find(name).is_none() {
            return mount_add(name, domain);
        }
    }
    u64::MAX
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
    mount_add("/", vfs::FAT32_DOMAIN);
    mount_add("/tmp", vfs::TMPFS_DOMAIN);
    mount_add("/mfs", vfs::MFS_DOMAIN);
}

/// 在挂载表中查最长匹配前缀, 返回 (服务域, 前缀长度)。
fn mount_resolve(path: &str) -> Option<(u64, usize)> {
    let mut best: Option<(u64, usize)> = None;
    for i in 0..MOUNT_MAX {
        let e = mount_at(i);
        if !e.used {
            continue;
        }
        if let Some(len) = mount_prefix_match(path, mount_prefix_of(e)) {
            if best.is_none_or(|(_, bl)| len > bl) {
                best = Some((e.domain, len));
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
        payload: [0; 32],
    };
    loop {
        sys_recv_msg(&mut msg as *mut Message as *mut u8);
        match msg.tag {
            vfs::VFS_LOOKUP_TAG => {
                let len = msg.payload.iter().position(|&b| b == 0).unwrap_or(32);
                let path = unsafe { core::str::from_utf8_unchecked(&msg.payload[..len]) };
                let r = match mount_resolve(path) {
                    Some((domain, prefix_len)) => (domain << 32) | prefix_len as u64,
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
                    mount_add(prefix, req.domain)
                };
                sys_reply(r);
            }
            vfs::VFS_UMOUNT_TAG => {
                let len = msg.payload.iter().position(|&b| b == 0).unwrap_or(32);
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
const TMP_PATH_MAX: usize = 64;
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

const TMP_FD_EMPTY: TmpFd = TmpFd { used: false, node: 0 };

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
        let slot = &*core::ptr::addr_of!(TMP_FDS).cast::<TmpFd>().add(fd as usize);
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
        let slot = &mut *core::ptr::addr_of_mut!(TMP_FDS).cast::<TmpFd>().add(fd as usize);
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
        payload: [0; 32],
    };
    let mut canon = [0u8; TMP_PATH_MAX];
    loop {
        sys_recv_msg(&mut msg as *mut Message as *mut u8);
        match msg.tag {
            vfs::VFS_OPEN_TAG => {
                let len = msg.payload.iter().position(|&b| b == 0).unwrap_or(32);
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
                let n = match tmp_fd_node(req.fd) {
                    Some(idx) => {
                        let nd = tmp_node_at(idx);
                        if nd.is_dir {
                            u64::MAX
                        } else if req.offset >= nd.size {
                            0
                        } else {
                            let cnt = (req.count).min(nd.size - req.offset) as usize;
                            unsafe {
                                core::ptr::copy_nonoverlapping(
                                    tmp_data_ptr(nd.data_off as usize + req.offset as usize),
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
                let n = match tmp_fd_node(req.fd) {
                    Some(idx) if !tmp_node_at(idx).is_dir => {
                        let end = req.offset as usize + req.count as usize;
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
                                    tmp_data_ptr(off + req.offset as usize),
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
                            let mut e = vfs::DirEntry {
                                name: [0u8; 11],
                                size: if child.is_dir { 0 } else { child.size },
                                is_dir: if child.is_dir { 1 } else { 0 },
                            };
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
                let len = msg.payload.iter().position(|&b| b == 0).unwrap_or(32);
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
                let len = msg.payload.iter().position(|&b| b == 0).unwrap_or(32);
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
                let len = msg.payload.iter().position(|&b| b == 0).unwrap_or(32);
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
                let len = msg.payload.iter().position(|&b| b == 0).unwrap_or(32);
                let path = unsafe { core::str::from_utf8_unchecked(&msg.payload[..len]) };
                let r = match tmp_normalize(path, &mut canon) {
                    Some(n) if n > 1 => match tmp_find(&canon[..n]) {
                        Some(idx)
                            if tmp_node_at(idx).is_dir && tmp_dir_empty(&canon[..n]) =>
                        {
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
                let len = msg.payload.iter().position(|&b| b == 0).unwrap_or(32);
                let path = unsafe { core::str::from_utf8_unchecked(&msg.payload[..len]) };
                let n = match tmp_normalize(path, &mut canon) {
                    Some(n) => match tmp_find(&canon[..n]) {
                        Some(idx) => {
                            let nd = tmp_node_at(idx);
                            let st = vfs::Stat {
                                size: nd.size,
                                is_dir: if nd.is_dir { 1 } else { 0 },
                            };
                            unsafe {
                                core::ptr::write_unaligned(vfs::RESULT_BUF as *mut vfs::Stat, st);
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
//   文件 payload: size u32 | nblocks u32 | blocks[1020] u32
//   数据 payload: 文件字节 (每块最多 4088 字节)
//
// 名称沿用 8.3 短名 (转大写), 与 libvfs 的 DirEntry ABI 及 shell 显示一致。

const MFS_BLOCK: usize = 4096;
const MFS_SECTORS_PER_BLOCK: u16 = (MFS_BLOCK / 512) as u16;
const MFS_HDR: usize = 8;
const MFS_PAYLOAD: usize = MFS_BLOCK - MFS_HDR;

/// 块设备号: 0 = FAT32 盘 (nsid 1), 1 = MFS 盘 (nsid 2)。
const MFS_DEV: u64 = 1;
/// 默认总块数 (16 MiB / 4 KiB), 须与 Makefile 的 `MFS_MIB=16` 对应。
/// 仅用于首次格式化; 之后以超级块记录的值为准。
const MFS_DEFAULT_TOTAL_BLOCKS: u32 = 4096;
/// 超级块副本数 (块 0 / 块 1)。
const MFS_SB_COPIES: u32 = 2;
/// 超级块内快照表容量。
const MFS_MAX_SNAP: usize = 8;

const MFS_MAGIC_SUPER: u32 = 0x4D46_5331; // "MFS1"
const MFS_MAGIC_DIR: u32 = 0x4D46_4449; // "MFDI"
const MFS_MAGIC_FILE: u32 = 0x4D46_464C; // "MFFL"
const MFS_MAGIC_DATA: u32 = 0x4D46_4441; // "MFDA"

// 目录块 payload 布局。
const MFS_DIR_ENTRY: usize = 24;
const MFS_MAX_ENTRIES: usize = (MFS_PAYLOAD - 8) / MFS_DIR_ENTRY;
const MFS_NAME_MAX: usize = 16;
// 文件块 payload 布局。
const MFS_FILE_MAX_BLOCKS: usize = (MFS_PAYLOAD - 8) / 4;
/// 单个数据块可存放的文件字节数。
const MFS_DATA_CAP: usize = MFS_PAYLOAD;
// 路径解析链最大深度 / 打开文件上限。
const MFS_MAX_DEPTH: usize = 12;
const MFS_MAX_FD: usize = 16;

// 节点类型 (目录项 type 字段)。
const MFS_TYPE_FILE: u32 = 1;
const MFS_TYPE_DIR: u32 = 2;

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

// 超级块/分配状态 (内存镜像, 与磁盘副本同步)。
static mut MFS_ROOT: u32 = 0;
static mut MFS_ALLOC_NEXT: u32 = 0;
static mut MFS_TOTAL_BLOCKS: u32 = 0;
static mut MFS_GEN: u64 = 0;
static mut MFS_SB_COPY: u32 = 0;
static mut MFS_SNAP_COUNT: usize = 0;

#[derive(Clone, Copy)]
struct MfsSnap {
    gen: u64,
    root: u32,
    alloc_next: u32,
}
impl MfsSnap {
    const EMPTY: MfsSnap = MfsSnap {
        gen: 0,
        root: 0,
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

/// 路径解析链: 记录从根到叶每一级的 (父目录块, 目录项下标)。
#[derive(Clone, Copy)]
struct MfsFrame {
    dir_block: u32,
    entry_index: u32,
}
const MFS_FRAME_EMPTY: MfsFrame = MfsFrame {
    dir_block: 0,
    entry_index: 0,
};
static mut MFS_CHAIN: [MfsFrame; MFS_MAX_DEPTH] = [MFS_FRAME_EMPTY; MFS_MAX_DEPTH];
static mut MFS_CHAIN_LEN: usize = 0;

fn mfs_set_frame(i: usize, f: MfsFrame) {
    unsafe {
        *core::ptr::addr_of_mut!(MFS_CHAIN).cast::<MfsFrame>().add(i) = f;
    }
}
fn mfs_frame(i: usize) -> MfsFrame {
    unsafe { *core::ptr::addr_of!(MFS_CHAIN).cast::<MfsFrame>().add(i) }
}

/// 打开文件描述符 (按路径而非 inode 记录: COW 后 inode 块会变, 每次操作重新解析
/// 路径即可始终指向最新版本, 避免句柄失效)。
#[derive(Clone, Copy)]
struct MfsFd {
    used: bool,
    is_dir: bool,
    path_len: u8,
    path: [u8; TMP_PATH_MAX],
}
const MFS_FD_EMPTY: MfsFd = MfsFd {
    used: false,
    is_dir: false,
    path_len: 0,
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
        MFS_DEV,
        block_no * MFS_SECTORS_PER_BLOCK as u32,
        MFS_SECTORS_PER_BLOCK,
        dst,
    )
}
fn mfs_write_blk(block_no: u32, src: *const u8) -> bool {
    block_write_dev(
        MFS_DEV,
        block_no * MFS_SECTORS_PER_BLOCK as u32,
        MFS_SECTORS_PER_BLOCK,
        src as *mut u8,
    )
}

/// 追加分配一个新块 (只增不回收), 空间耗尽返回 None。
fn mfs_alloc_block() -> Option<u32> {
    let n = unsafe { MFS_ALLOC_NEXT };
    if n >= unsafe { MFS_TOTAL_BLOCKS } {
        return None;
    }
    unsafe {
        MFS_ALLOC_NEXT = n + 1;
    }
    Some(n)
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
// 超级块 (A/B 双副本)
// ---------------------------------------------------------------------------

fn mfs_build_super(buf: *mut u8) {
    zero_bytes(buf, MFS_BLOCK);
    let p = MFS_HDR;
    write_u32(mfs_atm(buf, p), 1); // version
    write_u32(mfs_atm(buf, p + 4), MFS_BLOCK as u32); // block_size
    write_u32(mfs_atm(buf, p + 8), unsafe { MFS_TOTAL_BLOCKS });
    write_u32(mfs_atm(buf, p + 12), unsafe { MFS_ROOT });
    write_u32(mfs_atm(buf, p + 16), unsafe { MFS_ALLOC_NEXT });
    write_u32(mfs_atm(buf, p + 20), unsafe { MFS_SNAP_COUNT } as u32);
    write_u64(mfs_atm(buf, p + 24), unsafe { MFS_GEN });
    for i in 0..unsafe { MFS_SNAP_COUNT } {
        let s = mfs_snap(i);
        let off = p + 32 + i * 16;
        write_u64(mfs_atm(buf, off), s.gen);
        write_u32(mfs_atm(buf, off + 8), s.root);
        write_u32(mfs_atm(buf, off + 12), s.alloc_next);
    }
    mfs_seal(buf, MFS_MAGIC_SUPER);
}

/// 代际 +1 后写入另一份超级块副本 (交替, 保留上一代)。
fn mfs_write_super() -> bool {
    unsafe {
        MFS_GEN += 1;
    }
    let buf = mfs_s();
    mfs_build_super(buf);
    let copy = (MFS_SB_COPIES - 1) - unsafe { MFS_SB_COPY };
    if !mfs_write_blk(copy, buf) {
        return false;
    }
    unsafe {
        MFS_SB_COPY = copy;
    }
    true
}

/// 挂载: 取两份超级块中 CRC 有效且代际更高者; 都无效则格式化。
fn mfs_mount_or_format() -> bool {
    let mut found = false;
    let mut best_gen = 0u64;
    for copy in 0..MFS_SB_COPIES {
        let buf = mfs_a();
        if !mfs_read_blk(copy, buf) || !mfs_ok(buf, MFS_MAGIC_SUPER) {
            continue;
        }
        let p = MFS_HDR;
        let gen = read_u64(mfs_at(buf, p + 24));
        if found && gen <= best_gen {
            continue;
        }
        let total = read_u32(mfs_at(buf, p + 8));
        let root = read_u32(mfs_at(buf, p + 12));
        if total == 0 || root == 0 {
            continue;
        }
        let alloc = read_u32(mfs_at(buf, p + 16));
        let scount = (read_u32(mfs_at(buf, p + 20)) as usize).min(MFS_MAX_SNAP);
        unsafe {
            MFS_TOTAL_BLOCKS = total;
            MFS_ROOT = root;
            MFS_ALLOC_NEXT = alloc;
            MFS_GEN = gen;
            MFS_SB_COPY = copy;
            MFS_SNAP_COUNT = scount;
        }
        for i in 0..scount {
            let off = p + 32 + i * 16;
            mfs_set_snap(
                i,
                MfsSnap {
                    gen: read_u64(mfs_at(buf, off)),
                    root: read_u32(mfs_at(buf, off + 8)),
                    alloc_next: read_u32(mfs_at(buf, off + 12)),
                },
            );
        }
        best_gen = gen;
        found = true;
    }
    if !found {
        return mfs_format();
    }
    true
}

/// 首次格式化: 空白根目录 + 写超级块副本 0。
fn mfs_format() -> bool {
    unsafe {
        MFS_TOTAL_BLOCKS = MFS_DEFAULT_TOTAL_BLOCKS;
        MFS_ALLOC_NEXT = 2; // 块 0/1 是超级块副本
        MFS_GEN = 0;
        MFS_SNAP_COUNT = 0;
    }
    let buf = mfs_a();
    zero_bytes(buf, MFS_BLOCK);
    write_u32(mfs_atm(buf, MFS_HDR), 0); // nentries = 0
    write_u32(mfs_atm(buf, MFS_HDR + 4), 0); // pad
    let root = match mfs_commit(buf, MFS_MAGIC_DIR) {
        Some(b) => b,
        None => return false,
    };
    unsafe {
        MFS_ROOT = root;
        MFS_SB_COPY = 1; // write_super 写另一份 -> 副本 0
    }
    mfs_write_super()
}

// ---------------------------------------------------------------------------
// 目录 / 文件节点访问
// ---------------------------------------------------------------------------

fn mfs_dir_count(buf: *const u8) -> u32 {
    read_u32(mfs_at(buf, MFS_HDR))
}
fn mfs_dir_set_count(buf: *mut u8, n: u32) {
    write_u32(mfs_atm(buf, MFS_HDR), n);
}
fn mfs_entry(buf: *mut u8, i: usize) -> *mut u8 {
    mfs_atm(buf, MFS_HDR + 8 + i * MFS_DIR_ENTRY)
}
fn mfs_entry_block(buf: *mut u8, i: usize) -> u32 {
    read_u32(mfs_at(mfs_entry(buf, i), 16))
}
fn mfs_entry_set_block(buf: *mut u8, i: usize, b: u32) {
    write_u32(mfs_atm(mfs_entry(buf, i), 16), b);
}
fn mfs_entry_type(buf: *mut u8, i: usize) -> u32 {
    read_u32(mfs_at(mfs_entry(buf, i), 20))
}
fn mfs_entry_set_type(buf: *mut u8, i: usize, t: u32) {
    write_u32(mfs_atm(mfs_entry(buf, i), 20), t);
}

/// 比较目录项名字与规范化分量 (分量不以 NUL 结尾, 项名以 NUL 结尾)。
fn mfs_name_eq(name: *const u8, comp: &[u8]) -> bool {
    if comp.is_empty() || comp.len() >= MFS_NAME_MAX {
        return false;
    }
    for (i, &c) in comp.iter().enumerate() {
        if unsafe { *name.add(i) } != c {
            return false;
        }
    }
    (unsafe { *name.add(comp.len()) }) == 0
}
fn mfs_set_name(name: *mut u8, comp: &[u8]) {
    for i in 0..MFS_NAME_MAX {
        unsafe {
            *name.add(i) = 0;
        }
    }
    for (i, &c) in comp.iter().enumerate().take(MFS_NAME_MAX - 1) {
        unsafe {
            *name.add(i) = c;
        }
    }
}
/// 在目录块中查找分量, 返回 (项下标, 子块号, 类型)。
fn mfs_dir_find(buf: *mut u8, comp: &[u8]) -> Option<(usize, u32, u32)> {
    let n = mfs_dir_count(buf) as usize;
    for i in 0..n {
        let e = mfs_entry(buf, i);
        if mfs_name_eq(e, comp) {
            return Some((i, mfs_entry_block(buf, i), mfs_entry_type(buf, i)));
        }
    }
    None
}
/// 追加目录项, 返回下标; 满则 None。
fn mfs_dir_add(buf: *mut u8, comp: &[u8], block: u32, typ: u32) -> Option<usize> {
    let n = mfs_dir_count(buf) as usize;
    if n >= MFS_MAX_ENTRIES {
        return None;
    }
    let e = mfs_entry(buf, n);
    mfs_set_name(e, comp);
    write_u32(mfs_atm(e, 16), block);
    write_u32(mfs_atm(e, 20), typ);
    mfs_dir_set_count(buf, (n + 1) as u32);
    Some(n)
}
/// 删除目录项 (前移压缩)。
fn mfs_dir_remove(buf: *mut u8, idx: usize) -> bool {
    let n = mfs_dir_count(buf) as usize;
    if idx >= n {
        return false;
    }
    for i in idx..n - 1 {
        let src = mfs_entry(buf, i + 1);
        let dst = mfs_entry(buf, i);
        unsafe {
            core::ptr::copy_nonoverlapping(src, dst, MFS_DIR_ENTRY);
        }
    }
    mfs_dir_set_count(buf, (n - 1) as u32);
    true
}

fn mfs_file_size(buf: *const u8) -> u32 {
    read_u32(mfs_at(buf, MFS_HDR))
}
fn mfs_file_set_size(buf: *mut u8, v: u32) {
    write_u32(mfs_atm(buf, MFS_HDR), v);
}
fn mfs_file_nblocks(buf: *const u8) -> u32 {
    read_u32(mfs_at(buf, MFS_HDR + 4))
}
fn mfs_file_set_nblocks(buf: *mut u8, v: u32) {
    write_u32(mfs_atm(buf, MFS_HDR + 4), v);
}
fn mfs_file_block(buf: *const u8, i: usize) -> u32 {
    read_u32(mfs_at(buf, MFS_HDR + 8 + i * 4))
}
fn mfs_file_set_block(buf: *mut u8, i: usize, b: u32) {
    write_u32(mfs_atm(buf, MFS_HDR + 8 + i * 4), b);
}

/// 把存储名 (dotted 大写) 转成 11 字节 FAT 8.3 短名 (主名 8 + 扩展 3, 空格填充)。
fn mfs_name_to_fat(name: *const u8, out: &mut [u8; 11]) {
    *out = [b' '; 11];
    let mut seg = [0u8; MFS_NAME_MAX];
    let mut len = 0usize;
    for (i, slot) in seg.iter_mut().enumerate() {
        let c = unsafe { *name.add(i) };
        if c == 0 {
            break;
        }
        *slot = c;
        len = i + 1;
    }
    let mut dot = len;
    for (i, &c) in seg[..len].iter().enumerate() {
        if c == b'.' {
            dot = i;
            break;
        }
    }
    let base = &seg[..dot];
    let ext = if dot < len { &seg[dot + 1..len] } else { &seg[len..len] };
    let bn = base.len().min(8);
    out[..bn].copy_from_slice(&base[..bn]);
    let en = ext.len().min(3);
    out[8..8 + en].copy_from_slice(&ext[..en]);
}

// ---------------------------------------------------------------------------
// 路径解析 + COW 上溯
// ---------------------------------------------------------------------------

/// 解析规范化绝对路径, 填充 `MFS_CHAIN`, 返回叶子节点块号。
fn mfs_resolve(canon: &[u8]) -> Option<u32> {
    unsafe {
        MFS_CHAIN_LEN = 0;
    }
    let mut cur = unsafe { MFS_ROOT };
    if canon == b"/" {
        return Some(cur);
    }
    let mut i = 1usize;
    while i < canon.len() {
        let start = i;
        while i < canon.len() && canon[i] != b'/' {
            i += 1;
        }
        let comp = &canon[start..i];
        if i < canon.len() {
            i += 1;
        }
        let buf = mfs_a();
        if !mfs_read_blk(cur, buf) || !mfs_ok(buf, MFS_MAGIC_DIR) {
            return None;
        }
        let (idx, child, _t) = mfs_dir_find(buf, comp)?;
        let cl = unsafe { MFS_CHAIN_LEN };
        if cl >= MFS_MAX_DEPTH {
            return None;
        }
        mfs_set_frame(
            cl,
            MfsFrame {
                dir_block: cur,
                entry_index: idx as u32,
            },
        );
        unsafe {
            MFS_CHAIN_LEN = cl + 1;
        }
        cur = child;
    }
    Some(cur)
}

/// 用 `new_block` 替换深度为 `depth` 的节点 (0 = 根), 沿链复制父目录直到根,
/// 最后写超级块。这是 COW 的核心: 任何修改都不会覆盖旧块。
fn mfs_propagate(depth: usize, new_block: u32) -> bool {
    let mut nc = new_block;
    let mut i = depth;
    while i > 0 {
        i -= 1;
        let f = mfs_frame(i);
        let buf = mfs_b();
        if !mfs_read_blk(f.dir_block, buf) || !mfs_ok(buf, MFS_MAGIC_DIR) {
            return false;
        }
        mfs_entry_set_block(buf, f.entry_index as usize, nc);
        nc = match mfs_commit(buf, MFS_MAGIC_DIR) {
            Some(b) => b,
            None => return false,
        };
    }
    unsafe {
        MFS_ROOT = nc;
    }
    mfs_write_super()
}

/// 读取文件节点 `block` 的 [offset, offset+count) 区间到 `dst`, 返回读取字节数。
fn mfs_read_file(block: u32, offset: u32, count: u32, dst: *mut u8) -> u64 {
    let a = mfs_a();
    if !mfs_read_blk(block, a) || !mfs_ok(a, MFS_MAGIC_FILE) {
        return u64::MAX;
    }
    let size = mfs_file_size(a);
    if offset >= size {
        return 0;
    }
    let end = core::cmp::min(offset as u64 + count as u64, size as u64) as u32;
    let n = end - offset;
    let c = mfs_c();
    let mut done = 0u32;
    while done < n {
        let pos = offset + done;
        let bi = (pos as usize) / MFS_DATA_CAP;
        let boff = (pos as usize) % MFS_DATA_CAP;
        let chunk = core::cmp::min(MFS_DATA_CAP - boff, (n - done) as usize);
        if bi >= mfs_file_nblocks(a) as usize {
            break;
        }
        let db = mfs_file_block(a, bi);
        if db == 0 {
            break;
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

/// 写文件节点 `block` 的 [offset, offset+count) 区间, COW 数据块 + 文件节点,
/// 再上溯到根。返回写入字节数, 失败 `u64::MAX`。
fn mfs_write_file(block: u32, depth: usize, offset: u32, count: u32, src: *const u8) -> u64 {
    let a = mfs_a();
    if !mfs_read_blk(block, a) || !mfs_ok(a, MFS_MAGIC_FILE) {
        return u64::MAX;
    }
    let old_size = mfs_file_size(a);
    let mut nblocks = mfs_file_nblocks(a) as usize;
    let c = mfs_c();
    let end = offset + count;
    let mut done = 0u32;
    while done < count {
        let pos = offset + done;
        let bi = (pos as usize) / MFS_DATA_CAP;
        let boff = (pos as usize) % MFS_DATA_CAP;
        let chunk = core::cmp::min(MFS_DATA_CAP - boff, (count - done) as usize);
        if bi >= MFS_FILE_MAX_BLOCKS {
            return u64::MAX;
        }
        // 读旧数据块 (存在则复制, 否则清零)。
        let old_db = if bi < nblocks { mfs_file_block(a, bi) } else { 0 };
        if old_db != 0 {
            if !mfs_read_blk(old_db, c) || !mfs_ok(c, MFS_MAGIC_DATA) {
                return u64::MAX;
            }
        } else {
            zero_bytes(c, MFS_BLOCK);
        }
        unsafe {
            core::ptr::copy_nonoverlapping(src.add(done as usize), mfs_atm(c, MFS_HDR + boff), chunk);
        }
        let new_db = match mfs_commit(c, MFS_MAGIC_DATA) {
            Some(b) => b,
            None => return u64::MAX,
        };
        // 追加块时补齐中间空洞 (置 0)。
        if bi >= nblocks {
            for k in nblocks..=bi {
                mfs_file_set_block(a, k, 0);
            }
            nblocks = bi + 1;
            mfs_file_set_nblocks(a, nblocks as u32);
        }
        mfs_file_set_block(a, bi, new_db);
        done += chunk as u32;
    }
    if end > old_size {
        mfs_file_set_size(a, end);
    }
    let new_inode = match mfs_commit(a, MFS_MAGIC_FILE) {
        Some(b) => b,
        None => return u64::MAX,
    };
    if !mfs_propagate(depth, new_inode) {
        return u64::MAX;
    }
    count as u64
}

/// 列出目录节点 `block` 的条目, 写入 `out` (DirEntry 数组), 返回写入字节数。
fn mfs_readdir(block: u32, out: *mut vfs::DirEntry) -> u64 {
    let a = mfs_a();
    if !mfs_read_blk(block, a) || !mfs_ok(a, MFS_MAGIC_DIR) {
        return u64::MAX;
    }
    let n = mfs_dir_count(a) as usize;
    let mut count = 0usize;
    for i in 0..n {
        let t = mfs_entry_type(a, i);
        let mut de = vfs::DirEntry {
            name: [0u8; 11],
            size: 0,
            is_dir: if t == MFS_TYPE_DIR { 1 } else { 0 },
        };
        let e = mfs_entry(a, i);
        mfs_name_to_fat(e, &mut de.name);
        if t == MFS_TYPE_FILE {
            let child = mfs_entry_block(a, i);
            let b = mfs_b();
            if mfs_read_blk(child, b) && mfs_ok(b, MFS_MAGIC_FILE) {
                de.size = mfs_file_size(b);
            }
        }
        unsafe {
            core::ptr::write_unaligned(out.add(count), de);
        }
        count += 1;
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
        let s = &*core::ptr::addr_of!(MFS_FDS).cast::<MfsFd>().add(fd as usize);
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
        let s = &mut *core::ptr::addr_of_mut!(MFS_FDS).cast::<MfsFd>().add(fd as usize);
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

/// 域 11 — MFS 服务: 处理 VFS 协议 + MFS 快照操作。
fn mfs_main() {
    // 先为 4 个块缓冲分配页 (固定虚拟地址, 避开程序镜像 / 用户栈 / 数据区),
    // 再把它们共享给 block_srv (它按 req.buf 写入读到的扇区), 之后才能做块 I/O。
    if sys_alloc_page(mfs_a() as u64) != 1
        || sys_alloc_page(mfs_b() as u64) != 1
        || sys_alloc_page(mfs_c() as u64) != 1
        || sys_alloc_page(mfs_s() as u64) != 1
    {
        println("mfs: alloc block buffers FAILED");
        return;
    }
    if sys_share_page(mfs_a() as u64, BLOCK_DOMAIN) != 1
        || sys_share_page(mfs_b() as u64, BLOCK_DOMAIN) != 1
        || sys_share_page(mfs_c() as u64, BLOCK_DOMAIN) != 1
        || sys_share_page(mfs_s() as u64, BLOCK_DOMAIN) != 1
    {
        println("mfs: share block buffers FAILED");
        return;
    }
    if !mfs_mount_or_format() {
        println("mfs: mount/format FAILED");
        return;
    }

    let mut msg = Message {
        from: 0,
        to: 0,
        tag: 0,
        payload: [0; 32],
    };
    let mut canon = [0u8; TMP_PATH_MAX];
    loop {
        sys_recv_msg(&mut msg as *mut Message as *mut u8);
        match msg.tag {
            vfs::VFS_OPEN_TAG => {
                let len = msg.payload.iter().position(|&b| b == 0).unwrap_or(32);
                let path = unsafe { core::str::from_utf8_unchecked(&msg.payload[..len]) };
                let fd = match tmp_normalize(path, &mut canon) {
                    Some(n) => match mfs_resolve(&canon[..n]) {
                        Some(block) => {
                            let is_dir = mfs_is_dir(block);
                            mfs_fd_alloc(&canon[..n], is_dir)
                        }
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
                let n = match mfs_fd_get(req.fd) {
                    Some(fd) if !fd.is_dir => {
                        let p = &fd.path[..fd.path_len as usize];
                        match mfs_resolve(p) {
                            Some(block) => mfs_read_file(block, req.offset, req.count, req.buf as *mut u8),
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
                            Some(block) => {
                                let depth = unsafe { MFS_CHAIN_LEN };
                                mfs_write_file(block, depth, req.offset, req.count, req.buf as *const u8)
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
                            Some(block) => mfs_readdir(block, req.buf as *mut vfs::DirEntry),
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
                let is_dir = msg.tag == vfs::VFS_MKDIR_TAG;
                let len = msg.payload.iter().position(|&b| b == 0).unwrap_or(32);
                let path = unsafe { core::str::from_utf8_unchecked(&msg.payload[..len]) };
                let fd = mfs_create(path, is_dir);
                sys_reply(fd);
            }
            vfs::VFS_UNLINK_TAG | vfs::VFS_RMDIR_TAG => {
                let want_dir = msg.tag == vfs::VFS_RMDIR_TAG;
                let len = msg.payload.iter().position(|&b| b == 0).unwrap_or(32);
                let path = unsafe { core::str::from_utf8_unchecked(&msg.payload[..len]) };
                sys_reply(mfs_remove(path, want_dir));
            }
            vfs::VFS_STAT_TAG => {
                let len = msg.payload.iter().position(|&b| b == 0).unwrap_or(32);
                let path = unsafe { core::str::from_utf8_unchecked(&msg.payload[..len]) };
                let n = match tmp_normalize(path, &mut canon) {
                    Some(nn) => match mfs_resolve(&canon[..nn]) {
                        Some(block) => {
                            let a = mfs_a();
                            if !mfs_read_blk(block, a) {
                                u64::MAX
                            } else if mfs_ok(a, MFS_MAGIC_FILE) {
                                let st = vfs::Stat {
                                    size: mfs_file_size(a),
                                    is_dir: 0,
                                };
                                unsafe {
                                    core::ptr::write_unaligned(vfs::RESULT_BUF as *mut vfs::Stat, st);
                                }
                                core::mem::size_of::<vfs::Stat>() as u64
                            } else if mfs_ok(a, MFS_MAGIC_DIR) {
                                let st = vfs::Stat { size: 0, is_dir: 1 };
                                unsafe {
                                    core::ptr::write_unaligned(vfs::RESULT_BUF as *mut vfs::Stat, st);
                                }
                                core::mem::size_of::<vfs::Stat>() as u64
                            } else {
                                u64::MAX
                            }
                        }
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
                        root: unsafe { MFS_ROOT },
                        alloc_next: unsafe { MFS_ALLOC_NEXT },
                    },
                );
                unsafe {
                    MFS_SNAP_COUNT = idx + 1;
                }
                let r = if mfs_write_super() { idx as u64 } else { u64::MAX };
                sys_reply(r);
            }
            vfs::MFS_SNAPLIST_TAG => {
                let buf = read_u64(msg.payload.as_ptr()) as *mut u8;
                let n = unsafe { MFS_SNAP_COUNT };
                for i in 0..n {
                    let s = mfs_snap(i);
                    let dst = unsafe { buf.add(i * 16) };
                    write_u64(dst, s.gen);
                    write_u32(unsafe { dst.add(8) }, s.root);
                    write_u32(unsafe { dst.add(12) }, s.alloc_next);
                }
                sys_reply((n * 16) as u64);
            }
            vfs::MFS_SNAPRESTORE_TAG => {
                let idx = read_u32(msg.payload.as_ptr()) as usize;
                let r = if idx < unsafe { MFS_SNAP_COUNT } {
                    let s = mfs_snap(idx);
                    unsafe {
                        MFS_ROOT = s.root;
                    }
                    if mfs_write_super() {
                        1
                    } else {
                        u64::MAX
                    }
                } else {
                    u64::MAX
                };
                sys_reply(r);
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

/// 创建文件 (`is_dir=false`) 或目录 (`is_dir=true`)。成功返回 fd。
fn mfs_create(path: &str, is_dir: bool) -> u64 {
    let mut canon = [0u8; TMP_PATH_MAX];
    let n = match tmp_normalize(path, &mut canon) {
        Some(n) => n,
        None => return u64::MAX,
    };
    // 已存在: 文件直接打开; 目录按类型匹配 (creat 遇目录 / mkdir 遇任何已有项都失败)。
    if mfs_resolve(&canon[..n]).is_some() {
        if is_dir {
            return u64::MAX;
        }
        let block = mfs_resolve(&canon[..n]).unwrap();
        if mfs_is_dir(block) {
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
    let parent_block = match mfs_resolve(&canon[..parent_end]) {
        Some(b) => b,
        None => return u64::MAX,
    };
    let parent_depth = unsafe { MFS_CHAIN_LEN };
    // 读父目录 (resolve 后 A 已持有父目录内容, 但仍显式重读以明确状态)。
    let a = mfs_a();
    if !mfs_read_blk(parent_block, a) || !mfs_ok(a, MFS_MAGIC_DIR) {
        return u64::MAX;
    }
    if mfs_dir_find(a, comp).is_some() {
        return u64::MAX;
    }
    // 新建空节点 (用 C, 避免覆盖 A 中的父目录)。
    let c = mfs_c();
    zero_bytes(c, MFS_BLOCK);
    if is_dir {
        write_u32(mfs_atm(c, MFS_HDR), 0); // nentries = 0
        write_u32(mfs_atm(c, MFS_HDR + 4), 0);
    } else {
        mfs_file_set_size(c, 0);
        mfs_file_set_nblocks(c, 0);
    }
    let magic = if is_dir { MFS_MAGIC_DIR } else { MFS_MAGIC_FILE };
    let inode = match mfs_commit(c, magic) {
        Some(b) => b,
        None => return u64::MAX,
    };
    // 在父目录追加条目 -> COW 父目录 -> 上溯。
    let typ = if is_dir { MFS_TYPE_DIR } else { MFS_TYPE_FILE };
    if mfs_dir_add(a, comp, inode, typ).is_none() {
        return u64::MAX;
    }
    let new_dir = match mfs_commit(a, MFS_MAGIC_DIR) {
        Some(b) => b,
        None => return u64::MAX,
    };
    if !mfs_propagate(parent_depth, new_dir) {
        return u64::MAX;
    }
    if is_dir {
        1
    } else {
        mfs_fd_alloc(&canon[..n], false)
    }
}

/// 删除文件 (`want_dir=false`) 或空目录 (`want_dir=true`)。成功返回 1。
fn mfs_remove(path: &str, want_dir: bool) -> u64 {
    let mut canon = [0u8; TMP_PATH_MAX];
    let n = match tmp_normalize(path, &mut canon) {
        Some(n) => n,
        None => return u64::MAX,
    };
    if n == 1 {
        return u64::MAX; // 不允许删除根
    }
    let block = match mfs_resolve(&canon[..n]) {
        Some(b) => b,
        None => return u64::MAX,
    };
    let depth = unsafe { MFS_CHAIN_LEN };
    if depth == 0 {
        return u64::MAX;
    }
    // resolve 后 A 持有父目录块; 取其目录项下标。
    let parent_frame = mfs_frame(depth - 1);
    let pidx = parent_frame.entry_index as usize;
    let a = mfs_a();
    if !mfs_read_blk(parent_frame.dir_block, a) || !mfs_ok(a, MFS_MAGIC_DIR) {
        return u64::MAX;
    }
    let etype = mfs_entry_type(a, pidx);
    let is_dir = etype == MFS_TYPE_DIR;
    if want_dir != is_dir {
        return u64::MAX;
    }
    if is_dir {
        // 目录必须为空。
        let b = mfs_b();
        if !mfs_read_blk(block, b) || !mfs_ok(b, MFS_MAGIC_DIR) {
            return u64::MAX;
        }
        if mfs_dir_count(b) != 0 {
            return u64::MAX;
        }
    }
    if !mfs_dir_remove(a, pidx) {
        return u64::MAX;
    }
    let new_dir = match mfs_commit(a, MFS_MAGIC_DIR) {
        Some(b) => b,
        None => return u64::MAX,
    };
    if !mfs_propagate(depth - 1, new_dir) {
        return u64::MAX;
    }
    1
}

#[cfg(target_os = "none")]
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    // 用户态 panic: 无法恢复, 直接终止本任务。
    syscall::sys_exit();
}
