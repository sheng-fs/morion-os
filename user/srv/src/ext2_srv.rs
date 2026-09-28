use crate::common::*;
use morion::syscall::*;
use morion::vfs;

// ===========================================================================
// 域 12 — ext2 只读文件服务 (ext2_srv)
// ===========================================================================
// 阶段 C3: 挂载既有 Linux ext2 分区的只读兼容层。镜像由宿主 `mke2fs` 预格式化,
// 服务**不写盘、也不自动格式化** —— 超级块无效即挂载失败 (与 MFS 的「首挂载自动
// 格式化」相反: ext2 的定位就是读别人已有的分区)。
//
// 只实现读取所需的最小 ext2 子集:
//   - 超级块 (@1024, magic 0xEF53) → 块大小 / 每组块数 / 每组 inode 数 / inode 大小;
//   - 块组描述符表 → 缓存每组 inode 表起始块, 由 inode 号定位 inode;
//   - inode `block[15]` 的直接 / 一级间接 / 二级间接块映射 (三级不实现);
//   - 目录项 (`inode/rec_len/name_len/file_type/name`) 顺序遍历。
//
// ext2 名字是大小写敏感的字节串; 为便于交互, 精确匹配失败后再做一次 ASCII
// 大小写不敏感回退 (精确命中优先)。

/// 卷号回退值: 2 对应 `build/ext2.img` (namespace 3)。
const EXT2_VOL_FALLBACK: u64 = 2;
/// ext2 服务实际使用的卷号, 启动时由 `vol_claim` 认领 (见 `ext2_main`)。
static mut EXT2_VOL: u64 = EXT2_VOL_FALLBACK;

/// ext2 超级块 magic (超级块内偏移 0x38)。
const EXT2_MAGIC: u16 = 0xEF53;
/// 根目录 inode 号 (ext2 规范固定为 2)。
const EXT2_ROOT_INO: u32 = 2;

/// `i_mode` 的文件类型位 (高 4 位)。
const EXT2_S_IFMT: u16 = 0xF000;
const EXT2_S_IFDIR: u16 = 0x4000;

/// 目录项 `file_type` 值。
const EXT2_FT_DIR: u8 = 2;

/// inode `block[]` 布局: 前 12 个直接块, 其后依次是一 / 二 / 三级间接块。
const EXT2_DIRECT_BLOCKS: u32 = 12;
const EXT2_IND_BLOCK: usize = 12;
const EXT2_DIND_BLOCK: usize = 13;

/// 块组数上限 —— 决定 inode 表起始块缓存的大小 (`EXT2_MAX_GROUPS` × 4 字节)。
///
/// 取 4096: 1 KiB 块 + 默认 8192 块/组 = 每组 8 MiB, 故可覆盖到约 32 GiB 的卷;
/// 真实 U 盘 / 大分区 (M1b) 的块组数远超早期测试镜像 (16 MiB 只需 2 组), 上限过小
/// 会让挂载直接失败。
const EXT2_MAX_GROUPS: usize = 4096;
/// 打开文件上限。
const EXT2_MAX_FD: usize = 16;

/// ext2 块缓冲虚拟地址 (紧跟 MFS 缓冲页, 均位于程序镜像之外)。
///
/// 这些页要以「同地址」共享给 block_srv 供其 DMA 写入, 故必须避开程序镜像:
/// 所有域加载同一份用户镜像, 若地址落在镜像内, block_srv 自身镜像会占住该地址,
/// 共享时触发 PageAlreadyMapped。已占用: fat32 `+0x10_0000..0x10_4000`、
/// app/shell `+0x10_4000..0x10_8000`、MFS `+0x10_8000..0x10_C000`。
const EXT2_BUF_A_VADDR: u64 = 0x0000_0080_0010_C000;
const EXT2_BUF_B_VADDR: u64 = 0x0000_0080_0010_D000;
const EXT2_BUF_C_VADDR: u64 = 0x0000_0080_0010_E000;
const EXT2_BUF_D_VADDR: u64 = 0x0000_0080_0010_F000;

fn ext2_a() -> *mut u8 {
    EXT2_BUF_A_VADDR as *mut u8
}
fn ext2_b() -> *mut u8 {
    EXT2_BUF_B_VADDR as *mut u8
}
fn ext2_c() -> *mut u8 {
    EXT2_BUF_C_VADDR as *mut u8
}
fn ext2_d() -> *mut u8 {
    EXT2_BUF_D_VADDR as *mut u8
}

// 挂载后固定的卷参数 (内存镜像)。
static mut EXT2_BLOCK_SIZE: u32 = 0;
static mut EXT2_INODES_PER_GROUP: u32 = 0;
static mut EXT2_INODE_SIZE: u32 = 0;
static mut EXT2_GROUP_COUNT: u32 = 0;
static mut EXT2_INODE_TABLE: [u32; EXT2_MAX_GROUPS] = [0; EXT2_MAX_GROUPS];

/// ext2 inode 的读取所需字段。
#[derive(Clone, Copy)]
struct Ext2Inode {
    mode: u16,
    size: u32,
    block: [u32; 15],
}

/// 打开文件描述符 (只读: 记住 inode 号即可, 不需要路径)。
#[derive(Clone, Copy)]
struct Ext2Fd {
    used: bool,
    is_dir: bool,
    ino: u32,
    /// 打开时绑定的卷号 (M1b 多卷挂载)。
    vol: u64,
}
const EXT2_FD_EMPTY: Ext2Fd = Ext2Fd {
    used: false,
    is_dir: false,
    ino: 0,
    vol: 0,
};
static mut EXT2_FDS: [Ext2Fd; EXT2_MAX_FD] = [EXT2_FD_EMPTY; EXT2_MAX_FD];

/// 本服务**当前请求**落在的卷号 (M1b 多卷挂载; 见 fat32_srv 的 `FAT_CUR_VOL` 注释)。
static mut EXT2_CUR_VOL: u64 = 0;

/// 已解析的几何 (超级块 / 块组描述符) 属于哪个卷。各卷的块大小 / inode 表位置不同,
/// 请求落到别的卷上必须重新解析 (见 `ext2_mount`)。
static mut EXT2_GEO_VOL: u64 = u64::MAX;

/// 读一个 ext2 块 (块号 → LBA = 块号 × 每块扇区数)。
fn ext2_read_block(block_no: u32, dst: *mut u8) -> bool {
    let sectors = (unsafe { EXT2_BLOCK_SIZE } / 512) as u16;
    if sectors == 0 {
        return false;
    }
    block_read_dev(
        unsafe { EXT2_CUR_VOL },
        block_no * sectors as u32,
        sectors,
        dst,
    )
}

/// 由 inode 号读 inode: inode 表块读入 `buf`, 需要的字段拷进返回值。
///
/// inode 尺寸 (128 / 256) 整除块大小, 故 inode 不会跨块。
fn ext2_read_inode_buf(ino: u32, buf: *mut u8) -> Option<Ext2Inode> {
    if ino == 0 {
        return None;
    }
    let per_group = unsafe { EXT2_INODES_PER_GROUP };
    let inode_size = unsafe { EXT2_INODE_SIZE };
    let block_size = unsafe { EXT2_BLOCK_SIZE };
    let idx = ino - 1;
    let group = idx / per_group;
    if group >= unsafe { EXT2_GROUP_COUNT } {
        return None;
    }
    let in_table = unsafe { EXT2_INODE_TABLE[group as usize] };
    let byte_off = (idx % per_group) as u64 * inode_size as u64;
    let block_no = in_table as u64 + byte_off / block_size as u64;
    let within = (byte_off % block_size as u64) as usize;
    if within + inode_size as usize > block_size as usize || block_no > u32::MAX as u64 {
        return None;
    }
    if !ext2_read_block(block_no as u32, buf) {
        return None;
    }
    let p = unsafe { buf.add(within) };
    let mut inode = Ext2Inode {
        mode: read_u16(p),
        size: read_u32(unsafe { p.add(4) }),
        block: [0; 15],
    };
    let mut i = 0usize;
    while i < 15 {
        inode.block[i] = read_u32(unsafe { p.add(0x28 + i * 4) });
        i += 1;
    }
    Some(inode)
}
fn ext2_read_inode(ino: u32) -> Option<Ext2Inode> {
    ext2_read_inode_buf(ino, ext2_a())
}
/// 用 B 缓冲读 inode: 供 readdir 在 A 缓冲持有目录数据时使用 (避免互相覆盖)。
fn ext2_read_inode_b(ino: u32) -> Option<Ext2Inode> {
    ext2_read_inode_buf(ino, ext2_b())
}

/// 把 inode 的逻辑块号映射为物理块号 (空洞 / 越界返回 None)。
fn ext2_map_block(inode: &Ext2Inode, logical: u32) -> Option<u32> {
    let ptrs = unsafe { EXT2_BLOCK_SIZE } / 4;
    if ptrs == 0 {
        return None;
    }
    if logical < EXT2_DIRECT_BLOCKS {
        let b = inode.block[logical as usize];
        return if b == 0 { None } else { Some(b) };
    }
    let mut idx = logical - EXT2_DIRECT_BLOCKS;
    if idx < ptrs {
        let ind = inode.block[EXT2_IND_BLOCK];
        if ind == 0 {
            return None;
        }
        let buf = ext2_c();
        if !ext2_read_block(ind, buf) {
            return None;
        }
        let b = read_u32(unsafe { buf.add(idx as usize * 4) });
        return if b == 0 { None } else { Some(b) };
    }
    idx -= ptrs;
    if idx < ptrs * ptrs {
        let dind = inode.block[EXT2_DIND_BLOCK];
        if dind == 0 {
            return None;
        }
        let buf = ext2_c();
        if !ext2_read_block(dind, buf) {
            return None;
        }
        let first = read_u32(unsafe { buf.add((idx / ptrs) as usize * 4) });
        if first == 0 {
            return None;
        }
        let buf2 = ext2_d();
        if !ext2_read_block(first, buf2) {
            return None;
        }
        let b = read_u32(unsafe { buf2.add((idx % ptrs) as usize * 4) });
        return if b == 0 { None } else { Some(b) };
    }
    None // 三级间接不实现 (只读演示足够)
}

/// 名字比较; `ci = true` 时按 ASCII 大小写不敏感比较。
fn ext2_name_eq(a: &[u8], b: &[u8], ci: bool) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0usize;
    while i < a.len() {
        if a[i] == b[i] || (ci && ascii_upper(a[i]) == ascii_upper(b[i])) {
            i += 1;
            continue;
        }
        return false;
    }
    true
}

/// 在目录 inode 的数据块中顺序查找名字, 返回 (inode, file_type)。
fn ext2_dir_find(dir: &Ext2Inode, name: &[u8], ci: bool) -> Option<(u32, u8)> {
    let block_size = unsafe { EXT2_BLOCK_SIZE };
    let buf = ext2_a();
    let mut off = 0u32;
    while off < dir.size {
        let block = match ext2_map_block(dir, off / block_size) {
            Some(b) => b,
            None => {
                off += block_size;
                continue;
            }
        };
        if !ext2_read_block(block, buf) {
            return None;
        }
        let mut pos = 0usize;
        while pos + 8 <= block_size as usize {
            let e = unsafe { buf.add(pos) };
            let ino = read_u32(e);
            let rec_len = read_u16(unsafe { e.add(4) }) as usize;
            if rec_len < 8 || pos + rec_len > block_size as usize {
                break;
            }
            let name_len = unsafe { *e.add(6) } as usize;
            if ino != 0 && name_len == name.len() {
                let ename = unsafe { core::slice::from_raw_parts(e.add(8), name_len) };
                if ext2_name_eq(ename, name, ci) {
                    return Some((ino, unsafe { *e.add(7) }));
                }
            }
            pos += rec_len;
        }
        off += block_size;
    }
    None
}

/// 把绝对路径解析为 (inode 号, 是否目录)。路径必须以 '/' 开头。
fn ext2_resolve(path: &str) -> Option<(u32, bool)> {
    let bytes = path.as_bytes();
    if bytes.is_empty() || bytes[0] != b'/' {
        return None;
    }
    let mut ino = EXT2_ROOT_INO;
    let mut i = 1usize;
    while i < bytes.len() {
        let start = i;
        while i < bytes.len() && bytes[i] != b'/' {
            i += 1;
        }
        let comp = &bytes[start..i];
        if i < bytes.len() {
            i += 1; // 跳过分隔符
        }
        if comp.is_empty() || comp == b"." {
            continue;
        }
        let dir = ext2_read_inode(ino)?;
        if dir.mode & EXT2_S_IFMT != EXT2_S_IFDIR {
            return None;
        }
        let hit = match ext2_dir_find(&dir, comp, false) {
            Some(x) => Some(x),
            None => ext2_dir_find(&dir, comp, true),
        };
        ino = hit?.0;
    }
    let inode = ext2_read_inode(ino)?;
    Some((ino, inode.mode & EXT2_S_IFMT == EXT2_S_IFDIR))
}

/// 把 ext2 名字 (字节串) 拷进长名字段, 截断到容量且不切断 UTF-8 字符。
fn ext2_copy_long(name: &[u8], out: &mut [u8; vfs::DIR_LONG_MAX]) -> u8 {
    let mut n = name.len().min(vfs::DIR_LONG_MAX);
    // 截断点若落在字符中间 (续字节 10xxxxxx), 回退到该字符首字节之前。
    while n > 0 && n < name.len() && (name[n] & 0xC0) == 0x80 {
        n -= 1;
    }
    out[..n].copy_from_slice(&name[..n]);
    n as u8
}

/// 由 ext2 名字派生一个 8.3 形式的短名 (大写), 供无长名时的回退显示。
fn ext2_short_name(name: &[u8]) -> [u8; 11] {
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

/// 构造一条目录条目记录 (需要文件大小时额外读一次目标 inode, 用 B 缓冲)。
fn ext2_make_entry(name: &[u8], ftype: u8, ino: u32) -> vfs::DirEntry {
    let short = ext2_short_name(name);
    let mut long = [0u8; vfs::DIR_LONG_MAX];
    let llen = ext2_copy_long(name, &mut long);
    let mut is_dir = ftype == EXT2_FT_DIR;
    let mut size = 0u64;
    if let Some(inode) = ext2_read_inode_b(ino) {
        // file_type 在旧版 ext2 可能为 0, 这时以 inode 的 mode 为准。
        is_dir = inode.mode & EXT2_S_IFMT == EXT2_S_IFDIR;
        if !is_dir {
            size = inode.size as u64;
        }
    }
    vfs::DirEntry::with_long(short, long, llen, size, if is_dir { 1 } else { 0 })
}

/// 读文件区间 [offset, offset+count) 到 `dst`, 返回实际读取字节数。
fn ext2_read_data(ino: u32, offset: u32, count: u32, dst: *mut u8) -> Option<u64> {
    let inode = ext2_read_inode(ino)?;
    if inode.mode & EXT2_S_IFMT == EXT2_S_IFDIR {
        return None;
    }
    if offset >= inode.size {
        return Some(0);
    }
    let n = count.min(inode.size - offset);
    let block_size = unsafe { EXT2_BLOCK_SIZE };
    let buf = ext2_b();
    let mut done = 0u32;
    while done < n {
        let pos = offset + done;
        let boff = (pos % block_size) as usize;
        let chunk = (block_size as usize - boff).min((n - done) as usize);
        match ext2_map_block(&inode, pos / block_size) {
            Some(block) => {
                if !ext2_read_block(block, buf) {
                    return None;
                }
                unsafe {
                    core::ptr::copy_nonoverlapping(buf.add(boff), dst.add(done as usize), chunk);
                }
            }
            // 稀疏文件: 空洞按零填充。
            None => zero_bytes(unsafe { dst.add(done as usize) }, chunk),
        }
        done += chunk as u32;
    }
    Some(n as u64)
}

/// 列出目录条目 (跳过 "." / ".."), 返回写入字节数。
fn ext2_readdir(ino: u32, dst: *mut vfs::DirEntry) -> Option<u64> {
    let dir = ext2_read_inode(ino)?;
    if dir.mode & EXT2_S_IFMT != EXT2_S_IFDIR {
        return None;
    }
    let block_size = unsafe { EXT2_BLOCK_SIZE };
    let buf = ext2_a();
    let entry_size = core::mem::size_of::<vfs::DirEntry>();
    let mut count = 0usize;
    let mut off = 0u32;
    while off < dir.size {
        let block = match ext2_map_block(&dir, off / block_size) {
            Some(b) => b,
            None => {
                off += block_size;
                continue;
            }
        };
        if !ext2_read_block(block, buf) {
            return None;
        }
        let mut pos = 0usize;
        while pos + 8 <= block_size as usize {
            let e = unsafe { buf.add(pos) };
            let eino = read_u32(e);
            let rec_len = read_u16(unsafe { e.add(4) }) as usize;
            if rec_len < 8 || pos + rec_len > block_size as usize {
                break;
            }
            let name_len = unsafe { *e.add(6) } as usize;
            if eino != 0 && name_len > 0 {
                let name = unsafe { core::slice::from_raw_parts(e.add(8), name_len) };
                // "." / ".." 不列出 (与 FAT32 / MFS 的 readdir 输出保持一致)。
                if name != b"." && name != b".." {
                    if count >= vfs::RESULT_MAX_ENTRIES {
                        return Some((count * entry_size) as u64);
                    }
                    let de = ext2_make_entry(name, unsafe { *e.add(7) }, eino);
                    unsafe {
                        core::ptr::write_unaligned(dst.add(count), de);
                    }
                    count += 1;
                }
            }
            pos += rec_len;
        }
        off += block_size;
    }
    Some((count * entry_size) as u64)
}

// ---------------------------------------------------------------------------
// fd 表
// ---------------------------------------------------------------------------

fn ext2_fd_alloc(ino: u32, is_dir: bool, vol: u64) -> u64 {
    for i in 0..EXT2_MAX_FD {
        unsafe {
            let s = &mut *core::ptr::addr_of_mut!(EXT2_FDS).cast::<Ext2Fd>().add(i);
            if !s.used {
                s.used = true;
                s.is_dir = is_dir;
                s.ino = ino;
                s.vol = vol;
                return i as u64;
            }
        }
    }
    u64::MAX
}
/// 查 fd 并把「当前卷寄存器」切到该 fd 绑定的卷 (与路径类请求的 tag 卷编码等价)。
fn ext2_fd_get(fd: u32) -> Option<Ext2Fd> {
    if fd as usize >= EXT2_MAX_FD {
        return None;
    }
    unsafe {
        let s = &*core::ptr::addr_of!(EXT2_FDS)
            .cast::<Ext2Fd>()
            .add(fd as usize);
        if s.used {
            EXT2_CUR_VOL = s.vol;
            Some(*s)
        } else {
            None
        }
    }
}
fn ext2_fd_free(fd: u32) -> u64 {
    if fd as usize >= EXT2_MAX_FD {
        return 0;
    }
    unsafe {
        let s = &mut *core::ptr::addr_of_mut!(EXT2_FDS)
            .cast::<Ext2Fd>()
            .add(fd as usize);
        if s.used {
            s.used = false;
            1
        } else {
            0
        }
    }
}

// ---------------------------------------------------------------------------
// 挂载
// ---------------------------------------------------------------------------

/// 读取并校验超级块 + 块组描述符表; 成功即完成挂载 (只读, 不改盘)。
fn ext2_mount() -> bool {
    // 超级块固定在字节偏移 1024 (LBA 2), 前 1024 字节已含所需全部字段。
    let sb = ext2_a();
    if !block_read_dev(unsafe { EXT2_CUR_VOL }, 2, 2, sb) {
        return false;
    }
    if read_u16(unsafe { sb.add(0x38) }) != EXT2_MAGIC {
        return false;
    }
    let log_block_size = read_u32(unsafe { sb.add(0x18) });
    if log_block_size > 6 {
        return false; // 块大小上限 64 KiB
    }
    let block_size = 1024u32 << log_block_size;
    let blocks_count = read_u32(unsafe { sb.add(0x04) });
    let first_data_block = read_u32(unsafe { sb.add(0x14) });
    let blocks_per_group = read_u32(unsafe { sb.add(0x20) });
    let inodes_per_group = read_u32(unsafe { sb.add(0x28) });
    let rev_level = read_u32(unsafe { sb.add(0x4C) });
    let mut inode_size = 128u32;
    if rev_level != 0 {
        let s = read_u16(unsafe { sb.add(0x58) }) as u32;
        if s >= 128 {
            inode_size = s;
        }
    }
    if blocks_count == 0 || blocks_per_group == 0 || inodes_per_group == 0 {
        return false;
    }
    let groups = blocks_count.div_ceil(blocks_per_group);
    if groups == 0 || groups as usize > EXT2_MAX_GROUPS {
        return false;
    }
    // 卷参数必须先落盘到内存状态: 之后所有块 I/O 都要用 `EXT2_BLOCK_SIZE` 换算 LBA。
    unsafe {
        EXT2_BLOCK_SIZE = block_size;
        EXT2_INODES_PER_GROUP = inodes_per_group;
        EXT2_INODE_SIZE = inode_size;
        EXT2_GROUP_COUNT = groups;
    }
    // 块组描述符表紧随超级块所在块: 块号 = `s_first_data_block + 1`。
    let gdt_block = first_data_block + 1;
    let per_block = block_size / 32;
    if per_block == 0 {
        return false;
    }
    let gbuf = ext2_a();
    let mut g = 0u32;
    let mut blk = gdt_block;
    while g < groups {
        if !ext2_read_block(blk, gbuf) {
            return false;
        }
        let n = (groups - g).min(per_block);
        let mut k = 0u32;
        while k < n {
            let off = k as usize * 32 + 0x08; // bg_inode_table
            unsafe {
                EXT2_INODE_TABLE[(g + k) as usize] = read_u32(gbuf.add(off));
            }
            k += 1;
        }
        g += n;
        blk += 1;
    }
    // 根 inode 必须存在且是目录, 否则视为无效卷。
    let ok =
        matches!(ext2_read_inode(EXT2_ROOT_INO), Some(i) if i.mode & EXT2_S_IFMT == EXT2_S_IFDIR);
    if ok {
        // 记录「当前几何属于哪个卷」(M1b: 卷切换时据此判断要不要重新解析)。
        unsafe {
            EXT2_GEO_VOL = EXT2_CUR_VOL;
        }
    }
    ok
}

// ---------------------------------------------------------------------------
// 服务循环
// ---------------------------------------------------------------------------

/// 域 12 — ext2_srv: 只读服务 OPEN / READ / READDIR / STAT / CLOSE, 写操作一律拒绝。
pub fn run() {
    if sys_alloc_page(ext2_a() as u64) != 1
        || sys_alloc_page(ext2_b() as u64) != 1
        || sys_alloc_page(ext2_c() as u64) != 1
        || sys_alloc_page(ext2_d() as u64) != 1
    {
        println("ext2: alloc block buffers FAILED");
        return;
    }
    if sys_share_page(ext2_a() as u64, BLOCK_DOMAIN) != 1
        || sys_share_page(ext2_b() as u64, BLOCK_DOMAIN) != 1
        || sys_share_page(ext2_c() as u64, BLOCK_DOMAIN) != 1
        || sys_share_page(ext2_d() as u64, BLOCK_DOMAIN) != 1
    {
        println("ext2: share block buffers FAILED");
        return;
    }
    // 认领卷: 第一个 ext2 签名的卷; 无分区表的整盘镜像即卷 2 (回退值)。
    unsafe {
        EXT2_VOL = vol_claim(ext2_a(), 16, VOL_KIND_EXT2, EXT2_VOL_FALLBACK);
        EXT2_CUR_VOL = EXT2_VOL;
    }
    if !ext2_mount() {
        println("ext2: mount FAILED (not a valid ext2 volume)");
        return;
    }

    // M1b: 把**额外**的 ext2 卷挂到 `/usb<卷号>` (元数据已解析完, `ext2_a` 可作暂存)。
    mount_extra_volumes(
        ext2_a(),
        VOL_KIND_EXT2,
        unsafe { EXT2_VOL },
        vfs::EXT2_DOMAIN,
    );

    let mut msg = Message {
        from: 0,
        to: 0,
        tag: 0,
        payload: [0; PAYLOAD_LEN],
    };
    loop {
        sys_recv_msg(&mut msg as *mut Message as *mut u8);
        // 同 fat32_srv: tag 高位带卷编码 (M1b); fd 类请求的卷由 fd 绑定决定。
        let tag = vfs::tag_body(msg.tag);
        let mut vol = vfs::vol_from_enc(vfs::tag_vol(msg.tag), unsafe { EXT2_VOL });
        if matches!(tag, vfs::VFS_READ_TAG | vfs::VFS_READDIR_TAG) {
            if let Some(fd) = ext2_fd_get(read_u32(msg.payload.as_ptr())) {
                vol = fd.vol;
            }
        }
        unsafe {
            EXT2_CUR_VOL = vol;
        }
        // 卷切换: 各 ext2 卷的块大小 / inode 表位置不同, 必须重新解析该卷的超级块与
        // 块组描述符表 (只读, 不改盘), 否则会用上个卷的几何去换算块号。
        if unsafe { EXT2_GEO_VOL } != vol && !ext2_mount() {
            sys_reply(u64::MAX);
            continue;
        }
        match tag {
            vfs::VFS_OPEN_TAG => {
                let len = msg
                    .payload
                    .iter()
                    .position(|&b| b == 0)
                    .unwrap_or(PAYLOAD_LEN);
                let path = unsafe { core::str::from_utf8_unchecked(&msg.payload[..len]) };
                let fd = match ext2_resolve(path) {
                    Some((ino, is_dir)) => ext2_fd_alloc(ino, is_dir, vol),
                    None => u64::MAX,
                };
                sys_reply(fd);
            }
            vfs::VFS_READ_TAG => {
                let req: vfs::ReadReq = unsafe {
                    core::ptr::read_unaligned(msg.payload.as_ptr() as *const vfs::ReadReq)
                };
                // ext2 inode 的 size 是 u32: 协议 offset 超出 u32 直接失败。
                if req.offset > u32::MAX as u64 {
                    sys_reply(u64::MAX);
                    continue;
                }
                let n = match ext2_fd_get(req.fd) {
                    Some(fd) if !fd.is_dir => {
                        ext2_read_data(fd.ino, req.offset as u32, req.count, req.buf as *mut u8)
                            .unwrap_or(u64::MAX)
                    }
                    _ => u64::MAX,
                };
                sys_reply(n);
            }
            vfs::VFS_READDIR_TAG => {
                let req: vfs::DirReq = unsafe {
                    core::ptr::read_unaligned(msg.payload.as_ptr() as *const vfs::DirReq)
                };
                let n = match ext2_fd_get(req.fd) {
                    Some(fd) if fd.is_dir => {
                        ext2_readdir(fd.ino, req.buf as *mut vfs::DirEntry).unwrap_or(u64::MAX)
                    }
                    _ => u64::MAX,
                };
                sys_reply(n);
            }
            vfs::VFS_STAT_TAG => {
                let (buf, path) = parse_path_req(msg.payload.as_ptr());
                let n = match ext2_resolve(path) {
                    Some((ino, is_dir)) => match ext2_read_inode(ino) {
                        Some(inode) => {
                            // ext2 侧不做元数据映射 (M5 只覆盖 MFS), 大小按类型给。
                            let st = vfs::Stat::plain(
                                if is_dir { 0 } else { inode.size as u64 },
                                u32::from(is_dir),
                            );
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
            vfs::VFS_CLOSE_TAG => {
                let fd = read_u32(msg.payload.as_ptr());
                sys_reply(ext2_fd_free(fd));
            }
            // 只读服务: WRITE / CREAT / MKDIR / UNLINK / RMDIR 及其它一律拒绝。
            _ => {
                sys_reply(u64::MAX);
            }
        }
    }
}
