//! 域 16 — 网络驱动服务（virtio-net，驱动路线 **N0–N3**）。
//!
//! **N0（本步）**：只起域 + 报到 + 保持存活 —— 先把"多一个引导期服务域"这条链路
//! （引导器服务表 / 内核建域 / init 监督 / 回归基线）打通，**功能零变化**。
//! **N1**：内核按类找到 virtio-net 并用通用 `device::grant` 交出 `DeviceGrant`。
//! **N2**：读描述 → 解析 virtio PCI 能力 → 取 MAC → 建 RX/TX virtqueue → 收中断。
//! **N3**：发 ARP 请求 → 收应答（端到端取证）。

use morion::syscall::*;

/// 域 16 — net_srv: 保持存活（N2 起这里换成 virtio-net 驱动循环）。
pub fn run() {
    println("net: net_srv up (N0 skeleton; virtio-net driver lands in N2)");
    loop {
        sys_sleep(500);
    }
}
