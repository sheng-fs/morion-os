//! 域 24 — e1000_srv：第三台网卡驱动（Intel 82540EM，驱动路线 **DRV-B**）。
//!
//! 与 `e1000e_srv`（82574L）同属 8254x 家族、寄存器模型一致，故共用 [`crate::intel_nic`]
//! 核心 —— 本文件只声明型号名与自测 marker。DRV-A 之后，加这台网卡**不改协议栈**：
//! 内核探测后往只读 NIC 表追加一条，`netstack_srv` 即把它当 NIC2 出口。
//!
//! 自测 marker：`NET15 e1000(82540EM) OK … ARP reply OK`。

pub fn run() {
    crate::intel_nic::run("e1000", "NET15 e1000(82540EM)");
}
