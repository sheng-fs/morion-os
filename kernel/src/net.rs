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

/// DRV-A — 内核写给协议栈的**只读 NIC 表**页（表驱动网卡接线的数据面）。
///
/// 放在用户数据区（`USER_SPACE_BASE + 0x85_0000`，紧随 `DEVICE_MSIX_VADDR` 之后），
/// 与 `DEVICE_CFG_VADDR` 同构：内核探测完网卡后分配一帧、填表、只读映射进 `netstack_srv`
/// （域 21）。协议栈因此**不再硬编码**「网卡域号 / IO 页 VA / 网卡数量」——加一台网卡只需
/// 内核探测后往表里追加一条。
pub const NIC_TABLE_VADDR: u64 = crate::memory::paging::USER_SPACE_BASE + 0x85_0000;
/// NIC 表 magic（"NICTB01"）。
pub const NIC_TABLE_MAGIC: u64 = 0x004E_4943_5442_3031;
/// 表中网卡条目上限（定长，避免变长结构跨模块布局不一致）。
pub const NIC_MAX: usize = 8;
/// 网卡型号：virtio-net（net_srv，域 16）。
pub const NIC_KIND_VIRTIO_NET: u64 = 1;
/// 网卡型号：Intel e1000e（e1000e_srv，域 22）。
pub const NIC_KIND_E1000E: u64 = 2;
/// 网卡型号：Intel e1000 82540EM（e1000_srv，域 24，DRV-B）。
pub const NIC_KIND_E1000: u64 = 3;

/// 一条网卡接线：驱动服务域 + 帧共享页 VA + 型号。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct NicEntry {
    pub domain: u64,
    pub io_vaddr: u64,
    pub kind: u64,
}

/// NIC 表（逐字段与 `user/srv/src/common.rs::NicTable` 对齐）。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct NicTable {
    pub magic: u64,
    pub count: u64,
    pub entries: [NicEntry; NIC_MAX],
}

/// 把探测到的网卡组装成 NIC 表，写入一帧并只读映射进 `netstack_domain`。
///
/// 条目按探测顺序排列，第 i 条的 IO 页 = `USER_SPACE_BASE + 0x1A_0000 + i * 4 KiB`
/// （与协议栈历史约定一致，保证回归逐字不变）。
pub fn publish_nic_table(netstack_domain: u64, entries: &[NicEntry]) -> bool {
    let paddr = match crate::memory::frame_allocator::allocate_frame() {
        Some(p) => p,
        None => return false,
    };
    let mut tbl = NicTable {
        magic: NIC_TABLE_MAGIC,
        count: 0,
        entries: [NicEntry {
            domain: 0,
            io_vaddr: 0,
            kind: 0,
        }; NIC_MAX],
    };
    let mut i = 0;
    while i < entries.len() && i < NIC_MAX {
        tbl.entries[i] = entries[i];
        i += 1;
    }
    tbl.count = i as u64;
    unsafe {
        core::ptr::write(paddr as *mut NicTable, tbl);
    }
    crate::memory::paging::map_user_page(
        netstack_domain,
        NIC_TABLE_VADDR,
        paddr,
        crate::memory::paging::UserPagePerm::ReadOnly,
    );
    true
}

/// NIC 表里第 `index` 条网卡的 IO 帧共享页 VA（与协议栈约定一致）。
pub const fn nic_io_vaddr(index: u64) -> u64 {
    crate::memory::paging::USER_SPACE_BASE + 0x1A_0000 + index * 0x1000
}

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
