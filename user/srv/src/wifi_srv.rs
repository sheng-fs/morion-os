//! 域 25 — wifi_srv：无线 station 服务（W1，plan §3.4「只备抽象」）。
//!
//! **本轮不做真驱动**：QEMU 无 802.11 设备，真机需 vfio 直通真卡 + 固件（`.ucode`）加载 +
//! 完整的 802.11 关联 / WPA 栈 —— 体量≈一个子系统（见 plan-net-v6.md §3.4）。本服务做三件事：
//!
//! 1. **固化 L2 链路契约**：与 net_srv 同契约 —— `NET_REQ_TAG` + `NetReq{op,len,buf}`，
//!    `NET_OP_TX/RX/INFO`。协议栈对「有线 / 无线」一视同仁（见 common.rs 的契约注释）。
//! 2. **定义「关联即链路」模型**：无线 station **关联成功后**应把自己注册成 NIC 表里的一条
//!    `NIC_KIND_WIFI` 条目（`kernel/src/net.rs::NicEntry`）—— 抽象层不注册，因为无 radio。
//! 3. **控制面 IPC**：`WIFI_OP_STATUS/SCAN/ASSOC/DISCONNECT` 的**空实现**：无 radio 时
//!    STATUS 报 `radio=0`、SCAN 返 0 条、ASSOC/DISCONNECT 返 0（失败）。
//!
//! 真机落地时只换 radio 后端（真驱动 + 固件），本文件的 L2 / 控制面契约不变。

use crate::common::*;
use morion::syscall::*;

/// 无线控制面：把状态快照写进调用方共享页；返回 1（成功写出）。
fn wifi_status(req: &WifiReq) -> u64 {
    let st = WifiStatus {
        radio: 0, // 抽象层：本机无可用 radio（真驱动接入后置 1）
        firmware: 0,
        associated: 0,
        ssid_len: 0,
        ssid: [0; 32],
    };
    unsafe {
        core::ptr::write_unaligned(req.buf as *mut WifiStatus, st);
    }
    1
}

/// 无线控制面分发。`SCAN`/`ASSOC`/`DISCONNECT` 本轮为空实现（返 0）。
fn wifi_serve(req: &WifiReq) -> u64 {
    match req.op {
        WIFI_OP_STATUS => wifi_status(req),
        WIFI_OP_SCAN => 0,       // 无 radio → 0 个 BSS
        WIFI_OP_ASSOC => 0,      // 无 radio → 关联失败
        WIFI_OP_DISCONNECT => 0, // 无 radio → 无操作
        _ => 0,
    }
}

/// L2 链路契约（驱动 ↔ 协议栈）。未关联时无链路：TX 丢弃、RX 无帧、INFO 报 MAC=0。
fn l2_serve(req: &NetReq) -> u64 {
    match req.op {
        NET_OP_TX => 0,
        NET_OP_RX => 0,
        NET_OP_INFO => 0,
        _ => 0,
    }
}

/// 域 25 — wifi_srv 入口。
pub fn run() {
    println("WIFI0 wifi_srv up (abstraction only: no radio, no firmware)");
    loop {
        let mut msg = Message {
            from: 0,
            to: 0,
            tag: 0,
            payload: [0; PAYLOAD_LEN],
        };
        if sys_try_recv(&mut msg as *mut Message as *mut u8) == u64::MAX {
            sys_sleep(20);
            continue;
        }
        let reply = match msg.tag {
            NET_REQ_TAG => {
                let req: NetReq =
                    unsafe { core::ptr::read_unaligned(msg.payload.as_ptr() as *const NetReq) };
                l2_serve(&req)
            }
            WIFI_REQ_TAG => {
                let req: WifiReq =
                    unsafe { core::ptr::read_unaligned(msg.payload.as_ptr() as *const WifiReq) };
                wifi_serve(&req)
            }
            _ => 0,
        };
        let _ = sys_reply(reply);
    }
}
