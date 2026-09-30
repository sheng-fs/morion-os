//! 内核 → 用户态驱动的**通用设备授权描述**（D1 的交接结构）。
//!
//! 本文件是 [`DeviceGrant`] 在用户态的**唯一来源**：`block_srv`（NVMe）与 `net_srv`
//! （virtio-net）都从这里取，字段与内核 `kernel/src/device.rs` 的 `DeviceGrant` 严格对齐
//! （`#[repr(C)]`）。内核只交出"BAR + 连续 DMA 块 + MSI-X 参数"，**不含设备语义**。

/// 描述结构映射到每个驱动域的**固定虚拟地址**（= 内核 `device::DEVICE_CFG_VADDR`
/// = `USER_SPACE_BASE + 0x81_0000`）。
pub const DEVICE_CFG_VADDR: u64 = 0x0000_0080_0081_0000;

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
    /// 从本域固定地址 [`DEVICE_CFG_VADDR`] 读一份授权描述。
    pub fn load() -> Self {
        unsafe { core::ptr::read_volatile(DEVICE_CFG_VADDR as *const DeviceGrant) }
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
