//! Morion OS 微内核入口点
//!
//! 由引导器 (UEFI) 在长模式下跳转到此, 物理地址 0x100000。

#![no_std]
#![no_main]

use morion_kernel::{arch, bootinfo, cap, domain, ipc, memory, scheduler, video};

extern crate alloc;
use alloc::boxed::Box;
use alloc::vec::Vec;

use x86_64::instructions::interrupts;

// 链接脚本 (.stack 段) 导出的内核栈顶
extern "C" {
    static _stack_end: u8;
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
    video::println("[OK] PIC remapped + PIT timer started (100 Hz)");

    // ============================================================
    //  阶段八: 能力系统 (无能力即不可访问)
    // ============================================================
    video::println("");
    video::println("Stage 8: Capability system (least privilege)");
    scheduler::init();
    let domain_prod = domain::create();   // 域 0: 生产者
    let domain_cons = domain::create();   // 域 1: 消费者
    let domain_cap = domain::create();    // 域 2: 能力管理器
    ipc::init(3);
    cap::init(3);
    video::println("[OK] 3 domains + capability tables initialized");
    // 最小权限: 域 0 初始不持有 SendTo(1) 能力。
    scheduler::spawn(task_idle, domain_prod);        // 域 0: 空闲兜底
    scheduler::spawn(task_producer, domain_prod);    // 域 0: 生产者
    scheduler::spawn(task_consumer, domain_cons);    // 域 1: 消费者
    scheduler::spawn(task_cap_manager, domain_cap);  // 域 2: 能力管理器
    video::println("[OK] producer(0) consumer(1) cap-manager(2)");
    video::println("");
    video::println("Producer tries send without capability first (denied),");
    video::println("then cap-manager grants SendTo(1) after about 1s (succeeds).");
    video::println("");

    // 交给调度器。首次切换在中断关闭下进行, 避免 enable 与首次调度之间
    // 的竞态 (否则定时器中断会在 run 完成前触发 schedule 抢走主执行流)。
    scheduler::run();
}

/// 空闲任务 (域 0): 当其他任务都阻塞/睡眠时兜底运行, 停机等待中断。
extern "C" fn task_idle() {
    // 首次进入时中断仍关闭 (run 未开启中断), 在此开启; 之后由中断帧恢复
    interrupts::enable();
    loop {
        x86_64::instructions::hlt();
    }
}

/// 生产者 (域 0): 每 200ms 尝试发送到域 1, 无能力时被内核拒绝。
extern "C" fn task_producer() {
    // 首次进入时中断仍关闭 (run 未开启中断), 在此开启; 之后由中断帧恢复
    interrupts::enable();
    let mut seq = 0u64;
    loop {
        let payload = [0u8; ipc::PAYLOAD_LEN];
        let ok = ipc::send(1, seq, &payload);
        video::print("[send] #");
        video::print_u64(seq);
        if ok {
            video::println("");
        } else {
            video::println(" denied (no capability)");
        }
        seq += 1;
        scheduler::sleep(200);
    }
}

/// 消费者 (域 1): 阻塞接收消息并打印。
extern "C" fn task_consumer() {
    // 首次进入时中断仍关闭 (run 未开启中断), 在此开启; 之后由中断帧恢复
    interrupts::enable();
    loop {
        let msg = ipc::receive();
        video::print("[recv] #");
        video::print_u64(msg.tag);
        video::print(" from domain ");
        video::print_u64(msg.from);
        video::println("");
    }
}

/// 能力管理器 (域 2): 运行约 1 秒后向域 0 授予 SendTo(1) 能力。
extern "C" fn task_cap_manager() {
    // 首次进入时中断仍关闭 (run 未开启中断), 在此开启; 之后由中断帧恢复
    interrupts::enable();
    scheduler::sleep(1000);
    let ok = cap::grant(0, cap::Capability::SendTo(1));
    if ok {
        video::println("[cap] granted SendTo(1) to domain 0");
    } else {
        video::println("[cap] grant failed (table full?)");
    }
    loop {
        x86_64::instructions::hlt();
    }
}

#[cfg(target_os = "none")]
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    if video::ready() {
        video::clear(0x000033);
        video::set_cursor(20, 20);
        video::println("KERNEL PANIC");
        video::println("An unrecoverable error occurred.");
    }
    morion_kernel::halt();
}

// 仅用于 rust-analyzer 在 host 目标上检查时满足 [[bin]] 的 main 要求
// 实际内核编译时 (target_os = "none") 此函数被排除
#[cfg(not(target_os = "none"))]
fn main() {}
