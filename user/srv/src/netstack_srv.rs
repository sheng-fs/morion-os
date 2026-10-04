//! 域 21 — netstack_srv：用户态网络协议栈（N6）。
//!
//! 架构（见 `docs/plan-network.md`）：
//!
//! ```text
//! 应用 ── libnetv ──▶ netstack_srv(本服务: ARP/IPv4/UDP + socket + 端口能力门禁)
//!                          │  帧级 IPC (NetReq, 帧走共享页)
//!                          ▼
//!                     net_srv(域 16: 纯 NIC 驱动)
//! ```
//!
//! **N6.4**：先打通「驱动 ↔ 协议栈」的**帧级 IPC 通道** —— 与 net_srv 共享一页、取回网卡
//! MAC。后续 N6.5/N6.6 在此之上实现 UDP socket 服务与端口能力门禁。

use crate::common::*;
use morion::syscall::*;

/// net_srv 域号（帧级 NIC 驱动）。
const NET_DOMAIN: u64 = 16;

/// 与 net_srv 传递帧的共享页（同址共享）。
const IO_VADDR: u64 = 0x0000_0080_001A_0000;

/// 取 MAC 的重试次数与间隔（net_srv 可能还在跑 NET1..4 自测，稍等即可）。
const MAC_RETRY: u64 = 200;
const MAC_WAIT_MS: u64 = 50;

/// 永不返回的保活循环（无网卡 / 初始化失败时用）。
fn idle() -> ! {
    loop {
        sys_sleep(500);
    }
}

/// 向 net_srv 发一条 `NetReq` 并取回复（帧数据经共享页 `IO_VADDR`）。
fn net_call(op: u64, len: u64) -> u64 {
    let req = NetReq {
        op,
        len,
        buf: IO_VADDR,
    };
    let payload = unsafe {
        core::slice::from_raw_parts(
            &req as *const NetReq as *const u8,
            core::mem::size_of::<NetReq>(),
        )
    };
    sys_call_payload(NET_DOMAIN, NET_REQ_TAG, payload)
}

/// 域 21 — netstack_srv 入口。
pub fn run() {
    // 与 net_srv 共享一页收发帧。
    if sys_alloc_page(IO_VADDR) != 1 || sys_share_page(IO_VADDR, NET_DOMAIN) != 1 {
        println("netstack: cannot share IO page with net_srv, idle");
        idle();
    }

    // 取网卡 MAC（INFO 在 net_srv 自测期也应答，故重试直到拿到）。
    let mut mac = 0u64;
    let mut i = 0u64;
    while i < MAC_RETRY {
        mac = net_call(NET_OP_INFO, 0);
        if mac != 0 {
            break;
        }
        sys_sleep(MAC_WAIT_MS);
        i += 1;
    }
    if mac == 0 {
        println("netstack: no NIC (net_srv gave no MAC), idle");
        idle();
    }

    print("netstack: up (frame link to net_srv OK), nic mac=0x");
    print_hex(mac);
    println("");

    idle();
}
