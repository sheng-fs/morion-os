//! libvfs — 用户态文件系统客户端库
//!
//! 统一文件操作接口 (open/read/write/readdir/close/mkdir/...): 先把请求路径
//! 交给挂载服务 (mount_srv) 解析出「应由哪个文件服务处理 + 挂载点前缀长度」,
//! 再对目标服务域发起同步 IPC。数据经共享结果页零拷贝回传 (IPC 仅传控制信息
//! 与返回字节数), 与微内核「数据走共享内存、控制走消息」的约定一致。
//!
//! 这正是 docs/architecture.md「挂载与统一目录树」一节描述的分工: 应用只看到
//! 单一根 `/`, 具体路由 (如 `/tmp` → tmpfs_srv, 其余 → fat32_srv) 在 VFS 库内完成。
//!
//! 「能力即句柄」: `open` / `creat` 成功后向内核申请一个能力句柄 (`sys_cap_issue`),
//! 把句柄索引编进对外 fd 的高 16 位; 之后每次 I/O 都先用 `sys_cap_lookup` 校验
//! 句柄 (见 `cap_guard`), `close` 时撤销句柄。句柄被撤销后, 该 fd 上的任何操作
//! 都会失败 —— 即使 fd 数值被伪造也无法访问服务 (无能力即不可访问)。

use crate::syscall::{
    sys_call_payload, sys_cap_drop, sys_cap_issue, sys_cap_lookup, PAYLOAD_LEN,
};

/// fat32 文件服务域 id (与内核 `main.rs` 创建顺序一致), 挂载于 `/`。
pub const FAT32_DOMAIN: u64 = 6;
/// 挂载管理服务域 id: 维护「挂载点前缀 → 文件服务域」表, 供 libvfs 查询路由。
pub const MOUNT_DOMAIN: u64 = 9;
/// tmpfs 内存文件服务域 id, 挂载于 `/tmp`。
pub const TMPFS_DOMAIN: u64 = 10;
/// MorionFS (MFS) 文件服务域 id, 挂载于 `/mfs`。
pub const MFS_DOMAIN: u64 = 11;
/// ext2 只读文件服务域 id, 挂载于 `/ext2`。
pub const EXT2_DOMAIN: u64 = 12;
/// exFAT 读写文件服务域 id, 挂载于 `/usb`。
pub const EXFAT_DOMAIN: u64 = 13;

/// 结果页虚拟地址: app 分配并共享给 fat32_srv, fat32_srv 在此写入文件内容或
/// 目录列表。`read` / `readdir` 返回的字节数即该页内的有效数据长度。
/// 地址须避开程序镜像 / 用户栈 / NVMe MMIO 区域, 与 fat32_srv 缓冲页同置于 1MB 偏移处。
pub const RESULT_BUF: u64 = 0x0000_0080_0010_4000;

/// 写缓冲页虚拟地址: app 把要写的数据拷入此页 (已共享给 fat32_srv), 再发
/// `write` 请求; fat32_srv 从此页读取数据写盘。最多一页 (4096 字节)。
pub const WRITE_BUF: u64 = 0x0000_0080_0010_5000;

/// shell 专用结果页虚拟地址。
///
/// 必须与 app 的 `RESULT_BUF` 不同: 两者各自把自己的页共享给 fat32_srv 的**同一虚拟
/// 地址空间**, 若用同一地址, 后共享者会覆盖 fat32_srv 内的映射, 使先共享者读不到结果。
/// 故每个客户端用独立地址, 并在请求中携带缓冲地址 (见 `ReadReq.buf` / `DirReq.buf`)。
pub const SHELL_RESULT_BUF: u64 = 0x0000_0080_0010_6000;

/// shell 专用写缓冲页虚拟地址 (写操作使用, 与 app 的 `WRITE_BUF` 同理需独立地址)。
pub const SHELL_WRITE_BUF: u64 = 0x0000_0080_0010_7000;

/// VFS 操作 tag (4 字节 ASCII, 与 fat32_srv 服务循环的分发一致)。
pub const VFS_OPEN_TAG: u64 = 0x4F50_454E; // "OPEN"
pub const VFS_READ_TAG: u64 = 0x5245_4144; // "READ"
pub const VFS_READDIR_TAG: u64 = 0x5244_4952; // "RDIR"
pub const VFS_CLOSE_TAG: u64 = 0x434C_5345; // "CLSE"
pub const VFS_WRITE_TAG: u64 = 0x5752_4954; // "WRIT"
pub const VFS_CREAT_TAG: u64 = 0x4352_4541; // "CREA"
pub const VFS_MKDIR_TAG: u64 = 0x4D4B_4449; // "MKDI"
pub const VFS_UNLINK_TAG: u64 = 0x554E_4C4B; // "UNLK"
pub const VFS_RMDIR_TAG: u64 = 0x524D_4449; // "RMDI"
pub const VFS_STAT_TAG: u64 = 0x5354_4154; // "STAT"
/// 截断/扩展到指定长度: payload = `TruncateReq { fd, size }`。
pub const VFS_TRUNCATE_TAG: u64 = 0x5452_4E43; // "TRNC"
/// 重命名/移动 (可跨目录): payload = `TwoPathReq`, 两条路径在共享页里。
pub const VFS_RENAME_TAG: u64 = 0x5245_4E4D; // "RENM"
/// 修改权限位: payload = `PathReq`, 路径在共享页里。
pub const VFS_CHMOD_TAG: u64 = 0x4348_4D44; // "CHMD"
/// 硬链接: payload = `TwoPathReq`, 两条路径在共享页里 (`src\0dst`)。
pub const VFS_LINK_TAG: u64 = 0x4C49_4E4B; // "LINK"
/// 软链接 (M5c): payload = `TwoPathReq`, 共享页里是 `目标\0链接自身`。
///
/// 与硬链接的区别: `a` 是**目标字符串**, 原样存进链接节点, 不要求存在、不经挂载层
/// 路由 (只有 `b` 即链接自身的路径需要路由)。
pub const VFS_SYMLINK_TAG: u64 = 0x5359_4D4C; // "SYML"

/// `Stat.mode` / `DirEntry.mode` 高 4 位的节点类型, 与 ext2 `i_mode` 的 `S_IFMT` 同构。
///
/// MFS 会填这几位 (故 `ls -l` 能显示 `l`); 其它文件服务的 `mode` 只有权限位、这几位
/// 全 0, 显示时按 `is_dir` 回退。
pub const MODE_FTYPE_MASK: u16 = 0xF000;
pub const MODE_FTYPE_FILE: u16 = 0x8000;
pub const MODE_FTYPE_DIR: u16 = 0x4000;
pub const MODE_FTYPE_LINK: u16 = 0xA000;

/// 挂载查询 tag: 请求 payload 为路径, 回复为 `(服务域 << 32) | 挂载点前缀长度`,
/// 无匹配返回 `u64::MAX`。仅 mount_srv 处理。
pub const VFS_LOOKUP_TAG: u64 = 0x4D4E_5451; // "MNTQ"

/// 运行时挂载 tag: payload 为 `MountReq` (服务域 + 挂载点前缀; 前缀为空表示请
/// mount_srv 自动分配一个空闲 `/mnt<N>`)。回复挂载槽位号 (1 起), 失败 `u64::MAX`。
pub const VFS_MOUNT_TAG: u64 = 0x4D4E_5441; // "MNTA"
/// 运行时卸载 tag: payload 为挂载点前缀 (NUL 结尾)。回复 1 / `u64::MAX`。
pub const VFS_UMOUNT_TAG: u64 = 0x4D4E_5444; // "MNTD"
/// 运行时挂载**额外卷** tag (M1b 多卷挂载): payload 为 `MountVolReq`。
/// 由 mount_srv 自动挂到 `/usb<卷号>`。回复挂载槽位号 (1 起), 失败 `u64::MAX`。
pub const VFS_MOUNT_VOL_TAG: u64 = 0x4D4E_5456; // "MNTV"

/// 挂载点前缀最大长度 (与 mount_srv 的 `MOUNT_PREFIX_MAX` 一致)。
pub const MOUNT_PREFIX_MAX: usize = 24;

/// 运行时挂载额外卷的请求 (序列化进 IPC payload, 16 字节)。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct MountVolReq {
    /// 要额外挂载的文件服务域。
    pub domain: u64,
    /// 该服务要额外挂载的**卷号** (block_srv 卷表里的 id; 各服务自己认领的是默认卷)。
    pub vol: u64,
}

/// 把服务的额外卷 `vol` 挂到 `/usb<卷号>` (M1b 多卷挂载)。
///
/// 挂载点直接取「卷号」编号, 使同一台机器上任何服务挂额外卷都不会撞名, 且挂载点
/// 与 block_srv 卷表里的卷号一一对应 (`/usb3` = 卷 3), 便于排查。
/// 成功返回挂载槽位号 (1 起), 失败 `u64::MAX`。
pub fn mount_vol(domain: u64, vol: u64) -> u64 {
    let req = MountVolReq { domain, vol };
    let payload = unsafe {
        core::slice::from_raw_parts(
            &req as *const MountVolReq as *const u8,
            core::mem::size_of::<MountVolReq>(),
        )
    };
    sys_call_payload(MOUNT_DOMAIN, VFS_MOUNT_VOL_TAG, payload)
}

/// 运行时挂载请求 (序列化进 IPC payload, 32 字节)。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct MountReq {
    /// 要挂载的文件服务域。
    pub domain: u64,
    /// 挂载点前缀 (绝对路径, NUL 填充); 全 0 表示请服务端自动分配 `/mnt<N>`。
    pub prefix: [u8; MOUNT_PREFIX_MAX],
}

/// 运行时把文件服务 `domain` 挂到 `prefix`; `prefix` 为空串时由 mount_srv 自动
/// 分配一个空闲的 `/mnt<N>` 挂载点。成功返回挂载槽位号 (1 起), 失败 `u64::MAX`。
pub fn mount(prefix: &str, domain: u64) -> u64 {
    let mut req = MountReq { domain, prefix: [0; MOUNT_PREFIX_MAX] };
    let n = prefix.len().min(MOUNT_PREFIX_MAX);
    req.prefix[..n].copy_from_slice(&prefix.as_bytes()[..n]);
    let payload = unsafe {
        core::slice::from_raw_parts(
            &req as *const MountReq as *const u8,
            core::mem::size_of::<MountReq>(),
        )
    };
    sys_call_payload(MOUNT_DOMAIN, VFS_MOUNT_TAG, payload)
}

/// 运行时卸载挂载点 `prefix`, 成功返回 1, 失败 `u64::MAX`。
pub fn umount(prefix: &str) -> u64 {
    let payload = path_payload(prefix);
    sys_call_payload(MOUNT_DOMAIN, VFS_UMOUNT_TAG, &payload)
}

// MorionFS 快照操作 tag (仅 mfs_srv 处理)。
/// 创建快照, 回复新快照索引, 失败返回 `u64::MAX`。
pub const MFS_SNAP_TAG: u64 = 0x4D53_4E50; // "MSNP"
/// 列出快照: 每条 16 字节 `{gen u64, root u32, alloc_next u32}` 写入 `buf`。
/// 回复写入字节数 (= 条数 × 16)。payload: `{buf u64}`。
pub const MFS_SNAPLIST_TAG: u64 = 0x4D53_4E4C; // "MSNL"
/// 回滚到快照: payload = 快照索引 (u32)。回复 1 / `u64::MAX`。
pub const MFS_SNAPRESTORE_TAG: u64 = 0x4D53_4E52; // "MSNR"
/// 空间回收 (mark & sweep): 回复本次回收的块数, 失败 `u64::MAX`。
pub const MFS_GC_TAG: u64 = 0x4D53_4743; // "MSGC"
/// 查询空间用量: 回复 `(总块数 << 32) | 空闲块数`。
pub const MFS_STAT_TAG: u64 = 0x4D53_5354; // "MSST"

/// 触发 MorionFS 空间回收 (回收不可达的 COW 旧块), 成功返回回收的块数。
///
/// 回收以可达性为唯一判据, 快照仍引用的历史版本不会被回收。
pub fn mfs_gc() -> u64 {
    sys_call_payload(MFS_DOMAIN, MFS_GC_TAG, &[])
}

/// 查询 MorionFS 空间用量, 返回 `(总块数 << 32) | 空闲块数`, 失败 `u64::MAX`。
pub fn mfs_stat() -> u64 {
    sys_call_payload(MFS_DOMAIN, MFS_STAT_TAG, &[])
}

/// 快照列表单条记录的字节数 (与 mfs_srv 的 `MFS_SNAP_REC` 一致)：
/// gen(u64) + itab_root / ino_hint / alloc_hint / reserved (4 × u32)。
pub const SNAP_REC_LEN: usize = 24;

/// 创建 MorionFS 快照 (记录当前根与代际), 成功返回快照索引。
pub fn mfs_snapshot() -> u64 {
    sys_call_payload(MFS_DOMAIN, MFS_SNAP_TAG, &[])
}

/// 把快照列表写入共享缓冲页 `buf` (须已共享给 mfs_srv), 返回写入字节数。
pub fn mfs_snapshot_list(buf: u64) -> u64 {
    let payload = buf.to_le_bytes();
    sys_call_payload(MFS_DOMAIN, MFS_SNAPLIST_TAG, &payload)
}

/// 回滚到索引为 `idx` 的快照, 成功返回 1。
pub fn mfs_snapshot_restore(idx: u32) -> u64 {
    let payload = idx.to_le_bytes();
    sys_call_payload(MFS_DOMAIN, MFS_SNAPRESTORE_TAG, &payload)
}

/// 请求 tag 中「卷编码」的位偏移与掩码 (M1b 多卷挂载)。
///
/// VFS tag 正文是 4 字节 ASCII (低 32 位); 高 8 位另作**卷编码**: `0` = 让服务用它
/// 自己认领的默认卷, 否则 = 卷号 + 1。这样「这次请求落在哪个卷」随请求一起到达
/// 服务, 而无需给每个请求结构体都加字段 —— 服务端分发前先用 `tag_body` 剥掉高位。
const TAG_VOL_SHIFT: u32 = 32;
const TAG_VOL_MASK: u64 = 0xFF;

/// 请求 tag 的正文 (剥掉高位卷编码), 供服务端 `match` 分发。
pub fn tag_body(tag: u64) -> u64 {
    tag & 0xFFFF_FFFF
}

/// 请求 tag 携带的卷编码 (`0` = 服务默认卷)。
pub fn tag_vol(tag: u64) -> u32 {
    ((tag >> TAG_VOL_SHIFT) & TAG_VOL_MASK) as u32
}

/// 卷号 → 卷编码 (0 保留给「默认卷」, 故整体 +1; 卷号 ≤ 254)。
pub fn enc_of_vol(vol: u64) -> u32 {
    (vol + 1) as u32
}

/// 卷编码 → 卷号; 编码 0 解释为服务默认卷 `default_vol`。
pub fn vol_from_enc(enc: u32, default_vol: u64) -> u64 {
    if enc == 0 {
        default_vol
    } else {
        (enc - 1) as u64
    }
}

/// 把卷编码写进请求 tag。
fn with_vol(tag: u64, vol_enc: u32) -> u64 {
    tag | (((vol_enc as u64) & TAG_VOL_MASK) << TAG_VOL_SHIFT)
}

/// 向挂载服务查询 `path` 应路由到的文件服务域, 返回 (服务域, 卷编码, 挂载点前缀长度)。
///
/// 未挂载 / 查询失败返回 None。路径长度按 payload 上限截断 (挂载点都是短前缀)。
fn mount_lookup(path: &str) -> Option<(u64, u32, usize)> {
    let payload = path_payload(path);
    let r = sys_call_payload(MOUNT_DOMAIN, VFS_LOOKUP_TAG, &payload);
    if r == u64::MAX {
        return None;
    }
    let domain = (r >> 32) & 0xFF;
    let vol_enc = ((r >> 40) & 0xFF) as u32;
    let prefix_len = (r & 0xFFFF_FFFF) as usize;
    // 域 0/挂载前缀长度非法视为查询失败 (无文件服务挂在域 0)。
    if domain == 0 || prefix_len == 0 || prefix_len > path.len() {
        return None;
    }
    Some((domain, vol_enc, prefix_len))
}

/// 把绝对路径 `path` 解析为 (目标服务域, 卷编码, 相对挂载点根的路径)。
///
/// 相对路径保留前导 '/' (服务端一律按以 '/' 开头的绝对路径处理子路径)。例如
/// `/tmp/a` 在 `/tmp`→tmpfs 下得到 `("/a", tmpfs)`; `/h.txt` 在 `/`→fat32 下
/// 得到 `("/h.txt", fat32)`。
fn route(path: &str) -> Option<(u64, u32, &str)> {
    let (domain, vol_enc, prefix_len) = mount_lookup(path)?;
    if prefix_len > path.len() {
        return None;
    }
    // 根挂载 ("/", 前缀长度 1): 路径本身就是服务内绝对路径, 原样下发。
    if prefix_len == 1 {
        return Some((domain, vol_enc, path));
    }
    // 其它挂载点: 去掉挂载前缀, 余下部分自带前导 '/' (如 `/tmp/D1` → `/D1`);
    // 恰好等于挂载点时余下为空, 即子树根 `/`。
    let rest = &path[prefix_len..];
    Some((domain, vol_enc, if rest.is_empty() { "/" } else { rest }))
}

/// 把「服务域 + 服务内局部 fd」打包为对外 fd, 并在高 16 位带上能力句柄索引。
///
/// 布局: `[63:48] 能力句柄 | [47:32] 服务域 | [31:0] 服务内局部 fd`。
/// 域号仅 4 位有效 (MAX_TASKS=16), 句柄索引 < 32 (内核 `HANDLE_SLOTS`), 故高 16 位
/// 足够, 三者互不干扰。`u64::MAX` (失败) 原样透传。
fn make_fd(handle: u64, domain: u64, local: u64) -> u64 {
    if local == u64::MAX {
        return u64::MAX;
    }
    (handle << 48) | ((domain & 0xFFFF) << 32) | (local & 0xFFFF_FFFF)
}

/// 从对外 fd 解出能力句柄索引。
#[inline]
fn fd_handle(fd: u64) -> u64 {
    fd >> 48
}

/// 从对外 fd 解出目标服务域。
#[inline]
fn fd_domain(fd: u64) -> u64 {
    (fd >> 32) & 0xFFFF
}

/// 从对外 fd 解出服务内局部 fd。
#[inline]
fn fd_local(fd: u64) -> u32 {
    (fd & 0xFFFF_FFFF) as u32
}

/// 句柄校验 (「能力即句柄」的执行点): 句柄必须仍然有效, 且其对象标识必须与
/// fd 里编码的 `(服务域, 局部 fd)` 一致 —— 否则视为无能力, 拒绝访问。
///
/// `fd == u64::MAX` (失败值) 直接判否, 不落内核。
#[inline]
fn cap_guard(fd: u64) -> bool {
    if fd == u64::MAX {
        return false;
    }
    let obj = ((fd_domain(fd)) << 32) | (fd_local(fd) as u64);
    sys_cap_lookup(fd_handle(fd)) == obj
}

/// READ 请求 (序列化进 IPC payload, 24 字节)。数据写入 `buf` 指向的共享结果页。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ReadReq {
    pub fd: u32,
    pub offset: u32,
    pub count: u32,
    /// 对齐填充 (使 `buf` 8 字节对齐)。
    pub _pad: u32,
    /// 结果数据写入的缓冲页虚拟地址 (须已共享给 fat32_srv)。
    pub buf: u64,
}

/// WRITE 请求 (序列化进 IPC payload, 24 字节)。数据从 `buf` 指向的共享写缓冲读取。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct WriteReq {
    pub fd: u32,
    pub offset: u32,
    pub count: u32,
    /// 对齐填充 (使 `buf` 8 字节对齐)。
    pub _pad: u32,
    /// 数据来源缓冲页虚拟地址 (须已共享给 fat32_srv)。
    pub buf: u64,
}

/// READDIR 请求 (序列化进 IPC payload, 16 字节)。条目写入 `buf` 指向的共享结果页。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct DirReq {
    pub fd: u32,
    /// 对齐填充 (使 `buf` 8 字节对齐)。
    pub _pad: u32,
    /// 条目写入的缓冲页虚拟地址 (须已共享给 fat32_srv)。
    pub buf: u64,
}

/// 截断请求 (序列化进 IPC payload, 8 字节)。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct TruncateReq {
    pub fd: u32,
    /// 目标长度 (字节); 小于现有长度则截短, 大于则稀疏扩展。
    pub size: u32,
}

/// 双路径请求 (rename / 将来的 link 共用), 序列化进 IPC payload (16 字节)。
///
/// 单条 IPC payload 只有 95 字节可用, 装不下两条绝对路径, 故路径放进调用方
/// **共享页** (`buf`, 与 `DirReq.buf` 同模式): 布局为 `src\0dst`。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct TwoPathReq {
    /// 第一条路径的字节数 (不含 NUL)。
    pub a_len: u32,
    /// 第二条路径的字节数 (不含 NUL)。
    pub b_len: u32,
    /// 路径所在共享页虚拟地址 (须已共享给目标文件服务域)。
    pub buf: u64,
}

/// 「路径放在共享页里」的请求 (序列化进 IPC payload, 16 字节), `STAT` / `CHMOD` 共用。
///
/// 单条 IPC payload 只有 95 字节可用, 且结果页地址也要随请求下发 (shell 与 app 的
/// 结果页地址不同, 见 `RESULT_BUF` / `SHELL_RESULT_BUF`), 故路径改放调用方共享页
/// (`buf`, NUL 结尾), payload 只带附加参数与缓冲地址。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PathReq {
    /// 附加参数: `CHMOD` = 新权限位; `STAT` 未用 (填 0)。
    pub aux: u32,
    /// 对齐填充 (使 `buf` 8 字节对齐)。
    pub _pad: u32,
    /// 路径所在 (也是结果写入的) 共享页虚拟地址。
    pub buf: u64,
}

/// 目录条目里长名 (VFAT LFN / ext2 / MFS 名字) 的最大字节数。
///
/// VFAT 的长名上限是 255 个 UTF-16 码元, 但内核字体只有 ASCII, 超出这个长度的名字
/// 既显示不下也没必要: 超过则截断 (只截整字符边界)。
///
/// 客户端路径走 IPC payload (96 字节), 故端到端可达 ~90 字节; 这里取 128 保证服务端
/// 能回传完整名字 (磁盘侧 MFS 支持到 255, 见 roadmap 阶段 D/M4)。
pub const DIR_LONG_MAX: usize = 128;

/// 结果页的大小 (与内核 `PAGE_SIZE` 一致), 用于给 readdir 定条目上限。
pub const RESULT_PAGE_SIZE: usize = 4096;

/// 一页结果缓冲最多能放的条目数。
///
/// readdir 必须按此上限截断: 结果页只有一页, 条目写多了会越界写到相邻映射之外
/// (每个文件服务都共享同一个客户端页)。
pub const RESULT_MAX_ENTRIES: usize = RESULT_PAGE_SIZE / core::mem::size_of::<DirEntry>();

/// 结构化目录条目 — `readdir` 写入 `RESULT_BUF` 的固定大小记录。
/// 返回字节数 = 条目数 × `size_of::<DirEntry>()`。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct DirEntry {
    /// 原始 8.3 短名: 主名 8 字节 + 扩展名 3 字节, 空格填充 (无 '.' 分隔)。
    /// 没有短名概念的文件系统 (ext2) 也填一个截断后的等价形式供回退显示。
    pub name: [u8; 11],
    /// 长名字节数 (UTF-8); 0 = 该条目没有长名, 显示与匹配都用短名。
    pub long_len: u8,
    /// 对齐填充 (使 `long` 与 `size` 保持 4 字节对齐)。
    pub _pad: [u8; 3],
    /// 长名 (UTF-8, 最多 `DIR_LONG_MAX` 字节); `long_len == 0` 时内容无意义。
    pub long: [u8; DIR_LONG_MAX],
    /// 文件大小 (目录为 0)。
    pub size: u32,
    /// 1 = 目录, 0 = 普通文件。
    pub is_dir: u32,
    /// 权限位 (仅 MFS 提供; 其它服务填 0755/0644)。仅展示。
    pub mode: u16,
    /// 属主域 id (仅 MFS 提供; 其它服务填 0)。
    pub owner: u16,
    /// 硬链接数 (仅 MFS 提供; 其它服务填 1)。
    pub nlink: u32,
    /// 最后修改时间 (Unix 秒; 0 = 未知)。
    pub mtime: u64,
}

impl DirEntry {
    /// 只有 8.3 短名的条目 (内部只按短名寻址的文件服务: tmpfs / MFS)。
    pub const fn short(name: [u8; 11], size: u32, is_dir: u32) -> Self {
        Self {
            name,
            long_len: 0,
            _pad: [0; 3],
            long: [0; DIR_LONG_MAX],
            size,
            is_dir,
            mode: if is_dir == 1 { 0o755 } else { 0o644 },
            owner: 0,
            nlink: 1,
            mtime: 0,
        }
    }

    /// 带长名的条目 (VFAT 长名 / ext2 名字)。
    pub const fn with_long(name: [u8; 11], long: [u8; DIR_LONG_MAX], long_len: u8, size: u32, is_dir: u32) -> Self {
        Self {
            name,
            long_len,
            _pad: [0; 3],
            long,
            size,
            is_dir,
            mode: if is_dir == 1 { 0o755 } else { 0o644 },
            owner: 0,
            nlink: 1,
            mtime: 0,
        }
    }
}

/// 路径元数据 — `stat` 写入 `RESULT_BUF` 的单条记录。
/// 返回字节数 = `size_of::<Stat>()`。
///
/// `mode` / `owner` / `nlink` / 三个时间是 MFS 提供的节点元数据 (见 roadmap M5);
/// 其它文件服务没有对应概念, 一律填默认值 (`mode` 按目录/文件给 0755/0644, 时间为 0)。
/// 时间单位是 Unix 秒 (UTC), 0 表示"未知"。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Stat {
    /// 文件大小 (目录为 0)。
    pub size: u32,
    /// 1 = 目录, 0 = 普通文件。
    pub is_dir: u32,
    /// 权限位 (低 12 位: setuid/setgid/sticky + rwxrwxrwx)。仅存储/显示, **不强制**。
    pub mode: u16,
    /// 属主域 id (创建者)。仅展示。
    pub owner: u16,
    /// 硬链接数。
    pub nlink: u32,
    /// 最后修改时间 (内容变更)。
    pub mtime: u64,
    /// 状态变更时间 (元数据变更)。
    pub ctime: u64,
    /// 最后访问时间 (MFS 不随读更新, 与创建/写入同刻)。
    pub atime: u64,
}

impl Stat {
    /// 元数据缺省值: 按类型给常规权限, 其余置 0 ("未知")。
    pub const fn plain(size: u32, is_dir: u32) -> Self {
        Self {
            size,
            is_dir,
            mode: if is_dir == 1 { 0o755 } else { 0o644 },
            owner: 0,
            nlink: 1,
            mtime: 0,
            ctime: 0,
            atime: 0,
        }
    }
}

/// 把 `path` 编码进 IPC payload (其余字节为 0, 故路径恒以 NUL 结尾,
/// 服务端按 NUL 取长度)。返回实际写入的路径长度。
///
/// payload 只有 `PAYLOAD_LEN` (96) 字节, 故路径上限为 `PAYLOAD_LEN - 1`。
fn encode_path(path: &str, payload: &mut [u8; PAYLOAD_LEN]) -> usize {
    let n = path.len().min(PAYLOAD_LEN - 1);
    payload[..n].copy_from_slice(&path.as_bytes()[..n]);
    n
}

/// 把路径编码成 VFS 请求 payload (未用字节为 0, 故恒以 NUL 结尾)。
fn path_payload(path: &str) -> [u8; PAYLOAD_LEN] {
    let mut payload = [0u8; PAYLOAD_LEN];
    encode_path(path, &mut payload);
    payload
}

/// 服务端返回局部 fd 后, 签发能力句柄并打包为对外 fd。
///
/// 句柄表满时不静默降级: 立刻把刚拿到的服务端 fd 还回去, 整体失败
/// (否则会泄漏一个服务端 fd 槽)。
fn wrap_open(domain: u64, local: u64) -> u64 {
    if local == u64::MAX {
        return u64::MAX;
    }
    let obj = (domain << 32) | (local & 0xFFFF_FFFF);
    let handle = sys_cap_issue(obj);
    if handle == u64::MAX {
        let payload = (local as u32).to_le_bytes();
        sys_call_payload(domain, VFS_CLOSE_TAG, &payload);
        return u64::MAX;
    }
    make_fd(handle, domain, local)
}

/// 打开文件或目录, 成功返回 fd (已编码目标服务域 + 能力句柄), 失败返回 `u64::MAX`。
pub fn open(path: &str) -> u64 {
    match route(path) {
        Some((domain, vol_enc, sub)) => {
            let payload = path_payload(sub);
            wrap_open(
                domain,
                sys_call_payload(domain, with_vol(VFS_OPEN_TAG, vol_enc), &payload),
            )
        }
        None => u64::MAX,
    }
}

/// 从 fd 的 `offset` 起读最多 `count` 字节到 `RESULT_BUF`。
/// 返回实际读取字节数, 失败返回 `u64::MAX`。
pub fn read(fd: u64, offset: u32, count: u32) -> u64 {
    read_into(fd, offset, count, RESULT_BUF)
}

/// 同 `read`, 但结果写入指定的共享缓冲页 `buf` (须已共享给目标服务域)。
pub fn read_into(fd: u64, offset: u32, count: u32, buf: u64) -> u64 {
    if !cap_guard(fd) {
        return u64::MAX;
    }
    let req = ReadReq {
        fd: fd_local(fd),
        offset,
        count,
        _pad: 0,
        buf,
    };
    let payload = unsafe {
        core::slice::from_raw_parts(
            &req as *const ReadReq as *const u8,
            core::mem::size_of::<ReadReq>(),
        )
    };
    sys_call_payload(fd_domain(fd), VFS_READ_TAG, payload)
}

/// 把 `data` (最多一页) 拷入共享写缓冲 `WRITE_BUF`, 并从 fd 的 `offset` 起写
/// `data.len()` 字节。返回实际写入字节数, 失败返回 `u64::MAX`。
pub fn write(fd: u64, offset: u32, data: &[u8]) -> u64 {
    write_into(fd, offset, data, WRITE_BUF)
}

/// 同 `write`, 但数据来自指定的共享写缓冲页 `buf` (须已共享给目标服务域)。
pub fn write_into(fd: u64, offset: u32, data: &[u8], buf: u64) -> u64 {
    if !cap_guard(fd) {
        return u64::MAX;
    }
    let n = data.len().min(4096);
    unsafe {
        core::ptr::copy_nonoverlapping(data.as_ptr(), buf as *mut u8, n);
    }
    let req = WriteReq {
        fd: fd_local(fd),
        offset,
        count: n as u32,
        _pad: 0,
        buf,
    };
    let payload = unsafe {
        core::slice::from_raw_parts(
            &req as *const WriteReq as *const u8,
            core::mem::size_of::<WriteReq>(),
        )
    };
    sys_call_payload(fd_domain(fd), VFS_WRITE_TAG, payload)
}

/// 列出 fd 指向目录的条目, 以 `DirEntry` 记录数组写入 `RESULT_BUF`。
/// 返回写入字节数 (= 条目数 × `size_of::<DirEntry>()`), 失败返回 `u64::MAX`。
pub fn readdir(fd: u64) -> u64 {
    readdir_into(fd, RESULT_BUF)
}

/// 同 `readdir`, 但条目写入指定的共享缓冲页 `buf` (须已共享给目标服务域)。
pub fn readdir_into(fd: u64, buf: u64) -> u64 {
    if !cap_guard(fd) {
        return u64::MAX;
    }
    let req = DirReq {
        fd: fd_local(fd),
        _pad: 0,
        buf,
    };
    let payload = unsafe {
        core::slice::from_raw_parts(
            &req as *const DirReq as *const u8,
            core::mem::size_of::<DirReq>(),
        )
    };
    sys_call_payload(fd_domain(fd), VFS_READDIR_TAG, payload)
}

/// 关闭 fd: 先让服务端释放其局部 fd, 再撤销能力句柄。
/// 成功返回 1, 失败返回 0。
///
/// 句柄撤销后, 继续用该 fd 做任何 I/O 都会被 `cap_guard` 拒绝 (即使 fd 数值
/// 被再次伪造, 内核也不会给它签发新句柄)。
pub fn close(fd: u64) -> u64 {
    if fd == u64::MAX {
        return 0;
    }
    let payload = fd_local(fd).to_le_bytes();
    let r = sys_call_payload(fd_domain(fd), VFS_CLOSE_TAG, &payload);
    sys_cap_drop(fd_handle(fd));
    if r == 1 {
        1
    } else {
        0
    }
}

/// 创建/打开文件: 若 `path` 已存在则直接打开 (不截断), 否则创建空文件。
/// 成功返回 fd (已编码目标服务域 + 能力句柄), 失败返回 `u64::MAX`。
pub fn creat(path: &str) -> u64 {
    match route(path) {
        Some((domain, vol_enc, sub)) => {
            let payload = path_payload(sub);
            wrap_open(
                domain,
                sys_call_payload(domain, with_vol(VFS_CREAT_TAG, vol_enc), &payload),
            )
        }
        None => u64::MAX,
    }
}

/// 创建目录 `path`, 成功返回 1, 失败返回 `u64::MAX`。
pub fn mkdir(path: &str) -> u64 {
    match route(path) {
        Some((domain, vol_enc, sub)) => {
            let payload = path_payload(sub);
            sys_call_payload(domain, with_vol(VFS_MKDIR_TAG, vol_enc), &payload)
        }
        None => u64::MAX,
    }
}

/// 删除文件 `path` (不删目录), 成功返回 1, 失败返回 `u64::MAX`。
pub fn unlink(path: &str) -> u64 {
    match route(path) {
        Some((domain, vol_enc, sub)) => {
            let payload = path_payload(sub);
            sys_call_payload(domain, with_vol(VFS_UNLINK_TAG, vol_enc), &payload)
        }
        None => u64::MAX,
    }
}

/// 删除空目录 `path`, 成功返回 1, 失败返回 `u64::MAX`。
pub fn rmdir(path: &str) -> u64 {
    match route(path) {
        Some((domain, vol_enc, sub)) => {
            let payload = path_payload(sub);
            sys_call_payload(domain, with_vol(VFS_RMDIR_TAG, vol_enc), &payload)
        }
        None => u64::MAX,
    }
}

/// 查询 `path` 的元数据, 以 `Stat` 记录写入 `RESULT_BUF`。
/// 返回字节数 (= `size_of::<Stat>()`), 失败返回 `u64::MAX`。
///
/// 以 `RESULT_BUF` 为共享页的客户端 (app) 直接用它; 其它客户端 (shell) 用
/// `stat_into` 指定自己的结果页。
pub fn stat(path: &str) -> u64 {
    stat_into(path, RESULT_BUF)
}

/// 同 `stat`, 但结果写入 `buf` 指定的共享页 (须已共享给目标文件服务域)。
pub fn stat_into(path: &str, buf: u64) -> u64 {
    match route(path) {
        Some((domain, vol_enc, sub)) => {
            let req = PathReq { aux: 0, _pad: 0, buf };
            write_cstr(sub, buf);
            let payload = unsafe {
                core::slice::from_raw_parts(
                    &req as *const PathReq as *const u8,
                    core::mem::size_of::<PathReq>(),
                )
            };
            sys_call_payload(domain, with_vol(VFS_STAT_TAG, vol_enc), payload)
        }
        None => u64::MAX,
    }
}

/// 修改权限位为 `mode` (低 12 位), 成功返回 1, 失败 `u64::MAX`。
///
/// 权限位当前只存储与显示, 不参与访问判定 (没有多用户概念)。
pub fn chmod(path: &str, mode: u32) -> u64 {
    chmod_into(path, mode, RESULT_BUF)
}

/// 同 `chmod`, 但把路径写进 `buf` 指定的共享页 (须已共享给目标文件服务域)。
pub fn chmod_into(path: &str, mode: u32, buf: u64) -> u64 {
    match route(path) {
        Some((domain, vol_enc, sub)) => {
            let req = PathReq { aux: mode, _pad: 0, buf };
            write_cstr(sub, buf);
            let payload = unsafe {
                core::slice::from_raw_parts(
                    &req as *const PathReq as *const u8,
                    core::mem::size_of::<PathReq>(),
                )
            };
            sys_call_payload(domain, with_vol(VFS_CHMOD_TAG, vol_enc), payload)
        }
        None => u64::MAX,
    }
}

/// 把文件 `fd` 的长度截断/扩展到 `size` 字节, 成功返回 1, 失败 `u64::MAX`。
///
/// 截短会释放尾部数据块; 扩展为**稀疏**(未写过的区间读回 0)。目录不支持截断。
pub fn truncate(fd: u64, size: u32) -> u64 {
    if !cap_guard(fd) {
        return u64::MAX;
    }
    let req = TruncateReq { fd: fd_local(fd), size };
    let payload = unsafe {
        core::slice::from_raw_parts(
            &req as *const TruncateReq as *const u8,
            core::mem::size_of::<TruncateReq>(),
        )
    };
    sys_call_payload(fd_domain(fd), VFS_TRUNCATE_TAG, payload)
}

/// 把 `src` 重命名/移动为 `dst` (可跨目录, 必须在同一文件服务内)。
/// 成功返回 1, 失败 `u64::MAX`。
pub fn rename(src: &str, dst: &str) -> u64 {
    rename_into(src, dst, RESULT_BUF)
}

/// 同 `rename`, 但把两条路径写进 `buf` 指定的共享页 (须已共享给目标文件服务域)。
///
/// 两条路径经共享页传 (IPC payload 装不下), 布局 `src\0dst\0`。
pub fn rename_into(src: &str, dst: &str, buf: u64) -> u64 {
    let (sd, senc, ssub) = match route(src) {
        Some(x) => x,
        None => return u64::MAX,
    };
    let (dd, denc, dsub) = match route(dst) {
        Some(x) => x,
        None => return u64::MAX,
    };
    // 跨文件服务 (跨挂载点) 的 rename 需要搬迁数据, 当前不支持; 跨卷同理。
    if sd != dd || senc != denc {
        return u64::MAX;
    }
    let req = TwoPathReq {
        a_len: ssub.len() as u32,
        b_len: dsub.len() as u32,
        buf,
    };
    // 先写页再取 payload: 两条路径都进共享页, 服务端从页里读。
    write_two_paths(ssub, dsub, buf);
    let payload = unsafe {
        core::slice::from_raw_parts(
            &req as *const TwoPathReq as *const u8,
            core::mem::size_of::<TwoPathReq>(),
        )
    };
    sys_call_payload(sd, with_vol(VFS_RENAME_TAG, senc), payload)
}

/// 把 NUL 结尾的短字符串写进共享页 (供 `PathReq` / `TwoPathReq` 使用)。
///
/// 长度按 `PAYLOAD_LEN - 1` 截断 —— 与服务端从页里读路径的上限一致。
fn write_cstr(s: &str, buf: u64) {
    let n = s.len().min(PAYLOAD_LEN - 1);
    unsafe {
        let p = buf as *mut u8;
        core::ptr::copy_nonoverlapping(s.as_bytes().as_ptr(), p, n);
        *p.add(n) = 0;
    }
}

/// 为 `src` 再建一个名字 `dst` (硬链接, 仅同一文件服务内)。成功返回 1。
pub fn link(src: &str, dst: &str) -> u64 {
    link_into(src, dst, RESULT_BUF)
}

/// 同 `link`, 但把两条路径写进 `buf` 指定的共享页 (须已共享给目标文件服务域)。
pub fn link_into(src: &str, dst: &str, buf: u64) -> u64 {
    let (sd, senc, ssub) = match route(src) {
        Some(x) => x,
        None => return u64::MAX,
    };
    let (dd, denc, dsub) = match route(dst) {
        Some(x) => x,
        None => return u64::MAX,
    };
    if sd != dd || senc != denc {
        return u64::MAX; // 跨文件服务 / 跨卷不支持
    }
    let req = TwoPathReq {
        a_len: ssub.len() as u32,
        b_len: dsub.len() as u32,
        buf,
    };
    write_two_paths(ssub, dsub, buf);
    let payload = unsafe {
        core::slice::from_raw_parts(
            &req as *const TwoPathReq as *const u8,
            core::mem::size_of::<TwoPathReq>(),
        )
    };
    sys_call_payload(sd, with_vol(VFS_LINK_TAG, senc), payload)
}

/// 为 `linkpath` 建一个指向 `target` 的软链接 (M5c, 仅同一文件服务内)。成功返回 1。
pub fn symlink(target: &str, linkpath: &str) -> u64 {
    symlink_into(target, linkpath, RESULT_BUF)
}

/// 同 `symlink`, 但把两条路径写进 `buf` 指定的共享页 (须已共享给目标文件服务域)。
///
/// 只有 `linkpath` 走挂载层路由: `target` 是**存进链接节点的字符串**, 由服务端在解析
/// 时才解释 (绝对 / 相对), 因此不在这里查它的所属文件系统。
///
/// 但绝对目标要先**换命名空间**: 文件服务只看得见自己那棵子树 (挂载点就是它的根),
/// 不认识 `/mfs` 这一层挂载前缀 —— 直接存 `/mfs/a` 会变成服务内部的 `ROOT/mfs/a`,
/// 解析必然失败。若目标落在**同一个**挂载点内, 这里剥掉前缀 (`/mfs/a` -> `/a`);
/// 落在别的文件系统上则原样存下 (服务端解析不到, 成为悬空链接 —— 跨文件系统的软
/// 链接本就不支持, 见 docs/roadmap-fs.md M5c)。相对目标不受影响, 原样存。
pub fn symlink_into(target: &str, linkpath: &str, buf: u64) -> u64 {
    let (ld, lenc, lsub) = match route(linkpath) {
        Some(x) => x,
        None => return u64::MAX,
    };
    if target.is_empty() {
        return u64::MAX;
    }
    let mut tgt = target;
    if target.as_bytes()[0] == b'/' {
        if let Some((td, tenc, tplen)) = mount_lookup(target) {
            if td == ld && tenc == lenc {
                let rest = &target[tplen..];
                tgt = if rest.is_empty() { "/" } else { rest };
            }
        }
    }
    // 两条路径要一起塞进共享页, 服务端按 `a_len + 1 + b_len` 校验, 这里先挡一次
    // (超长直接失败, 不写出一份会被服务端拒绝的请求)。
    if tgt.len() + 1 + lsub.len() > PAYLOAD_LEN - 1 {
        return u64::MAX;
    }
    let req = TwoPathReq {
        a_len: tgt.len() as u32,
        b_len: lsub.len() as u32,
        buf,
    };
    write_two_paths(tgt, lsub, buf);
    let payload = unsafe {
        core::slice::from_raw_parts(
            &req as *const TwoPathReq as *const u8,
            core::mem::size_of::<TwoPathReq>(),
        )
    };
    sys_call_payload(ld, with_vol(VFS_SYMLINK_TAG, lenc), payload)
}

/// 把两条路径按 `src\0dst\0` 写进共享页。
fn write_two_paths(a: &str, b: &str, buf: u64) {
    unsafe {
        let p = buf as *mut u8;
        core::ptr::copy_nonoverlapping(a.as_bytes().as_ptr(), p, a.len());
        *p.add(a.len()) = 0;
        core::ptr::copy_nonoverlapping(b.as_bytes().as_ptr(), p.add(a.len() + 1), b.len());
        *p.add(a.len() + 1 + b.len()) = 0;
    }
}
