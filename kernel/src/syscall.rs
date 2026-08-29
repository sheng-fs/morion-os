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
#[no_mangle]
extern "C" fn syscall_dispatch(num: u64, a1: u64, a2: u64, _a3: u64) -> u64 {
    match num {
        SYS_YIELD => {
            crate::scheduler::yield_now();
            0
        }
        SYS_SLEEP => {
            crate::scheduler::sleep(a1);
            0
        }
        SYS_SEND => crate::ipc::send(a1, a2, &[]) as u64,
        SYS_RECV => crate::ipc::receive().tag,
        SYS_PUTS => {
            // 从用户地址空间读取字符串并打印 (当前 CR3 即用户域, 可直接访问)。
            // 用 print 而非 println: 换行由用户态通过发送 "\n" 自行控制。
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
