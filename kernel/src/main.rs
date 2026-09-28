//! Morion OS 微内核入口点
//!
//! 由引导器 (UEFI) 在长模式下跳转到此, 物理地址 0x100000。

#![no_std]
#![no_main]

use morion_kernel::{
    arch, bootinfo, cap, domain, exec, ipc, memory, nvme, pager, scheduler, syscall, video,
};

extern crate alloc;
use alloc::boxed::Box;
use alloc::vec::Vec;

// 链接脚本 (.stack 段) 导出的内核栈顶
extern "C" {
    static _stack_end: u8;
}

// 真正的 ELF 入口: 必须在任何 Rust 栈帧建立之前设定 rsp (栈顶) 并关中断。
// 用纯汇编桩实现 (而非普通 Rust 函数), 否则 LLVM 会先按引导器 rsp 分配栈帧,
// 之后重置 rsp 会把该帧平移到镜像之上、破坏帧分配器交出的内存 (详见 kernel_main)。
core::arch::global_asm!(
    ".global _start",
    "_start:",
    "lea rsp, [rip + _stack_end]",
    "cli",
    "jmp kernel_main",
);

// ---------------------------------------------------------------------------
// 阶段十 / E2b: 引导期加载服务程序 (每个服务一份独立 ELF, 见 user/srv)
// ---------------------------------------------------------------------------
/// 引导期服务程序表: `(固定域号, 编译期嵌入的 ELF 镜像)`。
///
/// E2b 起每个服务是**独立程序** (独立 crate / 独立 ELF, 见 `user/srv`), 由内核在引导期
/// 经 `exec::spawn_elf_at` 载入到**各自的固定域**。域号是 ABI: libvfs 写死
/// `FAT32_DOMAIN=6` 等, shell 直接 `SendTo(5)`, 所以"域号 ↔ 程序"的对应必须稳定。
///
/// ELF 由 Makefile 构建到 `build/user/srv/<name>.elf` (`make` 先构建 `morion-srv` 的 14 个
/// bin 再逐个拷贝)。此前 14 个域共用一份扁平二进制 (`build/user/user.bin`) 按域 id 分流 ——
/// 那是过渡态, 现已删除; 现在每个域跑的是它自己那份 ELF, 各自独立地址空间。
const SERVICE_ELFS: [(u64, &[u8]); 14] = [
    (0, include_bytes!("../../build/user/srv/sender.elf")),
    (1, include_bytes!("../../build/user/srv/receiver.elf")),
    (2, include_bytes!("../../build/user/srv/pager.elf")),
    (3, include_bytes!("../../build/user/srv/echo.elf")),
    (4, include_bytes!("../../build/user/srv/kbd.elf")),
    (5, include_bytes!("../../build/user/srv/block_srv.elf")),
    (6, include_bytes!("../../build/user/srv/fat32_srv.elf")),
    (7, include_bytes!("../../build/user/srv/app.elf")),
    (8, include_bytes!("../../build/user/srv/shell.elf")),
    (9, include_bytes!("../../build/user/srv/mount_srv.elf")),
    (10, include_bytes!("../../build/user/srv/tmpfs_srv.elf")),
    (11, include_bytes!("../../build/user/srv/mfs_srv.elf")),
    (12, include_bytes!("../../build/user/srv/ext2_srv.elf")),
    (13, include_bytes!("../../build/user/srv/exfat_srv.elf")),
];

/// 空闲任务: 当用户任务退出后兜底运行, 停机等待中断。
///
/// `hlt` 让出 CPU 的同时把控制权交还宿主 (KVM 里 vCPU 因此退出客户机,
/// QEMU 的设备模型才有机会在主循环里 post 完成并投中断); `hlt` 返回后
/// `yield_now` 让**刚被中断唤醒的域立刻接手**, 而不必再等一个时钟 tick ——
/// 这是 `SYS_IRQ_WAIT` 阻塞等中断时唤醒延迟的关键一环。
extern "C" fn task_idle() {
    // 首次进入时中断仍关闭 (run 未开启中断), 在此开启。
    x86_64::instructions::interrupts::enable();
    loop {
        x86_64::instructions::hlt();
        scheduler::yield_now();
    }
}

/// 内核入口 — 引导器通过 `jmp` 进入, Boot Info 指针在 rdi
///
/// 该符号由 `global_asm!` 定义 (见下方), 是真正的 ELF 入口: 它必须在任何
/// Rust 栈帧建立之前设置 `rsp`。若把 `mov rsp, _stack_end` 放进普通 Rust
/// 函数 (如 `extern "C" fn _start`), LLVM 会在入口处先按引导器的 rsp
/// 「向下」分配栈帧, 随后该 asm 把 rsp 重置到 `_stack_end`, 导致整个栈帧
/// 被平移到 `[_stack_end, _stack_end + frame_size)` —— 恰好落在镜像之后、
/// 帧分配器最先交出去的内存上 (堆的第 0 页), 从而随机破坏堆的链表元数据。
#[no_mangle]
pub extern "C" fn kernel_main() -> ! {
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

    // 创建 13 个保护域:
    //   0 = sender    (持有 SendTo(1)+MapInto(1) 能力, 触发按需分页 + call 演示)
    //   1 = receiver  (接收消息)
    //   2 = pager     (分页器, 服务所有域的缺页)
    //   3 = echo      (同步 IPC 服务: recv → reply 回显)
    //   4 = kbd       (用户态键盘驱动, 注册接收 IRQ1)
    //   5 = block_srv (IDE PIO / NVMe 块设备服务)
    //   6 = fat32_srv (FAT32 文件服务)
    //   7 = app       (测试应用, 经 libvfs 读文件)
    //   8 = shell     (命令行解释器, 经 libvfs 访问文件服务)
    //   9 = mount_srv (挂载服务: 路径前缀 → 文件服务域, 支撑统一目录树)
    //  10 = tmpfs_srv (内存文件系统, 挂载于 /tmp)
    //  11 = mfs_srv   (MorionFS: 块设备后端的原创文件系统, 挂载于 /mfs)
    //  12 = ext2_srv  (ext2 只读兼容: 挂载既有 Linux 分区, 挂载于 /ext2)
    //  13 = exfat_srv (exFAT 读写: 挂载既有 exFAT 卷/U 盘, 挂载于 /usb)
    let sender_domain = domain::create();
    let receiver_domain = domain::create();
    let pager_domain = domain::create();
    let echo_domain = domain::create();
    let kbd_domain = domain::create();
    let block_domain = domain::create();
    let fat32_domain = domain::create();
    let app_domain = domain::create();
    let shell_domain = domain::create();
    let mount_domain = domain::create();
    let tmpfs_domain = domain::create();
    let mfs_domain = domain::create();
    let ext2_domain = domain::create();
    let exfat_domain = domain::create();

    // 初始化 IPC 邮箱、能力表与分页器映射 (数量 = 域数量)。
    ipc::init(14);
    cap::init(14);
    pager::init(14, pager_domain);

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
        shell_domain,
        mount_domain,
        tmpfs_domain,
        mfs_domain,
        ext2_domain,
        exfat_domain,
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
    // 授权: shell 经 IPC 调 fat32_srv 执行文件操作, 并共享结果/写缓冲页 (MapInto)。
    cap::grant(shell_domain, cap::Capability::SendTo(fat32_domain));
    cap::grant(shell_domain, cap::Capability::MapInto(fat32_domain));
    // 授权: app / shell 经 mount_srv 查询路径路由 (阶段 C1)。
    cap::grant(app_domain, cap::Capability::SendTo(mount_domain));
    cap::grant(shell_domain, cap::Capability::SendTo(mount_domain));
    // 授权: app / shell 可访问 tmpfs_srv (挂载于 /tmp), 并共享缓冲页 (阶段 C2)。
    cap::grant(app_domain, cap::Capability::SendTo(tmpfs_domain));
    cap::grant(app_domain, cap::Capability::MapInto(tmpfs_domain));
    cap::grant(shell_domain, cap::Capability::SendTo(tmpfs_domain));
    cap::grant(shell_domain, cap::Capability::MapInto(tmpfs_domain));
    // 授权: mfs_srv 经 IPC 调 block_srv (SendTo) 访问 MFS 盘 (namespace 2) 并共享块缓冲。
    cap::grant(mfs_domain, cap::Capability::SendTo(block_domain));
    cap::grant(mfs_domain, cap::Capability::MapInto(block_domain));
    // 授权: app / shell 可访问 mfs_srv (挂载于 /mfs), 并共享缓冲页 (阶段 C3)。
    cap::grant(app_domain, cap::Capability::SendTo(mfs_domain));
    cap::grant(app_domain, cap::Capability::MapInto(mfs_domain));
    cap::grant(shell_domain, cap::Capability::SendTo(mfs_domain));
    cap::grant(shell_domain, cap::Capability::MapInto(mfs_domain));
    // 授权: ext2_srv 经 IPC 调 block_srv (SendTo) 访问 ext2 盘 (namespace 3) 并共享块缓冲。
    cap::grant(ext2_domain, cap::Capability::SendTo(block_domain));
    cap::grant(ext2_domain, cap::Capability::MapInto(block_domain));
    // 授权: app / shell 可访问 ext2_srv (挂载于 /ext2), 并共享缓冲页。
    cap::grant(app_domain, cap::Capability::SendTo(ext2_domain));
    cap::grant(app_domain, cap::Capability::MapInto(ext2_domain));
    cap::grant(shell_domain, cap::Capability::SendTo(ext2_domain));
    cap::grant(shell_domain, cap::Capability::MapInto(ext2_domain));
    // 授权: exfat_srv 经 IPC 调 block_srv 访问 exFAT 卷 (namespace 5) 并共享块缓冲。
    cap::grant(exfat_domain, cap::Capability::SendTo(block_domain));
    cap::grant(exfat_domain, cap::Capability::MapInto(block_domain));
    // 授权: app / shell 可访问 exfat_srv (挂载于 /usb), 并共享缓冲页。
    cap::grant(app_domain, cap::Capability::SendTo(exfat_domain));
    cap::grant(app_domain, cap::Capability::MapInto(exfat_domain));
    cap::grant(shell_domain, cap::Capability::SendTo(exfat_domain));
    cap::grant(shell_domain, cap::Capability::MapInto(exfat_domain));
    // 授权: app 可直接查询 block_srv 的卷表 (自测卷层/分区解析); 只需读 + 共享结果页。
    cap::grant(app_domain, cap::Capability::SendTo(block_domain));
    cap::grant(app_domain, cap::Capability::MapInto(block_domain));
    // 授权: app / shell 可加载可执行文件并启动 (`SYS_SPAWN_ELF`)。
    //   - app: E1 自测用 (从文件读回镜像 → 载入新域);
    //   - shell: `run <path>` 命令 —— 让"可执行文件加载"成为用户可见的功能。
    // 新域默认零能力 —— 「能造进程」这张凭证只给需要它的域。
    cap::grant(app_domain, cap::Capability::Spawn);
    cap::grant(shell_domain, cap::Capability::Spawn);
    // 授权: shell 可直接让 block_srv 改分区表 (shell 的 `part.*` 命令)。分区表写入只用块
    // 服务自己的暂存页, 不需要共享缓冲, 故只给 SendTo。
    cap::grant(shell_domain, cap::Capability::SendTo(block_domain));
    // 授权: 各文件服务把**自己那类的额外卷**上报给 mount_srv (M1b 多卷挂载:
    // `/usb<卷号>`)。只需 SendTo —— 挂载请求是一条普通 IPC, 不经共享页。
    cap::grant(fat32_domain, cap::Capability::SendTo(mount_domain));
    cap::grant(ext2_domain, cap::Capability::SendTo(mount_domain));
    cap::grant(exfat_domain, cap::Capability::SendTo(mount_domain));
    // mfs_srv 也要上报额外卷: 真盘上可以有多块 MFS 卷, 除主卷 (/mfs) 外的挂到 `/usb<卷号>`。
    cap::grant(mfs_domain, cap::Capability::SendTo(mount_domain));
    video::println("[OK] IPC + capability + pager initialized (14 domains)");

    // 探测 NVMe 控制器并配置 block 域 (文件系统阶段 1: NVMe 块设备后端)。
    // 找到则配置 MSI-X、映射 BAR0/队列/DMA 并授权 Mmio/Irq; 否则降级 (magic=0),
    // block 回退 IDE PIO。
    match arch::pci::find_nvme(&pci_devices) {
        Some((bus, dev, func, bar0)) => {
            nvme::setup(block_domain, bus, dev, func, bar0);
            video::print("[OK] NVMe controller BAR0=0x");
            video::print_hex(bar0);
            video::println("");
        }
        None => {
            nvme::setup_empty(block_domain);
            video::println("[OK] no NVMe controller, block falls back to IDE PIO");
        }
    }

    // 逐个加载服务 ELF 并起任务: 域号已按 `SERVICE_ELFS` 的顺序 (0..13) 建好, 故直接
    // 按表内域号载入 —— 每个服务跑自己的 ELF、进自己的地址空间 (E2b)。
    for (dom, image) in SERVICE_ELFS {
        if !exec::spawn_elf_at(dom, image) {
            video::println("[FAILED] service ELF load (embedded)");
        }
    }
    video::println("[OK] 14 service ELFs loaded (embedded)");

    // 空闲任务兜底 (归属 sender 域)。
    scheduler::spawn(task_idle, sender_domain);
    video::println("[OK] 14 service tasks + idle task spawned");
    video::println("");
    // 启动 LOGO (日志末尾, shell 提示符之前)。
    video::print_logo();
    video::println("");
    // 中文渲染自检: 汉字是 16x16 点阵、占 2 个字符格 (字库见 video/cjk.bin),
    // 与 8x16 的 ASCII 混排 —— 这一行同时验证「三字节 UTF-8 解码 + 双宽度排版」。
    video::println("MorionOS 微内核 · 中文渲染就绪：汉字、全角标点、双宽度混排。");
    video::println("");

    // 交给调度器。首次切换在中断关闭下进行, 避免 enable 与首次调度之间
    // 的竞态 (否则定时器中断会在 run 完成前触发 schedule 抢走主执行流)。
    scheduler::run();
}

/// 供 panic 处理器使用的免分配格式化输出 (直接写视频/串口)。
#[cfg(target_os = "none")]
struct VidWriter;
#[cfg(target_os = "none")]
impl core::fmt::Write for VidWriter {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        video::print(s);
        Ok(())
    }
}

#[cfg(target_os = "none")]
#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    if video::ready() {
        video::clear(0x000033);
        video::set_cursor(2, 2);
        video::println("KERNEL PANIC");
        // 打印 panic 位置与消息, 便于定位崩溃点 (黑匣子)。
        // 注意: 这里不用 format! (依赖堆), 否则堆一旦异常会造成 panic 递归。
        use core::fmt::Write;
        let mut w = VidWriter;
        if let Some(loc) = info.location() {
            let _ = writeln!(w, "  at {}:{}:{}", loc.file(), loc.line(), loc.column());
        }
        let _ = writeln!(w, "  msg: {}", info.message());
    }
    morion_kernel::halt();
}

// 仅用于 rust-analyzer 在 host 目标上检查时满足 [[bin]] 的 main 要求
// 实际内核编译时 (target_os = "none") 此函数被排除
#[cfg(not(target_os = "none"))]
fn main() {}
