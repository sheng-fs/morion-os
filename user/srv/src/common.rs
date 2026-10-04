//! 各服务程序共用的「小工具 + 线协议定义」。
//!
//! E2b 把 14 个服务拆成各自独立的程序后, 各模块之间**唯一**的共享面就是这里:
//!   * 顶部几个域 id / 固定地址常量与能力自测常量;
//!   * `Message` / `PageFaultInfo` (与内核布局一致的 IPC 结构);
//!   * 块设备客户端封装 (`block_read_dev` / `block_part_*` / 卷表查询) 与卷/分区协议;
//!   * 通用字节、路径、CRC、日期助手。
//!
//! 放进这里的是「被两个及以上服务模块引用」的项 (用编译器判定); 只被单个模块用到的
//! 东西留在那个模块里。

// 这些助手为跨模块可见而声明成 `pub`, 但它们是**本 crate 内部**的线协议实现, 并非对外
// API; 参数里的裸指针 (`*const u8` 等) 是拆分前那份单文件私有函数的原样代码。clippy 的
// `not_unsafe_ptr_arg_deref` 只对"可从 crate 外调用的 pub fn"报警, 对这里的内部助手放行。
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use morion::syscall::*;
use morion::vfs;

/// 各服务域 id (与内核 `main.rs` 创建顺序一致)。
///   0 sender / 1 receiver / 2 pager / 3 echo / 4 kbd
///   5 block_srv / 6 fat32_srv / 7 app / 8 shell
///   9 mount_srv / 10 tmpfs_srv
pub const BLOCK_DOMAIN: u64 = 5;
pub const APP_DOMAIN: u64 = 7;

/// 用户态固定数据区基址 (与内核约定): `USER_BASE + 8 MiB`。
/// 布置在程序镜像 (自 `USER_BASE` 起, 随代码增长) 与用户栈 (`USER_BASE + 4 MiB`)
/// 之上, 避免镜像增长后踩到这些固定映射。子区域:
///   `+0x80_0000` = sender/receiver 共享页; `+0x81_0000` = NVMe 配置页 (同内核 nvme.rs)。
pub const USER_DATA_BASE: u64 = 0x0000_0080_0080_0000;
/// sender/receiver 共享页虚拟地址 (双方约定同一地址)。
pub const SHARED_PAGE: u64 = USER_DATA_BASE;

/// 「能力随 IPC 传递」自测: 句柄指向的**不透明**对象标识 —— 内核不解释它的含义,
/// 只负责在移交时原样搬运 (真实的文件 fd 也是这么打包的: `(服务域 << 32) | fd`)。
pub const CAP_TOKEN: u64 = 0x5A5A_1234_5678_9ABC;
/// `sender` 通知 `receiver`「新句柄已就绪」的 tag 基数: 低 8 位放句柄索引。
pub const CAP_HANDLE_TAG: u64 = 0xCA00;

/// sender/receiver 自测的**启动握手** (方案 A): receiver 的「启动时零能力」负例跑完后,
/// 才允许 sender 委派 `SendTo(3)`。
///
/// 动机: 那条负例的前提「receiver 启动时零能力」原本依赖调度顺序 —— sender (域 0) 比
/// receiver (域 1) 先起跑, 且在 1000 Hz 下「sender 委派」会与「receiver 的负例」赛跑,
/// 约 18% 的启动里委派抢先 (见 `docs/dev-reference.md` §9 第 83 行)。改成显式握手后,
/// 顺序由同步而非时间片决定: sender 先发 [`RECV_HANDSHAKE_TAG`] 并阻塞, receiver 跑完
/// 负例后回 [`RECV_HANDSHAKE_ACK`]。
///
/// receiver 在回 ack 时**仍是零能力** —— 内核对 `reply` 不查 `SendTo` (回复目标由内核在
/// `recv` 时记录), 所以这次握手不削弱「零能力启动」这个前提。
pub const RECV_HANDSHAKE_TAG: u64 = 0x5243_4853; // "RCHS"
pub const RECV_HANDSHAKE_ACK: u64 = 0x5243_4854; // "RCHT"

/// echo 的控制消息 (E3c): 收到就退出。
///
/// 让"服务实例崩溃/退出"这件事可以被**从外部触发** —— 监督者 `init` 的巡检与自测 FS-29
/// 都靠它模拟一次服务死亡 (走的是普通的 `SYS_EXIT` 退出即回收路径, 不是杀进程)。
pub const ECHO_QUIT_TAG: u64 = 0x4543_4851; // "ECHQ"

/// IPC 消息 (与内核 `ipc::Message` 布局一致: 24 字节头 + `PAYLOAD_LEN`)。
#[repr(C)]
#[allow(dead_code)]
pub struct Message {
    pub from: u64,
    pub to: u64,
    pub tag: u64,
    pub payload: [u8; PAYLOAD_LEN],
}

/// 缺页信息 (与内核 `pager::PageFaultInfo` 布局一致, 24 字节)。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PageFaultInfo {
    pub fault_domain: u64,
    pub fault_addr: u64,
    pub error_code: u64,
}

/// 块设备请求 tag (block_srv 据此识别读/写请求)。
pub const BLOCK_REQ_TAG: u64 = 0x424C_4F43; // "BLOC"

/// 块设备请求 (序列化进 IPC payload, 32 字节, 与内核 `PAYLOAD_LEN` 一致)。
/// `buf` 为数据缓冲页虚拟地址, 须已由调用方共享映射进 block_srv 地址空间。
///
/// `op` 打包了**卷号**与操作码: 低 8 位 = 操作码 (0 读 / 1 写 / 2 查询卷表),
/// 高位 = 卷号。卷号由 block_srv 的卷层分配 (扫描各 namespace 的 MBR/GPT 分区表;
/// 无分区表的 namespace 视为一个整盘卷)。之所以打包而非新增字段, 是因为 payload
/// 恰好 32 字节, 已无空位。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct BlockReq {
    pub op: u64,    // (volume << 8) | opcode
    pub lba: u64,   // 卷内起始扇区号 (块层会加上分区偏移)
    pub count: u64, // 扇区数 (>0; 超过单条 NVMe 命令上限时由 block_srv 自动切分)
    pub buf: u64,   // 数据缓冲页虚拟地址; opcode=2 时为卷描述符输出页
}

/// 帧级网卡请求 tag (N6): net_srv 据此识别"收发一帧"请求。
pub const NET_REQ_TAG: u64 = 0x4E45_5457; // "NETW"

/// 发一帧: 帧在 `buf` 页里 (长 `len` 字节), net_srv 拷进 NIC 的 TX 缓冲发出去; 成功回复 1。
pub const NET_OP_TX: u64 = 0;
/// 收一帧: net_srv 排空 RX; 有帧则写进 `buf` 页并回复**帧长**, 无帧回复 0。
pub const NET_OP_RX: u64 = 1;
/// 取网卡 MAC: 回复 = MAC (低 48 位; 全 0 表示无网卡)。
pub const NET_OP_INFO: u64 = 2;

/// 单帧最大字节数 (以太帧上界 ~1518; 取 2 KiB 对齐, 共享页一页足够)。
pub const NET_FRAME_MAX: u64 = 2048;

/// 帧级网卡请求 (N6): 帧数据经**共享页**传递 (同址共享), 本结构只带元数据。
///
/// net_srv (域 16) 是**帧级驱动服务** —— 只收发以太帧、不含 IP/TCP 语义; 协议栈在
/// `netstack_srv`。`buf` 是调用方 `sys_alloc_page` 后 `sys_share_page` 共享过来的一页
/// (4 KiB, 足够一帧): TX 时内含待发帧, RX 时由 net_srv 写入收到的帧。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct NetReq {
    pub op: u64,  // NET_OP_*
    pub len: u64, // TX: 待发帧长; RX/INFO: 忽略
    pub buf: u64, // 共享页虚拟地址 (调用方与本服务同址)
}

/// netstack_srv 域号（用户态网络协议栈，N6）。
pub const NETSTACK_DOMAIN: u64 = 21;

/// 套接字服务请求 tag（N6.5）: 应用经 `libnetv` 与 netstack_srv 交互。
pub const NETS_REQ_TAG: u64 = 0x4E53_544B; // "NSTK"
/// 建 socket; 回复 socket id（>0）/ 0。请求里 `sock` 字段 = 出口网卡索引（N9.2: 0=virtio-net, 1=e1000e）。
pub const NETS_OP_SOCKET: u64 = 0;
/// 绑端口（需内核 `Net` 能力）; 回复 1/0。
pub const NETS_OP_BIND: u64 = 1;
/// 发 UDP; 回复 1/0。
pub const NETS_OP_SENDTO: u64 = 2;
/// 收 UDP; 回复负载长度（无数据 0），负载写入 `buf` 共享页。
pub const NETS_OP_RECVFROM: u64 = 3;
/// 关 socket; 回复 1。
pub const NETS_OP_CLOSE: u64 = 4;

/// 建 TCP 连接（N7.2）; 请求 `sock` 字段 = 网卡索引; 回复连接 id（>0）/ 0。
pub const NETS_OP_TSOCKET: u64 = 5;
/// 绑 TCP 本地端口（需内核 `Net` 能力）; 回复 1/0。
pub const NETS_OP_TBIND: u64 = 6;
/// 主动连接; `addr` = 目的 IPv4, `port` = 目的端口; 回复 1/0。
pub const NETS_OP_TCONNECT: u64 = 7;
/// 发 TCP 数据; `len` = 负载长度, `buf` = 负载共享页; 回复 1/0。
pub const NETS_OP_TSEND: u64 = 8;
/// 收 TCP 数据; `buf` = 输出共享页; 回复字节数（无 0）。
pub const NETS_OP_TRECV: u64 = 9;
/// 关 TCP 连接; 回复 1。
pub const NETS_OP_TCLOSE: u64 = 10;

/// 单条 UDP 负载上界（一页共享页内，留出帧头余量）。
pub const NETS_PAYLOAD_MAX: u64 = 1472;

/// 套接字服务请求（N6.5）。负载经**共享页** `buf` 传递（同址共享）。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct NetSReq {
    pub op: u64,   // NETS_OP_*
    pub sock: u64, // socket id
    pub port: u64, // bind: 本地端口; sendto: 目的端口
    pub addr: u64, // sendto: 目的 IPv4（`a<<24 | b<<16 | c<<8 | d`）
    pub len: u64,  // sendto: 负载长度
    pub buf: u64,  // 共享页 va（sendto 负载输入 / recvfrom 负载输出）
}

/// 带卷号的读 (卷号由卷层分配, 0 = 第一个卷)。**直通**实现 (不经写背缓存)。
pub fn block_raw_read(dev: u64, lba: u32, count: u16, buf: *mut u8) -> bool {
    let req = BlockReq {
        op: dev << 8,
        lba: lba as u64,
        count: count as u64,
        buf: buf as u64,
    };
    let payload = unsafe {
        core::slice::from_raw_parts(
            &req as *const BlockReq as *const u8,
            core::mem::size_of::<BlockReq>(),
        )
    };
    sys_call_payload(BLOCK_DOMAIN, BLOCK_REQ_TAG, payload) == 1
}

/// 带卷号的写 (卷号由卷层分配, 0 = 第一个卷)。**直通**实现 (不经写背缓存)。
pub fn block_raw_write(dev: u64, lba: u32, count: u16, buf: *mut u8) -> bool {
    let req = BlockReq {
        op: (dev << 8) | 1,
        lba: lba as u64,
        count: count as u64,
        buf: buf as u64,
    };
    let payload = unsafe {
        core::slice::from_raw_parts(
            &req as *const BlockReq as *const u8,
            core::mem::size_of::<BlockReq>(),
        )
    };
    sys_call_payload(BLOCK_DOMAIN, BLOCK_REQ_TAG, payload) == 1
}

// ---------------------------------------------------------------------------
// 通用可选写背缓存 (opt-in, 默认关闭)
//
// 详见 [block_wb_enable]。只对「一连串 <= 4 KiB 的小写」有意义的负载 (如 fat32 / exfat
// 的元数据更新) 才打开; 打开后小块写先攒进**调用方的暂存窗**, 满窗或显式 flush 时用
// [`block_batch`] 一次下发, 把 N 次「提交-等完成」并成 1 次。
// ---------------------------------------------------------------------------

/// 写背缓存: 暂存窗最大块数 (= 批量子请求上限)。
const WB_MAX: usize = BLOCK_BATCH_MAX;
/// 写背缓存: 每块最多扇区数 (一页 = 4 KiB)。
const WB_SUB_SECTORS: u64 = 8;
/// 计数打印的间隔 (累计块数)。
const WB_PRINT_EVERY: u64 = 512;

static mut WB_ON: bool = false;
/// 调用方暂存窗基址 (每块一页, 需同址共享给 block_srv)。
static mut WB_STAGE: u64 = 0;
/// 描述符数组页基址 (同址共享给 block_srv)。
static mut WB_DESC: u64 = 0;
/// 当前攒批所属的卷号 (`u64::MAX` = 空)。
static mut WB_DEV: u64 = u64::MAX;
static mut WB_N: usize = 0;
static mut WB_LBA: [u64; WB_MAX] = [0; WB_MAX];
static mut WB_SEC: [u64; WB_MAX] = [0; WB_MAX];
static mut WB_WRITTEN: u64 = 0;
static mut WB_BATCHES: u64 = 0;
static mut WB_NEXT: u64 = WB_PRINT_EVERY;

/// 暂存窗第 `i` 块的虚拟地址。
fn wb_page(i: usize) -> *mut u8 {
    (unsafe { WB_STAGE } + (i as u64) * 4096) as *mut u8
}

/// 启用写背缓存。`stage_vaddr` 起 `pages` 页是暂存窗 (每块一页), `desc_vaddr` 是描述符
/// 数组页 —— 两者都须由调用方 `sys_alloc_page` + `sys_share_page(.., BLOCK_DOMAIN)`。
/// 只对 `count <= 8` 扇区的小写攒批; 大写或换卷会先 flush。
pub fn block_wb_enable(stage_vaddr: u64, pages: usize, desc_vaddr: u64) {
    if pages == 0 || pages > WB_MAX {
        return;
    }
    unsafe {
        WB_STAGE = stage_vaddr;
        WB_DESC = desc_vaddr;
        WB_ON = true;
        WB_N = 0;
        WB_DEV = u64::MAX;
    }
}

/// 待写入区间是否与暂存区重叠 (读前须先落盘, 保证写后读一致)。
fn wb_overlap(dev: u64, lba: u64, count: u64) -> bool {
    unsafe {
        if WB_N == 0 || WB_DEV != dev {
            return false;
        }
        let end = lba + count;
        let mut i = 0usize;
        while i < WB_N {
            let s = WB_LBA[i];
            let e = s + WB_SEC[i];
            if lba < e && s < end {
                return true;
            }
            i += 1;
        }
    }
    false
}

/// 把暂存区一次落盘 (batch 不行则退回逐块直写)。无待写时直接返回 true。
pub fn block_wb_flush() -> bool {
    unsafe {
        if !WB_ON || WB_N == 0 {
            return true;
        }
        let dev = WB_DEV;
        let n = WB_N;
        let desc = WB_DESC as *mut BatchEnt;
        let mut i = 0usize;
        while i < n {
            core::ptr::write_unaligned(
                desc.add(i),
                BatchEnt {
                    lba: WB_LBA[i],
                    sectors: WB_SEC[i],
                    buf: wb_page(i) as u64,
                },
            );
            i += 1;
        }
        let mut ok = block_batch(dev, desc, n, true) == 1;
        if !ok {
            ok = true;
            i = 0;
            while i < n {
                if !block_raw_write(dev, WB_LBA[i] as u32, WB_SEC[i] as u16, wb_page(i)) {
                    ok = false;
                    break;
                }
                i += 1;
            }
        }
        WB_WRITTEN += n as u64;
        WB_BATCHES += 1;
        WB_N = 0;
        WB_DEV = u64::MAX;
        if WB_WRITTEN >= WB_NEXT {
            print("blk-wb: blocks=");
            print_u64(WB_WRITTEN);
            print(" batches=");
            print_u64(WB_BATCHES);
            print(" avg=");
            print_u64(WB_WRITTEN / WB_BATCHES.max(1));
            println("");
            WB_NEXT += WB_PRINT_EVERY;
        }
        ok
    }
}

/// 把一次小块写攒进暂存区 (不可攒时先 flush 再直写)。
fn wb_put(dev: u64, lba: u32, count: u16, buf: *mut u8) -> bool {
    unsafe {
        if count == 0 || count as u64 > WB_SUB_SECTORS {
            return block_wb_flush() && block_raw_write(dev, lba, count, buf);
        }
        if (WB_N >= WB_MAX || (WB_DEV != u64::MAX && WB_DEV != dev)) && !block_wb_flush() {
            return false;
        }
        WB_DEV = dev;
        let i = WB_N;
        core::ptr::copy_nonoverlapping(buf, wb_page(i), count as usize * 512);
        WB_LBA[i] = lba as u64;
        WB_SEC[i] = count as u64;
        WB_N += 1;
        true
    }
}

/// 带卷号的读 (卷号由卷层分配, 0 = 第一个卷)。启用写背缓存且命中暂存区时先落盘。
pub fn block_read_dev(dev: u64, lba: u32, count: u16, buf: *mut u8) -> bool {
    if unsafe { WB_ON } && wb_overlap(dev, lba as u64, count as u64) && !block_wb_flush() {
        return false;
    }
    block_raw_read(dev, lba, count, buf)
}

/// 带卷号的写 (卷号由卷层分配, 0 = 第一个卷)。启用写背缓存时先攒批, 否则直写。
pub fn block_write_dev(dev: u64, lba: u32, count: u16, buf: *mut u8) -> bool {
    if unsafe { WB_ON } {
        wb_put(dev, lba, count, buf)
    } else {
        block_raw_write(dev, lba, count, buf)
    }
}

/// 读一个 16 位小端无符号整数 (引导扇区字段)。
pub fn read_u16(ptr: *const u8) -> u16 {
    unsafe {
        let lo = core::ptr::read_volatile(ptr) as u16;
        let hi = core::ptr::read_volatile(ptr.add(1)) as u16;
        lo | (hi << 8)
    }
}

/// 读一个 32 位小端无符号整数 (引导扇区 / FSInfo 字段)。
pub fn read_u32(ptr: *const u8) -> u32 {
    unsafe {
        let b0 = core::ptr::read_volatile(ptr) as u32;
        let b1 = core::ptr::read_volatile(ptr.add(1)) as u32;
        let b2 = core::ptr::read_volatile(ptr.add(2)) as u32;
        let b3 = core::ptr::read_volatile(ptr.add(3)) as u32;
        b0 | (b1 << 8) | (b2 << 16) | (b3 << 24)
    }
}

/// 写一个 16 位小端无符号整数 (目录项字段)。
pub fn write_u16(ptr: *mut u8, v: u16) {
    unsafe {
        core::ptr::write_volatile(ptr, (v & 0xFF) as u8);
        core::ptr::write_volatile(ptr.add(1), (v >> 8) as u8);
    }
}

/// 写一个 32 位小端无符号整数 (目录项 / FAT 表项)。
pub fn write_u32(ptr: *mut u8, v: u32) {
    unsafe {
        core::ptr::write_volatile(ptr, (v & 0xFF) as u8);
        core::ptr::write_volatile(ptr.add(1), ((v >> 8) & 0xFF) as u8);
        core::ptr::write_volatile(ptr.add(2), ((v >> 16) & 0xFF) as u8);
        core::ptr::write_volatile(ptr.add(3), ((v >> 24) & 0xFF) as u8);
    }
}

pub fn read_u64(ptr: *const u8) -> u64 {
    (read_u32(ptr) as u64) | ((read_u32(unsafe { ptr.add(4) }) as u64) << 32)
}
pub fn write_u64(ptr: *mut u8, v: u64) {
    write_u32(ptr, (v & 0xFFFF_FFFF) as u32);
    write_u32(unsafe { ptr.add(4) }, (v >> 32) as u32);
}

/// CRC-32 的**增量**形式: 传入上一段的寄存器 (初值 `0xFFFF_FFFF`), 返回更新后的寄存器;
/// 全部分段喂完后取反才是最终 CRC。用于**流式**校验大于一页的数据 (如 GPT 项数组: 128 项
/// × 128 字节 = 16 KiB, 超过一个共享页, 只能分块读)。
pub fn crc32_update(mut crc: u32, data: &[u8]) -> u32 {
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ 0xEDB8_8320;
            } else {
                crc >>= 1;
            }
        }
    }
    crc
}

/// CRC-32 (IEEE 802.3, 多项式 0xEDB88320, 反射)。
pub fn mfs_crc32(data: &[u8]) -> u32 {
    !crc32_update(0xFFFF_FFFF, data)
}

/// 从共享页 `buf` 起读一条 NUL 结尾的路径, 返回其字节切片。
///
/// 上限 `PAYLOAD_LEN - 1` —— 与客户端 `vfs` 写入共享页时的截断长度一致, 服务端不会
/// 越过它去扫描整页。页内无 NUL 时按上限截断。
pub fn page_path(buf: u64) -> &'static [u8] {
    let p = buf as *const u8;
    let mut n = 0usize;
    while n < PAYLOAD_LEN - 1 && unsafe { *p.add(n) } != 0 {
        n += 1;
    }
    unsafe { core::slice::from_raw_parts(p, n) }
}

/// 解析 `PathReq` (STAT / CHMOD 共用): 返回 (共享页地址, 页内路径)。
///
/// 路径走共享页而 payload 只带附加参数与缓冲地址 —— 单条 payload 装不下路径 +
/// 地址, 且结果页要按客户端指定 (app 与 shell 的地址不同)。
pub fn parse_path_req(payload: *const u8) -> (u64, &'static str) {
    let req: vfs::PathReq = unsafe { core::ptr::read_unaligned(payload as *const vfs::PathReq) };
    let p = page_path(req.buf);
    (req.buf, unsafe { core::str::from_utf8_unchecked(p) })
}

/// 从 `TwoPathReq` 取出调用方共享页里的两条路径 (`src\0dst`), 交给 `f`。
///
/// 长度必须自洽且落在单条 IPC 可达范围内, 否则视为坏请求 (不信任客户端给的长度)。
/// `f` 是泛型而非 `fn` 指针: 软链接还要带一个 owner 参数, 用闭包捕获比再多传一层更直接。
pub fn with_two_paths<F: FnOnce(&str, &str) -> u64>(payload: *const u8, f: F) -> u64 {
    let req: vfs::TwoPathReq =
        unsafe { core::ptr::read_unaligned(payload as *const vfs::TwoPathReq) };
    let total = req.a_len as usize + 1 + req.b_len as usize;
    if total > PAYLOAD_LEN || req.buf == 0 {
        return u64::MAX;
    }
    unsafe {
        let p = req.buf as *const u8;
        let a = core::str::from_utf8_unchecked(core::slice::from_raw_parts(p, req.a_len as usize));
        let b = core::str::from_utf8_unchecked(core::slice::from_raw_parts(
            p.add(req.a_len as usize + 1),
            req.b_len as usize,
        ));
        f(a, b)
    }
}

/// 清零 `len` 字节。
pub fn zero_bytes(ptr: *mut u8, len: usize) {
    for i in 0..len {
        unsafe {
            core::ptr::write_volatile(ptr.add(i), 0u8);
        }
    }
}

/// 打印字符串, 不可打印字节 (除换行) 替换为 '.'。用于安全显示文件内容,
/// 避免二进制文件 (如 NVRAM 变量) 中的控制字节扰乱屏幕输出。
pub fn print_sanitized(s: &str) {
    for &b in s.as_bytes() {
        let c = if b == b'\n' || (0x20..=0x7E).contains(&b) {
            b
        } else {
            b'.'
        };
        let byte = [c];
        print(unsafe { core::str::from_utf8_unchecked(&byte) });
    }
}

/// 把 `v` 的十进制写法写进 `dst`, 返回写入的字节数 (不使用堆)。
pub fn dec_to_str(v: u64, dst: &mut [u8]) -> usize {
    let mut tmp = [0u8; 20];
    let mut n = 0;
    let mut x = v;
    loop {
        tmp[n] = b'0' + (x % 10) as u8;
        n += 1;
        x /= 10;
        if x == 0 || n == tmp.len() {
            break;
        }
    }
    let n = n.min(dst.len());
    for i in 0..n {
        dst[i] = tmp[n - 1 - i];
    }
    n
}

/// 距 1970-01-01 的天数 → 民用历 (年/月/日), `days_from_civil` 的逆运算。
pub fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// 民用历 (年/月/日) → 距 1970-01-01 的天数 (Howard Hinnant 的 days_from_civil)。
pub fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146097 + doe - 719468
}

// ---------------------------------------------------------------------------
// 卷 / 分区协议 (客户端侧) 与卷表查询
// ---------------------------------------------------------------------------

pub const VOL_MAX: usize = 16;

/// 文件系统类型 (按卷首签名探测)。
pub const VOL_KIND_UNKNOWN: u32 = 0;
pub const VOL_KIND_FAT: u32 = 1; // FAT12/16/32 (引导扇区尾 0x55AA)
pub const VOL_KIND_EXFAT: u32 = 2;
pub const VOL_KIND_MFS: u32 = 3;
pub const VOL_KIND_EXT2: u32 = 4;
/// ISO9660 (CD/安装盘): 卷描述符 (LBA 16 起) 以 `"CD001"` 标识。**只读**。
pub const VOL_KIND_ISO: u32 = 5;

/// 块请求操作码 (`BlockReq.op` 低 8 位)。
pub const BLOCK_OP_READ: u8 = 0;
pub const BLOCK_OP_WRITE: u8 = 1;
/// 查询卷表: 把 `VolumeDesc` 数组写进 `buf`, 回复卷数。
pub const BLOCK_OP_LIST_VOLUMES: u8 = 2;
/// 建分区 (GPT / MBR): 见 `PartReq`。回复**新分区的卷号**, 失败 `u64::MAX`。
pub const BLOCK_OP_PART_CREATE: u8 = 3;
/// 删分区 (只清条目, **不动数据**): 回复 1/0。删掉最后一个分区后整张表被清空 (盘回到空白)。
pub const BLOCK_OP_PART_DELETE: u8 = 4;
/// 清空分区表 (盘回到「无分区表」): 回复 1/0。
pub const BLOCK_OP_PART_WIPE: u8 = 5;
/// 重读分区表 (重建卷表并打印): 回复重读后的卷数。
pub const BLOCK_OP_PART_RELOAD: u8 = 6;
/// 裸盘读**一个扇区** (按 nsid 寻址, **绕过卷层**): 回复 1/0。
///
/// 只给分区诊断/自测用 —— 建完分区后 LBA 0/1 已不属于任何卷 (整盘卷没了、新分区从 2048
/// 起), 想校验写进去的字节就只能直接按盘读。
pub const BLOCK_OP_DISK_READ: u8 = 7;
/// 把一块**非 NVMe** 盘挂进卷层 (后端驱动 → block_srv): `count` = 盘容量 (扇区), 回复无意义。
///
/// 目前唯一的发送者是 ahci_srv (域 18): 它自测通过后**异步**通知 block_srv, block_srv
/// 为它分配一个传输暂存页 (同址共享给 ahci), 登记成一个 AHCI 后端卷, 之后对外的读/写
/// 都经 block_srv 的卷层转发回 ahci_srv —— 上层文件系统因此完全不必知道盘挂在哪种控制器上。
pub const BLOCK_OP_ATTACH: u8 = 8;
/// 散聚**批读**: 一次 IPC 带 K 个子请求。`BlockReq.lba` = 子请求数 K,
/// `BlockReq.buf` = 调用方共享页里的 [`BatchEnt`] 数组 (每子请求 ≤ 8 扇区 = 一页)。
///
/// 动机 (02b-2): 量化发现耗时主因是**每条 NVMe 命令的完成等待**, 故把「逐个提交-等完成」
/// 改成「一次排 K 条 SQE、只敲一次门铃、统一等完成」。非 NVMe 后端自动退回逐块。回复 1/0。
pub const BLOCK_OP_BATCH_READ: u8 = 9;
/// 散聚**批写**: 语义同 [`BLOCK_OP_BATCH_READ`], 方向为写。
pub const BLOCK_OP_BATCH_WRITE: u8 = 10;
/// 一次批量子请求数上限 (也是写暂存窗的页数)。
pub const BLOCK_BATCH_MAX: usize = 16;

/// 批量子请求描述符 (24 字节, `repr(C)`)。数据缓冲 `buf` 须已共享给 block_srv 且页对齐。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct BatchEnt {
    /// 卷内起始扇区号。
    pub lba: u64,
    /// 扇区数 (1..=8, 一页以内)。
    pub sectors: u64,
    /// 数据缓冲页虚拟地址 (页对齐)。
    pub buf: u64,
}

/// 一次提交 K 个块读 / 写子请求 (`write = true` 为写)。`ents` 指向共享页里的描述符数组。
/// 成功返回 1, 失败 0。
pub fn block_batch(vol: u64, ents: *const BatchEnt, k: usize, write: bool) -> u64 {
    let opcode = if write {
        BLOCK_OP_BATCH_WRITE as u64
    } else {
        BLOCK_OP_BATCH_READ as u64
    };
    let req = BlockReq {
        op: (vol << 8) | opcode,
        lba: k as u64,
        count: 0,
        buf: ents as u64,
    };
    let payload = unsafe {
        core::slice::from_raw_parts(
            &req as *const BlockReq as *const u8,
            core::mem::size_of::<BlockReq>(),
        )
    };
    sys_call_payload(BLOCK_DOMAIN, BLOCK_REQ_TAG, payload)
}

/// 分区表 / 裸盘请求 (`op & 0xFF` 是 `BLOCK_OP_PART_*` / `BLOCK_OP_DISK_READ` 时按本结构解释)。
///
/// 与 `BlockReq` **同为 4 × u64 且字段顺序一一对应** (`op / lba→nsid / count→arg0 / buf→arg1`),
/// 故发送端复用同一个 payload 布局, 接收端按 opcode 决定读成哪个结构。
///
/// 分区表属于**整块盘**, 所以这里一律按 `nsid` 寻址 —— 卷号只是分区表的产物 (建分区前没有
/// 这个卷、删完分区它又没了), 拿卷号当目标既不稳定也没法表达「在空白盘上建第一张表」。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PartReq {
    /// `(flags << 8) | opcode`。
    pub op: u64,
    /// 目标盘 (NVMe namespace id)。
    pub nsid: u64,
    /// 主参数: 建分区 = 大小 (扇区, 0 = 用尽剩余空间); 删分区 = 分区序号; 裸读 = 扇区号。
    pub arg0: u64,
    /// 次参数: 建分区 = flags; 裸读 = 目标缓冲页虚拟地址。
    pub arg1: u64,
}

/// 卷描述符 (经「卷列表」IPC 写给调用方共享页, `repr(C)` 固定布局)。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VolumeDesc {
    pub id: u32,
    pub nsid: u32,
    pub start_lba: u32,
    pub sectors: u32,
    pub kind: u32,
    pub _pad: u32,
}

/// 查询卷表到调用方共享页 `buf`, 返回卷数 (失败 0)。
pub fn block_list_volumes(buf: *mut u8, max: u32) -> u64 {
    let req = BlockReq {
        op: (BLOCK_OP_LIST_VOLUMES as u64),
        lba: 0,
        count: max as u64,
        buf: buf as u64,
    };
    let payload = unsafe {
        core::slice::from_raw_parts(
            &req as *const BlockReq as *const u8,
            core::mem::size_of::<BlockReq>(),
        )
    };
    sys_call_payload(BLOCK_DOMAIN, BLOCK_REQ_TAG, payload)
}

/// 发一个分区 / 裸盘请求给 block_srv, 返回回复值 (`u64::MAX` / 0 都表示失败, 见各调用方)。
pub fn block_part_call(req: &PartReq) -> u64 {
    let payload = unsafe {
        core::slice::from_raw_parts(
            req as *const PartReq as *const u8,
            core::mem::size_of::<PartReq>(),
        )
    };
    sys_call_payload(BLOCK_DOMAIN, BLOCK_REQ_TAG, payload)
}

/// 在盘 `nsid` 上建一个 `size_sectors` 扇区的分区 (`0` = 用尽剩余空间)。
///
/// 表风格**按盘自适应**: 已有 GPT 就继续 GPT、已有 MBR 就继续 MBR; **空白盘默认建 GPT**
/// (`force_mbr = true` 时改建 MBR, 只在盘上还没有分区表时可用)。
/// 成功返回新分区的**卷号** (可直接喂给 `mkfs.mfs`), 失败 `u64::MAX`。
pub fn block_part_create(nsid: u64, size_sectors: u64, force_mbr: bool) -> u64 {
    let req = PartReq {
        op: BLOCK_OP_PART_CREATE as u64 | ((force_mbr as u64) << 8),
        nsid,
        arg0: size_sectors,
        arg1: 0,
    };
    block_part_call(&req)
}

/// 删掉盘 `nsid` 上序号为 `index` 的分区 (**只清条目, 不动数据**)。成功返回 1。
pub fn block_part_delete(nsid: u64, index: u64) -> u64 {
    let req = PartReq {
        op: BLOCK_OP_PART_DELETE as u64,
        nsid,
        arg0: index,
        arg1: 0,
    };
    block_part_call(&req)
}

/// 清空盘 `nsid` 的分区表, 让它回到「无分区表」状态。成功返回 1。
pub fn block_part_wipe(nsid: u64) -> u64 {
    let req = PartReq {
        op: BLOCK_OP_PART_WIPE as u64,
        nsid,
        arg0: 0,
        arg1: 0,
    };
    block_part_call(&req)
}

/// 重读全部分区表并重建卷表 (分区表改动后调用)。返回重读后的卷数, 失败 0。
pub fn block_part_reload() -> u64 {
    let req = PartReq {
        op: BLOCK_OP_PART_RELOAD as u64,
        nsid: 0,
        arg0: 0,
        arg1: 0,
    };
    block_part_call(&req)
}

/// 裸读盘 `nsid` 的**一个扇区** `lba` 到 `buf` (绕过卷层; 分区诊断与自测校验表字节用)。
pub fn block_disk_read(nsid: u64, lba: u64, buf: *mut u8) -> bool {
    let req = PartReq {
        op: BLOCK_OP_DISK_READ as u64,
        nsid,
        arg0: lba,
        arg1: buf as u64,
    };
    block_part_call(&req) == 1
}

/// 在卷描述符数组中查找第一个 `kind` 匹配的卷号。
pub fn vol_find_kind(list: *const u8, count: u64, kind: u32) -> Option<u32> {
    let esize = core::mem::size_of::<VolumeDesc>();
    let mut i = 0u64;
    while i < count {
        let d =
            unsafe { core::ptr::read_unaligned(list.add(i as usize * esize) as *const VolumeDesc) };
        if d.kind == kind {
            return Some(d.id);
        }
        i += 1;
    }
    None
}

/// 取卷描述符数组中第 `i` 个描述符 (供自测读取卷表)。
pub fn vol_desc(list: *const u8, i: usize) -> VolumeDesc {
    let esize = core::mem::size_of::<VolumeDesc>();
    unsafe { core::ptr::read_unaligned(list.add(i * esize) as *const VolumeDesc) }
}

/// 在卷描述符数组里找 `(nsid, start_lba)` 对应的那一项 (自测断言分区/整盘卷在用)。
pub fn vol_desc_find(list: *const u8, count: u64, nsid: u32, start_lba: u32) -> Option<VolumeDesc> {
    let mut i = 0u64;
    while i < count {
        let d = vol_desc(list, i as usize);
        if d.nsid == nsid && d.start_lba == start_lba {
            return Some(d);
        }
        i += 1;
    }
    None
}

/// 文件服务启动时「认领」自己要用的卷号。
///
/// 先在卷表里找第一个 `kind` 匹配的卷; 找不到则回退 `fallback` (约定卷号)。
/// 需要回退的原因: 空白 MFS 盘没有 magic, 必须先格式化才能被探测到。
/// `scratch` 必须是本域**已共享给 block_srv** 的缓冲页 (卷描述符经它回传)。
///
/// MFS **不用**这个函数: 一块盘上可以有多块 MFS 卷, 「第一个」不够用 —— 它按主卷
/// 序号认领 (见 mfs 模块的 `mfs_vol_claim`)。
pub fn vol_claim(scratch: *mut u8, max: u32, kind: u32, fallback: u64) -> u64 {
    let n = block_list_volumes(scratch, max);
    if n == 0 || n == u64::MAX {
        return fallback;
    }
    match vol_find_kind(scratch, n, kind) {
        Some(v) => v as u64,
        None => fallback,
    }
}

/// 卷表里卷号 `vol` 的描述符; 查不到返回 `None`。
///
/// `scratch` 必须是本域**已共享给 block_srv** 的缓冲页 (卷描述符经它回传)。
pub fn vol_find_desc(scratch: *mut u8, vol: u64) -> Option<VolumeDesc> {
    let n = block_list_volumes(scratch, VOL_MAX as u32);
    if n == 0 || n == u64::MAX {
        return None;
    }
    let esize = core::mem::size_of::<VolumeDesc>();
    let mut i = 0u64;
    while i < n {
        let d = unsafe {
            core::ptr::read_unaligned(scratch.add(i as usize * esize) as *const VolumeDesc)
        };
        if d.id as u64 == vol {
            return Some(d);
        }
        i += 1;
    }
    None
}

/// 卷表里卷号 `vol` 的文件系统类型 (查不到则返回 `VOL_KIND_UNKNOWN`)。
pub fn vol_kind_of(scratch: *mut u8, vol: u64) -> u32 {
    vol_find_desc(scratch, vol).map_or(VOL_KIND_UNKNOWN, |d| d.kind)
}

/// 卷表里卷号 `vol` 的容量 (扇区数); 0 = 未知。
pub fn vol_sectors(scratch: *mut u8, vol: u64) -> u32 {
    vol_find_desc(scratch, vol).map_or(0, |d| d.sectors)
}

/// 把本服务**默认卷之外**的同类卷挂到 `/usb<卷号>` (M1b 多卷挂载)。
///
/// `scratch` 必须是本域**已共享给 block_srv** 的缓冲页 (卷描述符经它回传), 且调用
/// 时机须在服务自己的元数据解析**之后** —— 它会覆盖该页内容。
/// 真实多盘/多分区机器上, 各文件服务借此把自己那一类的其余卷也提供给客户端, 而不是
/// 只认领「第一个匹配卷」。
pub fn mount_extra_volumes(scratch: *mut u8, kind: u32, primary: u64, domain: u64) {
    let n = block_list_volumes(scratch, VOL_MAX as u32);
    if n == 0 || n == u64::MAX {
        return;
    }
    let esize = core::mem::size_of::<VolumeDesc>();
    let mut i = 0u64;
    while i < n {
        let d = unsafe {
            core::ptr::read_unaligned(scratch.add(i as usize * esize) as *const VolumeDesc)
        };
        if d.kind == kind && d.id as u64 != primary {
            vfs::mount_vol(domain, d.id as u64);
        }
        i += 1;
    }
}

// ---------------------------------------------------------------------------
// 文件名 (8.3 / 长名) 与目录项助手 (fat32 / shell / ext2 / exfat / app 共用)
// ---------------------------------------------------------------------------

/// ASCII 大写 (仅处理 a-z)。
pub fn ascii_upper(c: u8) -> u8 {
    if c.is_ascii_lowercase() {
        c - 0x20
    } else {
        c
    }
}

/// 把路径段 (如 "hello.txt" / "dir1") 转成 FAT 8.3 短名 (11 字节, 大写 + 空格填充)。
/// 扩展名按最后一个 '.' 分隔; 主名 > 8 或扩展名 > 3 视为不合法, 返回 None。
pub fn short_name_from_query(name: &[u8]) -> Option<[u8; 11]> {
    let mut dot = None;
    for (i, &c) in name.iter().enumerate() {
        if c == b'.' {
            dot = Some(i);
        }
    }
    let (base, ext): (&[u8], &[u8]) = match dot {
        Some(d) => (&name[..d], &name[d + 1..]),
        None => (name, &[]),
    };
    if base.len() > 8 || ext.len() > 3 {
        return None;
    }
    let mut sn = [b' '; 11];
    for (i, &c) in base.iter().enumerate() {
        sn[i] = ascii_upper(c);
    }
    for (i, &c) in ext.iter().enumerate() {
        sn[8 + i] = ascii_upper(c);
    }
    Some(sn)
}

/// 比较目录项 11 字节短名与目标短名 (大小写不敏感)。
///
/// 目标短名 `sn` 由 `short_name_from_query` 统一转成大写; 但目录项里的**字节本身**
/// 不保证是大写 —— Windows / mtools 用小写标志字节 (偏移 12) 记大小写、字节写大写,
/// 而别的实现可能直接把小写字节写进去。故这里把目录项一侧归一为大写后再比, 两种都认。
pub fn entry_name_matches(entry: *const u8, sn: &[u8; 11]) -> bool {
    unsafe {
        for (i, &b) in sn.iter().enumerate() {
            if ascii_upper(*entry.add(i)) != b {
                return false;
            }
        }
    }
    true
}

/// 列出目录 fd 的条目, 判断是否存在短名为 `name` 且类型匹配 `want_dir` 的条目。
/// readdir 失败或未命中返回 false。
pub fn readdir_has(fd: u64, name: &str, want_dir: bool) -> bool {
    let sn = match short_name_from_query(name.as_bytes()) {
        Some(sn) => sn,
        None => return false,
    };
    let n = vfs::readdir(fd);
    if n == u64::MAX {
        return false;
    }
    let entry_size = core::mem::size_of::<vfs::DirEntry>();
    let count = n as usize / entry_size;
    let list =
        unsafe { core::slice::from_raw_parts(vfs::RESULT_BUF as *const vfs::DirEntry, count) };
    for de in list {
        // 目录项里是盘上的原始 11 字节 (字节大小写不保证, 见 `entry_name_matches`),
        // 故按大小写不敏感比对, 不能直接 `de.name == sn`。
        if entry_name_matches(de.name.as_ptr(), &sn) && (de.is_dir != 0) == want_dir {
            return true;
        }
    }
    false
}

/// 在 readdir 结果中按**长名字段**查找条目 (VFAT LFN / ext2 名字)。
pub fn readdir_has_long(fd: u64, name: &str, want_dir: bool) -> bool {
    let n = vfs::readdir(fd);
    if n == u64::MAX {
        return false;
    }
    let entry_size = core::mem::size_of::<vfs::DirEntry>();
    let count = n as usize / entry_size;
    let list =
        unsafe { core::slice::from_raw_parts(vfs::RESULT_BUF as *const vfs::DirEntry, count) };
    let q = name.as_bytes();
    for de in list {
        let llen = de.long_len as usize;
        if llen == q.len() && &de.long[..llen] == q && (de.is_dir != 0) == want_dir {
            return true;
        }
    }
    false
}

/// 把 ext2 名字 (字节串) 拷进长名字段, 截断到容量且不切断 UTF-8 字符。
pub fn ext2_copy_long(name: &[u8], out: &mut [u8; vfs::DIR_LONG_MAX]) -> u8 {
    let mut n = name.len().min(vfs::DIR_LONG_MAX);
    // 截断点若落在字符中间 (续字节 10xxxxxx), 回退到该字符首字节之前。
    while n > 0 && n < name.len() && (name[n] & 0xC0) == 0x80 {
        n -= 1;
    }
    out[..n].copy_from_slice(&name[..n]);
    n as u8
}

/// 由 ext2 名字派生一个 8.3 形式的短名 (大写), 供无长名时的回退显示。
pub fn ext2_short_name(name: &[u8]) -> [u8; 11] {
    let mut out = [b' '; 11];
    if name == b"." || name == b".." {
        let mut i = 0usize;
        while i < name.len() && i < 11 {
            out[i] = name[i];
            i += 1;
        }
        return out;
    }
    // 主名/扩展名以最后一个 '.' 切分 (无 '.' 则整段都是主名)。
    let mut dot = name.len();
    let mut i = name.len();
    while i > 0 {
        i -= 1;
        if name[i] == b'.' {
            dot = i;
            break;
        }
    }
    let mut k = 0usize;
    let mut n = 0usize;
    while k < dot && n < 8 {
        out[n] = ascii_upper(name[k]);
        n += 1;
        k += 1;
    }
    let mut k = dot + 1;
    let mut m = 8usize;
    while k < name.len() && m < 11 {
        out[m] = ascii_upper(name[k]);
        m += 1;
        k += 1;
    }
    out
}

// ---------------------------------------------------------------------------
// 各服务内部布局常量 (app 自测直接引用; 放在 common 以复用)
// ---------------------------------------------------------------------------

/// tmpfs 路径上限 (= 一条 IPC payload)。
pub const TMP_PATH_MAX: usize = PAYLOAD_LEN;

/// MorionFS 块布局 (mfs 与 app 自测共用)。
pub const MFS_BLOCK: usize = 4096;
pub const MFS_HDR: usize = 8;
pub const MFS_PAYLOAD: usize = MFS_BLOCK - MFS_HDR;
/// 文件块 direct 区槽位数 (与 mfs 模块一致)。
pub const MFS_FILE_DIRECT: usize = 1005;
/// 间接块可容纳的块号数。
pub const MFS_IND_CAP: usize = MFS_PAYLOAD / 4;
/// 单块可承载的文件数据字节数。
pub const MFS_DATA_CAP: usize = MFS_PAYLOAD;

/// MBR 分区项类型字节: 0x83 = Linux 文件系统 (block_srv 写入 / app 校验用)。
pub const PART_MBR_TYPE_LINUX: u8 = 0x83;
/// GPT 分区类型 GUID: Linux 文件系统数据 (block_srv 写入 / app 校验用)。
pub const PART_GPT_TYPE_LINUX: [u8; 16] = [
    0xAF, 0x3D, 0xC6, 0x0F, 0x83, 0x84, 0x72, 0x47, 0x8E, 0x79, 0x3D, 0x69, 0xD8, 0x47, 0x7D, 0xE4,
];

// ---------------------------------------------------------------------------
// CMOS RTC 读时钟 (mfs / exfat 共用)
// ---------------------------------------------------------------------------
//
// 内核没有时间系统调用, 但 `SYS_PORT_IN8/OUT8` 已开放, 故用户态直接读 CMOS RTC
// (端口 0x70 选寄存器 / 0x71 读写)。读失败或时间明显不合理时返回 0 —— 元数据里
// 0 表示"时间未知", 不阻塞任何操作。

const CMOS_IDX: u16 = 0x70;
const CMOS_DAT: u16 = 0x71;
/// RTC 寄存器号。
const CMOS_SEC: u8 = 0x00;
const CMOS_MIN: u8 = 0x02;
const CMOS_HOUR: u8 = 0x04;
const CMOS_DAY: u8 = 0x07;
const CMOS_MON: u8 = 0x08;
const CMOS_YEAR: u8 = 0x09;
/// 状态寄存器 A (bit7 = update in progress) / B (bit2 = 二进制, bit1 = 24 小时制)。
const CMOS_STAT_A: u8 = 0x0A;
const CMOS_STAT_B: u8 = 0x0B;

fn cmos_read(reg: u8) -> u8 {
    sys_port_out8(CMOS_IDX, reg);
    sys_port_in8(CMOS_DAT)
}

/// BCD → 二进制 (状态寄存器 B 的 bit2 为 0 时 RTC 用 BCD 编码)。
fn cmos_bcd(v: u8) -> u8 {
    (v & 0x0F) + ((v >> 4) * 10)
}

/// 读 CMOS RTC 得到当前 Unix 秒 (UTC); 读取失败或字段不合理返回 0。
///
/// 连读两次并要求一致: RTC 更新周期内读到的字段可能跨秒, 两次相同才认为稳定。
pub fn mfs_now() -> u64 {
    let mut attempt = 0;
    while attempt < 4 {
        attempt += 1;
        let a = cmos_rtc_snapshot();
        let b = cmos_rtc_snapshot();
        let (sa, sb) = match (a, b) {
            (Some(x), Some(y)) => (x, y),
            _ => return 0,
        };
        if sa == sb {
            return sa;
        }
    }
    0
}

/// 单次读取 RTC 并转成 Unix 秒; 等 update-in-progress 清零后读, 字段不合法返回 None。
fn cmos_rtc_snapshot() -> Option<u64> {
    let mut guard = 0u32;
    while cmos_read(CMOS_STAT_A) & 0x80 != 0 {
        guard += 1;
        if guard > 1_000_000 {
            return None;
        }
    }
    let stat_b = cmos_read(CMOS_STAT_B);
    let binary = stat_b & 0x04 != 0;
    let h24 = stat_b & 0x02 != 0;
    let mut sec = cmos_read(CMOS_SEC);
    let mut min = cmos_read(CMOS_MIN);
    let mut hour = cmos_read(CMOS_HOUR);
    let mut day = cmos_read(CMOS_DAY);
    let mut mon = cmos_read(CMOS_MON);
    let mut year = cmos_read(CMOS_YEAR);
    if !binary {
        // 12 小时制时 bit7 是 PM 标志, 必须在 BCD 转换前摘掉。
        let pm = !h24 && (hour & 0x80) != 0;
        hour &= 0x7F;
        sec = cmos_bcd(sec);
        min = cmos_bcd(min);
        hour = cmos_bcd(hour);
        day = cmos_bcd(day);
        mon = cmos_bcd(mon);
        year = cmos_bcd(year);
        if pm && hour < 12 {
            hour += 12;
        }
    } else if !h24 && (hour & 0x80) != 0 {
        hour = ((hour & 0x7F) + 12) % 24;
    }
    // 两位数年份: 按 70..99 → 19xx, 00..69 → 20xx 归一。
    let full_year = if year >= 70 {
        1900 + year as i64
    } else {
        2000 + year as i64
    };
    if !(1..=12).contains(&mon) || !(1..=31).contains(&day) || hour > 23 || min > 59 || sec > 60 {
        return None;
    }
    let days = days_from_civil(full_year, mon as i64, day as i64);
    let secs = days * 86400 + hour as i64 * 3600 + min as i64 * 60 + sec as i64;
    if secs < 0 {
        return None;
    }
    Some(secs as u64)
}
