//! 任务上下文 (callee-saved 寄存器 + 栈指针) 与底层切换汇编
//!
//! 遵循 System V AMD64 ABI: 上下文切换只需保存/恢复被调用者保存的寄存器
//! (rbp, rbx, r12..r15) 与栈指针 rsp。其余 caller-saved 寄存器由编译器在
//! 调用 `switch` 前自行保存到栈上, 无需在此处理。
//!
//! 任务入口地址 (rip) 作为"返回地址"预置在任务栈上, 首次切换时通过 `ret` 跳入。

use core::arch::global_asm;

/// 任务上下文。仅保存栈指针, 其余寄存器由 `switch` 在栈上保存/恢复。
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct TaskContext {
    pub rsp: u64,
}

impl TaskContext {
    /// 空上下文 (首次切换时作为占位, 不会再次被调度)。
    pub const fn empty() -> Self {
        Self { rsp: 0 }
    }

    /// 为任务入口函数构造初始上下文。
    ///
    /// 栈布局 (从低地址 rsp 到高地址), 与 `switch` 的 `pop`/`ret` 顺序严格对应:
    ///   r15, r14, r13, r12, rbx, rbp, [rip=entry], [padding]
    ///
    /// `padding` 保证首次通过 `ret` 跳入入口函数时 rsp 满足 ABI 的 16 字节对齐。
    ///
    /// # Safety
    /// `stack` 必须是任务独占的内核栈, 且生命周期覆盖任务运行期。
    pub unsafe fn from_entry(entry: extern "C" fn(), stack: &mut [u8]) -> Self {
        let top = (stack.as_ptr() as u64 + stack.len() as u64) & !0xF; // 16 字节对齐
        let mut sp = top as *mut u64;

        unsafe {
            sp = sp.sub(1);
            sp.write(0); // padding (未使用, 仅供对齐)
            sp = sp.sub(1);
            sp.write(entry as usize as u64); // rip (任务入口)
            sp = sp.sub(1);
            sp.write(0); // rbp
            sp = sp.sub(1);
            sp.write(0); // rbx
            sp = sp.sub(1);
            sp.write(0); // r12
            sp = sp.sub(1);
            sp.write(0); // r13
            sp = sp.sub(1);
            sp.write(0); // r14
            sp = sp.sub(1);
            sp.write(0); // r15
        }
        Self { rsp: sp as u64 }
    }
}

// 切换到新任务上下文 (保存 old 到其栈, 恢复 new)。
extern "C" {
    pub fn switch(old: *mut TaskContext, new: *const TaskContext);
}

// x86_64 上下文切换: 保存 callee-saved 寄存器到旧栈, 从新栈恢复。
// 参数: rdi = old (被保存), rsi = new (被恢复)。
global_asm!(
    ".global switch",
    "switch:",
    "    push rbp",
    "    push rbx",
    "    push r12",
    "    push r13",
    "    push r14",
    "    push r15",
    "    mov [rdi], rsp",
    "    mov rsp, [rsi]",
    "    pop r15",
    "    pop r14",
    "    pop r13",
    "    pop r12",
    "    pop rbx",
    "    pop rbp",
    "    ret",
);