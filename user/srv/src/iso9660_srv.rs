//! 域 20 — ISO9660 只读文件服务 (iso9660_srv, 03c 续: 安装介质)
//!
//! 只读读取 CD/安装盘 (ISO9660)。镜像由宿主 `xorriso`/`mkisofs` 生成, 服务**不写盘**。
//! 第一版把 `.iso` 当**裸块设备**接进来 (QEMU 里作 `nvme-ns`), 与"CD 硬件 (ATAPI)"解耦 ——
//! 文件系统只面对线性扇区流, 真机光驱通路留作后续增量。
//!
//! 实现读取所需的最小 ISO9660 子集:
//!   - 主卷描述符 (PVD, LBA 16 起, 标识 `"CD001"`) → 逻辑块大小 / 根目录记录 / 卷标识;
//!   - 目录记录 (`length` / `extent` / `data_len` / `flags` / `name[;1]`) 顺序遍历,
//!     `;N` 版本后缀剥掉、名字大小写不敏感比较 (与 ISO 惯例一致);
//!   - 文件按 `extent` (逻辑块号) → 512 扇区直接读, 块层读写经 block_srv 卷层。
//!
//! 写类请求 (`CREAT` / `WRITE` / `MKDIR` / …) 一律回复失败 —— CD 不可写, 能力上诚实。

use crate::common::*;
use morion::syscall::*;
use morion::vfs;

/// 卷号回退值 (探测到 `VOL_KIND_ISO` 时按签名认领; 回退仅兜底)。ISO 通常接成整盘卷。
const ISO_VOL_FALLBACK: u64 = 8;

/// 缓冲页: 逻辑块读出目标 (同址共享给 block_srv)。`A` 放主块, `B` 供跨页文件读的尾部扇区。
///
/// 必须避开其它服务的固定页 (所有域加载同一份镜像, 撞址会 `PageAlreadyMapped`):
/// fat32 `+0x10_0/1/2`, app/shell `+0x10_4..7`, MFS `+0x10_8..B` 与 `+0x11_0..3`/`+0x16_2`/
/// `+0x18..1B`, ext2 `+0x10_C..F`, exfat `+0x11_4000`/`+0x15_4..6`, block/xhci `+0x16_1/3/4`。
/// 取 `+0x12_0000` / `+0x12_1000` (两页, 空档)。
const ISO_BUF_A_VADDR: u64 = 0x0000_0080_0012_0000;
const ISO_BUF_B_VADDR: u64 = 0x0000_0080_0012_1000;

fn iso_a() -> *mut u8 {
    ISO_BUF_A_VADDR as *mut u8
}
fn iso_b() -> *mut u8 {
    ISO_BUF_B_VADDR as *mut u8
}

/// 打开文件/目录上限。
const ISO_MAX_FD: usize = 16;

/// 卷号 (启动时由 `vol_claim` 认领)。
static mut ISO_VOL: u64 = ISO_VOL_FALLBACK;
/// 逻辑块大小 (字节, 512/1024/2048) 与根目录 extent (逻辑块号) / 数据长度 (字节)。
static mut ISO_BLK: u32 = 2048;
static mut ISO_ROOT_EXTENT: u32 = 0;
static mut ISO_ROOT_LEN: u32 = 0;
/// 卷标识 (PVD 偏移 40, 32 字节, 空格填充), 供 marker 打印。
static mut ISO_VOLID: [u8; 32] = [0; 32];

/// 每逻辑块的 512 扇区数 (= `ISO_BLK / 512`)。
fn iso_secs_per_blk() -> u32 {
    (unsafe { ISO_BLK }) / 512
}

/// 打开文件描述符 (记住 extent / 长度 / 类型; 不需要路径)。
#[derive(Clone, Copy)]
struct IsoFd {
    used: bool,
    is_dir: bool,
    extent: u32,
    size: u32,
    vol: u64,
}
const ISO_FD_EMPTY: IsoFd = IsoFd {
    used: false,
    is_dir: false,
    extent: 0,
    size: 0,
    vol: 0,
};
static mut ISO_FDS: [IsoFd; ISO_MAX_FD] = [ISO_FD_EMPTY; ISO_MAX_FD];

fn iso_fd_alloc(extent: u32, size: u32, is_dir: bool, vol: u64) -> u64 {
    for i in 0..ISO_MAX_FD {
        unsafe {
            let s = &mut *core::ptr::addr_of_mut!(ISO_FDS).cast::<IsoFd>().add(i);
            if !s.used {
                s.used = true;
                s.is_dir = is_dir;
                s.extent = extent;
                s.size = size;
                s.vol = vol;
                return i as u64;
            }
        }
    }
    u64::MAX
}

/// 查 fd, 并把「当前卷」切到该 fd 绑定的卷 (与路径类请求的 tag 卷编码等价)。
fn iso_fd_get(fd: u32) -> Option<IsoFd> {
    if fd as usize >= ISO_MAX_FD {
        return None;
    }
    unsafe {
        let s = &*core::ptr::addr_of!(ISO_FDS)
            .cast::<IsoFd>()
            .add(fd as usize);
        if s.used {
            ISO_VOL = s.vol;
            Some(*s)
        } else {
            None
        }
    }
}

fn iso_fd_free(fd: u32) -> u64 {
    if fd as usize >= ISO_MAX_FD {
        return 0;
    }
    unsafe {
        let s = &mut *core::ptr::addr_of_mut!(ISO_FDS)
            .cast::<IsoFd>()
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
// ISO9660 解析
// ---------------------------------------------------------------------------

/// 读一个逻辑块 (块号 → 卷内扇区 = 块号 × 每块扇区数) 到 `buf`。
fn iso_read_block(blk: u32, buf: *mut u8) -> bool {
    let secs = iso_secs_per_blk();
    if secs == 0 || secs > u16::MAX as u32 {
        return false;
    }
    block_read_dev(unsafe { ISO_VOL }, blk * secs, secs as u16, buf)
}

/// ISO 名字比较: 剥掉 `;N` 版本后缀后按大小写不敏感比较 (与一次精确匹配语义一致)。
fn iso_name_eq(raw: &[u8], want: &[u8]) -> bool {
    let end = raw.iter().position(|&b| b == b';').unwrap_or(raw.len());
    let raw = &raw[..end];
    raw.len() == want.len() && raw.iter().zip(want).all(|(a, b)| a.eq_ignore_ascii_case(b))
}

/// 目录记录 (在 `dir_extent` / `dir_len` 描述的目录里) 查名字 `want`,
/// 命中返回 `(extent, data_len, is_dir)`。
fn iso_dir_lookup(dir_extent: u32, dir_len: u32, want: &[u8]) -> Option<(u32, u32, bool)> {
    let blk = unsafe { ISO_BLK };
    let nblocks = dir_len.div_ceil(blk);
    let buf = iso_a();
    let mut b = 0u32;
    while b < nblocks {
        if !iso_read_block(dir_extent + b, buf) {
            return None;
        }
        let lim = blk as usize;
        let mut pos = 0usize;
        while pos < lim {
            let r = unsafe { buf.add(pos) };
            let rec_len = unsafe { *r } as usize;
            if rec_len == 0 {
                break; // 本块剩余是填充
            }
            if rec_len < 33 || pos + rec_len > lim {
                break; // 坏记录, 停止扫本块
            }
            let name_len = unsafe { *r.add(32) } as usize;
            if name_len >= 1 && name_len <= rec_len - 33 {
                let name = unsafe { core::slice::from_raw_parts(r.add(33), name_len) };
                let special = name_len == 1 && (name[0] == 0 || name[0] == 1); // "." / ".."
                if !special && iso_name_eq(name, want) {
                    let extent = read_u32(unsafe { r.add(2) });
                    let len = read_u32(unsafe { r.add(10) });
                    let is_dir = unsafe { *r.add(25) } & 0x02 != 0;
                    return Some((extent, len, is_dir));
                }
            }
            pos += rec_len;
        }
        b += 1;
    }
    None
}

/// 解析以 `/` 开头的服务内绝对路径, 返回 `(extent, data_len, is_dir)`。
fn iso_resolve(path: &str) -> Option<(u32, u32, bool)> {
    let mut cur = unsafe { (ISO_ROOT_EXTENT, ISO_ROOT_LEN, true) };
    for comp in path.split('/') {
        if comp.is_empty() {
            continue;
        }
        if !cur.2 {
            return None;
        }
        cur = iso_dir_lookup(cur.0, cur.1, comp.as_bytes())?;
    }
    Some(cur)
}

/// 读文件 `extent` 的 `[off, off+count)` 字节到 `dst`。返回实际读取字节数, 失败 `u64::MAX`。
///
/// 单次最多 9 个 512 扇区 (`count ≤ 4096` 且可能不对齐), 故用两页缓冲: 前 8 扇区进 `A`,
/// 第 9 扇区进 `B` —— 每次 `block_read_dev` 都 ≤ 8 扇区 (一页内), 不依赖跨页连续 DMA。
fn iso_read_file(extent: u32, size: u32, off: u64, count: u32, dst: *mut u8) -> u64 {
    if off >= size as u64 {
        return 0;
    }
    let n = (count as u64).min(size as u64 - off) as usize;
    let base = extent * iso_secs_per_blk();
    let s0 = base + (off / 512) as u32;
    let off_in = (off % 512) as usize;
    let total_secs = (off_in + n).div_ceil(512);
    let secs1 = total_secs.min(8) as u16;
    if !block_read_dev(unsafe { ISO_VOL }, s0, secs1, iso_a()) {
        return u64::MAX;
    }
    if total_secs > 8 && !block_read_dev(unsafe { ISO_VOL }, s0 + 8, 1, iso_b()) {
        return u64::MAX;
    }
    let first = (4096 - off_in).min(n);
    unsafe {
        core::ptr::copy_nonoverlapping(iso_a().add(off_in), dst, first);
    }
    if n > first {
        unsafe {
            core::ptr::copy_nonoverlapping(iso_b(), dst.add(first), n - first);
        }
    }
    n as u64
}

/// 填充一条 `DirEntry`: 名字剥掉 `;N`, 8.3 短名尽力而为 (截断大写), 长名放全名。
fn iso_fill_entry(e: &mut vfs::DirEntry, raw: &[u8], size: u64, is_dir: bool) {
    let end = raw.iter().position(|&b| b == b';').unwrap_or(raw.len());
    let name = &raw[..end];
    let n = name.len().min(vfs::DIR_LONG_MAX);
    let mut long = [0u8; vfs::DIR_LONG_MAX];
    long[..n].copy_from_slice(&name[..n]);
    let mut sn = [b' '; 11];
    for (i, &c) in name.iter().take(11).enumerate() {
        sn[i] = c.to_ascii_uppercase();
    }
    *e = vfs::DirEntry::with_long(sn, long, n as u8, size, u32::from(is_dir));
}

/// 列目录条目到 `dst` (最多一页), 返回字节数。`;N` 与 `.`/`..` 均跳过。
fn iso_readdir(fd: IsoFd, dst: *mut vfs::DirEntry) -> u64 {
    let blk = unsafe { ISO_BLK };
    let nblocks = fd.size.div_ceil(blk);
    let entry_size = core::mem::size_of::<vfs::DirEntry>();
    let cap = vfs::RESULT_MAX_ENTRIES;
    let buf = iso_a();
    let mut count = 0usize;
    let mut b = 0u32;
    while b < nblocks {
        if !iso_read_block(fd.extent + b, buf) {
            return u64::MAX;
        }
        let lim = blk as usize;
        let mut pos = 0usize;
        while pos < lim {
            let r = unsafe { buf.add(pos) };
            let rec_len = unsafe { *r } as usize;
            if rec_len == 0 {
                break;
            }
            if rec_len < 33 || pos + rec_len > lim {
                break;
            }
            let name_len = unsafe { *r.add(32) } as usize;
            if name_len >= 1 && name_len <= rec_len - 33 {
                let name = unsafe { core::slice::from_raw_parts(r.add(33), name_len) };
                let special = name_len == 1 && (name[0] == 0 || name[0] == 1);
                if !special {
                    if count >= cap {
                        return (count * entry_size) as u64;
                    }
                    let is_dir = unsafe { *r.add(25) } & 0x02 != 0;
                    let size = if is_dir {
                        0
                    } else {
                        read_u32(unsafe { r.add(10) }) as u64
                    };
                    let e = unsafe { &mut *dst.add(count) };
                    iso_fill_entry(e, name, size, is_dir);
                    count += 1;
                }
            }
            pos += rec_len;
        }
        b += 1;
    }
    (count * entry_size) as u64
}

/// 数目录里的条目数 (不含 `.`/`..`), 供自测 marker。
fn iso_count_entries(extent: u32, len: u32) -> u64 {
    let blk = unsafe { ISO_BLK };
    let nblocks = len.div_ceil(blk);
    let buf = iso_a();
    let mut c = 0u64;
    let mut b = 0u32;
    while b < nblocks {
        if !iso_read_block(extent + b, buf) {
            return c;
        }
        let lim = blk as usize;
        let mut pos = 0usize;
        while pos < lim {
            let r = unsafe { buf.add(pos) };
            let rec_len = unsafe { *r } as usize;
            if rec_len == 0 {
                break;
            }
            if rec_len < 33 || pos + rec_len > lim {
                break;
            }
            let name_len = unsafe { *r.add(32) } as usize;
            if name_len >= 1 && name_len <= rec_len - 33 {
                let name = unsafe { core::slice::from_raw_parts(r.add(33), name_len) };
                let special = name_len == 1 && (name[0] == 0 || name[0] == 1);
                if !special {
                    c += 1;
                }
            }
            pos += rec_len;
        }
        b += 1;
    }
    c
}

/// 挂载: 从第 16 个 ISO 逻辑扇区 (2048 字节, 即 512 字节 LBA **64**) 起找 PVD
/// (type=1, 标识 `"CD001"`), 解析逻辑块大小 / 根目录 / 卷标识。
fn iso_mount() -> bool {
    let buf = iso_a();
    // 卷描述符集: 从 ISO 逻辑扇区 16 起, 每条 2048 字节 (= 4 个 512 扇区)。
    let mut lba = 64u32;
    let mut found = false;
    // El Torito 可能把引导记录排在 PVD 之前, 故往后扫几条描述符找 type=1。
    while lba < 64 + 8 * 4 {
        if !block_read_dev(unsafe { ISO_VOL }, lba, 1, buf) {
            return false;
        }
        let ty = unsafe { *buf };
        let mut id_ok = true;
        for (i, &c) in b"CD001".iter().enumerate() {
            if unsafe { *buf.add(1 + i) } != c {
                id_ok = false;
                break;
            }
        }
        if id_ok && ty == 1 {
            found = true;
            break;
        }
        lba += 4;
    }
    if !found {
        return false;
    }
    let blk = read_u16(unsafe { buf.add(128) }) as u32;
    if blk != 512 && blk != 1024 && blk != 2048 {
        return false;
    }
    unsafe {
        ISO_BLK = blk;
        // 根目录记录固定在 PVD 偏移 156 (34 字节)。
        let rr = buf.add(156);
        ISO_ROOT_EXTENT = read_u32(rr.add(2));
        ISO_ROOT_LEN = read_u32(rr.add(10));
        // 卷标识: PVD 偏移 40, 32 字节。
        core::ptr::copy_nonoverlapping(
            buf.add(40),
            core::ptr::addr_of_mut!(ISO_VOLID).cast::<u8>(),
            32,
        );
    }
    true
}

/// 打印卷标识 (去掉尾部空格)。
fn iso_print_volid() {
    let v = unsafe { ISO_VOLID };
    let end = v.iter().position(|&b| b == 0).unwrap_or(v.len());
    let s = unsafe { core::str::from_utf8_unchecked(&v[..end]) };
    print(s.trim_end());
}

/// 端到端自测 (marker `ISO1`): 解析根目录 + 读 ISO 根下的 `EFIBOOT.IMG` 并校验 FAT 引导签名。
///
/// `efiboot.img` 是 Makefile 用 mtools 造的 FAT32 ESP 镜像, 其首扇区以 `0xEB`/`0xE9` 跳转开头、
/// 尾两字节 `0x55AA` —— 三个字节都对, 才说明"目录解析 + 按 extent 真读出了文件内容"。
fn iso_selftest() {
    let entries = iso_count_entries(unsafe { ISO_ROOT_EXTENT }, unsafe { ISO_ROOT_LEN });
    let mut read_ok = false;
    if let Some((extent, len, is_dir)) = iso_dir_lookup(
        unsafe { ISO_ROOT_EXTENT },
        unsafe { ISO_ROOT_LEN },
        b"EFIBOOT.IMG",
    ) {
        if !is_dir && len >= 512 {
            let sec = extent * iso_secs_per_blk();
            if block_read_dev(unsafe { ISO_VOL }, sec, 1, iso_a()) {
                let jmp = unsafe { *iso_a() };
                let lo = unsafe { *iso_a().add(510) };
                let hi = unsafe { *iso_a().add(511) };
                read_ok = (jmp == 0xEB || jmp == 0xE9) && lo == 0x55 && hi == 0xAA;
            }
        }
    }
    print("ISO1 iso9660 OK, volid=");
    iso_print_volid();
    print(", root_entries=");
    print_u64(entries);
    print(", read=");
    println(if read_ok { "ok" } else { "BAD" });
    if !read_ok {
        println("iso9660: self-test read FAILED");
    }
}

/// 域 20 — ISO9660 只读服务主循环。
pub fn run() {
    if sys_alloc_page(iso_a() as u64) != 1 || sys_alloc_page(iso_b() as u64) != 1 {
        println("iso9660: alloc block buffers FAILED");
        return;
    }
    if sys_share_page(iso_a() as u64, BLOCK_DOMAIN) != 1
        || sys_share_page(iso_b() as u64, BLOCK_DOMAIN) != 1
    {
        println("iso9660: share block buffers FAILED");
        return;
    }
    unsafe {
        ISO_VOL = vol_claim(iso_a(), 16, VOL_KIND_ISO, ISO_VOL_FALLBACK);
    }
    if !iso_mount() {
        println("iso9660: mount FAILED (not a valid ISO9660 volume)");
        return;
    }
    iso_selftest();

    // 额外 ISO 卷 (一般没有): 与其它服务保持一致的挂载行为, 无副作用。
    mount_extra_volumes(
        iso_a(),
        VOL_KIND_ISO,
        unsafe { ISO_VOL },
        vfs::ISO9660_DOMAIN,
    );

    let mut msg = Message {
        from: 0,
        to: 0,
        tag: 0,
        payload: [0; PAYLOAD_LEN],
    };
    loop {
        sys_recv_msg(&mut msg as *mut Message as *mut u8);
        let tag = vfs::tag_body(msg.tag);
        let vol = vfs::vol_from_enc(vfs::tag_vol(msg.tag), unsafe { ISO_VOL });
        unsafe {
            ISO_VOL = vol;
        }
        match tag {
            vfs::VFS_OPEN_TAG => {
                let len = msg
                    .payload
                    .iter()
                    .position(|&b| b == 0)
                    .unwrap_or(PAYLOAD_LEN);
                let path = unsafe { core::str::from_utf8_unchecked(&msg.payload[..len]) };
                let fd = match iso_resolve(path) {
                    Some((extent, size, is_dir)) => iso_fd_alloc(extent, size, is_dir, vol),
                    None => u64::MAX,
                };
                sys_reply(fd);
            }
            vfs::VFS_READ_TAG => {
                let req: vfs::ReadReq = unsafe {
                    core::ptr::read_unaligned(msg.payload.as_ptr() as *const vfs::ReadReq)
                };
                let n = match iso_fd_get(req.fd) {
                    Some(fd) if !fd.is_dir => iso_read_file(
                        fd.extent,
                        fd.size,
                        req.offset,
                        req.count,
                        req.buf as *mut u8,
                    ),
                    _ => u64::MAX,
                };
                sys_reply(n);
            }
            vfs::VFS_READDIR_TAG => {
                let req: vfs::DirReq = unsafe {
                    core::ptr::read_unaligned(msg.payload.as_ptr() as *const vfs::DirReq)
                };
                let n = match iso_fd_get(req.fd) {
                    Some(fd) if fd.is_dir => iso_readdir(fd, req.buf as *mut vfs::DirEntry),
                    _ => u64::MAX,
                };
                sys_reply(n);
            }
            vfs::VFS_STAT_TAG => {
                let (buf, path) = parse_path_req(msg.payload.as_ptr());
                let n = match iso_resolve(path) {
                    Some((_, size, is_dir)) => {
                        let st = vfs::Stat::plain(
                            if is_dir { 0 } else { size as u64 },
                            u32::from(is_dir),
                        );
                        unsafe {
                            core::ptr::write_unaligned(buf as *mut vfs::Stat, st);
                        }
                        core::mem::size_of::<vfs::Stat>() as u64
                    }
                    None => u64::MAX,
                };
                sys_reply(n);
            }
            vfs::VFS_CLOSE_TAG => {
                let fd = read_u32(msg.payload.as_ptr());
                sys_reply(iso_fd_free(fd));
            }
            // CD 只读: 写类请求一律拒绝。
            _ => {
                sys_reply(u64::MAX);
            }
        }
    }
}
