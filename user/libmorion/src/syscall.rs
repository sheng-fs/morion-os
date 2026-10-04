//! 系统调用封装 (libuser 雏形)
//!
//! 系统调用编号须与 kernel/src/syscall.rs 保持一致。
//! ABI: 编号在 `rax`, 参数在 `rdi/rsi/rdx`, 返回值在 `rax`。

// 预留的 syscall 封装 (yield/send/recv) 后续阶段才会使用, 先抑制 dead_code 警告。
#![allow(dead_code)]

use core::arch::asm;
use core::cell::UnsafeCell;

/// 本程序所在域的 id, 由 `lib.rs` 的 `_start` 在进入 `morion_main` 前写入。
///
/// 内核把域 id 放进 RDI 交给 `_start` —— 程序自己拿不到这个参数（入口在库里），
/// 故由库代记, 需要时问 [`domain_id`]。
static mut DOMAIN_ID: u64 = u64::MAX;

/// 记录本域 id（只应由 libmorion 的 `_start` 调用）。
pub fn set_domain_id(id: u64) {
    unsafe { DOMAIN_ID = id };
}

/// 本程序所在的域 id（未初始化时为 `u64::MAX`）。
pub fn domain_id() -> u64 {
    unsafe { DOMAIN_ID }
}

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
// 15..=20 与 27 曾用于「内核终端输入行」(滚动 / 退格 / 逐键编辑 / 读一行);
// G4 把输入搬进用户态屏幕控制台后**退役** —— 输入改走表尾的 SYS_KEY_PUSH / SYS_KEY_READ。
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
pub const SYS_HANDLE_SEND: u64 = 32;
pub const SYS_CAP_SEND: u64 = 33;
pub const SYS_IRQ_POLL: u64 = 34;
pub const SYS_MSIX_ENABLE: u64 = 35;
pub const SYS_IRQ_WAIT: u64 = 36;
/// 加载可执行文件 (ELF64 `ET_EXEC`) 并启动, 返回新域 id (失败 `u64::MAX`)。
pub const SYS_SPAWN_ELF: u64 = 37;
/// 销毁一个域并回收它的全部资源 (`rdi = 域 id`), 成功返回 1, 失败 0。
///
/// 门禁: 需 `Capability::Spawn`, 且调用者必须是该域的**分页器** (= 加载它的那个域) ——
/// 也就是"谁加载谁负责"。所以自我销毁不可达。
pub const SYS_DOMAIN_DESTROY: u64 = 38;
/// 存活域数 (自测取证: 销毁之后应回到基线)。
pub const SYS_DOMAIN_COUNT: u64 = 39;
/// 当前空闲物理帧数。
pub const SYS_FRAME_FREE: u64 = 40;
/// 在**指定域**里加载可执行文件并启动 (E3c): `(域 id, 镜像首地址, 长度)` → 域 id / `u64::MAX`。
pub const SYS_SPAWN_ELF_AT: u64 = 41;
/// 该域是否还有存活任务: `(域 id)` → 1 / 0。
pub const SYS_DOMAIN_ALIVE: u64 = 42;
/// 用**引导模块内存镜像**在指定域原地重启 (E3c 后续): `(域 id)` → 域 id / `u64::MAX`。
///
/// 镜像不走用户态: 内核按域号去引导模块表里取自己那一份 (见内核 `bootinfo::ServiceModule`),
/// 故调用方只需给出目标域号。用于重启**文件服务本身** —— 它不依赖磁盘, 解掉"读盘要靠
/// 文件服务"的鸡生蛋问题。需持有 `Capability::Spawn`。
pub const SYS_SPAWN_ELF_MODULE: u64 = 43;
/// 取帧缓冲几何: `(用户缓冲指针)` → 1 / 0 (写入 [`FbInfo`])。需 `Capability::Fb`。
pub const SYS_FB_INFO: u64 = 44;
/// 把**整块帧缓冲**映射进本域: `(页对齐用户虚拟地址)` → 1 / 0。需 `Capability::Fb`。
pub const SYS_FB_MAP: u64 = 45;
/// 宣告本域**接管**显示 (内核终端停止写帧缓冲): `()` → 1 / 0。需 `Capability::Fb`。
pub const SYS_FB_TAKEOVER: u64 = 46;
/// 显示是否已交用户态 (无能力要求)。见 [`sys_console_ready`]。
pub const SYS_CONSOLE_READY: u64 = 47;
/// 把一个**按键字节**推进内核键队列: `(字节)` → 1。键盘域 (`kbd_srv`) 用。
pub const SYS_KEY_PUSH: u64 = 48;
/// **阻塞取**一个按键字节: `()` → 字节值。队列空则睡到有键为止。
pub const SYS_KEY_READ: u64 = 49;
/// 读本域被授权设备的 PCI 配置空间 dword (N2): `rdi = offset` → dword; 无设备返回 `u64::MAX`。
pub const SYS_DEVICE_CONFIG_READ: u64 = 50;
/// `SYS_UNAME` (V1 版本串): `(缓冲指针, 缓冲长度, 选择)` → 写入字节数 (不含结尾 NUL), 失败 0。
///
/// `选择`: 0 = 整行 `MorionOS <release> <machine>`、1 = release、2 = 构建号。
pub const SYS_UNAME: u64 = 51;
/// 能力审计 (②): `(目标域, 槽号)` → 打包的能力 / `0`(空槽) / `u64::MAX`(越界, 表尾)。
///
/// 需 `Capability::Spawn` (只有监督者该调)。编码: 高 8 位 = `种类 + 1`, 低 56 位 = 参数
/// (`IoPort` 为 `(base << 16) | len`)。见 [`sys_cap_audit`]。
pub const SYS_CAP_AUDIT: u64 = 54;

/// 非阻塞接收 (N6): 邮箱空返回 `u64::MAX`, 否则把完整消息写回 `buf` 并返回 tag。
pub const SYS_TRY_RECV: u64 = 55;
/// 绑定网络端口 (N6): 需覆盖该端口的 `Net` 能力; 成功返回 1。
pub const SYS_NET_BIND: u64 = 56;
/// 查询网络端口归属域 (N6): 返回归属域号 (`u64::MAX` = 未绑定)。
pub const SYS_NET_OWNER: u64 = 57;
/// 控制台日志累计字节数 (Phase 0 / P0.1): 无参, 返回日志环形缓冲的绝对总量。
pub const SYS_LOG_TOTAL: u64 = 58;
/// 读控制台日志 (Phase 0 / P0.1): `a1` = 缓冲、`a2` = 长度、`a3` = 起始绝对偏移。
pub const SYS_LOG_READ: u64 = 59;
/// 读一台 PCI 设备记录 (Phase 0 / P0.3): `a1` = 索引、`a2` = 缓冲 (3 个 u64)。
pub const SYS_PCI_INFO: u64 = 60;

/// 本程序是否属于**无图形**构建 (V2): 由 `Makefile` 注入的 `MORION_NOGUI` 决定。
///
/// 与内核 `version.rs` 的 `IS_NOGUI` 同一约定 —— `shell` 据此决定要不要开屏幕镜像。
pub const NOGUI: bool = option_env!("MORION_NOGUI").is_some();

/// 本程序是否属于**安装盘**构建: 由 `Makefile` 注入的 `MORION_INSTALL` 决定 (`make INSTALL=1 iso`)。
///
/// 与内核 `version.rs` 的 `IS_INSTALL` 同一约定 —— 安装盘要能把系统装进本机盘, 故这一变体里
/// `mfs_srv` 对**非空白卷**的格式化护栏默认放开 (装机要覆盖的正是盘上原有的文件系统)。
/// 日常镜像为 `false`, 护栏一字不放宽。
pub const INSTALL_MODE: bool = option_env!("MORION_INSTALL").is_some();

/// 本程序是否属于**安全模式**构建 (Phase 0 / P0.2): 由 `Makefile` 注入的 `MORION_SAFE` 决定。
///
/// 安全模式 = **真机启动专用**镜像: 真机上内核会把**本机盘**当卷挂上, 而 `app` 自测会
/// 创建/删除文件、拍快照、做分区写 —— 那等于对着你的真实系统盘动手。安全模式下 `app`
/// 自测整体跳过 (只报一行), 把"首次真机启动"的数据风险降到最低。
/// 与内核 `version.rs` 的 `IS_SAFE` 同一约定。
pub const SAFE_MODE: bool = option_env!("MORION_SAFE").is_some();

#[inline(always)]
unsafe fn syscall(n: u64, a1: u64, a2: u64, a3: u64) -> u64 {
    let ret: u64;
    asm!(
        "syscall",
        inlateout("rax") n => ret,
        // rdi/rsi/rdx 是 syscall 参数寄存器, 内核 syscall_entry 会改写它们
        // (rdi←编号, rsi←a1, rdx←a2), 故须用 inout 声明并丢弃输出, 否则
        // 编译器会假设它们跨 syscall 不变 (复用 rdi 作写地址导致页错误)。
        inout("rdi") a1 => _,
        inout("rsi") a2 => _,
        inout("rdx") a3 => _,
        // rcx/r11 被 syscall 指令本身改写; r8/r9/r10 是 caller-saved,
        // 内核 syscall_entry 并不保存它们 (会经 syscall_dispatch 被破坏)。
        // 必须声明为 clobber, 否则编译器会假设它们跨 syscall 不变。
        lateout("rcx") _,
        lateout("r11") _,
        lateout("r8") _,
        lateout("r9") _,
        lateout("r10") _,
        options(nostack)
    );
    ret
}

pub fn sys_yield() {
    unsafe {
        syscall(SYS_YIELD, 0, 0, 0);
    }
}

pub fn sys_sleep(ms: u64) {
    unsafe {
        syscall(SYS_SLEEP, ms, 0, 0);
    }
}

pub fn sys_send(to: u64, tag: u64) -> u64 {
    unsafe { syscall(SYS_SEND, to, tag, 0) }
}

/// 消息 payload 固定大小 (与内核 `ipc::PAYLOAD_LEN` 一致)。
pub const PAYLOAD_LEN: usize = 96;

/// 发送带 payload 的消息 (payload 最多 `PAYLOAD_LEN` 字节, 超出部分截断)。
pub fn sys_send_payload(to: u64, tag: u64, payload: &[u8]) -> u64 {
    let mut buf = [0u8; PAYLOAD_LEN];
    let n = payload.len().min(PAYLOAD_LEN);
    buf[..n].copy_from_slice(&payload[..n]);
    unsafe { syscall(SYS_SEND, to, tag, buf.as_ptr() as u64) }
}

pub fn sys_recv() -> u64 {
    unsafe { syscall(SYS_RECV, 0, 0, 0) }
}

/// 阻塞接收一条消息, 把完整消息 (24 字节头 + `PAYLOAD_LEN` payload) 写入 `buf`,
/// 返回消息 tag。
pub fn sys_recv_msg(buf: *mut u8) -> u64 {
    unsafe { syscall(SYS_RECV, buf as u64, 0, 0) }
}

/// **非阻塞**接收一条消息 (N6): 邮箱为空立即返回 `u64::MAX`, 否则把完整消息
/// (24 字节头 + `PAYLOAD_LEN` payload) 写入 `buf` 并返回消息 tag。
pub fn sys_try_recv(buf: *mut u8) -> u64 {
    unsafe { syscall(SYS_TRY_RECV, buf as u64, 0, 0) }
}

pub fn sys_call(to: u64, tag: u64) -> u64 {
    unsafe { syscall(SYS_CALL, to, tag, 0) }
}

/// 同步调用带 payload 的消息, 返回回复 tag。
pub fn sys_call_payload(to: u64, tag: u64, payload: &[u8]) -> u64 {
    let mut buf = [0u8; PAYLOAD_LEN];
    let n = payload.len().min(PAYLOAD_LEN);
    buf[..n].copy_from_slice(&payload[..n]);
    unsafe { syscall(SYS_CALL, to, tag, buf.as_ptr() as u64) }
}

pub fn sys_reply(tag: u64) -> u64 {
    unsafe { syscall(SYS_REPLY, tag, 0, 0) }
}

pub fn sys_register_irq(irq: u64) -> u64 {
    unsafe { syscall(SYS_REGISTER_IRQ, irq, 0, 0) }
}

/// 非阻塞取走**掩码**里任意一个 MSI/MSI-X 向量的「待处理」标志: 返回命中的**向量号**, 无则 0。
///
/// 掩码位 `i` ↔ 向量 `NvmeConfig::msix_vector + i` (= 完成队列 `i` 的向量)。中断不投 IPC
/// 消息 (那会和驱动的请求邮箱混在一起), 故驱动用本调用取位: 只有中断处理器置位才能命中。
/// 需持有命中向量的 `Capability::Irq` 且是该向量的注册者。
pub fn sys_irq_poll(mask: u64) -> u64 {
    unsafe { syscall(SYS_IRQ_POLL, mask, 0, 0) }
}

/// 阻塞等待**掩码**里任意一个 MSI/MSI-X 向量的中断, 最多等 `timeout_ms` 毫秒。
///
/// 返回命中的**向量号** (据此可知是哪条队列完成), 超时返回 0 (调用方据此回退轮询)。
/// 等中断期间本域处于阻塞态, CPU 交给别的域 (通常是空闲任务 `hlt`), 不占用时间片空转 ——
/// 这正是与 `sys_irq_poll` 自旋的本质区别。需持有命中向量的 `Capability::Irq` 且是其注册者。
pub fn sys_irq_wait(mask: u64, timeout_ms: u64) -> u64 {
    unsafe { syscall(SYS_IRQ_WAIT, mask, timeout_ms, 0) }
}

/// 打开本域所属设备的 MSI-X (需在写好 MSI-X 表项后调用), 成功返回 1。
///
/// 只有该设备的驱动域能调用, 且只成功一次; 配置空间写留在内核。
pub fn sys_msix_enable() -> u64 {
    unsafe { syscall(SYS_MSIX_ENABLE, 0, 0, 0) }
}

/// MSI/MSI-X 向量段的基址（须与内核 `arch::idt::MSI_VECTOR_BASE` 一致）。
///
/// `SYS_IRQ_POLL` / `SYS_IRQ_WAIT` 的掩码**位 `i` 对应向量 `MSI_VECTOR_BASE + i`**：驱动拿到的
/// 向量段不一定从段首开始（多设备各占一段），故掩码要整体左移 `向量段基址 - MSI_VECTOR_BASE`。
pub const MSI_VECTOR_BASE: u64 = 0x50;

/// 读本域被授权设备的 PCI 配置空间 dword (`offset` 会被对齐到 4)。
///
/// 驱动靠它自行解析能力链表 (PCI 通用能力 / 厂商能力, 如 virtio 各 BAR 区域偏移) ——
/// 内核因此不必懂设备协议。没有绑定设备时返回 `u64::MAX`。
pub fn sys_device_config_read(offset: u64) -> u64 {
    unsafe { syscall(SYS_DEVICE_CONFIG_READ, offset, 0, 0) }
}

/// 读系统名/版本/构建号 (V1), 写入 `buf`, 返回写入字节数 (不含结尾 NUL); 失败 0。
///
/// `which` 取 [`SYS_UNAME`] 的选择值; 版本常量由内核 `version.rs` **单一维护**。
pub fn sys_uname(buf: &mut [u8], which: u64) -> u64 {
    unsafe { syscall(SYS_UNAME, buf.as_ptr() as u64, buf.len() as u64, which) }
}

/// 加载可执行文件 (ELF64 `ET_EXEC`) 并启动, 返回**新域 id** (失败 `u64::MAX`)。
///
/// `image` 是完整的 ELF 镜像字节 (须在本域已映射的内存里)。内核会全量校验再映射;
/// 新域零能力, 其分页器登记为本域。需持有 `Capability::Spawn`。
pub fn sys_spawn_elf(image: &[u8]) -> u64 {
    unsafe { syscall(SYS_SPAWN_ELF, image.as_ptr() as u64, image.len() as u64, 0) }
}

/// 销毁域 `domain` 并回收它的地址空间与内核状态, 成功返回 1。
///
/// 调用者必须是它的分页器 (加载它的域), 且持有 `Capability::Spawn`。
pub fn sys_domain_destroy(domain: u64) -> u64 {
    unsafe { syscall(SYS_DOMAIN_DESTROY, domain, 0, 0) }
}

/// 当前存活域数。
pub fn sys_domain_count() -> u64 {
    unsafe { syscall(SYS_DOMAIN_COUNT, 0, 0, 0) }
}

/// 在**指定域**里加载可执行文件并启动, 成功返回该域 id (失败 `u64::MAX`)。
///
/// 目标域必须已存在且**没有存活任务** (旧实例已退出), 内核会先清空它的用户地址空间再
/// 映射新镜像 —— 于是域号不变。监督者用它把退出/崩溃的服务原地拉起来 (E3c)。
/// 需持有 `Capability::Spawn`。
pub fn sys_spawn_elf_at(domain: u64, image: &[u8]) -> u64 {
    unsafe {
        syscall(
            SYS_SPAWN_ELF_AT,
            domain,
            image.as_ptr() as u64,
            image.len() as u64,
        )
    }
}

/// 该域是否**还有存活任务**: 1 / 0 (域不存在也算 0)。
pub fn sys_domain_alive(domain: u64) -> u64 {
    unsafe { syscall(SYS_DOMAIN_ALIVE, domain, 0, 0) }
}

/// 用**引导模块内存镜像**在指定域原地重启, 成功返回该域 id (失败 `u64::MAX`)。
///
/// 镜像由内核按域号从引导模块表取, 不依赖磁盘 —— 文件服务 (fat32_srv / mfs_srv) 自己
/// 崩了也能被拉起来。目标域须已存在且没有存活任务 (与 `sys_spawn_elf_at` 同一前提)。
pub fn sys_spawn_elf_module(domain: u64) -> u64 {
    unsafe { syscall(SYS_SPAWN_ELF_MODULE, domain, 0, 0) }
}

/// 帧缓冲几何 (`SYS_FB_INFO` 写回的布局, 与内核 `syscall::FbInfo` 严格对应)。
#[repr(C)]
#[derive(Clone, Copy, Default)]
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

/// 取帧缓冲几何; 成功返回 1 并写入 `info`, 失败 0 (需 `Capability::Fb`)。
pub fn sys_fb_info(info: &mut FbInfo) -> u64 {
    unsafe { syscall(SYS_FB_INFO, info as *mut FbInfo as u64, 0, 0) }
}

/// 把整块帧缓冲映射到本域的 `vaddr` (页对齐); 成功返回 1, 失败 0 (需 `Capability::Fb`)。
pub fn sys_fb_map(vaddr: u64) -> u64 {
    unsafe { syscall(SYS_FB_MAP, vaddr, 0, 0) }
}

/// 宣告本域接管显示 (内核终端此后不再写帧缓冲); 成功返回 1 (需 `Capability::Fb`)。
pub fn sys_fb_takeover() -> u64 {
    unsafe { syscall(SYS_FB_TAKEOVER, 0, 0, 0) }
}

/// 显示是否已交用户态 (`gfx_srv` 接管过帧缓冲)。
///
/// `print` 的屏幕镜像 ([`screen_mirror_on`]) 必须先问这一句: 没接管时内核终端根本不写屏,
/// 镜像只会白等一次 `SYS_CALL`。
pub fn sys_console_ready() -> bool {
    unsafe { syscall(SYS_CONSOLE_READY, 0, 0, 0) == 1 }
}

/// 当前空闲物理帧数。
pub fn sys_frame_free() -> u64 {
    unsafe { syscall(SYS_FRAME_FREE, 0, 0, 0) }
}

/// 把一个按键字节推进内核键队列 (`kbd_srv` 用; 队列满时该字节被丢弃)。
///
/// 内核不解释这个字节 —— 可打印字符 / 退格 / 回车一视同仁, 行编辑在用户态。
pub fn sys_key_push(c: u8) -> u64 {
    unsafe { syscall(SYS_KEY_PUSH, c as u64, 0, 0) }
}

/// **阻塞**取一个按键字节: 无键可用时睡眠, 由键盘域推键唤醒。
pub fn sys_key_read() -> u64 {
    unsafe { syscall(SYS_KEY_READ, 0, 0, 0) }
}

pub fn sys_alloc_page(vaddr: u64) -> u64 {
    unsafe { syscall(SYS_ALLOC_PAGE, vaddr, 0, 0) }
}

pub fn sys_share_page(vaddr: u64, to: u64) -> u64 {
    unsafe { syscall(SYS_SHARE_PAGE, vaddr, to, 0) }
}

pub fn sys_unmap(vaddr: u64) -> u64 {
    unsafe { syscall(SYS_UNMAP, vaddr, 0, 0) }
}

pub fn sys_map_anon(domain: u64, vaddr: u64) -> u64 {
    unsafe { syscall(SYS_MAP_ANON, domain, vaddr, 0) }
}

pub fn sys_page_fault_reply() -> u64 {
    unsafe { syscall(SYS_PAGE_FAULT_REPLY, 0, 0, 0) }
}

/// 把物理 MMIO 页 (`bar_paddr`, 页对齐) 映射到本域 `vaddr`, 需 Mmio 能力。
pub fn sys_map_mmio(bar_paddr: u64, vaddr: u64) -> u64 {
    unsafe { syscall(SYS_MAP_MMIO, bar_paddr, vaddr, 0) }
}

/// 从 I/O 端口 `port` 读一个字节。
pub fn sys_port_in8(port: u16) -> u8 {
    unsafe { syscall(SYS_PORT_IN8, port as u64, 0, 0) as u8 }
}

/// 从 I/O 端口 `port` 读一个字节的**原始返回值**（D0）。
///
/// 与 [`sys_port_in8`] 的区别: 不截断成 `u8` —— 无覆盖该端口的 `IoPort` 能力时内核回
/// `u64::MAX`（端口读只可能是 `0..=0xFF`），自测据此断言"端口门禁生效"。
pub fn sys_port_in8_raw(port: u16) -> u64 {
    unsafe { syscall(SYS_PORT_IN8, port as u64, 0, 0) }
}

/// 从 I/O 端口 `port` 读一个 16 位字。
pub fn sys_port_in16(port: u16) -> u16 {
    unsafe { syscall(SYS_PORT_IN16, port as u64, 0, 0) as u16 }
}

/// 向 I/O 端口 `port` 写一个字节。
pub fn sys_port_out8(port: u16, value: u8) {
    unsafe {
        syscall(SYS_PORT_OUT8, port as u64, value as u64, 0);
    }
}

/// 向 I/O 端口 `port` 写一个 16 位字。
pub fn sys_port_out16(port: u16, value: u16) {
    unsafe {
        syscall(SYS_PORT_OUT16, port as u64, value as u64, 0);
    }
}

/// 查询本域用户虚拟地址 `vaddr` 对应的物理地址 (供 NVMe PRP 使用), 失败返回 0。
pub fn sys_virt_to_phys(vaddr: u64) -> u64 {
    unsafe { syscall(SYS_VIRT_TO_PHYS, vaddr, 0, 0) }
}

/// 清屏并复位内核终端状态 (历史 / 当前行)。返回 1。
pub fn sys_clear() -> u64 {
    unsafe { syscall(SYS_CLEAR, 0, 0, 0) }
}

/// 「能力即句柄」: 为不透明对象 `obj` 签发一个句柄, 返回句柄索引 (0 起);
/// 句柄槽耗尽返回 `u64::MAX`。
pub fn sys_cap_issue(obj: u64) -> u64 {
    unsafe { syscall(SYS_CAP_ISSUE, obj, 0, 0) }
}

/// 校验句柄是否有效, 有效则返回其对象标识; 已被撤销 / 非法返回 `u64::MAX`。
pub fn sys_cap_lookup(handle: u64) -> u64 {
    unsafe { syscall(SYS_CAP_LOOKUP, handle, 0, 0) }
}

/// 撤销句柄 (关闭打开对象时调用), 成功返回 1。
pub fn sys_cap_drop(handle: u64) -> u64 {
    unsafe { syscall(SYS_CAP_DROP, handle, 0, 0) }
}

/// 「能力随 IPC 传递」: 把自己句柄 `handle` 指向的对象**移入**目标域 `to`,
/// 返回 `to` 域里的新句柄索引; 失败 / 目标域句柄槽满返回 `u64::MAX`。
///
/// 移动语义: 成功后本域的 `handle` 立即失效 (交出 fd 后自己不再持有)。
/// 前置能力 `SendTo(to)`。
pub fn sys_handle_send(to: u64, handle: u64) -> u64 {
    unsafe { syscall(SYS_HANDLE_SEND, to, handle, 0) }
}

/// 能力类型编码 (与内核 `cap::CAP_KIND_*` 一致)。
pub const CAP_KIND_SEND_TO: u64 = 0;
pub const CAP_KIND_MAP_INTO: u64 = 1;
pub const CAP_KIND_IRQ: u64 = 2;
pub const CAP_KIND_MMIO: u64 = 3;
/// `Spawn` / `Fb` 无参数, `arg` 被忽略 (与内核 `CAP_KIND_*` 一致)。
pub const CAP_KIND_SPAWN: u64 = 4;
pub const CAP_KIND_FB: u64 = 5;
/// `IoPort` (D0): `arg = (base << 16) | len`。
pub const CAP_KIND_IO_PORT: u64 = 6;
/// `Net` (N6): `arg = (port_lo << 16) | port_hi`（闭区间）。
pub const CAP_KIND_NET: u64 = 7;

/// 「能力随 IPC 传递」: 把自己**持有**的能力委派给目标域 `to`, 成功返回 1。
///
/// 前置能力 `SendTo(to)`; 且必须**确实持有**要委派的能力 (无放大: 没有的能力给不出去)。
/// `kind` 取 `CAP_KIND_*`, `arg` 是该能力的参数 (目标域 id / IRQ 号 / 页对齐 MMIO 基址)。
pub fn sys_cap_send(to: u64, kind: u64, arg: u64) -> u64 {
    unsafe { syscall(SYS_CAP_SEND, to, kind, arg) }
}

/// 绑定网络端口 (N6): 调用者须持覆盖 `port` 的 [`CAP_KIND_NET`] 能力; 成功返回 1。
///
/// 绑定登记在内核侧, 供 `netstack_srv` 用 [`sys_net_owner`] 核对 `bind` 请求的发起域。
pub fn sys_net_bind(port: u16) -> u64 {
    unsafe { syscall(SYS_NET_BIND, port as u64, 0, 0) }
}

/// 查询端口 `port` 的归属域 (N6): 返回域号, 未绑定返回 `u64::MAX`。
pub fn sys_net_owner(port: u16) -> u64 {
    unsafe { syscall(SYS_NET_OWNER, port as u64, 0, 0) }
}

/// 能力审计 (②): 读域 `domain` 第 `slot` 个能力槽。返回编码见 [`SYS_CAP_AUDIT`]:
/// 非 0 = 打包的能力, `0` = 空槽, `u64::MAX` = 越界 (表尾)。需 `Capability::Spawn`。
pub fn sys_cap_audit(domain: u64, slot: u64) -> u64 {
    unsafe { syscall(SYS_CAP_AUDIT, domain, slot, 0) }
}

/// 控制台日志累计字节数 (Phase 0 / P0.1): 环形缓冲的绝对总量。
pub fn sys_log_total() -> u64 {
    unsafe { syscall(SYS_LOG_TOTAL, 0, 0, 0) }
}

/// 从绝对偏移 `start` 起把控制台日志读进 `dst`, 返回实际读取字节数 (0 = 读到头)。
pub fn sys_log_read(start: u64, dst: &mut [u8]) -> usize {
    unsafe {
        syscall(
            SYS_LOG_READ,
            dst.as_mut_ptr() as u64,
            dst.len() as u64,
            start,
        ) as usize
    }
}

/// 读第 `i` 台 PCI 设备记录 `[ids, 位置+class, bar0]`; 越界返回 `None` (Phase 0 / P0.3)。
pub fn sys_pci_info(i: u64) -> Option<[u64; 3]> {
    let mut rec = [0u64; 3];
    let ok = unsafe { syscall(SYS_PCI_INFO, i, rec.as_mut_ptr() as u64, 0) };
    if ok == 0 {
        None
    } else {
        Some(rec)
    }
}

pub fn sys_puts(s: &str) {
    unsafe {
        syscall(SYS_PUTS, s.as_ptr() as u64, s.len() as u64, 0);
    }
}

/// 终止当前用户任务 (永不返回)。
pub fn sys_exit() -> ! {
    unsafe {
        syscall(SYS_EXIT, 0, 0, 0);
    }
    loop {
        core::hint::spin_loop();
    }
}

// ---------------------------------------------------------------------------
// 极简打印辅助 (core-only, 无分配器)
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// 行缓冲打印
// ---------------------------------------------------------------------------
// 各域地址空间独立, 每个域有各自的缓冲; 单核下同一时刻仅一个域运行, 无需锁。
// 把「一行 = 多次 SYS_PUTS」合并为「一行 = 一次 SYS_PUTS」, 消除多域并发打印
// 在多次 syscall 之间被调度打断而造成的字符交错。

/// 可在 `static` 中存放可变数据的包装: 手动标记 `Sync`。
/// 安全前提: 单核 + 各域地址空间独立, 实际不存在对同一 static 的并发访问。
struct StaticCell<T>(UnsafeCell<T>);
unsafe impl<T> Sync for StaticCell<T> {}

impl<T> StaticCell<T> {
    const fn new(value: T) -> Self {
        StaticCell(UnsafeCell::new(value))
    }
    // 有意为之的内部可变性: 单核 + 各域地址空间独立, 不存在对同一 static 的
    // 并发访问, 故从 &self 返回 &mut T 是安全的, 无需改为 unsafe fn。
    #[allow(clippy::mut_from_ref)]
    fn borrow_mut(&self) -> &mut T {
        unsafe { &mut *self.0.get() }
    }
}

static PRINT_BUF: StaticCell<[u8; 256]> = StaticCell::new([0; 256]);
static PRINT_LEN: StaticCell<usize> = StaticCell::new(0);

/// 本进程是否把打印**镜像**一份到用户态屏幕控制台 (`gfx_srv`)。
///
/// 只有需要上屏的程序 (目前是 shell) 显式打开 —— 每提交一行都要多一次 `SYS_CALL` 往返,
/// 让自测那种成千上万条的路径去付这个代价不划算。
static SCREEN_MIRROR: StaticCell<bool> = StaticCell::new(false);

/// 打开屏幕镜像 (幂等)。
///
/// **前提**: 帧缓冲已交用户态 ([`sys_console_ready`]) 且 `gfx_srv` 活着 —— 镜像走 `SYS_CALL`,
/// 目标域若没有活任务, 调用方会一直等回复。
pub fn screen_mirror_on() {
    *SCREEN_MIRROR.borrow_mut() = true;
}

/// 打印的唯一出口: 内核终端 (经 `SYS_PUTS`, 内核顺带写 COM1) —— 开了镜像再送一份给屏幕控制台。
fn sink(s: &str) {
    sys_puts(s);
    if *SCREEN_MIRROR.borrow_mut() {
        // 服务端逐像素回读校验; 失败也不影响串口这条主路, 故忽略返回值。
        crate::gfx::print(s);
    }
}

/// 把字符串追加到行缓冲 (缓冲满时先提交当前行, 再继续写入)。
fn print_push(s: &str) {
    for &b in s.as_bytes() {
        let buf = PRINT_BUF.borrow_mut();
        let len = PRINT_LEN.borrow_mut();
        if *len >= buf.len() {
            // 缓冲已满: 先提交当前行, 避免后续字节被静默丢弃。
            let line = unsafe { core::str::from_utf8_unchecked(&buf[..*len]) };
            sink(line);
            *len = 0;
        }
        buf[*len] = b;
        *len += 1;
    }
}

/// 提交当前行缓冲 (整行一次 syscall), 然后清空。
fn print_flush() {
    let len = PRINT_LEN.borrow_mut();
    if *len > 0 {
        let buf = PRINT_BUF.borrow_mut();
        let s = unsafe { core::str::from_utf8_unchecked(&buf[..*len]) };
        sink(s);
        *len = 0;
    }
}

pub fn print(s: &str) {
    print_push(s);
}

pub fn println(s: &str) {
    print_push(s);
    print_push("\n");
    print_flush();
}

/// 立即提交当前行缓冲 (把尚未以换行结束的内容送出)。用于「行内提示符」——
/// 提示符须在用户输入前显示出来, 不能等换行。
pub fn flush() {
    print_flush();
}

/// 以十进制打印无符号整数。
pub fn print_u64(mut v: u64) {
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
    let s = unsafe { core::str::from_utf8_unchecked(&buf[i..]) };
    print_push(s);
}

/// 以十六进制打印无符号整数。
pub fn print_hex(mut v: u64) {
    let mut buf = [0u8; 16];
    let mut i = buf.len();
    loop {
        i -= 1;
        let d = (v & 0xF) as u8;
        buf[i] = if d < 10 { b'0' + d } else { b'a' + d - 10 };
        v >>= 4;
        if v == 0 {
            break;
        }
    }
    let s = unsafe { core::str::from_utf8_unchecked(&buf[i..]) };
    print_push(s);
}
