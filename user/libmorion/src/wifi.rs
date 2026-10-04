//! 无线控制面客户端（`wifi_srv`，域 25）—— plan §3.4 无线抽象层的用户侧。
//!
//! 与 `net` 客户端同一套「共享页 + 同步 IPC」模式：请求经 `sys_call_payload` 送到 wifi_srv，
//! 结构化结果（状态 / 扫描列表）写回**本域的共享页**（按域派生，互不覆盖）。
//!
//! 抽象层语义：无硬件时 [`status`] 返回 `radio=0`；[`scan`] 返回 0 条；[`assoc`] 返回 `false`。
//! 真机接入前后**接口不变**，上层（shell / app）无需改动。

use crate::syscall::*;

/// wifi_srv 域号（与内核建域顺序一致）。
pub const WIFI_DOMAIN: u64 = 25;

/// 控制面请求 tag 与 op（与 `common.rs` 逐字段一致）。
const WIFI_REQ_TAG: u64 = 0x5749_4649;
const WIFI_OP_STATUS: u64 = 0;
const WIFI_OP_SCAN: u64 = 1;
const WIFI_OP_ASSOC: u64 = 2;
const WIFI_OP_DISCONNECT: u64 = 3;

/// 本域负载共享页 VA（按域派生；选在远离其它固定映射的空档 `USER_BASE + 0xA0_0000`）。
const SHARE_BASE: u64 = 0x0000_0080_00A0_0000;
fn share_vaddr() -> u64 {
    SHARE_BASE + crate::syscall::domain_id() * 0x1000
}

/// 无线状态快照（与 `common::WifiStatus` 逐字段一致）。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct WifiStatus {
    pub radio: u64,      // 1 = 本机有可用 radio（真驱动）；抽象层恒 0
    pub firmware: u64,   // 固件是否已加载
    pub associated: u64, // 是否已关联（关联成功才成为一条 Link）
    pub ssid_len: u64,
    pub ssid: [u8; 32],
}

impl WifiStatus {
    pub const fn zeroed() -> WifiStatus {
        WifiStatus {
            radio: 0,
            firmware: 0,
            associated: 0,
            ssid_len: 0,
            ssid: [0; 32],
        }
    }
    /// 当前 SSID（按 `ssid_len` 截断）。
    pub fn ssid_str(&self) -> &[u8] {
        let n = (self.ssid_len as usize).min(32);
        &self.ssid[..n]
    }
}

/// 单条扫描结果（与 `common::WifiBss` 逐字段一致）。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct WifiBss {
    pub ssid_len: u64,
    pub rssi: u64,
    pub channel: u64,
    pub ssid: [u8; 32],
}

impl WifiBss {
    pub const fn zeroed() -> WifiBss {
        WifiBss {
            ssid_len: 0,
            rssi: 0,
            channel: 0,
            ssid: [0; 32],
        }
    }
    pub fn ssid_str(&self) -> &[u8] {
        let n = (self.ssid_len as usize).min(32);
        &self.ssid[..n]
    }
}

/// 控制面请求（与 `common::WifiReq` 逐字段一致）。
#[repr(C)]
#[derive(Clone, Copy)]
struct WifiReq {
    op: u64,
    len: u64,
    buf: u64,
}

static mut INITED: bool = false;

fn ensure() -> bool {
    unsafe {
        if INITED {
            return true;
        }
        let va = share_vaddr();
        if sys_alloc_page(va) != 1 || sys_share_page(va, WIFI_DOMAIN) != 1 {
            return false;
        }
        INITED = true;
        true
    }
}

fn call(op: u64, len: u64) -> u64 {
    let req = WifiReq {
        op,
        len,
        buf: share_vaddr(),
    };
    let bytes = unsafe {
        core::slice::from_raw_parts(
            &req as *const WifiReq as *const u8,
            core::mem::size_of::<WifiReq>(),
        )
    };
    sys_call_payload(WIFI_DOMAIN, WIFI_REQ_TAG, bytes)
}

/// 读无线状态；服务不可达时返回 `None`。
pub fn status() -> Option<WifiStatus> {
    if !ensure() {
        return None;
    }
    if call(WIFI_OP_STATUS, 0) == 0 {
        return None;
    }
    Some(unsafe { core::ptr::read_unaligned(share_vaddr() as *const WifiStatus) })
}

/// 扫描可见 BSS，写入 `out`，返回条数（抽象层恒 0）。
pub fn scan(out: &mut [WifiBss]) -> usize {
    if !ensure() {
        return 0;
    }
    let n = call(WIFI_OP_SCAN, 0) as usize;
    let m = n.min(out.len());
    unsafe {
        core::ptr::copy_nonoverlapping(share_vaddr() as *const WifiBss, out.as_mut_ptr(), m);
    }
    m
}

/// 关联一个 SSID（WPA2-PSK）。抽象层恒失败（无 radio）。
///
/// `ssid` 写入共享页 `[0..32]`、`psk` 写入 `[32..32+len]`（供真驱动将来取用）。
pub fn assoc(ssid: &[u8], psk: &[u8]) -> bool {
    if !ensure() {
        return false;
    }
    let va = share_vaddr();
    unsafe {
        let base = va as *mut u8;
        core::ptr::write_bytes(base, 0, 96);
        let sn = ssid.len().min(32);
        core::ptr::copy_nonoverlapping(ssid.as_ptr(), base, sn);
        let pn = psk.len().min(64);
        core::ptr::copy_nonoverlapping(psk.as_ptr(), base.add(32), pn);
    }
    call(WIFI_OP_ASSOC, psk.len().min(64) as u64) == 1
}

/// 断开当前关联（抽象层无操作）。
pub fn disconnect() -> bool {
    if !ensure() {
        return false;
    }
    call(WIFI_OP_DISCONNECT, 0) == 1
}
