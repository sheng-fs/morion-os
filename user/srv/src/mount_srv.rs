use crate::common::*;
use morion::syscall::*;
use morion::vfs;

// ===========================================================================
// 域 9 — mount_srv (挂载管理服务)
// ===========================================================================
// docs/architecture.md「挂载与统一目录树」: 由用户态挂载服务维护全局命名空间,
// 把各文件服务的目录树拼成一个逻辑树。libvfs 在发起请求前查询本服务, 由 VFS
// 库据此完成路由 —— 应用只看到单一根 `/`。
//
// 查询回复打包为 `(服务域 << 32) | 挂载点前缀长度`: libvfs 依此去掉挂载点前缀,
// 得到发给目标文件服务的子路径。

// 挂载表是**运行时可变的**: 启动时写入引导用的默认项 (三个编译期已知的核心
// 文件服务), 之后任何服务都能通过 `MNTA` / `MNTD` 在运行时挂载 / 卸载, 无需
// 重新编译 (阶段 C3)。

/// 挂载表容量 (含自动分配出来的空闲挂载点)。
///
/// 8 个槽在「多卷挂载」(M1b) 下不够用: 引导默认项 5 个 + 每个文件服务上报的
/// 额外卷各占一个 (`/usb<卷号>`), 插一块带两个分区的 U 盘就撑满了 —— 表满时
/// 连 `MNTA` 自动分配的 `/mnt<N>` 都拿不到槽位 (FS-6 因此失败)。
const MOUNT_MAX: usize = 16;
/// 挂载点前缀最大长度 (含前导 '/', 不含结尾 NUL)。
const MOUNT_PREFIX_MAX: usize = 24;

/// 一条挂载记录: 挂载点前缀 → 文件服务域 (可选绑定一个卷)。
#[derive(Clone, Copy)]
struct MountEntry {
    used: bool,
    /// 前缀长度 (不含结尾 NUL)。
    plen: u8,
    /// 卷编码: 0 = 该服务的默认卷; 否则 = block_srv 卷号 + 1 (M1b 多卷挂载)。
    vol_enc: u32,
    domain: u64,
    prefix: [u8; MOUNT_PREFIX_MAX],
}

const MOUNT_EMPTY: MountEntry = MountEntry {
    used: false,
    plen: 0,
    vol_enc: 0,
    domain: 0,
    prefix: [0; MOUNT_PREFIX_MAX],
};

static mut MOUNTS: [MountEntry; MOUNT_MAX] = [MOUNT_EMPTY; MOUNT_MAX];

fn mount_at(i: usize) -> &'static MountEntry {
    unsafe { &*core::ptr::addr_of!(MOUNTS).cast::<MountEntry>().add(i) }
}

fn mount_at_mut(i: usize) -> &'static mut MountEntry {
    unsafe { &mut *core::ptr::addr_of_mut!(MOUNTS).cast::<MountEntry>().add(i) }
}

/// 取该记录的挂载点前缀字符串。
fn mount_prefix_of(e: &MountEntry) -> &str {
    unsafe { core::str::from_utf8_unchecked(&e.prefix[..e.plen as usize]) }
}

/// 判断 `path` 是否落在挂载点 `prefix` 下; 是则返回前缀长度。
/// 匹配须落在组件边界: `/tmpfoo` 不匹配 `/tmp`。
fn mount_prefix_match(path: &str, prefix: &str) -> Option<usize> {
    let p = path.as_bytes();
    if p.first() != Some(&b'/') {
        return None;
    }
    if prefix == "/" {
        return Some(1);
    }
    let q = prefix.as_bytes();
    if p.len() < q.len() || &p[..q.len()] != q {
        return None;
    }
    // 路径恰为挂载点, 或挂载点后紧跟 '/', 才算命中。
    if p.len() == q.len() || p[q.len()] == b'/' {
        Some(q.len())
    } else {
        None
    }
}

/// 精确查找挂载点 `prefix`, 返回槽位下标。
fn mount_find(prefix: &str) -> Option<usize> {
    for i in 0..MOUNT_MAX {
        let e = mount_at(i);
        if e.used && mount_prefix_of(e) == prefix {
            return Some(i);
        }
    }
    None
}

/// 写入一条挂载记录, 成功返回挂载槽位号 (1 起)。
///
/// `vol_enc` 为 0 表示「该服务的默认卷」(服务自己认领的那一个); 非 0 表示把该
/// 挂载点绑定到 `vol_enc - 1` 号卷 (M1b 多卷挂载), 该编码随每次请求下发。
///
/// 拒绝: 空前缀 / 非绝对路径 / 前缀过长 / 域号为 0 / 该前缀已被占用
/// (重复挂载同一前缀需先 `MNTD`, 避免静默改写别人的命名空间)。
fn mount_add(prefix: &str, domain: u64, vol_enc: u32) -> u64 {
    let b = prefix.as_bytes();
    if b.is_empty() || b[0] != b'/' || b.len() >= MOUNT_PREFIX_MAX || domain == 0 {
        return u64::MAX;
    }
    if mount_find(prefix).is_some() {
        return u64::MAX;
    }
    for i in 0..MOUNT_MAX {
        let e = mount_at_mut(i);
        if !e.used {
            e.used = true;
            e.domain = domain;
            e.vol_enc = vol_enc;
            e.plen = b.len() as u8;
            e.prefix = [0; MOUNT_PREFIX_MAX];
            e.prefix[..b.len()].copy_from_slice(b);
            return (i + 1) as u64;
        }
    }
    u64::MAX
}

/// 自动分配挂载点: 取最小的未被占用的 `/mnt<N>`, 服务默认卷。
/// 成功返回挂载槽位号 (1 起)。
fn mount_auto(domain: u64) -> u64 {
    let mut buf = [0u8; MOUNT_PREFIX_MAX];
    buf[..4].copy_from_slice(b"/mnt");
    for n in 0..MOUNT_MAX {
        buf[4] = b'0' + n as u8;
        let name = unsafe { core::str::from_utf8_unchecked(&buf[..5]) };
        if mount_find(name).is_none() {
            return mount_add(name, domain, 0);
        }
    }
    u64::MAX
}

/// 自动分配「额外卷」挂载点: `/usb<卷号>`, 并绑定该卷 (M1b 多卷挂载)。
///
/// 命名直接用卷号, 故同一台机器上多个服务挂各自的额外卷也不会撞名, 且挂载点与
/// block_srv 卷表一一对应 (`/usb3` = 卷 3)。已经挂过同一卷时静默成功 (幂等)。
fn mount_auto_vol(domain: u64, vol: u64) -> u64 {
    let mut buf = [0u8; MOUNT_PREFIX_MAX];
    buf[..4].copy_from_slice(b"/usb");
    let digits = dec_to_str(vol, &mut buf[4..]);
    let name = unsafe { core::str::from_utf8_unchecked(&buf[..4 + digits]) };
    let r = if let Some(i) = mount_find(name) {
        // 同一前缀已挂上: 只在「同域同卷」时算成功 (重复上报幂等)。
        let e = mount_at(i);
        if e.domain == domain && e.vol_enc == vfs::enc_of_vol(vol) {
            (i + 1) as u64
        } else {
            u64::MAX
        }
    } else {
        mount_add(name, domain, vfs::enc_of_vol(vol))
    };
    // 启动期诊断 (与 `mfs-dbg` / `exfat-dbg` 同类): 记下额外卷挂到了哪个前缀。
    print("mount-dbg: ");
    print(name);
    print(" domain=");
    print_u64(domain);
    print(" slot=");
    print_u64(r);
    println("");
    r
}

/// 把 `v` 的十进制写法写进 `dst`, 返回写入的字节数 (不使用堆)。
fn dec_to_str(v: u64, dst: &mut [u8]) -> usize {
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

/// 卸载挂载点 `prefix`, 成功返回 1。
/// 根 `/` 不可卸载 (否则整个命名空间失去根)。
fn mount_del(prefix: &str) -> u64 {
    if prefix == "/" {
        return u64::MAX;
    }
    match mount_find(prefix) {
        Some(i) => {
            mount_at_mut(i).used = false;
            1
        }
        None => u64::MAX,
    }
}

/// 写入引导用的默认挂载: 三个编译期已知的核心文件服务。
/// 其余服务一律走运行时 `MNTA`。
fn mounts_init() {
    mount_add("/", vfs::FAT32_DOMAIN, 0);
    mount_add("/tmp", vfs::TMPFS_DOMAIN, 0);
    mount_add("/mfs", vfs::MFS_DOMAIN, 0);
    mount_add("/ext2", vfs::EXT2_DOMAIN, 0);
    mount_add("/usb", vfs::EXFAT_DOMAIN, 0);
}

/// 在挂载表中查最长匹配前缀, 返回 (服务域, 卷编码, 前缀长度)。
fn mount_resolve(path: &str) -> Option<(u64, u32, usize)> {
    let mut best: Option<(u64, u32, usize)> = None;
    for i in 0..MOUNT_MAX {
        let e = mount_at(i);
        if !e.used {
            continue;
        }
        if let Some(len) = mount_prefix_match(path, mount_prefix_of(e)) {
            if best.is_none_or(|(_, _, bl)| len > bl) {
                best = Some((e.domain, e.vol_enc, len));
            }
        }
    }
    best
}

/// 域 9 — 挂载服务: 处理路由查询 (`MNTQ`) 与运行时挂载 / 卸载 (`MNTA` / `MNTD`)。
pub fn run() {
    mounts_init();
    let mut msg = Message {
        from: 0,
        to: 0,
        tag: 0,
        payload: [0; PAYLOAD_LEN],
    };
    loop {
        sys_recv_msg(&mut msg as *mut Message as *mut u8);
        match msg.tag {
            vfs::VFS_LOOKUP_TAG => {
                let len = msg
                    .payload
                    .iter()
                    .position(|&b| b == 0)
                    .unwrap_or(PAYLOAD_LEN);
                let path = unsafe { core::str::from_utf8_unchecked(&msg.payload[..len]) };
                // 回复布局: `[63:40] 卷编码 | [39:32] 服务域 | [31:0] 前缀长度`
                // (libvfs 按同布局解出, 见 `vfs::mount_lookup`)。
                let r = match mount_resolve(path) {
                    Some((domain, vol_enc, prefix_len)) => {
                        ((vol_enc as u64) << 40) | (domain << 32) | prefix_len as u64
                    }
                    None => u64::MAX,
                };
                sys_reply(r);
            }
            vfs::VFS_MOUNT_TAG => {
                // payload = MountReq { domain, prefix[24] }; 前缀为空则自动分配。
                let req = unsafe { &*(msg.payload.as_ptr() as *const vfs::MountReq) };
                let plen = req
                    .prefix
                    .iter()
                    .position(|&b| b == 0)
                    .unwrap_or(vfs::MOUNT_PREFIX_MAX);
                let r = if plen == 0 {
                    mount_auto(req.domain)
                } else {
                    let prefix = unsafe { core::str::from_utf8_unchecked(&req.prefix[..plen]) };
                    mount_add(prefix, req.domain, 0)
                };
                sys_reply(r);
            }
            vfs::VFS_MOUNT_VOL_TAG => {
                // payload = MountVolReq { domain, vol }: 把某服务的额外卷挂到 `/usb<卷号>`。
                let req = unsafe { &*(msg.payload.as_ptr() as *const vfs::MountVolReq) };
                sys_reply(mount_auto_vol(req.domain, req.vol));
            }
            vfs::VFS_UMOUNT_TAG => {
                let len = msg
                    .payload
                    .iter()
                    .position(|&b| b == 0)
                    .unwrap_or(PAYLOAD_LEN);
                let prefix = unsafe { core::str::from_utf8_unchecked(&msg.payload[..len]) };
                sys_reply(mount_del(prefix));
            }
            _ => {
                sys_reply(u64::MAX);
            }
        }
    }
}
