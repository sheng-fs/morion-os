//! 内核 → 用户态驱动的**通用设备授权描述**（D1 的交接结构）。
//!
//! 本文件是 [`DeviceGrant`] 在用户态的**唯一来源**：`block_srv`（NVMe）与 `net_srv`
//! （virtio-net）都从这里取，字段与内核 `kernel/src/device.rs` 的 `DeviceGrant` 严格对齐
//! （`#[repr(C)]`）。内核只交出"BAR + 连续 DMA 块 + MSI-X 参数"，**不含设备语义**。

/// 描述结构映射到每个驱动域的**固定虚拟地址**（= 内核 `device::DEVICE_CFG_VADDR`
/// = `USER_SPACE_BASE + 0x81_0000`）。
pub const DEVICE_CFG_VADDR: u64 = 0x0000_0080_0081_0000;

/// 运行期设备授权 syscall 号（= 内核 `syscall::SYS_DEVICE_GRANT`，D1b）。
///
/// libdevice 保持**零依赖**（不引 `morion`），故这里本地定义编号 + 裸 `syscall` 指令，
/// 与 `user/libmorion/src/syscall.rs` 的编号表手工对齐。
const SYS_DEVICE_GRANT: u64 = 53;

/// `SYS_DEVICE_GRANT` 的 `a1` 哨兵: 申请**本域**已被绑定的设备
/// （= 内核 `device::DEVICE_SELF`）。
const DEVICE_SELF: u64 = u64::MAX;

/// 发一次 `SYS_DEVICE_GRANT`（`a1 = 设备选择`，`a2`/`a3` 保留），
/// 返回可用授权描述页地址（`0` = 无设备 / 门禁拒绝）。
#[inline(always)]
fn sys_device_grant(a1: u64) -> u64 {
    let ret: u64;
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") SYS_DEVICE_GRANT => ret,
            // rdi/rsi/rdx 是 syscall 参数寄存器, 内核入口会改写它们 (同 libmorion)。
            inout("rdi") a1 => _,
            inout("rsi") 0u64 => _,
            inout("rdx") 0u64 => _,
            // rcx/r11 被 `syscall` 指令本身改写; r8/r9/r10 是 caller-saved 且内核不保存。
            lateout("rcx") _,
            lateout("r11") _,
            lateout("r8") _,
            lateout("r9") _,
            lateout("r10") _,
            options(nostack)
        );
    }
    ret
}

/// 描述结构 magic（= 内核 `device::DEVICE_GRANT_MAGIC`，`"DEVOS!1"`）。
///
/// 既用来校验内核与用户态布局一致，也用来判断"内核到底有没有授权设备"：没有设备时内核走
/// `grant_empty`（映射一页全零描述），故 `magic == 0` 即"无设备，请优雅退出"。
pub const DEVICE_GRANT_MAGIC: u64 = 0x0044_4556_4F53_2131;

/// 内核写入、用户态读取的设备授权描述（与内核逐字段对齐）。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct DeviceGrant {
    pub magic: u64,
    pub bar_paddr: u64,
    pub bar_vaddr: u64,
    pub bar_bytes: u64,
    pub dma_paddr: u64,
    pub dma_vaddr: u64,
    pub dma_bytes: u64,
    /// MSI-X 向量段基址（0 = 未启用 MSI-X）。
    pub msix_vector_base: u32,
    /// 内核分配的向量条数（0 = 未启用）。
    pub msix_vector_count: u32,
    /// MSI-X 表相对其所在 BAR 的字节偏移。
    pub msix_table_offset: u32,
    /// MSI-X 中断消息地址（低 32 位；物理目的模式，高 32 位恒 0）。
    pub msix_msg_addr: u32,
    /// 页大小（恒 4096）。
    pub page_size: u32,
    pub _reserved: u32,
    /// MSI-X **表所在 BAR** 映射到本域的虚拟地址（表在该 BAR 的 `msix_table_offset` 处）。
    /// 0 = 未映射；表与设备 BAR 同根时等于 `bar_vaddr`。
    pub msix_table_vaddr: u64,
}

impl DeviceGrant {
    /// 向内核**申请本域设备授权**并读出描述（D1b 运行期路径）。
    ///
    /// 内核侧做能力门禁（须持有该设备 BAR 的 `Mmio` 凭证），通过后返回描述页地址；无设备
    /// 或被拒返回 `0`，此时回一份全零描述（[`is_valid`](Self::is_valid) == `false`），
    /// 与旧行为"读到 `grant_empty` 的全零页"一致 —— 驱动无需改动。
    pub fn load() -> Self {
        let page = sys_device_grant(DEVICE_SELF);
        if page == 0 {
            return unsafe { core::mem::zeroed() };
        }
        unsafe { core::ptr::read_volatile(page as *const DeviceGrant) }
    }

    /// 是否是有效授权（magic 匹配）；不匹配表示内核没授权设备（`grant_empty`）。
    pub fn is_valid(&self) -> bool {
        self.magic == DEVICE_GRANT_MAGIC
    }

    /// 页大小（内核填；缺省 4096）。
    pub fn page(&self) -> u64 {
        if self.page_size == 0 {
            4096
        } else {
            self.page_size as u64
        }
    }
}
