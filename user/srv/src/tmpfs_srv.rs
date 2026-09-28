use crate::common::*;
use morion::syscall::*;
use morion::vfs;

// ===========================================================================
// 域 10 — tmpfs_srv (内存文件系统)
// ===========================================================================
// 阶段 C2: 纯内存文件系统, 挂载于 `/tmp`, 与 fat32_srv 共存构成统一目录树,
// 用于验证「多文件服务 + 挂载层路由」。
//
// 存储模型: 平铺节点表 (绝对路径 → 节点) + 字节区; 目录语义由「父路径」关系表达
// (节点 `/A/B` 的父为 `/A`, 根 `/` 预置)。名称统一转大写并限定为 8.3 短名, 与
// libvfs 的 `DirEntry` (11 字节定长) 及 FAT 的大小写不敏感语义保持一致。

/// 节点数量上限。
const TMP_MAX_NODES: usize = 32;
/// 规范化后路径的最大长度。
///
/// 取值 = 单条 IPC payload 的长度: 客户端把绝对路径编码进 payload, 故服务端路径
/// 缓冲按此上限即可覆盖任何合法请求 (tmpfs 与 MFS 的 fd 表都用它)。
const TMP_PATH_MAX: usize = PAYLOAD_LEN;
/// 文件数据区总容量 (字节)。
const TMP_DATA_CAP: usize = 32 * 1024;
/// 打开文件数上限。
const TMP_MAX_FD: usize = 16;

#[derive(Clone, Copy)]
struct TmpNode {
    used: bool,
    is_dir: bool,
    path_len: u8,
    /// 文件逻辑大小。
    size: u32,
    /// 数据区分配容量 (0 = 尚未分配)。
    cap: u32,
    /// 数据区起始偏移。
    data_off: u32,
    path: [u8; TMP_PATH_MAX],
}

const TMP_NODE_EMPTY: TmpNode = TmpNode {
    used: false,
    is_dir: false,
    path_len: 0,
    size: 0,
    cap: 0,
    data_off: 0,
    path: [0; TMP_PATH_MAX],
};

static mut TMP_NODES: [TmpNode; TMP_MAX_NODES] = [TMP_NODE_EMPTY; TMP_MAX_NODES];
static mut TMP_DATA: [u8; TMP_DATA_CAP] = [0; TMP_DATA_CAP];
static mut TMP_DATA_USED: usize = 0;

#[derive(Clone, Copy)]
struct TmpFd {
    used: bool,
    node: u32,
}

const TMP_FD_EMPTY: TmpFd = TmpFd {
    used: false,
    node: 0,
};

static mut TMP_FDS: [TmpFd; TMP_MAX_FD] = [TMP_FD_EMPTY; TMP_MAX_FD];

fn tmp_node_at(i: usize) -> &'static TmpNode {
    unsafe { &*core::ptr::addr_of!(TMP_NODES).cast::<TmpNode>().add(i) }
}

fn tmp_node_at_mut(i: usize) -> &'static mut TmpNode {
    unsafe { &mut *core::ptr::addr_of_mut!(TMP_NODES).cast::<TmpNode>().add(i) }
}

fn tmp_data_ptr(off: usize) -> *mut u8 {
    unsafe { core::ptr::addr_of_mut!(TMP_DATA).cast::<u8>().add(off) }
}

/// 把一个路径分量规整为 8.3 短名 (保留原大小写)。非法 (空主名 / 主名>8 / 扩展>3 /
/// 多个 '.') 返回 None。
///
/// 注意这里**不做**大写归一: `/tmp` 的名字按调用方给的原样存、原样匹配 (大小写
/// 敏感, 与 MFS 一致), 这样 `ls` 看到的就是你输入的样子。
fn tmp_norm_component(seg: &[u8], out: &mut [u8]) -> Option<usize> {
    let mut dot: Option<usize> = None;
    for (i, &b) in seg.iter().enumerate() {
        if b == b'.' {
            if dot.is_some() {
                return None;
            }
            dot = Some(i);
        }
    }
    let (base, ext) = match dot {
        Some(i) => (&seg[..i], &seg[i + 1..]),
        None => (seg, &seg[seg.len()..]),
    };
    if base.is_empty() || base.len() > 8 || ext.len() > 3 {
        return None;
    }
    let mut n = 0usize;
    for &b in base {
        out[n] = b;
        n += 1;
    }
    if !ext.is_empty() {
        out[n] = b'.';
        n += 1;
        for &b in ext {
            out[n] = b;
            n += 1;
        }
    }
    Some(n)
}

/// 规范化绝对路径: 逐分量规整为 8.3, 处理 "." / ".." 与重复 '/'。
/// 结果以 '/' 开头且无尾随 '/' (根为 "/")。返回长度。
fn tmp_normalize(path: &str, out: &mut [u8]) -> Option<usize> {
    let bytes = path.as_bytes();
    if bytes.first() != Some(&b'/') || out.is_empty() {
        return None;
    }
    let mut n = 0usize;
    out[n] = b'/';
    n += 1;
    let mut i = 1usize;
    while i < bytes.len() {
        if bytes[i] == b'/' {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && bytes[i] != b'/' {
            i += 1;
        }
        let seg = &bytes[start..i];
        if seg == b"." {
            continue;
        }
        if seg == b".." {
            if n > 1 {
                let mut k = n - 1;
                while k > 0 && out[k - 1] != b'/' {
                    k -= 1;
                }
                n = if k > 1 { k - 1 } else { 1 };
            }
            continue;
        }
        let mut comp = [0u8; 12];
        let clen = tmp_norm_component(seg, &mut comp)?;
        if n > 1 {
            if n + 1 > out.len() {
                return None;
            }
            out[n] = b'/';
            n += 1;
        }
        if n + clen > out.len() {
            return None;
        }
        out[n..n + clen].copy_from_slice(&comp[..clen]);
        n += clen;
    }
    Some(n)
}

/// 按规范化路径查找节点索引。
fn tmp_find(path: &[u8]) -> Option<usize> {
    for i in 0..TMP_MAX_NODES {
        let nd = tmp_node_at(i);
        if nd.used && &nd.path[..nd.path_len as usize] == path {
            return Some(i);
        }
    }
    None
}

/// 取路径的父路径 (写入 `out`), 返回长度。根 "/" 的父仍为 "/"。
fn tmp_parent(path: &[u8], out: &mut [u8]) -> usize {
    let mut n = path.len();
    while n > 1 && path[n - 1] != b'/' {
        n -= 1;
    }
    let mut m = n;
    while m > 1 && path[m - 1] == b'/' {
        m -= 1;
    }
    if m == 0 {
        m = 1;
    }
    out[..m].copy_from_slice(&path[..m]);
    m
}

/// 分配一个新节点 (路径已规范化), 表满返回 None。
fn tmp_alloc_node(path: &[u8], is_dir: bool) -> Option<usize> {
    if path.is_empty() || path.len() > TMP_PATH_MAX {
        return None;
    }
    for i in 0..TMP_MAX_NODES {
        let nd = tmp_node_at_mut(i);
        if !nd.used {
            nd.used = true;
            nd.is_dir = is_dir;
            nd.path_len = path.len() as u8;
            nd.size = 0;
            nd.cap = 0;
            nd.data_off = 0;
            nd.path[..path.len()].copy_from_slice(path);
            return Some(i);
        }
    }
    None
}

/// 目录 `dir` 是否为空 (无任何其它节点以它为父)。
fn tmp_dir_empty(dir: &[u8]) -> bool {
    let mut parent = [0u8; TMP_PATH_MAX];
    for i in 0..TMP_MAX_NODES {
        let nd = tmp_node_at(i);
        if !nd.used {
            continue;
        }
        let p = &nd.path[..nd.path_len as usize];
        if p == dir {
            continue;
        }
        let plen = tmp_parent(p, &mut parent);
        if &parent[..plen] == dir {
            return false;
        }
    }
    true
}

/// 取路径的最后分量, 填为 11 字节 8.3 短名 (主名 8 + 扩展 3, 空格填充)。
fn tmp_name_83(path: &[u8], out: &mut [u8; 11]) {
    *out = [b' '; 11];
    let mut k = path.len();
    while k > 1 && path[k - 1] != b'/' {
        k -= 1;
    }
    let seg = &path[k..];
    let mut dot = seg.len();
    for (i, &b) in seg.iter().enumerate() {
        if b == b'.' {
            dot = i;
            break;
        }
    }
    let base = &seg[..dot];
    let ext = if dot < seg.len() {
        &seg[dot + 1..]
    } else {
        &seg[seg.len()..]
    };
    let bn = base.len().min(8);
    out[..bn].copy_from_slice(&base[..bn]);
    let en = ext.len().min(3);
    out[8..8 + en].copy_from_slice(&ext[..en]);
}

/// 从数据区分配 `need` 字节 (只增不回收), 空间不足返回 None。
fn tmp_data_alloc(need: usize) -> Option<usize> {
    unsafe {
        let used = *core::ptr::addr_of!(TMP_DATA_USED);
        if used + need > TMP_DATA_CAP {
            return None;
        }
        *core::ptr::addr_of_mut!(TMP_DATA_USED) = used + need;
        Some(used)
    }
}

fn tmp_fd_alloc(node: usize) -> u64 {
    for i in 0..TMP_MAX_FD {
        unsafe {
            let slot = &mut *core::ptr::addr_of_mut!(TMP_FDS).cast::<TmpFd>().add(i);
            if !slot.used {
                slot.used = true;
                slot.node = node as u32;
                return i as u64;
            }
        }
    }
    u64::MAX
}

fn tmp_fd_node(fd: u32) -> Option<usize> {
    if fd as usize >= TMP_MAX_FD {
        return None;
    }
    unsafe {
        let slot = &*core::ptr::addr_of!(TMP_FDS)
            .cast::<TmpFd>()
            .add(fd as usize);
        if slot.used {
            Some(slot.node as usize)
        } else {
            None
        }
    }
}

fn tmp_fd_free(fd: u32) -> u64 {
    if fd as usize >= TMP_MAX_FD {
        return 0;
    }
    unsafe {
        let slot = &mut *core::ptr::addr_of_mut!(TMP_FDS)
            .cast::<TmpFd>()
            .add(fd as usize);
        if slot.used {
            slot.used = false;
            1
        } else {
            0
        }
    }
}

/// 域 10 — tmpfs 服务: 处理与 fat32_srv 相同的 VFS 协议 (open/read/write/...)。
pub fn run() {
    // 预置根目录节点 "/"。
    if tmp_alloc_node(b"/", true).is_none() {
        println("tmpfs: init root FAILED");
        return;
    }

    let mut msg = Message {
        from: 0,
        to: 0,
        tag: 0,
        payload: [0; PAYLOAD_LEN],
    };
    let mut canon = [0u8; TMP_PATH_MAX];
    loop {
        sys_recv_msg(&mut msg as *mut Message as *mut u8);
        // 卷编码 (tag 高位) 对本服务无意义 (tmpfs 没有卷概念), 分发前剥掉。
        match vfs::tag_body(msg.tag) {
            vfs::VFS_OPEN_TAG => {
                let len = msg
                    .payload
                    .iter()
                    .position(|&b| b == 0)
                    .unwrap_or(PAYLOAD_LEN);
                let path = unsafe { core::str::from_utf8_unchecked(&msg.payload[..len]) };
                let fd = match tmp_normalize(path, &mut canon) {
                    Some(n) => match tmp_find(&canon[..n]) {
                        Some(idx) => tmp_fd_alloc(idx),
                        None => u64::MAX,
                    },
                    None => u64::MAX,
                };
                sys_reply(fd);
            }
            vfs::VFS_READ_TAG => {
                let req: vfs::ReadReq = unsafe {
                    core::ptr::read_unaligned(msg.payload.as_ptr() as *const vfs::ReadReq)
                };
                // tmpfs 节点 size 是 u32 (存储 32 KiB), 协议 offset 超出 u32 直接失败。
                if req.offset > u32::MAX as u64 {
                    sys_reply(u64::MAX);
                    continue;
                }
                let offset = req.offset as u32;
                let n = match tmp_fd_node(req.fd) {
                    Some(idx) => {
                        let nd = tmp_node_at(idx);
                        if nd.is_dir {
                            u64::MAX
                        } else if offset >= nd.size {
                            0
                        } else {
                            let cnt = (req.count).min(nd.size - offset) as usize;
                            unsafe {
                                core::ptr::copy_nonoverlapping(
                                    tmp_data_ptr(nd.data_off as usize + offset as usize),
                                    req.buf as *mut u8,
                                    cnt,
                                );
                            }
                            cnt as u64
                        }
                    }
                    None => u64::MAX,
                };
                sys_reply(n);
            }
            vfs::VFS_WRITE_TAG => {
                let req: vfs::WriteReq = unsafe {
                    core::ptr::read_unaligned(msg.payload.as_ptr() as *const vfs::WriteReq)
                };
                if req.offset > u32::MAX as u64 {
                    sys_reply(u64::MAX);
                    continue;
                }
                let offset = req.offset as u32;
                let n = match tmp_fd_node(req.fd) {
                    Some(idx) if !tmp_node_at(idx).is_dir => {
                        let end = offset as usize + req.count as usize;
                        let (mut off, mut cap) = {
                            let nd = tmp_node_at(idx);
                            (nd.data_off as usize, nd.cap as usize)
                        };
                        if end > cap {
                            if let Some(o) = tmp_data_alloc(end) {
                                if cap > 0 {
                                    unsafe {
                                        core::ptr::copy_nonoverlapping(
                                            tmp_data_ptr(off),
                                            tmp_data_ptr(o),
                                            cap,
                                        );
                                    }
                                }
                                off = o;
                                cap = end;
                            }
                        }
                        if cap < end {
                            u64::MAX // 数据区已满
                        } else {
                            unsafe {
                                core::ptr::copy_nonoverlapping(
                                    req.buf as *const u8,
                                    tmp_data_ptr(off + offset as usize),
                                    req.count as usize,
                                );
                            }
                            let nd = tmp_node_at_mut(idx);
                            nd.data_off = off as u32;
                            nd.cap = cap as u32;
                            if end as u32 > nd.size {
                                nd.size = end as u32;
                            }
                            req.count as u64
                        }
                    }
                    _ => u64::MAX,
                };
                sys_reply(n);
            }
            vfs::VFS_READDIR_TAG => {
                let req: vfs::DirReq = unsafe {
                    core::ptr::read_unaligned(msg.payload.as_ptr() as *const vfs::DirReq)
                };
                let n = match tmp_fd_node(req.fd) {
                    Some(idx) if tmp_node_at(idx).is_dir => {
                        let mut dir = [0u8; TMP_PATH_MAX];
                        let dlen = {
                            let nd = tmp_node_at(idx);
                            let d = nd.path_len as usize;
                            dir[..d].copy_from_slice(&nd.path[..d]);
                            d
                        };
                        let entry_size = core::mem::size_of::<vfs::DirEntry>();
                        let out = req.buf as *mut vfs::DirEntry;
                        let mut parent = [0u8; TMP_PATH_MAX];
                        let mut count = 0usize;
                        for i in 0..TMP_MAX_NODES {
                            let child = tmp_node_at(i);
                            if !child.used {
                                continue;
                            }
                            let cp = &child.path[..child.path_len as usize];
                            if cp == &dir[..dlen] {
                                continue; // 自身
                            }
                            let plen = tmp_parent(cp, &mut parent);
                            if plen != dlen || parent[..plen] != dir[..dlen] {
                                continue;
                            }
                            // 结果页只有一页: 放不下就停在已写入的条目上。
                            if count + 1 > vfs::RESULT_MAX_ENTRIES {
                                break;
                            }
                            let mut e = vfs::DirEntry::short(
                                [0u8; 11],
                                if child.is_dir { 0 } else { child.size as u64 },
                                if child.is_dir { 1 } else { 0 },
                            );
                            tmp_name_83(cp, &mut e.name);
                            unsafe {
                                core::ptr::write_unaligned(out.add(count), e);
                            }
                            count += 1;
                        }
                        (count * entry_size) as u64
                    }
                    _ => u64::MAX,
                };
                sys_reply(n);
            }
            vfs::VFS_CLOSE_TAG => {
                let fd = read_u32(msg.payload.as_ptr());
                sys_reply(tmp_fd_free(fd));
            }
            vfs::VFS_CREAT_TAG => {
                let len = msg
                    .payload
                    .iter()
                    .position(|&b| b == 0)
                    .unwrap_or(PAYLOAD_LEN);
                let path = unsafe { core::str::from_utf8_unchecked(&msg.payload[..len]) };
                let fd = match tmp_normalize(path, &mut canon) {
                    Some(n) => {
                        if let Some(idx) = tmp_find(&canon[..n]) {
                            if tmp_node_at(idx).is_dir {
                                u64::MAX
                            } else {
                                tmp_fd_alloc(idx)
                            }
                        } else {
                            let mut parent = [0u8; TMP_PATH_MAX];
                            let plen = tmp_parent(&canon[..n], &mut parent);
                            match tmp_find(&parent[..plen]) {
                                Some(pidx) if tmp_node_at(pidx).is_dir => {
                                    match tmp_alloc_node(&canon[..n], false) {
                                        Some(idx) => tmp_fd_alloc(idx),
                                        None => u64::MAX,
                                    }
                                }
                                _ => u64::MAX,
                            }
                        }
                    }
                    None => u64::MAX,
                };
                sys_reply(fd);
            }
            vfs::VFS_MKDIR_TAG => {
                let len = msg
                    .payload
                    .iter()
                    .position(|&b| b == 0)
                    .unwrap_or(PAYLOAD_LEN);
                let path = unsafe { core::str::from_utf8_unchecked(&msg.payload[..len]) };
                let r = match tmp_normalize(path, &mut canon) {
                    Some(n) if n > 1 && tmp_find(&canon[..n]).is_none() => {
                        let mut parent = [0u8; TMP_PATH_MAX];
                        let plen = tmp_parent(&canon[..n], &mut parent);
                        match tmp_find(&parent[..plen]) {
                            Some(pidx)
                                if tmp_node_at(pidx).is_dir
                                    && tmp_alloc_node(&canon[..n], true).is_some() =>
                            {
                                1
                            }
                            _ => u64::MAX,
                        }
                    }
                    _ => u64::MAX,
                };
                sys_reply(r);
            }
            vfs::VFS_UNLINK_TAG => {
                let len = msg
                    .payload
                    .iter()
                    .position(|&b| b == 0)
                    .unwrap_or(PAYLOAD_LEN);
                let path = unsafe { core::str::from_utf8_unchecked(&msg.payload[..len]) };
                let r = match tmp_normalize(path, &mut canon) {
                    Some(n) => match tmp_find(&canon[..n]) {
                        Some(idx) if !tmp_node_at(idx).is_dir => {
                            tmp_node_at_mut(idx).used = false;
                            1
                        }
                        _ => u64::MAX,
                    },
                    None => u64::MAX,
                };
                sys_reply(r);
            }
            vfs::VFS_RMDIR_TAG => {
                let len = msg
                    .payload
                    .iter()
                    .position(|&b| b == 0)
                    .unwrap_or(PAYLOAD_LEN);
                let path = unsafe { core::str::from_utf8_unchecked(&msg.payload[..len]) };
                let r = match tmp_normalize(path, &mut canon) {
                    Some(n) if n > 1 => match tmp_find(&canon[..n]) {
                        Some(idx) if tmp_node_at(idx).is_dir && tmp_dir_empty(&canon[..n]) => {
                            tmp_node_at_mut(idx).used = false;
                            1
                        }
                        _ => u64::MAX,
                    },
                    _ => u64::MAX,
                };
                sys_reply(r);
            }
            vfs::VFS_STAT_TAG => {
                let (buf, path) = parse_path_req(msg.payload.as_ptr());
                let n = match tmp_normalize(path, &mut canon) {
                    Some(n) => match tmp_find(&canon[..n]) {
                        Some(idx) => {
                            let nd = tmp_node_at(idx);
                            let st = vfs::Stat::plain(nd.size as u64, u32::from(nd.is_dir));
                            unsafe {
                                core::ptr::write_unaligned(buf as *mut vfs::Stat, st);
                            }
                            core::mem::size_of::<vfs::Stat>() as u64
                        }
                        None => u64::MAX,
                    },
                    None => u64::MAX,
                };
                sys_reply(n);
            }
            _ => {
                sys_reply(u64::MAX);
            }
        }
    }
}
