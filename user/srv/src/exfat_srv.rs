use crate::common::*;
use morion::syscall::*;
use morion::vfs;

// ---------------------------------------------------------------------------
// 目录项构造与增删
// ---------------------------------------------------------------------------

/// Unix 秒 → exFAT 打包时间戳 (UTC); 返回 (时间戳, 10ms 增量)。
fn exfat_encode_time(unix: u64) -> (u32, u8) {
    if unix == 0 {
        return (0, 0);
    }
    let secs = unix as i64;
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    if !(1980..=2107).contains(&y) {
        return (0, 0);
    }
    let rem = secs.rem_euclid(86_400);
    let hour = rem / 3600;
    let min = (rem % 3600) / 60;
    let sec = rem % 60;
    let ts = (((y - 1980) as u32) << 25)
        | ((m as u32) << 21)
        | ((d as u32) << 16)
        | ((hour as u32) << 11)
        | ((min as u32) << 5)
        | ((sec / 2) as u32);
    (ts, ((sec % 2) * 100) as u8)
}

/// upcase 表 / 位图的「按需窗口」缓存状态。
///
/// 这两个表在大容量卷上可以很大 (位图可达 MB 级), 因此不整体载入, 而是
/// 每次把用到的那个 512B 扇区读进一页窗口; `SEC` 记录窗口当前装的是哪个扇区
/// (命中时无需再走 FAT 链换算 LBA)。
static mut EXFAT_UPC_WIN_SEC: usize = usize::MAX;
static mut EXFAT_UPC_WIN_LBA: u32 = 0;
static mut EXFAT_BMP_WIN_SEC: usize = usize::MAX;
static mut EXFAT_BMP_WIN_LBA: u32 = 0;
static mut EXFAT_BMP_DIRTY: bool = false;

/// 表 (`first` 起始的 FAT 链) 第 `sec` 个 512B 扇区的 LBA。
fn exfat_chain_sec_lba(first: u32, sec: usize) -> Option<u32> {
    let spc = unsafe { EXFAT_SECTORS_PER_CLUSTER } as usize;
    if spc == 0 {
        return None;
    }
    let cl = exfat_chain_nth(first, (sec / spc) as u32)?;
    Some(exfat_cluster_lba(cl) + (sec % spc) as u32)
}

/// upcase 表查询: 表只覆盖前 N 个码元, 超出者映射为自身。
fn exfat_upcase_unit(u: u16) -> u16 {
    let off = u as usize * 2;
    if (off + 2) as u32 > unsafe { EXFAT_UPCASE_BYTES } {
        return u;
    }
    let sec = off / EXFAT_SECTOR_SIZE as usize;
    if unsafe { EXFAT_UPC_WIN_SEC } != sec {
        let lba = match exfat_chain_sec_lba(unsafe { EXFAT_UPCASE_CLUSTER }, sec) {
            Some(l) => l,
            None => return u,
        };
        if !exfat_read_sectors(lba, 1, exfat_upc()) {
            return u;
        }
        unsafe {
            EXFAT_UPC_WIN_SEC = sec;
            EXFAT_UPC_WIN_LBA = lba;
        }
    }
    read_u16(exfat_at(exfat_upc(), off % EXFAT_SECTOR_SIZE as usize))
}

/// exFAT NameHash: 每个码元先对散列循环右移 1 位再累加其 upcase 值, 末尾再右移一次。
fn exfat_name_hash(units: &[u16], len: usize) -> u16 {
    let mut h: u16 = 0;
    let mut i = 0usize;
    while i < len {
        h = h.rotate_right(1).wrapping_add(exfat_upcase_unit(units[i]));
        i += 1;
    }
    h.rotate_right(1)
}

/// UTF-8 名字 → UTF-16 码元 (仅 BMP; 补充平面 (4 字节序列) 不支持)。
fn exfat_encode_name(name: &[u8], out: &mut [u16; 255]) -> Option<usize> {
    let mut n = 0usize;
    let mut i = 0usize;
    while i < name.len() {
        let b = name[i];
        let (cp, adv) = if b < 0x80 {
            (b as u32, 1)
        } else if b & 0xE0 == 0xC0 {
            if i + 1 >= name.len() {
                return None;
            }
            ((((b & 0x1F) as u32) << 6) | (name[i + 1] & 0x3F) as u32, 2)
        } else if b & 0xF0 == 0xE0 {
            if i + 2 >= name.len() {
                return None;
            }
            (
                (((b & 0x0F) as u32) << 12)
                    | (((name[i + 1] & 0x3F) as u32) << 6)
                    | (name[i + 2] & 0x3F) as u32,
                3,
            )
        } else {
            return None;
        };
        if cp > 0xFFFF || n >= 255 {
            return None;
        }
        out[n] = cp as u16;
        n += 1;
        i += adv;
    }
    if n == 0 {
        None
    } else {
        Some(n)
    }
}

/// 在 `out` 构造一组文件 entry set (`0x85` + `0xC0` + N×`0xC1`), 返回总条目数。
#[allow(clippy::too_many_arguments)]
fn exfat_build_set(
    out: *mut u8,
    name: &[u8],
    is_dir: bool,
    no_fat_chain: bool,
    first_cluster: u32,
    data_len: u64,
    valid_len: u64,
    mtime: u64,
) -> Option<usize> {
    let mut units = [0u16; 255];
    let nu = exfat_encode_name(name, &mut units)?;
    let name_entries = nu.div_ceil(EXFAT_NAME_UNITS_PER_ENTRY);
    let total = 2 + name_entries;
    let (ts, ten) = exfat_encode_time(mtime);
    zero_bytes(out, total * EXFAT_DIR_ENTRY);
    // 0x85 File
    unsafe {
        *out = EXFAT_TYPE_FILE;
        *out.add(1) = (1 + name_entries) as u8; // SecondaryCount = 0xC0 + N×0xC1
    }
    write_u16(
        exfat_atm(out, 4),
        if is_dir {
            EXFAT_ATTR_DIR
        } else {
            EXFAT_ATTR_ARCHIVE
        },
    );
    write_u32(exfat_atm(out, 8), ts); // CreateTimestamp
    write_u32(exfat_atm(out, 12), ts); // LastModifiedTimestamp
    write_u32(exfat_atm(out, 16), ts); // LastAccessedTimestamp
    unsafe {
        *out.add(20) = ten;
        *out.add(21) = ten;
    }
    // 0xC0 Stream Extension
    let stream = exfat_atm(out, EXFAT_DIR_ENTRY);
    unsafe {
        *stream = EXFAT_TYPE_STREAM;
        *stream.add(1) = EXFAT_SF_ALLOC | if no_fat_chain { EXFAT_SF_NOFATCHAIN } else { 0 };
        *stream.add(3) = nu as u8; // NameLength (UTF-16 码元数)
    }
    write_u16(exfat_atm(stream, 4), exfat_name_hash(&units, nu));
    write_u64(exfat_atm(stream, 8), valid_len);
    write_u32(exfat_atm(stream, 20), first_cluster);
    write_u64(exfat_atm(stream, 24), data_len);
    // N×0xC1 File Name
    let mut k = 0usize;
    while k < nu {
        let ent = exfat_atm(out, (2 + k / EXFAT_NAME_UNITS_PER_ENTRY) * EXFAT_DIR_ENTRY);
        unsafe {
            *ent = EXFAT_TYPE_NAME;
        }
        write_u16(
            exfat_atm(ent, 2 + (k % EXFAT_NAME_UNITS_PER_ENTRY) * 2),
            units[k],
        );
        k += 1;
    }
    let sum = exfat_set_checksum(out, (total * EXFAT_DIR_ENTRY) as u32);
    write_u16(exfat_atm(out, 2), sum);
    Some(total)
}

/// 目录条目组的磁盘位置: 第 `cluster` 簇内下标 `index`, 共 `1 + sec_count` 个条目。
#[derive(Clone, Copy)]
struct ExfatLoc {
    cluster: u32,
    index: usize,
    /// `0x85` 的 SecondaryCount (从属条目数)。
    sec_count: usize,
}

/// 在目录链中定位 `want` 的条目组 (需要簇与簇内下标, 便于原地改写)。
fn exfat_dir_locate(dir_first: u32, want: &[u8]) -> Option<ExfatLoc> {
    let cb = unsafe { EXFAT_CLUSTER_BYTES } as usize;
    if cb < EXFAT_DIR_ENTRY {
        return None;
    }
    let per = cb / EXFAT_DIR_ENTRY;
    let buf = exfat_clu();
    let mut cl = dir_first;
    let mut guard = 0u32;
    while cl >= 2 && guard <= unsafe { EXFAT_CLUSTER_COUNT } + 1 {
        if !exfat_read_cluster(cl, buf) {
            return None;
        }
        let mut i = 0usize;
        while i < per {
            let t = unsafe { *exfat_at(buf, i * EXFAT_DIR_ENTRY) };
            if t == EXFAT_TYPE_UNUSED {
                // `0x00` 只表示「本簇剩余条目未使用」; 目录链的后续簇仍可能有条目,
                // 故只跳过本簇余下部分, 不能整体停止 (否则会漏掉后面的簇)。
                break;
            }
            if t == EXFAT_TYPE_FILE {
                let sec = unsafe { *exfat_at(buf, i * EXFAT_DIR_ENTRY + 1) } as usize;
                if let Some(e) = exfat_parse_file_set(buf, i, per, sec) {
                    if exfat_name_eq(&e.name[..e.name_len as usize], want, true) {
                        return Some(ExfatLoc {
                            cluster: cl,
                            index: i,
                            sec_count: sec,
                        });
                    }
                }
                i += 1 + sec;
                continue;
            }
            i += 1;
        }
        let next = exfat_fat_get(cl)?;
        if next == EXFAT_FAT_FREE || exfat_is_eoc(next) {
            return None;
        }
        cl = next;
        guard += 1;
    }
    None
}

/// 在目录链中找一段可容纳 `need` 个连续条目的空位 (结尾 `0x00` 或已删除条目)。
///
/// 已删除条目 (in-use 位为 0) 可复用; 成组条目整体跳过, 避免把组中间当空位。
fn exfat_dir_find_slot(dir_first: u32, need: usize) -> Option<ExfatLoc> {
    let cb = unsafe { EXFAT_CLUSTER_BYTES } as usize;
    if cb < EXFAT_DIR_ENTRY || need == 0 {
        return None;
    }
    let per = cb / EXFAT_DIR_ENTRY;
    let buf = exfat_clu();
    let mut cl = dir_first;
    let mut guard = 0u32;
    while cl >= 2 && guard <= unsafe { EXFAT_CLUSTER_COUNT } + 1 {
        if !exfat_read_cluster(cl, buf) {
            return None;
        }
        let mut i = 0usize;
        while i < per {
            let t = unsafe { *exfat_at(buf, i * EXFAT_DIR_ENTRY) };
            if t != EXFAT_TYPE_UNUSED && t & 0x80 != 0 {
                if t == EXFAT_TYPE_FILE {
                    let sec = unsafe { *exfat_at(buf, i * EXFAT_DIR_ENTRY + 1) } as usize;
                    i += 1 + sec;
                } else {
                    i += 1;
                }
                continue;
            }
            let mut k = 0usize;
            while k < need && i + k < per {
                let tt = unsafe { *exfat_at(buf, (i + k) * EXFAT_DIR_ENTRY) };
                if tt != EXFAT_TYPE_UNUSED && tt & 0x80 != 0 {
                    break;
                }
                k += 1;
            }
            if k == need {
                return Some(ExfatLoc {
                    cluster: cl,
                    index: i,
                    sec_count: need - 1,
                });
            }
            i += 1;
        }
        let next = exfat_fat_get(cl)?;
        if next == EXFAT_FAT_FREE || exfat_is_eoc(next) {
            return None;
        }
        cl = next;
        guard += 1;
    }
    None
}

/// 目录链尾追加一个清零的簇 (目录放不下时扩容)。
///
/// 扩容前把链尾簇中尾部的未使用项 (`0x00`) 填成非 0 的「已删除」标记 ——
/// exFAT 规定 `0x00` 之后不得再出现非 0 项, 否则链上后续簇里的条目会被判为
/// 损坏 (宿主 `fsck.exfat` 直接报 `other entry follows unused entry`)。
fn exfat_dir_grow(dir_first: u32) -> Option<u32> {
    let cb = unsafe { EXFAT_CLUSTER_BYTES } as usize;
    if cb == 0 {
        return None;
    }
    let per = cb / EXFAT_DIR_ENTRY;
    // 1) 走到链尾簇。
    let mut cur = dir_first;
    let mut guard = 0u32;
    loop {
        let next = exfat_fat_get(cur)?;
        if next == EXFAT_FAT_FREE || exfat_is_eoc(next) {
            break;
        }
        cur = next;
        guard += 1;
        if guard > unsafe { EXFAT_CLUSTER_COUNT } + 1 {
            return None;
        }
    }
    // 2) 填掉尾部空位 (幂等: 已填过的簇不会再出现 0x00 尾部)。
    let buf = exfat_clu();
    if !exfat_read_cluster(cur, buf) {
        return None;
    }
    let mut i = 0usize;
    let mut need_fill = false;
    while i < per {
        if unsafe { *exfat_at(buf, i * EXFAT_DIR_ENTRY) } == EXFAT_TYPE_UNUSED {
            need_fill = true;
            break;
        }
        i += 1;
    }
    if need_fill {
        let mut k = i;
        while k < per {
            unsafe {
                *exfat_atm(buf, k * EXFAT_DIR_ENTRY) = EXFAT_FILLER;
            }
            k += 1;
        }
        if !exfat_write_cluster(cur, buf) {
            return None;
        }
    }
    // 3) 追加并链接一个清零的新簇。
    let cl = exfat_alloc_cluster()?;
    let z = exfat_clu();
    zero_bytes(z, cb);
    if !exfat_write_cluster(cl, z) {
        return None;
    }
    if !exfat_fat_set(cur, cl) {
        return None;
    }
    Some(cl)
}

/// 把一组条目写入 `loc` 指定的位置 (读-改-写所在簇)。
fn exfat_dir_put_set(loc: ExfatLoc, set: *const u8, total: usize) -> bool {
    let cb = unsafe { EXFAT_CLUSTER_BYTES } as usize;
    if cb == 0 || (loc.index + total) * EXFAT_DIR_ENTRY > cb {
        return false;
    }
    let buf = exfat_clu();
    if !exfat_read_cluster(loc.cluster, buf) {
        return false;
    }
    unsafe {
        core::ptr::copy_nonoverlapping(
            set,
            exfat_atm(buf, loc.index * EXFAT_DIR_ENTRY),
            total * EXFAT_DIR_ENTRY,
        );
    }
    exfat_write_cluster(loc.cluster, buf)
}

/// 把一组条目整体标记为已删除 (清 in-use 位); 不回收簇, 由调用方决定。
fn exfat_dir_del_set(loc: ExfatLoc) -> bool {
    let cb = unsafe { EXFAT_CLUSTER_BYTES } as usize;
    let total = 1 + loc.sec_count;
    if cb == 0 || (loc.index + total) * EXFAT_DIR_ENTRY > cb {
        return false;
    }
    let buf = exfat_clu();
    if !exfat_read_cluster(loc.cluster, buf) {
        return false;
    }
    for k in 0..total {
        unsafe {
            *exfat_atm(buf, (loc.index + k) * EXFAT_DIR_ENTRY) &= 0x7F;
        }
    }
    exfat_write_cluster(loc.cluster, buf)
}

/// 目录是否为空 (忽略系统项与卷标: 它们不是 `0x85` 条目)。
fn exfat_dir_is_empty(dir_first: u32) -> bool {
    let mut empty = true;
    exfat_dir_scan(dir_first, |_e| {
        empty = false;
        false
    });
    empty
}

/// 用新的元数据重写目录里 `name` 的 entry set (含 SetChecksum)。
fn exfat_rewrite_set(parent_first: u32, name: &[u8], e: &ExfatEntry) -> bool {
    let loc = match exfat_dir_locate(parent_first, name) {
        Some(l) => l,
        None => return false,
    };
    let mut set = [0u8; EXFAT_SET_MAX_BYTES];
    let total = match exfat_build_set(
        set.as_mut_ptr(),
        name,
        e.is_dir,
        e.no_fat_chain,
        e.first_cluster,
        e.size,
        e.valid_size,
        e.mtime,
    ) {
        Some(t) => t,
        None => return false,
    };
    if total != 1 + loc.sec_count {
        return false; // 名字编码长度变化会改变条目数, 不支持原地替换
    }
    exfat_dir_put_set(loc, set.as_ptr(), total)
}

/// 把绝对路径拆成 (父目录路径, 最后一段名字字节); 父目录为根时返回 `"/"`。
fn exfat_split_parent(path: &str) -> Option<(&str, &[u8])> {
    let b = path.as_bytes();
    if b.is_empty() || b[0] != b'/' || b.len() > TMP_PATH_MAX {
        return None;
    }
    let mut end = b.len();
    while end > 0 && b[end - 1] == b'/' {
        end -= 1;
    }
    if end <= 1 {
        return None; // 根或空路径: 不能创建/删除根
    }
    let mut i = end;
    while i > 0 && b[i - 1] != b'/' {
        i -= 1;
    }
    let name = &b[i..end];
    if name.is_empty() || name == b"." || name == b".." {
        return None;
    }
    let parent = if i <= 1 { "/" } else { &path[..i - 1] };
    Some((parent, name))
}

/// 创建文件 / 目录 (已存在且类型匹配则直接打开), 返回 fd。
fn exfat_create(path: &str, is_dir: bool, vol: u64) -> u64 {
    if path.is_empty() || path == "/" {
        return u64::MAX;
    }
    if let Some(e) = exfat_resolve(path) {
        return if e.is_dir == is_dir {
            exfat_fd_alloc(path, is_dir, vol)
        } else {
            u64::MAX
        };
    }
    let (pdir, name) = match exfat_split_parent(path) {
        Some(v) => v,
        None => return u64::MAX,
    };
    let parent = match exfat_resolve(pdir) {
        Some(e) if e.is_dir => e,
        _ => return u64::MAX,
    };
    // 目录先占一个清零的簇 (文件按需在写入时分配)。
    let mut first = 0u32;
    if is_dir {
        first = match exfat_alloc_cluster() {
            Some(cl) => cl,
            None => return u64::MAX,
        };
        let z = exfat_clu();
        zero_bytes(z, unsafe { EXFAT_CLUSTER_BYTES } as usize);
        if !exfat_write_cluster(first, z) {
            return u64::MAX;
        }
    }
    let mtime = mfs_now();
    let mut set = [0u8; EXFAT_SET_MAX_BYTES];
    let total = match exfat_build_set(set.as_mut_ptr(), name, is_dir, false, first, 0, 0, mtime) {
        Some(t) => t,
        None => return u64::MAX,
    };
    let loc = match exfat_dir_find_slot(parent.first_cluster, total) {
        Some(l) => l,
        None => {
            if exfat_dir_grow(parent.first_cluster).is_none() {
                return u64::MAX;
            }
            match exfat_dir_find_slot(parent.first_cluster, total) {
                Some(l) => l,
                None => return u64::MAX,
            }
        }
    };
    if !exfat_dir_put_set(loc, set.as_ptr(), total) {
        return u64::MAX;
    }
    exfat_fd_alloc(path, is_dir, vol)
}

/// 删除文件 (`want_dir = false`) 或空目录 (`want_dir = true`)。
fn exfat_remove(path: &str, want_dir: bool) -> u64 {
    let (pdir, name) = match exfat_split_parent(path) {
        Some(v) => v,
        None => return u64::MAX,
    };
    let parent = match exfat_resolve(pdir) {
        Some(e) if e.is_dir => e,
        _ => return u64::MAX,
    };
    let e = match exfat_dir_lookup(parent.first_cluster, name) {
        Some(e) => e,
        None => return u64::MAX,
    };
    if e.is_dir != want_dir {
        return u64::MAX;
    }
    if e.is_dir && !exfat_dir_is_empty(e.first_cluster) {
        return u64::MAX;
    }
    let loc = match exfat_dir_locate(parent.first_cluster, name) {
        Some(l) => l,
        None => return u64::MAX,
    };
    // 先摘名字 (此后对象不可达), 再释放簇。
    if !exfat_dir_del_set(loc) {
        return u64::MAX;
    }
    if e.first_cluster >= 2 {
        let n = exfat_entry_clusters(&e);
        if !exfat_free_chain(e.first_cluster, e.no_fat_chain, n) {
            return u64::MAX;
        }
    }
    1
}

/// 写文件区间 `[offset, offset+count)`; 需要时扩展簇链并更新 entry set。
fn exfat_write_file(
    e: &ExfatEntry,
    parent_first: u32,
    name: &[u8],
    offset: u32,
    count: u32,
    src: *const u8,
) -> Option<u64> {
    if e.is_dir || count == 0 {
        return Some(0);
    }
    let cb = unsafe { EXFAT_CLUSTER_BYTES };
    if cb == 0 {
        return None;
    }
    let end = offset as u64 + count as u64;
    let new_size = end.max(e.size);
    let need = new_size.div_ceil(cb as u64) as u32;
    let have = exfat_entry_clusters(e);
    let (first, no_chain, n) = exfat_grow_to(e.first_cluster, e.no_fat_chain, have, need)?;
    // 新增簇内容未定义, 先清零 (exFAT 无稀疏文件)。
    let mut idx = have.min(n);
    while idx < n {
        let cl = exfat_chain_nth(first, idx)?;
        let z = exfat_clu();
        zero_bytes(z, cb as usize);
        if !exfat_write_cluster(cl, z) {
            return None;
        }
        idx += 1;
    }
    let mut done = 0u32;
    let scratch = exfat_clu();
    while done < count {
        let pos = offset as u64 + done as u64;
        let cl = exfat_chain_nth(first, (pos / cb as u64) as u32)?;
        let boff = (pos % cb as u64) as usize;
        let chunk = (cb as usize - boff).min((count - done) as usize);
        if !exfat_read_cluster(cl, scratch) {
            return None;
        }
        unsafe {
            core::ptr::copy_nonoverlapping(src.add(done as usize), exfat_atm(scratch, boff), chunk);
        }
        if !exfat_write_cluster(cl, scratch) {
            return None;
        }
        done += chunk as u32;
    }
    let mut e2 = *e;
    e2.first_cluster = first;
    e2.no_fat_chain = no_chain;
    e2.size = new_size;
    e2.valid_size = e.valid_size.max(end).min(new_size);
    e2.mtime = mfs_now();
    if !exfat_rewrite_set(parent_first, name, &e2) {
        return None;
    }
    Some(count as u64)
}

/// 截断 / 扩展文件到 `size` 字节 (exFAT 无稀疏文件: 扩展会实际分配并清零簇)。
fn exfat_truncate(e: &ExfatEntry, parent_first: u32, name: &[u8], size: u32) -> Option<u64> {
    if e.is_dir {
        return None;
    }
    let cb = unsafe { EXFAT_CLUSTER_BYTES } as u64;
    if cb == 0 {
        return None;
    }
    let need = (size as u64).div_ceil(cb) as u32;
    let have = exfat_entry_clusters(e);
    // 截到 0: 释放整条链 (可能还是连续文件, 按 `have` 个簇处理)。
    if need == 0 {
        if !exfat_free_chain(e.first_cluster, e.no_fat_chain, have) {
            return None;
        }
        let mut e2 = *e;
        e2.first_cluster = 0;
        e2.no_fat_chain = false;
        e2.size = 0;
        e2.valid_size = 0;
        e2.mtime = mfs_now();
        return if exfat_rewrite_set(parent_first, name, &e2) {
            Some(0)
        } else {
            None
        };
    }
    let (first, no_chain, n) = exfat_grow_to(e.first_cluster, e.no_fat_chain, have, need)?;
    // 扩展: 新簇清零。
    let mut idx = have.min(n);
    while idx < n {
        let cl = exfat_chain_nth(first, idx)?;
        let z = exfat_clu();
        zero_bytes(z, cb as usize);
        if !exfat_write_cluster(cl, z) {
            return None;
        }
        idx += 1;
    }
    // 截短: 断链并释放尾部多余的簇。
    if n > need {
        let tail = exfat_chain_nth(first, need - 1)?;
        let next = exfat_fat_get(tail)?;
        if !exfat_fat_set(tail, EXFAT_FAT_EOC) {
            return None;
        }
        if next >= 2 && !exfat_is_eoc(next) && !exfat_free_chain(next, false, 0) {
            return None;
        }
    }
    let mut e2 = *e;
    e2.first_cluster = first;
    e2.no_fat_chain = no_chain;
    e2.size = size as u64;
    e2.valid_size = e.valid_size.min(size as u64);
    e2.mtime = mfs_now();
    if !exfat_rewrite_set(parent_first, name, &e2) {
        return None;
    }
    Some(size as u64)
}

/// 由 fd 保存的路径取出 (父目录簇, 名字长度); 名字拷进 `out`。写路径共用。
fn exfat_fd_parent(fd: &ExfatFd, out: &mut [u8; vfs::DIR_LONG_MAX]) -> Option<(u32, usize)> {
    let plen = fd.path_len as usize;
    if plen == 0 || plen > TMP_PATH_MAX {
        return None;
    }
    let path = unsafe { core::str::from_utf8_unchecked(&fd.path[..plen]) };
    let (pdir, name) = exfat_split_parent(path)?;
    if name.len() > vfs::DIR_LONG_MAX {
        return None;
    }
    let parent = exfat_resolve(pdir)?;
    if !parent.is_dir {
        return None;
    }
    out[..name.len()].copy_from_slice(name);
    Some((parent.first_cluster, name.len()))
}
// ===========================================================================
// 域 13 — exFAT 读写文件服务 (exfat_srv)
// ===========================================================================
// 阶段 D/M6a: 挂载宿主 `mkfs.exfat` 预格式化的 exFAT 卷 (U 盘/分区) 的只读兼容。
// 服务**不**自动格式化 —— 与 ext2 同: 定位是读写别人已有的卷, 而非自建。
//
// 已实现的读取子集:
//   - 引导扇区 (sector 0) + 备份 (sector 12) 的 boot checksum 校验;
//   - FAT 链表 (每簇一个 u32; `0` = 空闲, `>= 0xFFFFFFF8` = 链尾);
//   - 集群堆映射 (cluster → LBA) 与目录 entry set 解析;
//   - 分配位图 (0x81) / upcase 表 (0x82) 整体载入内存;
//   - 文件读取 (含 `NoFatChain` 连续文件) 与目录遍历。
//
// 边界 (M6a): 只读; 只支持 `BytesPerSectorShift == 9` (512B 扇区, mkfs.exfat 与
// 常见 U 盘均如此), 更大扇区在挂载时拒绝; 位图 / upcase 表必须能整体装入缓存
// (本卷尺寸下足够)。M6b 再加分配位图分配与 entry set 增删。

/// 卷号回退值: 5 对应 `build/exfat.img` (namespace 5, 见 Makefile)。
const EXFAT_VOL_FALLBACK: u64 = 5;
/// exFAT 服务实际使用的卷号, 启动时由 `vol_claim` 认领。
static mut EXFAT_VOL: u64 = EXFAT_VOL_FALLBACK;

/// exFAT **集群缓冲**虚拟地址 (紧跟 MFS 的 `+0x11_0000..0x11_4000` 之后)。
///
/// 与其它块缓冲页同理: 必须位于程序镜像之外, 且以「同地址」共享给 block_srv
/// 供其 DMA 读写, 否则 NVMe 会回「非法字段」。
/// 尺寸按实际簇大小**动态分配** (`spc` 页), 上限 `EXFAT_MAX_CLUSTER_PAGES`
/// (256 KiB 簇); 因此后续固定窗口从该上限之上开始排布, 避免重叠。
const EXFAT_CLU_VADDR: u64 = 0x0000_0080_0011_4000;
/// 集群缓冲页数上限 (对应 `spc_shift <= 9`, 即 256 KiB 簇)。
const EXFAT_MAX_CLUSTER_PAGES: usize = 64;
/// 分配位图窗口 (一页 = 一个 512B 扇区 + 余量, 按需读入、按扇区回写)。
///
/// 位图不再整体载入内存 —— 大容量卷的位图可达数十 KB 甚至 MB。
const EXFAT_BMP_VADDR: u64 = 0x0000_0080_0015_4000;
/// upcase 表窗口 (一页, 按需读入; 只在生成 NameHash 时用到)。
const EXFAT_UPC_VADDR: u64 = 0x0000_0080_0015_5000;
/// 通用单页暂存: FAT 表项读改写、引导扇区、卷表扫描。
const EXFAT_PG_VADDR: u64 = 0x0000_0080_0015_6000;
/// 02b-2 写背缓存: 暂存窗 16 页 `+0x24_0000` + 描述符 1 页 `+0x25_0000` (同址共享给 block_srv)。
const EXFAT_WB_VADDR: u64 = 0x0000_0080_0024_0000;
const EXFAT_WB_PAGES: usize = 16;
const EXFAT_WB_DESC_VADDR: u64 = 0x0000_0080_0025_0000;

fn exfat_clu() -> *mut u8 {
    EXFAT_CLU_VADDR as *mut u8
}
fn exfat_bmp() -> *mut u8 {
    EXFAT_BMP_VADDR as *mut u8
}
fn exfat_upc() -> *mut u8 {
    EXFAT_UPC_VADDR as *mut u8
}
fn exfat_pg() -> *mut u8 {
    EXFAT_PG_VADDR as *mut u8
}
fn exfat_at(buf: *const u8, off: usize) -> *const u8 {
    unsafe { buf.add(off) }
}
fn exfat_atm(buf: *mut u8, off: usize) -> *mut u8 {
    unsafe { buf.add(off) }
}

/// 目录项类型 (高位置 1 = 在用; 清位即「已删除」)。
const EXFAT_TYPE_UNUSED: u8 = 0x00; // 目录结尾
const EXFAT_TYPE_BITMAP: u8 = 0x81;
const EXFAT_TYPE_UPCASE: u8 = 0x82;
const EXFAT_TYPE_LABEL: u8 = 0x83;
const EXFAT_TYPE_FILE: u8 = 0x85;
const EXFAT_TYPE_STREAM: u8 = 0xC0;
const EXFAT_TYPE_NAME: u8 = 0xC1;
/// `FileAttributes` 的目录位。
const EXFAT_ATTR_DIR: u16 = 0x0010;
/// `GeneralSecondaryFlags` 的 `NoFatChain` 位 (1 = 该文件连续, 忽略 FAT 链)。
const EXFAT_SF_NOFATCHAIN: u8 = 0x02;
/// FAT 值: `0` = 空闲; `>= EXFAT_FAT_EOC_MIN` = 链尾。
const EXFAT_FAT_FREE: u32 = 0;
const EXFAT_FAT_EOC_MIN: u32 = 0xFFFF_FFF8;

/// 每个目录项 32 字节。
const EXFAT_DIR_ENTRY: usize = 32;
/// 打开文件上限。
const EXFAT_MAX_FD: usize = 16;
/// 仅支持 512 字节扇区 (`BytesPerSectorShift == 9`)。
const EXFAT_SECTOR_SIZE: u32 = 512;
/// 簇大小上限对应的 `SectorsPerClusterShift` (256 KiB 簇, 受集群缓冲页数约束)。
const EXFAT_MAX_SPC_SHIFT: u8 = 9;
/// 页大小 (逐页分配 / 共享的粒度)。
const EXFAT_PAGE_SIZE: usize = 4096;

// 挂载后固定的卷参数 (内存镜像)。
static mut EXFAT_SECTORS_PER_CLUSTER: u32 = 0;
static mut EXFAT_CLUSTER_BYTES: u32 = 0;
static mut EXFAT_FAT_OFFSET: u32 = 0; // 单位: 扇区
static mut EXFAT_FAT_LENGTH: u32 = 0; // 单位: 扇区
static mut EXFAT_HEAP_OFFSET: u32 = 0; // 单位: 扇区
static mut EXFAT_CLUSTER_COUNT: u32 = 0;
static mut EXFAT_ROOT_CLUSTER: u32 = 0;
static mut EXFAT_NUM_FATS: u32 = 1;
static mut EXFAT_BITMAP_CLUSTER: u32 = 0;
static mut EXFAT_BITMAP_BYTES: u32 = 0;
static mut EXFAT_UPCASE_CLUSTER: u32 = 0;
static mut EXFAT_UPCASE_BYTES: u32 = 0;

/// 目录 entry set 解析结果 (一个文件/目录的描述)。
#[derive(Clone, Copy)]
struct ExfatEntry {
    is_dir: bool,
    no_fat_chain: bool,
    first_cluster: u32,
    size: u64,
    valid_size: u64,
    mtime: u64,
    name_len: u8,
    name: [u8; vfs::DIR_LONG_MAX],
}
impl ExfatEntry {
    const EMPTY: ExfatEntry = ExfatEntry {
        is_dir: false,
        no_fat_chain: false,
        first_cluster: 0,
        size: 0,
        valid_size: 0,
        mtime: 0,
        name_len: 0,
        name: [0; vfs::DIR_LONG_MAX],
    };
}

/// 打开文件描述符: 记住规范化路径, 每次操作重新解析 (元数据不会变陈旧)。
#[derive(Clone, Copy)]
struct ExfatFd {
    used: bool,
    is_dir: bool,
    path_len: u8,
    path: [u8; TMP_PATH_MAX],
    /// 打开时绑定的卷号 (M1b 多卷挂载)。
    vol: u64,
}
const EXFAT_FD_EMPTY: ExfatFd = ExfatFd {
    used: false,
    is_dir: false,
    path_len: 0,
    path: [0; TMP_PATH_MAX],
    vol: 0,
};
static mut EXFAT_FDS: [ExfatFd; EXFAT_MAX_FD] = [EXFAT_FD_EMPTY; EXFAT_MAX_FD];

/// 本服务**当前请求**落在的卷号 (M1b 多卷挂载; 见 fat32_srv 的 `FAT_CUR_VOL` 注释)。
static mut EXFAT_CUR_VOL: u64 = 0;

/// 已解析的几何 (引导区 / FAT / 集群堆 / 位图 / upcase) 属于哪个卷。各卷的簇大小与
/// 各部分偏移都不同, 请求落到别的卷上必须重新挂载解析 (见 `exfat_mount`)。
static mut EXFAT_GEO_VOL: u64 = u64::MAX;
/// 集群缓冲**已分配并共享**的页数 (卷切换只需补分配差额, 见 `exfat_bufs_init`)。
static mut EXFAT_BUFS_PAGES: usize = 0;

// ---------------------------------------------------------------------------
// 块 I/O / FAT / 集群
// ---------------------------------------------------------------------------

/// 经 block_srv 读 `count` 个 512B 扇区到 `dst`。
fn exfat_read_sectors(lba: u32, count: u16, dst: *mut u8) -> bool {
    block_read_dev(unsafe { EXFAT_CUR_VOL }, lba, count, dst)
}

/// 集群 `cl` 的首个 512B 扇区号 (集群 2 是堆内第一簇)。
fn exfat_cluster_lba(cl: u32) -> u32 {
    unsafe { EXFAT_HEAP_OFFSET + (cl - 2) * EXFAT_SECTORS_PER_CLUSTER }
}

/// 读一个完整集群到 `dst` (`dst` 必须至少有 `spc` 页)。
fn exfat_read_cluster(cl: u32, dst: *mut u8) -> bool {
    if cl < 2 {
        return false;
    }
    let spc = unsafe { EXFAT_SECTORS_PER_CLUSTER };
    if spc == 0 || spc > u16::MAX as u32 {
        return false;
    }
    exfat_read_sectors(exfat_cluster_lba(cl), spc as u16, dst)
}

/// 读集群 `cl` 的 FAT 表项。FAT 表项 4 字节对齐, 必落在单个 512B 扇区内。
fn exfat_fat_get(cl: u32) -> Option<u32> {
    let byte = unsafe { EXFAT_FAT_OFFSET } as u64 * EXFAT_SECTOR_SIZE as u64 + cl as u64 * 4;
    let lba = (byte / EXFAT_SECTOR_SIZE as u64) as u32;
    let off = (byte % EXFAT_SECTOR_SIZE as u64) as usize;
    let buf = exfat_pg();
    if !exfat_read_sectors(lba, 1, buf) {
        return None;
    }
    Some(read_u32(exfat_at(buf, off)))
}

fn exfat_is_eoc(v: u32) -> bool {
    v >= EXFAT_FAT_EOC_MIN
}

/// 分配位图里集群 `cl` 是否已占用 (按需读入所在扇区)。
fn exfat_bitmap_get(cl: u32) -> bool {
    match exfat_bitmap_byte(cl) {
        Some(p) => unsafe { *p & (1u8 << ((cl - 2) & 7)) != 0 },
        None => false,
    }
}

/// 定位位图中集群 `cl` 对应的那个字节所在扇区, 返回窗口内的字节指针。
///
/// 窗口是**写回缓存**: 一旦装载了新扇区, 之前的脏扇区会先落盘。
fn exfat_bitmap_byte(cl: u32) -> Option<*mut u8> {
    if cl < 2 {
        return None;
    }
    let byte = (cl - 2) as usize / 8;
    if byte >= unsafe { EXFAT_BITMAP_BYTES } as usize {
        return None;
    }
    let sec = byte / EXFAT_SECTOR_SIZE as usize;
    let off = byte % EXFAT_SECTOR_SIZE as usize;
    if unsafe { EXFAT_BMP_WIN_SEC } != sec {
        if !exfat_bitmap_flush() {
            return None;
        }
        let lba = exfat_chain_sec_lba(unsafe { EXFAT_BITMAP_CLUSTER }, sec)?;
        if !exfat_read_sectors(lba, 1, exfat_bmp()) {
            return None;
        }
        unsafe {
            EXFAT_BMP_WIN_SEC = sec;
            EXFAT_BMP_WIN_LBA = lba;
        }
    }
    Some(exfat_atm(exfat_bmp(), off))
}

/// 只改内存位图 (1 = 占用), 标脏由 `exfat_bitmap_flush` 落盘。
fn exfat_bitmap_put(cl: u32, used: bool) -> bool {
    let mask = 1u8 << ((cl - 2) & 7);
    let p = match exfat_bitmap_byte(cl) {
        Some(p) => p,
        None => return false,
    };
    unsafe {
        if used {
            *p |= mask;
        } else {
            *p &= !mask;
        }
        EXFAT_BMP_DIRTY = true;
    }
    true
}

/// 把位图窗口里的脏扇区写回 (无脏数据时是空操作)。
fn exfat_bitmap_flush() -> bool {
    if !unsafe { EXFAT_BMP_DIRTY } {
        return true;
    }
    let lba = unsafe { EXFAT_BMP_WIN_LBA };
    if !exfat_write_sectors(lba, 1, exfat_bmp()) {
        return false;
    }
    unsafe { EXFAT_BMP_DIRTY = false };
    true
}

/// 文件逻辑簇号 `idx` 对应的物理簇 (连续文件直接相加, 否则沿 FAT 链走)。
fn exfat_cluster_at(e: &ExfatEntry, idx: u32) -> Option<u32> {
    if e.no_fat_chain {
        let cl = e.first_cluster.checked_add(idx)?;
        if cl < 2 || cl >= unsafe { EXFAT_CLUSTER_COUNT } + 2 {
            return None;
        }
        return Some(cl);
    }
    let mut cl = e.first_cluster;
    let mut i = 0u32;
    while i < idx {
        let next = exfat_fat_get(cl)?;
        if next == EXFAT_FAT_FREE || exfat_is_eoc(next) {
            return None;
        }
        cl = next;
        i += 1;
    }
    if cl < 2 {
        return None;
    }
    Some(cl)
}

// ---------------------------------------------------------------------------
// 引导扇区 / 目录项
// ---------------------------------------------------------------------------

/// boot checksum: 主引导区前 11 个扇区的滚动 32 位校验, 跳过 `VolumeFlags`
/// (106/107) 与 `PercentInUse` (112); 结果重复填入第 11 扇区。
fn exfat_verify_boot_checksum() -> bool {
    // 前 11 个 512B 扇区 = 5632 字节; 逐扇区读进单页暂存后滚动累加,
    // 不依赖「一次读多页」, 也不需要额外的连续两页缓冲。
    let buf = exfat_pg();
    let mut sum: u32 = 0;
    let mut sec: usize = 0;
    while sec < 11 {
        if !exfat_read_sectors(sec as u32, 1, buf) {
            return false;
        }
        let base = sec * EXFAT_SECTOR_SIZE as usize;
        let mut i = 0usize;
        while i < EXFAT_SECTOR_SIZE as usize {
            let off = base + i;
            if off != 106 && off != 107 && off != 112 {
                let byte = unsafe { *exfat_at(buf, i) } as u32;
                let rot: u32 = if sum & 1 != 0 { 0x8000_0000 } else { 0 };
                sum = rot.wrapping_add(sum >> 1).wrapping_add(byte);
            }
            i += 1;
        }
        sec += 1;
    }
    if !exfat_read_sectors(11, 1, buf) {
        return false;
    }
    read_u32(buf) == sum
}

/// entry set 的 16 位校验和: 覆盖整组 (count 字节), 跳过 SetChecksum 字段本身
/// (首项第 2/3 字节)。
fn exfat_set_checksum(e: *const u8, count: u32) -> u16 {
    let mut sum: u16 = 0;
    let mut i = 0u32;
    while i < count {
        if i != 2 && i != 3 {
            let byte = unsafe { *e.add(i as usize) } as u16;
            let rot: u16 = if sum & 1 != 0 { 0x8000 } else { 0 };
            sum = rot.wrapping_add(sum >> 1).wrapping_add(byte);
        }
        i += 1;
    }
    sum
}

/// UTF-16 码元 → UTF-8 (仅基本多文种平面; 代理对不处理)。
fn exfat_utf16_to_utf8(c: u16, out: &mut [u8; 3]) -> usize {
    if c < 0x80 {
        out[0] = c as u8;
        1
    } else if c < 0x800 {
        out[0] = 0xC0 | (c >> 6) as u8;
        out[1] = 0x80 | (c & 0x3F) as u8;
        2
    } else {
        out[0] = 0xE0 | (c >> 12) as u8;
        out[1] = 0x80 | ((c >> 6) & 0x3F) as u8;
        out[2] = 0x80 | (c & 0x3F) as u8;
        3
    }
}

/// 从首个 File Name 条目起收集 `units` 个 UTF-16 码元并转成 UTF-8。
///
/// `first` 是首个 0xC1 条目在 `buf` 中的**条目下标**; 每 15 个字符占一个条目。
fn exfat_read_name(
    buf: *const u8,
    first: usize,
    units: usize,
    out: &mut [u8; vfs::DIR_LONG_MAX],
) -> u8 {
    let mut n = 0usize;
    let mut k = 0usize;
    while k < units {
        let ent = k / 15;
        let pos = k % 15;
        let e = exfat_at(buf, (first + ent) * EXFAT_DIR_ENTRY);
        let c = read_u16(exfat_at(e, 2 + pos * 2));
        let mut tmp = [0u8; 3];
        let l = exfat_utf16_to_utf8(c, &mut tmp);
        let mut j = 0usize;
        while j < l {
            if n >= vfs::DIR_LONG_MAX {
                return n as u8;
            }
            out[n] = tmp[j];
            n += 1;
            j += 1;
        }
        k += 1;
    }
    n as u8
}

/// 解析 `first_index` 处的一组文件 entry set (0x85 + 0xC0 + N×0xC1)。
///
/// 校验 SetChecksum; 不完整 / 校验失败 / 跨簇边界一律返回 None (跳过该组)。
fn exfat_parse_file_set(
    buf: *const u8,
    first_index: usize,
    per: usize,
    sec_count: usize,
) -> Option<ExfatEntry> {
    if sec_count < 2 {
        return None;
    }
    let total = 1 + sec_count;
    if first_index + total > per {
        return None; // entry set 不跨簇边界
    }
    let file = exfat_at(buf, first_index * EXFAT_DIR_ENTRY);
    let stream = exfat_at(buf, (first_index + 1) * EXFAT_DIR_ENTRY);
    if unsafe { *stream } != EXFAT_TYPE_STREAM {
        return None;
    }
    let units = unsafe { *exfat_at(stream, 3) } as usize;
    if units == 0 || units > 255 {
        return None;
    }
    let name_entries = units.div_ceil(15);
    if 2 + name_entries > total {
        return None;
    }
    let stored = read_u16(exfat_at(file, 2));
    if stored != exfat_set_checksum(file, (total * EXFAT_DIR_ENTRY) as u32) {
        return None;
    }
    let mut e = ExfatEntry::EMPTY;
    let attrs = read_u16(exfat_at(file, 4));
    e.is_dir = attrs & EXFAT_ATTR_DIR != 0;
    let flags = unsafe { *exfat_at(stream, 1) };
    e.no_fat_chain = flags & EXFAT_SF_NOFATCHAIN != 0;
    e.first_cluster = read_u32(exfat_at(stream, 20));
    e.valid_size = read_u64(exfat_at(stream, 8));
    e.size = read_u64(exfat_at(stream, 24));
    e.mtime = exfat_decode_time(read_u32(exfat_at(file, 12)), unsafe { *exfat_at(file, 21) });
    let mut long = [0u8; vfs::DIR_LONG_MAX];
    e.name_len = exfat_read_name(buf, first_index + 2, units, &mut long);
    e.name = long;
    if e.name_len == 0 {
        return None;
    }
    Some(e)
}

/// exFAT 打包时间戳 → Unix 秒 (忽略 UTC 偏移字段, 按 UTC 处理)。
///
/// 位域: `[4:0]` 2 秒计数 / `[10:5]` 分 / `[15:11]` 时 / `[20:16]` 日 /
/// `[24:21]` 月 / `[31:25]` 年 - 1980; `ten_ms` 是 0~199 的 10ms 增量。
fn exfat_decode_time(ts: u32, ten_ms: u8) -> u64 {
    if ts == 0 {
        return 0;
    }
    let sec = (ts & 0x1F) as i64 * 2;
    let min = ((ts >> 5) & 0x3F) as i64;
    let hour = ((ts >> 11) & 0x1F) as i64;
    let day = ((ts >> 16) & 0x1F) as i64;
    let mon = ((ts >> 21) & 0x0F) as i64;
    let year = 1980 + ((ts >> 25) & 0x7F) as i64;
    if day == 0 || mon == 0 {
        return 0;
    }
    let secs = days_from_civil(year, mon, day) * 86_400
        + hour * 3600
        + min * 60
        + sec
        + (ten_ms as i64) / 100;
    if secs < 0 {
        0
    } else {
        secs as u64
    }
}

/// 遍历目录 `dir_first` 的所有文件 entry set; 回调返回 false 表示提前停止。
///
/// 目录本身是 FAT 链; 逐簇读取, 遇 `0x00` 条目即目录结束。
fn exfat_dir_scan<F: FnMut(&ExfatEntry) -> bool>(dir_first: u32, mut cb: F) -> bool {
    let cb_size = unsafe { EXFAT_CLUSTER_BYTES } as usize;
    if cb_size < EXFAT_DIR_ENTRY {
        return false;
    }
    let per = cb_size / EXFAT_DIR_ENTRY;
    let buf = exfat_clu();
    let mut cl = dir_first;
    let mut guard = 0u32;
    while cl >= 2 && guard <= unsafe { EXFAT_CLUSTER_COUNT } + 1 {
        if !exfat_read_cluster(cl, buf) {
            return false;
        }
        let mut i = 0usize;
        while i < per {
            let t = unsafe { *exfat_at(buf, i * EXFAT_DIR_ENTRY) };
            if t == EXFAT_TYPE_UNUSED {
                // `0x00` = 本簇剩余条目未使用; 目录链的后续簇仍要扫描
                // (否则「本簇放不下整组条目而留白」会遮住后面簇里的条目)。
                break;
            }
            if t == EXFAT_TYPE_FILE {
                let sec = unsafe { *exfat_at(buf, i * EXFAT_DIR_ENTRY + 1) } as usize;
                if let Some(e) = exfat_parse_file_set(buf, i, per, sec) {
                    if !cb(&e) {
                        return true;
                    }
                }
                i += 1 + sec;
                continue;
            }
            i += 1;
        }
        let next = match exfat_fat_get(cl) {
            Some(v) => v,
            None => return false,
        };
        if next == EXFAT_FAT_FREE || exfat_is_eoc(next) {
            break;
        }
        cl = next;
        guard += 1;
    }
    true
}

/// 名字比较; `ci = true` 按 ASCII 大小写不敏感。
fn exfat_name_eq(a: &[u8], b: &[u8], ci: bool) -> bool {
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

/// 在目录 `dir_first` 中按名查找 (大小写不敏感)。
fn exfat_dir_lookup(dir_first: u32, want: &[u8]) -> Option<ExfatEntry> {
    let mut found: Option<ExfatEntry> = None;
    exfat_dir_scan(dir_first, |e| {
        if exfat_name_eq(&e.name[..e.name_len as usize], want, true) {
            found = Some(*e);
            false
        } else {
            true
        }
    });
    found
}

/// 把 entry set 转成 VFS 目录条目 (长名 + 8.3 后备 + 大小 / 时间)。
fn exfat_make_direntry(e: &ExfatEntry) -> vfs::DirEntry {
    let name = &e.name[..e.name_len as usize];
    let short = ext2_short_name(name);
    let mut long = [0u8; vfs::DIR_LONG_MAX];
    let llen = ext2_copy_long(name, &mut long);
    let size = if e.is_dir { 0 } else { e.size };
    let mut de = vfs::DirEntry::with_long(short, long, llen, size, u32::from(e.is_dir));
    de.mtime = e.mtime;
    de
}

// ---------------------------------------------------------------------------
// 路径解析 / 数据读取
// ---------------------------------------------------------------------------

/// 把绝对路径解析为 entry (根目录是特例: 没有 entry set, 直接返回根簇)。
fn exfat_resolve(path: &str) -> Option<ExfatEntry> {
    let bytes = path.as_bytes();
    if bytes.is_empty() || bytes[0] != b'/' {
        return None;
    }
    let mut cur = ExfatEntry::EMPTY;
    cur.is_dir = true;
    cur.first_cluster = unsafe { EXFAT_ROOT_CLUSTER };
    let mut i = 1usize;
    while i < bytes.len() {
        let start = i;
        while i < bytes.len() && bytes[i] != b'/' {
            i += 1;
        }
        let comp = &bytes[start..i];
        let had_sep = i < bytes.len();
        if had_sep {
            i += 1;
        }
        if comp.is_empty() || comp == b"." {
            continue;
        }
        if !cur.is_dir {
            return None;
        }
        cur = exfat_dir_lookup(cur.first_cluster, comp)?;
        if had_sep && !cur.is_dir {
            return None; // 中间分量必须是目录
        }
    }
    Some(cur)
}

/// 读文件区间 `[offset, offset+count)` 到 `dst`, 返回实际读取字节数。
///
/// 超过 `ValidDataLength` 的已分配区段按 0 读 (exFAT 的「有效数据长度」语义)。
fn exfat_read_file(e: &ExfatEntry, offset: u32, count: u32, dst: *mut u8) -> Option<u64> {
    if e.is_dir {
        return None;
    }
    if offset as u64 >= e.size {
        return Some(0);
    }
    let n = (count as u64).min(e.size - offset as u64) as u32;
    let cb = unsafe { EXFAT_CLUSTER_BYTES };
    if cb == 0 {
        return None;
    }
    let buf = exfat_clu();
    let mut done = 0u32;
    while done < n {
        let pos = offset + done;
        let boff = (pos % cb) as usize;
        let chunk = (cb as usize - boff).min((n - done) as usize);
        let cl = exfat_cluster_at(e, pos / cb).unwrap_or(0);
        if pos as u64 >= e.valid_size || cl < 2 {
            zero_bytes(unsafe { dst.add(done as usize) }, chunk);
        } else {
            if !exfat_read_cluster(cl, buf) {
                return None;
            }
            unsafe {
                core::ptr::copy_nonoverlapping(exfat_at(buf, boff), dst.add(done as usize), chunk);
            }
        }
        done += chunk as u32;
    }
    Some(n as u64)
}

/// 列出目录条目 (跳过系统项与卷标), 返回写入字节数。
fn exfat_readdir(dir_first: u32, dst: *mut vfs::DirEntry) -> Option<u64> {
    let entry_size = core::mem::size_of::<vfs::DirEntry>();
    let mut count = 0usize;
    let ok = exfat_dir_scan(dir_first, |e| {
        if count >= vfs::RESULT_MAX_ENTRIES {
            return false;
        }
        let de = exfat_make_direntry(e);
        unsafe {
            core::ptr::write_unaligned(dst.add(count), de);
        }
        count += 1;
        true
    });
    if !ok {
        return None;
    }
    Some((count * entry_size) as u64)
}

// ---------------------------------------------------------------------------
// 写路径 (M6b)
// ---------------------------------------------------------------------------
//
// 设计要点:
//   * 我们创建的文件/目录一律用 **FAT 链** (不置 `NoFatChain`); 改写既有的
//     连续文件时先把它转成 FAT 链 (补齐簇间链接), 之后只有一种寻簇方式。
//   * 顺序保证「不留下悬空引用」: 创建 = 先备好簇与数据, 最后写目录项;
//     删除 = 先摘目录项 (文件即刻不可达), 再释放簇。
//   * 位图与 FAT 是两份持久状态: 改位图后立即 `exfat_bitmap_flush`,
//     FAT 表项改动逐项落盘 (有第二份 FAT 时同步镜像)。

/// FAT 链尾标记 (mkfs.exfat 写成全 1)。
const EXFAT_FAT_EOC: u32 = 0xFFFF_FFFF;
/// `FileAttributes` 的归档位 (普通文件)。
const EXFAT_ATTR_ARCHIVE: u16 = 0x0020;
/// `GeneralSecondaryFlags` 的 `AllocationPossible` 位 (1 = 允许含分配)。
const EXFAT_SF_ALLOC: u8 = 0x01;
/// 目录尾部空位的填充值: 「已删除的良性次级项」(InUse=0, 非 0)。
///
/// exFAT 要求未使用项 (`0x00`) 之后不得再出现非 0 项, 故目录扩容时
/// 必须把链尾簇的空位「占掉」, 不能留 `0x00`。
const EXFAT_FILLER: u8 = 0x20;
/// entry set 缓冲上限: `0x85` + `0xC0` + 17 个 `0xC1` (255 / 15)。
const EXFAT_SET_MAX_ENTRIES: usize = 19;
const EXFAT_SET_MAX_BYTES: usize = EXFAT_SET_MAX_ENTRIES * EXFAT_DIR_ENTRY;
/// 每个 `0xC1` 承载的 UTF-16 码元数。
const EXFAT_NAME_UNITS_PER_ENTRY: usize = 15;
/// 分配游标: 从上次分配处继续找空闲簇, 避免每次都从第 2 簇线性扫描。
static mut EXFAT_ALLOC_HINT: u32 = 2;

/// 经 block_srv 写 `count` 个 512B 扇区 (源需页对齐; 超过块层单命令上限时自动切分)。
fn exfat_write_sectors(lba: u32, count: u16, src: *mut u8) -> bool {
    if count == 0 {
        return false;
    }
    block_write_dev(unsafe { EXFAT_CUR_VOL }, lba, count, src)
}

/// 写一个完整集群 (`src` 必须至少有 `spc` 页)。
fn exfat_write_cluster(cl: u32, src: *mut u8) -> bool {
    if cl < 2 {
        return false;
    }
    let spc = unsafe { EXFAT_SECTORS_PER_CLUSTER };
    if spc == 0 || spc > u16::MAX as u32 {
        return false;
    }
    exfat_write_sectors(exfat_cluster_lba(cl), spc as u16, src)
}

/// 写集群 `cl` 的 FAT 表项 (读-改-写所在扇区); 有第二份 FAT 时同步镜像。
///
/// FAT 表项 4 字节对齐, 必落在单个 512B 扇区内, 故每次只动一个扇区。
fn exfat_fat_set(cl: u32, val: u32) -> bool {
    let mut idx = 0u32;
    loop {
        let byte = (unsafe { EXFAT_FAT_OFFSET } as u64
            + idx as u64 * unsafe { EXFAT_FAT_LENGTH } as u64)
            * EXFAT_SECTOR_SIZE as u64
            + cl as u64 * 4;
        let lba = (byte / EXFAT_SECTOR_SIZE as u64) as u32;
        let off = (byte % EXFAT_SECTOR_SIZE as u64) as usize;
        let buf = exfat_pg();
        if !exfat_read_sectors(lba, 1, buf) {
            return false;
        }
        write_u32(exfat_atm(buf, off), val);
        if !exfat_write_sectors(lba, 1, buf) {
            return false;
        }
        idx += 1;
        if idx >= unsafe { EXFAT_NUM_FATS } {
            return true;
        }
    }
}

/// 分配一个空闲集群: 置位图 + FAT 置链尾 + 落盘 (位图与 FAT 保持一致)。
fn exfat_alloc_cluster() -> Option<u32> {
    let count = unsafe { EXFAT_CLUSTER_COUNT };
    if count == 0 {
        return None;
    }
    let start = unsafe { EXFAT_ALLOC_HINT };
    let mut i = 0u32;
    while i < count {
        let cl = 2 + ((start - 2 + i) % count);
        if !exfat_bitmap_get(cl) {
            if !exfat_bitmap_put(cl, true) || !exfat_fat_set(cl, EXFAT_FAT_EOC) {
                return None;
            }
            if !exfat_bitmap_flush() {
                return None;
            }
            unsafe {
                EXFAT_ALLOC_HINT = if cl + 1 < count + 2 { cl + 1 } else { 2 };
            }
            return Some(cl);
        }
        i += 1;
    }
    None
}

/// 释放整条簇链: 清位图 + FAT 归零; `no_fat_chain` 时按 `nclusters` 个连续簇处理。
fn exfat_free_chain(first: u32, no_fat_chain: bool, nclusters: u32) -> bool {
    if first < 2 {
        return true; // 无簇 (空文件)
    }
    let mut cl = first;
    let mut i = 0u32;
    let guard_max = unsafe { EXFAT_CLUSTER_COUNT } + 1;
    loop {
        if cl < 2 || i > guard_max {
            return false;
        }
        if !exfat_bitmap_put(cl, false) {
            return false;
        }
        if no_fat_chain {
            i += 1;
            if i >= nclusters {
                break;
            }
            cl += 1;
            continue;
        }
        let next = match exfat_fat_get(cl) {
            Some(v) => v,
            None => return false,
        };
        if !exfat_fat_set(cl, EXFAT_FAT_FREE) {
            return false;
        }
        if next == EXFAT_FAT_FREE || exfat_is_eoc(next) {
            break;
        }
        cl = next;
        i += 1;
    }
    exfat_bitmap_flush()
}

/// 沿 FAT 链走到第 `idx` 个簇 (0 基)。
fn exfat_chain_nth(first: u32, idx: u32) -> Option<u32> {
    if first < 2 {
        return None;
    }
    let mut cl = first;
    let mut i = 0u32;
    while i < idx {
        let next = exfat_fat_get(cl)?;
        if next == EXFAT_FAT_FREE || exfat_is_eoc(next) {
            return None;
        }
        cl = next;
        i += 1;
        if i > unsafe { EXFAT_CLUSTER_COUNT } + 1 {
            return None;
        }
    }
    Some(cl)
}

/// 把链扩展到至少 `needed` 个簇; 返回 (首簇, no_fat_chain, 现有簇数)。
///
/// 既有的连续 (`NoFatChain`) 文件会先补齐簇间 FAT 链接转成链式 —— 之后
/// 只有一种寻簇方式, 读写路径不必再分叉。
fn exfat_grow_to(
    first: u32,
    no_fat_chain: bool,
    have: u32,
    needed: u32,
) -> Option<(u32, bool, u32)> {
    if needed == 0 {
        return Some((0, false, 0));
    }
    let mut first = first;
    let mut have = have;
    if first < 2 || have == 0 {
        first = exfat_alloc_cluster()?;
        have = 1;
    }
    if no_fat_chain {
        let mut i = 0u32;
        while i + 1 < have {
            if !exfat_fat_set(first + i, first + i + 1) {
                return None;
            }
            i += 1;
        }
        if !exfat_fat_set(first + have - 1, EXFAT_FAT_EOC) {
            return None;
        }
    }
    // 从链尾继续分配。
    let mut tail = first;
    let mut n = 1u32;
    loop {
        let next = exfat_fat_get(tail)?;
        if next == EXFAT_FAT_FREE || exfat_is_eoc(next) {
            break;
        }
        tail = next;
        n += 1;
        if n > unsafe { EXFAT_CLUSTER_COUNT } + 1 {
            return None;
        }
    }
    while n < needed {
        let nc = exfat_alloc_cluster()?;
        if !exfat_fat_set(tail, nc) {
            return None;
        }
        tail = nc;
        n += 1;
    }
    Some((first, false, n))
}

/// 当前已分配簇数 (`no_fat_chain` 的连续文件由大小推算)。
fn exfat_entry_clusters(e: &ExfatEntry) -> u32 {
    let cb = unsafe { EXFAT_CLUSTER_BYTES } as u64;
    if cb == 0 {
        return 0;
    }
    if e.no_fat_chain {
        e.size.div_ceil(cb) as u32
    } else {
        let mut cl = e.first_cluster;
        let mut n = 0u32;
        let mut guard = 0u32;
        while cl >= 2 && guard <= unsafe { EXFAT_CLUSTER_COUNT } + 1 {
            n += 1;
            match exfat_fat_get(cl) {
                Some(v) if v != EXFAT_FAT_FREE && !exfat_is_eoc(v) => cl = v,
                _ => break,
            }
            guard += 1;
        }
        n
    }
}

// ---------------------------------------------------------------------------
// fd 表
// ---------------------------------------------------------------------------

fn exfat_fd_alloc(path: &str, is_dir: bool, vol: u64) -> u64 {
    if path.len() > TMP_PATH_MAX {
        return u64::MAX;
    }
    for i in 0..EXFAT_MAX_FD {
        unsafe {
            let s = &mut *core::ptr::addr_of_mut!(EXFAT_FDS).cast::<ExfatFd>().add(i);
            if !s.used {
                s.used = true;
                s.is_dir = is_dir;
                s.path_len = path.len() as u8;
                s.path = [0; TMP_PATH_MAX];
                s.path[..path.len()].copy_from_slice(path.as_bytes());
                s.vol = vol;
                return i as u64;
            }
        }
    }
    u64::MAX
}
/// 查 fd 并把「当前卷寄存器」切到该 fd 绑定的卷 (与路径类请求的 tag 卷编码等价)。
fn exfat_fd_get(fd: u32) -> Option<ExfatFd> {
    if fd as usize >= EXFAT_MAX_FD {
        return None;
    }
    unsafe {
        let s = &*core::ptr::addr_of!(EXFAT_FDS)
            .cast::<ExfatFd>()
            .add(fd as usize);
        if s.used {
            EXFAT_CUR_VOL = s.vol;
            Some(*s)
        } else {
            None
        }
    }
}
fn exfat_fd_free(fd: u32) -> u64 {
    if fd as usize >= EXFAT_MAX_FD {
        return 0;
    }
    unsafe {
        let s = &mut *core::ptr::addr_of_mut!(EXFAT_FDS)
            .cast::<ExfatFd>()
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

/// 扫根目录的**系统项**: 记录分配位图 (0x81) / upcase 表 (0x82) 的簇与长度。
fn exfat_scan_system_entries() -> bool {
    let cb = unsafe { EXFAT_CLUSTER_BYTES } as usize;
    if cb < EXFAT_DIR_ENTRY {
        return false;
    }
    let per = cb / EXFAT_DIR_ENTRY;
    let buf = exfat_clu();
    let mut cl = unsafe { EXFAT_ROOT_CLUSTER };
    let mut guard = 0u32;
    let mut bitmap_found = false;
    let mut upcase_found = false;
    while cl >= 2 && guard <= unsafe { EXFAT_CLUSTER_COUNT } + 1 {
        if !exfat_read_cluster(cl, buf) {
            return false;
        }
        let mut i = 0usize;
        while i < per {
            let e = exfat_at(buf, i * EXFAT_DIR_ENTRY);
            let t = unsafe { *e };
            if t == EXFAT_TYPE_UNUSED {
                return bitmap_found && upcase_found;
            }
            if t & 0x80 != 0 {
                let first = read_u32(exfat_at(e, 20));
                let len = read_u64(exfat_at(e, 24));
                match t {
                    EXFAT_TYPE_BITMAP => {
                        // 位图按需读取 (只缓存一个扇区), 故只校验存在性与覆盖面。
                        if first < 2 || len == 0 || len * 8 < unsafe { EXFAT_CLUSTER_COUNT } as u64
                        {
                            return false;
                        }
                        unsafe {
                            EXFAT_BITMAP_CLUSTER = first;
                            EXFAT_BITMAP_BYTES = len as u32;
                        }
                        bitmap_found = true;
                    }
                    EXFAT_TYPE_UPCASE => {
                        if first < 2 || len == 0 {
                            return false;
                        }
                        unsafe {
                            EXFAT_UPCASE_CLUSTER = first;
                            EXFAT_UPCASE_BYTES = len as u32;
                        }
                        upcase_found = true;
                    }
                    EXFAT_TYPE_LABEL => {} // 卷标可选, 不参与挂载判定
                    _ => {}
                }
            }
            i += 1;
        }
        let next = match exfat_fat_get(cl) {
            Some(v) => v,
            None => return false,
        };
        if next == EXFAT_FAT_FREE || exfat_is_eoc(next) {
            break;
        }
        cl = next;
        guard += 1;
    }
    bitmap_found && upcase_found
}

/// 挂载: 解析引导扇区 + 校验 boot checksum + 读取位图/upcase 表。
///
/// 返回 0 表示成功; 非 0 是失败阶段编号 (供诊断打印定位)。
fn exfat_mount() -> u32 {
    let a = exfat_pg();
    if !exfat_read_sectors(0, 1, a) {
        return 1;
    }
    for (i, &c) in b"EXFAT   ".iter().enumerate() {
        if unsafe { *exfat_at(a, 3 + i) } != c {
            return 2;
        }
    }
    if read_u16(exfat_at(a, 510)) != 0xAA55 {
        return 3;
    }
    // 只支持 512B 扇区: 扇区更大时引导区 / 扇区换算都要另一套逻辑。
    if unsafe { *exfat_at(a, 108) } != 9 {
        return 4;
    }
    let spc_shift = unsafe { *exfat_at(a, 109) };
    if spc_shift == 0 || spc_shift > EXFAT_MAX_SPC_SHIFT {
        // 簇上限 = 集群缓冲页数 (256 KiB); 更大簇的卷当前不支持。
        return 5;
    }
    let num_fats = unsafe { *exfat_at(a, 110) } as u32;
    if num_fats == 0 || num_fats > 2 {
        return 6;
    }
    let fat_off = read_u32(exfat_at(a, 80));
    let fat_len = read_u32(exfat_at(a, 84));
    let heap_off = read_u32(exfat_at(a, 88));
    let clu_count = read_u32(exfat_at(a, 92));
    let root = read_u32(exfat_at(a, 96));
    if fat_len == 0 || clu_count == 0 || root < 2 || root >= clu_count + 2 {
        return 7;
    }
    unsafe {
        EXFAT_SECTORS_PER_CLUSTER = 1u32 << spc_shift;
        EXFAT_CLUSTER_BYTES = EXFAT_SECTORS_PER_CLUSTER * EXFAT_SECTOR_SIZE;
        EXFAT_FAT_OFFSET = fat_off;
        EXFAT_FAT_LENGTH = fat_len;
        EXFAT_HEAP_OFFSET = heap_off;
        EXFAT_CLUSTER_COUNT = clu_count;
        EXFAT_ROOT_CLUSTER = root;
        EXFAT_NUM_FATS = num_fats;
        // 分配游标是**每卷**状态: 换卷时必须复位, 否则会指到新卷的簇范围之外。
        EXFAT_ALLOC_HINT = 2;
    }
    // 簇大小已知, 现在按需分配集群缓冲 (簇字节数 / 页大小 页, 至少一页)。
    let clu_bytes = (1u32 << spc_shift) * EXFAT_SECTOR_SIZE;
    let pages = (clu_bytes as usize).div_ceil(EXFAT_PAGE_SIZE).max(1);
    if !exfat_bufs_init(pages) {
        return 13;
    }
    if !exfat_verify_boot_checksum() {
        return 8;
    }
    if !exfat_scan_system_entries() {
        return 9;
    }
    // 根目录所在簇必须被位图标为占用 —— 同时对「按需位图」做一次端到端校验。
    if !exfat_bitmap_get(root) {
        return 12;
    }
    // 记录「当前几何属于哪个卷」(M1b: 卷切换时据此判断要不要重新解析)。
    unsafe {
        EXFAT_GEO_VOL = EXFAT_CUR_VOL;
    }
    0
}

/// 按簇大小分配并共享 exFAT 的集群缓冲 (逐页 alloc + 同地址 share)。
///
/// 已经分配过的页**不能**再 alloc/share 一次 —— 同地址重复共享会让 block_srv 侧触发
/// 内核 `map_user_page: PageAlreadyMapped` panic。故卷切换 (M1b) 需要更大簇时, 只补
/// 分配「多出来的那几页」。
fn exfat_bufs_init(spc_pages: usize) -> bool {
    if spc_pages == 0 || spc_pages > EXFAT_MAX_CLUSTER_PAGES {
        return false;
    }
    let mut i = unsafe { EXFAT_BUFS_PAGES };
    while i < spc_pages {
        let va = EXFAT_CLU_VADDR + (i * EXFAT_PAGE_SIZE) as u64;
        if sys_alloc_page(va) != 1 || sys_share_page(va, BLOCK_DOMAIN) != 1 {
            return false;
        }
        i += 1;
    }
    unsafe {
        EXFAT_BUFS_PAGES = spc_pages.max(EXFAT_BUFS_PAGES);
    }
    true
}

/// 域 13 — exFAT 服务主循环 (M6a 只读 + M6b 读写)。
pub fn run() {
    // 固定缓冲: 单页暂存 + 位图窗口 + upcase 窗口。集群缓冲按实际簇大小
    // 在 `exfat_mount` 里动态分配 (逐页 alloc + 同地址 share 给 block_srv;
    // 漏了共享 NVMe 会直接回「非法字段」而写入静默失败)。
    for b in [exfat_pg(), exfat_bmp(), exfat_upc()] {
        if sys_alloc_page(b as u64) != 1 || sys_share_page(b as u64, BLOCK_DOMAIN) != 1 {
            println("exfat: alloc/share block buffers FAILED");
            return;
        }
    }
    // 02b-2 写背缓存: 暂存窗 + 描述符页 (同址共享给 block_srv), 使连续小块写攒批下发。
    let mut w = 0usize;
    while w < EXFAT_WB_PAGES {
        let p = EXFAT_WB_VADDR + (w as u64) * 4096;
        if sys_alloc_page(p) != 1 || sys_share_page(p, BLOCK_DOMAIN) != 1 {
            println("exfat: alloc/share write-back buffer FAILED");
            return;
        }
        w += 1;
    }
    if sys_alloc_page(EXFAT_WB_DESC_VADDR) != 1
        || sys_share_page(EXFAT_WB_DESC_VADDR, BLOCK_DOMAIN) != 1
    {
        println("exfat: alloc/share write-back descriptor FAILED");
        return;
    }
    block_wb_enable(EXFAT_WB_VADDR, EXFAT_WB_PAGES, EXFAT_WB_DESC_VADDR);
    // 认领卷: 第一个 exFAT 签名的卷; 无分区表的整盘镜像即卷 5 (回退值)。
    unsafe {
        EXFAT_VOL = vol_claim(exfat_pg(), 16, VOL_KIND_EXFAT, EXFAT_VOL_FALLBACK);
        EXFAT_CUR_VOL = EXFAT_VOL;
    }
    let stage = exfat_mount();
    if stage != 0 {
        print("exfat: mount FAILED vol=");
        print_u64(unsafe { EXFAT_VOL });
        print(" stage=");
        print_u64(stage as u64);
        println("");
        return;
    }
    print("exfat-dbg: vol=");
    print_u64(unsafe { EXFAT_VOL });
    print(" cluster=");
    print_u64(unsafe { EXFAT_CLUSTER_BYTES } as u64);
    print(" clusters=");
    print_u64(unsafe { EXFAT_CLUSTER_COUNT } as u64);
    print(" root=");
    print_u64(unsafe { EXFAT_ROOT_CLUSTER } as u64);
    print(" bitmap=");
    print_u64(unsafe { EXFAT_BITMAP_BYTES } as u64);
    print(" upcase=");
    print_u64(unsafe { EXFAT_UPCASE_BYTES } as u64);
    println("");

    // M1b: 把**额外**的 exFAT 卷挂到 `/usb<卷号>` (元数据已解析完, `exfat_pg` 可作暂存)。
    mount_extra_volumes(
        exfat_pg(),
        VOL_KIND_EXFAT,
        unsafe { EXFAT_VOL },
        vfs::EXFAT_DOMAIN,
    );

    let mut msg = Message {
        from: 0,
        to: 0,
        tag: 0,
        payload: [0; PAYLOAD_LEN],
    };
    loop {
        sys_recv_msg(&mut msg as *mut Message as *mut u8);
        // 02b-2: 每个请求处理前把上一轮攒下的写落盘 (避免暂存写跨请求滞留太久)。
        block_wb_flush();
        // 同 fat32_srv: tag 高位带卷编码 (M1b); fd 类请求的卷由 fd 绑定决定。
        let tag = vfs::tag_body(msg.tag);
        let mut vol = vfs::vol_from_enc(vfs::tag_vol(msg.tag), unsafe { EXFAT_VOL });
        if matches!(
            tag,
            vfs::VFS_READ_TAG | vfs::VFS_WRITE_TAG | vfs::VFS_READDIR_TAG | vfs::VFS_TRUNCATE_TAG
        ) {
            if let Some(fd) = exfat_fd_get(read_u32(msg.payload.as_ptr())) {
                vol = fd.vol;
            }
        }
        unsafe {
            EXFAT_CUR_VOL = vol;
        }
        // 卷切换: 各 exFAT 卷的簇大小 / 区域偏移都不同, 必须按该卷重新解析 (会顺带把
        // 几何、集群缓冲、位图 / upcase 视图都切过去)。
        if unsafe { EXFAT_GEO_VOL } != vol && exfat_mount() != 0 {
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
                let fd = match exfat_resolve(path) {
                    Some(e) => exfat_fd_alloc(path, e.is_dir, vol),
                    None => u64::MAX,
                };
                sys_reply(fd);
            }
            vfs::VFS_READ_TAG => {
                let req: vfs::ReadReq = unsafe {
                    core::ptr::read_unaligned(msg.payload.as_ptr() as *const vfs::ReadReq)
                };
                // exFAT 侧的内部偏移/计数仍是 32 位: 协议 offset 超出 u32 直接失败。
                if req.offset > u32::MAX as u64 {
                    sys_reply(u64::MAX);
                    continue;
                }
                let n = match exfat_fd_get(req.fd) {
                    Some(fd) if !fd.is_dir => {
                        let path = unsafe {
                            core::str::from_utf8_unchecked(&fd.path[..fd.path_len as usize])
                        };
                        match exfat_resolve(path) {
                            Some(e) => exfat_read_file(
                                &e,
                                req.offset as u32,
                                req.count,
                                req.buf as *mut u8,
                            )
                            .unwrap_or(u64::MAX),
                            None => u64::MAX,
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
                let n = match exfat_fd_get(req.fd) {
                    Some(fd) if fd.is_dir => {
                        let path = unsafe {
                            core::str::from_utf8_unchecked(&fd.path[..fd.path_len as usize])
                        };
                        match exfat_resolve(path) {
                            Some(e) => {
                                exfat_readdir(e.first_cluster, req.buf as *mut vfs::DirEntry)
                                    .unwrap_or(u64::MAX)
                            }
                            None => u64::MAX,
                        }
                    }
                    _ => u64::MAX,
                };
                sys_reply(n);
            }
            vfs::VFS_STAT_TAG => {
                let (buf, path) = parse_path_req(msg.payload.as_ptr());
                let n = match exfat_resolve(path) {
                    Some(e) => {
                        let st = vfs::Stat {
                            size: if e.is_dir { 0 } else { e.size },
                            is_dir: u32::from(e.is_dir),
                            mode: if e.is_dir { 0o755 } else { 0o644 },
                            owner: 0,
                            uid: 0,
                            gid: 0,
                            nlink: 1,
                            mtime: e.mtime,
                            ctime: e.mtime,
                            atime: e.mtime,
                        };
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
                sys_reply(exfat_fd_free(fd));
            }
            vfs::VFS_CREAT_TAG | vfs::VFS_MKDIR_TAG => {
                let is_dir = tag == vfs::VFS_MKDIR_TAG;
                let len = msg
                    .payload
                    .iter()
                    .position(|&b| b == 0)
                    .unwrap_or(PAYLOAD_LEN);
                let path = unsafe { core::str::from_utf8_unchecked(&msg.payload[..len]) };
                sys_reply(exfat_create(path, is_dir, vol));
            }
            vfs::VFS_UNLINK_TAG | vfs::VFS_RMDIR_TAG => {
                let want_dir = tag == vfs::VFS_RMDIR_TAG;
                let len = msg
                    .payload
                    .iter()
                    .position(|&b| b == 0)
                    .unwrap_or(PAYLOAD_LEN);
                let path = unsafe { core::str::from_utf8_unchecked(&msg.payload[..len]) };
                sys_reply(exfat_remove(path, want_dir));
            }
            vfs::VFS_WRITE_TAG => {
                let req: vfs::WriteReq = unsafe {
                    core::ptr::read_unaligned(msg.payload.as_ptr() as *const vfs::WriteReq)
                };
                if req.offset > u32::MAX as u64 {
                    sys_reply(u64::MAX);
                    continue;
                }
                let n = match exfat_fd_get(req.fd) {
                    Some(fd) if !fd.is_dir => {
                        let mut nm = [0u8; vfs::DIR_LONG_MAX];
                        match exfat_fd_parent(&fd, &mut nm) {
                            Some((parent_first, nlen)) => {
                                let mut p = [0u8; TMP_PATH_MAX];
                                let plen = fd.path_len as usize;
                                p[..plen].copy_from_slice(&fd.path[..plen]);
                                let path = unsafe { core::str::from_utf8_unchecked(&p[..plen]) };
                                match exfat_resolve(path) {
                                    Some(e) => exfat_write_file(
                                        &e,
                                        parent_first,
                                        &nm[..nlen],
                                        req.offset as u32,
                                        req.count,
                                        req.buf as *const u8,
                                    )
                                    .unwrap_or(u64::MAX),
                                    None => u64::MAX,
                                }
                            }
                            None => u64::MAX,
                        }
                    }
                    _ => u64::MAX,
                };
                sys_reply(n);
            }
            vfs::VFS_TRUNCATE_TAG => {
                let req: vfs::TruncateReq = unsafe {
                    core::ptr::read_unaligned(msg.payload.as_ptr() as *const vfs::TruncateReq)
                };
                if req.size > u32::MAX as u64 {
                    sys_reply(u64::MAX);
                    continue;
                }
                let n = match exfat_fd_get(req.fd) {
                    Some(fd) if !fd.is_dir => {
                        let mut nm = [0u8; vfs::DIR_LONG_MAX];
                        match exfat_fd_parent(&fd, &mut nm) {
                            Some((parent_first, nlen)) => {
                                let mut p = [0u8; TMP_PATH_MAX];
                                let plen = fd.path_len as usize;
                                p[..plen].copy_from_slice(&fd.path[..plen]);
                                let path = unsafe { core::str::from_utf8_unchecked(&p[..plen]) };
                                match exfat_resolve(path) {
                                    Some(e) => exfat_truncate(
                                        &e,
                                        parent_first,
                                        &nm[..nlen],
                                        req.size as u32,
                                    )
                                    .unwrap_or(u64::MAX),
                                    None => u64::MAX,
                                }
                            }
                            None => u64::MAX,
                        }
                    }
                    _ => u64::MAX,
                };
                sys_reply(n);
            }
            // 其余未实现 tag (rename/chmod/link 等) 一律拒绝。
            _ => {
                sys_reply(u64::MAX);
            }
        }
    }
}
