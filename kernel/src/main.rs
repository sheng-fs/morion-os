//! Morion OS 微内核入口点
//!
//! 由引导器 (UEFI) 在长模式下跳转到此, 物理地址 0x100000。

#![no_std]
#![no_main]

use morion_kernel::{arch, bootinfo, cap, domain, ipc, memory, nvme, pager, scheduler, syscall, video};

extern crate alloc;
use alloc::boxed::Box;
use alloc::vec::Vec;

// 链接脚本 (.stack 段) 导出的内核栈顶
extern "C" {
    static _stack_end: u8;
}

// ---------------------------------------------------------------------------
// 阶段十: 用户程序加载 (编译产物, 替代阶段九的手写机器码)
// ---------------------------------------------------------------------------
/// 用户程序基址 (P4[1] 用户空间基址), 与 user/linker.ld 的链接地址一致。
const USER_BASE: u64 = memory::paging::USER_SPACE_BASE;
/// 用户栈页虚拟地址。
///
/// 不能紧邻程序镜像 (程序已超 1 页, 会与镜像后续页重叠); 也不得占用
/// `USER_BASE + 0x9000` (用户态 sender/receiver 共享页演示) 与
/// `USER_BASE + 0x10000` 起的 NVMe 配置/MMIO/DMA 区域。故预留 0x8000 起。
const USER_STACK_ADDR: u64 = USER_BASE + 0x8000;
/// 用户栈顶虚拟地址 (栈向下增长)。
const USER_STACK_TOP: u64 = USER_STACK_ADDR + 0x1000;
/// 页大小。
const PAGE_SIZE: u64 = 4096;

/// 编译期嵌入的用户程序扁平二进制 (由 Makefile 先构建 user, 再 objcopy 产出)。
const USER_PROGRAM: &[u8] = include_bytes!("../../build/user/user.bin");

/// 把用户程序加载到指定域: 分配连续物理帧, 映射到 `USER_BASE` 起, 拷贝镜像,
/// 并映射一页用户栈。
fn load_user_program(domain_id: u64) {
    let bytes = USER_PROGRAM;
    let pages = (bytes.len() as u64 + PAGE_SIZE - 1) / PAGE_SIZE;

    for i in 0..pages {
        let frame =
            memory::frame_allocator::allocate_frame().expect("allocate user program frame");
        let vaddr = USER_BASE + i * PAGE_SIZE;
        memory::paging::map_user_page(domain_id, vaddr, frame);

        let start = (i * PAGE_SIZE) as usize;
        let end = core::cmp::min(start + PAGE_SIZE as usize, bytes.len());
        unsafe {
            core::ptr::copy_nonoverlapping(
                bytes.as_ptr().add(start),
                frame as *mut u8,
                end - start,
            );
        }
    }

    // 用户栈页。
    let stack_frame =
        memory::frame_allocator::allocate_frame().expect("allocate user stack frame");
    memory::paging::map_user_page(domain_id, USER_STACK_ADDR, stack_frame);
}

/// 空闲任务: 当用户任务退出后兜底运行, 停机等待中断。
extern "C" fn task_idle() {
    // 首次进入时中断仍关闭 (run 未开启中断), 在此开启。
    x86_64::instructions::interrupts::enable();
    loop {
        x86_64::instructions::hlt();
    }
}

/// 内核入口 — 引导器通过 `jmp` 进入, Boot Info 指针在 rdi
#[no_mangle]
pub extern "C" fn _start() -> ! {
    // 设置内核栈 + 关中断 (IDT 就绪前不允许中断)
    unsafe {
        core::arch::asm!(
            "mov rsp, {0}",
            "cli",
            in(reg) &_stack_end as *const u8 as u64,
        );
    }

    // 1. 读取并校验 Boot Info
    let info = bootinfo::get();

    // 2. 初始化视频输出
    video::init(info);
    video::println("Morion OS Kernel");
    video::println("Stage 1: CPU initialization");
    video::println("");
    video::println("Boot Info verified (MORI)");

    // 3. GDT + TSS
    arch::gdt::init();
    video::println("[OK] GDT + TSS initialized");

    // 4. IDT
    arch::idt::init();
    video::println("[OK] IDT initialized (#BP / #DF / #PF)");

    // 5. 验证 IDT 工作 — 触发一次断点异常, 处理函数会打印后返回
    video::println("");
    video::println("Testing breakpoint exception...");
    unsafe { core::arch::asm!("int3") };
    video::println("[OK] Returned from #BP handler");

    // ============================================================
    //  阶段二: 物理内存管理
    // ============================================================
    video::println("");
    video::println("Stage 2: Physical memory management");
    memory::frame_allocator::init(info);
    memory::frame_allocator::print_stats();

    // 测试分配 / 释放
    video::println("");
    video::println("Testing frame allocation...");
    let f1 = memory::frame_allocator::allocate_frame();
    let f2 = memory::frame_allocator::allocate_frame();
    match (f1, f2) {
        (Some(a), Some(b)) => {
            video::print("[OK] Allocated frames at 0x");
            video::print_hex(a);
            video::print(" and 0x");
            video::print_hex(b);
            video::println("");
            memory::frame_allocator::free_frame(a);
            memory::frame_allocator::free_frame(b);
            video::println("[OK] Frames freed");
        }
        _ => {
            video::println("[FAIL] Frame allocation returned None");
        }
    }

    video::println("");
    video::println("Stage 2 complete.");

    // ============================================================
    //  阶段三: 虚拟内存与内核堆
    // ============================================================
    video::println("");
    video::println("Stage 3: Virtual memory & kernel heap");
    memory::paging::init();
    video::println("[OK] Paging initialized (identity + offset mapping)");

    // 测试内核堆 (Box / Vec)
    video::println("");
    video::println("Testing heap allocation...");
    let boxed = Box::new(0x2A);
    video::print("[OK] Box::new allocated, value = 0x");
    video::print_hex(*boxed as u64);
    video::println("");

    let mut vec = Vec::new();
    for i in 0..8 {
        vec.push(i);
    }
    video::print("[OK] Vec pushed ");
    video::print_u64(vec.len() as u64);
    video::println(" elements");

    video::println("");
    video::println("Stage 3 complete.");

    // ============================================================
    //  阶段四: 硬件中断框架
    // ============================================================
    video::println("");
    video::println("Stage 4: Hardware interrupts");
    arch::pic::init();
    arch::pit::init();
    arch::keyboard::init();
    video::println("[OK] PIC remapped + PIT timer started (100 Hz)");

    // ============================================================
    //  阶段 4.5: PCI 枚举 (文件系统阶段 0)
    // ============================================================
    video::println("");
    video::println("Stage 4.5: PCI enumeration");
    let pci_devices = arch::pci::enumerate();
    video::print("[OK] PCI devices found: ");
    video::print_u64(pci_devices.len() as u64);
    video::println("");
    for d in &pci_devices {
        video::print("  ");
        video::print_hex(((d.bus as u64) << 8) | ((d.dev as u64) << 3) | d.func as u64);
        video::print("  vend ");
        video::print_hex(d.vendor as u64);
        video::print("  dev ");
        video::print_hex(d.device as u64);
        video::print("  class ");
        video::print_hex(((d.class as u64) << 16) | ((d.subclass as u64) << 8) | d.progif as u64);
        video::println("");
    }

    // ============================================================
    //  阶段十: 用户态运行库 + 可加载用户程序
    // ============================================================
    video::println("");
    video::println("Stage 10: libuser + loadable user program");
    syscall::init();
    video::println("[OK] syscall/sysret enabled (EFER.SCE + STAR + LSTAR)");

    // ============================================================
    //  阶段十一: IPC + 能力系统
    // ============================================================
    scheduler::init();

    // 创建 8 个保护域:
    //   0 = sender    (持有 SendTo(1)+MapInto(1) 能力, 触发按需分页 + call 演示)
    //   1 = receiver  (接收消息)
    //   2 = pager     (分页器, 服务所有域的缺页)
    //   3 = echo      (同步 IPC 服务: recv → reply 回显)
    //   4 = kbd       (用户态键盘驱动, 注册接收 IRQ1)
    //   5 = block_srv (IDE PIO 块设备服务)
    //   6 = fat32_srv (FAT32 文件服务)
    //   7 = app       (测试应用, 经 libvfs 读文件)
    let sender_domain = domain::create();
    let receiver_domain = domain::create();
    let pager_domain = domain::create();
    let echo_domain = domain::create();
    let kbd_domain = domain::create();
    let block_domain = domain::create();
    let fat32_domain = domain::create();
    let app_domain = domain::create();

    // 初始化 IPC 邮箱、能力表与分页器映射 (数量 = 域数量)。
    ipc::init(8);
    cap::init(8);
    pager::init(8, pager_domain);

    // 授权: sender 可向 receiver 发送 + 共享内存。
    cap::grant(sender_domain, cap::Capability::SendTo(receiver_domain));
    cap::grant(sender_domain, cap::Capability::MapInto(receiver_domain));
    // 授权: sender 可向 echo 服务发起同步调用 (Stage 15)。
    cap::grant(sender_domain, cap::Capability::SendTo(echo_domain));
    // 授权: 分页器是全部域的分页器, 授予其向每个域映射匿名帧的能力 (按需分页)。
    for d in [
        sender_domain,
        receiver_domain,
        pager_domain,
        echo_domain,
        kbd_domain,
        block_domain,
        fat32_domain,
        app_domain,
    ] {
        cap::grant(pager_domain, cap::Capability::MapInto(d));
    }
    // 授权: 键盘驱动域注册接收 IRQ1 (Stage 16)。
    cap::grant(kbd_domain, cap::Capability::Irq(1));
    // 授权: fat32_srv 经 IPC 调 block_srv (SendTo) 并共享缓冲页 (MapInto)。
    cap::grant(fat32_domain, cap::Capability::SendTo(block_domain));
    cap::grant(fat32_domain, cap::Capability::MapInto(block_domain));
    // 授权: app 经 IPC 调 fat32_srv 读文件 (阶段 C), 并共享结果页 (MapInto)。
    cap::grant(app_domain, cap::Capability::SendTo(fat32_domain));
    cap::grant(app_domain, cap::Capability::MapInto(fat32_domain));
    // 授权: block_srv (域 5) 访问 IDE primary 通道 I/O 端口 (0x1F0-0x1F7),
    // 用于无 NVMe 控制器时的 IDE PIO 回退路径。NVMe 模式通过 MMIO 访问,
    // 不使用 I/O 端口, 授予这些能力不影响其他域 (无能力即被拒绝)。
    for port in 0x1F0u16..=0x1F7 {
        cap::grant(block_domain, cap::Capability::IoPort(port));
    }
    video::println("[OK] IPC + capability + pager initialized (8 domains)");

    // 探测 NVMe 控制器并配置 block 域 (文件系统阶段 1: NVMe 块设备后端)。
    // 找到则映射 BAR0/队列/DMA 并授权 Mmio; 否则降级 (magic=0), block 回退 IDE PIO。
    match arch::pci::find_nvme(&pci_devices) {
        Some((_bus, _dev, _func, bar0)) => {
            nvme::setup(block_domain, bar0);
            video::print("[OK] NVMe controller BAR0=0x");
            video::print_hex(bar0);
            video::println("");
        }
        None => {
            nvme::setup_empty(block_domain);
            video::println("[OK] no NVMe controller, block falls back to IDE PIO");
        }
    }

    // 加载用户程序到八个域 (同一镜像, 经 domain_id 参数区分角色)。
    load_user_program(sender_domain);
    load_user_program(receiver_domain);
    load_user_program(pager_domain);
    load_user_program(echo_domain);
    load_user_program(kbd_domain);
    load_user_program(block_domain);
    load_user_program(fat32_domain);
    load_user_program(app_domain);
    video::println("[OK] user program loaded into domains 0 & 1 & 2 & 3 & 4 & 5 & 6 & 7");

    // 域 0..7 各起一个用户任务。
    scheduler::spawn_user(USER_BASE, USER_STACK_TOP, sender_domain);
    scheduler::spawn_user(USER_BASE, USER_STACK_TOP, receiver_domain);
    scheduler::spawn_user(USER_BASE, USER_STACK_TOP, pager_domain);
    scheduler::spawn_user(USER_BASE, USER_STACK_TOP, echo_domain);
    scheduler::spawn_user(USER_BASE, USER_STACK_TOP, kbd_domain);
    scheduler::spawn_user(USER_BASE, USER_STACK_TOP, block_domain);
    scheduler::spawn_user(USER_BASE, USER_STACK_TOP, fat32_domain);
    scheduler::spawn_user(USER_BASE, USER_STACK_TOP, app_domain);
    // 空闲任务兜底 (归属 sender 域)。
    scheduler::spawn(task_idle, sender_domain);
    video::println("[OK] sender + receiver + pager + echo + kbd + block + fat32 + app + idle tasks spawned");
    video::println("");
    video::println("Expected: sender shares memory with receiver, then");
    video::println("touches an unmapped page to trigger demand paging.");
    video::println("");

    // 交给调度器。首次切换在中断关闭下进行, 避免 enable 与首次调度之间
    // 的竞态 (否则定时器中断会在 run 完成前触发 schedule 抢走主执行流)。
    scheduler::run();
}

#[cfg(target_os = "none")]
#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    if video::ready() {
        video::clear(0x000033);
        video::set_cursor(2, 2);
        video::println("KERNEL PANIC");
        // 打印 panic 位置与消息, 便于定位崩溃点 (黑匣子)。
        if let Some(loc) = info.location() {
            let s = alloc::format!("  at {}:{}:{}", loc.file(), loc.line(), loc.column());
            video::println(&s);
        }
        let s = alloc::format!("  {}", info.message());
        video::println(&s);
    }
    morion_kernel::halt();
}

// 仅用于 rust-analyzer 在 host 目标上检查时满足 [[bin]] 的 main 要求
// 实际内核编译时 (target_os = "none") 此函数被排除
#[cfg(not(target_os = "none"))]
fn main() {}
