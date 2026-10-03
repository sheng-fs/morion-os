//! **通用设备授权**（驱动通用化 D1）。
//!
//! 内核在这里只做"把一台 PCI 设备**安全地交出去**"这件事, 且**不含任何设备专属逻辑**
//! （NVMe 的队列页数、寄存器偏移、协议全在用户态驱动里）:
//!
//!   1. 按 PCI 位置准备 **MSI-X**（向量段按设备从全局段里分配, 不再写死第一条）;
//!   2. 分配**物理连续**的 DMA 块（队列 + 数据缓冲用）;
//!   3. 分配一页**描述结构**（`DeviceGrant`），写入 BAR 与 DMA 的物理/虚拟地址、MSI-X 参数;
//!   4. 把描述页、BAR 窗口（`map_mmio`，非缓存）与 DMA 块映射进请求域;
//!   5. 签发 `Mmio(bar)`（以及 MSI-X 时 `Irq(vector)`）能力。
//!
//! 用户态驱动读到 `DeviceGrant` 后**自行决定**怎么在 DMA 块里排版队列、怎么初始化控制器。
//! 于是加一台新驱动**不必再动内核**（boot 侧只需给一条"设备需求"声明，见 `main.rs`）。
//!
//! ## 虚拟地址约定
//!
//! 描述页 / BAR 窗口 / DMA 窗口在此**按域固定**（`DEVICE_*_VADDR`），当前一个域只授一台设备;
//! 一台域要接多台设备时，把这些窗口按设备实例偏移即可（还没到那一步，先不做）。

use alloc::vec::Vec;

use crate::arch::{apic, idt, pci};
use crate::memory::{frame_allocator, paging};

/// 页大小 (字节)。
const PAGE: u64 = 4096;

/// 描述页 / BAR 窗口 / DMA 窗口在请求域内的约定虚拟地址。
///
/// 布置在 `USER_BASE + 8 MiB` 起的高地址数据区, 远离用户程序镜像 (自 `USER_BASE`
/// 起且随代码增长) 与用户栈 (`USER_BASE + 4 MiB`), 避免镜像增长后踩到这些固定映射。
/// 共享页 (`USER_BASE + 0x80_0000`) 亦属同一数据区, 由 sender/receiver 使用。
pub const DEVICE_CFG_VADDR: u64 = paging::USER_SPACE_BASE + 0x81_0000;
pub const DEVICE_BAR_VADDR: u64 = paging::USER_SPACE_BASE + 0x82_0000;
pub const DEVICE_DMA_VADDR: u64 = paging::USER_SPACE_BASE + 0x83_0000;
/// MSI-X 表所在 BAR 的映射窗口（仅当表**不在**设备 BAR 上时才用，如 virtio-net 的表在 BAR1）。
pub const DEVICE_MSIX_VADDR: u64 = paging::USER_SPACE_BASE + 0x84_0000;

/// 描述结构 magic (校验内核与用户态布局一致)。
pub const DEVICE_GRANT_MAGIC: u64 = 0x0044_4556_4F53_2131; // "DEVOS!1"

/// 内核写入、用户态读取的**通用设备授权描述**。
///
/// `#[repr(C)]` 保证跨 crate 布局一致。设备如何用这些资源由驱动决定 —— 内核不解释。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct DeviceGrant {
    pub magic: u64,
    /// 被授权 BAR 的物理基址 (页对齐, 即 `bar_index` 那根 BAR)。
    pub bar_paddr: u64,
    /// 该 BAR 映射到驱动域的虚拟地址 (寄存器基址)。
    pub bar_vaddr: u64,
    /// BAR 窗口字节数 (= 映射页数 × 页大小)。
    pub bar_bytes: u64,
    /// DMA 块物理基址 (物理连续、页对齐)。
    pub dma_paddr: u64,
    /// DMA 块映射到驱动域的虚拟地址 (与 `dma_paddr + i*page` 一一对应)。
    pub dma_vaddr: u64,
    /// DMA 块字节数。
    pub dma_bytes: u64,
    /// MSI-X **向量段基址** (0 = 未启用, 驱动走轮询); 第 `i` 条向量 = base + i。
    pub msix_vector_base: u32,
    /// 内核为本设备分配的向量条数 (0 = 未启用)。
    pub msix_vector_count: u32,
    /// MSI-X 表相对 BAR 的字节偏移 (表在 BAR 内, 已随 BAR 一起映射给驱动)。
    pub msix_table_offset: u32,
    /// MSI-X 中断消息地址 (低 32 位; 物理目的模式, 高 32 位恒 0)。
    pub msix_msg_addr: u32,
    /// 页大小 (恒 4096)。
    pub page_size: u32,
    /// 保留 (对齐 / 将来扩展)。
    pub _reserved: u32,
    /// MSI-X 表所在 BAR 映射到驱动域的虚拟地址（表在该 BAR 的 `msix_table_offset` 处）。
    /// 0 = 未映射 (未启用 MSI-X); 表与设备 BAR 同根时 = `bar_vaddr`。
    pub msix_table_vaddr: u64,
}

/// 一次设备授权请求 (由 boot 侧的"设备需求"声明给出)。
pub struct GrantRequest<'a> {
    /// 接收设备的域。
    pub domain: u64,
    /// 设备 PCI 位置 (配置空间访问与 MSI-X 用)。
    pub bus: u8,
    pub dev: u8,
    pub func: u8,
    /// 被授权的 BAR 物理基址 (内部做页对齐)。
    pub bar_paddr: u64,
    /// BAR 需要映射的页数 (寄存器官网 + 门铃 + MSI-X 表须被覆盖)。
    pub bar_pages: u64,
    /// 需要分配的 DMA 页数 (物理连续, 供队列 / 数据缓冲)。
    pub dma_pages: u64,
    /// 期望的 MSI-X 向量条数 (0 = 不要中断)。
    pub msix_vectors: u32,
    /// 日志前缀 (如 `"nvme"`)。
    pub label: &'a str,
}

/// 配置一台设备并授权给 `req.domain`。成功返回 `true`。
pub fn grant(req: GrantRequest) -> bool {
    let bar_paddr = req.bar_paddr & !(PAGE - 1);
    let dma_pages = req.dma_pages.max(1);

    // 0. MSI-X。必须在写描述结构之前完成: 向量 / 表位置要写进描述。
    let msix = setup_msix(
        req.domain,
        req.bus,
        req.dev,
        req.func,
        req.msix_vectors,
        req.bar_pages,
        req.label,
    );

    // 1. 分配物理连续 DMA 内存并清零 (队列内存须从干净状态开始)。
    let dma_paddr = match frame_allocator::allocate_frames(dma_pages as usize) {
        Some(p) => p,
        None => return false,
    };
    unsafe {
        core::ptr::write_bytes(dma_paddr as *mut u8, 0, (dma_pages * PAGE) as usize);
    }

    // 2. 分配描述页。
    let cfg_paddr = match frame_allocator::allocate_frame() {
        Some(p) => p,
        None => return false,
    };

    // 3. 写描述结构到描述页 (恒等映射, 直接以物理地址作为指针写)。
    let desc = DeviceGrant {
        magic: DEVICE_GRANT_MAGIC,
        bar_paddr,
        bar_vaddr: DEVICE_BAR_VADDR,
        bar_bytes: req.bar_pages * PAGE,
        dma_paddr,
        dma_vaddr: DEVICE_DMA_VADDR,
        dma_bytes: dma_pages * PAGE,
        msix_vector_base: msix.vector,
        msix_vector_count: msix.count,
        msix_table_offset: msix.table_offset,
        msix_msg_addr: msix.msg_addr,
        page_size: PAGE as u32,
        _reserved: 0,
        msix_table_vaddr: msix.table_vaddr,
    };
    unsafe {
        core::ptr::write(cfg_paddr as *mut DeviceGrant, desc);
    }

    // 记下"域 ↔ 已授权设备": 既是 `SYS_DEVICE_CONFIG_READ` 的放行依据 (只放行"读自己
    // 那台设备"), 也是 D1b 运行期 `SYS_DEVICE_INFO` / `SYS_DEVICE_GRANT` 的登记来源
    // (查询本域设备 / 能力门禁 / 幂等确认)。
    bind(req.domain, req.bus, req.dev, req.func, bar_paddr, cfg_paddr);

    // 4. 映射描述页、BAR 窗口 (非缓存 MMIO) 与 DMA 各页 (数据: RW + NX)。
    paging::map_user_page(
        req.domain,
        DEVICE_CFG_VADDR,
        cfg_paddr,
        paging::UserPagePerm::ReadWrite,
    );
    for i in 0..req.bar_pages {
        paging::map_mmio(
            req.domain,
            DEVICE_BAR_VADDR + i * PAGE,
            bar_paddr + i * PAGE,
        );
    }
    for i in 0..dma_pages {
        paging::map_user_page(
            req.domain,
            DEVICE_DMA_VADDR + i * PAGE,
            dma_paddr + i * PAGE,
            paging::UserPagePerm::ReadWrite,
        );
    }

    // 5. 授予 MMIO 能力 (允许驱动域后续自行管理映射)。
    crate::cap::grant(req.domain, crate::cap::Capability::Mmio(bar_paddr));

    true
}

/// 无该类型设备时的降级: 仅映射一个**零描述页**到约定地址, 让驱动读到
/// `magic == 0` (不等于 [`DEVICE_GRANT_MAGIC`]) 后自行优雅退出, 而不是读未映射地址
/// 触发缺页死循环。
pub fn grant_empty(domain: u64) {
    let cfg_paddr = match frame_allocator::allocate_frame() {
        Some(p) => p,
        None => return,
    };
    // 分配器不保证新帧内容为 0, 显式清零。
    unsafe {
        core::ptr::write_bytes(cfg_paddr as *mut u8, 0, PAGE as usize);
    }
    paging::map_user_page(
        domain,
        DEVICE_CFG_VADDR,
        cfg_paddr,
        paging::UserPagePerm::ReadWrite,
    );
}

/// 域 → 已授权设备的登记表 (供 [`config_read`] 与运行期 `SYS_DEVICE_*` 使用)。
static BINDINGS: spin::Mutex<Vec<DeviceBinding>> = spin::Mutex::new(Vec::new());

/// 一条"域 ↔ 设备"绑定, 记下运行期 syscall 需要的全部字段。
struct DeviceBinding {
    domain: u64,
    bus: u8,
    dev: u8,
    func: u8,
    /// 被授权 BAR 的页对齐物理基址 —— 也是 `SYS_DEVICE_GRANT` 能力门禁的判据
    /// (`Capability::Mmio(bar_paddr)` 由 [`grant`] 一并签发)。
    bar_paddr: u64,
    /// 描述页物理地址 (内核恒等映射, 可直接读其 magic 做幂等确认)。
    cfg_paddr: u64,
}

/// 记下某域被授权的设备 (同一域重新授权时覆盖)。
fn bind(domain: u64, bus: u8, dev: u8, func: u8, bar_paddr: u64, cfg_paddr: u64) {
    let mut bindings = BINDINGS.lock();
    if let Some(e) = bindings.iter_mut().find(|e| e.domain == domain) {
        e.bus = bus;
        e.dev = dev;
        e.func = func;
        e.bar_paddr = bar_paddr;
        e.cfg_paddr = cfg_paddr;
    } else {
        bindings.push(DeviceBinding {
            domain,
            bus,
            dev,
            func,
            bar_paddr,
            cfg_paddr,
        });
    }
}

/// 读**调用域**被授权设备的 PCI 配置空间 dword (`offset` 内部对齐到 4)。
///
/// 驱动靠它自行解析能力链表 (PCI 通用能力 / 厂商能力, 如 virtio 各 BAR 区域偏移) ——
/// 内核因此不必懂任何设备协议; 只放行"读自己那台设备", 别的设备读不到。
/// 返回 `None` = 本域没有被授权设备。
pub fn config_read(offset: u32) -> Option<u32> {
    let me = crate::scheduler::current_domain();
    let bindings = BINDINGS.lock();
    let e = bindings.iter().find(|e| e.domain == me)?;
    Some(pci::config_read_dword(e.bus, e.dev, e.func, offset as u8))
}

/// MSI-X 的配置结果 (向量 0 = 未启用)。
struct MsixSetup {
    /// 向量段基址 (= 本设备第 0 条向量; 0 = 未启用)。
    vector: u32,
    count: u32,
    table_offset: u32,
    msg_addr: u32,
    /// 表所在 BAR 映射到驱动域的虚拟地址 (0 = 未启用)。
    table_vaddr: u64,
}

const MSIX_DISABLED: MsixSetup = MsixSetup {
    vector: 0,
    count: 0,
    table_offset: 0,
    msg_addr: 0,
    table_vaddr: 0,
};

/// MSI 向量段的**分配游标** (从 [`idt::MSI_VECTOR_BASE`] 起按设备递增)。
///
/// 原来是"每台设备都从段首拿固定几条", 只够一台 NVMe; 改成游标后, 多台设备各有各的段,
/// 段用尽则**降级轮询**(中断化是优化, 不是启动前提)。
static MSI_NEXT: spin::Mutex<u32> = spin::Mutex::new(idt::MSI_VECTOR_BASE as u32);

/// 从全局 MSI 向量段里划出 `n` 条连续的; 不够则返回 `None`。
fn alloc_msi_vectors(n: u32) -> Option<u32> {
    if n == 0 {
        return None;
    }
    let mut next = MSI_NEXT.lock();
    let end = *next + n;
    if end > idt::MSI_VECTOR_BASE as u32 + idt::MSI_VECTOR_COUNT as u32 {
        return None;
    }
    let base = *next;
    *next = end;
    Some(base)
}

/// 待打开的 MSI-X (记下, 由驱动域经 `SYS_MSIX_ENABLE` 在写好表项后打开)。
static MSIX_PENDING: spin::Mutex<Vec<MsixPending>> = spin::Mutex::new(Vec::new());

/// 待打开项: 控制器位置 + MSI-X 能力偏移 + 允许调用 `SYS_MSIX_ENABLE` 的域。
struct MsixPending {
    domain: u64,
    bus: u8,
    dev: u8,
    func: u8,
    cap_ptr: u8,
}

/// 打开 MSI-X (由该设备的驱动域调用, 且必须在它写好 MSI-X 表项之后)。
///
/// 顺序是硬要求: 表项没写好就置 Enable, 设备可能按表里未定义的内容发中断消息。
/// 幂等: 成功后清掉该域的待打开项, 之后的调用返回 `false`。
/// PCI 配置空间写留在内核 —— 驱动只能通过这一个窄接口, 动不了别的设备。
pub fn enable_msix() -> bool {
    let me = crate::scheduler::current_domain();
    let mut pending = MSIX_PENDING.lock();
    let idx = match pending.iter().position(|p| p.domain == me) {
        Some(i) => i,
        None => return false,
    };
    let p = pending.remove(idx);
    drop(pending);
    pci::enable_msix(p.bus, p.dev, p.func, p.cap_ptr);
    crate::video::println("dev: MSI-X enabled (driver wrote table, kernel set config)");
    true
}

/// 为设备准备 MSI-X, 成功返回向量段与表位置, 任一步不满足返回禁用状态。
///
/// 分工: 内核负责 LAPIC、PCI 配置空间与向量段, 驱动负责写设备 MMIO 里的 MSI-X 表 ——
/// 内核自身到不了这个 BAR (由固件分配在 4 GiB 以上, 不在内核的恒等映射内), 而该 BAR
/// 本来就已经非缓存地映射给了驱动。任一步不满足都只**降级** (打印原因 + 返回禁用),
/// 不失败启动 —— 中断化是优化, 轮询才是保底路径。
fn setup_msix(
    domain: u64,
    bus: u8,
    dev: u8,
    func: u8,
    want_vectors: u32,
    bar_pages: u64,
    label: &str,
) -> MsixSetup {
    if want_vectors == 0 {
        return MSIX_DISABLED;
    }
    let cap = match pci::find_msix(bus, dev, func) {
        Some(c) => c,
        None => {
            log(label, ": no MSI-X capability, IRQ mode off (polling)");
            return MSIX_DISABLED;
        }
    };
    if cap.table_size < want_vectors as u16 {
        log(
            label,
            ": MSI-X table too small for requested vectors -> polling",
        );
        return MSIX_DISABLED;
    }
    // 表在哪根 BAR 决定怎么把它交给驱动:
    //   - BIR=0 = 内核已经映射的设备 BAR (NVMe 就是): 直接用它的窗口;
    //   - BIR!=0 = 表在**另一根** BAR 上 (virtio-net 的表在 BAR1): 内核把那根 BAR 也
    //     非缓存地映射给驱动 (窗口 DEVICE_MSIX_VADDR), 驱动写表项时用这个窗口基址。
    let table_bytes = cap.table_offset as u64 + want_vectors as u64 * 16;
    let table_vaddr = if cap.table_bir == 0 {
        if table_bytes > bar_pages * PAGE {
            log(label, ": MSI-X table outside mapped BAR window -> polling");
            return MSIX_DISABLED;
        }
        DEVICE_BAR_VADDR
    } else {
        let msix_bar = match pci::read_bar(bus, dev, func, cap.table_bir) {
            Some(p) => p,
            None => {
                crate::video::print(label);
                crate::video::print(": MSI-X BAR");
                crate::video::print_u64(cap.table_bir as u64);
                crate::video::println(" unreadable -> polling");
                return MSIX_DISABLED;
            }
        };
        let pages = table_bytes.div_ceil(PAGE);
        for i in 0..pages {
            paging::map_mmio(domain, DEVICE_MSIX_VADDR + i * PAGE, msix_bar + i * PAGE);
        }
        DEVICE_MSIX_VADDR
    };
    // LAPIC 是 MSI 消息的接收方, 没有它就没有中断可言。
    if apic::init().is_none() {
        log(label, ": no LAPIC, MSI-X off (polling)");
        return MSIX_DISABLED;
    }
    let vector = match alloc_msi_vectors(want_vectors) {
        Some(v) => v,
        None => {
            log(label, ": MSI vector segment exhausted -> polling");
            return MSIX_DISABLED;
        }
    };

    // 先关 INTx (MSI-X 开起来后 INTx 必须不再触发), 再交给驱动写表、请内核打开。
    pci::disable_intx(bus, dev, func);
    MSIX_PENDING.lock().push(MsixPending {
        domain,
        bus,
        dev,
        func,
        cap_ptr: cap.cap_ptr,
    });
    // 让驱动能 `SYS_REGISTER_IRQ` / `SYS_IRQ_POLL` / `SYS_IRQ_WAIT` 每一条向量。
    for i in 0..want_vectors {
        crate::cap::grant(domain, crate::cap::Capability::Irq((vector + i) as u8));
    }

    crate::video::print(label);
    crate::video::print(": MSI-X prepared vectors=0x");
    crate::video::print_hex(vector as u64);
    crate::video::print("..0x");
    crate::video::print_hex((vector + want_vectors - 1) as u64);
    crate::video::print(" msi_addr=0x");
    crate::video::print_hex(apic::msi_address() as u64);
    crate::video::print(" table_off=0x");
    crate::video::print_hex(cap.table_offset as u64);
    crate::video::print(" table_bar=");
    crate::video::print_u64(cap.table_bir as u64);
    crate::video::print(" table_vaddr=0x");
    crate::video::print_hex(table_vaddr);
    crate::video::print(" entries=");
    crate::video::print_u64(cap.table_size as u64);
    crate::video::println("");
    MsixSetup {
        vector,
        count: want_vectors,
        table_offset: cap.table_offset,
        msg_addr: apic::msi_address(),
        table_vaddr,
    }
}

/// 打印一条带设备前缀的内核日志。
fn log(label: &str, msg: &str) {
    crate::video::print(label);
    crate::video::println(msg);
}

// ---------------------------------------------------------------------------
// 并行开发接线层: 运行期设备授权入口 (D1b)
// ---------------------------------------------------------------------------
// 号与转发臂已在 [`crate::syscall`] 备好 (`SYS_DEVICE_INFO` / `SYS_DEVICE_GRANT`), 所以 D1b
// 任务**只改本文件** (+ `user/libdevice`), 不必去碰 `syscall.rs` —— 见 docs/dev-workflow.md 的「并行协作」。

/// `SYS_DEVICE_*` 的 `a1` 哨兵: 申请 / 查询**本域**已被绑定的那台设备。
///
/// 用 `u64::MAX` 而非 `0` —— `0` 是合法的 `pci_addr` (00:00.0), 不能当哨兵。
pub const DEVICE_SELF: u64 = u64::MAX;

/// 把 PCI 位置打包成 `SYS_DEVICE_*` 的 `pci_addr`: `bus<<16 | dev<<8 | func`。
pub const fn pci_addr(bus: u8, dev: u8, func: u8) -> u64 {
    ((bus as u64) << 16) | ((dev as u64) << 8) | func as u64
}

/// 解开 `pci_addr`; `bus/dev/func` 任一超出位宽 (`dev > 31` / `func > 7`) 返回 `None`。
///
/// 纯函数 (不碰全局表 / 硬件), 便于单测。
fn split_pci_addr(a: u64) -> Option<(u8, u8, u8)> {
    if a >> 24 != 0 {
        return None;
    }
    let bus = ((a >> 16) & 0xFF) as u8;
    let dev = ((a >> 8) & 0xFF) as u8;
    let func = (a & 0xFF) as u8;
    if dev > 31 || func > 7 {
        return None;
    }
    Some((bus, dev, func))
}

/// `a1` 是否指向 `bus:dev.func` 这台设备 (纯函数, 便于单测)。
///
/// `DEVICE_SELF` 恒真; 显式地址必须**精确匹配** —— 域只能认领自己那台设备。
fn selects(a1: u64, bus: u8, dev: u8, func: u8) -> bool {
    a1 == DEVICE_SELF || split_pci_addr(a1) == Some((bus, dev, func))
}

/// 把 `SYS_DEVICE_INFO` 的答复打包进一个 `u64`: `vendor<<48 | device<<32 | class(低 24 位)`。
///
/// `class_code` 取 PCI 配置空间 0x08 处 dword 的高三字节 (class : subclass : prog-if)。
/// 返回 `0` = 无设备 (真实设备 vendor != 0, 且 class 不同时为 0)。
fn pack_info(vendor: u16, device: u16, class_code: u32) -> u64 {
    ((vendor as u64) << 48) | ((device as u64) << 32) | (class_code as u64 & 0x00FF_FFFF)
}

/// `SYS_DEVICE_INFO` 的处理入口（D1b）：查询本域可用设备。
///
/// `a1` = [`DEVICE_SELF`] (本域已绑定设备) 或显式 `pci_addr`; 返回打包的
/// `vendor/device/class` (见 [`pack_info`]), 无设备 / 地址不属于本域返回 `0`。
pub fn syscall_info(a1: u64, _a2: u64, _a3: u64) -> u64 {
    let me = crate::scheduler::current_domain();
    let (bus, dev, func) = {
        let bindings = BINDINGS.lock();
        let Some(e) = bindings.iter().find(|e| e.domain == me) else {
            return 0; // 本域无设备 (boot 期走了 `grant_empty`)。
        };
        if !selects(a1, e.bus, e.dev, e.func) {
            return 0;
        }
        (e.bus, e.dev, e.func)
    };

    let ids = pci::config_read_dword(bus, dev, func, 0x00);
    if (ids & 0xFFFF) == 0xFFFF {
        return 0; // 位置空 (设备已消失)。
    }
    let class = pci::config_read_dword(bus, dev, func, 0x08);
    pack_info((ids & 0xFFFF) as u16, (ids >> 16) as u16, class)
}

/// `SYS_DEVICE_GRANT` 的处理入口（D1b）：申请设备并拿到 `DeviceGrant`。
///
/// `a1` = [`DEVICE_SELF`] 或显式 `pci_addr` (`a2`/`a3` 保留); 成功返回描述页在**本域**
/// 的虚拟地址 ([`DEVICE_CFG_VADDR`]), 无设备 / 被拒返回 `0`。
///
/// 资源 (BAR / 连续 DMA 块 / MSI-X / `Mmio` 能力) 在 boot 期由声明式 [`grant`] 一次性备好;
/// 这里做的是**运行期申请**的完整语义: 定位本域设备 → **能力门禁** (须持有该 BAR 的
/// `Mmio` 凭证) → 幂等确认描述页确带授权 (`magic` 校验) 后把它交给驱动。域号是 ABI 且域
/// 在 boot 期即建好, 故不存在"运行期首次分配"路径 (重复分配会与既有映射冲突)。
pub fn syscall_grant(a1: u64, _a2: u64, _a3: u64) -> u64 {
    let me = crate::scheduler::current_domain();
    let (bar_paddr, cfg_paddr) = {
        let bindings = BINDINGS.lock();
        let Some(e) = bindings.iter().find(|e| e.domain == me) else {
            return 0; // 本域无设备。
        };
        if !selects(a1, e.bus, e.dev, e.func) {
            return 0; // 申请的不是本域那台设备。
        }
        (e.bar_paddr, e.cfg_paddr)
    };

    // 能力门禁 (参考 `Spawn` / `IoPort`): 只有持有该设备 BAR 的 `Mmio` 凭证的域才准认领
    // 授权 —— 该凭证由 `grant()` 一并签发, 故合法驱动必过, 其它域拿不到。
    if !crate::cap::has(me, crate::cap::Capability::Mmio(bar_paddr)) {
        crate::video::println("dev: device grant denied (no Mmio capability)");
        return 0;
    }

    // 描述页必须确实带着授权 (magic 匹配), 否则视为"无设备"。
    let magic = unsafe { core::ptr::read_volatile(cfg_paddr as *const u64) };
    if magic != DEVICE_GRANT_MAGIC {
        return 0;
    }
    DEVICE_CFG_VADDR
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `pci_addr` 编码与 `split_pci_addr` 解码互逆; `bus/dev/func` 位宽越界一律拒绝。
    #[test]
    fn pci_addr_roundtrip_and_bounds() {
        assert_eq!(split_pci_addr(pci_addr(0, 2, 0)), Some((0, 2, 0)));
        assert_eq!(split_pci_addr(pci_addr(0, 31, 7)), Some((0, 31, 7)));
        assert_eq!(split_pci_addr(1 << 24), None); // bus 超 8 位
        assert_eq!(split_pci_addr(32 << 8), None); // dev 超 5 位
        assert_eq!(split_pci_addr(8), None); // func 超 3 位
    }

    /// 设备选择: `DEVICE_SELF` 恒选中; 显式地址必须与绑定精确一致 (别的设备不认领)。
    #[test]
    fn device_selection() {
        assert!(selects(DEVICE_SELF, 0, 2, 0));
        assert!(selects(pci_addr(0, 2, 0), 0, 2, 0));
        assert!(!selects(pci_addr(0, 3, 0), 0, 2, 0));
        assert!(!selects(0xDEAD_BEEF, 0, 2, 0)); // 越界编码
    }

    /// `SYS_DEVICE_INFO` 打包: vendor/device 各占高/中 16 位, class 取低 24 位; 0 = 无设备。
    #[test]
    fn info_packing() {
        assert_eq!(pack_info(0x1AF4, 0x1001, 0x000102), 0x1AF4_1001_0000_0102);
        assert_eq!(pack_info(0, 0, 0), 0);
    }
}
