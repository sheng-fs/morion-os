//! 包管理 MVP (E4): `.mpk` 归档 + Nix 式 store + 代际切换。
//!
//! 设计对应 `docs/architecture.md` 的「包管理」一节:
//!
//! - **store**: `/nix/store/<哈希>-<包名>-<版本>/` 是不可变目录。载体是自研 MFS,
//!   故物理路径是 `/mfs/nix/store/...` (用户可见输出里把 `/mfs` 前缀去掉, 仍叫
//!   `/nix/store/...`)。
//! - **`.mpk`**: 自定义归档 = 条目表 (路径 + 内容) + 末尾 manifest。魔数 `MPK1`,
//!   纯 no_std, 解析时对所有长度做越界校验。
//! - **哈希**: 对「文件清单 (相对路径 + 长度) + 内容」做 FNV-1a 64, 输出 16 位十六进制,
//!   直接构成 store 目录名前缀。
//! - **代际**: `/nix/var/nix/profiles/default` 每行一个 store 路径, 是当前视图;
//!   历史代际保留在 `default-<N>`。切换 = 写临时文件 + `rename` 原子替换视图。
//! - **GC**: 删除不被任何一代引用的 store 路径 (按引用图的可达性判断)。
//!
//! 这是最小闭环: 不做签名、远程仓库、依赖解析与沙盒构建。

use crate::common::print_sanitized;
use morion::syscall::{print, print_u64, println};
use morion::vfs;

// ===========================================================================
// 常量
// ===========================================================================

/// `.mpk` 魔数。
const MPK_MAGIC: [u8; 4] = *b"MPK1";
/// 归档头: 魔数 (4) + 条目数 (u32) + manifest 长度 (u32)。
const MPK_HDR: usize = 12;
/// 条目头: 路径长度 (u16) + 内容长度 (u32)。
const ENTRY_HDR: usize = 6;
/// 单个归档最多条目数 (固定缓冲, 不分配堆)。
const MAX_FILES: usize = 64;
/// 归档内相对路径上限 (含 NUL 前)。
const REL_MAX: usize = 64;
/// store 内完整路径上限 (vfs 的 IPC payload 只到 95 字节)。
const PATH_MAX: usize = 96;
/// 包名 / 版本上限。
const NAME_MAX: usize = 32;
const VER_MAX: usize = 32;
/// manifest 文本上限。
const MANIFEST_MAX: usize = 8192;
/// 代际视图文本上限。
const GENBUF_MAX: usize = 4096;
/// 一次 build 最多遍历的目录数。
const MAX_DIRS: usize = 32;
/// 最多记录的历史代际数。
const MAX_GENS: usize = 64;
/// 单次 IPC 读写的最大字节数 (一页)。
const CHUNK: usize = 4096;

/// store 物理根 (MFS 挂载点之下)。
const STORE_ROOT: &str = "/mfs/nix/store";
/// 代际目录。
const PROFILE_DIR: &str = "/mfs/nix/var/nix/profiles";
/// 当前代际序号文件。
const PROFILE_CUR: &str = "/mfs/nix/var/nix/profiles/current";
/// 当前代际视图 (每行一个 store 路径)。
const PROFILE_VIEW: &str = "/mfs/nix/var/nix/profiles/default";
/// 安装时随包写入的 manifest 文件名。
const MANIFEST_FILE: &str = "mpk.manifest";

/// 打包时记录的一个文件。
#[derive(Clone, Copy)]
struct Entry {
    /// 源文件完整路径。
    full: [u8; PATH_MAX],
    full_len: u8,
    /// 归档内相对路径。
    rel: [u8; REL_MAX],
    rel_len: u8,
    /// 内容长度。
    size: u32,
    /// 内容 + 清单的 FNV-1a 64。
    hash: u64,
}

const ENTRY_EMPTY: Entry = Entry {
    full: [0; PATH_MAX],
    full_len: 0,
    rel: [0; REL_MAX],
    rel_len: 0,
    size: 0,
    hash: 0,
};

static mut ENTRIES: [Entry; MAX_FILES] = [ENTRY_EMPTY; MAX_FILES];
static mut DIRQ: [[u8; REL_MAX]; MAX_DIRS] = [[0; REL_MAX]; MAX_DIRS];
static mut DIRQ_LENS: [u8; MAX_DIRS] = [0; MAX_DIRS];
static mut MANIFEST: [u8; MANIFEST_MAX] = [0; MANIFEST_MAX];
static mut GEN_OUT: [u8; GENBUF_MAX] = [0; GENBUF_MAX];
static mut GENS: [u32; MAX_GENS] = [0; MAX_GENS];
static mut GC_NAMES: [[u8; 64]; 24] = [[0; 64]; 24];
static mut GC_LENS: [u8; 24] = [0; 24];
static mut GC_IS_DIR: [u8; 24] = [0; 24];

/// 一个包的元数据 (来自 manifest / 目录名)。
struct PkgMeta {
    hash: u64,
    files: u32,
    name: [u8; NAME_MAX],
    name_len: u8,
    ver: [u8; VER_MAX],
    ver_len: u8,
}

/// 安装结果: store 路径 + 元数据。
struct StoreRef {
    store: [u8; PATH_MAX + 32],
    store_len: u8,
    meta: PkgMeta,
}

// ===========================================================================
// 小工具
// ===========================================================================

fn as_str(b: &[u8]) -> &str {
    unsafe { core::str::from_utf8_unchecked(b) }
}

fn entries_mut() -> &'static mut [Entry] {
    unsafe { &mut *core::ptr::addr_of_mut!(ENTRIES) }
}

fn dirq_mut() -> &'static mut [[u8; REL_MAX]; MAX_DIRS] {
    unsafe { &mut *core::ptr::addr_of_mut!(DIRQ) }
}

fn dirq_lens_mut() -> &'static mut [u8; MAX_DIRS] {
    unsafe { &mut *core::ptr::addr_of_mut!(DIRQ_LENS) }
}

fn manifest_mut() -> &'static mut [u8; MANIFEST_MAX] {
    unsafe { &mut *core::ptr::addr_of_mut!(MANIFEST) }
}

fn gen_out_mut() -> &'static mut [u8; GENBUF_MAX] {
    unsafe { &mut *core::ptr::addr_of_mut!(GEN_OUT) }
}

fn gens_mut() -> &'static mut [u32; MAX_GENS] {
    unsafe { &mut *core::ptr::addr_of_mut!(GENS) }
}

/// 逐字节打印 UTF-8 (路径只可能是 ASCII)。
fn print_bytes(b: &[u8]) {
    print(as_str(b));
}

/// 把物理路径 `/mfs/nix/store/...` 显示成设计文档里的 `/nix/store/...`。
fn print_logical(path: &str) {
    match path.strip_prefix("/mfs") {
        Some(rest) => print(rest),
        None => print(path),
    }
}

fn push_bytes(out: &mut [u8], n: &mut usize, data: &[u8]) -> Option<()> {
    if *n + data.len() > out.len() {
        return None;
    }
    out[*n..*n + data.len()].copy_from_slice(data);
    *n += data.len();
    Some(())
}

fn push_str(out: &mut [u8], n: &mut usize, s: &str) -> Option<()> {
    push_bytes(out, n, s.as_bytes())
}

fn get_u16(b: &[u8]) -> u16 {
    u16::from_le_bytes([b[0], b[1]])
}

fn get_u32(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

const HEX: &[u8; 16] = b"0123456789abcdef";

fn hex16(v: u64, out: &mut [u8; 16]) {
    for (i, slot) in out.iter_mut().enumerate() {
        *slot = HEX[((v >> (60 - i * 4)) & 0xF) as usize];
    }
}

fn parse_hex16(b: &[u8]) -> Option<u64> {
    if b.len() != 16 {
        return None;
    }
    let mut v = 0u64;
    for &c in b {
        let d = match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'f' => c - b'a' + 10,
            b'A'..=b'F' => c - b'A' + 10,
            _ => return None,
        };
        v = (v << 4) | d as u64;
    }
    Some(v)
}

fn parse_dec(b: &[u8]) -> Option<u32> {
    if b.is_empty() {
        return None;
    }
    let mut v = 0u32;
    for &c in b {
        if !c.is_ascii_digit() {
            return None;
        }
        v = v.checked_mul(10)?.checked_add((c - b'0') as u32)?;
    }
    Some(v)
}

fn write_dec(mut v: u32, out: &mut [u8]) -> usize {
    let mut tmp = [0u8; 10];
    let mut n = 0usize;
    if v == 0 {
        out[0] = b'0';
        return 1;
    }
    while v > 0 {
        tmp[n] = b'0' + (v % 10) as u8;
        v /= 10;
        n += 1;
    }
    let mut i = 0usize;
    while i < n {
        out[i] = tmp[n - 1 - i];
        i += 1;
    }
    n
}

/// 拼接 `a + "/" + b` (a 以 '/' 结尾时不重复加)。
fn join2(a: &str, b: &[u8], out: &mut [u8]) -> Option<usize> {
    let mut n = 0usize;
    push_str(out, &mut n, a)?;
    if !a.ends_with('/') {
        push_bytes(out, &mut n, b"/")?;
    }
    push_bytes(out, &mut n, b)?;
    Some(n)
}

/// 拼接归档内相对路径 `rel + "/" + name` (rel 为空则只有 name)。
fn join_rel(rel: &[u8], name: &[u8], out: &mut [u8]) -> Option<usize> {
    let mut n = 0usize;
    if !rel.is_empty() {
        push_bytes(out, &mut n, rel)?;
        push_bytes(out, &mut n, b"/")?;
    }
    push_bytes(out, &mut n, name)?;
    Some(n)
}

/// 校验归档内相对路径: 非空、无 NUL、无空段 / `.` / `..`、不以 '/' 开头。
fn valid_rel(p: &[u8]) -> bool {
    if p.is_empty() || p[0] == b'/' || p.len() > REL_MAX || p.contains(&0) {
        return false;
    }
    for seg in p.split(|&b| b == b'/') {
        if seg.is_empty() || seg == b"." || seg == b".." {
            return false;
        }
    }
    true
}

/// 校验包名 / 版本: 非空、长度受限、只允许安全字符。
fn valid_component(c: &[u8], max: usize) -> bool {
    if c.is_empty() || c.len() > max {
        return false;
    }
    c.iter()
        .all(|&b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'+' || b == b'-')
}

// ===========================================================================
// FNV-1a 64
// ===========================================================================

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

fn fnv_update(h: &mut u64, data: &[u8]) {
    for &b in data {
        *h ^= b as u64;
        *h = h.wrapping_mul(FNV_PRIME);
    }
}

// ===========================================================================
// vfs 小封装 (读写按一页分块; 结果/写缓冲用 shell 专用页)
// ===========================================================================

fn is_dir(path: &str) -> bool {
    let want = core::mem::size_of::<vfs::Stat>() as u64;
    if vfs::stat_into(path, vfs::SHELL_RESULT_BUF) != want {
        return false;
    }
    let st = unsafe { core::ptr::read_unaligned(vfs::SHELL_RESULT_BUF as *const vfs::Stat) };
    st.is_dir != 0
}

fn exists(path: &str) -> bool {
    vfs::stat_into(path, vfs::SHELL_RESULT_BUF) == core::mem::size_of::<vfs::Stat>() as u64
}

/// 递归创建目录 (已存在视为成功)。
fn ensure_dir(path: &str) -> bool {
    if is_dir(path) {
        return true;
    }
    if path.len() > 90 {
        return false;
    }
    if let Some(i) = path.rfind('/') {
        if i > 0 && !is_dir(&path[..i]) && !ensure_dir(&path[..i]) {
            return false;
        }
    }
    let _ = vfs::mkdir(path);
    is_dir(path)
}

fn ensure_parent(path: &str) -> bool {
    match path.rfind('/') {
        Some(i) if i > 0 => ensure_dir(&path[..i]),
        Some(_) => true,
        None => false,
    }
}

/// 从 fd 的 `off` 起精确读 `dst.len()` 字节。
fn read_exact_at(fd: u64, off: u64, dst: &mut [u8]) -> bool {
    let mut done = 0usize;
    while done < dst.len() {
        let want = (dst.len() - done).min(CHUNK);
        let n = vfs::read_into(fd, off + done as u64, want as u32, vfs::SHELL_RESULT_BUF);
        if n == u64::MAX || n == 0 || n as usize > want {
            return false;
        }
        let src =
            unsafe { core::slice::from_raw_parts(vfs::SHELL_RESULT_BUF as *const u8, n as usize) };
        dst[done..done + n as usize].copy_from_slice(src);
        done += n as usize;
    }
    true
}

/// 从共享页把数据写到 fd 的 `*off`, 并推进偏移。
fn write_all(fd: u64, off: &mut u64, data: &[u8]) -> bool {
    let mut done = 0usize;
    while done < data.len() {
        let want = (data.len() - done).min(CHUNK);
        let n = vfs::write_into(fd, *off, &data[done..done + want], vfs::SHELL_WRITE_BUF);
        if n != want as u64 {
            return false;
        }
        done += want;
        *off += want as u64;
    }
    true
}

/// 把 src 的 `size` 字节流式拷到 dst 的 `*off` (不经过中间缓冲)。
fn copy_stream(src: u64, size: u32, dst: u64, off: &mut u64) -> bool {
    let mut done = 0u64;
    let total = size as u64;
    while done < total {
        let want = ((total - done) as usize).min(CHUNK);
        let n = vfs::read_into(src, done, want as u32, vfs::SHELL_RESULT_BUF);
        if n == u64::MAX || n == 0 || n as usize > want {
            return false;
        }
        let data =
            unsafe { core::slice::from_raw_parts(vfs::SHELL_RESULT_BUF as *const u8, n as usize) };
        if !write_all(dst, off, data) {
            return false;
        }
        done += n;
    }
    true
}

/// 读整个文件 (小文件): 超过 `buf` 或失败返回 None。
fn read_file(path: &str, buf: &mut [u8]) -> Option<usize> {
    let fd = vfs::open(path);
    if fd == u64::MAX {
        return None;
    }
    let want = core::mem::size_of::<vfs::Stat>() as u64;
    let got = vfs::stat_into(path, vfs::SHELL_RESULT_BUF);
    if got != want {
        vfs::close(fd);
        return None;
    }
    let st = unsafe { core::ptr::read_unaligned(vfs::SHELL_RESULT_BUF as *const vfs::Stat) };
    let size = st.size as usize;
    if st.is_dir != 0 || size > buf.len() {
        vfs::close(fd);
        return None;
    }
    let ok = read_exact_at(fd, 0, &mut buf[..size]);
    vfs::close(fd);
    if ok {
        Some(size)
    } else {
        None
    }
}

/// 写整个文件 (存在则截断); 失败时删掉半成品。
fn write_file(path: &str, data: &[u8]) -> bool {
    if path.len() > 90 {
        return false;
    }
    let fd = vfs::creat(path);
    if fd == u64::MAX {
        return false;
    }
    let mut off = 0u64;
    let ok = vfs::truncate(fd, 0) == 1 && write_all(fd, &mut off, data);
    vfs::close(fd);
    if !ok {
        let _ = vfs::unlink(path);
    }
    ok
}

/// 原子写: 先写 `<path>.tmp`, 再 rename 覆盖 `<path>`。
fn write_file_atomic(path: &str, data: &[u8]) -> bool {
    let mut tmp = [0u8; 128];
    let mut n = 0usize;
    if push_str(&mut tmp, &mut n, path).is_none() || push_str(&mut tmp, &mut n, ".tmp").is_none() {
        return false;
    }
    let tmp_s = as_str(&tmp[..n]);
    if !write_file(tmp_s, data) {
        return false;
    }
    if vfs::rename_into(tmp_s, path, vfs::SHELL_RESULT_BUF) != 1 {
        let _ = vfs::unlink(tmp_s);
        return false;
    }
    true
}

/// 递归删除目录树 (只取每个目录的第一条, 删掉后重复; 不占额外栈)。
fn rm_tree(path: &str) -> bool {
    if path.len() > 90 {
        return false;
    }
    if !exists(path) {
        return true;
    }
    if !is_dir(path) {
        return vfs::unlink(path) == 1;
    }
    loop {
        let fd = vfs::open(path);
        if fd == u64::MAX {
            return false;
        }
        let n = vfs::readdir_into(fd, vfs::SHELL_RESULT_BUF);
        vfs::close(fd);
        if n == u64::MAX {
            return false;
        }
        if n == 0 {
            break;
        }
        let de =
            unsafe { core::ptr::read_unaligned(vfs::SHELL_RESULT_BUF as *const vfs::DirEntry) };
        let mut name = [0u8; 80];
        let nl = match de_name(&de, &mut name) {
            Some(x) if x > 0 => x,
            _ => return false,
        };
        let mut child = [0u8; PATH_MAX];
        let cl = match join2(path, &name[..nl], &mut child) {
            Some(x) if x <= 90 => x,
            _ => return false,
        };
        let child_s = as_str(&child[..cl]);
        if de.is_dir != 0 {
            if !rm_tree(child_s) {
                return false;
            }
        } else if vfs::unlink(child_s) != 1 {
            return false;
        }
    }
    vfs::rmdir(path) == 1
}

/// 取目录条目名字 (优先长名, 回退 8.3 短名)。
fn de_name(de: &vfs::DirEntry, out: &mut [u8]) -> Option<usize> {
    if de.long_len > 0 {
        let n = (de.long_len as usize).min(out.len());
        out[..n].copy_from_slice(&de.long[..n]);
        return Some(n);
    }
    let mut main = 0usize;
    while main < 8 && de.name[main] != b' ' {
        main += 1;
    }
    let mut ext = 0usize;
    while ext < 3 && de.name[8 + ext] != b' ' {
        ext += 1;
    }
    let mut n = 0usize;
    out[..main].copy_from_slice(&de.name[..main]);
    n += main;
    if ext > 0 {
        out[n] = b'.';
        n += 1;
        out[n..n + ext].copy_from_slice(&de.name[8..8 + ext]);
        n += ext;
    }
    Some(n)
}

/// 列出目录条目数 (不保留 fd); 失败返回 None。
fn dir_count(path: &str) -> Option<usize> {
    let fd = vfs::open(path);
    if fd == u64::MAX {
        return None;
    }
    let n = vfs::readdir_into(fd, vfs::SHELL_RESULT_BUF);
    vfs::close(fd);
    if n == u64::MAX {
        return None;
    }
    let size = core::mem::size_of::<vfs::DirEntry>();
    Some(n as usize / size)
}

// ===========================================================================
// `.mpk` 构建
// ===========================================================================

fn hash_content_at(fd: u64, start: u64, size: u32, h: &mut u64) -> Result<(), &'static str> {
    let mut done = 0u64;
    let total = size as u64;
    while done < total {
        let want = ((total - done) as usize).min(CHUNK);
        let n = vfs::read_into(fd, start + done, want as u32, vfs::SHELL_RESULT_BUF);
        if n == u64::MAX || n == 0 || n as usize > want {
            return Err("short read");
        }
        let data =
            unsafe { core::slice::from_raw_parts(vfs::SHELL_RESULT_BUF as *const u8, n as usize) };
        fnv_update(h, data);
        done += n;
    }
    Ok(())
}

/// 遍历 `base` 下所有普通文件, 填入静态 entries 表。
fn collect_files(base: &str) -> Result<usize, &'static str> {
    let entries = entries_mut();
    let q = dirq_mut();
    let ql = dirq_lens_mut();
    let mut nfiles = 0usize;
    let mut head = 0usize;
    let mut tail = 1usize;
    ql[0] = 0;

    while head < tail {
        let mut rel = [0u8; REL_MAX];
        let rl = ql[head] as usize;
        rel[..rl].copy_from_slice(&q[head][..rl]);
        head += 1;

        let mut dirpath = [0u8; PATH_MAX];
        let dl = join2(base, &rel[..rl], &mut dirpath).ok_or("path too long")?;
        if dl > 90 {
            return Err("path too long");
        }
        let count = dir_count(as_str(&dirpath[..dl])).ok_or("cannot read directory")?;
        if count >= vfs::RESULT_MAX_ENTRIES {
            return Err("directory too large for one readdir page");
        }
        let page = vfs::SHELL_RESULT_BUF as *const vfs::DirEntry;
        for i in 0..count {
            let de = unsafe { core::ptr::read_unaligned(page.add(i)) };
            let mut name = [0u8; REL_MAX];
            let nl = de_name(&de, &mut name).ok_or("bad entry name")?;
            if nl == 0 {
                continue;
            }
            if name[..nl].contains(&b'/') || name[..nl].contains(&0) {
                return Err("bad entry name");
            }
            let mut rel2 = [0u8; REL_MAX];
            let r2 = join_rel(&rel[..rl], &name[..nl], &mut rel2).ok_or("path too long")?;
            if de.is_dir != 0 {
                if tail >= MAX_DIRS {
                    return Err("too many directories");
                }
                ql[tail] = r2 as u8;
                q[tail][..r2].copy_from_slice(&rel2[..r2]);
                tail += 1;
            } else {
                if nfiles >= MAX_FILES {
                    return Err("too many files");
                }
                if de.size > u32::MAX as u64 {
                    return Err("file too large");
                }
                let mut full = [0u8; PATH_MAX];
                let fl = join2(base, &rel2[..r2], &mut full).ok_or("path too long")?;
                let e = &mut entries[nfiles];
                e.full[..fl].copy_from_slice(&full[..fl]);
                e.full_len = fl as u8;
                e.rel[..r2].copy_from_slice(&rel2[..r2]);
                e.rel_len = r2 as u8;
                e.size = de.size as u32;
                e.hash = 0;
                nfiles += 1;
            }
        }
    }
    Ok(nfiles)
}

/// 计算每个文件的稳定摘要: `rel_len | rel | size | content`。
fn hash_entries(list: &mut [Entry]) -> Result<(), &'static str> {
    for e in list.iter_mut() {
        let path = as_str(&e.full[..e.full_len as usize]);
        let fd = vfs::open(path);
        if fd == u64::MAX {
            return Err("cannot open file");
        }
        let mut h = FNV_OFFSET;
        fnv_update(&mut h, &(e.rel_len as u32).to_le_bytes());
        fnv_update(&mut h, &e.rel[..e.rel_len as usize]);
        fnv_update(&mut h, &e.size.to_le_bytes());
        let r = hash_content_at(fd, 0, e.size, &mut h);
        vfs::close(fd);
        r?;
        e.hash = h;
    }
    Ok(())
}

/// 生成 manifest 文本。
fn build_manifest(name: &[u8], ver: &[u8], hash: u64, list: &[Entry]) -> Option<usize> {
    let buf = manifest_mut();
    let mut n = 0usize;
    let mut hx = [0u8; 16];
    hex16(hash, &mut hx);
    push_str(buf, &mut n, "MPK-MANIFEST 1\n")?;
    push_str(buf, &mut n, "name: ")?;
    push_bytes(buf, &mut n, name)?;
    push_bytes(buf, &mut n, b"\nversion: ")?;
    push_bytes(buf, &mut n, ver)?;
    push_bytes(buf, &mut n, b"\nhash: ")?;
    push_bytes(buf, &mut n, &hx)?;
    push_bytes(buf, &mut n, b"\nfiles: ")?;
    let mut num = [0u8; 10];
    let nn = write_dec(list.len() as u32, &mut num);
    push_bytes(buf, &mut n, &num[..nn])?;
    push_bytes(buf, &mut n, b"\n")?;
    for e in list {
        let mut eh = [0u8; 16];
        hex16(e.hash, &mut eh);
        push_str(buf, &mut n, "f ")?;
        push_bytes(buf, &mut n, &eh)?;
        push_str(buf, &mut n, " ")?;
        let sn = write_dec(e.size, &mut num);
        push_bytes(buf, &mut n, &num[..sn])?;
        push_str(buf, &mut n, " ")?;
        push_bytes(buf, &mut n, &e.rel[..e.rel_len as usize])?;
        push_str(buf, &mut n, "\n")?;
    }
    Some(n)
}

/// 把 entries + manifest 写成 `.mpk`。
fn write_archive(out: &str, list: &[Entry], manifest: &[u8]) -> Result<(), &'static str> {
    let fd = vfs::creat(out);
    if fd == u64::MAX {
        return Err("cannot create output");
    }
    if vfs::truncate(fd, 0) != 1 {
        vfs::close(fd);
        return Err("cannot truncate output");
    }
    let mut off = 0u64;
    let mut hdr = [0u8; MPK_HDR];
    hdr[..4].copy_from_slice(&MPK_MAGIC);
    hdr[4..8].copy_from_slice(&(list.len() as u32).to_le_bytes());
    hdr[8..12].copy_from_slice(&(manifest.len() as u32).to_le_bytes());
    let mut ok = write_all(fd, &mut off, &hdr);
    let mut i = 0usize;
    while ok && i < list.len() {
        let e = &list[i];
        let mut eh = [0u8; ENTRY_HDR];
        eh[..2].copy_from_slice(&(e.rel_len as u16).to_le_bytes());
        eh[2..6].copy_from_slice(&e.size.to_le_bytes());
        ok = write_all(fd, &mut off, &eh) && write_all(fd, &mut off, &e.rel[..e.rel_len as usize]);
        if ok {
            let src = vfs::open(as_str(&e.full[..e.full_len as usize]));
            if src == u64::MAX {
                ok = false;
            } else {
                ok = copy_stream(src, e.size, fd, &mut off);
                vfs::close(src);
            }
        }
        i += 1;
    }
    if ok {
        ok = write_all(fd, &mut off, manifest);
    }
    vfs::close(fd);
    if !ok {
        let _ = vfs::unlink(out);
        return Err("write failed");
    }
    Ok(())
}

/// `pkg build <dir> <out.mpk>` 的实现。
fn build_archive(dir: &str, out: &str) -> Result<PkgMeta, &'static str> {
    if dir.len() > 90 || out.len() > 90 {
        return Err("path too long (max 90 bytes)");
    }
    let base = dir.trim_end_matches('/');
    if base.is_empty() {
        return Err("bad directory");
    }
    let base_name = match base.rfind('/') {
        Some(i) => &base[i + 1..],
        None => base,
    };
    let (name, ver) = match base_name.rfind('-') {
        Some(i) if i > 0 && i + 1 < base_name.len() => (&base_name[..i], &base_name[i + 1..]),
        _ => (base_name, "0"),
    };
    if !valid_component(name.as_bytes(), NAME_MAX) || !valid_component(ver.as_bytes(), VER_MAX) {
        return Err("bad package name/version (use <name>-<version> as directory name)");
    }

    let n = collect_files(base)?;
    let list = &mut entries_mut()[..n];
    list.sort_unstable_by(|a, b| a.rel[..a.rel_len as usize].cmp(&b.rel[..b.rel_len as usize]));
    hash_entries(list)?;

    let mut h = FNV_OFFSET;
    fnv_update(&mut h, &(n as u32).to_le_bytes());
    for e in list.iter() {
        fnv_update(&mut h, &e.hash.to_le_bytes());
    }

    let mlen =
        build_manifest(name.as_bytes(), ver.as_bytes(), h, list).ok_or("manifest too large")?;
    let manifest = manifest_mut();
    write_archive(out, list, &manifest[..mlen])?;

    let mut meta = PkgMeta {
        hash: h,
        files: n as u32,
        name: [0; NAME_MAX],
        name_len: name.len() as u8,
        ver: [0; VER_MAX],
        ver_len: ver.len() as u8,
    };
    meta.name[..name.len()].copy_from_slice(name.as_bytes());
    meta.ver[..ver.len()].copy_from_slice(ver.as_bytes());
    Ok(meta)
}

// ===========================================================================
// `.mpk` 解析 / 安装
// ===========================================================================

/// 在 manifest 文本里找 `key: ` 开头的行, 返回冒号后的字节。
fn manifest_field<'a>(buf: &'a [u8], key: &[u8]) -> Option<&'a [u8]> {
    for line in buf.split(|&b| b == b'\n') {
        if line.len() > key.len() && line.starts_with(key) && line[key.len()] == b':' {
            let v = &line[key.len() + 1..];
            return Some(v.strip_prefix(b" ").unwrap_or(v));
        }
    }
    None
}

fn parse_manifest(buf: &[u8], count: u32) -> Result<PkgMeta, &'static str> {
    let name = manifest_field(buf, b"name").ok_or("manifest: missing name")?;
    let ver = manifest_field(buf, b"version").ok_or("manifest: missing version")?;
    let hash_s = manifest_field(buf, b"hash").ok_or("manifest: missing hash")?;
    let files_s = manifest_field(buf, b"files").ok_or("manifest: missing files")?;
    if !valid_component(name, NAME_MAX) || !valid_component(ver, VER_MAX) {
        return Err("manifest: bad name/version");
    }
    let hash = parse_hex16(hash_s).ok_or("manifest: bad hash")?;
    let files = parse_dec(files_s).ok_or("manifest: bad file count")?;
    if files != count {
        return Err("manifest: file count mismatch");
    }
    let mut meta = PkgMeta {
        hash,
        files,
        name: [0; NAME_MAX],
        name_len: name.len() as u8,
        ver: [0; VER_MAX],
        ver_len: ver.len() as u8,
    };
    meta.name[..name.len()].copy_from_slice(name);
    meta.ver[..ver.len()].copy_from_slice(ver);
    Ok(meta)
}

/// 校验归档结构 + 摘要, 并把 manifest 读进静态缓冲。返回 (meta, manifest 长度)。
fn verify_archive(fd: u64, file_size: u64) -> Result<(PkgMeta, usize), &'static str> {
    if file_size < MPK_HDR as u64 {
        return Err("file too small for MPK1");
    }
    let mut hdr = [0u8; MPK_HDR];
    if !read_exact_at(fd, 0, &mut hdr) {
        return Err("read header failed");
    }
    if hdr[..4] != MPK_MAGIC {
        return Err("bad magic (want MPK1)");
    }
    let count = get_u32(&hdr[4..8]) as usize;
    let mlen = get_u32(&hdr[8..12]) as usize;
    if count > MAX_FILES {
        return Err("too many entries");
    }
    if mlen == 0 || mlen > MANIFEST_MAX {
        return Err("bad manifest length");
    }

    let mut off = MPK_HDR as u64;
    let mut per = [0u64; MAX_FILES];
    for slot in per.iter_mut().take(count) {
        if off + ENTRY_HDR as u64 > file_size {
            return Err("truncated entry header");
        }
        let mut eh = [0u8; ENTRY_HDR];
        if !read_exact_at(fd, off, &mut eh) {
            return Err("read entry header failed");
        }
        off += ENTRY_HDR as u64;
        let plen = get_u16(&eh[..2]) as usize;
        let clen = get_u32(&eh[2..6]);
        if plen == 0 || plen > REL_MAX {
            return Err("bad entry path length");
        }
        let mut path = [0u8; REL_MAX];
        if !read_exact_at(fd, off, &mut path[..plen]) {
            return Err("read entry path failed");
        }
        off += plen as u64;
        if !valid_rel(&path[..plen]) {
            return Err("unsafe entry path");
        }
        let end = off + clen as u64;
        if end > file_size {
            return Err("entry content out of bounds");
        }
        // 与构建端同一套算法: 每个文件一个摘要, 再对摘要列表求总摘要。
        let mut h = FNV_OFFSET;
        fnv_update(&mut h, &(plen as u32).to_le_bytes());
        fnv_update(&mut h, &path[..plen]);
        fnv_update(&mut h, &clen.to_le_bytes());
        hash_content_at(fd, off, clen, &mut h)?;
        *slot = h;
        off = end;
    }
    let mut h = FNV_OFFSET;
    fnv_update(&mut h, &(count as u32).to_le_bytes());
    for &ph in per.iter().take(count) {
        fnv_update(&mut h, &ph.to_le_bytes());
    }

    if off + mlen as u64 != file_size {
        return Err("archive size / manifest offset mismatch");
    }
    let manifest = manifest_mut();
    if !read_exact_at(fd, off, &mut manifest[..mlen]) {
        return Err("read manifest failed");
    }
    let meta = parse_manifest(&manifest[..mlen], count as u32)?;
    if meta.hash != h {
        return Err("content hash mismatch");
    }
    Ok((meta, mlen))
}

/// 把归档内容展开到 store 目录 (第二遍解析)。
fn unpack_archive(fd: u64, file_size: u64, store: &str, mlen: usize) -> Result<(), &'static str> {
    let mut hdr = [0u8; MPK_HDR];
    if !read_exact_at(fd, 0, &mut hdr) {
        return Err("read header failed");
    }
    let count = get_u32(&hdr[4..8]) as usize;
    let mut off = MPK_HDR as u64;

    for _ in 0..count {
        let mut eh = [0u8; ENTRY_HDR];
        if !read_exact_at(fd, off, &mut eh) {
            return Err("read entry header failed");
        }
        off += ENTRY_HDR as u64;
        let plen = get_u16(&eh[..2]) as usize;
        let clen = get_u32(&eh[2..6]);
        if plen == 0 || plen > REL_MAX {
            return Err("bad entry path length");
        }
        let mut rel = [0u8; REL_MAX];
        if !read_exact_at(fd, off, &mut rel[..plen]) {
            return Err("read entry path failed");
        }
        off += plen as u64;
        if !valid_rel(&rel[..plen]) || off + clen as u64 > file_size {
            return Err("bad entry");
        }

        let mut dest = [0u8; PATH_MAX + 32];
        let dl = join2(store, &rel[..plen], &mut dest).ok_or("store path too long")?;
        if dl > 90 {
            return Err("store path too long");
        }
        let dest_s = as_str(&dest[..dl]);
        if !ensure_parent(dest_s) {
            return Err("mkdir failed");
        }
        let dfd = vfs::creat(dest_s);
        if dfd == u64::MAX {
            return Err("create file failed");
        }
        if vfs::truncate(dfd, 0) != 1 {
            vfs::close(dfd);
            return Err("truncate file failed");
        }
        let mut doff = 0u64;
        if !copy_stream(fd, clen, dfd, &mut doff) {
            vfs::close(dfd);
            return Err("copy content failed");
        }
        vfs::close(dfd);
        off += clen as u64;
    }

    if off + mlen as u64 > file_size {
        return Err("manifest out of bounds");
    }
    let mut mpath = [0u8; PATH_MAX + 32];
    let ml = join2(store, MANIFEST_FILE.as_bytes(), &mut mpath).ok_or("manifest path too long")?;
    if ml > 90 {
        return Err("manifest path too long");
    }
    let manifest = manifest_mut();
    if !write_file(as_str(&mpath[..ml]), &manifest[..mlen]) {
        return Err("write manifest failed");
    }
    Ok(())
}

/// 生成 store 目录名 `<hash16>-<name>-<ver>`。
fn store_path(meta: &PkgMeta, out: &mut [u8]) -> Option<usize> {
    let mut n = 0usize;
    push_str(out, &mut n, STORE_ROOT)?;
    push_bytes(out, &mut n, b"/")?;
    let mut hx = [0u8; 16];
    hex16(meta.hash, &mut hx);
    push_bytes(out, &mut n, &hx)?;
    push_bytes(out, &mut n, b"-")?;
    push_bytes(out, &mut n, &meta.name[..meta.name_len as usize])?;
    push_bytes(out, &mut n, b"-")?;
    push_bytes(out, &mut n, &meta.ver[..meta.ver_len as usize])?;
    Some(n)
}

/// `pkg install <.mpk>` 的实现: 校验 → 展开 → 追加一代。
fn install_archive(src: &str) -> Result<StoreRef, &'static str> {
    if src.len() > 90 {
        return Err("path too long");
    }
    let fd = vfs::open(src);
    if fd == u64::MAX {
        return Err("cannot open .mpk");
    }
    let want = core::mem::size_of::<vfs::Stat>() as u64;
    if vfs::stat_into(src, vfs::SHELL_RESULT_BUF) != want {
        vfs::close(fd);
        return Err("cannot stat .mpk");
    }
    let st = unsafe { core::ptr::read_unaligned(vfs::SHELL_RESULT_BUF as *const vfs::Stat) };
    if st.is_dir != 0 {
        vfs::close(fd);
        return Err("not a file");
    }
    let file_size = st.size;

    let (meta, mlen) = match verify_archive(fd, file_size) {
        Ok(x) => x,
        Err(e) => {
            vfs::close(fd);
            return Err(e);
        }
    };

    let mut store = [0u8; PATH_MAX + 32];
    let sl = store_path(&meta, &mut store).ok_or("store path too long")?;
    if sl > 90 {
        vfs::close(fd);
        return Err("store path too long");
    }
    let store_s = as_str(&store[..sl]);

    if !is_dir(store_s) {
        if !ensure_dir(STORE_ROOT) || !ensure_dir(store_s) {
            vfs::close(fd);
            return Err("cannot create store directory");
        }
        if let Err(e) = unpack_archive(fd, file_size, store_s, mlen) {
            let _ = rm_tree(store_s);
            vfs::close(fd);
            return Err(e);
        }
    }
    vfs::close(fd);

    append_generation(store_s).map_err(|_| "cannot write generation")?;

    Ok(StoreRef {
        store,
        store_len: sl as u8,
        meta,
    })
}

// ===========================================================================
// 代际
// ===========================================================================

fn gen_path(n: u32, out: &mut [u8]) -> Option<usize> {
    let mut k = 0usize;
    push_str(out, &mut k, PROFILE_DIR)?;
    push_str(out, &mut k, "/default-")?;
    let mut num = [0u8; 10];
    let nl = write_dec(n, &mut num);
    push_bytes(out, &mut k, &num[..nl])?;
    Some(k)
}

fn current_gen() -> u32 {
    let mut b = [0u8; 16];
    match read_file(PROFILE_CUR, &mut b) {
        Some(n) => parse_dec(&b[..n]).unwrap_or(0),
        None => 0,
    }
}

fn read_view(buf: &mut [u8]) -> usize {
    read_file(PROFILE_VIEW, buf).unwrap_or(0)
}

/// 读当前视图; 文件存在但读不进来 (超限/IO 错误) 返回 Err, 避免静默丢包。
fn read_view_checked(buf: &mut [u8]) -> Result<usize, &'static str> {
    if !exists(PROFILE_VIEW) {
        return Ok(0);
    }
    read_file(PROFILE_VIEW, buf).ok_or("generation view too large")
}

fn write_generation(n: u32, data: &[u8]) -> bool {
    if !ensure_dir(PROFILE_DIR) {
        return false;
    }
    let mut p = [0u8; 128];
    let pl = match gen_path(n, &mut p) {
        Some(x) => x,
        None => return false,
    };
    if !write_file_atomic(as_str(&p[..pl]), data) {
        return false;
    }
    if !write_file_atomic(PROFILE_VIEW, data) {
        return false;
    }
    let mut num = [0u8; 10];
    let nl = write_dec(n, &mut num);
    write_file_atomic(PROFILE_CUR, &num[..nl])
}

fn has_line(buf: &[u8], line: &[u8]) -> bool {
    buf.split(|&b| b == b'\n')
        .any(|l| !l.is_empty() && l == line)
}

fn append_line(buf: &mut [u8], len: usize, line: &[u8]) -> Option<usize> {
    if has_line(&buf[..len], line) {
        return Some(len);
    }
    let need = line.len() + 1;
    if len + need > buf.len() {
        return None;
    }
    buf[len..len + line.len()].copy_from_slice(line);
    buf[len + line.len()] = b'\n';
    Some(len + need)
}

fn append_generation(store: &str) -> Result<u32, ()> {
    let mut buf = [0u8; GENBUF_MAX];
    let len = read_view_checked(&mut buf).map_err(|_| ())?;
    let len = append_line(&mut buf, len, store.as_bytes()).ok_or(())?;
    let n = current_gen() + 1;
    if !write_generation(n, &buf[..len]) {
        return Err(());
    }
    Ok(n)
}

/// 代际号 → 该代的视图文本 (读进 `buf`)。
fn read_gen(n: u32, buf: &mut [u8]) -> Option<usize> {
    let mut p = [0u8; 128];
    let pl = gen_path(n, &mut p)?;
    read_file(as_str(&p[..pl]), buf)
}

/// 扫描代际目录, 代际号升序写进 `gens_mut()`, 返回个数。
fn scan_generations() -> usize {
    let gens = gens_mut();
    let fd = vfs::open(PROFILE_DIR);
    if fd == u64::MAX {
        return 0;
    }
    let n = vfs::readdir_into(fd, vfs::SHELL_RESULT_BUF);
    vfs::close(fd);
    if n == u64::MAX {
        return 0;
    }
    let size = core::mem::size_of::<vfs::DirEntry>();
    let count = (n as usize / size).min(MAX_GENS);
    let page = vfs::SHELL_RESULT_BUF as *const vfs::DirEntry;
    let mut k = 0usize;
    for i in 0..count {
        let de = unsafe { core::ptr::read_unaligned(page.add(i)) };
        let mut name = [0u8; 64];
        let nl = match de_name(&de, &mut name) {
            Some(x) if x > 0 => x,
            _ => continue,
        };
        let s = as_str(&name[..nl]);
        if let Some(rest) = s.strip_prefix("default-") {
            if let Some(num) = parse_dec(rest.as_bytes()) {
                gens[k] = num;
                k += 1;
            }
        }
    }
    gens[..k].sort_unstable();
    k
}

fn switch_to(n: u32) -> bool {
    let mut buf = [0u8; GENBUF_MAX];
    match read_gen(n, &mut buf) {
        Some(len) => write_generation(n, &buf[..len]),
        None => false,
    }
}

// ===========================================================================
// 查询 / GC
// ===========================================================================

/// 从 store 路径 (每行一个) 解析出 (hash, name, version)。
fn split_store(line: &str) -> Option<(&str, &str, &str)> {
    let base = line.rsplit('/').next()?;
    if base.len() < 18 {
        return None;
    }
    let (hash, rest) = base.split_at(16);
    if !hash.bytes().all(|b| b.is_ascii_hexdigit()) || !rest.starts_with('-') {
        return None;
    }
    let rest = &rest[1..];
    match rest.rfind('-') {
        Some(i) if i > 0 && i + 1 < rest.len() => Some((hash, &rest[..i], &rest[i + 1..])),
        None if !rest.is_empty() => Some((hash, rest, "0")),
        _ => None,
    }
}

fn count_packages() -> usize {
    let mut buf = [0u8; GENBUF_MAX];
    let len = read_view(&mut buf);
    buf[..len]
        .split(|&b| b == b'\n')
        .filter(|l| split_store(as_str(l)).is_some())
        .count()
}

fn print_package_line(line: &[u8]) {
    let s = as_str(line);
    match split_store(s) {
        Some((hash, name, ver)) => {
            print("  ");
            print(name);
            print(" ");
            print(ver);
            print(" ");
            print(hash);
            print("  ");
            print_logical(s);
            println("");
        }
        None => {
            print("  ? ");
            println(s);
        }
    }
}

fn print_list() {
    let mut buf = [0u8; GENBUF_MAX];
    let len = match read_view_checked(&mut buf) {
        Ok(n) => n,
        Err(e) => {
            print("pkg list: failed: ");
            println(e);
            return;
        }
    };
    let mut count = 0usize;
    for line in buf[..len].split(|&b| b == b'\n') {
        if line.is_empty() {
            continue;
        }
        print_package_line(line);
        count += 1;
    }
    print("pkg list: ");
    print_u64(count as u64);
    print(" package(s), generation ");
    print_u64(current_gen() as u64);
    println("");
}

fn cmd_info(name: &str) {
    let mut buf = [0u8; GENBUF_MAX];
    let len = match read_view_checked(&mut buf) {
        Ok(n) => n,
        Err(e) => {
            print("pkg info: failed: ");
            println(e);
            return;
        }
    };
    let mut found = false;
    for line in buf[..len].split(|&b| b == b'\n') {
        let s = as_str(line);
        let Some((hash, pname, ver)) = split_store(s) else {
            continue;
        };
        if pname != name {
            continue;
        }
        found = true;
        print("pkg info: ");
        println(pname);
        print("  version: ");
        println(ver);
        print("  hash: ");
        println(hash);
        print("  store: ");
        print_logical(s);
        println("");
        print("  generation: ");
        print_u64(current_gen() as u64);
        println("");
        let mut mpath = [0u8; PATH_MAX + 32];
        if let Some(ml) = join2(s, MANIFEST_FILE.as_bytes(), &mut mpath) {
            if ml > 90 {
                println("  manifest: path too long");
            } else {
                let mbuf = manifest_mut();
                if let Some(mn) = read_file(as_str(&mpath[..ml]), mbuf) {
                    println("  manifest:");
                    print_sanitized(as_str(&mbuf[..mn]));
                    println("");
                }
            }
        }
        break;
    }
    if !found {
        print("pkg info: not installed: ");
        println(name);
    }
}

fn cmd_remove(name: &str) {
    if !valid_component(name.as_bytes(), NAME_MAX) {
        println("pkg remove: invalid package name");
        return;
    }
    let mut buf = [0u8; GENBUF_MAX];
    let len = match read_view_checked(&mut buf) {
        Ok(n) => n,
        Err(e) => {
            print("pkg remove: failed: ");
            println(e);
            return;
        }
    };
    let out = gen_out_mut();
    let mut on = 0usize;
    let mut removed = 0usize;
    for line in buf[..len].split(|&b| b == b'\n') {
        if line.is_empty() {
            continue;
        }
        let s = as_str(line);
        let same = matches!(split_store(s), Some((_, n, _)) if n == name);
        if same {
            removed += 1;
            continue;
        }
        if on + line.len() + 1 > out.len() {
            println("pkg remove: generation list too large");
            return;
        }
        out[on..on + line.len()].copy_from_slice(line);
        out[on + line.len()] = b'\n';
        on += line.len() + 1;
    }
    if removed == 0 {
        print("pkg remove: not installed: ");
        println(name);
        return;
    }
    let n = current_gen() + 1;
    if !write_generation(n, &out[..on]) {
        println("pkg remove: cannot write generation");
        return;
    }
    print("pkg remove: ");
    print(name);
    print(" -> generation ");
    print_u64(n as u64);
    println("");
}

fn cmd_generations() {
    let n = scan_generations();
    let cur = current_gen();
    if n == 0 {
        println("pkg generations: none (install a package first)");
        return;
    }
    let gens = gens_mut();
    for &g in gens.iter().take(n) {
        let mut buf = [0u8; GENBUF_MAX];
        let len = read_gen(g, &mut buf).unwrap_or(0);
        let pkgs = buf[..len]
            .split(|&b| b == b'\n')
            .filter(|l| split_store(as_str(l)).is_some())
            .count();
        print("  gen ");
        print_u64(g as u64);
        print(": ");
        print_u64(pkgs as u64);
        print(" package(s)");
        if g == cur {
            print("  <- current");
        }
        println("");
    }
    print("pkg generations: current = ");
    print_u64(cur as u64);
    println("");
}

fn cmd_switch(arg: &str) {
    let Some(n) = parse_dec(arg.as_bytes()) else {
        println("pkg switch: usage: pkg switch <N>");
        return;
    };
    if switch_to(n) {
        print("pkg switch: generation ");
        print_u64(n as u64);
        println("");
    } else {
        print("pkg switch: no such generation: ");
        print_u64(n as u64);
        println("");
    }
}

fn cmd_rollback() {
    let cur = current_gen();
    if cur <= 1 {
        println("pkg rollback: no previous generation");
        return;
    }
    let target = cur - 1;
    if switch_to(target) {
        print("pkg rollback: generation ");
        print_u64(cur as u64);
        print(" -> ");
        print_u64(target as u64);
        println("");
    } else {
        print("pkg rollback: cannot read generation ");
        print_u64(target as u64);
        println("");
    }
}

/// 该 store 路径是否被任一保留代际引用。
///
/// 读不到某个代际时按「仍被引用」处理 —— GC 宁可少删, 也不能误删活着的包。
fn referenced(gens: &[u32], path: &str) -> bool {
    let mut buf = [0u8; GENBUF_MAX];
    for &g in gens {
        match read_gen(g, &mut buf) {
            Some(len) => {
                if has_line(&buf[..len], path.as_bytes()) {
                    return true;
                }
            }
            None => return true,
        }
    }
    false
}

/// 删除不被任何一代引用的 store 路径; 返回 (删除数, 保留数)。
fn gc_collect(verbose: bool) -> Result<(u32, u32), &'static str> {
    let ngen = scan_generations();
    let gens = gens_mut();
    let gens: &[u32] = &gens[..ngen];

    // 先确认所有代际都读得进来, 否则引用图不完整, 宁可拒绝 GC。
    let mut tmp = [0u8; GENBUF_MAX];
    for &g in gens {
        if read_gen(g, &mut tmp).is_none() {
            return Err("generation too large / unreadable");
        }
    }

    let fd = vfs::open(STORE_ROOT);
    if fd == u64::MAX {
        return Ok((0, 0));
    }
    let n = vfs::readdir_into(fd, vfs::SHELL_RESULT_BUF);
    vfs::close(fd);
    if n == u64::MAX {
        return Err("cannot list store");
    }
    let size = core::mem::size_of::<vfs::DirEntry>();
    let count = (n as usize / size).min(24);
    let page = vfs::SHELL_RESULT_BUF as *const vfs::DirEntry;
    let names = unsafe { &mut *core::ptr::addr_of_mut!(GC_NAMES) };
    let lens = unsafe { &mut *core::ptr::addr_of_mut!(GC_LENS) };
    let dirs = unsafe { &mut *core::ptr::addr_of_mut!(GC_IS_DIR) };
    for i in 0..count {
        let de = unsafe { core::ptr::read_unaligned(page.add(i)) };
        let mut name = [0u8; 64];
        let nl = de_name(&de, &mut name).unwrap_or(0).min(64);
        names[i][..nl].copy_from_slice(&name[..nl]);
        lens[i] = nl as u8;
        dirs[i] = (de.is_dir != 0) as u8;
    }

    let mut removed = 0u32;
    let mut kept = 0u32;
    for i in 0..count {
        let nl = lens[i] as usize;
        if nl == 0 {
            continue;
        }
        let mut full = [0u8; PATH_MAX];
        let fl = match join2(STORE_ROOT, &names[i][..nl], &mut full) {
            Some(x) if x <= 90 => x,
            _ => continue,
        };
        let path = as_str(&full[..fl]);
        if referenced(gens, path) {
            kept += 1;
            continue;
        }
        let ok = if dirs[i] != 0 {
            rm_tree(path)
        } else {
            vfs::unlink(path) == 1
        };
        if ok {
            removed += 1;
            if verbose {
                print("  removed ");
                print_logical(path);
                println("");
            }
        }
    }
    Ok((removed, kept))
}

// ===========================================================================
// 自测
// ===========================================================================

const FIX_A_TXT: &[u8] = b"hello from morion pkg\n";
const FIX_A_BIN: &[u8] = b"MEX1 fake executable payload\n";
const FIX_B_TXT: &[u8] = b"second package\n";

fn selftest_inner() -> Result<(), &'static str> {
    let _ = rm_tree("/mfs/pkgtest");
    let a_dir = "/mfs/pkgtest/pkgtest-a-1.0";
    let b_dir = "/mfs/pkgtest/pkgtest-b-1.0";
    let a_mpk = "/mfs/pkgtest/a.mpk";
    let b_mpk = "/mfs/pkgtest/b.mpk";
    println("PKG1 stage 1: create fixtures under /mfs/pkgtest");
    if !ensure_dir(alloc_join(a_dir, "src")?)
        || !ensure_dir(alloc_join(a_dir, "bin")?)
        || !ensure_dir(alloc_join(b_dir, "share")?)
    {
        return Err("create fixture directories");
    }
    if !write_file(alloc_join(a_dir, "src/a.txt")?, FIX_A_TXT)
        || !write_file(alloc_join(a_dir, "bin/tool.mex")?, FIX_A_BIN)
        || !write_file(alloc_join(b_dir, "share/b.txt")?, FIX_B_TXT)
    {
        return Err("create fixture files");
    }

    let gen0 = current_gen();
    print("PKG1 stage 2: build -> ");
    println(a_mpk);
    let meta_a = build_archive(a_dir, a_mpk)?;
    let mut hx = [0u8; 16];
    hex16(meta_a.hash, &mut hx);
    print("PKG1 build: hash=");
    print(as_str(&hx));
    print(" files=");
    print_u64(meta_a.files as u64);
    println("");

    println("PKG1 stage 3: install a.mpk");
    let a = install_archive(a_mpk)?;
    print("PKG1 install: ");
    print_logical(as_str(&a.store[..a.store_len as usize]));
    println("");
    if current_gen() != gen0 + 1 {
        return Err("generation after install A");
    }
    println("PKG1 generation: new generation created by install");

    println("PKG1 stage 4: list current generation");
    print_list();
    if count_packages() == 0 {
        return Err("list shows no packages");
    }

    println("PKG1 stage 5: install second package (new generation)");
    build_archive(b_dir, b_mpk)?;
    let b = install_archive(b_mpk)?;
    if current_gen() != gen0 + 2 {
        return Err("generation after install B");
    }
    print("PKG1 generation: now ");
    print_u64(current_gen() as u64);
    println(" (two packages)");

    println("PKG1 stage 6: rollback to previous generation");
    if !switch_to(gen0 + 1) {
        return Err("rollback");
    }
    if current_gen() != gen0 + 1 {
        return Err("current generation after rollback");
    }
    print("PKG1 rollback: current generation = ");
    print_u64(current_gen() as u64);
    println("");
    if !is_dir(as_str(&a.store[..a.store_len as usize]))
        || !is_dir(as_str(&b.store[..b.store_len as usize]))
    {
        return Err("store paths missing after rollback");
    }
    println("PKG1 rollback: old generation and both store paths retained");

    println("PKG1 stage 7: gc removes unreferenced store path");
    let orphan = "/mfs/nix/store/ffffffffffffffff-orphan-0.0";
    if !ensure_dir(orphan) || !write_file("/mfs/nix/store/ffffffffffffffff-orphan-0.0/x", b"x") {
        return Err("create orphan store path");
    }
    let (removed, kept) = gc_collect(false)?;
    if is_dir(orphan) || !is_dir(as_str(&a.store[..a.store_len as usize])) {
        return Err("gc result");
    }
    print("PKG1 gc: removed ");
    print_u64(removed as u64);
    print(" orphan(s), kept ");
    print_u64(kept as u64);
    println(" referenced path(s)");

    println("PKG1 stage 8: clean fixtures");
    if !rm_tree("/mfs/pkgtest") {
        return Err("cleanup");
    }
    Ok(())
}

/// 拼接短路径 (只在自测里用; 失败返回静态错误)。
fn alloc_join(base: &str, rel: &str) -> Result<&'static str, &'static str> {
    static mut BUF: [u8; 128] = [0u8; 128];
    unsafe {
        let out = &mut *core::ptr::addr_of_mut!(BUF);
        let n = join2(base, rel.as_bytes(), out).ok_or("path too long")?;
        Ok(as_str(&out[..n]))
    }
}

/// `pkg selftest`。
pub fn selftest() {
    println("PKG selftest: start");
    match selftest_inner() {
        Ok(()) => println("PKG SELFTEST OK"),
        Err(stage) => {
            print("PKG SELFTEST FAILED at ");
            println(stage);
        }
    }
}

// ===========================================================================
// 命令入口
// ===========================================================================

fn usage() {
    println("usage: pkg <command> [args]");
    println("  build <dir> <out.mpk>  pack a <name>-<version> directory into an .mpk");
    println("  install <.mpk>         verify, unpack into the store, create a generation");
    println("  remove <name>          drop a package from the current view (new generation)");
    println("  list                   list packages in the current generation");
    println("  info <name>            show store path / hash / manifest");
    println("  generations            list kept generations");
    println("  rollback               switch to the previous generation");
    println("  switch <N>             switch to generation N");
    println("  gc                     delete store paths no generation references");
    println("  selftest               run the end-to-end self test");
    println("  (store is on MFS: /mfs/nix/store, shown as /nix/store)");
}

fn cmd_build(cwd: &str, arg: &str) {
    let Some(i) = arg.find(' ') else {
        println("pkg build: usage: pkg build <dir> <out.mpk>");
        return;
    };
    let (d, o) = (arg[..i].trim(), arg[i + 1..].trim());
    if d.is_empty() || o.is_empty() {
        println("pkg build: usage: pkg build <dir> <out.mpk>");
        return;
    }
    let mut dir = [0u8; 128];
    let dn;
    let mut out = [0u8; 128];
    let on;
    {
        let Some(p) = super::resolve_in_cwd(cwd, d) else {
            println("pkg build: path too long");
            return;
        };
        if p.len() >= dir.len() {
            println("pkg build: path too long");
            return;
        }
        dir[..p.len()].copy_from_slice(p.as_bytes());
        dn = p.len();
    }
    {
        let Some(p) = super::resolve_in_cwd(cwd, o) else {
            println("pkg build: path too long");
            return;
        };
        if p.len() >= out.len() {
            println("pkg build: path too long");
            return;
        }
        out[..p.len()].copy_from_slice(p.as_bytes());
        on = p.len();
    }
    let dir_s = as_str(&dir[..dn]);
    let out_s = as_str(&out[..on]);
    match build_archive(dir_s, out_s) {
        Ok(meta) => {
            let mut hx = [0u8; 16];
            hex16(meta.hash, &mut hx);
            print("pkg build: ");
            print(out_s);
            print("  hash=");
            print(as_str(&hx));
            print(" name=");
            print(as_str(&meta.name[..meta.name_len as usize]));
            print(" version=");
            print(as_str(&meta.ver[..meta.ver_len as usize]));
            print(" files=");
            print_u64(meta.files as u64);
            println("");
        }
        Err(e) => {
            print("pkg build: failed: ");
            println(e);
        }
    }
}

fn cmd_install(cwd: &str, arg: &str) {
    if arg.is_empty() || arg.contains(' ') {
        println("pkg install: usage: pkg install <.mpk>");
        return;
    }
    let Some(p) = super::resolve_in_cwd(cwd, arg) else {
        println("pkg install: path too long");
        return;
    };
    let mut path = [0u8; 128];
    if p.len() >= path.len() {
        println("pkg install: path too long");
        return;
    }
    path[..p.len()].copy_from_slice(p.as_bytes());
    let path_s = as_str(&path[..p.len()]);
    match install_archive(path_s) {
        Ok(r) => {
            let mut hx = [0u8; 16];
            hex16(r.meta.hash, &mut hx);
            print("pkg install: ");
            print_logical(as_str(&r.store[..r.store_len as usize]));
            print("  hash=");
            print(as_str(&hx));
            print(" files=");
            print_u64(r.meta.files as u64);
            println("");
        }
        Err(e) => {
            print("pkg install: failed: ");
            println(e);
        }
    }
}

fn cmd_gc() {
    match gc_collect(true) {
        Ok((removed, kept)) => {
            print("pkg gc: removed ");
            print_u64(removed as u64);
            print(" path(s), kept ");
            print_u64(kept as u64);
            println(" referenced path(s)");
        }
        Err(e) => {
            print("pkg gc: failed: ");
            println(e);
        }
    }
}

/// `pkg` 子命令分发。非法参数只打印用法, 不 panic。
pub fn run(cwd: &str, arg: &str) {
    let arg = arg.trim();
    let (sub, rest) = match arg.find(' ') {
        Some(i) => (&arg[..i], arg[i + 1..].trim()),
        None => (arg, ""),
    };
    match sub {
        "" => usage(),
        "build" => cmd_build(cwd, rest),
        "install" => cmd_install(cwd, rest),
        "remove" => {
            if rest.is_empty() {
                println("pkg remove: usage: pkg remove <name>");
            } else {
                cmd_remove(rest)
            }
        }
        "list" => print_list(),
        "info" => {
            if rest.is_empty() {
                println("pkg info: usage: pkg info <name>");
            } else {
                cmd_info(rest)
            }
        }
        "generations" => cmd_generations(),
        "rollback" => cmd_rollback(),
        "switch" => cmd_switch(rest),
        "gc" => cmd_gc(),
        "selftest" => selftest(),
        _ => {
            print("pkg: unknown command: ");
            println(sub);
            usage();
        }
    }
}
