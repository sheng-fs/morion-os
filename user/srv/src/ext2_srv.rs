use crate::common::*;
use morion::syscall::*;
use morion::vfs;

// ===========================================================================
// 域 12 — ext2 有限读写文件服务 (ext2_srv)
// ===========================================================================
// 阶段 C3: 挂载既有 Linux ext2 分区。镜像由宿主 `mke2fs` 预格式化, 服务**不自动
// 格式化** —— 超级块无效即挂载失败 (与 MFS 的「首挂载自动格式化」相反: ext2 的定位
// 就是操作别人已有的分区)。
//
// 实现读取所需的最小 ext2 子集:
//   - 超级块 (@1024, magic 0xEF53) → 块大小 / 每组块数 / 每组 inode 数 / inode 大小;
//   - 块组描述符表 → 缓存每组的块位图 / inode 位图 / inode 表起始块;
//   - inode `block[15]` 的直接 / 一级间接块映射 (二级只读, 三级不实现);
//   - 目录项 (`inode/rec_len/name_len/file_type/name`) 顺序遍历。
//
// 在此基础上提供**有限写支持** (自测够用):
//   - CREAT: 在目标目录新建空普通文件 (分配 inode + 插入目录项, 目录块满则扩展);
//   - WRITE: 覆盖写, 按需分配数据块 (块位图 + inode block[], 支持一级间接);
//   - UNLINK: 摘除目录项并释放 inode / 数据块 / 位图位;
//   - 同步维护超级块与块组描述符的空闲计数。
//
// 镜像实测**未启用 metadata_csum** (`s_feature_incompat` 无 0x0400), 故不更新校验和。
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
/// 普通文件 `i_mode` (S_IFREG | 0644)。
const EXT2_S_IFREG_MODE: u16 = 0x81A4;

/// 目录项 `file_type` 值。
const EXT2_FT_FILE: u8 = 1;
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
// 写支持所需的额外几何: 总量、首个数据块、每组块数, 以及每组的块 / inode 位图起始块。
static mut EXT2_BLOCKS_COUNT: u32 = 0;
static mut EXT2_INODES_COUNT: u32 = 0;
static mut EXT2_FIRST_DATA_BLOCK: u32 = 0;
static mut EXT2_BLOCKS_PER_GROUP: u32 = 0;
static mut EXT2_BLOCK_BITMAP: [u32; EXT2_MAX_GROUPS] = [0; EXT2_MAX_GROUPS];
static mut EXT2_INODE_BITMAP: [u32; EXT2_MAX_GROUPS] = [0; EXT2_MAX_GROUPS];

/// ext2 inode 的读写所需字段 (只覆盖服务会读 / 会改的字段)。
#[derive(Clone, Copy)]
struct Ext2Inode {
    mode: u16,
    size: u32,
    atime: u32,
    ctime: u32,
    mtime: u32,
    links: u16,
    /// `i_blocks`: 已分配块数, 单位 512 字节。
    blocks: u32,
    block: [u32; 15],
}

/// 空 inode (删除时清零写回; 创建时作初值模板)。
const EXT2_INODE_EMPTY: Ext2Inode = Ext2Inode {
    mode: 0,
    size: 0,
    atime: 0,
    ctime: 0,
    mtime: 0,
    links: 0,
    blocks: 0,
    block: [0; 15],
};

/// 打开文件描述符 (记住 inode 号与打开时的卷即可, 不需要路径)。
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

/// 写一个 ext2 块 (块号 → LBA = 块号 × 每块扇区数)。
fn ext2_write_block(block_no: u32, src: *const u8) -> bool {
    let sectors = (unsafe { EXT2_BLOCK_SIZE } / 512) as u16;
    if sectors == 0 {
        return false;
    }
    block_write_dev(
        unsafe { EXT2_CUR_VOL },
        block_no * sectors as u32,
        sectors,
        src as *mut u8,
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
        atime: read_u32(unsafe { p.add(0x08) }),
        ctime: read_u32(unsafe { p.add(0x0C) }),
        mtime: read_u32(unsafe { p.add(0x10) }),
        links: read_u16(unsafe { p.add(0x1A) }),
        blocks: read_u32(unsafe { p.add(0x1C) }),
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

/// 把内存中的 inode 字段写回盘上对应槽位 (读-改-写, 未触及字节原样保留)。
///
/// 调用方须保证 `buf` 当前空闲; 传 `ext2_a()` 即可 (读写路径 A 缓冲在写 inode 时都空闲)。
fn ext2_write_inode_buf(ino: u32, inode: &Ext2Inode, buf: *mut u8) -> bool {
    if ino == 0 {
        return false;
    }
    let per_group = unsafe { EXT2_INODES_PER_GROUP };
    let inode_size = unsafe { EXT2_INODE_SIZE };
    let block_size = unsafe { EXT2_BLOCK_SIZE };
    let idx = ino - 1;
    let group = idx / per_group;
    if group >= unsafe { EXT2_GROUP_COUNT } {
        return false;
    }
    let in_table = unsafe { EXT2_INODE_TABLE[group as usize] };
    let byte_off = (idx % per_group) as u64 * inode_size as u64;
    let block_no = in_table as u64 + byte_off / block_size as u64;
    let within = (byte_off % block_size as u64) as usize;
    if within + inode_size as usize > block_size as usize || block_no > u32::MAX as u64 {
        return false;
    }
    if !ext2_read_block(block_no as u32, buf) {
        return false;
    }
    let p = unsafe { buf.add(within) };
    write_u16(p, inode.mode);
    write_u32(unsafe { p.add(4) }, inode.size);
    write_u32(unsafe { p.add(0x08) }, inode.atime);
    write_u32(unsafe { p.add(0x0C) }, inode.ctime);
    write_u32(unsafe { p.add(0x10) }, inode.mtime);
    write_u16(unsafe { p.add(0x1A) }, inode.links);
    write_u32(unsafe { p.add(0x1C) }, inode.blocks);
    let mut i = 0usize;
    while i < 15 {
        write_u32(unsafe { p.add(0x28 + i * 4) }, inode.block[i]);
        i += 1;
    }
    ext2_write_block(block_no as u32, buf)
}
fn ext2_write_inode(ino: u32, inode: &Ext2Inode) -> bool {
    ext2_write_inode_buf(ino, inode, ext2_a())
}

/// 当前 Unix 秒 (ext2 时间字段是 u32; 0 表示未知)。
fn ext2_now() -> u32 {
    mfs_now() as u32
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
    None // 三级间接不实现 (读写均不覆盖)
}

// ---------------------------------------------------------------------------
// 写原语: 位图 / 分配与释放 / 超级块与块组描述符计数
// ---------------------------------------------------------------------------

/// 位图字节中第 `bit` 位是否为 1。
fn ext2_bitmap_test(buf: *const u8, bit: u32) -> bool {
    let byte = unsafe { core::ptr::read_volatile(buf.add((bit / 8) as usize)) };
    byte & (1u8 << (bit % 8)) != 0
}

/// 设置 / 清除位图第 `bit` 位。
fn ext2_bitmap_put(buf: *mut u8, bit: u32, set: bool) {
    let byte = (bit / 8) as usize;
    let mask = 1u8 << (bit % 8);
    let cur = unsafe { core::ptr::read_volatile(buf.add(byte)) };
    let nv = if set { cur | mask } else { cur & !mask };
    unsafe {
        core::ptr::write_volatile(buf.add(byte), nv);
    }
}

/// 修改超级块空闲计数 (块 / inode 增量可为负), 读-改-写 LBA 2 起的 2 个扇区。
fn ext2_sb_bump(blk_delta: i64, ino_delta: i64) -> bool {
    let buf = ext2_d();
    if !block_read_dev(unsafe { EXT2_CUR_VOL }, 2, 2, buf) {
        return false;
    }
    let fb = read_u32(unsafe { buf.add(0x0C) }) as i64 + blk_delta;
    let fi = read_u32(unsafe { buf.add(0x10) }) as i64 + ino_delta;
    if fb < 0 || fi < 0 {
        return false;
    }
    write_u32(unsafe { buf.add(0x0C) }, fb as u32);
    write_u32(unsafe { buf.add(0x10) }, fi as u32);
    block_write_dev(unsafe { EXT2_CUR_VOL }, 2, 2, buf)
}

/// 修改块组 `group` 描述符的空闲块 / inode / 已用目录计数。
fn ext2_gdt_bump(group: u32, blk_delta: i64, ino_delta: i64, dir_delta: i64) -> bool {
    let block_size = unsafe { EXT2_BLOCK_SIZE };
    let per_block = block_size / 32;
    if per_block == 0 || group >= unsafe { EXT2_GROUP_COUNT } {
        return false;
    }
    let gdt_block = unsafe { EXT2_FIRST_DATA_BLOCK } + 1;
    let blk = gdt_block + group / per_block;
    let off = (group % per_block) as usize * 32;
    let buf = ext2_d();
    if !ext2_read_block(blk, buf) {
        return false;
    }
    let fb = read_u16(unsafe { buf.add(off + 0x0C) }) as i64 + blk_delta;
    let fi = read_u16(unsafe { buf.add(off + 0x0E) }) as i64 + ino_delta;
    let ud = read_u16(unsafe { buf.add(off + 0x10) }) as i64 + dir_delta;
    if !(0..=0xFFFF).contains(&fb) || !(0..=0xFFFF).contains(&fi) || !(0..=0xFFFF).contains(&ud) {
        return false;
    }
    write_u16(unsafe { buf.add(off + 0x0C) }, fb as u16);
    write_u16(unsafe { buf.add(off + 0x0E) }, fi as u16);
    write_u16(unsafe { buf.add(off + 0x10) }, ud as u16);
    ext2_write_block(blk, buf)
}

/// 分配一个空闲数据块, 置位块位图、更新计数并清零该块。失败返回 None。
fn ext2_alloc_block() -> Option<u32> {
    let bpg = unsafe { EXT2_BLOCKS_PER_GROUP };
    let first = unsafe { EXT2_FIRST_DATA_BLOCK };
    let total = unsafe { EXT2_BLOCKS_COUNT };
    if bpg == 0 {
        return None;
    }
    let mut g = 0u32;
    while g < unsafe { EXT2_GROUP_COUNT } {
        let bm = unsafe { EXT2_BLOCK_BITMAP[g as usize] };
        let buf = ext2_a();
        if !ext2_read_block(bm, buf) {
            return None;
        }
        let mut bit = 0u32;
        while bit < bpg {
            let blk = first as u64 + g as u64 * bpg as u64 + bit as u64;
            if blk >= total as u64 {
                break;
            }
            if !ext2_bitmap_test(buf, bit) {
                ext2_bitmap_put(buf, bit, true);
                if !ext2_write_block(bm, buf) || !ext2_gdt_bump(g, -1, 0, 0) || !ext2_sb_bump(-1, 0)
                {
                    return None;
                }
                zero_bytes(ext2_b(), unsafe { EXT2_BLOCK_SIZE } as usize);
                if !ext2_write_block(blk as u32, ext2_b()) {
                    return None;
                }
                return Some(blk as u32);
            }
            bit += 1;
        }
        g += 1;
    }
    None
}

/// 释放数据块 `blk`: 清块位图位并更新计数。
fn ext2_free_block(blk: u32) -> bool {
    let bpg = unsafe { EXT2_BLOCKS_PER_GROUP };
    let first = unsafe { EXT2_FIRST_DATA_BLOCK };
    if bpg == 0 || blk < first {
        return false;
    }
    let rel = blk - first;
    let g = rel / bpg;
    let bit = rel % bpg;
    if g >= unsafe { EXT2_GROUP_COUNT } {
        return false;
    }
    let bm = unsafe { EXT2_BLOCK_BITMAP[g as usize] };
    let buf = ext2_a();
    if !ext2_read_block(bm, buf) {
        return false;
    }
    ext2_bitmap_put(buf, bit, false);
    ext2_write_block(bm, buf) && ext2_gdt_bump(g, 1, 0, 0) && ext2_sb_bump(1, 0)
}

/// 分配一个空闲 inode, 置位 inode 位图与计数。失败返回 None。
fn ext2_alloc_inode() -> Option<u32> {
    let ipg = unsafe { EXT2_INODES_PER_GROUP };
    let total = unsafe { EXT2_INODES_COUNT };
    if ipg == 0 {
        return None;
    }
    let mut g = 0u32;
    while g < unsafe { EXT2_GROUP_COUNT } {
        let bm = unsafe { EXT2_INODE_BITMAP[g as usize] };
        let buf = ext2_a();
        if !ext2_read_block(bm, buf) {
            return None;
        }
        let mut bit = 0u32;
        while bit < ipg {
            let ino = g as u64 * ipg as u64 + bit as u64 + 1;
            if ino > total as u64 {
                break;
            }
            if !ext2_bitmap_test(buf, bit) {
                ext2_bitmap_put(buf, bit, true);
                if !ext2_write_block(bm, buf) || !ext2_gdt_bump(g, 0, -1, 0) || !ext2_sb_bump(0, -1)
                {
                    return None;
                }
                return Some(ino as u32);
            }
            bit += 1;
        }
        g += 1;
    }
    None
}

/// 释放 inode `ino` 的 inode 位图位并更新计数。
fn ext2_free_inode(ino: u32) -> bool {
    let ipg = unsafe { EXT2_INODES_PER_GROUP };
    if ino == 0 || ipg == 0 {
        return false;
    }
    let g = (ino - 1) / ipg;
    let bit = (ino - 1) % ipg;
    if g >= unsafe { EXT2_GROUP_COUNT } {
        return false;
    }
    let bm = unsafe { EXT2_INODE_BITMAP[g as usize] };
    let buf = ext2_a();
    if !ext2_read_block(bm, buf) {
        return false;
    }
    ext2_bitmap_put(buf, bit, false);
    ext2_write_block(bm, buf) && ext2_gdt_bump(g, 0, 1, 0) && ext2_sb_bump(0, 1)
}

/// 依据 inode 现有的 block[] 重算 `i_blocks` (单位 512 字节, 含间接块自身)。
fn ext2_recount_blocks(inode: &mut Ext2Inode) {
    let per = unsafe { EXT2_BLOCK_SIZE } / 512;
    let mut total = 0u32;
    let mut i = 0usize;
    while i < EXT2_DIRECT_BLOCKS as usize {
        if inode.block[i] != 0 {
            total += per;
        }
        i += 1;
    }
    let ind = inode.block[EXT2_IND_BLOCK];
    if ind != 0 {
        total += per;
        let ptrs = unsafe { EXT2_BLOCK_SIZE } / 4;
        let buf = ext2_c();
        if ext2_read_block(ind, buf) {
            let mut k = 0usize;
            while k < ptrs as usize {
                if read_u32(unsafe { buf.add(k * 4) }) != 0 {
                    total += per;
                }
                k += 1;
            }
        }
    }
    inode.blocks = total;
}

/// 释放 inode 名下的全部数据块 (直接 + 一级间接) 与间接块本身。
fn ext2_free_inode_blocks(inode: &Ext2Inode) -> bool {
    let mut i = 0usize;
    while i < EXT2_DIRECT_BLOCKS as usize {
        let b = inode.block[i];
        if b != 0 && !ext2_free_block(b) {
            return false;
        }
        i += 1;
    }
    let ind = inode.block[EXT2_IND_BLOCK];
    if ind != 0 {
        let ptrs = unsafe { EXT2_BLOCK_SIZE } / 4;
        let buf = ext2_c();
        if !ext2_read_block(ind, buf) {
            return false;
        }
        let mut k = 0usize;
        while k < ptrs as usize {
            let b = read_u32(unsafe { buf.add(k * 4) });
            if b != 0 && !ext2_free_block(b) {
                return false;
            }
            k += 1;
        }
        if !ext2_free_block(ind) {
            return false;
        }
    }
    true
}

/// 取得逻辑块 `logical` 的物理块号; 为空 (未分配) 时分配一个并写回 inode / 间接表。
///
/// `*newly` 置位表示该块是本次新分配的 (调用方可不读盘直接覆盖写)。只支持直接 + 一级间接。
fn ext2_get_or_alloc_block(inode: &mut Ext2Inode, logical: u32, newly: &mut bool) -> Option<u32> {
    *newly = false;
    let ptrs = unsafe { EXT2_BLOCK_SIZE } / 4;
    if logical < EXT2_DIRECT_BLOCKS {
        let cur = inode.block[logical as usize];
        if cur != 0 {
            return Some(cur);
        }
        let b = ext2_alloc_block()?;
        inode.block[logical as usize] = b;
        *newly = true;
        return Some(b);
    }
    let idx = logical - EXT2_DIRECT_BLOCKS;
    if ptrs == 0 || idx >= ptrs {
        return None; // 二级 / 三级间接不写入
    }
    if inode.block[EXT2_IND_BLOCK] == 0 {
        inode.block[EXT2_IND_BLOCK] = ext2_alloc_block()?;
    }
    let ind = inode.block[EXT2_IND_BLOCK];
    let buf = ext2_c();
    if !ext2_read_block(ind, buf) {
        return None;
    }
    let cur = read_u32(unsafe { buf.add(idx as usize * 4) });
    if cur != 0 {
        return Some(cur);
    }
    let b = ext2_alloc_block()?;
    write_u32(unsafe { buf.add(idx as usize * 4) }, b);
    if !ext2_write_block(ind, buf) {
        return None;
    }
    *newly = true;
    Some(b)
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

/// 把数据写入文件 `ino` 的 [offset, offset+count): 按需分配数据块并更新 inode。
///
/// 覆盖写语义: 只支持直接 + 一级间接块 (越界返回 None)。返回写入字节数。
fn ext2_write_data(ino: u32, offset: u32, count: u32, src: *const u8) -> Option<u64> {
    let mut inode = ext2_read_inode(ino)?;
    if inode.mode & EXT2_S_IFMT == EXT2_S_IFDIR {
        return None;
    }
    if count == 0 {
        return Some(0);
    }
    let block_size = unsafe { EXT2_BLOCK_SIZE };
    let ptrs = block_size / 4;
    let max_logical = EXT2_DIRECT_BLOCKS + ptrs; // 直接 + 一级间接
    let end = offset as u64 + count as u64;
    if (end - 1) / block_size as u64 >= max_logical as u64 {
        return None;
    }
    let buf = ext2_b();
    let mut done = 0u32;
    while done < count {
        let pos = offset + done;
        let logical = pos / block_size;
        let boff = (pos % block_size) as usize;
        let chunk = (block_size as usize - boff).min((count - done) as usize);
        let mut newly = false;
        let blk = ext2_get_or_alloc_block(&mut inode, logical, &mut newly)?;
        if newly {
            zero_bytes(buf, block_size as usize);
        } else if !ext2_read_block(blk, buf) {
            return None;
        }
        unsafe {
            core::ptr::copy_nonoverlapping(src.add(done as usize), buf.add(boff), chunk);
        }
        if !ext2_write_block(blk, buf) {
            return None;
        }
        done += chunk as u32;
    }
    if (end as u32) > inode.size {
        inode.size = end as u32;
    }
    let now = ext2_now();
    inode.mtime = now;
    inode.ctime = now;
    ext2_recount_blocks(&mut inode);
    if !ext2_write_inode(ino, &inode) {
        return None;
    }
    Some(count as u64)
}

/// 拆分绝对路径为 (父目录路径, 末段名字); 非法 (末段为空 / "." / "..") 返回 None。
fn ext2_split_parent(path: &str) -> Option<(&str, &str)> {
    let b = path.as_bytes();
    if b.is_empty() || b[0] != b'/' {
        return None;
    }
    let mut i = b.len();
    while i > 0 {
        i -= 1;
        if b[i] == b'/' {
            break;
        }
    }
    if i + 1 >= b.len() {
        return None; // 以 '/' 结尾或路径就是 "/"
    }
    let name = &path[i + 1..];
    if name == "." || name == ".." {
        return None;
    }
    let parent = if i == 0 { "/" } else { &path[..i] };
    Some((parent, name))
}

/// 在目录里定位名字对应的目录项, 返回 (inode, file_type, 物理块号, 块内偏移, rec_len)。
fn ext2_dir_find_at(
    dir: &Ext2Inode,
    name: &[u8],
    ci: bool,
) -> Option<(u32, u8, u32, usize, usize)> {
    let block_size = unsafe { EXT2_BLOCK_SIZE };
    let buf = ext2_b();
    let mut off = 0u32;
    while off < dir.size {
        let blk = match ext2_map_block(dir, off / block_size) {
            Some(b) => b,
            None => {
                off += block_size;
                continue;
            }
        };
        if !ext2_read_block(blk, buf) {
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
                    return Some((ino, unsafe { *e.add(7) }, blk, pos, rec_len));
                }
            }
            pos += rec_len;
        }
        off += block_size;
    }
    None
}

/// 在目录 `dir` 中插入一条指向 `child` 的目录项; 目录块满则分配新块并扩展。
///
/// 成功时 `dir` 已就地更新 size / i_blocks / 时间, 并写回盘。`dir_ino` 为其 inode 号。
fn ext2_dir_add(dir_ino: u32, dir: &mut Ext2Inode, name: &[u8], child: u32, ftype: u8) -> bool {
    let block_size = unsafe { EXT2_BLOCK_SIZE };
    let name_len = name.len();
    if name_len == 0 || name_len > 255 {
        return false;
    }
    let need = (8 + name_len + 3) & !3usize;
    if need > block_size as usize {
        return false;
    }
    let buf = ext2_b();
    let nblocks = dir.size.div_ceil(block_size);
    let mut lb = 0u32;
    while lb < nblocks {
        let blk = match ext2_map_block(dir, lb) {
            Some(b) => b,
            None => {
                lb += 1;
                continue;
            }
        };
        if !ext2_read_block(blk, buf) {
            return false;
        }
        let mut pos = 0usize;
        while pos + 8 <= block_size as usize {
            let e = unsafe { buf.add(pos) };
            let eino = read_u32(e);
            let rec_len = read_u16(unsafe { e.add(4) }) as usize;
            if rec_len < 8 || pos + rec_len > block_size as usize {
                break;
            }
            let enl = unsafe { *e.add(6) } as usize;
            if eino == 0 {
                if rec_len >= need {
                    write_u32(e, child);
                    unsafe {
                        *e.add(6) = name_len as u8;
                        *e.add(7) = ftype;
                        core::ptr::copy_nonoverlapping(name.as_ptr(), e.add(8), name_len);
                    }
                    return ext2_write_block(blk, buf);
                }
            } else {
                let actual = (8 + enl + 3) & !3usize;
                if rec_len >= actual + need {
                    // 切分尾项: 前段保留原名, 后段成为新项的槽位。
                    write_u16(unsafe { e.add(4) }, actual as u16);
                    let ne = unsafe { e.add(actual) };
                    write_u32(ne, child);
                    write_u16(unsafe { ne.add(4) }, (rec_len - actual) as u16);
                    unsafe {
                        *ne.add(6) = name_len as u8;
                        *ne.add(7) = ftype;
                        core::ptr::copy_nonoverlapping(name.as_ptr(), ne.add(8), name_len);
                    }
                    return ext2_write_block(blk, buf);
                }
            }
            pos += rec_len;
        }
        lb += 1;
    }
    // 目录内无空槽: 追加一个新目录块 (整块作为一条空记录再填入本项)。
    let mut newly = false;
    let nb = match ext2_get_or_alloc_block(dir, nblocks, &mut newly) {
        Some(b) => b,
        None => return false,
    };
    zero_bytes(buf, block_size as usize);
    write_u32(buf, child);
    write_u16(unsafe { buf.add(4) }, block_size as u16);
    unsafe {
        *buf.add(6) = name_len as u8;
        *buf.add(7) = ftype;
        core::ptr::copy_nonoverlapping(name.as_ptr(), buf.add(8), name_len);
    }
    if !ext2_write_block(nb, buf) {
        return false;
    }
    dir.size += block_size;
    ext2_recount_blocks(dir);
    let now = ext2_now();
    dir.mtime = now;
    dir.ctime = now;
    ext2_write_inode(dir_ino, dir)
}

/// 在 `path` 的父目录中新建空普通文件, 返回新 inode 号; 名字冲突 / 失败返回 None。
fn ext2_creat(path: &str) -> Option<u32> {
    let (parent, name) = ext2_split_parent(path)?;
    let name_b = name.as_bytes();
    let (pino, is_dir) = ext2_resolve(parent)?;
    if !is_dir {
        return None;
    }
    let mut pdir = ext2_read_inode(pino)?;
    if ext2_dir_find(&pdir, name_b, false).is_some() || ext2_dir_find(&pdir, name_b, true).is_some()
    {
        return None;
    }
    let ino = ext2_alloc_inode()?;
    let now = ext2_now();
    let inode = Ext2Inode {
        mode: EXT2_S_IFREG_MODE,
        size: 0,
        atime: now,
        ctime: now,
        mtime: now,
        links: 1,
        blocks: 0,
        block: [0; 15],
    };
    if !ext2_write_inode(ino, &inode) {
        return None;
    }
    if !ext2_dir_add(pino, &mut pdir, name_b, ino, EXT2_FT_FILE) {
        return None;
    }
    Some(ino)
}

/// 删除 `path` 指向的普通文件: 摘除目录项并释放其数据块与 inode。成功返回 1。
fn ext2_unlink(path: &str) -> Option<u64> {
    let (parent, name) = ext2_split_parent(path)?;
    let (pino, is_dir) = ext2_resolve(parent)?;
    if !is_dir {
        return None;
    }
    let mut pdir = ext2_read_inode(pino)?;
    let name_b = name.as_bytes();
    let (child_ino, _ftype, blk, pos, rec_len) =
        ext2_dir_find_at(&pdir, name_b, false).or_else(|| ext2_dir_find_at(&pdir, name_b, true))?;
    let child = ext2_read_inode(child_ino)?;
    if child.mode & EXT2_S_IFMT == EXT2_S_IFDIR {
        return None; // unlink 不删目录
    }
    // 摘除目录项: 置 ino=0, 并把它并入前一条记录 (保持目录块紧凑)。
    let buf = ext2_b();
    if !ext2_read_block(blk, buf) {
        return None;
    }
    let mut ppos = 0usize;
    let mut prev: Option<usize> = None;
    while ppos < pos {
        let e = unsafe { buf.add(ppos) };
        let rl = read_u16(unsafe { e.add(4) }) as usize;
        if rl < 8 || ppos + rl > unsafe { EXT2_BLOCK_SIZE } as usize {
            break;
        }
        prev = Some(ppos);
        ppos += rl;
    }
    if let Some(pp) = prev {
        let pe = unsafe { buf.add(pp) };
        let prl = read_u16(unsafe { pe.add(4) }) as usize;
        write_u16(unsafe { pe.add(4) }, (prl + rec_len) as u16);
    }
    write_u32(unsafe { buf.add(pos) }, 0);
    if !ext2_write_block(blk, buf) {
        return None;
    }
    if !ext2_free_inode_blocks(&child) {
        return None;
    }
    if !ext2_free_inode(child_ino) {
        return None;
    }
    // 清空 inode 槽 (尽力而为; 位图清位后 fsck 已视其为空闲)。
    ext2_write_inode(child_ino, &EXT2_INODE_EMPTY);
    let now = ext2_now();
    pdir.mtime = now;
    pdir.ctime = now;
    if !ext2_write_inode(pino, &pdir) {
        return None;
    }
    Some(1)
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

/// 读取并校验超级块 + 块组描述符表; 成功即完成挂载 (解析本身不写盘)。
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
    let inodes_count = read_u32(unsafe { sb.add(0x00) });
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
    if blocks_count == 0 || inodes_count == 0 || blocks_per_group == 0 || inodes_per_group == 0 {
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
        EXT2_BLOCKS_COUNT = blocks_count;
        EXT2_INODES_COUNT = inodes_count;
        EXT2_FIRST_DATA_BLOCK = first_data_block;
        EXT2_BLOCKS_PER_GROUP = blocks_per_group;
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
            let base = k as usize * 32;
            unsafe {
                EXT2_BLOCK_BITMAP[(g + k) as usize] = read_u32(gbuf.add(base));
                EXT2_INODE_BITMAP[(g + k) as usize] = read_u32(gbuf.add(base + 0x04));
                EXT2_INODE_TABLE[(g + k) as usize] = read_u32(gbuf.add(base + 0x08));
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

/// 域 12 — ext2_srv: 读 OPEN / READ / READDIR / STAT / CLOSE, 写 CREAT / WRITE / UNLINK。
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
        if matches!(
            tag,
            vfs::VFS_READ_TAG | vfs::VFS_READDIR_TAG | vfs::VFS_WRITE_TAG
        ) {
            if let Some(fd) = ext2_fd_get(read_u32(msg.payload.as_ptr())) {
                vol = fd.vol;
            }
        }
        unsafe {
            EXT2_CUR_VOL = vol;
        }
        // 卷切换: 各 ext2 卷的块大小 / inode 表位置不同, 必须重新解析该卷的超级块与
        // 块组描述符表 (重新解析, 本身不改盘), 否则会用上个卷的几何去换算块号。
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
            vfs::VFS_CREAT_TAG => {
                let len = msg
                    .payload
                    .iter()
                    .position(|&b| b == 0)
                    .unwrap_or(PAYLOAD_LEN);
                let path = unsafe { core::str::from_utf8_unchecked(&msg.payload[..len]) };
                let fd = match ext2_creat(path) {
                    Some(ino) => ext2_fd_alloc(ino, false, vol),
                    None => u64::MAX,
                };
                sys_reply(fd);
            }
            vfs::VFS_WRITE_TAG => {
                let req: vfs::WriteReq = unsafe {
                    core::ptr::read_unaligned(msg.payload.as_ptr() as *const vfs::WriteReq)
                };
                // ext2 inode 的 size 是 u32: 协议 offset 超出 u32 直接失败。
                if req.offset > u32::MAX as u64 {
                    sys_reply(u64::MAX);
                    continue;
                }
                let n = match ext2_fd_get(req.fd) {
                    Some(fd) if !fd.is_dir => {
                        ext2_write_data(fd.ino, req.offset as u32, req.count, req.buf as *const u8)
                            .unwrap_or(u64::MAX)
                    }
                    _ => u64::MAX,
                };
                sys_reply(n);
            }
            vfs::VFS_UNLINK_TAG => {
                let len = msg
                    .payload
                    .iter()
                    .position(|&b| b == 0)
                    .unwrap_or(PAYLOAD_LEN);
                let path = unsafe { core::str::from_utf8_unchecked(&msg.payload[..len]) };
                sys_reply(ext2_unlink(path).unwrap_or(u64::MAX));
            }
            // 未实现的写类 tag (MKDIR / RMDIR / TRUNCATE / RENAME / ...) 一律拒绝。
            _ => {
                sys_reply(u64::MAX);
            }
        }
    }
}
