//! MSI-X 表项写入。
//!
//! 分工：**表由驱动写**（表所在 BAR 已非缓存地映射给驱动，内核自己到不了那个 BAR），
//! 而**配置空间写留在内核**（`SYS_MSIX_ENABLE`）—— 驱动写完表项后请内核打开 MSI-X。

use crate::grant::DeviceGrant;
use crate::mmio::wr32;

/// 写一条 MSI-X 表项：消息地址 + 数据（= 中断向量号）+ 不屏蔽。
///
/// 表在 `grant.msix_table_vaddr + grant.msix_table_offset` 处；表与设备 BAR 同根时
/// `msix_table_vaddr == bar_vaddr`，不同根时（如 virtio-net 的表在 BAR1）内核会另行映射并填好。
pub fn write_table_entry(grant: &DeviceGrant, entry: u64, vector: u64) {
    let base = grant.msix_table_vaddr + grant.msix_table_offset as u64 + entry * 16;
    wr32(base, grant.msix_msg_addr); // 消息地址 (低 32 位)
    wr32(base + 4, 0); // 消息地址 (高 32 位); 物理目的模式恒 0
    wr32(base + 8, vector as u32); // 消息数据 = 中断向量
    wr32(base + 12, 0); // 向量控制: bit0=1 屏蔽 → 0 = 不屏蔽
}
