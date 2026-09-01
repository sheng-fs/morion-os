//! 系统调用机制 (syscall / sysret) — 阶段 9 用户态运行模型
//!
//! 提供从 Ring 3 用户态进入内核的最小接口:
//!   - `init()` 配置 MSR (EFER.SCE / STAR / LSTAR / SFMASK)
//!   - `syscall_entry` 汇编入口: 保存用户上下文 → 切内核栈 → 分发 → 返回
//!   - `switch_to_user` 汇编: 构造中断返回帧, 首次切换到 Ring 3
//!
//! 系统调用 ABI (与 System V 对齐):
//!   - 编号在 `rax`, 参数在 `rdi, rsi, rdx`, 返回值在 `rax`。

use core::arch::global_asm;

use x86_64::instructions::port::Port;
use x86_64::registers::model_specific::{Efer, EferFlags, LStar, SFMask, Star};
use x86_64::registers::rflags::RFlags;
use x86_64::structures::gdt::SegmentSelector;
use x86_64::{PrivilegeLevel, VirtAddr};

// ---------------------------------------------------------------------------
// 系统调用编号
// ---------------------------------------------------------------------------
pub const SYS_YIELD: u64 = 0;
pub const SYS_SLEEP: u64 = 1;
pub const SYS_SEND: u64 = 2;
pub const SYS_RECV: u64 = 3;
pub const SYS_PUTS: u64 = 4;
pub const SYS_EXIT: u64 = 5;
pub const SYS_ALLOC_PAGE: u64 = 6;
pub const SYS_SHARE_PAGE: u64 = 7;
pub const SYS_UNMAP: u64 = 8;
pub const SYS_MAP_ANON: u64 = 9;
pub const SYS_PAGE_FAULT_REPLY: u64 = 10;
pub const SYS_CALL: u64 = 12;
pub const SYS_REPLY: u64 = 13;
pub const SYS_REGISTER_IRQ: u64 = 14;
pub const SYS_SCROLL_UP: u64 = 15;
pub const SYS_SCROLL_DOWN: u64 = 16;
pub const SYS_BACKSPACE: u64 = 17;
pub const SYS_TERM_PUT: u64 = 18;
pub const SYS_TERM_LEFT: u64 = 19;
pub const SYS_TERM_RIGHT: u64 = 20;
pub const SYS_MAP_MMIO: u64 = 21;
pub const SYS_PORT_IN8: u64 = 22;
pub const SYS_PORT_IN16: u64 = 23;
pub const SYS_PORT_OUT8: u64 = 24;
pub const SYS_PORT_OUT16: u64 = 25;

/// 当前任务的内核栈顶 — 由调度器在切换任务时更新, `syscall_entry` 汇编读取。
#[no_mangle]
static mut CURRENT_KERNEL_STACK_TOP: u64 = 0;

/// 更新当前任务的内核栈顶 (调度器调用)。
pub fn set_current_kernel_stack_top(top: u64) {
    unsafe { CURRENT_KERNEL_STACK_TOP = top; }
}

// ---------------------------------------------------------------------------
// 汇编: syscall 入口
// ---------------------------------------------------------------------------
// 进入时 (硬件已做): rcx = user rip, r11 = user rflags, rsp = user rsp,
//                   rax = 编号, rdi/rsi/rdx = 参数。
// 返回前 (硬件将做): sysretq 用 rcx → rip, r11 → rflags, 并切回用户段。
global_asm!(
    ".global syscall_entry",
    "syscall_entry:",
    // r10 暂存 user rsp, 再切换到当前任务的内核栈顶。
    // 不能用 rbx: rbx 是 callee-saved, 用户态依赖其跨 syscall 不变;
    // r10 是 caller-saved 且 syscall ABI 不占用, 用户态封装已声明 clobber。
    "  mov r10, rsp",
    "  mov rsp, [CURRENT_KERNEL_STACK_TOP]",
    // 保存 user 上下文 (rflags/rip) 与 callee-saved 寄存器。
    "  push r11",
    "  push rcx",
    "  push rbp",
    "  push rbx", // user rbx (原样保留)
    "  push r10", // user rsp
    "  push r12",
    "  push r13",
    "  push r14",
    "  push r15",
    // 参数搬移: (rax, rdi, rsi, rdx) → (rdi, rsi, rdx, rcx)。
    "  mov rcx, rdx",
    "  mov rdx, rsi",
    "  mov rsi, rdi",
    "  mov rdi, rax",
    "  call syscall_dispatch",
    // 返回值在 rax, 恢复寄存器。
    "  pop r15",
    "  pop r14",
    "  pop r13",
    "  pop r12",
    "  pop r10", // user rsp
    "  pop rbx", // user rbx
    "  pop rbp",
    "  pop rcx", // user rip
    "  pop r11", // user rflags
    "  mov rsp, r10",
    "  sysretq",
);

// ---------------------------------------------------------------------------
// 汇编: 首次切换到 Ring 3
// ---------------------------------------------------------------------------
// rdi = 用户入口 (rip), rsi = 用户栈顶 (rsp), rdx = 用户参数 (作为 _start 的
// 第一个参数, 经 rdi 传入; 用于向用户程序传递其所属域 id 等信息)。
global_asm!(
    ".global switch_to_user",
    "switch_to_user:",
    // 选择子须与 gdt.rs 的 USER_DATA_SEL_RPL3 / USER_CODE_SEL_RPL3 一致。
    "  push 0x1B",  // SS  (user data, RPL3)
    "  push rsi",   // RSP (user stack top)
    "  push 0x202", // RFLAGS (bit1 保留位 + IF=1)
    "  push 0x23",  // CS  (user code, RPL3)
    "  push rdi",   // RIP (user entry)
    "  mov rdi, rdx", // 把用户参数放入 rdi (SysV 第一个参数), 供 _start 读取
    "  iretq",
);

extern "C" {
    fn syscall_entry();
    pub fn switch_to_user(entry: u64, stack_top: u64, arg: u64) -> !;
}

// ---------------------------------------------------------------------------
// 系统调用分发
// ---------------------------------------------------------------------------
/// 从用户地址 `ptr` 读取一条固定大小 IPC payload (32 字节)。
///
/// `ptr` 为 0 表示无 payload (返回全零); 非用户空间地址同样拒绝 (返回全零),
/// 避免 syscall 在 Ring0 下读取内核内存 (与 SYS_ALLOC_PAGE 等信任边界一致)。
fn read_user_payload(ptr: u64) -> [u8; crate::ipc::PAYLOAD_LEN] {
    if ptr == 0 || !crate::memory::paging::is_user_address(ptr) {
        return [0; crate::ipc::PAYLOAD_LEN];
    }
    unsafe {
        let src = core::slice::from_raw_parts(ptr as *const u8, crate::ipc::PAYLOAD_LEN);
        let mut buf = [0u8; crate::ipc::PAYLOAD_LEN];
        buf.copy_from_slice(src);
        buf
    }
}

#[no_mangle]
extern "C" fn syscall_dispatch(num: u64, a1: u64, a2: u64, a3: u64) -> u64 {
    match num {
        SYS_YIELD => {
            crate::scheduler::yield_now();
            0
        }
        SYS_SLEEP => {
            crate::scheduler::sleep(a1);
            0
        }
        SYS_SEND => {
            let payload = read_user_payload(a3);
            crate::ipc::send(a1, a2, &payload) as u64
        }
        SYS_RECV => {
            // 阻塞接收一条消息; 若 `a1` 非零, 把完整消息写回用户缓冲区,
            // 返回消息 tag。这样分页器等可通过 payload 读取缺页信息。
            let msg = crate::ipc::receive();
            if a1 != 0 {
                // 拒绝向内核地址写入, 防止用户态覆盖内核内存。
                if !crate::memory::paging::is_user_address(a1) {
                    return 0;
                }
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        &msg as *const crate::ipc::Message as *const u8,
                        a1 as *mut u8,
                        core::mem::size_of::<crate::ipc::Message>(),
                    );
                }
            }
            msg.tag
        }
        SYS_CALL => {
            // 同步调用: 发送请求到 `a1` (to) 并阻塞等待回复, 返回回复 tag。
            // `a3` 为可选 payload 指针 (0 表示无 payload)。
            // 失败 (无 SendTo 能力) 时返回 u64::MAX。
            let payload = read_user_payload(a3);
            crate::ipc::call(a1, a2, &payload).tag
        }
        SYS_REPLY => {
            // 回复当前任务最近 `receive` 到的调用者, tag 为 `a1`。
            crate::ipc::reply(a1, &[]) as u64
        }
        SYS_ALLOC_PAGE => {
            // 分配一个物理帧并映射到当前域的 `vaddr` (a1), 引用计数置 1。
            // 先校验 a1 为用户空间地址, 拒绝内核地址被解析/重映射 (见 paging::is_user_address)。
            if !crate::memory::paging::is_user_address(a1) {
                return 0;
            }
            let paddr = match crate::memory::frame_allocator::allocate_frame() {
                Some(p) => p,
                None => return 0,
            };
            let domain = crate::scheduler::current_domain();
            crate::memory::paging::map_user_page(domain, a1, paddr);
            if !crate::memory::frame_allocator::inc_ref(paddr) {
                // 引用计数表已满: 回滚映射并释放帧, 避免内存泄漏。
                crate::memory::paging::unmap_user_page(domain, a1);
                crate::memory::frame_allocator::free_frame(paddr);
                return 0;
            }
            1
        }
        SYS_SHARE_PAGE => {
            // 把当前域 `vaddr` (a1) 的页共享映射进 `a2` 域同一地址, 需 MapInto 能力。
            let from = crate::scheduler::current_domain();
            if !crate::cap::has(from, crate::cap::Capability::MapInto(a2)) {
                0
            } else if !crate::memory::paging::is_user_address(a1) {
                // 拒绝内核地址被 resolve_user_page 反查后重映射。
                0
            } else {
                match crate::memory::paging::resolve_user_page(from, a1) {
                    Some(paddr) => {
                        crate::memory::paging::map_user_page(a2, a1, paddr);
                        if !crate::memory::frame_allocator::inc_ref(paddr) {
                            // 引用计数表已满: 回滚映射。
                            crate::memory::paging::unmap_user_page(a2, a1);
                            return 0;
                        }
                        1
                    }
                    None => 0,
                }
            }
        }
        SYS_UNMAP => {
            // 解除当前域 `vaddr` (a1) 的映射, 引用计数递减, 归零时释放帧。
            let domain = crate::scheduler::current_domain();
            match crate::memory::paging::unmap_user_page(domain, a1) {
                Some(paddr) => {
                    if crate::memory::frame_allocator::dec_ref(paddr) {
                        crate::memory::frame_allocator::free_frame(paddr);
                    }
                    1
                }
                None => 0,
            }
        }
        SYS_MAP_ANON => {
            // 分页器: 给指定域 `a1` 的 `a2` (vaddr) 映射一个匿名零帧, 需 MapInto 能力。
            let from = crate::scheduler::current_domain();
            if !crate::cap::has(from, crate::cap::Capability::MapInto(a1)) {
                0
            } else if !crate::memory::paging::is_user_address(a2) {
                // 拒绝把内核地址 (0 / 恒等映射 / 内核堆等) 作为缺页目标映射,
                // 否则会在 2 MiB 大页上映射 4 KiB 页, 触发 ParentEntryHugePage panic。
                0
            } else {
                match crate::memory::frame_allocator::allocate_frame() {
                    Some(p) => {
                        // 匿名帧必须清零: 分配器不保证新帧内容为 0, 若不清理,
                        // 用户态读到的会是上一任占用者释放后残留的数据。
                        // 物理地址 < 4 GiB, 位于恒等映射内, 可直接按虚拟地址写。
                        unsafe {
                            core::ptr::write_bytes(
                                p as *mut u8,
                                0,
                                crate::memory::frame_allocator::FRAME_SIZE,
                            );
                        }
                        crate::memory::paging::map_user_page(a1, a2, p);
                        if !crate::memory::frame_allocator::inc_ref(p) {
                            // 引用计数表已满: 回滚映射并释放帧。
                            crate::memory::paging::unmap_user_page(a1, a2);
                            crate::memory::frame_allocator::free_frame(p);
                            return 0;
                        }
                        1
                    }
                    None => 0,
                }
            }
        }
        SYS_PAGE_FAULT_REPLY => {
            // 分页器回复: 唤醒因缺页阻塞的域。回复目标由 `receive` 记录
            // (即缺页消息的 from 域), 无需分页器显式传入域 id。
            let target = crate::scheduler::current_reply_target();
            if target != u64::MAX {
                crate::scheduler::wake_one(target);
                1
            } else {
                0
            }
        }
        SYS_REGISTER_IRQ => {
            // 注册当前域接收 `a1` (IRQ), 需持有 `Capability::Irq(irq)`。
            let domain = crate::scheduler::current_domain();
            let irq = a1 as u8;
            if crate::cap::has(domain, crate::cap::Capability::Irq(irq)) {
                crate::irq::register(irq, domain);
                1
            } else {
                0
            }
        }
        SYS_SCROLL_UP => {
            crate::video::scroll_view_up();
            1
        }
        SYS_SCROLL_DOWN => {
            crate::video::scroll_view_down();
            1
        }
        SYS_BACKSPACE => {
            crate::video::term_backspace();
            1
        }
        SYS_TERM_PUT => {
            crate::video::term_put(a1 as u8);
            1
        }
        SYS_TERM_LEFT => {
            crate::video::term_left();
            1
        }
        SYS_TERM_RIGHT => {
            crate::video::term_right();
            1
        }
        SYS_MAP_MMIO => {
            // 把物理 MMIO 页 (a1, 页对齐) 映射到当前域 a2 虚拟地址, 需 Mmio 能力。
            let domain = crate::scheduler::current_domain();
            let bar = a1 & !0xFFF;
            if crate::cap::has(domain, crate::cap::Capability::Mmio(bar))
                && crate::memory::paging::is_user_address(a2)
            {
                crate::memory::paging::map_mmio(domain, a2, bar);
                1
            } else {
                0
            }
        }
        SYS_PORT_IN8 => {
            // 从 I/O 端口 a1 读一个字节 (供用户态设备驱动, 如 IDE PIO)。
            // 须持有 Capability::IoPort(port)。
            let port = a1 as u16;
            let domain = crate::scheduler::current_domain();
            if crate::cap::has(domain, crate::cap::Capability::IoPort(port)) {
                unsafe { Port::<u8>::new(port).read() as u64 }
            } else {
                0
            }
        }
        SYS_PORT_IN16 => {
            let port = a1 as u16;
            let domain = crate::scheduler::current_domain();
            if crate::cap::has(domain, crate::cap::Capability::IoPort(port)) {
                unsafe { Port::<u16>::new(port).read() as u64 }
            } else {
                0
            }
        }
        SYS_PORT_OUT8 => {
            let port = a1 as u16;
            let domain = crate::scheduler::current_domain();
            if crate::cap::has(domain, crate::cap::Capability::IoPort(port)) {
                unsafe { Port::<u8>::new(port).write(a2 as u8) };
            }
            0
        }
        SYS_PORT_OUT16 => {
            let port = a1 as u16;
            let domain = crate::scheduler::current_domain();
            if crate::cap::has(domain, crate::cap::Capability::IoPort(port)) {
                unsafe { Port::<u16>::new(port).write(a2 as u16) };
            }
            0
        }
        SYS_PUTS => {
            // 从用户地址空间读取字符串并打印 (当前 CR3 即用户域, 可直接访问)。
            // 用 print 而非 println: 换行由用户态通过发送 "\n" 自行控制。
            // 必须校验地址为用户空间, 防止信息泄露; 长度限制在一页避免无界读取。
            const MAX_PUTS_LEN: u64 = 4096;
            if !crate::memory::paging::is_user_address(a1) || a2 > MAX_PUTS_LEN {
                return 0;
            }
            let slice = unsafe { core::slice::from_raw_parts(a1 as *const u8, a2 as usize) };
            let s = unsafe { core::str::from_utf8_unchecked(slice) };
            crate::video::print(s);
            0
        }
        SYS_EXIT => crate::scheduler::exit_current(),
        _ => 0,
    }
}

// ---------------------------------------------------------------------------
// 初始化: 配置 syscall/sysret 所需 MSR
// ---------------------------------------------------------------------------
pub fn init() {
    // EFER.SCE: 启用 syscall/sysret 指令。
    unsafe {
        Efer::update(|e| e.insert(EferFlags::SYSTEM_CALL_EXTENSIONS));
    }

    // STAR: 指定 syscall (Ring0) 与 sysret (Ring3) 的 CS/SS 段基址。
    Star::write(
        SegmentSelector::new(4, PrivilegeLevel::Ring3), // user code (sysret CS)
        SegmentSelector::new(3, PrivilegeLevel::Ring3), // user data (sysret SS)
        SegmentSelector::new(1, PrivilegeLevel::Ring0), // kernel code (syscall CS)
        SegmentSelector::new(2, PrivilegeLevel::Ring0), // kernel data (syscall SS)
    )
    .expect("syscall::init: invalid Star selectors");

    // LSTAR: syscall 入口地址。
    LStar::write(VirtAddr::new(syscall_entry as *const () as u64));

    // SFMASK: 进入 syscall 时清除 IF (处理期间关中断, 防止重入)。
    SFMask::write(RFlags::INTERRUPT_FLAG);
}
