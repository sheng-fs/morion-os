//! 域 22 — e1000e_srv：第二台**真网卡**驱动（Intel 82574L，驱动路线 **N9**）。
//!
//! 寄存器模型 / 描述符环 / 自测全部在共享的 8254x 核心 [`crate::intel_nic`] 里
//! （与 `e1000_srv` 的 82540EM 共用）。本文件只声明型号名与自测 marker。
//!
//! 自测 marker：`NET9 e1000e OK … ARP reply OK`。

pub fn run() {
    crate::intel_nic::run("e1000e", "NET9 e1000e");
}
