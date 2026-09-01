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
    print, print_hex, print_u64, println, sys_alloc_page, sys_backspace, sys_call,
    sys_call_payload, sys_map_anon, sys_page_fault_reply, sys_port_in16, sys_port_in8,
    sys_port_out8, sys_recv, sys_recv_msg, sys_register_irq, sys_reply, sys_scroll_down,
    sys_scroll_up, sys_send, sys_share_page, sys_term_left, sys_term_put, sys_term_right,
    sys_unmap, sys_virt_to_phys,
};

/// 各服务域 id (与内核 `main.rs` 创建顺序一致)。
///   0 sender / 1 receiver / 2 pager / 3 echo / 4 kbd
///   5 block_srv / 6 fat32_srv / 7 app
const BLOCK_DOMAIN: u64 = 5;
const APP_DOMAIN: u64 = 7;

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
        _ => {}
    }
    syscall::sys_exit();
}

/// 域 0 — 发送者: 持有 SendTo(1) + MapInto(1) 能力, 无 SendTo(2) 能力。
fn sender_main() {
    println("sender (domain 0) starting...");

    // 共享内存演示: 申请一页 → 写入 → 共享给域 1 → IPC 通知。
    let page = 0x8000_0090_00u64;
    if sys_alloc_page(page) == 1 {
        let msg = "HELLO SHARED";
        unsafe {
            core::ptr::copy_nonoverlapping(msg.as_ptr(), page as *mut u8, msg.len());
        }
        println("sender: wrote \"HELLO SHARED\" to shared page");
    } else {
        println("sender: alloc_page FAILED");
    }

    // 共享给域 1 (需 MapInto(1) 能力)。
    if sys_share_page(page, 1) == 1 {
        println("sender: shared page with domain 1");
    } else {
        println("sender: share_page DENIED");
    }

    // IPC 通知 receiver 读取 (复用现有 SendTo 能力)。
    let sent = sys_send(1, 777);
    if sent == 1 {
        println("sender: notified receiver (tag=777)");
    }

    // 解除本域映射: 引用计数 2 -> 1, 帧不释放 (receiver 仍持有)。
    if sys_unmap(page) == 1 {
        println("sender: unmapped own copy (ref 2 -> 1)");
    } else {
        println("sender: unmap FAILED");
    }

    // Stage 14: 按需分页演示 — 访问一个用户空间内从未映射的地址触发缺页。
    // 注意: 必须是 canonical 用户空间地址 (P4[1], 低 40 位在 0x80_0000_0000 ~
    // 0xFF_FFFF_FFFF, 即 10 位 hex)。若写成 12 位 hex (如 0x9000_0000_0000) 会令
    // bit47=1, 成为非 canonical 地址, 访问时触发 #GP 而非 #PF, 进而 double fault。
    let fault_addr = 0x81_0000_0000u64;
    println("sender: touching unmapped page (demand paging)...");
    let val = unsafe { core::ptr::read_volatile(fault_addr as *const u64) };
    print("sender: read from demand-paged page = ");
    print_u64(val);
    println("");

    // Stage 15: 同步 IPC call/reply 演示 — 调用 echo 服务 (域 3)。
    // 期望回复 tag = 0xABCE (echo 把收到的 tag + 1 回显)。
    let echo_tag = sys_call(3, 0xABCD);
    print("sender: call echo -> reply tag = ");
    print_hex(echo_tag);
    println("");

    println("sender done, exiting...");
}

/// 域 1 — 接收者: 经 IPC 收到通知后, 直接从共享页读取数据。
fn receiver_main() {
    println("receiver (domain 1) starting...");

    // 等 sender 通知共享页就绪。
    let tag = sys_recv();
    print("receiver: got notify, tag = ");
    print_u64(tag);
    println("");

    // 直接读共享页 (零拷贝, 数据未经 IPC 传递)。
    let page = 0x8000_0090_00u64;
    let bytes = unsafe { core::slice::from_raw_parts(page as *const u8, 12) };
    let s = unsafe { core::str::from_utf8_unchecked(bytes) };
    print("receiver: read from shared page -> \"");
    print(s);
    println("\"");

    // 解除映射: 引用计数 1 -> 0, 真正释放帧。
    if sys_unmap(page) == 1 {
        println("receiver: unmapped (ref 1 -> 0, frame freed)");
    } else {
        println("receiver: unmap FAILED");
    }

    println("receiver done, exiting...");
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
    println("pager (domain 2) starting...");
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

        print("pager: fault on domain ");
        print_u64(info.fault_domain);
        print(" at addr 0x");
        print_hex(info.fault_addr);
        println("");

        if sys_map_anon(info.fault_domain, info.fault_addr) == 1 {
            println("pager: mapped anonymous zero frame");
        } else {
            println("pager: map_anon FAILED");
        }

        sys_page_fault_reply();
        println("pager: replied, faulting task resumed");
    }
}

/// 域 3 — echo 服务: 同步 IPC 演示, 循环 `recv` → `reply` (回显 tag + 1)。
fn echo_main() {
    println("echo (domain 3) starting...");
    loop {
        let tag = sys_recv();
        print("echo: received call, tag = ");
        print_hex(tag);
        println("");

        // 回复当前调用者 (回复目标由内核在 recv 时记录)。
        if sys_reply(tag + 1) == 1 {
            println("echo: replied");
        } else {
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
    println("kbd (domain 4) starting...");

    if sys_register_irq(1) != 1 {
        println("kbd: register irq1 FAILED");
        return;
    }
    println("kbd: registered for IRQ1");

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

/// 内核映射到本域的配置结构虚拟地址 (见 kernel/src/nvme.rs)。
const NVME_CFG_VADDR: u64 = 0x0000_0080_0000_0000 + 0x1_0000;
/// 配置结构 magic 校验值 (与内核一致)。
const NVME_CONFIG_MAGIC: u64 = 0x4E56_4D45_4F53_21;

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
            let sc = (cqe.sf >> 1) & 0xFFFF;
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
fn nvme_read_sectors(
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
        print("nvme: read virt_to_phys failed buf=");
        print_u64(buf as u64);
        print(" count=");
        print_u64(count as u64);
        println("");
        return false;
    }
    let mut sqe = Sqe::zero();
    sqe.opcode = OP_READ;
    sqe.cid = 5;
    sqe.nsid = 1;
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
    println("nvme (domain 5) starting...");

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
    println("nvme: controller ready");

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
    println("nvme: I/O queue 1 created");

    // 10. 进入块设备服务循环: 经 IPC 接收 BlockReq, 用 NVMe I/O 队列读扇区。
    let isq_doorbell = mmio + DOORBELL_BASE + 2 * stride;
    let icq_doorbell = mmio + DOORBELL_BASE + 3 * stride;
    let mut io_tail: u32 = 0;
    let mut io_head: u32 = 0;
    // 与 Admin 队列同理: 首条 completion 的 phase tag 为 1。
    let mut io_phase: u32 = 1;

    println("nvme: block service ready");
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
        print("nvme: block req op=");
        print_u64(req.op as u64);
        print(" lba=");
        print_u64(req.lba);
        print(" count=");
        print_u64(req.count);
        print(" buf=0x");
        print_u64(req.buf);
        println("");

        match req.op {
            0 => {
                let ok = nvme_read_sectors(
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
                print("nvme: read result=");
                print_u64(ok as u64);
                println("");
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
const IDE_ERROR: u16 = 0x1F1; // 错误寄存器 (读)
const IDE_SECT_CNT: u16 = 0x1F2; // 扇区数
const IDE_LBA_LO: u16 = 0x1F3; // LBA 位 0-7
const IDE_LBA_MID: u16 = 0x1F4; // LBA 位 8-15
const IDE_LBA_HI: u16 = 0x1F5; // LBA 位 16-23
const IDE_DRIVE: u16 = 0x1F6; // 驱动器/磁头 (bit6=1 LBA 模式, bit7=1 master)
const IDE_STATUS: u16 = 0x1F7; // 状态 (读) / 命令 (写)
const IDE_CMD: u16 = 0x1F7; // 命令寄存器 (写)

/// ATA 命令: READ SECTORS (28 位 LBA)。
const ATA_READ_SECTORS: u8 = 0x20;

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

/// 块设备请求 tag (block_srv 据此识别读/写请求)。
const BLOCK_REQ_TAG: u64 = 0x424C_4F43; // "BLOC"

/// 块设备请求 (序列化进 IPC payload, 32 字节, 与内核 `PAYLOAD_LEN` 一致)。
/// `buf` 为数据缓冲页虚拟地址, 须已由调用方共享映射进 block_srv 地址空间。
#[repr(C)]
#[derive(Clone, Copy)]
struct BlockReq {
    op: u64,    // 0 = read, 1 = write (本轮仅 read)
    lba: u64,   // 起始扇区号
    count: u64, // 扇区数 (1..=256)
    buf: u64,   // 数据缓冲页虚拟地址
}

/// 经 IPC 请求 block_srv 读 `count` 个扇区到 `buf`。成功返回 true。
///
/// fat32_srv 通过它间接访问块设备, 而非直接触碰 IDE 端口; IDE PIO 逻辑
/// 收拢在 block_srv 内, 符合微内核「驱动服务化」的解耦。
fn block_read(lba: u32, count: u16, buf: *mut u8) -> bool {
    let req = BlockReq {
        op: 0,
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

/// 简单的字节缓冲写入器 (无分配器, 用于把目录列表写进结果页)。
struct BufWriter {
    ptr: *mut u8,
    len: usize,
    cap: usize,
}

impl BufWriter {
    fn new(ptr: *mut u8, cap: usize) -> Self {
        BufWriter { ptr, len: 0, cap }
    }

    fn put(&mut self, b: u8) {
        if self.len < self.cap {
            unsafe {
                core::ptr::write(self.ptr.add(self.len), b);
            }
            self.len += 1;
        }
    }

    fn write(&mut self, s: &str) {
        for &b in s.as_bytes() {
            self.put(b);
        }
    }

    fn write_u64(&mut self, mut v: u64) {
        let mut buf = [0u8; 20];
        let mut i = buf.len();
        loop {
            i -= 1;
            buf[i] = b'0' + (v % 10) as u8;
            v /= 10;
            if v == 0 {
                break;
            }
        }
        for &b in &buf[i..] {
            self.put(b);
        }
    }
}

/// 把目录项的 8.3 文件名 (主名 + '.' + 扩展名, 去尾随空格) 写入缓冲。
fn write_dir_name(w: &mut BufWriter, entry: *const u8) {
    let name = unsafe { core::slice::from_raw_parts(entry, 8) };
    let ext = unsafe { core::slice::from_raw_parts(entry.add(8), 3) };

    let mut end = name.len();
    while end > 0 && name[end - 1] == b' ' {
        end -= 1;
    }
    for &b in &name[..end] {
        w.put(b);
    }

    if ext[0] != b' ' {
        w.put(b'.');
        let mut eend = ext.len();
        while eend > 0 && ext[eend - 1] == b' ' {
            eend -= 1;
        }
        for &b in &ext[..eend] {
            w.put(b);
        }
    }
}

/// 把目录 `dir_cluster` 的条目列表 (文本) 写入 `out`, 返回写入字节数; 失败返回 u64::MAX。
fn readdir_into(
    bpb: &Fat32Bpb,
    dir_cluster: u32,
    dir_buf: *mut u8,
    fat_buf: *mut u8,
    out: *mut u8,
) -> u64 {
    let entries_per_cluster = bpb.cluster_bytes() as usize / 32;
    let mut cluster = dir_cluster;
    let mut w = BufWriter::new(out, 4096);

    loop {
        if !read_cluster(bpb, cluster, dir_buf) {
            return u64::MAX;
        }
        for i in 0..entries_per_cluster {
            let entry = unsafe { dir_buf.add(i * 32) };
            let first = unsafe { *entry };
            if first == 0x00 {
                return w.len as u64; // 目录结束
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

            let file_size = read_u32(unsafe { entry.add(28) });

            if attr & ATTR_DIRECTORY != 0 {
                // "." / ".." 以 '.' 开头, 跳过。
                if first == b'.' {
                    continue;
                }
                w.write("[DIR]  ");
                write_dir_name(&mut w, entry);
                w.write("\n");
            } else {
                w.write("[FILE] ");
                write_dir_name(&mut w, entry);
                w.write(" size=");
                w.write_u64(file_size as u64);
                w.write("\n");
            }
        }

        // 跨簇: 读下一个目录簇。
        let next = read_fat_entry(bpb, cluster, fat_buf);
        if next >= 0x0FFF_FFF8 {
            return w.len as u64;
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
}

/// ASCII 大写 (仅处理 a-z)。
fn ascii_upper(c: u8) -> u8 {
    if (b'a'..=b'z').contains(&c) {
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
        for i in 0..11 {
            if *entry.add(i) != sn[i] {
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
            if !entry_name_matches(entry, &sn) {
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
        });
    }
    resolve_path(bpb, path, dir_buf, fat_buf)
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
    println("block (domain 5) starting...");

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

        match req.op {
            0 => {
                // read: 读 count (1..=256) 个扇区到 req.buf 指向的共享页。
                let ok = read_sectors(req.lba as u32, req.count as u16, req.buf as *mut u8);
                sys_reply(if ok { 1 } else { 0 });
            }
            _ => {
                // 本轮未实现 write。
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
}

static mut FD_TABLE: [Option<OpenNode>; MAX_FD] = [None; MAX_FD];

/// 分配一个空闲 fd 槽位, 返回 fd (0..MAX_FD), 表满返回 u64::MAX。
fn fd_alloc(is_dir: bool, start_cluster: u32, file_size: u32) -> u64 {
    unsafe {
        let base = core::ptr::addr_of_mut!(FD_TABLE).cast::<Option<OpenNode>>();
        for i in 0..MAX_FD {
            let slot = base.add(i);
            if (*slot).is_none() {
                *slot = Some(OpenNode {
                    is_dir,
                    start_cluster,
                    file_size,
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
    println("fat32 (domain 6) starting...");

    // 缓冲页: BPB / 目录簇 / FAT 扇区 / 文件内容。
    let bpb_buf = 0x0000_0080_0000_6000u64;
    let dir_buf = 0x0000_0080_0000_7000u64;
    let fat_buf = 0x0000_0080_0000_5000u64;
    let file_buf = 0x0000_0080_0000_4000u64;

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
                        vfs::RESULT_BUF as *mut u8,
                    ),
                    _ => u64::MAX,
                };
                sys_reply(n);
            }
            vfs::VFS_READDIR_TAG => {
                let fd = read_u32(msg.payload.as_ptr()) as u32;
                let n = match fd_lookup(fd) {
                    Some(node) if node.is_dir => readdir_into(
                        &bpb,
                        node.start_cluster,
                        dir_buf as *mut u8,
                        fat_buf as *mut u8,
                        vfs::RESULT_BUF as *mut u8,
                    ),
                    _ => u64::MAX,
                };
                sys_reply(n);
            }
            vfs::VFS_CLOSE_TAG => {
                let fd = read_u32(msg.payload.as_ptr()) as u32;
                sys_reply(fd_free(fd));
            }
            _ => {
                sys_reply(u64::MAX);
            }
        }
    }
}

/// 域 7 — 测试应用: 经 libvfs 请求 fat32_srv 读取文件与列出目录。
fn app_main() {
    println("app (domain 7) starting...");

    // 分配结果页并共享给 fat32_srv (同地址映射), 供其写入文件/目录内容。
    if sys_alloc_page(vfs::RESULT_BUF) != 1 {
        println("app: alloc result buf FAILED");
        return;
    }
    if sys_share_page(vfs::RESULT_BUF, vfs::FAT32_DOMAIN) != 1 {
        println("app: share result buf FAILED");
        return;
    }

    // 1. open -> read -> close: 读整个文件。
    let fd = vfs::open("/HELLO.TXT");
    if fd == u64::MAX {
        println("app: open \"/HELLO.TXT\" -> NOT FOUND");
        return;
    }
    let size = vfs::read(fd, 0, 4096);
    if size == u64::MAX {
        println("app: read \"/HELLO.TXT\" -> FAILED");
    } else {
        let content =
            unsafe { core::slice::from_raw_parts(vfs::RESULT_BUF as *const u8, size as usize) };
        let s = unsafe { core::str::from_utf8_unchecked(content) };
        print("app: read \"/HELLO.TXT\" = \"");
        print_sanitized(s);
        println("\"");
    }
    vfs::close(fd);

    // 2. open("/") -> readdir -> close: 列出根目录。
    let dfd = vfs::open("/");
    if dfd == u64::MAX {
        println("app: open \"/\" -> FAILED");
        return;
    }
    let n = vfs::readdir(dfd);
    if n == u64::MAX {
        println("app: readdir \"/\" -> FAILED");
    } else {
        println("app: readdir \"/\" ->");
        let listing =
            unsafe { core::slice::from_raw_parts(vfs::RESULT_BUF as *const u8, n as usize) };
        print(unsafe { core::str::from_utf8_unchecked(listing) });
        println("");
    }
    vfs::close(dfd);

    println("app done, exiting...");
}

#[cfg(target_os = "none")]
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    // 用户态 panic: 无法恢复, 直接终止本任务。
    syscall::sys_exit();
}
