use crate::common::*;
use morion::syscall::*;
use morion::vfs;

/// fat32_srv 的**默认卷**号, 启动时由 `vol_claim` 认领 (见 `fat32_main`)。
/// 回退值 0 对应 `build/nvme.img` (namespace 1)。
static mut FAT_VOL: u64 = 0;

/// 本服务**当前请求**落在的卷号 (M1b 多卷挂载)。
///
/// 除默认卷 `FAT_VOL` 外, 本服务还会为额外挂载的 FAT 卷 (`/usb<卷号>`) 服务: 路径
/// 类请求的 tag 高位带卷编码, fd 类请求由 fd 里绑定的卷决定 (见 `fd_lookup`)。
/// 分派时把解出的卷写进这个「当前卷寄存器」, 于是所有既有的读写扇区调用无需改签名
/// 就自动落在正确的卷上 —— 服务是**单任务串行**处理请求的, 不存在并发覆盖问题。
static mut FAT_CUR_VOL: u64 = 0;

/// 经 IPC 请求 block_srv 读 `count` 个扇区到 `buf`。成功返回 true。
///
/// fat32_srv 通过它间接访问块设备, 而非直接触碰 IDE 端口; IDE PIO 逻辑
/// 收拢在 block_srv 内, 符合微内核「驱动服务化」的解耦。
fn block_read(lba: u32, count: u16, buf: *mut u8) -> bool {
    block_read_dev(unsafe { FAT_CUR_VOL }, lba, count, buf)
}

/// 经 IPC 请求 block_srv 从 `buf` 写 `count` 个扇区到磁盘。成功返回 true。
fn block_write(lba: u32, count: u16, buf: *mut u8) -> bool {
    block_write_dev(unsafe { FAT_CUR_VOL }, lba, count, buf)
}
// ---------------------------------------------------------------------------
// FAT32 解析与目录遍历
// ---------------------------------------------------------------------------

/// FAT32 BPB 关键布局参数 (从引导扇区解析)。
struct Fat32Bpb {
    bytes_per_sector: u16,
    sectors_per_cluster: u8,
    reserved_sectors: u16,
    num_fats: u8,
    fat_size: u32,      // 单个 FAT 占用的扇区数
    total_sectors: u32, // BPB_TotSec32 (分区总扇区数)
    root_cluster: u32,
}

impl Fat32Bpb {
    /// 从 LBA 0 (引导扇区) 解析 BPB。
    fn parse(sector0: *const u8) -> Self {
        unsafe {
            Fat32Bpb {
                bytes_per_sector: read_u16(sector0.add(11)),
                sectors_per_cluster: *sector0.add(13),
                reserved_sectors: read_u16(sector0.add(14)),
                num_fats: *sector0.add(16),
                fat_size: read_u32(sector0.add(36)),
                total_sectors: read_u32(sector0.add(32)),
                root_cluster: read_u32(sector0.add(44)),
            }
        }
    }

    /// 每簇字节数。
    fn cluster_bytes(&self) -> u32 {
        self.sectors_per_cluster as u32 * self.bytes_per_sector as u32
    }

    /// 第一个 FAT 区的起始扇区。
    fn fat_start_sector(&self) -> u32 {
        self.reserved_sectors as u32
    }

    /// 数据区 (簇 2) 的起始扇区。
    fn data_start_sector(&self) -> u32 {
        self.reserved_sectors as u32 + self.num_fats as u32 * self.fat_size
    }

    /// 簇号 -> 起始扇区号 (簇 2 是数据区第一个簇)。
    fn cluster_to_sector(&self, cluster: u32) -> u32 {
        self.data_start_sector() + (cluster - 2) * self.sectors_per_cluster as u32
    }

    /// 数据区总簇数 (簇号有效范围 2..=total_clusters+1)。
    fn total_clusters(&self) -> u32 {
        let data_sectors = self.total_sectors - self.data_start_sector();
        data_sectors / self.sectors_per_cluster as u32
    }
}

/// 目录项属性位。
const ATTR_DIRECTORY: u8 = 0x10;
const ATTR_VOLUME_ID: u8 = 0x08;
const ATTR_LONG_NAME: u8 = 0x0F;

/// 目录项偏移 12 的 "小写标志" 字节 (NT 字节): 短名字节**恒存大写**, 一个 8.3 名是否
/// 按小写显示由它记录 —— 位 3 = 主名小写, 位 4 = 扩展名小写 (Windows / mtools 都这么写,
/// 也正是它们写小写名的方式)。`readdir` 要按输入原样显示名字, 就得读这个字节。
const NT_CASE_FLAGS_OFF: usize = 12;
const NT_CASE_BASE_LOWER: u8 = 0x08;
const NT_CASE_EXT_LOWER: u8 = 0x10;

/// 读 FAT 表中 `cluster` 指向的下一个簇号 (高 4 位保留, 屏蔽为 28 位)。
fn read_fat_entry(bpb: &Fat32Bpb, cluster: u32, fat_buf: *mut u8) -> u32 {
    let byte_offset = cluster * 4;
    let sector = bpb.fat_start_sector() + byte_offset / 512;
    let index = (byte_offset % 512) / 4;
    block_read(sector, 1, fat_buf);
    read_u32(unsafe { fat_buf.add(index as usize * 4) }) & 0x0FFF_FFFF
}

/// 把簇 `cluster` 的整簇内容读入 `buf` (至少一簇大小)。
fn read_cluster(bpb: &Fat32Bpb, cluster: u32, buf: *mut u8) -> bool {
    let sector = bpb.cluster_to_sector(cluster);
    block_read(sector, bpb.sectors_per_cluster as u16, buf)
}

/// 从文件 (首簇 `start_cluster`, 大小 `file_size`) 的 `offset` 处读最多 `count`
/// 字节到 `out` (须 >= min(count, file_size-offset) 字节)。返回实际读取字节数;
/// 读盘失败返回 u64::MAX。
// 参数较多但都是读文件所需的几何信息与裸缓冲, 拆分结构体会牵动所有调用点。
#[allow(clippy::too_many_arguments)]
fn read_file_range(
    bpb: &Fat32Bpb,
    start_cluster: u32,
    file_size: u32,
    offset: u32,
    count: u32,
    fat_buf: *mut u8,
    file_buf: *mut u8,
    out: *mut u8,
) -> u64 {
    if offset >= file_size {
        return 0;
    }
    let want = core::cmp::min(count, file_size - offset) as usize;
    let cluster_bytes = bpb.cluster_bytes() as usize;
    let mut cluster = start_cluster;
    let mut absolute = 0usize; // 当前簇首字节在文件内的偏移
    let mut copied = 0usize;

    // 1. 跳过 offset 之前的整簇。
    while absolute + cluster_bytes <= offset as usize {
        absolute += cluster_bytes;
        let next = read_fat_entry(bpb, cluster, fat_buf);
        if next >= 0x0FFF_FFF8 {
            return 0;
        }
        cluster = next;
    }

    // 2. 逐簇拷贝与 [offset, offset+want) 重叠的部分。
    while copied < want {
        if !read_cluster(bpb, cluster, file_buf) {
            return u64::MAX;
        }
        let skip = (offset as usize).saturating_sub(absolute);
        let take = core::cmp::min(want - copied, cluster_bytes - skip);
        unsafe {
            core::ptr::copy_nonoverlapping(file_buf.add(skip), out.add(copied), take);
        }
        copied += take;
        absolute += cluster_bytes;

        if copied < want {
            let next = read_fat_entry(bpb, cluster, fat_buf);
            if next >= 0x0FFF_FFF8 {
                break; // FAT 链提前结束: 数据不完整, 返回已读部分
            }
            cluster = next;
        }
    }
    copied as u64
}

// ---------------------------------------------------------------------------
// FAT32 写支持 (覆盖 + 扩展)
// ---------------------------------------------------------------------------

/// FAT32 链结束标记 (28 位)。
const FAT_EOC: u32 = 0x0FFF_FFFF;

/// 把 `cluster` 对应的 FAT 表项写为 `value` (低 28 位), 并同步写所有 FAT 副本。
fn write_fat_entry(bpb: &Fat32Bpb, cluster: u32, value: u32, fat_buf: *mut u8) -> bool {
    let byte_offset = cluster * 4;
    let sector_off = byte_offset / 512;
    let index = (byte_offset % 512) / 4;

    // 读第一个 FAT 的对应扇区, 改 4 字节槽位 (保留高 4 位保留位)。
    if !block_read(bpb.fat_start_sector() + sector_off, 1, fat_buf) {
        return false;
    }
    let slot = unsafe { fat_buf.add(index as usize * 4) };
    let old = read_u32(slot);
    write_u32(slot, (old & 0xF000_0000) | (value & 0x0FFF_FFFF));

    // 同步写所有 FAT 副本。
    for k in 0..bpb.num_fats as u32 {
        let sector = bpb.fat_start_sector() + k * bpb.fat_size + sector_off;
        if !block_write(sector, 1, fat_buf) {
            return false;
        }
    }
    true
}

/// 把整簇内容 `buf` (至少一簇大小) 写入簇 `cluster`。
fn write_cluster(bpb: &Fat32Bpb, cluster: u32, buf: *mut u8) -> bool {
    let sector = bpb.cluster_to_sector(cluster);
    block_write(sector, bpb.sectors_per_cluster as u16, buf)
}

/// fat32 的簇分配游标 (与 exFAT 的 `EXFAT_ALLOC_HINT` 同义)。
///
/// `find_free_cluster` 从它起向后扫描, 找到后推进到下一簇 —— 避免每次分配都从簇 2
/// 重新扫整张 FAT。没有它, 写一个 100000 字节的文件 (512 B 簇 = 196 簇) 会让
/// 分配变成 O(n²) 次 FAT 读, 实测整套自测被拖到 8 分钟以上还跑不完。
static mut FAT_ALLOC_HINT: u32 = 2;

/// 扫描 FAT 表找第一个空闲簇 (表项 == 0), 无空闲返回 None。
/// 从 `FAT_ALLOC_HINT` 起向后扫描, 扫到表尾回绕到 2, 回到起点仍无空闲则返回 None。
fn find_free_cluster(bpb: &Fat32Bpb, fat_buf: *mut u8) -> Option<u32> {
    let total = bpb.total_clusters();
    let start = unsafe { FAT_ALLOC_HINT }.max(2);
    let mut cluster = start;
    loop {
        if read_fat_entry(bpb, cluster, fat_buf) == 0 {
            unsafe {
                FAT_ALLOC_HINT = if cluster + 1 > total + 1 {
                    2
                } else {
                    cluster + 1
                };
            }
            return Some(cluster);
        }
        cluster += 1;
        if cluster > total + 1 {
            cluster = 2;
        }
        if cluster == start {
            return None; // 扫完一圈仍无空闲
        }
    }
}

/// 把目录项 (父目录簇 `dir_cluster`, 簇内字节偏移 `entry_offset`) 的
/// 首簇号与文件大小写回磁盘。
fn update_dir_entry(
    bpb: &Fat32Bpb,
    dir_cluster: u32,
    entry_offset: u32,
    start_cluster: u32,
    file_size: u32,
    dir_buf: *mut u8,
) -> bool {
    let sector = bpb.cluster_to_sector(dir_cluster) + entry_offset / 512;
    let off = (entry_offset % 512) as usize;
    if !block_read(sector, 1, dir_buf) {
        return false;
    }
    let e = unsafe { dir_buf.add(off) };
    write_u16(unsafe { e.add(20) }, (start_cluster >> 16) as u16);
    write_u16(unsafe { e.add(26) }, (start_cluster & 0xFFFF) as u16);
    write_u32(unsafe { e.add(28) }, file_size);
    block_write(sector, 1, dir_buf)
}

/// 释放从 `start_cluster` 开始的簇链: 沿 FAT 链把每个簇表项清零 (标记为空闲),
/// 直到遇到链结束标记或 0。带链长上限, 防止 FAT 损坏导致的死循环。返回是否完整释放。
fn free_cluster_chain(bpb: &Fat32Bpb, start_cluster: u32, fat_buf: *mut u8) -> bool {
    let mut cluster = start_cluster;
    let mut steps = 0u32;
    while cluster != 0 && cluster < 0x0FFF_FFF8 {
        if steps >= 4096 {
            return false; // 链异常过长
        }
        let next = read_fat_entry(bpb, cluster, fat_buf);
        if !write_fat_entry(bpb, cluster, 0, fat_buf) {
            return false;
        }
        cluster = next;
        steps += 1;
    }
    true
}

/// 写一条完整 32 字节目录项到父目录簇 `dir_cluster` 的 `entry_offset` (簇内字节
/// 偏移) 处。写入短名 `sn` (11 字节)、属性 `attr`、首簇号 `start_cluster` 与大小
/// `file_size`; 时间戳字段暂置 0 (FAT 允许 0, 表示未指定)。返回是否成功。
// 参数较多但都是写一条目录项所需的字段与裸缓冲, 拆分结构体会牵动所有调用点。
#[allow(clippy::too_many_arguments)]
fn write_dir_entry(
    bpb: &Fat32Bpb,
    dir_cluster: u32,
    entry_offset: u32,
    sn: &[u8; 11],
    attr: u8,
    start_cluster: u32,
    file_size: u32,
    dir_buf: *mut u8,
) -> bool {
    let sector = bpb.cluster_to_sector(dir_cluster) + entry_offset / 512;
    let off = (entry_offset % 512) as usize;
    if !block_read(sector, 1, dir_buf) {
        return false;
    }
    let e = unsafe { dir_buf.add(off) };
    for (i, &b) in sn.iter().enumerate() {
        unsafe {
            core::ptr::write_volatile(e.add(i), b);
        }
    }
    unsafe {
        core::ptr::write_volatile(e.add(11), attr);
    }
    zero_bytes(unsafe { e.add(12) }, 8); // 保留 + 创建/访问时间戳置 0
    write_u16(unsafe { e.add(20) }, (start_cluster >> 16) as u16);
    zero_bytes(unsafe { e.add(22) }, 4); // 修改时间/日期置 0
    write_u16(unsafe { e.add(26) }, (start_cluster & 0xFFFF) as u16);
    write_u32(unsafe { e.add(28) }, file_size);
    block_write(sector, 1, dir_buf)
}

/// 在目录 `dir_cluster` 中找一个空闲的 32 字节目录项槽位。
/// 优先复用已删除项 (首字节 0xE5), 否则使用空项 (0x00); 若目录簇全满且链已到
/// 末尾, 分配一个新簇挂到链尾并清零, 返回其首个槽位。
/// 返回 (entry_offset, cluster): 槽位在 `cluster` 簇内的字节偏移。
fn find_dir_slot(
    bpb: &Fat32Bpb,
    dir_cluster: u32,
    dir_buf: *mut u8,
    fat_buf: *mut u8,
) -> Option<(u32, u32)> {
    let entries_per_cluster = bpb.cluster_bytes() as usize / 32;
    let mut cluster = dir_cluster;

    loop {
        if !read_cluster(bpb, cluster, dir_buf) {
            return None;
        }
        for i in 0..entries_per_cluster {
            let entry = unsafe { dir_buf.add(i * 32) };
            let first = unsafe { *entry };
            if first == 0xE5 || first == 0x00 {
                return Some(((i * 32) as u32, cluster));
            }
        }
        // 当前簇满, 尝试下一个目录簇。
        let next = read_fat_entry(bpb, cluster, fat_buf);
        if next >= 0x0FFF_FFF8 {
            // 链尾: 分配新簇挂接并清零。
            let free = find_free_cluster(bpb, fat_buf)?;
            if !write_fat_entry(bpb, cluster, free, fat_buf) {
                return None;
            }
            if !write_fat_entry(bpb, free, FAT_EOC, fat_buf) {
                return None;
            }
            zero_bytes(dir_buf, bpb.cluster_bytes() as usize);
            if !write_cluster(bpb, free, dir_buf) {
                return None;
            }
            return Some((0, free));
        }
        cluster = next;
    }
}

/// 删除父目录簇 `dir_cluster` 中 `entry_offset` 处的条目 (纯底层原语, 不区分
/// 文件/目录): 首字节置 0xE5 标记删除, 并释放其簇链。是否允许删目录由上层
/// (unlink / rmdir 服务分支) 决定。返回是否成功。
fn unlink_entry(
    bpb: &Fat32Bpb,
    dir_cluster: u32,
    entry_offset: u32,
    start_cluster: u32,
    dir_buf: *mut u8,
    fat_buf: *mut u8,
) -> bool {
    // 先释放簇链, 再标记删除, 避免失败后残留半删除状态。
    if start_cluster != 0 && !free_cluster_chain(bpb, start_cluster, fat_buf) {
        return false;
    }
    let sector = bpb.cluster_to_sector(dir_cluster) + entry_offset / 512;
    let off = (entry_offset % 512) as usize;
    if !block_read(sector, 1, dir_buf) {
        return false;
    }
    unsafe {
        core::ptr::write_volatile(dir_buf.add(off), 0xE5);
    }
    block_write(sector, 1, dir_buf)
}

/// 从 `src` (至少 `count` 字节) 取数据, 覆盖/扩展文件 `node`, 从 `offset` 起写
/// `count` 字节。必要时分配新簇并更新 FAT 链与目录项。返回写入字节数, 失败返回
/// u64::MAX; 成功时同步更新 `node` 的 start_cluster / file_size。
// 参数较多但都是写文件所需的裸缓冲与几何信息, 拆分结构体会牵动所有调用点。
#[allow(clippy::too_many_arguments)]
fn write_file_range(
    bpb: &Fat32Bpb,
    node: &mut OpenNode,
    offset: u32,
    count: u32,
    fat_buf: *mut u8,
    file_buf: *mut u8,
    dir_buf: *mut u8,
    src: *const u8,
) -> u64 {
    if count == 0 {
        return 0;
    }
    let cluster_bytes = bpb.cluster_bytes();
    let end = offset.saturating_add(count);
    let new_size = node.file_size.max(end);

    // 1. 沿 FAT 链收集现有簇。
    const MAX_CHAIN: usize = 256;
    let mut chain = [0u32; MAX_CHAIN];
    let mut chain_len = 0usize;
    let mut cluster = node.start_cluster;
    while cluster != 0 && cluster < 0x0FFF_FFF8 {
        if chain_len >= MAX_CHAIN {
            return u64::MAX;
        }
        chain[chain_len] = cluster;
        chain_len += 1;
        cluster = read_fat_entry(bpb, cluster, fat_buf);
        if cluster == 0 {
            break; // 链中途损坏, 停止收集
        }
    }
    let old_clusters = chain_len;

    // 2. 计算所需簇数, 不足则分配新簇并挂到链尾。
    let needed = if new_size == 0 {
        0
    } else {
        (new_size as u64).div_ceil(cluster_bytes as u64) as usize
    };
    if needed > MAX_CHAIN {
        return u64::MAX;
    }
    while chain_len < needed {
        let free = match find_free_cluster(bpb, fat_buf) {
            Some(c) => c,
            None => return u64::MAX,
        };
        if chain_len > 0 && !write_fat_entry(bpb, chain[chain_len - 1], free, fat_buf) {
            return u64::MAX;
        }
        if !write_fat_entry(bpb, free, FAT_EOC, fat_buf) {
            return u64::MAX;
        }
        chain[chain_len] = free;
        chain_len += 1;
    }
    let new_start = if needed > 0 { chain[0] } else { 0 };

    // 3. 逐簇写入数据 (覆盖 + 稀疏间隙清零 + 扩展尾清零)。
    for (i, &c) in chain.iter().enumerate().take(needed) {
        let cs = (i as u32) * cluster_bytes;
        let ce = cs + cluster_bytes;

        // 整个簇落在写区间内 → 直接从 src 拷贝整簇。
        if cs >= offset && ce <= end {
            unsafe {
                core::ptr::copy_nonoverlapping(
                    src.add((cs - offset) as usize),
                    file_buf,
                    cluster_bytes as usize,
                );
            }
            if !write_cluster(bpb, c, file_buf) {
                return u64::MAX;
            }
            continue;
        }

        let write_start = cs.max(offset);
        let write_end = ce.min(end);
        let has_write = write_start < write_end;

        // 完全在旧数据区且与写区间无交集 → 保持原样。
        if i < old_clusters && !has_write && ce <= node.file_size {
            continue;
        }

        // 读-改-写: 先取旧内容或清零 (新分配的簇)。
        if i < old_clusters {
            if !read_cluster(bpb, c, file_buf) {
                return u64::MAX;
            }
        } else {
            zero_bytes(file_buf, cluster_bytes as usize);
        }

        // 旧文件末尾与 offset 之间的空隙清零 (稀疏扩展)。
        if offset > node.file_size {
            let gap_start = cs.max(node.file_size);
            let gap_end = ce.min(offset);
            if gap_start < gap_end {
                zero_bytes(
                    unsafe { file_buf.add((gap_start - cs) as usize) },
                    (gap_end - gap_start) as usize,
                );
            }
        }

        // 覆盖写入区间。
        if has_write {
            unsafe {
                core::ptr::copy_nonoverlapping(
                    src.add((write_start - offset) as usize),
                    file_buf.add((write_start - cs) as usize),
                    (write_end - write_start) as usize,
                );
            }
        }

        // 末簇超出 new_size 的部分清零 (仅扩展时)。
        if new_size > node.file_size && i + 1 == needed && new_size < ce {
            zero_bytes(
                unsafe { file_buf.add((new_size - cs) as usize) },
                (ce - new_size) as usize,
            );
        }

        if !write_cluster(bpb, c, file_buf) {
            return u64::MAX;
        }
    }

    // 4. 目录项与节点描述符同步 (首簇号可能因从空文件分配而改变)。
    if (node.start_cluster != new_start || node.file_size != new_size)
        && !update_dir_entry(
            bpb,
            node.dir_cluster,
            node.entry_offset,
            new_start,
            new_size,
            dir_buf,
        )
    {
        return u64::MAX;
    }
    node.start_cluster = new_start;
    node.file_size = new_size;
    count as u64
}

// ---------------------------------------------------------------------------
// VFAT 长文件名 (LFN)
// ---------------------------------------------------------------------------
// 长名由若干个 32 字节的 LFN 目录项 (attr = 0x0F) 承载, 紧挨在对应的短名项之前,
// 每项存 13 个 UTF-16LE 码元, 并在磁盘上**逆序**排列 (逻辑最后一段在前, 该段
// order 字节带 0x40 标志; 紧邻短名项的是逻辑第 1 段)。
//
// LFN 项布局: [0] order(bit6=末段, 低 6 位为 1 起的段号) [1..11] 5 个码元
//             [11] attr=0x0F [12] type=0 [13] 短名校验和
//             [14..26] 6 个码元 [26..28] 首簇=0 [28..32] 2 个码元

/// LFN 最多段数 (255 码元 / 13 每段, 向上取整)。
const MAX_LFN_ENTRIES: usize = 20;
/// 每段 LFN 存的码元数。
const LFN_CHARS_PER_ENTRY: usize = 13;
/// 长名码元上限 (含结尾 0x0000)。
const LFN_MAX_UNITS: usize = MAX_LFN_ENTRIES * LFN_CHARS_PER_ENTRY + 1;

/// VFAT 长名累加器: 按 LFN 项的段号把 UTF-16 码元填回它在长名中的位置。
#[derive(Clone, Copy)]
struct LfnBuf {
    units: [u16; LFN_MAX_UNITS],
    /// 已填充到的最大码元数 (可能包含结尾的 0x0000)。
    filled: usize,
    /// 当前这一段 LFN 组是否可信 (由带 0x40 标志的项开启)。
    active: bool,
    /// 本组 LFN 项记录的短名校验和 (LFN 项偏移 13; 组内每项都相同)。
    cksum: u8,
}

impl LfnBuf {
    const fn new() -> Self {
        LfnBuf {
            units: [0; LFN_MAX_UNITS],
            filled: 0,
            active: false,
            cksum: 0,
        }
    }

    /// 丢弃当前累积的长名 (遇到删除项 / 卷标 / 短名项之后调用)。
    fn reset(&mut self) {
        self.filled = 0;
        self.active = false;
        self.cksum = 0;
    }

    /// 消化一个 LFN 项, 把其中的码元放到长名中的正确位置。
    fn push(&mut self, entry: *const u8) {
        let order = unsafe { *entry };
        let seq = (order & 0x3F) as usize;
        if order & 0x40 != 0 {
            // 带 0x40 的是逻辑最后一段, 也是磁盘上最先出现的一项 → 新的一组开始。
            self.filled = 0;
            self.active = true;
        }
        if !self.active || seq == 0 || seq > MAX_LFN_ENTRIES {
            return;
        }
        // 短名校验和记在 LFN 项自己的偏移 13 上 (不是短名项)。
        self.cksum = unsafe { *entry.add(13) };
        let base = (seq - 1) * LFN_CHARS_PER_ENTRY;
        // 13 个码元在项内分三段: 5 个 (偏移 1) + 6 个 (偏移 14) + 2 个 (偏移 28)。
        let mut k = 0usize;
        for &start in &[1usize, 14, 28] {
            let seg_len = if start == 1 {
                5
            } else if start == 14 {
                6
            } else {
                2
            };
            for c in 0..seg_len {
                let idx = base + k;
                if idx >= LFN_MAX_UNITS {
                    return;
                }
                self.units[idx] = read_u16(unsafe { entry.add(start + c * 2) });
                k += 1;
            }
        }
        let end = base + LFN_CHARS_PER_ENTRY;
        if end > self.filled {
            self.filled = end.min(LFN_MAX_UNITS);
        }
    }

    /// 取有效长名的码元 (截到第一个 0x0000 结尾); 无效时返回空切片。
    fn name(&self) -> &[u16] {
        if !self.active {
            return &[];
        }
        let end = self.units[..self.filled]
            .iter()
            .position(|&u| u == 0)
            .unwrap_or(self.filled);
        &self.units[..end]
    }

    /// 长名所属的短名项校验和是否与本组 LFN 项记录的一致。
    ///
    /// 校验和按 VFAT 规范对 11 字节短名逐字节计算: 先循环右移一位, 再加上该字节
    /// (`sum = rot_right1(sum) + b`, 按 u8 回绕)。不匹配说明这组 LFN 项不属于该
    /// 短名项 (残留 / 损坏), 此时应退回短名而不是沿用错的长名。
    fn checksum_ok(&self, short_entry: *const u8) -> bool {
        let mut sum: u8 = 0;
        for i in 0..11 {
            let b = unsafe { *short_entry.add(i) };
            sum = sum.rotate_right(1).wrapping_add(b);
        }
        sum == self.cksum
    }
}

/// 把 UTF-16 码元转成 UTF-8 并写入 `out`, 最多 `out.len()` 字节 (只截整字符)。
/// 返回写入字节数。
fn utf16_to_utf8(units: &[u16], out: &mut [u8; vfs::DIR_LONG_MAX]) -> usize {
    let mut n = 0usize;
    for &u in units {
        // BMP 之外 (代理对) 的码元按替换字符处理, 控制字符跳过。
        let cp = if (0xD800..0xE000).contains(&u) {
            u32::from('?')
        } else {
            u32::from(u)
        };
        let mut buf = [0u8; 4];
        let encoded = match char::from_u32(cp) {
            Some(c) => c.encode_utf8(&mut buf).len(),
            None => 0,
        };
        if encoded == 0 {
            continue;
        }
        if n + encoded > out.len() {
            break; // 截断: 放不下的字符直接丢弃
        }
        out[n..n + encoded].copy_from_slice(&buf[..encoded]);
        n += encoded;
    }
    n
}

/// 判断长名码元是否与查询名 `query` (ASCII) 相等 (大小写不敏感)。
fn lfn_name_eq(units: &[u16], query: &[u8]) -> bool {
    if units.len() != query.len() {
        return false;
    }
    for (i, &u) in units.iter().enumerate() {
        if u > 0x7F {
            return false; // 非 ASCII 码元无法与 ASCII 查询匹配
        }
        if ascii_upper(u as u8) != ascii_upper(query[i]) {
            return false;
        }
    }
    true
}

/// 把目录 `dir_cluster` 的条目以结构化 `vfs::DirEntry` 数组写入 `out`,
/// 返回写入字节数 (= 条目数 × size_of::<DirEntry>()); 失败返回 u64::MAX。
///
/// VFAT: 逐个拼接短名项之前的 LFN 项, 得到长名 (校验和不符则退回短名);
/// 条目数按结果页容量 `vfs::RESULT_MAX_ENTRIES` 截断, 不会越界写客户端缓冲页。
fn readdir_into(
    bpb: &Fat32Bpb,
    dir_cluster: u32,
    dir_buf: *mut u8,
    fat_buf: *mut u8,
    out: *mut u8,
) -> u64 {
    let entries_per_cluster = bpb.cluster_bytes() as usize / 32;
    let mut cluster = dir_cluster;
    let mut count = 0usize;
    let dst = out as *mut vfs::DirEntry;
    let mut lfn = LfnBuf::new();

    loop {
        if !read_cluster(bpb, cluster, dir_buf) {
            return u64::MAX;
        }
        for i in 0..entries_per_cluster {
            let entry = unsafe { dir_buf.add(i * 32) };
            let first = unsafe { *entry };
            if first == 0x00 {
                return (count * core::mem::size_of::<vfs::DirEntry>()) as u64; // 目录结束
            }
            if first == 0xE5 {
                lfn.reset(); // 已删除: 其 LFN 组也一并作废
                continue;
            }
            let attr = unsafe { *entry.add(11) };
            if attr & ATTR_LONG_NAME == ATTR_LONG_NAME {
                lfn.push(entry); // 长文件名项: 累积, 等短名项到来时使用
                continue;
            }
            if attr & ATTR_VOLUME_ID != 0 {
                lfn.reset(); // 卷标
                continue;
            }

            let is_dir = attr & ATTR_DIRECTORY != 0;
            // "." / ".." 以 '.' 开头, 跳过 (其 LFN 组同样作废)。
            if is_dir && first == b'.' {
                lfn.reset();
                continue;
            }

            // 结果页只有一页: 放不下就停在这里 (继续扫描只会越界写)。
            if count + 1 > vfs::RESULT_MAX_ENTRIES {
                return (count * core::mem::size_of::<vfs::DirEntry>()) as u64;
            }

            let file_size = read_u32(unsafe { entry.add(28) });
            let mut de = vfs::DirEntry::short([0; 11], file_size as u64, is_dir as u32);
            unsafe {
                core::ptr::copy_nonoverlapping(entry, de.name.as_mut_ptr(), 11);
                // 短名字节恒为大写, "是否小写显示" 记在偏移 12 的小写标志字节里。
                let flags = *entry.add(NT_CASE_FLAGS_OFF);
                if flags & NT_CASE_BASE_LOWER != 0 {
                    de.name[..8].make_ascii_lowercase();
                }
                if flags & NT_CASE_EXT_LOWER != 0 {
                    de.name[8..].make_ascii_lowercase();
                }
            }
            // 长名: 仅当 LFN 校验和与这个短名项匹配时才采用 (否则可能张冠李戴)。
            if lfn.active && lfn.checksum_ok(entry) {
                let n = utf16_to_utf8(lfn.name(), &mut de.long);
                de.long_len = n as u8;
            }
            lfn.reset();
            unsafe {
                core::ptr::write_unaligned(dst.add(count), de);
            }
            count += 1;
        }

        // 跨簇: 读下一个目录簇。
        let next = read_fat_entry(bpb, cluster, fat_buf);
        if next >= 0x0FFF_FFF8 {
            return (count * core::mem::size_of::<vfs::DirEntry>()) as u64;
        }
        cluster = next;
    }
}

// ---------------------------------------------------------------------------
// 正斜杠路径解析 (最小闭环: 短名 8.3 查找 + 子目录递归)
// ---------------------------------------------------------------------------

/// 目录项解析结果 (路径查找用)。
struct DirEntryInfo {
    start_cluster: u32,
    file_size: u32,
    attr: u8,
    /// 目录项所在父目录簇 (写回 file_size / start_cluster 用)。
    dir_cluster: u32,
    /// 目录项在父目录簇内的字节偏移 (32 字节对齐)。
    entry_offset: u32,
}

/// ASCII 大写 (仅处理 a-z)。
fn ascii_upper(c: u8) -> u8 {
    if c.is_ascii_lowercase() {
        c - 0x20
    } else {
        c
    }
}

/// 把路径段 (如 "hello.txt" / "dir1") 转成 FAT 8.3 短名 (11 字节, 大写 + 空格填充)。
/// 扩展名按最后一个 '.' 分隔; 主名 > 8 或扩展名 > 3 视为不合法, 返回 None。
fn short_name_from_query(name: &[u8]) -> Option<[u8; 11]> {
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
fn entry_name_matches(entry: *const u8, sn: &[u8; 11]) -> bool {
    unsafe {
        for (i, &b) in sn.iter().enumerate() {
            if ascii_upper(*entry.add(i)) != b {
                return false;
            }
        }
    }
    true
}

/// 从短名项取出首簇 / 大小 / 属性, 组装查找结果。
fn entry_info(entry: *const u8, cluster: u32, i: usize) -> DirEntryInfo {
    let cluster_hi = read_u16(unsafe { entry.add(20) }) as u32;
    let cluster_lo = read_u16(unsafe { entry.add(26) }) as u32;
    DirEntryInfo {
        start_cluster: (cluster_hi << 16) | cluster_lo,
        file_size: read_u32(unsafe { entry.add(28) }),
        attr: unsafe { *entry.add(11) },
        dir_cluster: cluster,
        entry_offset: (i * 32) as u32,
    }
}

/// 按名字在目录 `dir_cluster` 中查找条目, 两种匹配方式共用一次扫描:
///
/// 1. **8.3 短名** (`sn`, 11 字节严格比较) —— 优先, 命中立即返回;
/// 2. **VFAT 长名** (与 `query` 做 ASCII 大小写不敏感比较) —— 只在没有短名命中时
///    采用, 且必须 LFN 校验和与所属短名项一致, 否则可能是残留的孤儿 LFN 项。
///
/// `sn` 为 None 表示调用方给的名字本身就不是合法 8.3 (例如含长名) 只按长名找。
/// 支持跨簇目录; 目录正常结束返回已找到的长名命中 (可能为 None)。
fn find_entry_named(
    bpb: &Fat32Bpb,
    dir_cluster: u32,
    sn: Option<&[u8; 11]>,
    query: &[u8],
    dir_buf: *mut u8,
    fat_buf: *mut u8,
) -> Option<DirEntryInfo> {
    let entries_per_cluster = bpb.cluster_bytes() as usize / 32;
    let mut cluster = dir_cluster;
    let mut lfn = LfnBuf::new();
    // 长名命中先记下, 但继续扫描: 短名命中优先级更高。
    let mut long_hit: Option<DirEntryInfo> = None;

    loop {
        if !read_cluster(bpb, cluster, dir_buf) {
            return long_hit;
        }
        for i in 0..entries_per_cluster {
            let entry = unsafe { dir_buf.add(i * 32) };
            let first = unsafe { *entry };
            if first == 0x00 {
                return long_hit; // 目录结束
            }
            if first == 0xE5 {
                lfn.reset();
                continue;
            }
            let attr = unsafe { *entry.add(11) };
            if attr & ATTR_LONG_NAME == ATTR_LONG_NAME {
                lfn.push(entry);
                continue;
            }
            if attr & ATTR_VOLUME_ID != 0 {
                lfn.reset();
                continue;
            }
            let is_dir = attr & ATTR_DIRECTORY != 0;
            let is_dot = is_dir && first == b'.';
            let long_match =
                !is_dot && lfn.active && lfn.checksum_ok(entry) && lfn_name_eq(lfn.name(), query);
            lfn.reset();

            if let Some(sn) = sn {
                if !is_dot && entry_name_matches(entry, sn) {
                    return Some(entry_info(entry, cluster, i));
                }
            }
            if long_match && long_hit.is_none() {
                long_hit = Some(entry_info(entry, cluster, i));
            }
        }
        // 跨簇: 读下一个目录簇。
        let next = read_fat_entry(bpb, cluster, fat_buf);
        if next >= 0x0FFF_FFF8 {
            return long_hit;
        }
        cluster = next;
    }
}

/// 按名字查找目录条目 (短名优先, 回退到 VFAT 长名)。
fn find_entry(
    bpb: &Fat32Bpb,
    dir_cluster: u32,
    name: &[u8],
    dir_buf: *mut u8,
    fat_buf: *mut u8,
) -> Option<DirEntryInfo> {
    let sn = short_name_from_query(name);
    find_entry_named(bpb, dir_cluster, sn.as_ref(), name, dir_buf, fat_buf)
}

/// 在目录 `dir_cluster` 中按 11 字节短名 `sn` 直接查找条目 (支持跨簇目录)。
/// 命中返回首簇/大小/属性, 未命中或读盘失败返回 None。
///
/// 用于创建/删除等**已知短名**的场景 (这些操作只写 8.3 短名, 不涉及长名)。
fn find_entry_sn(
    bpb: &Fat32Bpb,
    dir_cluster: u32,
    sn: &[u8; 11],
    dir_buf: *mut u8,
    fat_buf: *mut u8,
) -> Option<DirEntryInfo> {
    let entries_per_cluster = bpb.cluster_bytes() as usize / 32;
    let mut cluster = dir_cluster;

    loop {
        if !read_cluster(bpb, cluster, dir_buf) {
            return None;
        }
        for i in 0..entries_per_cluster {
            let entry = unsafe { dir_buf.add(i * 32) };
            let first = unsafe { *entry };
            if first == 0x00 {
                return None; // 目录结束
            }
            if first == 0xE5 {
                continue; // 已删除
            }
            let attr = unsafe { *entry.add(11) };
            if attr & ATTR_LONG_NAME == ATTR_LONG_NAME {
                continue; // 长文件名项
            }
            if attr & ATTR_VOLUME_ID != 0 {
                continue; // 卷标
            }
            if !entry_name_matches(entry, sn) {
                continue;
            }
            return Some(entry_info(entry, cluster, i));
        }
        // 跨簇: 读下一个目录簇。
        let next = read_fat_entry(bpb, cluster, fat_buf);
        if next >= 0x0FFF_FFF8 {
            return None;
        }
        cluster = next;
    }
}

/// 按正斜杠路径 (如 "/dir1/nested.txt") 从根目录解析到最终条目。
/// 忽略空段 (连续 '/' 或前导 '/'), 中间段必须是目录, 末段返回条目。
fn resolve_path(
    bpb: &Fat32Bpb,
    path: &str,
    dir_buf: *mut u8,
    fat_buf: *mut u8,
) -> Option<DirEntryInfo> {
    let bytes = path.as_bytes();
    let mut cur_cluster = bpb.root_cluster;
    let mut i = 0usize;

    while i < bytes.len() {
        let mut j = i;
        while j < bytes.len() && bytes[j] != b'/' {
            j += 1;
        }
        let seg = &bytes[i..j];
        if !seg.is_empty() {
            let info = find_entry(bpb, cur_cluster, seg, dir_buf, fat_buf)?;
            // 判断 seg 之后是否还有非空段。
            let mut k = j;
            while k < bytes.len() && bytes[k] == b'/' {
                k += 1;
            }
            if k >= bytes.len() {
                return Some(info); // 末段
            }
            if info.attr & ATTR_DIRECTORY == 0 {
                return None; // 中间段不是目录
            }
            cur_cluster = info.start_cluster;
        }
        i = j + 1;
    }
    None
}

/// 解析 open 路径: 空路径或单个 "/" 视为根目录, 其余按正斜杠路径解析。
fn resolve_open_path(
    bpb: &Fat32Bpb,
    path: &str,
    dir_buf: *mut u8,
    fat_buf: *mut u8,
) -> Option<DirEntryInfo> {
    let trimmed = path.trim_matches('/');
    if trimmed.is_empty() {
        return Some(DirEntryInfo {
            start_cluster: bpb.root_cluster,
            file_size: 0,
            attr: ATTR_DIRECTORY,
            dir_cluster: bpb.root_cluster,
            entry_offset: 0,
        });
    }
    resolve_path(bpb, path, dir_buf, fat_buf)
}

/// 解析"创建/删除"类路径: 最后一个非空段作为目标名 (转成 8.3 短名), 其余段必须
/// 是已存在的目录。返回 (父目录簇, 目标短名); 空路径 / 单个 "/" / 中间段非目录 /
/// 目标名非法时返回 None。
fn resolve_parent(
    bpb: &Fat32Bpb,
    path: &str,
    dir_buf: *mut u8,
    fat_buf: *mut u8,
) -> Option<(u32, [u8; 11])> {
    let bytes = path.as_bytes();
    let mut cur_cluster = bpb.root_cluster;
    let mut i = 0usize;

    while i < bytes.len() {
        let mut j = i;
        while j < bytes.len() && bytes[j] != b'/' {
            j += 1;
        }
        let seg = &bytes[i..j];

        // 跳过斜杠, 判断 seg 之后是否还有非空段。
        let mut k = j;
        while k < bytes.len() && bytes[k] == b'/' {
            k += 1;
        }

        if !seg.is_empty() {
            if k >= bytes.len() {
                // seg 是最后一个非空段 → 目标名。
                let sn = short_name_from_query(seg)?;
                return Some((cur_cluster, sn));
            }
            // 中间段必须是目录。
            let info = find_entry(bpb, cur_cluster, seg, dir_buf, fat_buf)?;
            if info.attr & ATTR_DIRECTORY == 0 {
                return None;
            }
            cur_cluster = info.start_cluster;
        }
        i = j + 1;
    }
    None
}

/// 判断目录 `dir_cluster` 是否为空 (只含 `.` 与 `..`, 或更少)。跨簇遍历, 跳过
/// 已删除项 / 长文件名 / 卷标; 遇到第 3 个有效条目即非空。读盘失败视为非空 (安全)。
fn dir_is_empty(bpb: &Fat32Bpb, dir_cluster: u32, dir_buf: *mut u8, fat_buf: *mut u8) -> bool {
    let entries_per_cluster = bpb.cluster_bytes() as usize / 32;
    let mut cluster = dir_cluster;
    let mut valid = 0u32;

    loop {
        if !read_cluster(bpb, cluster, dir_buf) {
            return false;
        }
        for i in 0..entries_per_cluster {
            let entry = unsafe { dir_buf.add(i * 32) };
            let first = unsafe { *entry };
            if first == 0x00 {
                return valid <= 2; // 目录结束
            }
            if first == 0xE5 {
                continue; // 已删除
            }
            let attr = unsafe { *entry.add(11) };
            if attr & ATTR_LONG_NAME == ATTR_LONG_NAME {
                continue; // 长文件名项
            }
            if attr & ATTR_VOLUME_ID != 0 {
                continue; // 卷标
            }
            valid += 1;
            if valid > 2 {
                return false; // 第 3 个有效条目 → 非空
            }
        }
        // 跨簇。
        let next = read_fat_entry(bpb, cluster, fat_buf);
        if next >= 0x0FFF_FFF8 {
            return valid <= 2;
        }
        cluster = next;
    }
}
/// fat32_srv 打开节点描述符表 (静态, 单任务独占访问, 无需锁)。
const MAX_FD: usize = 16;

#[derive(Clone, Copy)]
struct OpenNode {
    is_dir: bool,
    start_cluster: u32,
    file_size: u32,
    /// 目录项所在父目录簇 (文件写回用)。
    dir_cluster: u32,
    /// 目录项在父目录簇内的字节偏移 (文件写回用)。
    entry_offset: u32,
    /// 打开时绑定的卷号 (M1b 多卷挂载: 同一服务可同时服务默认卷与额外卷)。
    vol: u64,
}

static mut FD_TABLE: [Option<OpenNode>; MAX_FD] = [None; MAX_FD];

/// 分配一个空闲 fd 槽位, 返回 fd (0..MAX_FD), 表满返回 u64::MAX。
fn fd_alloc(
    is_dir: bool,
    start_cluster: u32,
    file_size: u32,
    dir_cluster: u32,
    entry_offset: u32,
    vol: u64,
) -> u64 {
    unsafe {
        let base = core::ptr::addr_of_mut!(FD_TABLE).cast::<Option<OpenNode>>();
        for i in 0..MAX_FD {
            let slot = base.add(i);
            if (*slot).is_none() {
                *slot = Some(OpenNode {
                    is_dir,
                    start_cluster,
                    file_size,
                    dir_cluster,
                    entry_offset,
                    vol,
                });
                return i as u64;
            }
        }
    }
    u64::MAX
}

/// 查询 fd 对应的节点描述符, 并把「当前卷寄存器」切到该 fd 绑定的卷。
///
/// fd 类请求 (READ/WRITE/READDIR) 的 payload 里没有卷号 —— 卷在 `open` 时就固定
/// 绑到了 fd 上, 这里顺带切换, 使后续读写自动落在同一卷 (与路径类请求的 tag 卷编码
/// 等价, 见 `FAT_CUR_VOL`)。
fn fd_lookup(fd: u32) -> Option<OpenNode> {
    if (fd as usize) >= MAX_FD {
        return None;
    }
    let node = unsafe {
        *core::ptr::addr_of!(FD_TABLE)
            .cast::<Option<OpenNode>>()
            .add(fd as usize)
    };
    if let Some(n) = node {
        unsafe {
            FAT_CUR_VOL = n.vol;
        }
    }
    node
}

/// 更新 fd 对应节点的首簇号与文件大小 (写操作后同步)。
fn fd_update(fd: u32, start_cluster: u32, file_size: u32) {
    if (fd as usize) >= MAX_FD {
        return;
    }
    unsafe {
        let slot = core::ptr::addr_of_mut!(FD_TABLE)
            .cast::<Option<OpenNode>>()
            .add(fd as usize);
        if let Some(mut node) = *slot {
            node.start_cluster = start_cluster;
            node.file_size = file_size;
            *slot = Some(node);
        }
    }
}

/// 释放 fd, 成功返回 1, 失败返回 0。
fn fd_free(fd: u32) -> u64 {
    if (fd as usize) >= MAX_FD {
        return 0;
    }
    unsafe {
        let slot = core::ptr::addr_of_mut!(FD_TABLE)
            .cast::<Option<OpenNode>>()
            .add(fd as usize);
        if (*slot).is_some() {
            *slot = None;
            1
        } else {
            0
        }
    }
}

/// 各卷几何 (根簇 / FAT 起址 / 簇大小) 不同, 这里记住 BPB 当前属于哪个卷。
/// 请求落在别的卷上时重新解析该卷的 BPB (见 `fat_load_bpb`)。
static mut FAT_BPB_VOL: u64 = u64::MAX;

/// fat32_srv 的**整簇缓冲**虚拟地址 (M1b: 大簇支持)。
///
/// 整簇读写 (目录簇扫描 / 文件数据暂存) 都在这里进行, 按 FAT32 的**最大簇**预留:
/// BPB 的 `SecPerClus` 只有 1 字节且必须是 2 的幂, 故簇最大 128 扇区 = 64 KiB。
/// 地址取 2 MiB 偏移处: 避开 1 MiB 处的小缓冲段 (`0x10_xxxx`) 与用户栈 (`0x3F_9000`),
/// 也避开 exFAT 的集群缓冲段 (`0x11_4000..0x15_3FFF`) —— 两个服务都会把自己的缓冲
/// **同地址共享给 block_srv**, 地址撞上会让 block_srv 侧触发内核
/// `map_user_page: PageAlreadyMapped` panic。
const FAT32_CLU_VADDR: u64 = 0x0000_0080_0020_0000;
/// 整簇缓冲页数 (64 KiB 上限簇 = 16 页)。
const FAT32_CLU_PAGES: u64 = 16;
/// FAT32 允许的最大簇字节数 (`SecPerClus` ≤ 128 扇区 × 512 B)。
const FAT32_MAX_CLUSTER_BYTES: u32 = 128 * 512;

/// 载入卷 `vol` 的 BPB 到 `out`, 并把 `FAT_BPB_VOL` 标成 `vol`。
///
/// 调用者须已把 `FAT_CUR_VOL` 指向 `vol`。几何不合法 (非 512B 扇区 / 簇为 0 /
/// 簇超过整簇缓冲) 时返回 false —— 请求按失败回复, 不拿错几何去读盘。
fn fat_load_bpb(vol: u64, bpb_buf: *mut u8, out: &mut Fat32Bpb) -> bool {
    if !block_read(0, 1, bpb_buf) {
        return false;
    }
    // 引导扇区签名 (偏移 510 = 0x55, 511 = 0xAA)。
    if read_u16(unsafe { bpb_buf.add(510) } as *const u8) != 0xAA55 {
        return false;
    }
    let b = Fat32Bpb::parse(bpb_buf as *const u8);
    let cb = b.cluster_bytes();
    if b.bytes_per_sector != 512 || cb == 0 || cb > FAT32_MAX_CLUSTER_BYTES {
        return false;
    }
    *out = b;
    unsafe {
        FAT_BPB_VOL = vol;
        // 换卷后分配游标归位 (各卷簇数不同, 沿用旧卷的 hint 可能越过新卷簇数)。
        FAT_ALLOC_HINT = 2;
    }
    true
}

/// 域 6 — FAT32 文件服务 (fat32_srv): 经 IPC 请求 block_srv 读扇区,
/// 解析 BPB / FAT / 目录 / 路径, 提供 open/read/readdir/close。
pub fn run() {
    // 小缓冲 (各 1 页): BPB / FAT 扇区 —— 放 1 MiB 处, 与 app·shell 的共享页同段。
    let bpb_buf = 0x0000_0080_0010_2000u64;
    let fat_buf = 0x0000_0080_0010_1000u64;
    // 整簇缓冲 (最多 16 页 = 64 KiB 簇): 目录簇扫描与文件数据暂存共用同一块。
    let clu_buf = FAT32_CLU_VADDR;
    let dir_buf = clu_buf;
    let file_buf = clu_buf;

    if sys_alloc_page(bpb_buf) != 1 || sys_alloc_page(fat_buf) != 1 {
        println("fat32: alloc buffer FAILED");
        return;
    }

    // 把缓冲页共享给 block_srv (同地址映射), 使其能直接写入读到的扇区数据。
    if sys_share_page(bpb_buf, BLOCK_DOMAIN) != 1 || sys_share_page(fat_buf, BLOCK_DOMAIN) != 1 {
        println("fat32: share buffer FAILED");
        return;
    }

    // 整簇缓冲逐页分配 + 同地址共享 (M1b: 大簇支持, 见 `FAT32_CLU_VADDR`)。
    for i in 0..FAT32_CLU_PAGES {
        let p = clu_buf + i * 4096;
        if sys_alloc_page(p) != 1 || sys_share_page(p, BLOCK_DOMAIN) != 1 {
            println("fat32: alloc/share cluster buffer FAILED");
            return;
        }
    }

    // 认领卷: 第一个 FAT 签名的卷; 无分区表的整盘镜像即卷 0 (回退值)。
    unsafe {
        FAT_VOL = vol_claim(bpb_buf as *mut u8, 16, VOL_KIND_FAT, 0);
        // 启动期 (读 BPB / FS-1 自测) 的读写都落在默认卷上。
        FAT_CUR_VOL = FAT_VOL;
    }

    // 经 block_srv 读 LBA 0 并解析 BPB。
    if !block_read(0, 1, bpb_buf as *mut u8) {
        println("fat32: read LBA 0 FAILED");
        return;
    }
    // 引导签名校验 (offset 510 = 0x55, 511 = 0xAA)。
    let sig = read_u16((bpb_buf + 510) as *const u8);
    if sig != 0xAA55 {
        println("fat32: not a boot sector");
        return;
    }
    let mut bpb = Fat32Bpb::parse(bpb_buf as *const u8);
    unsafe {
        FAT_BPB_VOL = FAT_VOL;
    }
    // 几何校验: 整簇缓冲按 64 KiB 上限预留, 更大的簇 (以及非 512B 扇区) 直接拒绝,
    // 绝不用「按小块缓冲算出的偏移」去读盘。
    let cbytes = bpb.cluster_bytes();
    if bpb.bytes_per_sector != 512 || cbytes == 0 || cbytes > FAT32_MAX_CLUSTER_BYTES {
        print("fat32: unsupported geometry bps=");
        print_u64(bpb.bytes_per_sector as u64);
        print(" cluster=");
        print_u64(cbytes as u64);
        println("");
        return;
    }

    // FS-1 自测: 验证元数据原语 (find_dir_slot / write_dir_entry / find_entry /
    // unlink_entry / free_cluster_chain) 的建项 → 命中 → 删项 → 释放簇全链路。
    {
        let sn = match short_name_from_query(b"FS1TEST") {
            Some(s) => s,
            None => {
                println("fat32: FS1 self-test invalid short name");
                return;
            }
        };
        let free = match find_free_cluster(&bpb, fat_buf as *mut u8) {
            Some(c) => c,
            None => {
                println("fat32: FS1 self-test no free cluster");
                return;
            }
        };
        if !write_fat_entry(&bpb, free, FAT_EOC, fat_buf as *mut u8) {
            println("fat32: FS1 self-test write FAT failed");
            return;
        }
        let (off, dc) = match find_dir_slot(
            &bpb,
            bpb.root_cluster,
            dir_buf as *mut u8,
            fat_buf as *mut u8,
        ) {
            Some(x) => x,
            None => {
                println("fat32: FS1 self-test find_dir_slot failed");
                return;
            }
        };
        if !write_dir_entry(&bpb, dc, off, &sn, 0, free, 0, dir_buf as *mut u8) {
            println("fat32: FS1 self-test write_dir_entry failed");
            return;
        }
        let found = find_entry(
            &bpb,
            bpb.root_cluster,
            b"FS1TEST",
            dir_buf as *mut u8,
            fat_buf as *mut u8,
        );
        let info = match found {
            Some(i) if i.start_cluster == free => i,
            _ => {
                println("fat32: FS1 self-test find_entry verify FAILED");
                return;
            }
        };
        if !unlink_entry(
            &bpb,
            info.dir_cluster,
            info.entry_offset,
            info.start_cluster,
            dir_buf as *mut u8,
            fat_buf as *mut u8,
        ) {
            println("fat32: FS1 self-test unlink_entry FAILED");
            return;
        }
        if find_entry(
            &bpb,
            bpb.root_cluster,
            b"FS1TEST",
            dir_buf as *mut u8,
            fat_buf as *mut u8,
        )
        .is_some()
        {
            println("fat32: FS1 self-test entry still present FAILED");
            return;
        }
        if read_fat_entry(&bpb, free, fat_buf as *mut u8) != 0 {
            println("fat32: FS1 self-test cluster not freed FAILED");
            return;
        }
    }

    // M1b: 把**额外**的 FAT 卷 (如分区盘上的第二个 FAT 分区、真 U 盘) 挂到 `/usb<卷号>`。
    // 用 `fat_buf` 暂存卷描述符 —— 元数据已解析完毕, 该页此刻是空闲暂存。
    mount_extra_volumes(
        fat_buf as *mut u8,
        VOL_KIND_FAT,
        unsafe { FAT_VOL },
        vfs::FAT32_DOMAIN,
    );

    // 服务循环: 经 IPC 提供 open / read / readdir / close (见 vfs.rs 协议)。
    loop {
        let mut msg = Message {
            from: 0,
            to: 0,
            tag: 0,
            payload: [0; PAYLOAD_LEN],
        };
        sys_recv_msg(&mut msg as *mut Message as *mut u8);

        let tag = vfs::tag_body(msg.tag);
        // tag 高位携带卷编码 (M1b): 路径类请求由它决定目标卷; fd 类请求由 fd 绑定的卷
        // 决定 (fd 是这些请求 payload 的首字段), 先探一次 fd, 使两类请求都对。
        let mut vol = vfs::vol_from_enc(vfs::tag_vol(msg.tag), unsafe { FAT_VOL });
        if matches!(
            tag,
            vfs::VFS_READ_TAG | vfs::VFS_WRITE_TAG | vfs::VFS_READDIR_TAG
        ) {
            if let Some(n) = fd_lookup(read_u32(msg.payload.as_ptr())) {
                vol = n.vol;
            }
        }
        unsafe {
            FAT_CUR_VOL = vol;
        }
        // 卷切换: 各 FAT 卷的根簇 / FAT 起址 / 簇大小都不同, 必须重新解析该卷的 BPB,
        // 否则会拿上一个卷的几何去算扇区号, 读到完全错误的位置。
        if unsafe { FAT_BPB_VOL } != vol && !fat_load_bpb(vol, bpb_buf as *mut u8, &mut bpb) {
            sys_reply(u64::MAX);
            continue;
        }
        match tag {
            vfs::VFS_OPEN_TAG => {
                let path_len = msg
                    .payload
                    .iter()
                    .position(|&b| b == 0)
                    .unwrap_or(PAYLOAD_LEN);
                let path = unsafe { core::str::from_utf8_unchecked(&msg.payload[..path_len]) };
                let fd = match resolve_open_path(&bpb, path, dir_buf as *mut u8, fat_buf as *mut u8)
                {
                    Some(info) => fd_alloc(
                        info.attr & ATTR_DIRECTORY != 0,
                        info.start_cluster,
                        info.file_size,
                        info.dir_cluster,
                        info.entry_offset,
                        vol,
                    ),
                    None => u64::MAX,
                };
                sys_reply(fd);
            }
            vfs::VFS_READ_TAG => {
                let req: vfs::ReadReq = unsafe {
                    core::ptr::read_unaligned(msg.payload.as_ptr() as *const vfs::ReadReq)
                };
                // 协议 offset 是 u64, 但 FAT32 的文件大小字段只有 32 位: 偏移一旦超出
                // u32 就不可能落在合法数据上, 直接判失败 (而不是截断成一个错的偏移)。
                if req.offset > u32::MAX as u64 {
                    sys_reply(u64::MAX);
                    continue;
                }
                let offset = req.offset as u32;
                let n = match fd_lookup(req.fd) {
                    Some(node) if !node.is_dir => read_file_range(
                        &bpb,
                        node.start_cluster,
                        node.file_size,
                        offset,
                        req.count,
                        fat_buf as *mut u8,
                        file_buf as *mut u8,
                        req.buf as *mut u8,
                    ),
                    _ => u64::MAX,
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
                let n = match fd_lookup(req.fd) {
                    Some(mut node) if !node.is_dir => {
                        let written = write_file_range(
                            &bpb,
                            &mut node,
                            offset,
                            req.count,
                            fat_buf as *mut u8,
                            file_buf as *mut u8,
                            dir_buf as *mut u8,
                            req.buf as *const u8,
                        );
                        if written != u64::MAX {
                            fd_update(req.fd, node.start_cluster, node.file_size);
                        }
                        written
                    }
                    _ => u64::MAX,
                };
                sys_reply(n);
            }
            vfs::VFS_READDIR_TAG => {
                let req: vfs::DirReq = unsafe {
                    core::ptr::read_unaligned(msg.payload.as_ptr() as *const vfs::DirReq)
                };
                let n = match fd_lookup(req.fd) {
                    Some(node) if node.is_dir => readdir_into(
                        &bpb,
                        node.start_cluster,
                        dir_buf as *mut u8,
                        fat_buf as *mut u8,
                        req.buf as *mut u8,
                    ),
                    _ => u64::MAX,
                };
                sys_reply(n);
            }
            vfs::VFS_CLOSE_TAG => {
                let fd = read_u32(msg.payload.as_ptr());
                sys_reply(fd_free(fd));
            }
            vfs::VFS_CREAT_TAG => {
                let path_len = msg
                    .payload
                    .iter()
                    .position(|&b| b == 0)
                    .unwrap_or(PAYLOAD_LEN);
                let path = unsafe { core::str::from_utf8_unchecked(&msg.payload[..path_len]) };
                let fd = match resolve_parent(&bpb, path, dir_buf as *mut u8, fat_buf as *mut u8) {
                    Some((parent, sn)) => match find_entry_sn(
                        &bpb,
                        parent,
                        &sn,
                        dir_buf as *mut u8,
                        fat_buf as *mut u8,
                    ) {
                        Some(info) if info.attr & ATTR_DIRECTORY == 0 => fd_alloc(
                            false,
                            info.start_cluster,
                            info.file_size,
                            info.dir_cluster,
                            info.entry_offset,
                            vol,
                        ),
                        Some(_) => u64::MAX, // 已存在目录
                        None => {
                            // 创建空文件 (首簇 0, 大小 0, 不预分配簇)。
                            match find_dir_slot(
                                &bpb,
                                parent,
                                dir_buf as *mut u8,
                                fat_buf as *mut u8,
                            ) {
                                Some((off, dc))
                                    if write_dir_entry(
                                        &bpb,
                                        dc,
                                        off,
                                        &sn,
                                        0,
                                        0,
                                        0,
                                        dir_buf as *mut u8,
                                    ) =>
                                {
                                    fd_alloc(false, 0, 0, dc, off, vol)
                                }
                                _ => u64::MAX,
                            }
                        }
                    },
                    None => u64::MAX,
                };
                sys_reply(fd);
            }
            vfs::VFS_MKDIR_TAG => {
                let path_len = msg
                    .payload
                    .iter()
                    .position(|&b| b == 0)
                    .unwrap_or(PAYLOAD_LEN);
                let path = unsafe { core::str::from_utf8_unchecked(&msg.payload[..path_len]) };
                let r = match resolve_parent(&bpb, path, dir_buf as *mut u8, fat_buf as *mut u8) {
                    Some((parent, sn)) => {
                        if find_entry_sn(&bpb, parent, &sn, dir_buf as *mut u8, fat_buf as *mut u8)
                            .is_some()
                        {
                            u64::MAX // 已存在
                        } else {
                            match find_free_cluster(&bpb, fat_buf as *mut u8) {
                                Some(free) => {
                                    if !write_fat_entry(&bpb, free, FAT_EOC, fat_buf as *mut u8) {
                                        u64::MAX
                                    } else {
                                        // 清零整簇, 写入 . 与 .. 目录项。
                                        zero_bytes(
                                            dir_buf as *mut u8,
                                            bpb.cluster_bytes() as usize,
                                        );
                                        if !write_cluster(&bpb, free, dir_buf as *mut u8) {
                                            u64::MAX
                                        } else {
                                            let dot = *b".          ";
                                            let dotdot = *b"..         ";
                                            if !write_dir_entry(
                                                &bpb,
                                                free,
                                                0,
                                                &dot,
                                                ATTR_DIRECTORY,
                                                free,
                                                0,
                                                dir_buf as *mut u8,
                                            ) || !write_dir_entry(
                                                &bpb,
                                                free,
                                                32,
                                                &dotdot,
                                                ATTR_DIRECTORY,
                                                parent,
                                                0,
                                                dir_buf as *mut u8,
                                            ) {
                                                u64::MAX
                                            } else {
                                                // 在父目录登记新目录项。
                                                match find_dir_slot(
                                                    &bpb,
                                                    parent,
                                                    dir_buf as *mut u8,
                                                    fat_buf as *mut u8,
                                                ) {
                                                    Some((off, dc))
                                                        if write_dir_entry(
                                                            &bpb,
                                                            dc,
                                                            off,
                                                            &sn,
                                                            ATTR_DIRECTORY,
                                                            free,
                                                            0,
                                                            dir_buf as *mut u8,
                                                        ) =>
                                                    {
                                                        1
                                                    }
                                                    _ => u64::MAX,
                                                }
                                            }
                                        }
                                    }
                                }
                                None => u64::MAX,
                            }
                        }
                    }
                    None => u64::MAX,
                };
                sys_reply(r);
            }
            vfs::VFS_UNLINK_TAG => {
                let path_len = msg
                    .payload
                    .iter()
                    .position(|&b| b == 0)
                    .unwrap_or(PAYLOAD_LEN);
                let path = unsafe { core::str::from_utf8_unchecked(&msg.payload[..path_len]) };
                let r = match resolve_parent(&bpb, path, dir_buf as *mut u8, fat_buf as *mut u8) {
                    Some((parent, sn)) => match find_entry_sn(
                        &bpb,
                        parent,
                        &sn,
                        dir_buf as *mut u8,
                        fat_buf as *mut u8,
                    ) {
                        Some(info)
                            if info.attr & ATTR_DIRECTORY == 0
                                && unlink_entry(
                                    &bpb,
                                    info.dir_cluster,
                                    info.entry_offset,
                                    info.start_cluster,
                                    dir_buf as *mut u8,
                                    fat_buf as *mut u8,
                                ) =>
                        {
                            1
                        }
                        _ => u64::MAX, // 不存在或目录
                    },
                    None => u64::MAX,
                };
                sys_reply(r);
            }
            vfs::VFS_RMDIR_TAG => {
                let path_len = msg
                    .payload
                    .iter()
                    .position(|&b| b == 0)
                    .unwrap_or(PAYLOAD_LEN);
                let path = unsafe { core::str::from_utf8_unchecked(&msg.payload[..path_len]) };
                let r = match resolve_parent(&bpb, path, dir_buf as *mut u8, fat_buf as *mut u8) {
                    Some((parent, sn)) => match find_entry_sn(
                        &bpb,
                        parent,
                        &sn,
                        dir_buf as *mut u8,
                        fat_buf as *mut u8,
                    ) {
                        Some(info)
                            if info.attr & ATTR_DIRECTORY != 0
                                && dir_is_empty(
                                    &bpb,
                                    info.start_cluster,
                                    dir_buf as *mut u8,
                                    fat_buf as *mut u8,
                                )
                                && unlink_entry(
                                    &bpb,
                                    info.dir_cluster,
                                    info.entry_offset,
                                    info.start_cluster,
                                    dir_buf as *mut u8,
                                    fat_buf as *mut u8,
                                ) =>
                        {
                            1
                        }
                        _ => u64::MAX, // 不存在或文件
                    },
                    None => u64::MAX,
                };
                sys_reply(r);
            }
            vfs::VFS_STAT_TAG => {
                let (buf, path) = parse_path_req(msg.payload.as_ptr());
                let n = match resolve_open_path(&bpb, path, dir_buf as *mut u8, fat_buf as *mut u8)
                {
                    Some(info) => {
                        let is_dir = u32::from(info.attr & ATTR_DIRECTORY != 0);
                        let st = vfs::Stat::plain(info.file_size as u64, is_dir);
                        unsafe {
                            core::ptr::write_unaligned(buf as *mut vfs::Stat, st);
                        }
                        core::mem::size_of::<vfs::Stat>() as u64
                    }
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
