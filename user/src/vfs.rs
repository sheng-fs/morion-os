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

use crate::syscall::{sys_call_payload, sys_cap_drop, sys_cap_issue, sys_cap_lookup};

/// fat32 文件服务域 id (与内核 `main.rs` 创建顺序一致), 挂载于 `/`。
pub const FAT32_DOMAIN: u64 = 6;
/// 挂载管理服务域 id: 维护「挂载点前缀 → 文件服务域」表, 供 libvfs 查询路由。
pub const MOUNT_DOMAIN: u64 = 9;
/// tmpfs 内存文件服务域 id, 挂载于 `/tmp`。
pub const TMPFS_DOMAIN: u64 = 10;
/// MorionFS (MFS) 文件服务域 id, 挂载于 `/mfs`。
pub const MFS_DOMAIN: u64 = 11;

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

/// 挂载查询 tag: 请求 payload 为路径, 回复为 `(服务域 << 32) | 挂载点前缀长度`,
/// 无匹配返回 `u64::MAX`。仅 mount_srv 处理。
pub const VFS_LOOKUP_TAG: u64 = 0x4D4E_5451; // "MNTQ"

/// 运行时挂载 tag: payload 为 `MountReq` (服务域 + 挂载点前缀; 前缀为空表示请
/// mount_srv 自动分配一个空闲 `/mnt<N>`)。回复挂载槽位号 (1 起), 失败 `u64::MAX`。
pub const VFS_MOUNT_TAG: u64 = 0x4D4E_5441; // "MNTA"
/// 运行时卸载 tag: payload 为挂载点前缀 (NUL 结尾)。回复 1 / `u64::MAX`。
pub const VFS_UMOUNT_TAG: u64 = 0x4D4E_5444; // "MNTD"

/// 挂载点前缀最大长度 (与 mount_srv 的 `MOUNT_PREFIX_MAX` 一致)。
pub const MOUNT_PREFIX_MAX: usize = 24;

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
    let mut payload = [0u8; 32];
    let n = prefix.len().min(31);
    payload[..n].copy_from_slice(&prefix.as_bytes()[..n]);
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

/// 向挂载服务查询 `path` 应路由到的文件服务域, 返回 (服务域, 挂载点前缀长度)。
///
/// 未挂载 / 查询失败返回 None。路径长度按 payload 上限截断 (挂载点都是短前缀)。
fn mount_lookup(path: &str) -> Option<(u64, usize)> {
    let mut payload = [0u8; 32];
    let n = path.len().min(31);
    payload[..n].copy_from_slice(&path.as_bytes()[..n]);
    let r = sys_call_payload(MOUNT_DOMAIN, VFS_LOOKUP_TAG, &payload);
    if r == u64::MAX {
        return None;
    }
    let domain = r >> 32;
    let prefix_len = (r & 0xFFFF_FFFF) as usize;
    // 域 0/挂载前缀长度非法视为查询失败 (无文件服务挂在域 0)。
    if domain == 0 || prefix_len == 0 || prefix_len > path.len() {
        return None;
    }
    Some((domain, prefix_len))
}

/// 把绝对路径 `path` 解析为 (目标服务域, 相对挂载点根的路径)。
///
/// 相对路径保留前导 '/' (服务端一律按以 '/' 开头的绝对路径处理子路径)。例如
/// `/tmp/a` 在 `/tmp`→tmpfs 下得到 `("/a", tmpfs)`; `/h.txt` 在 `/`→fat32 下
/// 得到 `("/h.txt", fat32)`。
fn route(path: &str) -> Option<(u64, &str)> {
    let (domain, prefix_len) = mount_lookup(path)?;
    if prefix_len > path.len() {
        return None;
    }
    // 根挂载 ("/", 前缀长度 1): 路径本身就是服务内绝对路径, 原样下发。
    if prefix_len == 1 {
        return Some((domain, path));
    }
    // 其它挂载点: 去掉挂载前缀, 余下部分自带前导 '/' (如 `/tmp/D1` → `/D1`);
    // 恰好等于挂载点时余下为空, 即子树根 `/`。
    let rest = &path[prefix_len..];
    Some((domain, if rest.is_empty() { "/" } else { rest }))
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

/// 结构化目录条目 — `readdir` 写入 `RESULT_BUF` 的固定大小记录。
/// 返回字节数 = 条目数 × `size_of::<DirEntry>()`。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct DirEntry {
    /// 原始 8.3 短名: 主名 8 字节 + 扩展名 3 字节, 空格填充 (无 '.' 分隔)。
    pub name: [u8; 11],
    /// 文件大小 (目录为 0)。
    pub size: u32,
    /// 1 = 目录, 0 = 普通文件。
    pub is_dir: u32,
}

/// 路径元数据 — `stat` 写入 `RESULT_BUF` 的单条记录。
/// 返回字节数 = `size_of::<Stat>()`。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Stat {
    /// 文件大小 (目录为 0)。
    pub size: u32,
    /// 1 = 目录, 0 = 普通文件。
    pub is_dir: u32,
}

/// 把 `path` 编码进 32 字节 payload (其余字节为 0, 故路径恒以 NUL 结尾,
/// 服务端按 NUL 取长度)。返回实际写入的路径长度。
fn encode_path(path: &str, payload: &mut [u8; 32]) -> usize {
    let n = path.len().min(31);
    payload[..n].copy_from_slice(&path.as_bytes()[..n]);
    n
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
        Some((domain, sub)) => {
            let mut payload = [0u8; 32];
            encode_path(sub, &mut payload);
            wrap_open(domain, sys_call_payload(domain, VFS_OPEN_TAG, &payload))
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
        Some((domain, sub)) => {
            let mut payload = [0u8; 32];
            encode_path(sub, &mut payload);
            wrap_open(domain, sys_call_payload(domain, VFS_CREAT_TAG, &payload))
        }
        None => u64::MAX,
    }
}

/// 创建目录 `path`, 成功返回 1, 失败返回 `u64::MAX`。
pub fn mkdir(path: &str) -> u64 {
    match route(path) {
        Some((domain, sub)) => {
            let mut payload = [0u8; 32];
            encode_path(sub, &mut payload);
            sys_call_payload(domain, VFS_MKDIR_TAG, &payload)
        }
        None => u64::MAX,
    }
}

/// 删除文件 `path` (不删目录), 成功返回 1, 失败返回 `u64::MAX`。
pub fn unlink(path: &str) -> u64 {
    match route(path) {
        Some((domain, sub)) => {
            let mut payload = [0u8; 32];
            encode_path(sub, &mut payload);
            sys_call_payload(domain, VFS_UNLINK_TAG, &payload)
        }
        None => u64::MAX,
    }
}

/// 删除空目录 `path`, 成功返回 1, 失败返回 `u64::MAX`。
pub fn rmdir(path: &str) -> u64 {
    match route(path) {
        Some((domain, sub)) => {
            let mut payload = [0u8; 32];
            encode_path(sub, &mut payload);
            sys_call_payload(domain, VFS_RMDIR_TAG, &payload)
        }
        None => u64::MAX,
    }
}

/// 查询 `path` 的元数据, 以 `Stat` 记录写入 `RESULT_BUF`。
/// 返回字节数 (= `size_of::<Stat>()`), 失败返回 `u64::MAX`。
///
/// 注: 当前 stat 不带缓冲地址 (沿用 `RESULT_BUF`), 故仅对以 `RESULT_BUF` 为
/// 共享页的客户端 (app) 有效; shell 不使用 stat。
pub fn stat(path: &str) -> u64 {
    match route(path) {
        Some((domain, sub)) => {
            let mut payload = [0u8; 32];
            encode_path(sub, &mut payload);
            sys_call_payload(domain, VFS_STAT_TAG, &payload)
        }
        None => u64::MAX,
    }
}
