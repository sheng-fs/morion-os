//! 网络端口归属 (N6 · 网络能力)。
//!
//! 微内核不实现协议栈，但**端口是内核要守的命名资源**：否则任何能向 `netstack_srv`
//! 发消息的域都能冒充别人绑任意端口。这里登记「哪个域绑定了哪个端口」，
//! 由 [`crate::cap::Capability::Net`] 能力作凭证：
//!
//! - `SYS_NET_BIND(port)`：调用者须持覆盖 `port` 的 `Net` 能力，登记归属后返回 1；
//! - `SYS_NET_OWNER(port)`：只读查询端口归属域，供 `netstack_srv` 在收到 `bind` 请求时
//!   核对 `msg.from` —— 二者配合，端口门禁**不可伪造**（应用无法绕过内核直接让 netstack
//!   替它绑一个自己没权限的端口）。

use alloc::vec::Vec;
use spin::Mutex;

/// 同时存在的端口绑定上限（每个都是 16 位端口 + 归属域）。
const MAX_BINDINGS: usize = 64;

/// 一条端口绑定记录。
#[derive(Clone, Copy)]
struct Binding {
    domain: u64,
    port: u16,
}

/// 全局端口归属表（按绑定次序线性存放；N6 规模下线性扫描足够）。
static BINDINGS: Mutex<Vec<Binding>> = Mutex::new(Vec::new());

/// 初始化（清空归属表）。
pub fn init() {
    BINDINGS.lock().clear();
}

/// 域 `domain` 绑定端口 `port`。
///
/// 前置：`domain` 持有覆盖 `port` 的 [`crate::cap::Capability::Net`] 能力（不可伪造）。
/// 已被**其它**域占用则失败；本域重复绑定同一端口幂等成功。
pub fn bind(domain: u64, port: u16) -> bool {
    if !crate::cap::has_net_port(domain, port) {
        return false;
    }
    let mut b = BINDINGS.lock();
    if b.iter().any(|x| x.port == port) {
        // 已被占用: 只有归属者本域重复 bind 才算成功（幂等），别的域一律拒绝。
        return b.iter().any(|x| x.port == port && x.domain == domain);
    }
    if b.len() >= MAX_BINDINGS {
        return false;
    }
    b.push(Binding { domain, port });
    true
}

/// 查询端口 `port` 的归属域；未绑定返回 `u64::MAX`。
pub fn owner(port: u16) -> u64 {
    let b = BINDINGS.lock();
    b.iter()
        .find(|x| x.port == port)
        .map(|x| x.domain)
        .unwrap_or(u64::MAX)
}

/// 域销毁时清掉它的全部绑定（域号会被复用，不清会让新域继承旧归属）。
pub fn remove_domain(domain: u64) {
    BINDINGS.lock().retain(|x| x.domain != domain);
}
