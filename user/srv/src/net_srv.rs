//! 域 16 — 网络驱动服务（virtio-net，驱动路线 **N0–N3**）。
//!
//! **N0**：只起域 + 报到 + 保持存活 —— 先把"多一个引导期服务域"这条链路
//! （引导器服务表 / 内核建域 / init 监督 / 回归基线）打通，**功能零变化**。
//! **N1（本步）**：内核按类找到 virtio-net（`find_net`）、用通用 `device::grant` 交出
//! `DeviceGrant`；本服务读描述并打印取证（BAR / DMA / MSI-X 参数）—— 仍未碰设备。
//! **N2**：读描述 → 解析 virtio PCI 能力 → 取 MAC → 建 RX/TX virtqueue → 收中断。
//! **N3**：发 ARP 请求 → 收应答（端到端取证）。

use crate::common::USER_DATA_BASE;
use morion::syscall::*;

/// 内核映射到本域的**通用设备授权描述**虚拟地址（见 kernel/src/device.rs, 属 USER_DATA_BASE 区）。
const DEVICE_CFG_VADDR: u64 = USER_DATA_BASE + 0x1_0000;
/// 描述结构 magic 校验值（与内核 `device::DEVICE_GRANT_MAGIC` 一致）。
const DEVICE_GRANT_MAGIC: u64 = 0x0044_4556_4F53_2131;

/// 内核写入、用户态读取的**通用设备授权描述**（与 kernel/src/device.rs 布局完全一致）。
///
/// D1 起内核不知道 virtio-net 的事: 它只交出"BAR + DMA 块 + MSI-X 参数", 队列怎么排、
/// 寄存器怎么写全由驱动决定 —— 这就是"加新驱动不必改内核"的前提。
#[repr(C)]
#[derive(Clone, Copy)]
#[allow(dead_code)]
struct DeviceGrant {
    magic: u64,
    bar_paddr: u64,
    bar_vaddr: u64,
    bar_bytes: u64,
    dma_paddr: u64,
    dma_vaddr: u64,
    dma_bytes: u64,
    msix_vector_base: u32,
    msix_vector_count: u32,
    msix_table_offset: u32,
    msix_msg_addr: u32,
    page_size: u32,
    _reserved: u32,
}

/// 域 16 — net_srv: 读内核交出的设备授权并取证（N2 起这里换成 virtio-net 驱动循环）。
pub fn run() {
    let g = unsafe { core::ptr::read_volatile(DEVICE_CFG_VADDR as *const DeviceGrant) };
    if g.magic != DEVICE_GRANT_MAGIC {
        // 内核没授权 = 机器上没有 virtio-net（或 BAR4 不可用）: 保持存活即可。
        println("net: no device grant (no virtio-net), idle");
        loop {
            sys_sleep(500);
        }
    }

    print("net: device grant bar_vaddr=0x");
    print_hex(g.bar_vaddr);
    print(" bar_paddr=0x");
    print_hex(g.bar_paddr);
    print(" dma_bytes=");
    print_u64(g.dma_bytes);
    print(" msix_vec=");
    print_u64(g.msix_vector_base as u64);
    print(" msix_count=");
    print_u64(g.msix_vector_count as u64);
    println("");
    println("net: net_srv up (N1 grant acquired; virtio-net driver lands in N2)");
    loop {
        sys_sleep(500);
    }
}
