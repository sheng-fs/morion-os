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
// 15..=20 与 27 曾用于「内核终端输入行」(历史滚动 / 退格 / 逐键编辑 / 阻塞读一行)。
// G4 把输入搬进用户态屏幕控制台后**退役**, 号段不再分配 —— 新号接在表尾 (48 / 49)。
pub const SYS_MAP_MMIO: u64 = 21;
pub const SYS_PORT_IN8: u64 = 22;
pub const SYS_PORT_IN16: u64 = 23;
pub const SYS_PORT_OUT8: u64 = 24;
pub const SYS_PORT_OUT16: u64 = 25;
pub const SYS_VIRT_TO_PHYS: u64 = 26;
pub const SYS_CLEAR: u64 = 28;
pub const SYS_CAP_ISSUE: u64 = 29;
pub const SYS_CAP_LOOKUP: u64 = 30;
pub const SYS_CAP_DROP: u64 = 31;
/// 「能力随 IPC 传递」: 把自己句柄槽里的对象**移入**目标域 (fd 传递)。
pub const SYS_HANDLE_SEND: u64 = 32;
/// 「能力随 IPC 传递」: 把自己**持有**的能力委派给目标域 (不允许放大)。
pub const SYS_CAP_SEND: u64 = 33;
/// 非阻塞取走**向量掩码**里任意一个 MSI/MSI-X 向量的「待处理」标志 (位 `i` ↔ 向量
/// `idt::MSI_VECTOR_BASE + i`), 命中返回向量号 (阶段 39/40)。
pub const SYS_IRQ_POLL: u64 = 34;
/// 打开 NVMe 控制器的 MSI-X (驱动写好表项后调用; PCI 配置空间写留在内核, 阶段 39)。
pub const SYS_MSIX_ENABLE: u64 = 35;
/// 阻塞等待**向量掩码**里任意一个向量的中断, 最多 `rsi` 毫秒 (返回命中的向量号;
/// 超时返回 0 —— 调用方据此回退轮询)。
pub const SYS_IRQ_WAIT: u64 = 36;
/// 加载可执行文件 (ELF64 `ET_EXEC`) 并启动: `rdi = 镜像首地址, rsi = 长度`,
/// 成功返回**新域 id**, 失败返回 `u64::MAX`。需 `Capability::Spawn`。
///
/// 镜像是用户态给的, 故内核侧做全部校验 (`elf::parse`); 新域零能力, 其分页器登记为调用者。
pub const SYS_SPAWN_ELF: u64 = 37;
/// 销毁一个域并回收它的全部资源: `rdi = 域 id`, 成功返回 1, 失败 0。
///
/// 门禁是**两条一起**: `Capability::Spawn` **且** 目标是自己的分页器
/// (`pager::of(target) == 调用者`) —— 即"谁加载谁负责"。`exec::spawn_elf` 把加载者
/// 登记为分页器, 于是"自我销毁"天然不可达 (拆自己的页表/内核栈会当场崩)。
/// 回收内容见 `domain::destroy`: 地址空间、页表、能力/句柄、邮箱、分页器、中断注册、任务。
pub const SYS_DOMAIN_DESTROY: u64 = 38;
/// 存活域数 (`domain::alive_count`) —— 自测取证用: 销毁之后应回到基线。
pub const SYS_DOMAIN_COUNT: u64 = 39;
/// 当前空闲物理帧数 (`frame_allocator::free_frames`) —— 自测取证用: 反复加载/销毁不应下降。
pub const SYS_FRAME_FREE: u64 = 40;
/// 在**指定域**里加载可执行文件并启动 (E3c 监督者重启服务):
/// `rdi = 域 id, rsi = 镜像首地址, rdx = 长度`, 成功返回该域 id, 失败 `u64::MAX`。
/// 需 `Capability::Spawn`。
///
/// 与 `SYS_SPAWN_ELF` 的区别是**不建新域**: 目标域必须已存在且**没有存活任务**
/// (重启的前提是旧实例已退出), 内核随即原地清空它的用户地址空间
/// ([`crate::domain::reset`]) 再映射新镜像。于是**域号不变** —— 服务域号是 ABI
/// (`libvfs` 里写死了 `FAT32_DOMAIN=6` 等, `init` 是 14), 而分页器登记、能力表这些
/// 按域 index 的东西也不受影响。
pub const SYS_SPAWN_ELF_AT: u64 = 41;
/// 该域**是否还有存活任务**: `rdi = 域 id`, 有返回 1, 没有 (或域已销毁) 返回 0。
///
/// 监督者 (`init`) 的巡检原语: 引导期服务域的槽位永不自动销毁, 所以"域还在"并不能
/// 说明"服务还在跑" —— 要问的是任务。故意不设能力门禁: 它只暴露"某个域号活没活"。
pub const SYS_DOMAIN_ALIVE: u64 = 42;
/// 用**引导模块内存镜像**在指定域里原地重启 (E3c 后续): `rdi = 域 id`, 成功返回该域 id,
/// 失败 `u64::MAX`。需 `Capability::Spawn`。
///
/// 与 `SYS_SPAWN_ELF_AT` 是同一套"原地重启"流程 (验镜像 → 目标域无存活任务 → 清地址空间
/// → 起任务), 区别只在**镜像来源**: 这里由内核按域号去**引导模块表**
/// ([`crate::bootinfo::BootInfo::service_modules`]) 里取 —— 那是引导器读进 `LOADER_DATA`
/// 页的那一份, 与内核同生命周期、**不依赖磁盘**。于是 `init` 能重启文件服务本身
/// (fat32_srv / mfs_srv), 解掉"读盘要靠文件服务、文件服务死了没法自救"的鸡生蛋问题。
pub const SYS_SPAWN_ELF_MODULE: u64 = 43;
/// 取帧缓冲几何 (G1 图形子系统): `rdi = 用户缓冲指针`, 成功写入 [`FbInfo`] 并返回 1, 失败 0。
/// 需 `Capability::Fb`。
///
/// 形如 `SYS_FB_MAP` / `SYS_FB_TAKEOVER` 的入口: 帧缓冲是内核仅存的「全局唯一」输出设备,
/// 交给用户态图形服务独占 —— 故单独一类能力 (`Fb`), 而不是逐页的 `Mmio`。
pub const SYS_FB_INFO: u64 = 44;
/// 把**整块帧缓冲**映射进本域: `rdi = 用户虚拟地址` (页对齐), 成功返回 1, 失败 0。
/// 需 `Capability::Fb`。
///
/// 映射按 4 KiB 页逐页建立 (与 `SYS_MAP_MMIO` 同口径, 非缓存); 若目标区间**已有**映射则
/// 整体拒绝 (不半途映射, 也避免撞内核 `PageAlreadyMapped` panic)。
pub const SYS_FB_MAP: u64 = 45;
/// 宣告本域**接管**显示: 之后内核终端不再写帧缓冲, 输出只保留 COM1 —— 屏幕交给用户态。
/// 成功返回 1。需 `Capability::Fb`。幂等。
pub const SYS_FB_TAKEOVER: u64 = 46;
/// 显示是否已交用户态 (`SYS_FB_TAKEOVER` 曾成功): 返回 1 / 0。**无能力要求**。
///
/// 给「要把输出镜像到用户态屏幕控制台」的客户端用: 接管之前镜像只会把字写进没人看的帧缓冲,
/// 而 `SYS_CALL` 到图形服务要等它进请求循环 —— 先问这一句就不必白等。
pub const SYS_CONSOLE_READY: u64 = 47;
/// 把一个**按键字节**推进内核键队列 (G4): `rdi = 字节` → 1。由用户态键盘域调用。
///
/// 内核**不解释**这个字节 (可打印字符 / 退格 / 回车一视同仁): 行编辑、回显、行历史都在
/// 用户态屏幕控制台。队列满时丢弃该字节 —— 交互输入绝不阻塞内核。
pub const SYS_KEY_PUSH: u64 = 48;
/// **阻塞取**一个按键字节 (G4): `()` → 字节值 (0..=255)。队列空则阻塞, 由 `SYS_KEY_PUSH` 唤醒。
pub const SYS_KEY_READ: u64 = 49;
/// 读**本域被授权设备**的 PCI 配置空间 dword: `rdi = offset` → 该 dword。
///
/// 驱动靠它自行解析能力链表 (PCI 通用能力 / 厂商能力), 内核不必懂设备协议; 只放行
/// "读自己那台设备"。本域没有被授权设备时返回 `u64::MAX`。
pub const SYS_DEVICE_CONFIG_READ: u64 = 50;

// ---------------------------------------------------------------------------
// 并行开发接线层: 号段预留
// ---------------------------------------------------------------------------
// 下面三个号在 `syscall.rs` 里**只占号 + 转发**, 实现各自落在自己的模块里 —— 这样并行推进
// 这三条任务时不会同时改同一个文件 (见 docs/dev-workflow.md 的「并行协作」)。

/// **预留** `SYS_UNAME` (V1 版本串): 实现在 [`crate::version`]。
pub const SYS_UNAME: u64 = 51;
/// **预留** `SYS_DEVICE_INFO` (D1b 运行期设备授权): 实现在 [`crate::device`]。
pub const SYS_DEVICE_INFO: u64 = 52;
/// **预留** `SYS_DEVICE_GRANT` (D1b 运行期设备授权): 实现在 [`crate::device`]。
pub const SYS_DEVICE_GRANT: u64 = 53;
/// 能力审计 (② `SYS_CAP_AUDIT`): `(目标域, 槽号)` → 打包的能力 / `0`(空槽) / `u64::MAX`(越界)。
///
/// 供监督者 (init, 持 `Capability::Spawn`) 按最小权限策略核对引导期的能力授权。编码见
/// [`crate::cap::pack_audit`]; 非 `Spawn` 持有者一律拒绝 (返回 `0`)。
pub const SYS_CAP_AUDIT: u64 = 54;

/// 非阻塞接收 (N6): 邮箱为空立即返回 `u64::MAX`, 否则把完整消息写回 `a1` 并返回 tag。
///
/// 供"既要轮询硬件、又要接 IPC"的驱动/网络服务使用 (见 [`crate::ipc::try_receive`])。
pub const SYS_TRY_RECV: u64 = 55;
/// 绑定网络端口 (N6): `a1` = 端口。调用者须持覆盖该端口的 [`crate::cap::Capability::Net`]
/// 能力, 成功登记归属并返回 1 (见 [`crate::net::bind`])。
pub const SYS_NET_BIND: u64 = 56;
/// 查询网络端口归属域 (N6): `a1` = 端口, 返回归属域号 (`u64::MAX` = 未绑定)。
/// 供 `netstack_srv` 在收到应用的 `bind` 请求时核对 `msg.from` (见 [`crate::net::owner`])。
pub const SYS_NET_OWNER: u64 = 57;
/// 控制台日志累计字节数 (Phase 0 / P0.1): 无参, 返回 [`crate::klog`] 的绝对总量。
/// shell `dmesg` 据此知道要读多少 (环形缓冲的真实长度)。
pub const SYS_LOG_TOTAL: u64 = 58;
/// 读控制台日志 (Phase 0 / P0.1): `a1` = 用户缓冲、`a2` = 长度、`a3` = 起始绝对偏移,
/// 返回写入字节数 (见 [`crate::klog::handle_read`])。
pub const SYS_LOG_READ: u64 = 59;
/// 读一台 PCI 设备记录 (Phase 0 / P0.3, shell `lspci`): `a1` = 索引、`a2` = 用户缓冲
/// (3 个 u64), 返回 1/0 (见 [`crate::arch::pci::syscall_info`])。
pub const SYS_PCI_INFO: u64 = 60;

/// 帧缓冲几何 (`SYS_FB_INFO` 写回用户的布局, 与用户态 `morion::syscall::FbInfo` 严格对应)。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct FbInfo {
    /// 帧缓冲物理基址。
    pub addr: u64,
    /// 宽 (像素)。
    pub width: u32,
    /// 高 (像素)。
    pub height: u32,
    /// 行跨度 (像素)。
    pub stride: u32,
    /// 每像素位数。
    pub bpp: u32,
}

/// `SYS_SPAWN_ELF` 接受的最大镜像长度 (1 MiB)。
///
/// 镜像要先整段放进调用方的地址空间, 这个上限既挡恶意的"巨大长度", 也避免内核
/// 为一个请求遍历过多页; 真正的页数上限在 `exec::MAX_IMAGE_PAGES`。
const MAX_ELF_LEN: u64 = 1024 * 1024;

/// 当前任务的内核栈顶 — 由调度器在切换任务时更新, `syscall_entry` 汇编读取。
#[no_mangle]
static mut CURRENT_KERNEL_STACK_TOP: u64 = 0;

/// 更新当前任务的内核栈顶 (调度器调用)。
pub fn set_current_kernel_stack_top(top: u64) {
    unsafe {
        CURRENT_KERNEL_STACK_TOP = top;
    }
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
    "  push 0x1B",    // SS  (user data, RPL3)
    "  push rsi",     // RSP (user stack top)
    "  push 0x202",   // RFLAGS (bit1 保留位 + IF=1)
    "  push 0x23",    // CS  (user code, RPL3)
    "  push rdi",     // RIP (user entry)
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

/// 校验并取出用户态交来的 ELF 镜像缓冲区 (`SYS_SPAWN_ELF` / `SYS_SPAWN_ELF_AT` 共用)。
///
/// 信任边界: 镜像是用户态给的, 解析与映射全部在核内做 (`elf::parse` 先全量校验), 因此
/// 加载器即使有 bug 也映射不出任意物理帧。用户缓冲按页确认**已映射** —— 内核以调用方的
/// CR3 直接读它, 未映射会在内核态缺页。
fn user_elf_image(ptr: u64, len: u64) -> Option<&'static [u8]> {
    if !crate::memory::paging::is_user_address(ptr) || !(64..=MAX_ELF_LEN).contains(&len) {
        return None;
    }
    let end = ptr.checked_add(len)?;
    if !crate::memory::paging::is_user_address(end - 1) {
        return None;
    }
    let from = crate::scheduler::current_domain();
    let mut page = ptr & !0xFFF;
    while page < end {
        crate::memory::paging::resolve_user_page(from, page)?;
        page += 4096;
    }
    Some(unsafe { core::slice::from_raw_parts(ptr as *const u8, len as usize) })
}

/// 按域号取**引导模块内存镜像** (`SYS_SPAWN_ELF_MODULE` 用): 引导器读进 `LOADER_DATA`
/// 页、经 `BootInfo` 模块表交过来的那一份, 与内核同生命周期、不依赖磁盘。
///
/// 与用户交来的镜像一样要在映射前过 `is_identity_mapped` 校验 (内核靠恒等映射读它);
/// 域号不在表里 (该服务没被引导器打包) 返回 `None`。
fn boot_module_image(domain: u64) -> Option<&'static [u8]> {
    let modules = crate::bootinfo::get().service_modules()?;
    let m = modules.iter().find(|m| m.domain == domain)?;
    if m.len == 0 || !crate::memory::paging::is_identity_mapped(m.addr, m.len) {
        return None;
    }
    Some(unsafe { core::slice::from_raw_parts(m.addr as *const u8, m.len as usize) })
}

/// 「原地重启」的公共流程 (`SYS_SPAWN_ELF_AT` 与 `SYS_SPAWN_ELF_MODULE` 共用):
/// 验镜像 → 目标域须存在且**已无存活任务** → 清空其用户地址空间 → 摘掉已终止任务
/// (含释放内核栈, 否则每轮漏一份、终会占满 `MAX_TASKS`) → 映射新镜像起任务。
/// 成功返回目标域号 (`exec::spawn_elf_at` 保证不换域 —— 域号是 ABI), 失败 `u64::MAX`。
fn restart_in_place(target: u64, image: &[u8]) -> u64 {
    // 先验镜像, 再动域状态: 镜像非法时不该白拆一次地址空间。
    if crate::elf::parse(image).is_none() {
        return u64::MAX;
    }
    if !crate::domain::is_alive(target) || crate::scheduler::live_tasks(target) != 0 {
        return u64::MAX;
    }
    crate::domain::reset(target);
    // 丢掉上一实例邮箱里**未处理**的请求: 它们的 payload 常引用客户端共享过来的页
    // (如 `GFX_OP_TEXT` 的文本页), 而 `domain::reset` 已把那些映射从本域清掉 —— 新实例
    // 再去处理就会读到悬空地址而缺页崩溃。请求方此刻多半也已通过 `ipc::call` 的超时
    // 失败返回并在重试, 故丢弃是安全的。
    crate::ipc::remove_domain(target);
    crate::scheduler::reap_terminated(target);
    if crate::exec::spawn_elf_at(target, image) {
        target
    } else {
        u64::MAX
    }
}

/// 掩码校验: 每一位都必须是本域**注册过的**向量, 且本域持有对应 `Capability::Irq`。
///
/// 掩码编码 (位 `i` ↔ 向量 `idt::MSI_VECTOR_BASE + i`) 与 `SYS_IRQ_POLL`/`SYS_IRQ_WAIT`
/// 一致 —— 校验只认整段掩码: 有不属于自己的位就整体拒绝, 而不是静默少等几个向量。
fn irq_mask_ok(domain: u64, mask: u64) -> bool {
    use crate::arch::idt::{MSI_VECTOR_BASE, MSI_VECTOR_COUNT};
    for i in 0..MSI_VECTOR_COUNT {
        if mask & (1 << i) == 0 {
            continue;
        }
        let vector = MSI_VECTOR_BASE + i;
        if !crate::cap::has(domain, crate::cap::Capability::Irq(vector))
            || !crate::irq::is_registered_by(vector, domain)
        {
            return false;
        }
    }
    true
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
            // 信任边界: 校验写回地址是用户空间, 避免恶意域传入内核地址导致任意内核写。
            let msg_size = core::mem::size_of::<crate::ipc::Message>() as u64;
            if a1 != 0
                && crate::memory::paging::is_user_address(a1)
                && crate::memory::paging::is_user_address(a1 + msg_size - 1)
            {
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        &msg as *const crate::ipc::Message as *const u8,
                        a1 as *mut u8,
                        msg_size as usize,
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
            crate::memory::paging::map_user_page(
                domain,
                a1,
                paddr,
                crate::memory::paging::UserPagePerm::ReadWrite,
            );
            crate::memory::frame_allocator::inc_ref(paddr);
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
                        // 目标域 `a2` 可能已在 `a1` 有映射: 客户端重启后重共享, 或重复
                        // 共享同一页。若不处理, map_user_page 会撞 PageAlreadyMapped panic。
                        if let Some(old) = crate::memory::paging::resolve_user_page(a2, a1) {
                            if old == paddr {
                                // 已映射同一帧: 幂等成功, 不重复 inc_ref。
                                return 1;
                            }
                            // 异帧: 先摘除旧映射并递减引用计数 (归零才释放), 再映射新帧。
                            if let Some(u) = crate::memory::paging::unmap_user_page(a2, a1) {
                                let was_last = crate::memory::frame_allocator::dec_ref(u);
                                if was_last {
                                    crate::memory::frame_allocator::free_frame(u);
                                }
                            }
                        }
                        crate::memory::paging::map_user_page(
                            a2,
                            a1,
                            paddr,
                            crate::memory::paging::UserPagePerm::ReadWrite,
                        );
                        crate::memory::frame_allocator::inc_ref(paddr);
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
                        crate::memory::paging::map_user_page(
                            a1,
                            a2,
                            p,
                            crate::memory::paging::UserPagePerm::ReadWrite,
                        );
                        crate::memory::frame_allocator::inc_ref(p);
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
            // 注册当前域接收 `a1`, 需持有 `Capability::Irq(a1)`。
            //   a1 < 16  → PIC 的 IRQ, 中断数据经 IPC 投递 (见 irq::dispatch);
            //   a1 >= 32 → MSI/MSI-X 向量, 中断只置待处理位 (见 irq::set_pending)。
            // 16..31 是 CPU 异常向量, 不允许注册。
            let domain = crate::scheduler::current_domain();
            let irq = a1 as u8;
            if !crate::cap::has(domain, crate::cap::Capability::Irq(irq)) {
                return 0;
            }
            if irq < 16 {
                crate::irq::register(irq, domain);
                1
            } else if irq >= 32 {
                crate::irq::register_vector(irq, domain);
                1
            } else {
                0
            }
        }
        SYS_IRQ_POLL => {
            // 非阻塞取走 `a1` (MSI/MSI-X **向量掩码**) 里任意一个的待处理标志,
            // 位 `i` 对应向量 `idt::MSI_VECTOR_BASE + i`; 返回**命中的向量号**, 没有则 0。
            // 掩码每一位都须是本域注册的向量且持有 `Capability::Irq`。
            let domain = crate::scheduler::current_domain();
            if a1 == 0 || !irq_mask_ok(domain, a1) {
                return 0;
            }
            crate::irq::take_pending_any(a1, domain).map_or(0, |v| v as u64)
        }
        SYS_MSIX_ENABLE => {
            // 打开被授权设备的 MSI-X (驱动已写好表项)。只有该设备的驱动域能调用,
            // 且只能成功一次; 配置空间写因此不会被下放到驱动域。
            if crate::device::enable_msix() {
                1
            } else {
                0
            }
        }
        SYS_DEVICE_CONFIG_READ => {
            // 读本域被授权设备的 PCI 配置空间 dword (N2: 驱动自行解析能力链表)。
            // 只放行"读自己那台设备"; 没有绑定设备返回 u64::MAX。
            crate::device::config_read(a1 as u32).map_or(u64::MAX, |v| v as u64)
        }
        SYS_IRQ_WAIT => {
            // 阻塞等待 `a1` (向量掩码, 编码同 `SYS_IRQ_POLL`) 里任意一个向量的中断,
            // 最多 `a2` 毫秒: 返回**命中的向量号**, 超时/非法返回 0。
            //
            // 掩码里含该向量正是中断处理器唤醒本域的**唯一**条件 (`irq::set_any_mask`
            // 记下掩码, `set_pending` 命中才 `wake_one`) —— 于是「等哪几个向量」这件事
            // 不必在调度器里表达。syscall 入口已用 SFMASK 清 IF, 故「查标志 → 登记掩码
            // → 阻塞」之间不会插进中断处理, 不存在丢唤醒。
            let domain = crate::scheduler::current_domain();
            let mask = a1;
            if mask == 0 || !irq_mask_ok(domain, mask) {
                return 0;
            }
            if let Some(vector) = crate::irq::take_pending_any(mask, domain) {
                return vector as u64;
            }
            if !crate::irq::set_any_mask(domain, mask) {
                return 0;
            }
            crate::scheduler::block_current_timeout_ms(
                crate::scheduler::irq_wait_token(domain),
                a2,
            );
            crate::irq::clear_any_mask(domain);
            // 醒来的原因可能是被中断唤醒, 也可能是超时: 只有标志真在才算等到。
            crate::irq::take_pending_any(mask, domain).map_or(0, |v| v as u64)
        }
        // 15..=20 (内核终端输入行: 滚动 / 退格 / 逐键编辑) 已随 G4 退役 ——
        // 输入改走 SYS_KEY_PUSH / SYS_KEY_READ, 行编辑在用户态屏幕控制台。
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
            // D0 门禁: 需持有覆盖该端口的 `IoPort` 能力; 被拒返回 `u64::MAX`
            // (端口读只可能是 0..=0xFF, 故该哨兵不会与真实值混淆)。
            let port = a1 as u16;
            if crate::cap::has_port(crate::scheduler::current_domain(), port) {
                unsafe { Port::<u8>::new(port).read() as u64 }
            } else {
                u64::MAX
            }
        }
        SYS_PORT_IN16 => {
            let port = a1 as u16;
            if crate::cap::has_port(crate::scheduler::current_domain(), port) {
                unsafe { Port::<u16>::new(port).read() as u64 }
            } else {
                u64::MAX
            }
        }
        SYS_PORT_OUT8 => {
            let port = a1 as u16;
            if crate::cap::has_port(crate::scheduler::current_domain(), port) {
                unsafe { Port::<u8>::new(port).write(a2 as u8) };
                0
            } else {
                u64::MAX
            }
        }
        SYS_PORT_OUT16 => {
            let port = a1 as u16;
            if crate::cap::has_port(crate::scheduler::current_domain(), port) {
                unsafe { Port::<u16>::new(port).write(a2 as u16) };
                0
            } else {
                u64::MAX
            }
        }
        SYS_VIRT_TO_PHYS => {
            // 把当前域用户虚拟地址 `a1` 反查为物理地址 (供 NVMe 等 DMA 驱动填 PRP)。
            let domain = crate::scheduler::current_domain();
            if crate::memory::paging::is_user_address(a1) {
                crate::memory::paging::resolve_user_page(domain, a1).unwrap_or(0)
            } else {
                0
            }
        }
        SYS_CLEAR => {
            // 清屏并复位内核终端状态 (历史 / 当前行)。
            crate::video::clear_screen();
            1
        }
        SYS_CAP_ISSUE => {
            // 「能力即句柄」: 为调用方域的不透明对象 `a1` 签发句柄, 返回句柄索引。
            // 微内核不解释对象含义 (文件句柄由 libvfs 打包成 (服务域<<32)|服务内 fd)。
            let domain = crate::scheduler::current_domain();
            crate::cap::handle_issue(domain, a1)
        }
        SYS_CAP_LOOKUP => {
            // 校验句柄是否仍然有效, 有效则返回其对象标识 (被撤销后一律失败)。
            let domain = crate::scheduler::current_domain();
            crate::cap::handle_lookup(domain, a1).unwrap_or(u64::MAX)
        }
        SYS_CAP_DROP => {
            // 撤销句柄 (关闭打开对象时调用)。
            let domain = crate::scheduler::current_domain();
            if crate::cap::handle_drop(domain, a1) {
                1
            } else {
                0
            }
        }
        SYS_HANDLE_SEND => {
            // 「能力随 IPC 传递」: 把自己句柄 `a2` 里的不透明对象移入 `a1` 域, 返回
            // 目标域里的新句柄。前置能力 `SendTo(a1)` —— 否则任何域都能往别的域塞
            // 句柄 (把自己的对象灌进对方 32 个槽位, 是纯粹的 DoS)。
            let me = crate::scheduler::current_domain();
            if !crate::cap::has(me, crate::cap::Capability::SendTo(a1)) {
                return u64::MAX;
            }
            crate::cap::handle_move(me, a1, a2)
        }
        SYS_CAP_SEND => {
            // 「能力随 IPC 传递」: 把自己**确实持有**的能力委派给 `a1` 域。
            // 两层校验都不可省: 外层 `SendTo(a1)` 管「能不能把东西给对方」,
            // 内层 `cap::delegate` 管「这东西是不是我的」(无放大)。
            let me = crate::scheduler::current_domain();
            if !crate::cap::has(me, crate::cap::Capability::SendTo(a1)) {
                return 0;
            }
            match crate::cap::decode(a2, a3) {
                Some(cap) => crate::cap::delegate(me, a1, cap) as u64,
                None => 0,
            }
        }
        SYS_PUTS => {
            // 从用户地址空间读取字符串并打印 (当前 CR3 即用户域, 可直接访问)。
            // 用 print 而非 println: 换行由用户态通过发送 "\n" 自行控制。
            // 信任边界: 必须校验 a1 是用户空间地址, 且 a1+a2 不越界, 否则恶意域可传
            // 恒等/offset 映射地址在内核态 (Ring0) 下读取任意物理内存。
            if a1 == 0 || a2 == 0 {
                return 0;
            }
            let Some(end) = a1.checked_add(a2) else {
                return 0;
            };
            if !crate::memory::paging::is_user_address(a1)
                || !crate::memory::paging::is_user_address(end - 1)
            {
                return 0;
            }
            let slice = unsafe { core::slice::from_raw_parts(a1 as *const u8, a2 as usize) };
            let s = unsafe { core::str::from_utf8_unchecked(slice) };
            crate::video::print(s);
            0
        }
        SYS_SPAWN_ELF => {
            // 加载可执行文件并启动 (rdi = 镜像首地址, rsi = 长度) → 新域 id / u64::MAX。
            let from = crate::scheduler::current_domain();
            if !crate::cap::has(from, crate::cap::Capability::Spawn) {
                return u64::MAX;
            }
            let Some(image) = user_elf_image(a1, a2) else {
                return u64::MAX;
            };
            crate::exec::spawn_elf(image, from).unwrap_or(u64::MAX)
        }
        SYS_SPAWN_ELF_AT => {
            // 在**指定域**里加载并启动 (E3c: 监督者把退出/崩溃的服务原地拉起来)。
            let from = crate::scheduler::current_domain();
            if !crate::cap::has(from, crate::cap::Capability::Spawn) {
                return u64::MAX;
            }
            let Some(image) = user_elf_image(a2, a3) else {
                return u64::MAX;
            };
            restart_in_place(a1, image)
        }
        SYS_SPAWN_ELF_MODULE => {
            // 用**引导模块内存镜像**在指定域原地重启 (E3c 后续) —— 镜像来自内核侧,
            // 不给用户态传地址, 故除了域号无需别的参数。
            let from = crate::scheduler::current_domain();
            if !crate::cap::has(from, crate::cap::Capability::Spawn) {
                return u64::MAX;
            }
            let Some(image) = boot_module_image(a1) else {
                return u64::MAX;
            };
            restart_in_place(a1, image)
        }
        SYS_FB_INFO => {
            // 取帧缓冲几何, 写回用户缓冲 (G1: 用户态图形服务用它算页面范围)。
            let domain = crate::scheduler::current_domain();
            if !crate::cap::has(domain, crate::cap::Capability::Fb) {
                return 0;
            }
            let size = core::mem::size_of::<FbInfo>() as u64;
            if a1 == 0
                || !crate::memory::paging::is_user_address(a1)
                || !crate::memory::paging::is_user_address(a1 + size - 1)
            {
                return 0;
            }
            let (width, height, stride, bpp) = crate::video::fb_geometry();
            let info = FbInfo {
                addr: crate::video::fb_base(),
                width,
                height,
                stride,
                bpp,
            };
            unsafe {
                core::ptr::write(a1 as *mut FbInfo, info);
            }
            1
        }
        SYS_FB_MAP => {
            // 把整块帧缓冲映射进本域 (按 4 KiB 页, 非缓存)。先整段确认"未映射"再动手 ——
            // 已有映射时半途返回会留下脏状态, 且逐页 map 撞已映射页会 panic。
            let domain = crate::scheduler::current_domain();
            if !crate::cap::has(domain, crate::cap::Capability::Fb) {
                return 0;
            }
            let base = crate::video::fb_base();
            let bytes = crate::video::fb_bytes();
            if base == 0 || bytes == 0 || a1 & 0xFFF != 0 {
                return 0;
            }
            let Some(end) = a1.checked_add(bytes) else {
                return 0;
            };
            if !crate::memory::paging::is_user_address(a1)
                || !crate::memory::paging::is_user_address(end - 1)
            {
                return 0;
            }
            let pages = bytes.div_ceil(4096);
            for i in 0..pages {
                if crate::memory::paging::resolve_user_page(domain, a1 + i * 4096).is_some() {
                    return 0; // 目标区间已被占用: 不映射, 也不 panic
                }
            }
            for i in 0..pages {
                crate::memory::paging::map_mmio(domain, a1 + i * 4096, base + i * 4096);
            }
            1
        }
        SYS_FB_TAKEOVER => {
            // 用户态宣告接管显示: 内核终端此后不再写帧缓冲 (输出只留 COM1)。
            let domain = crate::scheduler::current_domain();
            if !crate::cap::has(domain, crate::cap::Capability::Fb) {
                return 0;
            }
            crate::video::take_over();
            1
        }
        SYS_CONSOLE_READY => crate::video::is_taken_over() as u64,
        SYS_KEY_PUSH => {
            // 键盘域推一个按键字节进来; 内核只做搬运, 不解释语义。
            crate::key::push(a1 as u8);
            1
        }
        SYS_KEY_READ => {
            // 阻塞取一个按键字节: 队列空就睡, 由 `SYS_KEY_PUSH` 唤醒 (wait_on = KEY_WAIT)。
            loop {
                if let Some(c) = crate::key::pop() {
                    return c as u64;
                }
                crate::scheduler::block_current(crate::scheduler::KEY_WAIT);
            }
        }
        SYS_DOMAIN_ALIVE => {
            // 该域是否**还有存活任务** (监督者巡检原语)。
            (crate::domain::is_alive(a1) && crate::scheduler::live_tasks(a1) > 0) as u64
        }
        SYS_EXIT => crate::scheduler::exit_current(),
        SYS_DOMAIN_DESTROY => {
            // 「谁加载谁负责」: 既要能造进程 (Spawn), 又要是它的分页器 (= 加载它的域)。
            let from = crate::scheduler::current_domain();
            if !crate::cap::has(from, crate::cap::Capability::Spawn) {
                return 0;
            }
            if crate::pager::of(a1) != Some(from) {
                return 0;
            }
            crate::domain::destroy(a1) as u64
        }
        SYS_DOMAIN_COUNT => crate::domain::alive_count() as u64,
        SYS_FRAME_FREE => crate::memory::frame_allocator::free_frames() as u64,
        // 并行开发接线层: 三条转发臂, 实现分散在各自模块 (见 docs/dev-workflow.md)。
        SYS_UNAME => crate::version::handle(a1, a2, a3),
        SYS_DEVICE_INFO => crate::device::syscall_info(a1, a2, a3),
        SYS_DEVICE_GRANT => crate::device::syscall_grant(a1, a2, a3),
        SYS_CAP_AUDIT => {
            // 能力审计 (②): 持 `Spawn` 的可信域 (init 监督者) 枚举 `a1` 域第 `a2` 个能力槽,
            // 供其按最小权限策略核对引导期授权。非 `Spawn` 持有者一律拒绝 (返回 0, 与"空槽"
            // 同形 —— 该路径不可达, 因为只有 init 会调)。
            let me = crate::scheduler::current_domain();
            if !crate::cap::has(me, crate::cap::Capability::Spawn) {
                0
            } else {
                match crate::cap::audit_slot(a1, a2) {
                    Some(Some(cap)) => crate::cap::pack_audit(cap),
                    Some(None) => 0,  // 空槽
                    None => u64::MAX, // 域/槽越界: 审计者据此判定表尾
                }
            }
        }
        SYS_TRY_RECV => {
            // 非阻塞收: 有消息就把完整消息写回 `a1` 并返回 tag; 邮箱空返回 u64::MAX。
            // 信任边界: 校验写回地址是用户空间, 避免恶意域传入内核地址导致任意内核写。
            let msg_size = core::mem::size_of::<crate::ipc::Message>() as u64;
            match crate::ipc::try_receive() {
                Some(msg) => {
                    if a1 != 0
                        && crate::memory::paging::is_user_address(a1)
                        && crate::memory::paging::is_user_address(a1 + msg_size - 1)
                    {
                        unsafe {
                            core::ptr::copy_nonoverlapping(
                                &msg as *const crate::ipc::Message as *const u8,
                                a1 as *mut u8,
                                msg_size as usize,
                            );
                        }
                    }
                    msg.tag
                }
                None => u64::MAX,
            }
        }
        SYS_NET_BIND => {
            // 绑定端口: 需覆盖该端口的 `Net` 能力 (不可伪造), 成功后登记归属。
            let me = crate::scheduler::current_domain();
            crate::net::bind(me, a1 as u16) as u64
        }
        SYS_NET_OWNER => {
            // 只读查询端口归属域 (供 netstack 核对 bind 请求的调用者)。
            crate::net::owner(a1 as u16)
        }
        SYS_LOG_TOTAL => crate::klog::total(),
        SYS_LOG_READ => crate::klog::handle_read(a1, a2, a3),
        SYS_PCI_INFO => crate::arch::pci::syscall_info(a1, a2),
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
