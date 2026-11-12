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

/// **多实例设备**（一台域接多台同类型设备）的窗口。
///
/// ⚠️ 真机实测：AMD 笔记本的 xHCI 控制器远不止 "2–3 个" —— `lspci` 里
/// `class=0c:03:30` 有 13 个（00:00.2 + 00:18.0-7 + 06:00.x + 07:00.0），每个通常只带
/// 1–4 个端口。之前只授权前 4 台，启动盘所在的控制器很可能**根本没被探测**，
/// 表现就是"所有端口都试过，但没有一个是 U 盘"。
///
/// 窗口放在 `0x60_0000` 起的空闲带：用户栈是 `[0x3F_9000, 0x40_1000)`（自栈顶向下 8 页），
/// `0x80_0000` 起是单实例设备窗口，`0xA0_0000` 是 wifi 共享页 —— 中间这一段没人用。
/// 16 个实例共占 `[0x60_0000, 0x7D_0000)`：CFG 16 页 + BAR 16×16 KiB + DMA 16×96 KiB。
pub const DEVICE_MULTI_CFG_VADDR: u64 = paging::USER_SPACE_BASE + 0x60_0000;
pub const DEVICE_MULTI_BAR_VADDR: u64 = paging::USER_SPACE_BASE + 0x61_0000;
pub const DEVICE_MULTI_DMA_VADDR: u64 = paging::USER_SPACE_BASE + 0x65_0000;
/// 单实例 BAR / DMA 步长（4 页 / 24 页）。
pub const DEVICE_MULTI_BAR_STRIDE: u64 = 0x4000;
pub const DEVICE_MULTI_DMA_STRIDE: u64 = 0x1_8000;
/// 最多同时授权的设备实例数（xHCI 控制器上限；真机见过 13 个）。
pub const DEVICE_MULTI_SLOTS: u64 = 16;

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
    /// 独立 IOVA 窗口基址（0 = 未绑定：DMA 仍按**物理地址**走恒等映射，行为与之前完全一致）。
    ///
    /// 非 0 时驱动应把 DMA 描述符里的设备可见地址写成 `dma_iova + 偏移`，而不是
    /// `dma_paddr + 偏移`；绑定由内核 [`bind_dma_window`] 在设备开始 DMA 前完成。
    pub dma_iova: u64,
    /// 独立 IOVA 窗口覆盖的字节数（0 = 未绑定）。
    pub dma_iova_bytes: u64,
}

impl DeviceGrant {
    /// 该设备的 DMA 是否已绑定到独立 IOVA 窗口（`dma_iova != 0`）。
    pub fn dma_window_bound(&self) -> bool {
        self.dma_iova != 0
    }
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
    grant_at(
        &req,
        DEVICE_CFG_VADDR,
        DEVICE_BAR_VADDR,
        DEVICE_DMA_VADDR,
        false,
    )
}

/// 把第 `slot` 台同类型设备授权给 `req.domain`（多控制器场景，如 xHCI）。
///
/// 描述页 / BAR / DMA 各自按实例步长偏移；驱动在固定窗口 + `slot * 步长` 处读取。
pub fn grant_slot(req: GrantRequest, slot: u32) -> bool {
    let i = slot as u64;
    grant_at(
        &req,
        DEVICE_MULTI_CFG_VADDR + i * PAGE,
        DEVICE_MULTI_BAR_VADDR + i * DEVICE_MULTI_BAR_STRIDE,
        DEVICE_MULTI_DMA_VADDR + i * DEVICE_MULTI_DMA_STRIDE,
        true,
    )
}

fn grant_at(
    req: &GrantRequest,
    cfg_vaddr: u64,
    bar_vaddr: u64,
    dma_vaddr: u64,
    multi: bool,
) -> bool {
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

    // 2b. 把该设备**真正会 DMA** 的物理范围登记给 IOMMU：默认窗口只按登记表建叶项，
    //     未登记的内核页 / 页表 / 他人用户页不建映射，越界 DMA 被硬件拒绝。
    //     无 IOMMU（或 SAFE 构建）时 `register_device_range` 直接返回 false，行为不变。
    crate::arch::iommu::register_device_range(
        req.bus,
        req.dev,
        req.func,
        bar_paddr,
        req.bar_pages * PAGE,
        false,
    );
    crate::arch::iommu::register_device_range(
        req.bus,
        req.dev,
        req.func,
        dma_paddr,
        dma_pages * PAGE,
        true,
    );

    // 3. 写描述结构到描述页 (恒等映射, 直接以物理地址作为指针写)。
    let desc = DeviceGrant {
        magic: DEVICE_GRANT_MAGIC,
        bar_paddr,
        bar_vaddr,
        bar_bytes: req.bar_pages * PAGE,
        dma_paddr,
        dma_vaddr,
        dma_bytes: dma_pages * PAGE,
        msix_vector_base: msix.vector,
        msix_vector_count: msix.count,
        msix_table_offset: msix.table_offset,
        msix_msg_addr: msix.msg_addr,
        page_size: PAGE as u32,
        _reserved: 0,
        msix_table_vaddr: msix.table_vaddr,
        // 默认不绑定独立 IOVA 窗口 —— DMA 仍按物理地址（恒等映射），行为零变化。
        dma_iova: 0,
        dma_iova_bytes: 0,
    };
    unsafe {
        core::ptr::write(cfg_paddr as *mut DeviceGrant, desc);
    }

    // 记下"域 ↔ 已授权设备": 既是 `SYS_DEVICE_CONFIG_READ` 的放行依据 (只放行"读自己
    // 那台设备"), 也是 D1b 运行期 `SYS_DEVICE_INFO` / `SYS_DEVICE_GRANT` 的登记来源
    // (查询本域设备 / 能力门禁 / 幂等确认)。
    if multi {
        bind_push(req.domain, req.bus, req.dev, req.func, bar_paddr, cfg_paddr);
    } else {
        bind(req.domain, req.bus, req.dev, req.func, bar_paddr, cfg_paddr);
    }

    // 4. 映射描述页、BAR 窗口 (非缓存 MMIO) 与 DMA 各页 (数据: RW + NX)。
    paging::map_user_page(
        req.domain,
        cfg_vaddr,
        cfg_paddr,
        paging::UserPagePerm::ReadWrite,
    );
    for i in 0..req.bar_pages {
        paging::map_mmio(req.domain, bar_vaddr + i * PAGE, bar_paddr + i * PAGE);
    }
    for i in 0..dma_pages {
        paging::map_user_page(
            req.domain,
            dma_vaddr + i * PAGE,
            dma_paddr + i * PAGE,
            paging::UserPagePerm::ReadWrite,
        );
    }

    // 5. 授予 MMIO 能力 (允许驱动域后续自行管理映射)。
    crate::cap::grant(req.domain, crate::cap::Capability::Mmio(bar_paddr));

    true
}

/// 把某台**已授权**设备的物理连续 DMA 块绑定到独立 IOVA 窗口（性能飞地地基，内核内部 API）。
///
/// `dma_paddr` / `dma_bytes` 是授权时分配的同一块 DMA（物理连续、页对齐）。成功 =
/// 新建一个以 `iova_base` 为基的 per-domain 上下文、把该块按 4 KiB 页映射进去、再把设备
/// 功能点切到新上下文，并把 `dma_iova` / `dma_iova_bytes` 回写进描述页；返回 `Some(iova_base)`。
///
/// 任一步不支持（无 IOMMU / 多 DRHD / 表建不起来）都**保持恒等映射**并返回 `None` ——
/// 现有驱动的 DMA 不受影响。只应在设备**开始 DMA 之前**调用；本批没有 IOTLB 失效。
pub fn bind_dma_window(
    domain: u64,
    bus: u8,
    dev: u8,
    func: u8,
    dma_paddr: u64,
    dma_bytes: u64,
    iova_base: u64,
) -> Option<u64> {
    let cfg_paddr = {
        let bindings = BINDINGS.lock();
        bindings
            .iter()
            .find(|e| e.domain == domain && e.bus == bus && e.dev == dev && e.func == func)?
            .cfg_paddr
    };
    // 幂等：已绑定就直接返回既有窗口（不重复建表 / 映射）。
    let mut desc = unsafe { core::ptr::read_volatile(cfg_paddr as *const DeviceGrant) };
    if desc.dma_window_bound() {
        return Some(desc.dma_iova);
    }

    let mut window = crate::arch::iommu::IovaWindow::create(domain, iova_base, dma_bytes)?;
    if !window.map_contiguous(iova_base, dma_paddr, dma_bytes) {
        crate::video::println("dev: DMA window map failed, device keeps identity mapping");
        return None;
    }
    if !window.attach_device(bus, dev, func) {
        let _ = window.unmap(iova_base, dma_bytes);
        crate::video::println("dev: DMA window attach failed, device keeps identity mapping");
        return None;
    }

    // 回写描述页：驱动从此把设备可见地址写成 `dma_iova + 偏移`。
    desc.dma_iova = iova_base;
    desc.dma_iova_bytes = dma_bytes;
    unsafe { core::ptr::write(cfg_paddr as *mut DeviceGrant, desc) };
    crate::video::print("dev: DMA window bound iova=0x");
    crate::video::print_hex(iova_base);
    crate::video::print(" bytes=0x");
    crate::video::print_hex(dma_bytes);
    crate::video::println("");
    Some(iova_base)
}

// ---------------------------------------------------------------------------
// 性能飞地: 运行期设备窗口映射 (§6.2 最小切片)
// ---------------------------------------------------------------------------

/// `SYS_ENCLAVE_MAP` 的错误码（从 `u64::MAX` 向下编号, 集中在此便于用户态逐一打印原因）。
///
/// 成功返回值是**实际生效的 IOVA**（非 0、页对齐，且受 AGAW ≤ 57 约束），因此不会与这些
/// 哨兵混淆。任何一步不支持都返回其中之一，并且**设备上下文保持原状** —— 既有驱动继续
/// 走启动时建立的恒等窗口。
///
/// - `NO_DEVICE`     本域没有授权设备 / 申请的不是本域那台；
/// - `NO_CAP`        缺少该设备 BAR 的 `Mmio` 能力；
/// - `BAD_WINDOW`    IOVA / 长度非法、越界或超出已授权 DMA 块；
/// - `UNSUPPORTED`   IOMMU 没打开、多 DRHD、页表建不起来等；
/// - `CONFLICT`      BAR 窗口在本域被映射到了别的物理页；
/// - `ALREADY_OWNED` 同一 BAR 已被飞地占用（含本域重复 attach；需先 `SYS_ENCLAVE_DETACH`）；
/// - `NO_IOVA`       内核 IOVA 分配器窗口（64 MiB/域）已耗尽。
pub const ENCLAVE_ERR_NO_DEVICE: u64 = u64::MAX;
pub const ENCLAVE_ERR_NO_CAP: u64 = u64::MAX - 1;
pub const ENCLAVE_ERR_BAD_WINDOW: u64 = u64::MAX - 2;
pub const ENCLAVE_ERR_UNSUPPORTED: u64 = u64::MAX - 3;
pub const ENCLAVE_ERR_CONFLICT: u64 = u64::MAX - 4;
pub const ENCLAVE_ERR_ALREADY_OWNED: u64 = u64::MAX - 5;
pub const ENCLAVE_ERR_NO_IOVA: u64 = u64::MAX - 6;

/// 单次飞地窗口的长度上限（1 GiB）—— 先在这里挡住，避免拿一个巨大长度去逐页建表。
const ENCLAVE_MAX_BYTES: u64 = 1 << 30;

/// 判断 `[iova, iova + bytes)` 是否可作为飞地 DMA 窗口（纯函数，便于单测）。
///
/// 规则沿用 `syscall::user_buf_ok` 的风格：起点/终点都要合法、长度非零、加法不溢出。
/// 具体是：长度非 0、页对齐、不超过单次上限，且不超出该设备已授权的 DMA 块（`dma_bytes`）；
/// `iova != 0` 时还要求 IOVA 页对齐、整段不得落在**用户虚拟地址**空间（IOVA 是设备可见
/// 地址，不是 CPU 虚拟地址）。
///
/// `iova == 0` 表示**由内核自动分配**（[`crate::arch::iommu::alloc_iova`]），此处只校验长度，
/// 地址合法性由分配器保证。
fn enclave_window_ok(iova: u64, bytes: u64, dma_bytes: u64) -> bool {
    if bytes == 0 || bytes > ENCLAVE_MAX_BYTES {
        return false;
    }
    if (iova & (PAGE - 1)) != 0 || (bytes & (PAGE - 1)) != 0 {
        return false;
    }
    if (dma_bytes & (PAGE - 1)) != 0 || bytes > dma_bytes {
        return false;
    }
    if iova == 0 {
        return true; // 自动分配：选址交给 IOMMU 分配器。
    }
    if paging::is_user_address(iova) {
        return false;
    }
    let Some(end) = iova.checked_add(bytes) else {
        return false;
    };
    if paging::is_user_address(end - 1) {
        return false;
    }
    true
}

/// 确认 / 建立 **BAR 窗口**在 `domain` 里的映射（幂等）：已映射到同一物理页就当成功。
///
/// 走与 [`paging::map_mmio`] 相同的口径（USER + 可写 + 非缓存 + NX）。已有映射且目标物理页
/// 一致时**不重复映射** —— `map_to` 撞已映射页会 panic，那会毁掉既有驱动。
fn ensure_bar_window(domain: u64, bar_vaddr: u64, bar_paddr: u64, bar_bytes: u64) -> bool {
    if bar_vaddr == 0 || bar_bytes == 0 || (bar_bytes & (PAGE - 1)) != 0 {
        return false;
    }
    if !paging::is_user_address(bar_vaddr) {
        return false;
    }
    let pages = bar_bytes / PAGE;
    // 第一遍只检查、不映射：任何一页指向别的物理页就整体拒绝，不留半截窗口。
    for i in 0..pages {
        let va = bar_vaddr + i * PAGE;
        let pa = bar_paddr + i * PAGE;
        if let Some(mapped) = paging::resolve_user_page(domain, va) {
            if (mapped & !(PAGE - 1)) != pa {
                return false;
            }
        }
    }
    // 第二遍补齐缺失页（已映射到同一物理页的不重复映射 —— `map_to` 撞已映射页会 panic）。
    for i in 0..pages {
        let va = bar_vaddr + i * PAGE;
        let pa = bar_paddr + i * PAGE;
        if paging::resolve_user_page(domain, va).is_none() {
            paging::map_mmio(domain, va, pa);
        }
    }
    true
}

// ---------------------------------------------------------------------------
// 性能飞地: 设备独占 / 解绑
// ---------------------------------------------------------------------------

/// "设备已被飞地占用"的登记项（按 BAR 物理地址索引）。
struct EnclaveOwner {
    domain: u64,
    bar_paddr: u64,
    iova: u64,
    bytes: u64,
    /// IOVA 是否由内核自动分配（`detach` 时据此归还位图）。
    auto_iova: bool,
    /// 活着的 per-domain 窗口：`detach` 用它撤叶项（表页不回收）。
    window: crate::arch::iommu::IovaWindow,
}

/// 飞地独占表（按 BAR 物理地址）。
///
/// 内核**不做** PCI 复位：登记 owner 之后，原驱动若再次访问设备由驱动侧自行检查；
/// 这张表只负责拒绝**第二个域**把同一设备再绑到另一个 IOVA 窗口。
static ENCLAVE_OWNERS: spin::Mutex<Vec<EnclaveOwner>> = spin::Mutex::new(Vec::new());

/// 所有权判定结果（纯函数，便于单测）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum OwnerState {
    Free,
    SameDomain,
    OtherDomain,
}

fn owner_state(owner: Option<u64>, me: u64) -> OwnerState {
    match owner {
        None => OwnerState::Free,
        Some(d) if d == me => OwnerState::SameDomain,
        Some(_) => OwnerState::OtherDomain,
    }
}

/// 定位调用域被授权、且被 `a1` 选中的设备：`(bus, dev, func, bar_paddr, cfg_paddr)`。
fn enclave_binding(a1: u64) -> Option<(u8, u8, u8, u64, u64)> {
    let me = crate::scheduler::current_domain();
    let bindings = BINDINGS.lock();
    let e = bindings.iter().find(|e| e.domain == me)?;
    if !selects(a1, e.bus, e.dev, e.func) {
        return None;
    }
    Some((e.bus, e.dev, e.func, e.bar_paddr, e.cfg_paddr))
}

/// `SYS_ENCLAVE_MAP` 的处理入口（性能飞地 §6.2 最小切片）。
///
/// `a1` = 设备选择（[`DEVICE_SELF`] 或显式 `pci_addr`）；`a2` = 期望 IOVA（页对齐；
/// **`0` = 由内核自动分配**）；`a3` = 窗口字节数。成功返回**实际生效的 IOVA**，失败返回
/// `ENCLAVE_ERR_*`。
///
/// 顺序刻意如此：先把所有**只读校验**做完（设备归属 → `Mmio` 能力门禁 → 设备独占 →
/// 窗口合法性），再幂等确认 BAR 映射，最后建独立 IOVA 窗口并切设备上下文。任何一步失败
/// 都**不触碰**设备上下文 —— 未调用本入口的域完全走原有恒等映射，行为零变化。
pub fn syscall_enclave_map(a1: u64, a2: u64, a3: u64) -> u64 {
    let me = crate::scheduler::current_domain();
    let Some((bus, dev, func, bar_paddr, cfg_paddr)) = enclave_binding(a1) else {
        return ENCLAVE_ERR_NO_DEVICE; // 本域没有授权设备 / 申请的不是本域那台设备。
    };

    // 能力门禁：只允许持有该设备 BAR `Mmio` 凭证的域（`grant` 一并签发）。
    if !crate::cap::has(me, crate::cap::Capability::Mmio(bar_paddr)) {
        crate::video::println("dev: enclave map denied (no Mmio capability)");
        return ENCLAVE_ERR_NO_CAP;
    }

    // 设备独占：同一 BAR 已被飞地占用（无论哪个域）时拒绝第二次 attach。
    {
        let owners = ENCLAVE_OWNERS.lock();
        let owner = owners
            .iter()
            .find(|o| o.bar_paddr == bar_paddr)
            .map(|o| o.domain);
        if owner_state(owner, me) != OwnerState::Free {
            crate::video::print("dev: enclave map denied (device already owned by domain ");
            crate::video::print_u64(owner.unwrap_or(0));
            crate::video::println(")");
            return ENCLAVE_ERR_ALREADY_OWNED;
        }
    }

    let bytes = a3;
    let mut desc = unsafe { core::ptr::read_volatile(cfg_paddr as *const DeviceGrant) };
    if desc.magic != DEVICE_GRANT_MAGIC {
        return ENCLAVE_ERR_NO_DEVICE;
    }
    if !enclave_window_ok(a2, bytes, desc.dma_bytes) {
        return ENCLAVE_ERR_BAD_WINDOW;
    }
    // 幂等：已绑定就返回既有窗口，不重复建表、不改设备上下文。
    if desc.dma_window_bound() {
        return desc.dma_iova;
    }

    // IOVA 选址：`a2 == 0` 走每域位图分配器，否则沿用调用方指定值。
    let auto_iova = a2 == 0;
    let iova = if auto_iova {
        match crate::arch::iommu::alloc_iova(me, bytes) {
            Some(i) => i,
            None => return ENCLAVE_ERR_NO_IOVA,
        }
    } else {
        a2
    };
    let release_iova = || {
        if auto_iova {
            let _ = crate::arch::iommu::free_iova(me, iova, bytes);
        }
    };

    // 先确认 IOMMU 能给出独立上下文：不支持时**完全不碰** BAR 映射 / 设备上下文，
    // 立刻回错误 —— 这正是"不支持就保持原状"。
    let Some(mut window) = crate::arch::iommu::IovaWindow::create(me, iova, bytes) else {
        release_iova();
        return ENCLAVE_ERR_UNSUPPORTED;
    };

    if !ensure_bar_window(me, desc.bar_vaddr, desc.bar_paddr, desc.bar_bytes) {
        release_iova();
        return ENCLAVE_ERR_CONFLICT;
    }
    if !window.map_contiguous(iova, desc.dma_paddr, bytes) {
        release_iova();
        return ENCLAVE_ERR_UNSUPPORTED;
    }
    if !window.attach_device(bus, dev, func) {
        let _ = window.unmap(iova, bytes);
        release_iova();
        return ENCLAVE_ERR_UNSUPPORTED;
    }

    // 回写描述页：后续同域调用可直接复用这条窗口。
    desc.dma_iova = iova;
    desc.dma_iova_bytes = bytes;
    unsafe { core::ptr::write_volatile(cfg_paddr as *mut DeviceGrant, desc) };

    // 登记独占：原驱动从此刻起不应再碰这台设备（内核不做 PCI 复位）。
    ENCLAVE_OWNERS.lock().push(EnclaveOwner {
        domain: me,
        bar_paddr,
        iova,
        bytes,
        auto_iova,
        window,
    });

    crate::video::print("dev: enclave window mapped iova=0x");
    crate::video::print_hex(iova);
    crate::video::print(" bytes=0x");
    crate::video::print_hex(bytes);
    crate::video::print(if auto_iova { " auto" } else { "" });
    crate::video::println("");
    iova
}

/// `SYS_ENCLAVE_DETACH` 的处理入口：解除本域对设备的飞地独占并恢复默认恒等映射。
///
/// `a1` = 设备选择（同 `SYS_ENCLAVE_MAP`）。成功返回 `1`（含"本来就没占用"的幂等路径），
/// 失败返回 `ENCLAVE_ERR_*`。顺序：先切回默认上下文，再撤 IOVA 叶项、归还自动分配的
/// IOVA，最后清描述页 —— 设备任何时刻都不会指向一条已被撤掉的映射。
pub fn syscall_enclave_detach(a1: u64, _a2: u64, _a3: u64) -> u64 {
    let me = crate::scheduler::current_domain();
    let Some((bus, dev, func, bar_paddr, cfg_paddr)) = enclave_binding(a1) else {
        return ENCLAVE_ERR_NO_DEVICE;
    };
    if !crate::cap::has(me, crate::cap::Capability::Mmio(bar_paddr)) {
        crate::video::println("dev: enclave detach denied (no Mmio capability)");
        return ENCLAVE_ERR_NO_CAP;
    }

    let mut owner = {
        let mut owners = ENCLAVE_OWNERS.lock();
        let Some(idx) = owners.iter().position(|o| o.bar_paddr == bar_paddr) else {
            return 1; // 没有占用：幂等成功。
        };
        if owners[idx].domain != me {
            return ENCLAVE_ERR_ALREADY_OWNED; // 别的域占用，不能代拆。
        }
        owners.remove(idx)
    };

    // 1. 设备上下文切回启动时建立的默认恒等窗口。
    let restored = crate::arch::iommu::detach_device(bus, dev, func);
    // 2. 撤掉该窗口的 4 KiB 叶项（表页不回收）。
    let _ = owner.window.unmap(owner.iova, owner.bytes);
    // 3. 自动分配的 IOVA 归还位图。
    if owner.auto_iova {
        let _ = crate::arch::iommu::free_iova(me, owner.iova, owner.bytes);
    }

    // 4. 描述页回到"未绑定"（`dma_iova == 0` 是驱动与内核的共同哨兵）。
    let mut desc = unsafe { core::ptr::read_volatile(cfg_paddr as *const DeviceGrant) };
    if desc.magic == DEVICE_GRANT_MAGIC {
        desc.dma_iova = 0;
        desc.dma_iova_bytes = 0;
        unsafe { core::ptr::write_volatile(cfg_paddr as *mut DeviceGrant, desc) };
    }

    if !restored {
        crate::video::println("dev: enclave detach: context restore failed");
        return ENCLAVE_ERR_UNSUPPORTED;
    }
    crate::video::print("dev: enclave detached bar=0x");
    crate::video::print_hex(bar_paddr);
    crate::video::print(" iova=0x");
    crate::video::print_hex(owner.iova);
    crate::video::println("");
    1
}

/// 无该类型设备时的降级: 仅映射一个**零描述页**到约定地址, 让驱动读到
/// `magic == 0` (不等于 [`DEVICE_GRANT_MAGIC`]) 后自行优雅退出, 而不是读未映射地址
/// 触发缺页死循环。
pub fn grant_empty(domain: u64) {
    map_zero_cfg(domain, DEVICE_CFG_VADDR);
}

/// 多实例降级: 从 `first` 起为剩余实例各映射一页零描述。
///
/// 驱动会无条件探测 `DEVICE_MULTI_SLOTS` 个槽位，故未授权的槽位也必须是**已映射**的
/// 零页 —— 否则驱动读到未映射地址会陷入缺页循环。
pub fn grant_empty_multi_from(domain: u64, first: u64) {
    let mut i = first;
    while i < DEVICE_MULTI_SLOTS {
        map_zero_cfg(domain, DEVICE_MULTI_CFG_VADDR + i * PAGE);
        i += 1;
    }
}

pub fn grant_empty_multi(domain: u64) {
    grant_empty_multi_from(domain, 0);
}

fn map_zero_cfg(domain: u64, vaddr: u64) {
    let cfg_paddr = match frame_allocator::allocate_frame() {
        Some(p) => p,
        None => return,
    };
    // 分配器不保证新帧内容为 0, 显式清零。
    unsafe {
        core::ptr::write_bytes(cfg_paddr as *mut u8, 0, PAGE as usize);
    }
    paging::map_user_page(domain, vaddr, cfg_paddr, paging::UserPagePerm::ReadWrite);
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

/// 记下某域被授权的设备 (同一域多台设备时追加; 供多实例驱动使用)。
fn bind_push(domain: u64, bus: u8, dev: u8, func: u8, bar_paddr: u64, cfg_paddr: u64) {
    BINDINGS.lock().push(DeviceBinding {
        domain,
        bus,
        dev,
        func,
        bar_paddr,
        cfg_paddr,
    });
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

    /// 飞地窗口校验: 页对齐 / 非零 / 非用户地址 / 长度越界 / 加法溢出都必须拒绝。
    #[test]
    fn enclave_window_bounds_and_alignment() {
        // 7 页 DMA 块（与 `main.rs` 给 NVMe 的 `dma_pages: 7` 同量级）。
        const DMA: u64 = 7 * 4096;
        const IOVA: u64 = 0x1000_0000; // 256 MiB

        assert!(enclave_window_ok(IOVA, DMA, DMA));
        assert!(enclave_window_ok(IOVA, 4096, DMA)); // 子窗口允许

        // IOVA 0 = 自动分配：只校验长度（DMA 块可容纳即可）。
        assert!(enclave_window_ok(0, 4096, DMA));
        assert!(!enclave_window_ok(0, DMA + 4096, DMA));

        // 显式 IOVA 必须页对齐。
        assert!(!enclave_window_ok(IOVA + 1, 4096, DMA));

        // 长度非零且页对齐。
        assert!(!enclave_window_ok(IOVA, 0, DMA));
        assert!(!enclave_window_ok(IOVA, 4097, DMA));

        // 长度越界: 超出已授权 DMA 块 / 超出单次上限（1 GiB）。
        assert!(!enclave_window_ok(IOVA, DMA + 4096, DMA));
        assert!(!enclave_window_ok(IOVA, ENCLAVE_MAX_BYTES, DMA));

        // 用户虚拟地址不能当 IOVA: 起点或终点任一落入都拒。
        assert!(!enclave_window_ok(
            crate::memory::paging::USER_SPACE_BASE,
            4096,
            4096
        ));
        assert!(!enclave_window_ok(
            crate::memory::paging::USER_SPACE_BASE - 0x1000,
            0x2000,
            0x2000
        ));

        // 加法溢出。
        assert!(!enclave_window_ok(u64::MAX & !0xFFF, 0x2000, DMA));
    }

    /// 设备独占: 空闲 / 本域幂等 / 别的域占用 三种判定。
    #[test]
    fn enclave_ownership_states() {
        assert_eq!(owner_state(None, 7), OwnerState::Free);
        assert_eq!(owner_state(Some(7), 7), OwnerState::SameDomain);
        assert_eq!(owner_state(Some(7), 8), OwnerState::OtherDomain);
        // 域 0 也是合法域，不能把"已登记 0"误判成空闲。
        assert_eq!(owner_state(Some(0), 0), OwnerState::SameDomain);
        assert_eq!(owner_state(Some(0), 1), OwnerState::OtherDomain);
    }

    /// 能力门禁的判据是**精确匹配该 BAR 的 `Mmio` 凭证**: 别的 BAR / 别的能力种类都不认。
    #[test]
    fn enclave_capability_gate_requires_exact_mmio() {
        use crate::cap::Capability;
        let bar = 0x8000_0000u64;
        let held = Capability::Mmio(bar);
        assert_eq!(held, Capability::Mmio(bar));
        assert_ne!(held, Capability::Mmio(bar + 0x1000));
        assert_ne!(held, Capability::Irq(0));
        assert_ne!(held, Capability::Fb);
    }

    /// 七个错误码互不相同, 且都落在任何合法 IOVA 之上（AGAW ≤ 57 位）。
    #[test]
    fn enclave_error_codes_do_not_collide_with_iova() {
        const ERRS: [u64; 7] = [
            ENCLAVE_ERR_NO_DEVICE,
            ENCLAVE_ERR_NO_CAP,
            ENCLAVE_ERR_BAD_WINDOW,
            ENCLAVE_ERR_UNSUPPORTED,
            ENCLAVE_ERR_CONFLICT,
            ENCLAVE_ERR_ALREADY_OWNED,
            ENCLAVE_ERR_NO_IOVA,
        ];
        for (i, a) in ERRS.iter().enumerate() {
            for b in &ERRS[i + 1..] {
                assert_ne!(a, b);
            }
            assert!(*a > 1u64 << 57);
        }
    }
}
