//! Morion OS 微内核入口点
//!
//! 由引导器 (UEFI) 在长模式下跳转到此, 物理地址 0x100000。

#![no_std]
#![no_main]

use morion_kernel::{
    arch, bootinfo, cap, device, domain, exec, ipc, memory, net, pager, scheduler, syscall, video,
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
// 阶段十 / E2b + E3b: 引导期加载服务程序 (每个服务一份独立 ELF, 见 user/srv)
// ---------------------------------------------------------------------------
// 服务 ELF **不在内核镜像里**: E2b 时它们由 `include_bytes!` 嵌进内核 (`SERVICE_ELFS` 表),
// E3b 起改由**引导器**从自己所在的 ESP 读入 (`\EFI\morion\services\<name>.elf`), 经
// `BootInfo` 的模块表交给内核 (见 `bootinfo::ServiceModule`) —— 于是内核体积不再随
// 服务数量增长, 服务也能随 ISO 单独更新。
//
// 域号是 ABI: libvfs 写死 `FAT32_DOMAIN=6` 等, shell 直接 `SendTo(5)`, 所以
// "域号 ↔ 程序"的对应必须稳定 —— 引导器的服务表 (`boot/src/main.rs` 的 `SERVICE_FILES`)
// 与本函数建域的顺序 (0..13) 必须一致。

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
        // E1c 取证: 越界 DMA 由用户态驱动发起, 内核没有别的周期钩子 —— 空闲任务是那个观察点。
        arch::iommu::poll_faults();
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
    video::print("[OK] PIC remapped + PIT timer started (");
    video::print_u64(arch::pit::TARGET_FREQ as u64);
    video::println(" Hz)");

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
    //  阶段 4.6: ACPI / IOMMU (DMAR) 探测 —— E1a
    // ============================================================
    video::println("");
    video::println("Stage 4.6: ACPI / IOMMU (DMAR)");
    let dmar = arch::acpi::probe_dmar();
    if !dmar.found {
        // 固件没给 DMAR（如 QEMU 未开 intel-iommu）—— 不是错误：IOMMU 不存在时 DMA 就是直通
        // 物理地址，整条驱动路线照常。
        video::println("[OK] no ACPI DMAR (no IOMMU), VT-d disabled");
    } else {
        video::print("[OK] ACPI DMAR found: len=");
        video::print_u64(dmar.table_len as u64);
        video::print(" aw=");
        video::print_u64(dmar.host_address_width as u64);
        video::print(" drhd=");
        video::print_u64(dmar.drhd_count as u64);
        video::print(" rmrr=");
        video::print_u64(dmar.rmrr_count as u64);
        video::print(" checksum=");
        video::println(if dmar.checksum_ok { "ok" } else { "BAD" });
        if dmar.drhd_count > 0 {
            let d = dmar.first_drhd;
            video::print("[OK]   DRHD[0] base=0x");
            video::print_hex(d.reg_base);
            video::print(" segment=");
            video::print_u64(d.segment as u64);
            video::print(" include_pci_all=");
            video::print(if d.include_all { "yes" } else { "no" });
            video::print(" scopes=");
            video::print_u64(d.scope_count as u64);
            video::println("");
        }
    }

    // ============================================================
    //  阶段 4.7: VT-d DMA 重映射 —— E1b
    // ============================================================
    video::println("");
    video::println("Stage 4.7: VT-d DMA remapping");
    // 每个枚举到的 PCI 功能点都挂 translated + 恒等上下文项 (IOVA == 物理地址), 阶段一内核与
    // 既有驱动照旧按物理地址 DMA; E1c 再把飞地那台设备换成受限 IOVA 窗口。
    arch::iommu::init(&pci_devices);

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
    //  14 = init      (监督者: 巡检被监督的服务域, 实例退出后从盘原地把它拉起来 —— E3c)
    //  15 = gfx_srv   (图形服务: 持帧缓冲并在用户态渲染, 内核终端退役 —— G1)
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
    let init_domain = domain::create();
    let gfx_domain = domain::create();
    // 域 16 — net_srv (网络驱动, 驱动路线 N0): 通用设备授权把 virtio-net 交给它 (N1/N2)。
    let net_domain = domain::create();
    // 域 17 — virtio_blk_srv (块设备驱动, 驱动路线 D3): 同样走通用设备授权, 内核无设备专属逻辑。
    let blk2_domain = domain::create();
    // 域 18 — ahci_srv (SATA/AHCI 只读驱动, 驱动路线 D4): 仍走通用设备授权; 第一版全轮询, 不申请中断。
    let ahci_domain = domain::create();
    // 域 19 — xhci_srv (USB 存储驱动, 驱动路线 D4 续 03c): 仍走通用设备授权; 第一版全轮询
    // (轮询事件环), 不申请中断向量。
    let xhci_domain = domain::create();
    // 域 20 — iso9660_srv (ISO9660 只读文件服务, 03c 续: 安装介质): 无设备, 经 block_srv 卷层
    // 读 CD/安装盘 (把 .iso 当裸块设备), 挂载于 /cdrom。
    let iso9660_domain = domain::create();
    // 域 21 — netstack_srv (用户态网络协议栈, N6): 无设备, 经帧级 IPC 调 net_srv(域 16)
    // 收发以太帧; 对应用提供 UDP socket 并落实端口能力门禁。
    let netstack_domain = domain::create();
    // 域 22 — e1000e_srv (第二台真网卡 Intel 82574L 驱动, N9): 仍走通用设备授权; 第一版全轮询,
    // 不申请中断向量。与 virtio-net 并列, 证明"驱动 ≠ 栈"——同一套帧级 IPC、换硬件模型。
    let e1000e_domain = domain::create();

    // 初始化 IPC 邮箱、能力表与分页器映射 (数量 = 引导域数量)。
    // 用 `BOOT_DOMAINS` 而不是字面量: 这些表按**域 id 下标**访问, 建域数与表长度必须一致,
    // 否则访问新域 (如 17 号 virtio_blk_srv) 会越界 panic。
    let boot_domains = domain::BOOT_DOMAINS as usize;
    ipc::init(boot_domains);
    cap::init(boot_domains);
    net::init();
    pager::init(boot_domains, pager_domain);

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
        init_domain,
        gfx_domain,
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
    // 授权: init (域 14, E3c 监督者) —— 造进程 + 从 FAT32 卷读服务镜像 (共用中转页) +
    // 经 mount_srv 解析路径 + 向 echo 发控制消息。它是唯一持有 `SYS_SPAWN_ELF_AT` 的域。
    cap::grant(init_domain, cap::Capability::Spawn);
    cap::grant(init_domain, cap::Capability::SendTo(fat32_domain));
    cap::grant(init_domain, cap::Capability::MapInto(fat32_domain));
    cap::grant(init_domain, cap::Capability::SendTo(mount_domain));
    cap::grant(init_domain, cap::Capability::SendTo(echo_domain));
    // 授权: gfx_srv (域 15, G1 图形服务) —— 独占帧缓冲: 取几何 (`SYS_FB_INFO`)、
    // 映射整块 (`SYS_FB_MAP`)、宣告接管显示 (`SYS_FB_TAKEOVER`)。这是内核把"屏幕"
    // 交出去的**唯一凭证**; 只有持它者能让内核终端停止写帧缓冲。
    cap::grant(gfx_domain, cap::Capability::Fb);
    // 授权: app / shell 可向 gfx_srv 提交绘图请求 (`SendTo`) 并把表面页共享给它 (`MapInto`)。
    //   - app: G2 自测 GS-1 (共享表面 → blit → 服务端回读校验);
    //   - shell: 为 G3 的文本渲染/控制台铺路。
    cap::grant(app_domain, cap::Capability::SendTo(gfx_domain));
    cap::grant(app_domain, cap::Capability::MapInto(gfx_domain));
    cap::grant(shell_domain, cap::Capability::SendTo(gfx_domain));
    cap::grant(shell_domain, cap::Capability::MapInto(gfx_domain));
    // 授权: app 可直接给 echo 发控制消息 —— E3c 自测 FS-29 里让 echo 退出, 再看 init 重启它。
    cap::grant(app_domain, cap::Capability::SendTo(echo_domain));
    // 授权: **I/O 端口区间** (D0) —— 端口 syscall 此前是**无门禁**的 (任何域都能读写任意端口),
    // 现按"半开区间"授权, 只有真正需要的域拿到自己那一段:
    //   - block_srv: IDE PIO 数据/命令寄存器 0x1F0..0x1F8 (NVMe 不可用时的回退路径);
    //   - mfs_srv / exfat_srv: CMOS RTC 索引/数据口 0x70/0x71 (写节点时间戳)。
    cap::grant(block_domain, cap::Capability::IoPort(0x1F0, 8));
    cap::grant(mfs_domain, cap::Capability::IoPort(0x70, 2));
    cap::grant(exfat_domain, cap::Capability::IoPort(0x70, 2));
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
    video::println("[OK] IPC + capability + pager initialized (21 domains)");

    // 探测 NVMe 控制器并把设备**通用地**授权给 block 域（D1: 通用设备授权）:
    // 内核只负责 BAR 映射 / DMA 分配 / MSI-X / 能力签发, NVMe 的队列排版与协议在驱动里。
    // 找到则授权; 否则降级 (描述页 magic=0), block 回退 IDE PIO。
    match arch::pci::find_nvme(&pci_devices) {
        Some((bus, dev, func, bar0)) => {
            device::grant(device::GrantRequest {
                domain: block_domain,
                bus,
                dev,
                func,
                bar_paddr: bar0,
                // NVMe: BAR0 至少 16 KiB (寄存器 + 门铃 + MSI-X 表/PBA) 取 4 页;
                // DMA 7 页 (admin ASQ/ACQ + 两条 I/O 队列的 SQ/CQ + data 页); 3 条完成队列向量。
                bar_pages: 4,
                dma_pages: 7,
                msix_vectors: 3,
                label: "nvme",
            });
            video::print("[OK] NVMe controller BAR0=0x");
            video::print_hex(bar0);
            video::println("");
        }
        None => {
            device::grant_empty(block_domain);
            video::println("[OK] no NVMe controller, block falls back to IDE PIO");
        }
    }

    // 探测 virtio 网卡并**通用地**授权给 net 域（驱动路线 N1: PCI 查找 + 设备声明）:
    // 与 NVMe 走同一条 `device::grant` 路径 —— 加这台新设备没给内核加任何设备专属逻辑。
    // virtio-modern 的配置结构在 BAR4（MSI-X 表在 BAR1, 见 N2）。
    match arch::pci::find_net(&pci_devices) {
        Some((bus, dev, func, bar4)) => {
            device::grant(device::GrantRequest {
                domain: net_domain,
                bus,
                dev,
                func,
                bar_paddr: bar4,
                // virtio-modern 配置区 (common/notify/device/isr) 共 16 KiB -> 4 页;
                // DMA 8 页 (RX/TX virtqueue 环 + 收包缓冲)。
                bar_pages: 4,
                dma_pages: 8,
                msix_vectors: 2,
                label: "net",
            });
            video::print("[OK] virtio-net modern BAR4=0x");
            video::print_hex(bar4);
            video::println("");
        }
        None => {
            device::grant_empty(net_domain);
            video::println("[OK] no virtio-net controller, net_srv idle");
        }
    }

    // 探测 Intel e1000e (82574L) 网卡并授权给域 22（N9: 第二台真网卡, 仍走通用设备授权）。
    // 与 virtio-net 完全不同的硬件模型: BAR0 是 MMIO 寄存器窗口 (控制/状态 + RX/TX 描述符环),
    // 无 virtio 能力链表。第一版**全轮询**（本内核只有 MSI-X 通路, e1000e 常规用 INTx/MSI),
    // 故不申请向量。BAR0 需覆盖到 RAL/RAH (0x5400+), 取 8 页; DMA 8 页: RX/TX 环 + 收包缓冲。
    match arch::pci::find_e1000e(&pci_devices) {
        Some((bus, dev, func, bar0)) => {
            device::grant(device::GrantRequest {
                domain: e1000e_domain,
                bus,
                dev,
                func,
                bar_paddr: bar0,
                bar_pages: 8,
                dma_pages: 8,
                msix_vectors: 0,
                label: "e1000e",
            });
            video::print("[OK] e1000e BAR0=0x");
            video::print_hex(bar0);
            video::println("");
        }
        None => {
            device::grant_empty(e1000e_domain);
            video::println("[OK] no e1000e controller, e1000e_srv idle");
        }
    }

    // 探测 virtio-blk 并通用地授权给域 17（驱动路线 D3: 第二个真实驱动, 仍不改内核设备逻辑）。
    // virtio-blk 的 modern 配置同样在 BAR4（MSI-X 表在 BAR1）。DMA 8 页: 请求队列环 + 请求/数据缓冲。
    match arch::pci::find_virtio_blk(&pci_devices) {
        Some((bus, dev, func, bar4)) => {
            device::grant(device::GrantRequest {
                domain: blk2_domain,
                bus,
                dev,
                func,
                bar_paddr: bar4,
                bar_pages: 4,
                dma_pages: 8,
                msix_vectors: 2,
                label: "vblk",
            });
            video::print("[OK] virtio-blk modern BAR4=0x");
            video::print_hex(bar4);
            video::println("");
        }
        None => {
            device::grant_empty(blk2_domain);
            video::println("[OK] no virtio-blk controller, virtio_blk_srv idle");
        }
    }

    // 探测 AHCI/SATA 控制器并通用地授权给域 18（驱动路线 D4: 真机存储驱动第一版, 只读）。
    // AHCI 的寄存器窗口在 BAR5 (ABAR, 8 KiB → 2 页); DMA 6 页: 命令列表 + Received FIS +
    // 命令表 + 数据缓冲。**第一版全轮询** —— 本仓库只有 MSI-X 通路, 而 AHCI 常态用 INTx/MSI,
    // 故不申请向量 (msix_vectors=0), `irq_cmds == cmds` 判据保持不变。
    match arch::pci::find_ahci(&pci_devices) {
        Some((bus, dev, func, bar5)) => {
            device::grant(device::GrantRequest {
                domain: ahci_domain,
                bus,
                dev,
                func,
                bar_paddr: bar5,
                bar_pages: 2,
                dma_pages: 6,
                msix_vectors: 0,
                label: "ahci",
            });
            video::print("[OK] AHCI controller ABAR=0x");
            video::print_hex(bar5);
            video::println("");
        }
        None => {
            device::grant_empty(ahci_domain);
            video::println("[OK] no AHCI controller, ahci_srv idle");
        }
    }

    // 授权 (03b): AHCI 盘经 block_srv 的**卷层**对外提供 —— ahci_srv 启动后异步把盘挂进
    // block_srv (SendTo(block)), block_srv 分配传输暂存页并同址共享给 ahci_srv
    // (MapInto(ahci)), 之后把读/写经 IPC 转发回 ahci_srv (SendTo(ahci))。
    cap::grant(block_domain, cap::Capability::SendTo(ahci_domain));
    cap::grant(block_domain, cap::Capability::MapInto(ahci_domain));
    cap::grant(ahci_domain, cap::Capability::SendTo(block_domain));

    // 探测 xHCI (USB 3.x) 控制器并通用地授权给域 19（驱动路线 03c: USB 存储, 第一版只读
    // bring-up）。BAR0 是 64 位 MMIO 寄存器窗口 (约 4 页, 含端口寄存器区); DMA 16 页:
    // DCBAA / 命令环 / 事件环 / ERST / 输入·设备上下文 / EP0+两条 Bulk 传输环 / 数据页 /
    // 命令表。**第一版全轮询** —— 轮询事件环, 不申请向量 (msix_vectors=0), 判据不变。
    match arch::pci::find_xhci(&pci_devices) {
        Some((bus, dev, func, bar0)) => {
            device::grant(device::GrantRequest {
                domain: xhci_domain,
                bus,
                dev,
                func,
                bar_paddr: bar0,
                bar_pages: 4,
                dma_pages: 16,
                msix_vectors: 0,
                label: "xhci",
            });
            video::print("[OK] xHCI controller BAR0=0x");
            video::print_hex(bar0);
            video::println("");
        }
        None => {
            device::grant_empty(xhci_domain);
            video::println("[OK] no xHCI controller, xhci_srv idle");
        }
    }

    // 授权 (03c): USB 盘经 block_srv 的**卷层**对外提供 —— 同 03b 的 AHCI 模式：
    // xhci_srv 异步把盘挂进 block_srv (SendTo(block))，block_srv 分配传输暂存页并同址共享给
    // xhci_srv (MapInto(xhci))，之后把读/写经 IPC 转发回 xhci_srv (SendTo(xhci))。
    cap::grant(block_domain, cap::Capability::SendTo(xhci_domain));
    cap::grant(block_domain, cap::Capability::MapInto(xhci_domain));
    cap::grant(xhci_domain, cap::Capability::SendTo(block_domain));

    // 授权 (03c 续): ISO9660 只读文件服务 (域 20) 经 IPC 调 block_srv 读安装盘, 并把
    // 缓冲页共享给它 (与 ext2_srv 同款: 只需 iso9660 → SendTo/MapInto block)。
    cap::grant(iso9660_domain, cap::Capability::SendTo(block_domain));
    cap::grant(iso9660_domain, cap::Capability::MapInto(block_domain));

    // 授权 (N6): 网络协议栈 (域 21) 经帧级 IPC 调 net_srv (域 16) 收发帧, 并把收发帧的
    // 共享页映射进 net_srv (SendTo + MapInto)。回复方向不需要能力 (ipc::reply 直接投递)。
    cap::grant(netstack_domain, cap::Capability::SendTo(net_domain));
    cap::grant(netstack_domain, cap::Capability::MapInto(net_domain));

    // 授权 (N6.6): app(域 7) 用网络 —— 可向 netstack 发请求并共享负载页, 且持一条 Net
    // 端口能力 (自测端口 12345; 绑其它端口会被内核拒 —— 越权取证的负例)。
    cap::grant(app_domain, cap::Capability::SendTo(netstack_domain));
    cap::grant(app_domain, cap::Capability::MapInto(netstack_domain));
    cap::grant(app_domain, cap::Capability::Net(12345, 12350));

    // 授权 (N9): 网络协议栈 (域 21) 可把 e1000e (域 22) 当作第二台网卡 —— 帧级 IPC 走同一个
    // `NetReq` 契约 (SendTo + MapInto)。协议栈因此能按网卡索引选出口, 上层 socket API 不变。
    cap::grant(netstack_domain, cap::Capability::SendTo(e1000e_domain));
    cap::grant(netstack_domain, cap::Capability::MapInto(e1000e_domain));

    // 逐个加载服务 ELF 并起任务 (E3b: 镜像来自引导器交来的**模块表** —— 引导器已把它们
    // 读进 `LOADER_DATA` 页, 那些帧不在内核帧分配器的空闲池里, 故生命周期与内核一致)。
    // 域号已按 0..13 建好, 故直接按模块表里的域号载入 —— 每个服务跑自己的 ELF、进自己的
    // 地址空间 (E2b), 内核镜像里不再有它们的副本 (E3b)。
    match info.service_modules() {
        Some(modules) => {
            let mut loaded = 0u64;
            for m in modules {
                // 镜像以物理地址给出, 内核靠恒等映射读它; 超出覆盖范围只能判失败。
                if !memory::paging::is_identity_mapped(m.addr, m.len) {
                    video::println("[FAILED] service ELF outside identity map (4 GiB)");
                    continue;
                }
                let image =
                    unsafe { core::slice::from_raw_parts(m.addr as *const u8, m.len as usize) };
                if exec::spawn_elf_at(m.domain, image) {
                    loaded += 1;
                } else {
                    video::println("[FAILED] service ELF load (boot module)");
                }
            }
            video::print("[OK] ");
            video::print_u64(loaded);
            video::println(" service ELFs loaded (boot modules)");
        }
        None => {
            video::println("[FAILED] bootloader provided no service modules");
        }
    }

    // 空闲任务兜底 (归属 sender 域)。
    scheduler::spawn(task_idle, sender_domain);
    video::println("[OK] 16 service tasks + idle task spawned");
    video::println("");
    // 启动 LOGO (日志末尾, shell 提示符之前)。
    video::print_logo();
    video::println("");
    // 接管前的窗口自检: 内核终端现在只认 ASCII, 这一行里的汉字会画成**豆腐块**
    // (串口里仍是原样 UTF-8)。它验证的仍是「三字节 UTF-8 解码 + 双宽度排版」——
    // 豆腐块占 2 列, 列数与用户态 gfx_srv 的口径一致。
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
