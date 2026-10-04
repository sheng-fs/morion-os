use crate::common::*;
use morion::syscall::*;
use morion::vfs;

// ===========================================================================
// 域 11 — mfs_srv (MorionFS, 块设备后端)
// ===========================================================================
// MorionFS (MFS) 是原创的原生文件系统, 相对 fat32/tmpfs 的差异化设计:
//
//   * 块校验: 每个 4 KiB 块带 8 字节头 (magic + CRC32), 读时校验, 损坏即拒绝,
//     避免静默损坏被上层当成正常内容。
//   * 写时复制 (COW): 任何修改都分配**新块**写入, 旧块原地保留; 从叶子一路复制
//     父目录、祖父目录直到根, 最后写超级块 —— 天然产生不可变的历史版本。
//   * 快照: 超级块内保存 {generation, root_block, alloc_next}; 因 COW 从不覆盖旧块,
//     快照创建后其目录树始终有效, 回滚只需把根指回快照的根。
//   * 超级块 A/B 双副本 + generation: 交替写入, 挂载时取 CRC 有效且代际更高者,
//     掉电只损坏一份仍可挂载。
//   * 自动格式化: 两份超级块都无效 (空白盘) 时, 首次挂载即格式化。
//
// 一对一节点: 每个 4 KiB 块 = 一个节点 (目录/文件) 或一个数据块。
// 块 0/1 = 超级块 A/B; 块 2 起为 COW 分配区 (只增不回收, 空间保留给快照)。
//
// 统一块布局: [ magic u32 | crc32 u32 | payload 4088 ]
//   目录 payload: nentries u32 | pad u32 | entries[170] { name[16] | block u32 | type u32 }
//   文件 payload: size u64 | nblocks u32 | pad u32 | 直接指针 + 一/二/三级间接指针
//   数据 payload: 文件字节 (每块最多 4088 字节)
//
// 名称沿用 8.3 短名 (转大写), 与 libvfs 的 DirEntry ABI 及 shell 显示一致。

const MFS_BLOCK: usize = 4096;
const MFS_SECTORS_PER_BLOCK: u16 = (MFS_BLOCK / 512) as u16;
const MFS_HDR: usize = 8;
const MFS_PAYLOAD: usize = MFS_BLOCK - MFS_HDR;

/// 卷号回退值: MFS 空白盘没有 magic, 卷层探测不到时按约定认领卷 1
/// (对应 Makefile 的 `build/mfs.img`, 即 namespace 2)。
const MFS_VOL_FALLBACK: u64 = 1;
/// MFS 服务**主卷**号 (挂在 `/mfs`), 启动时由 `vol_claim` 认领 (见 `mfs_main`)。
static mut MFS_VOL: u64 = MFS_VOL_FALLBACK;
/// 主卷的容量 (扇区数, 启动时从卷表取); 0 = 未知。
/// 首次格式化按它决定文件系统大小 —— 不再假设「盘就是 16 MiB」。
static mut MFS_VOL_SECTORS: u32 = 0;
/// **当前卷**号 (M1b 多卷挂载): 本服务此刻正在服务的卷。
///
/// MFS 的内存态 (位图 / inode 表 / 快照 / 各游标) 只有一份, 对应**一个**卷, 故
/// 只能串行服务多卷: 请求带的卷号与当前卷不同时, 先把新卷的超级块载回内存态
/// (见 `mfs_switch_vol`) —— 之所以能这样切, 是因为每次改动都会随超级块写盘,
/// 请求边界上盘上状态总是自洽的。
static mut MFS_CUR_VOL: u64 = MFS_VOL_FALLBACK;
/// 当前卷的容量 (扇区数); 0 = 未知。格式化尺寸与挂载时的容量校验都按它算。
static mut MFS_CUR_SECTORS: u32 = 0;
/// 卷容量未知时的兜底总块数 (16 MiB / 4 KiB), 与 Makefile 的默认 `MFS_MIB=16` 对应。
/// 仅用于首次格式化; 之后以超级块记录的值为准。
const MFS_DEFAULT_TOTAL_BLOCKS: u32 = 4096;
/// 首次格式化的**下限** (256 KiB): 卷再小也得放得下两份超级块 + 根目录 + inode 表。
/// 低于这个数直接格式化会得到一个连元数据都装不下的文件系统。
const MFS_MIN_TOTAL_BLOCKS: u32 = 64;
/// 超级块副本数 (块 0 / 块 1)。
const MFS_SB_COPIES: u32 = 2;
/// 超级块内快照表容量。
const MFS_MAX_SNAP: usize = 8;

/// 超级块 magic: "MFS8"（文件 size 改 u64 + 三级间接块 → 单文件上限与卷容量同量级）。
///
/// magic 携带布局修订: `MFS1` = 纯 COW, `MFS2` = +空闲位图, `MFS3` = +文件间接块,
/// `MFS4` = +变长目录项/多块目录, `MFS5` = +节点元数据（目录块头部 8 → 48）,
/// `MFS6` = +inode 表（目录项改存 inode 号，块号经表映射）,
/// `MFS7` = +位图外置（位图改为独立数据块 + 头块，超级块 payload 不再内联位图）,
/// `MFS8` = +文件 size u64 +三级间接块（缩直接区 4 字节腾出 ind3 槽位，元数据偏移不变）。
/// 旧修订缺少新布局所需的字段/语义, 挂载时一律视为无效 -> 自动重新格式化
/// (卷层仍同时认各修订, 见 `vol_detect_kind`)。
const MFS_MAGIC_SUPER: u32 = 0x4D46_5338; // "MFS8"
/// 超级块内的格式版本 (magic 之外的二次校验)。
const MFS_VERSION: u32 = 8;
/// 卷首 magic 三态判定的结果 (01 保护数据)。
///
/// 与「magic 承载布局修订」的注释配套: 只有**空白卷**才允许自动格式化; 属 MFS 系但
/// 不匹配本构建的 (更旧 / 更新) 一律**拒绝挂载**, 绝不自动重格 —— 升 magic 会把老盘
/// 全判成不匹配, 那正是要避免的破坏。
const MFS_MAGIC_NONE: u8 = 0;
const MFS_MAGIC_MATCH: u8 = 1;
const MFS_MAGIC_FAMILY_MISMATCH: u8 = 2;
/// MFS 系 magic 的高三字节 ("MFS"); 低字节是修订号 ('0'..'9')。
const MFS_MAGIC_FAMILY_PREFIX: u32 = 0x4D46_5300;
/// 位图头块 magic: "MFBH"（记录 gen / 总块数 / 位图数据块数 + 各数据块 CRC32）。
const MFS_MAGIC_BMPHDR: u32 = 0x4D46_4248; // "MFBH"
/// 目录块 (base 节点或扩展块): ext + 元数据 + 变长条目区。
const MFS_MAGIC_DIR: u32 = 0x4D46_4449; // "MFDI"
/// 目录扩展索引块 (槽位全是扩展目录块指针)。
const MFS_MAGIC_DIDX: u32 = 0x4D46_5849; // "MFXI"
const MFS_MAGIC_FILE: u32 = 0x4D46_464C; // "MFFL"
/// 软链接节点 (M5c): 目标路径内联存在节点 payload 里, 不占数据块。
const MFS_MAGIC_LINK: u32 = 0x4D46_534C; // "MFSL"
const MFS_MAGIC_DATA: u32 = 0x4D46_4441; // "MFDA"
/// 一级间接块 (槽位全是数据块指针)。
const MFS_MAGIC_IND: u32 = 0x4D46_494E; // "MFIN"
/// 二级间接块 (槽位全是一级间接块指针)。
const MFS_MAGIC_IND2: u32 = 0x4D46_4932; // "MFI2"
/// 三级间接块 (槽位全是二级间接块指针)。
const MFS_MAGIC_IND3: u32 = 0x4D46_4933; // "MFI3"

// 节点元数据: 文件与目录布局相同, 只是所在偏移不同 (目录紧跟 ext 之后, 文件在 inode
// 尾部保留区)。三种时间都是 Unix 秒 (由 CMOS RTC 提供)。
//
//   +0 mode(u16) / +2 owner(u16) / +4 nlink(u32)
//   +8 mtime(u64) / +16 ctime(u64) / +24 atime(u64) / +32 reserved(u64)
//
// `owner` 记创建者域 id (没有多用户概念, 故不做 uid/gid); `mode` 只存储与显示,
// **不做强制检查** (见 docs/roadmap-fs.md M5)。`atime` 不随读更新 —— 否则每次读都要
// COW 整个 inode 并上溯到根, 读路径会退化成写路径。
const MFS_META_LEN: usize = 40;
const MFS_META_MODE: usize = 0;
const MFS_META_OWNER: usize = 2;
const MFS_META_NLINK: usize = 4;
const MFS_META_MTIME: usize = 8;
const MFS_META_CTIME: usize = 16;
const MFS_META_ATIME: usize = 24;
/// 04b: 元数据 `+32` 起 8 字节保留区启用为 **uid / gid**（各 u16）。**不升 magic、
/// 不改元数据尺寸** —— 老 MFS8 盘该处为 0，读出来即 `uid=0/gid=0`（旧文件全归 root）。
const MFS_META_UID: usize = 32;
const MFS_META_GID: usize = 34;

/// 新建目录的默认权限 (rwxr-xr-x)。
const MFS_MODE_DIR: u16 = 0o755;
/// 新建文件的默认权限 (rw-r--r--)。
const MFS_MODE_FILE: u16 = 0o644;
/// 新建软链接的默认权限 (rwxrwxrwx; 与 Unix 一致, 链接自身的权限位无意义)。
const MFS_MODE_LINK: u16 = 0o777;
/// 权限位掩码 (只保留低 12 位: setuid/setgid/sticky + rwxrwxrwx)。
const MFS_MODE_MASK: u16 = 0o7777;

// `mode` 的高 4 位是**节点类型** (与 ext2 `i_mode` 的 S_IFMT 同构), 低 12 位是权限。
//
// 为什么要它: 目录/文件之外多了软链接, 而 `vfs::Stat` / `vfs::DirEntry` 只有
// `is_dir` 一个类型信号 —— 光看它区分不出「普通文件」与「软链接」。把类型编码进
// 已经存在的 `mode` 字段即可让 `ls -l` / `stat` 显示 `l`, 不必改协议结构体。
// 非 MFS 的文件服务不填类型位 (mode 只有权限), 客户端按 `is_dir` 回退显示。
const MFS_FTYPE_MASK: u16 = 0xF000;
const MFS_FTYPE_FILE: u16 = 0x8000;
const MFS_FTYPE_DIR: u16 = 0x4000;
const MFS_FTYPE_LINK: u16 = 0xA000;

// 目录块 payload 布局: +0 ext(扩展索引块号, 0 = 无) / +4 pad / +8 元数据(40) / +48 起条目区。
//
// 条目按 rec_len 串联 (ext2 风格), 每项 4 字节对齐:
//   +0 block(u32) / +4 type(u8) / +5 name_len(u8) / +6 rec_len(u16) / +8 name
// `name_len == 0` 表示空槽; 空槽与"条目的尾部余量"都可被后续插入复用, 删除时把
// 释放的长度并给前一项以回收碎片。名字上限 255 字节 (name_len 是 u8)。
const MFS_DIR_HDR: usize = 8 + MFS_META_LEN;
/// 条目区字节数。
const MFS_DIR_AREA: usize = MFS_PAYLOAD - MFS_DIR_HDR;
/// 条目头长度。
const MFS_DIR_ENT_HDR: usize = 8;
/// 最小条目长度 (头 + 1 字节名字, 向上取 4 的倍数)。
const MFS_DIR_ENT_MIN: usize = 12;
/// 名字长度上限 (磁盘格式能力; 端到端受 IPC payload 限制, 见 docs/roadmap-fs.md M4)。
const MFS_NAME_MAX: usize = 255;
/// 扩展索引块的槽位数 (整个 payload 都是扩展目录块指针)。
const MFS_DIR_SLOTS: usize = MFS_PAYLOAD / 4;

// 文件块 payload 布局 (MFS8):
//   +0 size(u64) / +8 nblocks(u32) / +12 pad(u32) / +16 起 1005 个直接块指针
//   / 其后 一级 + 二级 + 三级间接指针 / 末尾 40 字节保留 (预留给元数据)。
//
// 逻辑块索引 (`bi`) 到物理块的映射分四段: 直接区 -> 一级 -> 二级 -> **三级**间接区;
// 每段容量见下。合计 `MFS_FILE_MAX_BLOCKS` 已远超位图能描述的块数, 故单文件上限实际
// 等于整卷可用块数 (即单文件与卷容量同量级; MFS7 起位图外置, ≈127.25 GiB)。
/// inode 内的直接块指针数 (直接区覆盖 ≈3.9 MiB, 小文件不产生额外 I/O)。
///
/// 由 1008 缩到 1005: size 由 u32 变 u64 (+4 字节)、新增 4 字节 pad (+4) 各让出
/// 一个槽位, 再把 `MFS_FILE_IND3_OFF` 需要的 4 字节腾出来 —— 合计缩掉 3 个指针
/// (12 字节), 使三个间接指针仍结束于 4056, 元数据偏移保持不变。
const MFS_FILE_DIRECT: usize = 1005;
/// 文件大小 (u64) 在块内的偏移。
const MFS_FILE_SIZE_OFF: usize = MFS_HDR;
/// 已分配逻辑块数 (u32) 在块内的偏移。
const MFS_FILE_NBLOCKS_OFF: usize = MFS_HDR + 8;
/// 直接块指针区在块内的起点。
const MFS_FILE_DIRECT_OFF: usize = MFS_HDR + 16;
/// 一级间接块指针在 inode 中的偏移。
const MFS_FILE_IND1_OFF: usize = MFS_FILE_DIRECT_OFF + MFS_FILE_DIRECT * 4;
/// 二级间接块指针在 inode 中的偏移。
const MFS_FILE_IND2_OFF: usize = MFS_FILE_IND1_OFF + 4;
/// 三级间接块指针在 inode 中的偏移 (缩直接区腾出的槽位)。
const MFS_FILE_IND3_OFF: usize = MFS_FILE_IND2_OFF + 4;
/// inode 保留区起点 (留给元数据: 时间戳 / 权限 / 链接数); 必须仍为 4056。
const MFS_FILE_RESERVED_OFF: usize = MFS_FILE_IND3_OFF + 4;
/// inode 保留区字节数。
const MFS_FILE_RESERVED: usize = MFS_BLOCK - MFS_FILE_RESERVED_OFF;

// 软链接块 payload 布局 (M5c) —— **沿用文件布局**, 这样所有元数据读写函数用
// `is_dir = false` 就能直接作用于软链接, 不必给它们再加一种节点类型分支:
//
//   +0 size(u64)  ← 复用文件的大小字段, 这里存**目标路径字节数** (`lstat` 的 size)
//   +8 起         ← 目标路径字节 (UTF-8, 无结尾 NUL), 见 `MFS_LINK_TARGET_OFF`
//                    (紧接 u64 size 之后, 顺带覆盖未用的 nblocks / 直接指针区)
//   ...
//   +MFS_FILE_RESERVED_OFF 起 40 字节元数据 (与文件同偏移)
//
// 目标内联在节点里 (fast symlink), 不占数据块 —— 软链接不参与硬链接, nlink 恒为 1。
/// 软链接目标路径在节点 payload 里的起始偏移 (紧接 u64 size 之后)。
const MFS_LINK_TARGET_OFF: usize = MFS_HDR + 8;
/// 软链接目标路径长度上限 (实际还受单条 IPC 路径长度约束, 见 `MFS_PATH_MAX`)。
const MFS_LINK_MAX: usize = MFS_FILE_RESERVED_OFF - MFS_LINK_TARGET_OFF;
/// 布局自检: 直接指针区 + 三个间接指针 + 保留区正好铺满一个 4 KiB 块。
const _: () = assert!(MFS_FILE_RESERVED >= 40);
/// 每个间接块的指针槽数 (整个 payload 都是指针)。
const MFS_IND_CAP: usize = MFS_PAYLOAD / 4;
/// 单个文件的逻辑块上限 = 直接 + 一级 + 二级 + 三级容量。
///
/// 三级槽数 (1022³ ≈ 1.07e9) 在 usize 上算, 避免中间量溢出。
const MFS_FILE_MAX_BLOCKS: usize = MFS_FILE_DIRECT
    + MFS_IND_CAP
    + MFS_IND_CAP * MFS_IND_CAP
    + MFS_IND_CAP * MFS_IND_CAP * MFS_IND_CAP;
/// 单个数据块可存放的文件字节数。
const MFS_DATA_CAP: usize = MFS_PAYLOAD;
// 路径解析链最大深度 / 打开文件上限。
// 深度上限按「单条 IPC 路径最长 95 字节、最短分量 1 字符 + '/'」估算 (≈47 级), 取 48;
// M4 起目录可任意嵌套 (无额外结构限制), 限制只来自路径编码长度。
const MFS_MAX_DEPTH: usize = 48;
/// 解析路径的工作缓冲大小 (与单条 IPC 路径上限一致)。
///
/// 跟随软链接时会把「目标 + 剩余分量」重新组装成一条路径再解析, 组装结果也受这个
/// 上限约束 —— 超长则解析失败 (返回 None), 而不是截断成一条错路径。
const MFS_PATH_MAX: usize = TMP_PATH_MAX;
/// 一条路径上最多跟随多少个软链接 (防环 + 限制展开长度)。
///
/// 环 (a→b→a) 会在这里被截住并返回"解析失败", 而不是无限展开; 正常的软链接链远
/// 短于这个值。
const MFS_SYMLINK_MAX_DEPTH: u32 = 16;
const MFS_MAX_FD: usize = 16;

// 节点类型 (目录项 type 字段)。
const MFS_TYPE_FILE: u32 = 1;
const MFS_TYPE_DIR: u32 = 2;
/// 软链接 (M5c)。目录项只按这个类型区分, 具体目标在节点 payload 里。
const MFS_TYPE_LINK: u32 = 3;

// MFS6: 目录项存 **inode 号** 而不是块号, inode 号到块号的映射由一棵独立的 COW 树
// 提供 (索引块 -> 表块 -> 槽位)。这样多个目录项可以指向同一个 inode (硬链接), 而
// 修改 inode 只需更新它那一个表槽 —— 所有链接自动看到新内容。
//
//   inode 表索引块 (MFIX): payload 全是表块指针, 第 k 项 = 第 k 个表块 (0 = 未分配)
//   inode 表块 (MFIT):     payload 全是对象块指针, 第 j 项 = ino = k*SLOTS + j 的块号
//
// ino 0 保留为「无效 / 空闲」, 根目录固定在 ino 1 (它永不改名/删除, 故无需在超级块里
// 存根号)。inode 更新 = COW 对象块 + COW 表块 + COW 索引块 + 写超级块。
const MFS_MAGIC_ITAB: u32 = 0x4D46_4954; // "MFIT" inode 表块
const MFS_MAGIC_ITABX: u32 = 0x4D46_4958; // "MFIX" inode 表索引块
/// inode 表块 / 索引块的槽位数 (整个 payload 都是 u32 指针)。
const MFS_ITAB_SLOTS: usize = MFS_PAYLOAD / 4;
/// inode 号上限 (索引块 × 表块 × 每块槽数)。
const MFS_INO_MAX: u32 = (MFS_ITAB_SLOTS * MFS_ITAB_SLOTS) as u32;
/// 根目录固定的 inode 号。
const MFS_ROOT_INO: u32 = 1;

// 超级块 payload 布局 (除下列区段外均为保留):
//   +0 version / +4 block_size / +8 total_blocks / +12 ino_count / +16 alloc_hint
//   +20 snap_count / +24 gen(u64) / +32 itab_root / +36 ino_hint
//   / +48 快照表(8 × 24B) / +256 起预留
// (MFS7 起空闲位图已移出超级块, +256 那片区域留空保留 —— 见位图头块布局;
//  其中 `MFS_SB_PRIMARY` 从 +256 起占了 8 字节, 其余仍保留。)
const MFS_SB_BLOCK_SIZE: usize = 4;
const MFS_SB_TOTAL: usize = 8;
const MFS_SB_INO_COUNT: usize = 12;
const MFS_SB_ALLOC_HINT: usize = 16;
const MFS_SB_SNAP_COUNT: usize = 20;
const MFS_SB_GEN: usize = 24;
/// inode 表索引块 (MFIX) 块号。
const MFS_SB_ITAB: usize = 32;
/// inode 分配游标 (下次从这里开始找空槽)。
const MFS_SB_INO_HINT: usize = 36;
/// 快照表起点。
const MFS_SB_SNAPS: usize = 48;
/// 单条快照记录字节数: gen(u64) + itab_root/ino_hint/alloc_hint/reserved (4 × u32)。
const MFS_SNAP_REC: usize = 24;
/// **主卷序号** (u64, 0 = 不是主卷) —— `MFS7` 起 payload `+256` 整片是保留区, 这里取头 8 字节。
///
/// 一台机器上可以有多块 MFS 卷; 谁当 `/mfs` 以前只由**卷表扫描顺序**决定, 显式
/// `mkfs.mfs` 过谁完全不影响 —— 这既不可控也无法解释。改为: `mkfs.mfs` 把目标卷的
/// 序号置成「现有最大 + 1」(见 `mfs_mkfs_volume`), 认领时取**序号最大**的 MFS 卷
/// (见 `mfs_vol_claim`)。于是「最近一次显式格式化的卷」稳定地就是下次启动的主卷。
///
/// 序号只增不清: 不需要回写别的卷就能表达「我更新」, 单主卷由「最大者胜出」保证。
/// 全为 0 (老卷 / 只被首次挂载自动格式化过) 时退回「第一个 MFS 卷」的旧行为。
const MFS_SB_PRIMARY: usize = 256;
// 位图头块 payload 布局 (MFS7): +0 gen(u64) / +8 total_blocks(u32) / +12 data_blocks(u32)
//   / +16 CRC32 数组 (每个位图数据块一项)。
/// 位图头块 payload: 代际 (必须与对应超级块副本的 gen 一致)。
const MFS_BMPH_GEN: usize = 0;
/// 位图头块 payload: 总块数 (必须与超级块一致)。
const MFS_BMPH_TOTAL: usize = 8;
/// 位图头块 payload: 位图数据块数 bb (必须等于该卷的 bb)。
const MFS_BMPH_DATA_BLOCKS: usize = 12;
/// 位图头块 payload: CRC32 数组起点。
const MFS_BMPH_CRC: usize = 16;

// MFS7 盘上布局 (位图外置):
//   块 0/1        = 超级块 A/B
//   块 2/3        = 位图头 A/B (magic "MFBH")
//   块 4..4+bb    = 位图数据副本 A (裸 4096 字节, 无块头)
//   块 4+bb..4+2bb = 位图数据副本 B
//   MFS_DATA_START = 4 + 2*bb; 它之前的块一律强制标记占用, 不参与分配。
/// 一个位图数据块覆盖的块数 (4096 字节 × 8 位 = 32768 块 = 128 MiB)。
const MFS_BMP_BLOCK_SPAN: u32 = (MFS_BLOCK * 8) as u32;
/// 位图数据块数上限 (= 位图头块 payload 里 CRC 数组的容量)。
const MFS_MAX_BMP_DATA_BLOCKS: u32 = ((MFS_PAYLOAD - MFS_BMPH_CRC) / 4) as u32;
/// 位图能描述的最大块数 (1018 × 32768 = 33_357_824 块 ≈ 127.25 GiB)。
const MFS_MAX_BLOCKS: u32 = MFS_MAX_BMP_DATA_BLOCKS * MFS_BMP_BLOCK_SPAN;
/// 低水位分频: 空闲块 < `总块数 / MFS_GC_LOW_WATER_DIV` 时, 服务空闲即自动回收。
const MFS_GC_LOW_WATER_DIV: u32 = 16;
/// 回收遍历栈容量 (只压入目录块, 深度优先)。
const MFS_GC_STACK_MAX: usize = 4096;

/// MFS 块缓冲虚拟地址。
///
/// 必须放在程序镜像之外 (`USER_BASE` 起, 随代码增长) 且与其他固定区不重叠:
/// 这些页要以「同地址」共享给 block_srv 供其 DMA 写入, 若位于镜像内, 目标域
/// block_srv 自身的镜像会占住同一地址, 共享时 map_user_page 触发
/// PageAlreadyMapped。已占用区间: fat32 `+0x10_0000..0x10_4000`、
/// app/shell 共享缓冲 `+0x10_4000..0x10_8000`, 故取其后相邻 4 页。
const MFS_BUF_A_VADDR: u64 = 0x0000_0080_0010_8000;
const MFS_BUF_B_VADDR: u64 = 0x0000_0080_0010_9000;
const MFS_BUF_C_VADDR: u64 = 0x0000_0080_0010_A000;
const MFS_BUF_S_VADDR: u64 = 0x0000_0080_0010_B000;
/// GC 遍历块专用缓冲页。
///
/// GC 需要读任意块; 若复用 A/B/C/S 就必须在每个调用点单独证明它们空闲 (A/B/C 常
/// 持有 COW 在建内容, S 供超级块使用)。独立一页让 GC 与这些缓冲彻底解耦。取 ext2
/// 缓冲页之后、block_srv 卷扫描页之前的空位, 同样位于程序镜像之外。
const MFS_GC_VADDR: u64 = 0x0000_0080_0011_0000;
/// inode 表块缓存页 (单条目: 记住当前载入的表块索引, `mfs_ino_block` 用)。
const MFS_ITAB_VADDR: u64 = 0x0000_0080_0011_1000;
/// inode 表索引块 scratch 页 (只在 `mfs_itab_flush` 重建索引块时用)。
const MFS_ITABX_VADDR: u64 = 0x0000_0080_0011_2000;
/// GC 遍历时翻译 ino 用的表块缓冲页 (GC 的主体缓冲在 `MFS_GC_VADDR`, 两个不能共用)。
const MFS_GC_TAB_VADDR: u64 = 0x0000_0080_0011_3000;
/// fsck 的「可达 inode」位图窗口 (按 **ino** 索引; 只在 fsck 期间按需分配, 服务私有)。
///
/// 单独一页起始地址: 可达块用 MARK/SEEN 窗口 (按块号索引), 而 ino 号空间与块号空间
/// 无关 (ino 可远超总块数), 复用会因窗口按块数定长而漏标 —— 故 fsck 用独立窗口。
/// 取 0x0200_0000: 排在 SEEN 窗口的最坏增长之外 (1018 页 ≈ 4.17 MiB), 又不碰 gfx 的
/// `SURFACE_BASE` (0x0400_0000)。
const MFS_FSCK_VADDR: u64 = 0x0000_0080_0200_0000;
/// 覆盖 `MFS_INO_MAX` 个 bit 所需的页数。
const MFS_FSCK_PAGES: u32 = ((MFS_INO_MAX as usize).div_ceil(8)).div_ceil(MFS_BLOCK) as u32;

// MFS7 位图窗口 (页数动态, 见 `mfs_win_ensure`)。
//
// 旧版把空闲位图 / GC 标记 / 本根已访问三个位图放在**编译期定长数组**里; 新容量上限
// 需要 ≈4.17 MB/窗口, 静态放不下 —— 改为按卷容量逐页分配的窗口。三个窗口都**只增不缩**
// (卷变小也不 sys_unmap: 窗口页共享给了 block_srv, 回收要走引用计数, 不值当)。
/// 主空闲位图窗口: 第 k 页 ↔ 该副本第 k 个位图数据块, 直接作 `block_read/write_dev`
/// 的缓冲 (免拷贝)。**必须**共享给 block_srv 供其 DMA 写入。
const MFS_BMP_VADDR: u64 = 0x0000_0080_0100_0000;
/// GC 可达标记窗口 (服务私有, 不共享)。
const MFS_MARK_VADDR: u64 = 0x0000_0080_0140_0000;
/// GC「本根已访问」窗口 (服务私有, 不共享)。
const MFS_SEEN_VADDR: u64 = 0x0000_0080_0180_0000;
/// 位图头块缓冲页 (单页)。同样共享给 block_srv —— 头块经它读入 / 写出。
const MFS_BMPH_VADDR: u64 = 0x0000_0080_0016_2000;

// ---------------------------------------------------------------------------
// 02b-2 批 I/O 缓冲 (mfs_srv 同址共享给 block_srv 供其 DMA)
//
//   读批窗口 16 页 `+0x18_0000` + 读描述符 1 页 `+0x19_0000`
//   写暂存窗 16 页 `+0x1A_0000` + 写描述符 1 页 `+0x1B_0000`
//
// 读 / 写用**不同**的描述符页: 否则一次写批 flush 会覆盖读批刚填好的描述符。地址段排
// 在 mfs 既有缓冲 (`..+0x16_3000`) 之上、fat32 整簇缓冲 (`+0x20_0000`) 之下, 互不撞址。
// ---------------------------------------------------------------------------
/// 读批窗口基址 (16 页)。
const MFS_RDBUF_VADDR: u64 = 0x0000_0080_0018_0000;
/// 读批窗口页数。
const MFS_RDBUF_PAGES: usize = BLOCK_BATCH_MAX;
/// 读批描述符页 (单页)。
const MFS_RDBUF_DESC_VADDR: u64 = 0x0000_0080_0019_0000;
/// 写暂存窗基址 (16 页)。
const MFS_WB_VADDR: u64 = 0x0000_0080_001A_0000;
/// 写暂存窗页数。
const MFS_WB_PAGES: usize = BLOCK_BATCH_MAX;
/// 写暂存描述符页 (单页)。
const MFS_WB_DESC_VADDR: u64 = 0x0000_0080_001B_0000;

/// 读批窗口第 `i` 个块缓冲 (每块一页)。
fn mfs_rdbuf(i: usize) -> *mut u8 {
    (MFS_RDBUF_VADDR + (i as u64) * MFS_BLOCK as u64) as *mut u8
}
/// 读批描述符数组页。
fn mfs_rdbuf_desc() -> *mut BatchEnt {
    MFS_RDBUF_DESC_VADDR as *mut BatchEnt
}
/// 写暂存窗第 `i` 个块缓冲 (每块一页)。
fn mfs_wbuf(i: usize) -> *mut u8 {
    (MFS_WB_VADDR + (i as u64) * MFS_BLOCK as u64) as *mut u8
}
/// 写暂存描述符数组页。
fn mfs_wb_desc() -> *mut BatchEnt {
    MFS_WB_DESC_VADDR as *mut BatchEnt
}

fn mfs_a() -> *mut u8 {
    MFS_BUF_A_VADDR as *mut u8
}
fn mfs_b() -> *mut u8 {
    MFS_BUF_B_VADDR as *mut u8
}
fn mfs_c() -> *mut u8 {
    MFS_BUF_C_VADDR as *mut u8
}
fn mfs_s() -> *mut u8 {
    MFS_BUF_S_VADDR as *mut u8
}
fn mfs_gc_buf() -> *mut u8 {
    MFS_GC_VADDR as *mut u8
}
fn mfs_itab_buf() -> *mut u8 {
    MFS_ITAB_VADDR as *mut u8
}
fn mfs_itabx_buf() -> *mut u8 {
    MFS_ITABX_VADDR as *mut u8
}
fn mfs_gc_tab_buf() -> *mut u8 {
    MFS_GC_TAB_VADDR as *mut u8
}
fn mfs_bmph_buf() -> *mut u8 {
    MFS_BMPH_VADDR as *mut u8
}

// 超级块/分配状态 (内存镜像, 与磁盘副本同步)。
/// inode 表索引块号 (根目录恒为 `MFS_ROOT_INO`)。
static mut MFS_ITAB: u32 = 0;
/// 已分配的 inode 数 (供 `MSST` 之类的统计)。
static mut MFS_INO_COUNT: u32 = 0;
/// inode 分配游标: 下次从这里开始找空槽。
static mut MFS_INO_HINT: u32 = MFS_ROOT_INO;
/// inode 表块缓存的当前表块索引 (u32::MAX = 未载入)。
static mut MFS_ITAB_CACHE_IDX: u32 = u32::MAX;
/// 分配游标 (超级块 payload +16): 下次分配优先从此块开始扫描。
static mut MFS_ALLOC_NEXT: u32 = 0;
static mut MFS_TOTAL_BLOCKS: u32 = 0;
static mut MFS_GEN: u64 = 0;
static mut MFS_SNAP_COUNT: usize = 0;
/// **当前卷**的主卷序号 (超级块 `MFS_SB_PRIMARY`; 0 = 不是主卷)。
///
/// 必须随每次提交一并写回: 否则一次普通的写盘就会把标记抹成 0, 下次启动主卷就丢了。
static mut MFS_PRIMARY_SERIAL: u64 = 0;
/// 空闲块数 (由位图窗口派生, 由 `mfs_bmp_set/clear` 增量维护)。
static mut MFS_FREE_BLOCKS: u32 = 0;
/// 三个位图窗口当前已分配的页数 (只增不缩, 见各窗口 VADDR 处的说明)。
static mut MFS_BMP_PAGES: u32 = 0;
static mut MFS_MARK_PAGES: u32 = 0;
static mut MFS_SEEN_PAGES: u32 = 0;
/// fsck 的 inode 位图窗口已分配的页数 (只增不缩; fsck 期间按需铺开)。
static mut MFS_FSCK_MAPPED: u32 = 0;
/// 位图数据块数 bb (当前卷; 0 = 未挂载)。
static mut MFS_BMP_DATA_BLOCKS: u32 = 0;
/// 位图数据块**脏位图**的字节数 (每位对应一个位图数据块)。
const MFS_BMP_DIRTY_BYTES: usize = (MFS_MAX_BMP_DATA_BLOCKS as usize).div_ceil(8);
/// 位图数据块的脏位图: 提交时只把变动过的区间落盘 (见 `mfs_bmp_flush`)。
static mut MFS_BMP_DIRTY: [u8; MFS_BMP_DIRTY_BYTES] = [0; MFS_BMP_DIRTY_BYTES];
/// 常驻 CRC32 数组 (每个位图数据块一项): 落盘进位图头块, 加载时逐块比对。
static mut MFS_BMP_CRC: [u32; MFS_MAX_BMP_DATA_BLOCKS as usize] =
    [0; MFS_MAX_BMP_DATA_BLOCKS as usize];
/// GC 的「本根已访问」位图 (每换一个可达根就清零; 现居 `MFS_SEEN_VADDR` 窗口)。
///
/// 必须与可达位图分开: 同一个目录块可能同时被当前树与某快照引用, 而块内条目的
/// ino 在不同根的表下会翻译成**不同**的对象块 —— 所以每个根都得重新遍历一遍。
/// 若拿可达位图当访问集去重, 快照那次就会被跳过, 快照引用的旧块漏标而被回收
/// (M5 之前目录项直接存块号, 不存在这个差异; 引入 inode 表后必须分开)。
/// GC 遍历栈 (待展开的目录块)。
static mut MFS_GC_STACK: [u32; MFS_GC_STACK_MAX] = [0; MFS_GC_STACK_MAX];
/// GC 遍历栈顶指针 (跨函数传递, 故用静态量)。
static mut MFS_GC_SP: usize = 0;

/// 快照记录: 根恒为 `MFS_ROOT_INO`, 故只需记 inode 表索引块 + 两个分配游标 + 代际
/// —— 有这四项就能完整重建当时的目录树与 inode 映射 (表块本身被 GC 视为快照可达)。
#[derive(Clone, Copy)]
struct MfsSnap {
    gen: u64,
    itab: u32,
    ino_hint: u32,
    alloc_next: u32,
}
impl MfsSnap {
    const EMPTY: MfsSnap = MfsSnap {
        gen: 0,
        itab: 0,
        ino_hint: 0,
        alloc_next: 0,
    };
}
static mut MFS_SNAPS: [MfsSnap; MFS_MAX_SNAP] = [MfsSnap::EMPTY; MFS_MAX_SNAP];

fn mfs_snap(i: usize) -> MfsSnap {
    unsafe { *core::ptr::addr_of!(MFS_SNAPS).cast::<MfsSnap>().add(i) }
}
fn mfs_set_snap(i: usize, s: MfsSnap) {
    unsafe {
        *core::ptr::addr_of_mut!(MFS_SNAPS).cast::<MfsSnap>().add(i) = s;
    }
}

/// 路径解析结果的叶子条目位置 (目录项的 ino 与条目所在块/偏移), 供删除 / 改名使用。
///
/// MFS6 起目录项存 ino, 父目录条目在对象更新时**不变**, 因此不再需要「从叶到根的
/// 完整回写链」—— 这就是 inode 间接层带来的简化: 改一个对象只需 COW 它自己 + 它的
/// 表槽, 与目录深度无关。
#[derive(Clone, Copy)]
struct MfsLoc {
    /// 条目所属目录的 inode 号 (COW 回写的锚点)。
    dir_ino: u32,
    /// 条目实际所在块 (目录节点本身, 或某个扩展目录块)。
    blk: u32,
    /// 块内偏移。
    off: usize,
}
const MFS_LOC_EMPTY: MfsLoc = MfsLoc {
    dir_ino: 0,
    blk: 0,
    off: 0,
};
/// 最近一次 `mfs_resolve` 命中的叶子条目位置 (调用方据此改/删条目)。
static mut MFS_LEAF: MfsLoc = MFS_LOC_EMPTY;

/// 打开文件描述符 (按路径而非 inode 记录: COW 后 inode 块会变, 每次操作重新解析
/// 路径即可始终指向最新版本, 避免句柄失效)。
#[derive(Clone, Copy)]
struct MfsFd {
    used: bool,
    is_dir: bool,
    path_len: u8,
    /// 打开时本服务服务的卷号 (M1b 多卷挂载: fd 类请求靠它找回该卷, 见服务循环)。
    vol: u64,
    /// 04b: 打开者身份与**打开时判定的有效权限位** (R/W/X)。已打开的 fd 在 `chmod`
    /// 之后仍按这份快照放行 —— 与 Unix 一致 (权限在 open 时判一次)。
    cred: Cred,
    perm: u8,
    path: [u8; TMP_PATH_MAX],
}
const MFS_FD_EMPTY: MfsFd = MfsFd {
    used: false,
    is_dir: false,
    path_len: 0,
    vol: 0,
    cred: Cred {
        uid: MFS_UID_ROOT,
        gid: MFS_GID_ROOT,
    },
    perm: 0,
    path: [0; TMP_PATH_MAX],
};
static mut MFS_FDS: [MfsFd; MFS_MAX_FD] = [MFS_FD_EMPTY; MFS_MAX_FD];

// ---------------------------------------------------------------------------
// 基础工具
// ---------------------------------------------------------------------------

fn mfs_at(buf: *const u8, off: usize) -> *const u8 {
    unsafe { buf.add(off) }
}
fn mfs_atm(buf: *mut u8, off: usize) -> *mut u8 {
    unsafe { buf.add(off) }
}
/// 计算并写入块头 CRC (payload 已填好)。
fn mfs_seal(buf: *mut u8, magic: u32) {
    write_u32(buf, magic);
    let crc = {
        let p = unsafe { core::slice::from_raw_parts(mfs_at(buf, MFS_HDR), MFS_PAYLOAD) };
        mfs_crc32(p)
    };
    write_u32(mfs_atm(buf, 4), crc);
}

/// 校验块头 magic 与 CRC。
fn mfs_ok(buf: *const u8, magic: u32) -> bool {
    if read_u32(buf) != magic {
        return false;
    }
    let stored = read_u32(mfs_at(buf, 4));
    let p = unsafe { core::slice::from_raw_parts(mfs_at(buf, MFS_HDR), MFS_PAYLOAD) };
    mfs_crc32(p) == stored
}

// ---------------------------------------------------------------------------
// 02b-2 写回攒批
//
// 所有块写 (`mfs_commit` 的 COW 内容块, `mfs_write_blk` 的位图数据 / 头块 / 超级块) 都先
// 拷进**写暂存窗**, 满窗或到强制落盘点才用一次 `block_batch` 下发 —— 把 N 次「提交-等完成」
// 并成 1 次。`mfs_commit_flush` 是唯一的落盘出口。
// ---------------------------------------------------------------------------
/// 写暂存窗当前块数。
static mut MFS_WB_COUNT: usize = 0;
/// 写暂存窗各块的块号。
static mut MFS_WB_BLOCKS: [u32; MFS_WB_PAGES] = [0; MFS_WB_PAGES];
static mut MFS_WB_WRITTEN: u64 = 0;
static mut MFS_WB_BATCHES: u64 = 0;
static mut MFS_WB_NEXT: u64 = 512;

/// 块 `block_no` 是否还在写暂存窗里 (写后读一致用)。
fn mfs_wb_has(block_no: u32) -> bool {
    unsafe {
        let n = MFS_WB_COUNT;
        let mut i = 0usize;
        while i < n {
            if MFS_WB_BLOCKS[i] == block_no {
                return true;
            }
            i += 1;
        }
    }
    false
}

/// 把 `src` 的内容攒进写暂存窗 (块号 `block_no`); 窗满先落盘。
fn mfs_wb_stage(block_no: u32, src: *const u8) -> bool {
    unsafe {
        if MFS_WB_COUNT >= MFS_WB_PAGES && !mfs_commit_flush() {
            return false;
        }
        let i = MFS_WB_COUNT;
        core::ptr::copy_nonoverlapping(src, mfs_wbuf(i), MFS_BLOCK);
        MFS_WB_BLOCKS[i] = block_no;
        MFS_WB_COUNT = i + 1;
        true
    }
}

/// 把写暂存窗一次批写落盘 (block_batch 不行则退回逐块直写)。无待写时直接返回 true。
/// **唯一的落盘出口** —— 强制落盘点 (GC / fsck / 换卷 / 裸读) 都调它。
fn mfs_commit_flush() -> bool {
    unsafe {
        let n = MFS_WB_COUNT;
        if n == 0 {
            return true;
        }
        let vol = MFS_CUR_VOL;
        let mut i = 0usize;
        while i < n {
            core::ptr::write_unaligned(
                mfs_wb_desc().add(i),
                BatchEnt {
                    lba: MFS_WB_BLOCKS[i] as u64 * MFS_SECTORS_PER_BLOCK as u64,
                    sectors: MFS_SECTORS_PER_BLOCK as u64,
                    buf: mfs_wbuf(i) as u64,
                },
            );
            i += 1;
        }
        let mut ok = block_batch(vol, mfs_wb_desc(), n, true) == 1;
        if !ok {
            ok = true;
            i = 0;
            while i < n {
                if !block_raw_write(
                    vol,
                    MFS_WB_BLOCKS[i] * MFS_SECTORS_PER_BLOCK as u32,
                    MFS_SECTORS_PER_BLOCK,
                    mfs_wbuf(i),
                ) {
                    ok = false;
                    break;
                }
                i += 1;
            }
        }
        MFS_WB_WRITTEN += n as u64;
        MFS_WB_BATCHES += 1;
        MFS_WB_COUNT = 0;
        if MFS_WB_WRITTEN >= MFS_WB_NEXT {
            print("mfs-wb: blocks=");
            print_u64(MFS_WB_WRITTEN);
            print(" batches=");
            print_u64(MFS_WB_BATCHES);
            print(" avg=");
            print_u64(MFS_WB_WRITTEN / MFS_WB_BATCHES.max(1));
            println("");
            MFS_WB_NEXT += 512;
        }
        ok
    }
}

fn mfs_read_blk(block_no: u32, dst: *mut u8) -> bool {
    // 写后读一致: 目标块还在写暂存窗里, 先落盘再读。
    if mfs_wb_has(block_no) && !mfs_commit_flush() {
        return false;
    }
    block_read_dev(
        unsafe { MFS_CUR_VOL },
        block_no * MFS_SECTORS_PER_BLOCK as u32,
        MFS_SECTORS_PER_BLOCK,
        dst,
    )
}
fn mfs_write_blk(block_no: u32, src: *const u8) -> bool {
    mfs_wb_stage(block_no, src)
}

/// 探测**任意卷** `vol` 的超级块: 只要能读出一份有效的 MFS 超级块, 就返回其中的主卷
/// 序号 (0 = 是 MFS, 只是还没被标成主卷); 两份都读不出 / 根本不是 MFS 时返回 `None`。
///
/// 「不是 MFS」与「是 MFS 但序号为 0」必须分得开: 前者不能当主卷目标 (要拒绝),
/// 后者可以 (它只是还没标记过) —— 见 `mfs_set_primary_volume`。
///
/// 直接用 `block_read_dev` 指定卷号, 与本服务的「当前卷」无关 —— 认领主卷要把**所有**
/// MFS 卷扫一遍, 不能靠切内存态 (那会把正在服务的卷换掉)。两份副本取较大者: 一次提交
/// 把两份写成同一序号, 崩溃在中途时落后的那份序号必不大于新的, 取大即取新。
///
/// 缓冲借用位图头块页 (`mfs_bmph_buf`) —— 该页只在 `mfs_bmp_flush` 内部使用, 而本函数
/// 只从启动认领与 `mkfs` / `set-primary` 的**提交之外**环节调用, 与提交路径不重叠。
fn mfs_sb_probe(vol: u64) -> Option<u64> {
    // 裸读超级块前先落盘 (写暂存窗里可能有更早的写)。
    if !mfs_commit_flush() {
        return None;
    }
    let buf = mfs_bmph_buf();
    let mut best: Option<u64> = None;
    for copy in 0..MFS_SB_COPIES {
        let lba = copy * MFS_SECTORS_PER_BLOCK as u32;
        if !block_read_dev(vol, lba, MFS_SECTORS_PER_BLOCK, buf) {
            continue;
        }
        if !mfs_ok(buf, MFS_MAGIC_SUPER) {
            continue;
        }
        let p = MFS_HDR;
        if read_u32(mfs_at(buf, p)) != MFS_VERSION {
            continue;
        }
        let s = read_u64(mfs_at(buf, p + MFS_SB_PRIMARY));
        best = Some(best.map_or(s, |b: u64| b.max(s)));
    }
    best
}

/// 卷 `vol` 的主卷序号 (0 = 非主卷 / 不是 MFS)。
fn mfs_primary_of_vol(vol: u64) -> u64 {
    mfs_sb_probe(vol).unwrap_or(0)
}

/// 目标卷 `target` 应得的主卷序号: 现有**所有** MFS 卷与 `target` 自身的最大序号 + 1。
///
/// 只加 1、不回写别的卷 —— 序号只增, 「最大者胜出」由认领端 (`mfs_vol_claim`) 保证。
/// 重复格式化同一块卷会让它继续胜出 (序号同样 +1), 正是「最近 mkfs 过的卷当主卷」。
fn mfs_next_primary_serial(scratch: *mut u8, target: u64) -> u64 {
    // 目标卷此刻的 kind 可能还是卷表里的旧值 (卷表在启动时就冻结了, 之后 mkfs 出来的
    // 卷在表里仍是 unknown), 故先按卷号直接读一次, 与卷表无关。
    let mut max = mfs_primary_of_vol(target);
    let n = block_list_volumes(scratch, VOL_MAX as u32);
    if n != 0 && n != u64::MAX {
        let esize = core::mem::size_of::<VolumeDesc>();
        let mut i = 0u64;
        while i < n {
            let d = unsafe {
                core::ptr::read_unaligned(scratch.add(i as usize * esize) as *const VolumeDesc)
            };
            if d.kind == VOL_KIND_MFS && d.id as u64 != target {
                max = max.max(mfs_primary_of_vol(d.id as u64));
            }
            i += 1;
        }
    }
    max + 1
}

// ---------------------------------------------------------------------------
// 空闲位图 (块分配 / 空间回收)
// ---------------------------------------------------------------------------
// MFS2 用一块常驻位图记录块占用 (1 = 占用, 0 = 空闲), 位图本身随超级块 COW
// 交替写入两份副本, 故 CRC 一并保护。分配不再「只增不减」: 旧块的可达性由 GC
// 判定, 不可达的老版本块会被归还给空闲池。

fn mfs_bmp_byte(i: usize) -> *mut u8 {
    (MFS_BMP_VADDR as *mut u8).wrapping_add(i)
}
fn mfs_mark_byte(i: usize) -> *mut u8 {
    (MFS_MARK_VADDR as *mut u8).wrapping_add(i)
}
fn mfs_seen_byte(i: usize) -> *mut u8 {
    (MFS_SEEN_VADDR as *mut u8).wrapping_add(i)
}
/// 位图需覆盖的字节数 (按当前卷总块数动态算)。
fn mfs_bmp_bytes() -> usize {
    (unsafe { MFS_TOTAL_BLOCKS } as usize).div_ceil(8)
}
/// 把块 `b` 所在的位图数据块标脏 (提交时只落盘变动过的区间)。
fn mfs_bmp_touch(b: u32) {
    let c = (b / MFS_BMP_BLOCK_SPAN) as usize;
    if c < MFS_MAX_BMP_DATA_BLOCKS as usize {
        unsafe {
            *core::ptr::addr_of_mut!(MFS_BMP_DIRTY)
                .cast::<u8>()
                .add(c >> 3) |= 1u8 << (c & 7);
        }
    }
}
/// 清空整个标记窗口 (GC 每轮的起点)。
fn mfs_mark_clear_all() {
    for i in 0..mfs_bmp_bytes() {
        unsafe {
            *mfs_mark_byte(i) = 0;
        }
    }
}
/// 清空整个「本根已访问」窗口 (每换一个可达根都要清)。
fn mfs_seen_clear_all() {
    for i in 0..mfs_bmp_bytes() {
        unsafe {
            *mfs_seen_byte(i) = 0;
        }
    }
}
/// 本根是否已访问过块 `b` (去重与防环)。
fn mfs_seen_get(b: u32) -> bool {
    if b >= unsafe { MFS_TOTAL_BLOCKS } {
        return false;
    }
    let i = b as usize;
    unsafe { *mfs_seen_byte(i >> 3) & (1u8 << (i & 7)) != 0 }
}
fn mfs_seen_set(b: u32) {
    if b < unsafe { MFS_TOTAL_BLOCKS } {
        let i = b as usize;
        unsafe {
            *mfs_seen_byte(i >> 3) |= 1u8 << (i & 7);
        }
    }
}

// --- fsck 的可达 inode 位图 (按 ino 索引, 独立窗口) ---
fn mfs_fsck_byte(i: usize) -> *mut u8 {
    unsafe { (MFS_FSCK_VADDR as *mut u8).add(i) }
}
/// 铺开 fsck 的 inode 位图窗口 (覆盖整个 `MFS_INO_MAX`); 失败返回 false。
fn mfs_fsck_win_ensure() -> bool {
    while unsafe { MFS_FSCK_MAPPED } < MFS_FSCK_PAGES {
        let n = unsafe { MFS_FSCK_MAPPED } as u64;
        if sys_alloc_page(MFS_FSCK_VADDR + n * MFS_BLOCK as u64) != 1 {
            return false;
        }
        unsafe {
            MFS_FSCK_MAPPED += 1;
        }
    }
    true
}
fn mfs_fsck_get(ino: u32) -> bool {
    let i = ino as usize;
    if i >= MFS_INO_MAX as usize {
        return false;
    }
    unsafe { *mfs_fsck_byte(i >> 3) & (1u8 << (i & 7)) != 0 }
}
fn mfs_fsck_set(ino: u32) {
    let i = ino as usize;
    if i < MFS_INO_MAX as usize {
        unsafe {
            *mfs_fsck_byte(i >> 3) |= 1u8 << (i & 7);
        }
    }
}
fn mfs_fsck_clear_all() {
    for i in 0..(MFS_INO_MAX as usize).div_ceil(8) {
        unsafe {
            *mfs_fsck_byte(i) = 0;
        }
    }
}

fn mfs_stack_slot(i: usize) -> *mut u32 {
    unsafe { core::ptr::addr_of_mut!(MFS_GC_STACK).cast::<u32>().add(i) }
}

fn mfs_bmp_get(b: u32) -> bool {
    if b >= unsafe { MFS_TOTAL_BLOCKS } {
        return false;
    }
    let i = b as usize;
    unsafe { *mfs_bmp_byte(i >> 3) & (1u8 << (i & 7)) != 0 }
}
fn mfs_bmp_set(b: u32) {
    if b >= unsafe { MFS_TOTAL_BLOCKS } {
        return;
    }
    let i = b as usize;
    let p = mfs_bmp_byte(i >> 3);
    unsafe {
        if *p & (1u8 << (i & 7)) == 0 {
            *p |= 1u8 << (i & 7);
            MFS_FREE_BLOCKS = MFS_FREE_BLOCKS.saturating_sub(1);
            mfs_bmp_touch(b);
        }
    }
}
fn mfs_bmp_clear(b: u32) {
    if b >= unsafe { MFS_TOTAL_BLOCKS } {
        return;
    }
    let i = b as usize;
    let p = mfs_bmp_byte(i >> 3);
    unsafe {
        if *p & (1u8 << (i & 7)) != 0 {
            *p &= !(1u8 << (i & 7));
            MFS_FREE_BLOCKS += 1;
            mfs_bmp_touch(b);
        }
    }
}
/// 按位图重算空闲块数 (挂载校验用; 位图是唯一依据, 不信任落盘计数)。
fn mfs_bmp_recount() -> u32 {
    let total = unsafe { MFS_TOTAL_BLOCKS } as usize;
    let mut used = 0usize;
    for b in 0..total {
        if mfs_bmp_get(b as u32) {
            used += 1;
        }
    }
    let free = (total - used) as u32;
    unsafe {
        MFS_FREE_BLOCKS = free;
    }
    free
}

/// 把一个 inode 表索引块及其引用的所有表块强制标为占用。
///
/// 挂载时用它保护元数据: 当前表与每张快照的表都是可达根的元数据, 一旦被当作空闲
/// 分配出去, 对应根的翻译就会失效 (GC 随之失败)。读失败 (块不在盘上 / CRC 坏) 时
/// 只保留索引块本身占位 —— 交给后续 GC 判定, 不在这里阻塞挂载。
fn mfs_mark_itab_meta(itab: u32) {
    if itab == 0 {
        return;
    }
    mfs_bmp_set(itab);
    let x = mfs_itabx_buf();
    if !mfs_read_blk(itab, x) || !mfs_ok(x, MFS_MAGIC_ITABX) {
        return;
    }
    for k in 0..MFS_ITAB_SLOTS {
        let t = read_u32(mfs_at(x, MFS_HDR + k * 4));
        if t != 0 {
            mfs_bmp_set(t);
        }
    }
}

/// 位图数据块数 `bb = ceil(total / 32768)` (一个数据块 = 4096 字节位图 = 32768 块)。
fn mfs_bb_for(total: u32) -> u32 {
    total.div_ceil(MFS_BMP_BLOCK_SPAN)
}

/// 数据区起点 (块 0/1 超级块 + 块 2/3 位图头 + 两副本位图数据); 之前的块一律强制占用。
fn mfs_data_start() -> u32 {
    4 + 2 * mfs_bb_for(unsafe { MFS_TOTAL_BLOCKS })
}

/// 从位图中取一个空闲块, **不**触发回收; 空间耗尽返回 None。
///
/// 这里不做 GC: 调用点常在一次 COW 操作中间 (已分配但尚未被根引用的块), GC 会把
/// 它们误判成垃圾。回收改在服务空闲时分派前统一触发 (见 `mfs_maybe_gc`)。
fn mfs_alloc_block() -> Option<u32> {
    let total = unsafe { MFS_TOTAL_BLOCKS };
    let start = mfs_data_start();
    if total <= start {
        return None;
    }
    // 先扫 [游标, 末尾), 再回头扫 [数据区起点, 游标), 避免每次都从头找。
    let hint = unsafe { MFS_ALLOC_NEXT }.clamp(start, total);
    let mut b = hint;
    while b < total {
        if !mfs_bmp_get(b) {
            return Some(mfs_bmp_take(b));
        }
        b += 1;
    }
    b = start;
    while b < hint {
        if !mfs_bmp_get(b) {
            return Some(mfs_bmp_take(b));
        }
        b += 1;
    }
    None
}

/// 占用块 `b` 并把分配游标推到其后。
fn mfs_bmp_take(b: u32) -> u32 {
    mfs_bmp_set(b);
    unsafe {
        MFS_ALLOC_NEXT = b + 1;
    }
    b
}

// ---------------------------------------------------------------------------
// 空间回收 (mark & sweep)
// ---------------------------------------------------------------------------

/// 在标记位图中置位 `b` (越界忽略)。
fn mfs_mark_set(b: u32, total: usize) {
    let i = b as usize;
    if i >= total {
        return;
    }
    unsafe {
        *mfs_mark_byte(i >> 3) |= 1u8 << (i & 7);
    }
}
fn mfs_mark_get(b: u32) -> bool {
    if b >= unsafe { MFS_TOTAL_BLOCKS } {
        return false;
    }
    let i = b as usize;
    unsafe { *mfs_mark_byte(i >> 3) & (1u8 << (i & 7)) != 0 }
}

/// GC 遍历栈顶指针的裸指针 (避免对 `static mut` 造 `&mut`, 那属于未定义行为)。
fn mfs_gc_sp_ptr() -> *mut usize {
    core::ptr::addr_of_mut!(MFS_GC_SP)
}
fn mfs_gc_sp_get() -> usize {
    unsafe { *mfs_gc_sp_ptr() }
}
fn mfs_gc_sp_set(v: usize) {
    unsafe {
        *mfs_gc_sp_ptr() = v;
    }
}

/// 把块 `b` 压入遍历栈并标记可达; 0 / 越界 / **本根已访问**的块直接跳过。栈满 false。
///
/// 去重用「本根已访问」位图而不是可达位图: 同一个块在不同根的表下会展开出不同的子树。
fn mfs_gc_push(b: u32, total: usize) -> bool {
    if b == 0 || b as usize >= total || mfs_seen_get(b) {
        return true;
    }
    let sp = mfs_gc_sp_get();
    if sp >= MFS_GC_STACK_MAX {
        return false;
    }
    unsafe {
        *mfs_stack_slot(sp) = b;
    }
    mfs_gc_sp_set(sp + 1);
    mfs_seen_set(b);
    mfs_mark_set(b, total);
    true
}

/// 与 `mfs_gc_push` 相同, 但**只**置「本根已访问」(不污染可达位图) —— fsck 统计某个
/// 泄漏 inode 名下块时用: 可达位图此时正保存着 GC 的全根标记结果, 不能被覆盖。
fn mfs_gc_push_seen(b: u32, total: usize) -> bool {
    if b == 0 || b as usize >= total || mfs_seen_get(b) {
        return true;
    }
    let sp = mfs_gc_sp_get();
    if sp >= MFS_GC_STACK_MAX {
        return false;
    }
    unsafe {
        *mfs_stack_slot(sp) = b;
    }
    mfs_gc_sp_set(sp + 1);
    mfs_seen_set(b);
    true
}

/// 载入某个可达根的 inode 表索引块到 X 缓冲 (GC 期间该缓冲专供此事), 并标记索引块
/// 与它引用的所有表块 —— 它们是元数据, 必须视为可达, 否则会被回收后重新分配出去。
fn mfs_gc_load_itab(itab: u32, total: usize) -> bool {
    if itab == 0 || itab as usize >= total {
        return false;
    }
    mfs_mark_set(itab, total);
    let x = mfs_itabx_buf();
    if !mfs_read_blk(itab, x) || !mfs_ok(x, MFS_MAGIC_ITABX) {
        return false;
    }
    for k in 0..MFS_ITAB_SLOTS {
        let t = read_u32(mfs_at(x, MFS_HDR + k * 4));
        if t == 0 {
            continue;
        }
        if t as usize >= total {
            return false;
        }
        mfs_mark_set(t, total);
    }
    true
}

/// GC 期间用「当前正在遍历的那个根的 inode 表」翻译 ino -> 块号。
///
/// 快照必须用它自己的表: 同一个 ino 在快照里指向的是当时的对象块, 用当前表翻译会
/// 把快照内容识别成最新版本, 从而漏标真正的历史块。
fn mfs_gc_ino_block(ino: u32) -> Option<u32> {
    if ino == 0 || ino >= MFS_INO_MAX {
        return None;
    }
    let (k, j) = mfs_ino_slot(ino);
    let x = mfs_itabx_buf();
    let t = read_u32(mfs_at(x, MFS_HDR + k as usize * 4));
    if t == 0 {
        return Some(0);
    }
    let tb = mfs_gc_tab_buf();
    if !mfs_read_blk(t, tb) || !mfs_ok(tb, MFS_MAGIC_ITAB) {
        return None;
    }
    Some(read_u32(mfs_at(tb, MFS_HDR + j * 4)))
}

/// 空间回收 (标记阶段): 从当前根 + 所有快照根出发标记可达块。
///
/// COW 只增不减时, 被新版本取代的旧块会一直占在位图里; 可达性的唯一判据是**从根可达**
/// —— 快照根同样算根, 因此快照仍引用的历史版本 (含它自己的 inode 表) 不会被回收
/// (回滚依旧可用)。成功后 `MFS_MARK` 窗口保存完整的可达块集合 (供 fsck 复用)。
/// 遍历失败返回 false, 不改动位图。
fn mfs_gc_mark() -> bool {
    let total = unsafe { MFS_TOTAL_BLOCKS } as usize;
    if total == 0 || total > MFS_MAX_BLOCKS as usize {
        return false;
    }
    mfs_mark_clear_all();
    // 元数据区 (超级块 / 位图头 / 位图数据块) 一律视为可达, 绝不回收。
    let meta_end = mfs_data_start();
    for b in 0..meta_end {
        mfs_mark_set(b, total);
    }
    // 可达根 = (当前 inode 表, 根 ino) + 每个快照的 (表, 根 ino); 根号恒为 1。
    let sn = unsafe { MFS_SNAP_COUNT };
    let mut roots = [(0u32, 0u32); MFS_MAX_SNAP + 1];
    roots[0] = (unsafe { MFS_ITAB }, MFS_ROOT_INO);
    for (i, slot) in roots.iter_mut().enumerate().take(sn + 1).skip(1) {
        slot.0 = mfs_snap(i - 1).itab;
        slot.1 = MFS_ROOT_INO;
    }
    for &(itab, root_ino) in roots.iter().take(sn + 1) {
        if !mfs_gc_load_itab(itab, total) {
            return false;
        }
        // 换根: 清空「本根已访问」, 保证这棵树用**它自己的表**重新展开一遍。
        mfs_seen_clear_all();
        let rblk = match mfs_gc_ino_block(root_ino) {
            Some(b) if b != 0 => b,
            _ => return false,
        };
        if !mfs_gc_push(rblk, total) {
            return false;
        }
        if !mfs_gc_drain(total) {
            return false;
        }
    }
    true
}

/// 空间回收 (清扫阶段): 用标记位图重建空闲位图, 落盘并返回本次回收的块数; 失败 `u64::MAX`。
fn mfs_gc_sweep() -> u64 {
    let total = unsafe { MFS_TOTAL_BLOCKS } as usize;
    let free_before = unsafe { MFS_FREE_BLOCKS };
    let mut used = 0u32;
    for b in 0..total {
        if mfs_mark_get(b as u32) {
            used += 1;
            mfs_bmp_set(b as u32);
        } else {
            mfs_bmp_clear(b as u32);
        }
    }
    unsafe {
        MFS_FREE_BLOCKS = total as u32 - used;
        MFS_ALLOC_NEXT = mfs_data_start();
    }
    if !mfs_bmp_flush() {
        return u64::MAX;
    }
    (total as u32 - used).saturating_sub(free_before) as u64
}

/// 空间回收: 标记 (当前根 + 全部快照) 后清扫不可达块。返回回收块数; 失败 `u64::MAX`。
fn mfs_gc() -> u64 {
    // 强制落盘: GC 搬运块前必须让写暂存窗里的写先出去。
    if !mfs_commit_flush() {
        return 0;
    }
    // 02b: GC 会搬运/回收块 —— 缓存里的 (块, 偏移) 一律作废。
    mfs_didx_invalidate_all();
    if !mfs_gc_mark() {
        return u64::MAX;
    }
    mfs_gc_sweep()
}

/// 最小 fsck: 对账「已分配但不可达」的 inode 槽 (01)。
///
/// - 默认**只报不修**: 回复 `(泄漏 inode 数 << 32) | 该名下可回收块数)`; 不写盘。
/// - `repair` 才回收: 清掉泄漏 inode 的槽, 再让 `mfs_gc()` 按可达性安全回收块
///   (快照仍引用的历史版本不会被收), 回复 `(泄漏 inode 数 << 32) | 实际回收块数`。
///
/// 失败返回 `u64::MAX`。可达性以**当前根目录树**为准 (快照不算当前命名空间的可达性),
/// 但块的可回收性仍按 GC 的全根可达性判定 —— 二者不可混用。
fn mfs_fsck(repair: bool) -> u64 {
    if !mfs_commit_flush() {
        return u64::MAX;
    }
    let total = unsafe { MFS_TOTAL_BLOCKS } as usize;
    if total == 0 || total > MFS_MAX_BLOCKS as usize {
        return u64::MAX;
    }
    if !mfs_fsck_win_ensure() {
        println("mfs: fsck alloc inode bitmap FAILED");
        return u64::MAX;
    }
    // (1) 沿当前根目录树标记可达 inode。
    if !mfs_fsck_walk_inos() {
        println("mfs: fsck walk FAILED");
        return u64::MAX;
    }
    // (2) 标记全部可达块 (含快照) —— 判定泄漏 inode 名下哪些块真可回收。
    if !mfs_gc_mark() {
        println("mfs: fsck mark FAILED");
        return u64::MAX;
    }
    // (3) 扫已分配 inode 槽, 找不可达者, 统计其名下「已分配但不可达」的块。
    let bound = mfs_fsck_scan_bound();
    let mut leaked_inos = 0u32;
    let mut leaked_blocks = 0u32;
    for ino in 1..bound {
        if ino == MFS_ROOT_INO {
            continue;
        }
        let blk = match mfs_ino_block(ino) {
            Some(b) => b,
            None => return u64::MAX,
        };
        if blk == 0 || mfs_fsck_get(ino) {
            continue;
        }
        leaked_inos += 1;
        leaked_blocks = leaked_blocks.saturating_add(mfs_fsck_owned_blocks(blk, total));
    }
    if leaked_inos > 0 {
        print("mfs: fsck leaked inodes=");
        print_u64(leaked_inos as u64);
        print(" blocks=");
        print_u64(leaked_blocks as u64);
        println(if repair {
            " (repairing)"
        } else {
            " (report only; use --repair to reclaim)"
        });
    }
    if !repair || leaked_inos == 0 {
        return ((leaked_inos as u64) << 32) | leaked_blocks as u64;
    }
    // (4) 修复: 清掉泄漏 inode 的槽, 再让 GC 按其全根可达性回收并落盘。
    for ino in 1..bound {
        if ino == MFS_ROOT_INO {
            continue;
        }
        let blk = match mfs_ino_block(ino) {
            Some(b) => b,
            None => return u64::MAX,
        };
        if blk == 0 || mfs_fsck_get(ino) {
            continue;
        }
        if !mfs_free_ino(ino) {
            return u64::MAX;
        }
    }
    let freed = mfs_gc();
    if freed == u64::MAX {
        return u64::MAX;
    }
    ((leaked_inos as u64) << 32) | freed
}

/// fsck 扫描 inode 槽的上界: 覆盖分配游标与已分配计数, 至少到 root+1, 上限 `MFS_INO_MAX`。
fn mfs_fsck_scan_bound() -> u32 {
    let hint = unsafe { MFS_INO_HINT };
    let cnt = unsafe { MFS_INO_COUNT };
    hint.max(cnt)
        .saturating_add(1)
        .clamp(MFS_ROOT_INO + 1, MFS_INO_MAX)
}

/// fsck: 从根目录出发, 沿**目录树**标记可达 inode (文件 / 软链接作为叶子, 不展开其内容)。
///
/// 复用 GC 的遍历栈与 SEEN 窗口 (此处 SEEN 用于目录块去重与防环)。结构不可信返回 false。
fn mfs_fsck_walk_inos() -> bool {
    let total = unsafe { MFS_TOTAL_BLOCKS } as usize;
    mfs_fsck_clear_all();
    mfs_fsck_set(MFS_ROOT_INO);
    mfs_gc_sp_set(0);
    mfs_seen_clear_all();
    let root = match mfs_ino_block(MFS_ROOT_INO) {
        Some(b) if b != 0 => b,
        _ => return false,
    };
    if !mfs_gc_push(root, total) {
        return false;
    }
    let gb = mfs_gc_buf();
    while mfs_gc_sp_get() > 0 {
        let sp = mfs_gc_sp_get() - 1;
        mfs_gc_sp_set(sp);
        let b = unsafe { *mfs_stack_slot(sp) };
        if !mfs_read_blk(b, gb) {
            return false;
        }
        if mfs_ok(gb, MFS_MAGIC_DIR) {
            let end = MFS_HDR + MFS_PAYLOAD;
            let mut off = MFS_HDR + MFS_DIR_HDR;
            while off + MFS_DIR_ENT_HDR <= end {
                if mfs_ent_name_len(gb, off) != 0 {
                    let child = mfs_ent_ino(gb, off);
                    if child != 0 && child < MFS_INO_MAX {
                        mfs_fsck_set(child);
                        match mfs_ino_block(child) {
                            Some(cb) if cb != 0 => {
                                if mfs_node_type(cb) == Some(MFS_TYPE_DIR)
                                    && !mfs_gc_push(cb, total)
                                {
                                    return false;
                                }
                            }
                            Some(_) => {}
                            None => return false,
                        }
                    }
                }
                match mfs_ent_step(gb, off) {
                    Some(n) => off = n,
                    None => return false,
                }
            }
            if !mfs_gc_push(mfs_dir_ext(gb), total) {
                return false;
            }
        } else if mfs_ok(gb, MFS_MAGIC_DIDX) {
            for i in 0..MFS_DIR_SLOTS {
                if !mfs_gc_push(read_u32(mfs_at(gb, MFS_HDR + i * 4)), total) {
                    return false;
                }
            }
        } else {
            return false;
        }
    }
    true
}

/// 统计对象块 `blk` (某泄漏 inode 的节点) 及其**自身拥有**的块中, 「已分配且不可达」的数量。
///
/// 目录只沿扩展链展开, **不进入子项** (子项归属它们自己的 ino); 文件展开数据/间接块。
/// 此时 `MFS_MARK` 保存着 GC 的全根可达集 (调用方保证), 故 `!mfs_mark_get` 即真可回收。
fn mfs_fsck_owned_blocks(blk: u32, total: usize) -> u32 {
    mfs_seen_clear_all();
    mfs_gc_sp_set(0);
    if !mfs_gc_push_seen(blk, total) {
        return 0;
    }
    let gb = mfs_gc_buf();
    let mut n = 0u32;
    while mfs_gc_sp_get() > 0 {
        let sp = mfs_gc_sp_get() - 1;
        mfs_gc_sp_set(sp);
        let b = unsafe { *mfs_stack_slot(sp) };
        if mfs_bmp_get(b) && !mfs_mark_get(b) {
            n = n.saturating_add(1);
        }
        if !mfs_read_blk(b, gb) {
            continue;
        }
        if mfs_ok(gb, MFS_MAGIC_DIR) {
            if !mfs_gc_push_seen(mfs_dir_ext(gb), total) {
                return n;
            }
        } else if mfs_ok(gb, MFS_MAGIC_DIDX) {
            for i in 0..MFS_DIR_SLOTS {
                if !mfs_gc_push_seen(read_u32(mfs_at(gb, MFS_HDR + i * 4)), total) {
                    return n;
                }
            }
        } else if mfs_ok(gb, MFS_MAGIC_FILE) {
            for i in 0..MFS_FILE_DIRECT {
                if !mfs_gc_push_seen(mfs_file_direct(gb, i), total) {
                    return n;
                }
            }
            if !mfs_gc_push_seen(mfs_file_ind1(gb), total)
                || !mfs_gc_push_seen(mfs_file_ind2(gb), total)
                || !mfs_gc_push_seen(mfs_file_ind3(gb), total)
            {
                return n;
            }
        } else if mfs_ok(gb, MFS_MAGIC_IND)
            || mfs_ok(gb, MFS_MAGIC_IND2)
            || mfs_ok(gb, MFS_MAGIC_IND3)
        {
            for i in 0..MFS_IND_CAP {
                if !mfs_gc_push_seen(mfs_ind_slot(gb, i), total) {
                    return n;
                }
            }
        }
        // 数据块 / 软链接: 叶子, 无子块。
    }
    n
}

/// 深度优先展开遍历栈: 目录展开其子项 (条目存 ino, 需经当前根的表翻译),
/// 文件展开其数据块, 数据块是叶子。
fn mfs_gc_drain(total: usize) -> bool {
    let gb = mfs_gc_buf();
    while mfs_gc_sp_get() > 0 {
        let sp = mfs_gc_sp_get() - 1;
        mfs_gc_sp_set(sp);
        let b = unsafe { *mfs_stack_slot(sp) };
        if !mfs_read_blk(b, gb) {
            return false;
        }
        if mfs_ok(gb, MFS_MAGIC_DIR) {
            // 变长条目: 按 rec_len 逐个走, 有效条目的 ino 翻译成块后入栈展开;
            // 目录的扩展索引块同样可达 (它下面挂着扩展目录块)。
            let end = MFS_HDR + MFS_PAYLOAD;
            let mut off = MFS_HDR + MFS_DIR_HDR;
            while off + MFS_DIR_ENT_HDR <= end {
                if mfs_ent_name_len(gb, off) != 0 {
                    let child = match mfs_gc_ino_block(mfs_ent_ino(gb, off)) {
                        Some(x) => x,
                        None => return false,
                    };
                    if !mfs_gc_push(child, total) {
                        return false;
                    }
                }
                match mfs_ent_step(gb, off) {
                    Some(n) => off = n,
                    // 结构不可信: 放弃本次回收 (宁可漏回收, 也不能把在用的块发出去)。
                    None => return false,
                }
            }
            if !mfs_gc_push(mfs_dir_ext(gb), total) {
                return false;
            }
        } else if mfs_ok(gb, MFS_MAGIC_DIDX) {
            // 目录扩展索引块: 槽位全是扩展目录块。
            for i in 0..MFS_DIR_SLOTS {
                if !mfs_gc_push(read_u32(mfs_at(gb, MFS_HDR + i * 4)), total) {
                    return false;
                }
            }
        } else if mfs_ok(gb, MFS_MAGIC_FILE) {
            // 文件节点: 直接槽逐个标记 (未用槽恒为 0), 一/二/三级间接块入栈展开。
            // 不看 nblocks: 计数若损坏, 少标就会把仍在用的数据块回收掉。
            for i in 0..MFS_FILE_DIRECT {
                let c = mfs_file_direct(gb, i);
                if c != 0 {
                    mfs_mark_set(c, total);
                }
            }
            // ind3 必须一并入栈: 漏标会让三级块被当作垃圾回收, 进而损坏 >4 GiB 文件。
            if !mfs_gc_push(mfs_file_ind1(gb), total)
                || !mfs_gc_push(mfs_file_ind2(gb), total)
                || !mfs_gc_push(mfs_file_ind3(gb), total)
            {
                return false;
            }
        } else if mfs_ok(gb, MFS_MAGIC_IND) {
            // 一级间接块: 槽位全是数据块 (叶子)。
            for i in 0..MFS_IND_CAP {
                let c = mfs_ind_slot(gb, i);
                if c != 0 {
                    mfs_mark_set(c, total);
                }
            }
        } else if mfs_ok(gb, MFS_MAGIC_IND2) {
            // 二级间接块: 槽位全是一级间接块。
            for i in 0..MFS_IND_CAP {
                if !mfs_gc_push(mfs_ind_slot(gb, i), total) {
                    return false;
                }
            }
        } else if mfs_ok(gb, MFS_MAGIC_IND3) {
            // 三级间接块: 槽位全是二级间接块。
            for i in 0..MFS_IND_CAP {
                if !mfs_gc_push(mfs_ind_slot(gb, i), total) {
                    return false;
                }
            }
        } else if mfs_ok(gb, MFS_MAGIC_LINK) {
            // 软链接节点 (M5c): 目标内联在 payload 里, 不引用任何其它块 —— 到这一层
            // 就算展开完了。少了这一支会掉进下面的 else 被判成「盘上结构不可信」,
            // 于是**只要卷上存在软链接, 整次回收都会被放弃**。
        } else {
            // 可达块却既不是目录也不是文件节点: 说明盘上结构不可信, 放弃本次回收
            // (宁可漏回收, 也不能把仍被引用的块分配出去)。
            return false;
        }
    }
    true
}

/// 低水位回收: 空闲块少于 `总块数 / MFS_GC_LOW_WATER_DIV` 时回收一次。
///
/// 只在**没有在建 COW 操作**时分派请求之前调用 (见 `mfs_main` 主循环)。
fn mfs_maybe_gc() {
    let total = unsafe { MFS_TOTAL_BLOCKS };
    if total == 0 || unsafe { MFS_FREE_BLOCKS } > total / MFS_GC_LOW_WATER_DIV {
        return;
    }
    if mfs_gc() == u64::MAX {
        println("mfs: gc FAILED");
    }
}

/// 封装并写入一个新块, 返回块号 (COW 的基本操作)。
fn mfs_commit(buf: *mut u8, magic: u32) -> Option<u32> {
    mfs_seal(buf, magic);
    let b = mfs_alloc_block()?;
    if !mfs_write_blk(b, buf) {
        return None;
    }
    Some(b)
}

// ---------------------------------------------------------------------------
// inode 表 (ino -> 对象块号)
// ---------------------------------------------------------------------------
//
// 索引块 (MFIX) 有 `MFS_ITAB_SLOTS` 个槽, 第 k 个槽指向第 k 个表块 (MFIT); 表块有
// `MFS_ITAB_SLOTS` 个槽, 第 j 个槽指向 ino = k * SLOTS + j 的对象块。索引块内容在
// 内存里留一份完整镜像 (`MFS_ITAB_MEM`), 改动时整体 COW 成新索引块 —— 这样查表
// 不需要读索引块, 只有表块要读 (单条目缓存让连续 ino 只读一次)。
//
// 表块 / 索引块都是普通分配块, 故 GC 必须把它们当作可达块标记 (见 `mfs_gc`)。

/// 索引块内容的内存镜像 (slot k = 第 k 个表块号, 0 = 未分配)。
static mut MFS_ITAB_MEM: [u32; MFS_ITAB_SLOTS] = [0; MFS_ITAB_SLOTS];

/// ino 落在哪个表块的第几个槽。
fn mfs_ino_slot(ino: u32) -> (u32, usize) {
    let n = ino as usize;
    ((n / MFS_ITAB_SLOTS) as u32, n % MFS_ITAB_SLOTS)
}

fn mfs_itab_table(k: u32) -> u32 {
    if k as usize >= MFS_ITAB_SLOTS {
        return 0;
    }
    unsafe {
        *core::ptr::addr_of!(MFS_ITAB_MEM)
            .cast::<u32>()
            .add(k as usize)
    }
}
fn mfs_set_itab_table(k: u32, blk: u32) {
    if (k as usize) < MFS_ITAB_SLOTS {
        unsafe {
            *core::ptr::addr_of_mut!(MFS_ITAB_MEM)
                .cast::<u32>()
                .add(k as usize) = blk;
        }
    }
}

/// 读 ino 对应的对象块号 (0 = 未分配); 结构不可信 / 越界返回 None。
///
/// 单条目表块缓存: 连续 ino (create 分配、readdir 遍历) 通常落在同一表块, 只读一次。
fn mfs_ino_block(ino: u32) -> Option<u32> {
    if ino == 0 || ino >= MFS_INO_MAX {
        return None;
    }
    let (k, j) = mfs_ino_slot(ino);
    let t = mfs_itab_table(k);
    if t == 0 {
        return Some(0);
    }
    let buf = mfs_itab_buf();
    if unsafe { MFS_ITAB_CACHE_IDX } != k {
        if !mfs_read_blk(t, buf) || !mfs_ok(buf, MFS_MAGIC_ITAB) {
            return None;
        }
        unsafe {
            MFS_ITAB_CACHE_IDX = k;
        }
    }
    Some(read_u32(mfs_at(buf, MFS_HDR + j * 4)))
}

/// 把 `MFS_ITAB` 指向的索引块内容载入内存镜像, 并让表块缓存失效。
///
/// 挂载与快照回滚都要用它 —— 回滚会把 `MFS_ITAB` 换成旧索引块, 镜像必须跟着换。
fn mfs_itab_reload() -> bool {
    // 02b: 整张 ino→对象块 映射被替换 (挂载 / 快照回滚) → 目录索引里的 (块, 偏移) 全部作废。
    mfs_didx_invalidate_all();
    let x = mfs_itabx_buf();
    let itab = unsafe { MFS_ITAB };
    if itab == 0 || !mfs_read_blk(itab, x) || !mfs_ok(x, MFS_MAGIC_ITABX) {
        return false;
    }
    for k in 0..MFS_ITAB_SLOTS {
        mfs_set_itab_table(k as u32, read_u32(mfs_at(x, MFS_HDR + k * 4)));
    }
    unsafe {
        MFS_ITAB_CACHE_IDX = u32::MAX;
    }
    true
}

/// 把内存索引镜像 COW 成一个新的索引块 (每次表块变化后调用), 并写超级块。
fn mfs_itab_flush() -> bool {
    let x = mfs_itabx_buf();
    zero_bytes(x, MFS_BLOCK);
    mfs_seal(x, MFS_MAGIC_ITABX); // 先占位, 内容随后填 (下面重新封装)
    for k in 0..MFS_ITAB_SLOTS {
        write_u32(mfs_atm(x, MFS_HDR + k * 4), mfs_itab_table(k as u32));
    }
    let nb = match mfs_commit(x, MFS_MAGIC_ITABX) {
        Some(b) => b,
        None => return false,
    };
    unsafe {
        MFS_ITAB = nb;
    }
    mfs_bmp_flush()
}

/// 在卷 `vol` 上写入一个全新的 MFS 文件系统 (**擦除**该卷现有内容), 成功后把该卷
/// 挂到 `/usb<卷号>` 立即可用。成功返回该卷落盘后的**主卷序号** (>0), 失败 `u64::MAX`。
///
/// 护栏: 只接受「**已是 MFS**」或「**整盘无文件系统**」的卷 —— FAT / exFAT / ext2
/// 等别人的分区一律拒绝, 绝不自动吞掉。真盘上卷号认错时, 这里就是最后一道闸。
///
/// 唯一的例外是 `flags` 里的 [`vfs::MKFS_FLAG_FORCE`]: 调用方 (shell 的 `--force`)
/// 已经拿到用户明确同意, 才放行别人的文件系统。这条路径存在是因为**分区表之外的残留**:
/// `part.wipe` 只清表不动数据, 旧文件系统的 VBR 还在原处, 于是新分区照样被探测成 exfat
/// (实测 thinkplus 盘), 而"用户就是要在这块盘上建 MFS"这个意图只有他自己能表达。
///
/// 第二个例外是**安装盘** (`make INSTALL=1`, 见 `morion::syscall::INSTALL_MODE`): 装机的本质
/// 就是「先 U 盘启动、再把系统装进本机盘」, 要覆盖的正是盘上原有的文件系统 —— 那种镜像里
/// 这条护栏默认放开, 不必逐条 `--force`。日常镜像 `INSTALL_MODE` 为 `false`, 护栏一字不放宽。
///
/// **主卷语义**: 格式化会把该卷标记为主卷 (序号 = 现有最大 + 1), 于是**下一次启动**
/// `/mfs` 就认领到它 —— 这就是「切换主卷」的手段。本次运行的主卷不变: 换主卷是要重启
/// 才生效的事, 在跑的会话里换挂载点会让所有已打开的路径句柄失效。
///
/// 格式化期间内存态被改写成新卷, 故结束后必须把**原卷**的状态重新载回来; 那里只用
/// `mfs_load_state` (只载入), 不会因原卷此刻读不出来而把它格式化掉。
fn mfs_mkfs_volume(vol: u64, flags: u64) -> u64 {
    let desc = match vol_find_desc(mfs_a(), vol) {
        Some(d) => d,
        None => {
            println("mfs: mkfs refused (no such volume)");
            return u64::MAX;
        }
    };
    let forced = flags & vfs::MKFS_FLAG_FORCE != 0;
    if desc.kind != VOL_KIND_UNKNOWN && desc.kind != VOL_KIND_MFS {
        if !forced && !INSTALL_MODE {
            println("mfs: mkfs refused (volume holds another filesystem; --force overwrites it)");
            return u64::MAX;
        }
        print("mfs: mkfs: overwriting an existing filesystem on volume ");
        print_u64(vol);
        println(if forced {
            " (--force; its files are lost)"
        } else {
            " (install image; its files are lost)"
        });
    }

    let prev_vol = unsafe { MFS_CUR_VOL };
    let prev_sectors = unsafe { MFS_CUR_SECTORS };
    let serial = mfs_next_primary_serial(mfs_a(), vol);
    unsafe {
        MFS_CUR_VOL = vol;
        MFS_CUR_SECTORS = desc.sectors; // 格式化尺寸按目标卷的真实容量算 (M7)
        MFS_PRIMARY_SERIAL = serial; // 提交时随超级块写盘, 下次启动据此认领主卷
        MFS_LEAF = MFS_LOC_EMPTY;
    }
    let ok = mfs_format();
    // 切回原卷并重建它的内存态 (位图 / inode 表 / 快照 / 各游标 / 主卷序号): 格式化已经
    // 把内存态写成了新卷, 不重建的话后续对原卷的读写会用错的总块数与位图。
    unsafe {
        MFS_CUR_VOL = prev_vol;
        MFS_CUR_SECTORS = prev_sectors;
    }
    if !mfs_load_state() {
        println("mfs: reload state after mkfs FAILED");
        return u64::MAX;
    }
    if !ok {
        println("mfs: mkfs FAILED");
        return u64::MAX;
    }
    // 立刻可用: 非主卷挂到 `/usb<卷号>` (主卷已挂在 `/mfs`, 不重复挂)。
    if vol != unsafe { MFS_VOL } && vfs::mount_vol(vfs::MFS_DOMAIN, vol) == u64::MAX {
        println("mfs: mkfs OK but mount FAILED (mount table full?)");
    }
    // 回复**从盘上回读**的序号, 而不是刚才设进内存的那个: 前者证明「标记确实写进了
    // 超级块并能再读出来」, 后者只说明「我记得我设过」。回读为 0 说明没落盘。
    let landed = mfs_primary_of_vol(vol);
    if landed == 0 {
        println("mfs: mkfs OK but primary mark missing on disk");
        return u64::MAX;
    }
    landed
}

/// 把**已格式化**的 MFS 卷 `vol` 标记为主卷, **不动它上面的数据**; 成功返回落盘后的
/// 主卷序号 (>0), 失败 `u64::MAX`。
///
/// 与 `mfs_mkfs_volume` 的分工: mkfs 是「建一个新文件系统」(必然擦数据), 本函数是
/// 「在已有数据的卷上换主卷」—— 后者才是日常要用的那个 (前者会把 `/mfs` 的数据清掉)。
///
/// 护栏比 mkfs 更严: **只**接受已经是 MFS 的卷。空白盘 / 别人的分区一律拒绝 —— 这里
/// 没有「格式化兜底」可言, 目标卷上放着用户的文件, 认错卷号绝不能有破坏性后果。
fn mfs_set_primary_volume(vol: u64) -> u64 {
    let desc = match vol_find_desc(mfs_a(), vol) {
        Some(d) => d,
        None => {
            println("mfs: set-primary refused (no such volume)");
            return u64::MAX;
        }
    };
    if mfs_sb_probe(vol).is_none() {
        println("mfs: set-primary refused (not a MorionFS volume)");
        return u64::MAX;
    }
    let prev_vol = unsafe { MFS_CUR_VOL };
    let prev_sectors = unsafe { MFS_CUR_SECTORS };
    let serial = mfs_next_primary_serial(mfs_a(), vol);
    unsafe {
        MFS_CUR_VOL = vol;
        MFS_CUR_SECTORS = desc.sectors;
        MFS_LEAF = MFS_LOC_EMPTY;
    }
    // 载入目标卷的内存态 (只载入, 不格式化 —— 它必须有可用的超级块, 上面已探测过)。
    if !mfs_load_state() {
        println("mfs: set-primary load FAILED");
        unsafe {
            MFS_CUR_VOL = prev_vol;
            MFS_CUR_SECTORS = prev_sectors;
        }
        let _ = mfs_load_state();
        return u64::MAX;
    }
    unsafe {
        MFS_PRIMARY_SERIAL = serial;
    }
    // 提交: 只改了超级块里的序号, 位图没有脏块, 故 flush 实际只写头块 + 超级块两份。
    let ok = mfs_bmp_flush();
    unsafe {
        MFS_CUR_VOL = prev_vol;
        MFS_CUR_SECTORS = prev_sectors;
    }
    if !mfs_load_state() {
        println("mfs: reload state after set-primary FAILED");
        return u64::MAX;
    }
    if !ok {
        println("mfs: set-primary FAILED");
        return u64::MAX;
    }
    // 与 mkfs 同样回复**盘上回读**的序号: 回读为 0 说明标记没落盘。
    let landed = mfs_primary_of_vol(vol);
    if landed == 0 {
        println("mfs: set-primary OK but primary mark missing on disk");
        return u64::MAX;
    }
    landed
}

/// 设置 ino 的槽位 (`blk == 0` 表示释放该 ino)。
///
/// 写路径: 载入表块 → 改槽位 → COW 表块 → 更新索引镜像 → COW 索引块 → 写超级块。
/// 索引块内存镜像里保留的就是刚写出去的表块内容, 故缓存仍有效。
fn mfs_itab_set(ino: u32, blk: u32) -> bool {
    if ino == 0 || ino >= MFS_INO_MAX {
        return false;
    }
    let (k, j) = mfs_ino_slot(ino);
    let t = mfs_itab_table(k);
    let buf = mfs_itab_buf();
    if t == 0 {
        zero_bytes(buf, MFS_BLOCK); // 该表块首次使用
    } else if unsafe { MFS_ITAB_CACHE_IDX } != k
        && (!mfs_read_blk(t, buf) || !mfs_ok(buf, MFS_MAGIC_ITAB))
    {
        return false;
    }
    write_u32(mfs_atm(buf, MFS_HDR + j * 4), blk);
    let new_t = match mfs_commit(buf, MFS_MAGIC_ITAB) {
        Some(b) => b,
        None => return false,
    };
    unsafe {
        MFS_ITAB_CACHE_IDX = k;
    }
    mfs_set_itab_table(k, new_t);
    // 02b: inode 表槽 (对象块号) 变了 → 该 ino 的目录索引失效 (目录内容改动 / chmod /
    // chown / 删除 / ino 复用都会经这里; 文件 ino 调用时只是空扫描, 无开销)。
    mfs_didx_invalidate(ino);
    mfs_itab_flush()
}

/// 为一个**新建**对象分配空闲 ino, 并把它的槽位直接设为 `blk`。返回 ino。
fn mfs_ino_alloc_for(blk: u32) -> Option<u32> {
    let hint = unsafe { MFS_INO_HINT }.clamp(MFS_ROOT_INO + 1, MFS_INO_MAX - 1);
    let mut ino = hint;
    for _ in 0..MFS_INO_MAX {
        if ino >= MFS_INO_MAX {
            ino = MFS_ROOT_INO + 1; // 回绕; ino 1 留给根, 永不复用
        }
        if mfs_ino_block(ino) == Some(0) {
            if !mfs_itab_set(ino, blk) {
                return None;
            }
            unsafe {
                MFS_INO_HINT = if ino + 1 >= MFS_INO_MAX {
                    MFS_ROOT_INO + 1
                } else {
                    ino + 1
                };
                MFS_INO_COUNT += 1;
            }
            return Some(ino);
        }
        ino += 1;
    }
    None
}

/// 释放 ino: 清空它的槽位。对象块随即不可达, 由 GC 回收。
fn mfs_free_ino(ino: u32) -> bool {
    if ino <= MFS_ROOT_INO {
        return false; // 根不可释放
    }
    if !mfs_itab_set(ino, 0) {
        return false;
    }
    unsafe {
        MFS_INO_COUNT = MFS_INO_COUNT.saturating_sub(1);
    }
    true
}

/// COW 提交一个已有对象 (目录 / 文件节点) 并同步它的表槽 —— MFS6 写路径的统一出口。
///
/// 对象块换了位置, 而引用它的目录项存的是 ino (不变), 故**无需**回写父目录:
/// 这正是硬链接的多个名字能自动保持一致的原因, 也让写代价与目录深度无关。
fn mfs_commit_object(ino: u32, buf: *mut u8, magic: u32) -> Option<u32> {
    let nb = mfs_commit(buf, magic)?;
    if !mfs_itab_set(ino, nb) {
        return None;
    }
    Some(nb)
}

// ---------------------------------------------------------------------------
// 超级块 (A/B 双副本)
// ---------------------------------------------------------------------------

fn mfs_build_super(buf: *mut u8) {
    zero_bytes(buf, MFS_BLOCK);
    let p = MFS_HDR;
    write_u32(mfs_atm(buf, p), MFS_VERSION);
    write_u32(mfs_atm(buf, p + MFS_SB_BLOCK_SIZE), MFS_BLOCK as u32);
    write_u32(mfs_atm(buf, p + MFS_SB_TOTAL), unsafe { MFS_TOTAL_BLOCKS });
    write_u32(mfs_atm(buf, p + MFS_SB_INO_COUNT), unsafe { MFS_INO_COUNT });
    write_u32(mfs_atm(buf, p + MFS_SB_ALLOC_HINT), unsafe {
        MFS_ALLOC_NEXT
    });
    write_u32(
        mfs_atm(buf, p + MFS_SB_SNAP_COUNT),
        unsafe { MFS_SNAP_COUNT } as u32,
    );
    write_u64(mfs_atm(buf, p + MFS_SB_GEN), unsafe { MFS_GEN });
    write_u32(mfs_atm(buf, p + MFS_SB_ITAB), unsafe { MFS_ITAB });
    write_u32(mfs_atm(buf, p + MFS_SB_INO_HINT), unsafe { MFS_INO_HINT });
    for i in 0..unsafe { MFS_SNAP_COUNT } {
        let s = mfs_snap(i);
        let off = p + MFS_SB_SNAPS + i * MFS_SNAP_REC;
        write_u64(mfs_atm(buf, off), s.gen);
        write_u32(mfs_atm(buf, off + 8), s.itab);
        write_u32(mfs_atm(buf, off + 12), s.ino_hint);
        write_u32(mfs_atm(buf, off + 16), s.alloc_next);
    }
    // 主卷序号: 认领 `/mfs` 的依据 (见 `mfs_vol_claim`)。必须每次都写 —— 提交走的是
    // 「重写整块超级块」而不是原地改字段, 漏写就会把标记清掉。
    write_u64(mfs_atm(buf, p + MFS_SB_PRIMARY), unsafe {
        MFS_PRIMARY_SERIAL
    });
    // MFS7: 空闲位图不再内联在超级块里, 位图改由 `mfs_bmp_flush` 写入独立的位图
    // 数据块 + 头块。原内联区 (+256 起) 只留 `MFS_SB_PRIMARY` 一项, 其余为保留。
    mfs_seal(buf, MFS_MAGIC_SUPER);
}

/// 构建位图头块 (magic "MFBH"): gen / 总块数 / 位图数据块数 bb + 各数据块 CRC32。
fn mfs_build_bmp_header(buf: *mut u8) {
    zero_bytes(buf, MFS_BLOCK);
    let p = MFS_HDR;
    unsafe {
        write_u64(mfs_atm(buf, p + MFS_BMPH_GEN), MFS_GEN);
        write_u32(mfs_atm(buf, p + MFS_BMPH_TOTAL), MFS_TOTAL_BLOCKS);
        write_u32(mfs_atm(buf, p + MFS_BMPH_DATA_BLOCKS), MFS_BMP_DATA_BLOCKS);
        for i in 0..MFS_BMP_DATA_BLOCKS as usize {
            write_u32(
                mfs_atm(buf, p + MFS_BMPH_CRC + i * 4),
                *core::ptr::addr_of!(MFS_BMP_CRC).cast::<u32>().add(i),
            );
        }
    }
    mfs_seal(buf, MFS_MAGIC_BMPHDR);
}

/// 把常驻位图窗口按副本落盘 (只写脏区间), 再写头块 + 超级块 —— **唯一**提交出口。
///
/// 取代 MFS6 的 `mfs_write_super`。流程 (代际 +1 后):
///   1. 清空脏位图, 重新计算本次要写的区间的 CRC;
///   2. 对 copy ∈ {0,1} 依次: 写脏的位图数据块 → 写该副本头块 (gen/total/bb + 全量 CRC) →
///      写该副本超级块 (带新 gen);
///   3. 两份都写完后清 dirty。
///
/// 崩溃在任一步骤时, 另一份仍是**旧代但自洽**的 (gen 不同 → 加载取 gen 高者), 因此
/// 不需要额外的副本选择状态 (`MFS_SB_COPY` 已删)。
fn mfs_bmp_flush() -> bool {
    unsafe {
        MFS_GEN += 1;
    }
    let bb = unsafe { MFS_BMP_DATA_BLOCKS };
    let (a_start, b_start) = (4u32, 4 + bb);
    let head = mfs_bmph_buf();
    // 重新计算全部位图数据块的 CRC (脏区间之外的块内容未变, 但 CRC 数组整体落盘,
    // 故这里统一按窗口内容算, 保证数组与数据块始终一致)。
    for c in 0..bb as usize {
        let p = (MFS_BMP_VADDR as *const u8).wrapping_add(c * MFS_BLOCK);
        let crc = unsafe { mfs_crc32(core::slice::from_raw_parts(p, MFS_BLOCK)) };
        unsafe {
            *core::ptr::addr_of_mut!(MFS_BMP_CRC).cast::<u32>().add(c) = crc;
        }
    }
    for copy in 0..MFS_SB_COPIES {
        let data_start = if copy == 0 { a_start } else { b_start };
        // 位图数据块: 直接以窗口页作 I/O 缓冲 (免拷贝), 只写脏区间。
        for c in 0..bb as usize {
            let dirty = unsafe {
                *core::ptr::addr_of!(MFS_BMP_DIRTY).cast::<u8>().add(c >> 3) & (1u8 << (c & 7)) != 0
            };
            if !dirty {
                continue;
            }
            let p = (MFS_BMP_VADDR as *const u8).wrapping_add(c * MFS_BLOCK);
            if !mfs_write_blk(data_start + c as u32, p) {
                return false;
            }
        }
        // 头块 (gen/total/bb + 全量 CRC 数组)。
        mfs_build_bmp_header(head);
        if !mfs_write_blk(2 + copy, head) {
            return false;
        }
        // 超级块 (带新 gen)。
        let sb = mfs_s();
        mfs_build_super(sb);
        if !mfs_write_blk(copy, sb) {
            return false;
        }
    }
    // 真正的落盘出口: 上面所有 `mfs_write_blk` 只是攒进写暂存窗, 这里一次批写出去。
    if !mfs_commit_flush() {
        return false;
    }
    for i in 0..MFS_BMP_DIRTY_BYTES {
        unsafe {
            *core::ptr::addr_of_mut!(MFS_BMP_DIRTY).cast::<u8>().add(i) = 0;
        }
    }
    true
}

/// 从盘上载入 MFS 内存态: 取两份超级块中 CRC 有效、版本匹配且代际更高者, 并据此
/// 重建位图 / inode 表镜像 / 快照表 / 各游标。两份都不可用返回 false。
///
/// **不**在这里格式化 —— 格式化是调用方的决定 (首次挂载可格式化, 但切卷时不行:
/// 那会把一块读不出来的盘直接抹掉, 里面可能是用户唯一的副本)。
fn mfs_load_state() -> bool {
    if !mfs_commit_flush() {
        return false;
    }
    let mut found = false;
    let mut best_gen = 0u64;
    let mut bitmap_ok = false;
    for copy in 0..MFS_SB_COPIES {
        let buf = mfs_a();
        if !mfs_read_blk(copy, buf) || !mfs_ok(buf, MFS_MAGIC_SUPER) {
            continue;
        }
        let p = MFS_HDR;
        if read_u32(mfs_at(buf, p)) != MFS_VERSION {
            continue;
        }
        let gen = read_u64(mfs_at(buf, p + MFS_SB_GEN));
        // 取代际更高者; 同代时**优先已成功载入位图的那份**。MFS7 起一次提交把两份
        // 副本写成同一 gen, 若只按 `gen <= best_gen` 跳过, 先试的副本位图坏了就会
        // 白白触发重建 —— 即使另一份完好。同代时两份超级块内容一致, 重复采用同一份
        // 内存态无副作用。
        if found && (gen < best_gen || (gen == best_gen && bitmap_ok)) {
            continue;
        }
        let total = read_u32(mfs_at(buf, p + MFS_SB_TOTAL));
        let itab = read_u32(mfs_at(buf, p + MFS_SB_ITAB));
        if total == 0 || itab == 0 || total > MFS_MAX_BLOCKS {
            continue;
        }
        // 盘上记的总块数不能超过该卷的实际容量: 换过镜像 / 卷号认领错 / 卷被缩小过时,
        // 超出的块一律读不到 —— 与其让后续读写大面积失败, 不如在这里判该副本不可用
        // (两份都不可用就会走格式化, 那才是正确处置)。容量未知 (0) 时跳过这项检查。
        let vsectors = unsafe { MFS_CUR_SECTORS } as u64;
        if vsectors != 0 && total as u64 * MFS_SECTORS_PER_BLOCK as u64 > vsectors {
            continue;
        }
        // 索引块是 inode 表的根, 先验证它再采纳这份副本 (坏了就试另一份)。
        let x = mfs_itabx_buf();
        if !mfs_read_blk(itab, x) || !mfs_ok(x, MFS_MAGIC_ITABX) {
            continue;
        }
        let alloc = read_u32(mfs_at(buf, p + MFS_SB_ALLOC_HINT));
        let scount = (read_u32(mfs_at(buf, p + MFS_SB_SNAP_COUNT)) as usize).min(MFS_MAX_SNAP);
        let bb = mfs_bb_for(total);
        unsafe {
            MFS_TOTAL_BLOCKS = total;
            MFS_BMP_DATA_BLOCKS = bb;
            MFS_ALLOC_NEXT = alloc;
            MFS_GEN = gen;
            MFS_SNAP_COUNT = scount;
            MFS_ITAB = itab;
            MFS_INO_COUNT = read_u32(mfs_at(buf, p + MFS_SB_INO_COUNT));
            MFS_INO_HINT = read_u32(mfs_at(buf, p + MFS_SB_INO_HINT)).max(MFS_ROOT_INO + 1);
            // 主卷序号随内存态一起载入: 后续任何一次提交都会把它原样写回, 标记不会丢。
            MFS_PRIMARY_SERIAL = read_u64(mfs_at(buf, p + MFS_SB_PRIMARY));
            MFS_ITAB_CACHE_IDX = u32::MAX;
        }
        // 位图窗口必须容得下本卷的 bb 页 (首次挂载时 `mfs_main` 已按卷容量预算过)。
        if !mfs_win_ensure(bb) {
            continue;
        }
        // 索引块内容载入内存镜像。
        if !mfs_itab_reload() {
            continue;
        }
        for i in 0..scount {
            let off = p + MFS_SB_SNAPS + i * MFS_SNAP_REC;
            mfs_set_snap(
                i,
                MfsSnap {
                    gen: read_u64(mfs_at(buf, off)),
                    itab: read_u32(mfs_at(buf, off + 8)),
                    ino_hint: read_u32(mfs_at(buf, off + 12)),
                    alloc_next: read_u32(mfs_at(buf, off + 16)),
                },
            );
        }
        // 位图数据按副本独立存放: 校验该副本头块与逐块 CRC, 通过则读进窗口。失败时
        // 只是本轮 `bitmap_ok = false` —— 所有候选都失败才走下面的重建兜底。
        bitmap_ok = mfs_load_bitmap_copy(copy);
        best_gen = gen;
        found = true;
    }
    if !found {
        return false;
    }
    if bitmap_ok {
        // 位图是分配的唯一依据: 重算空闲块数, 并强制保留元数据区 (超级块 / 位图头 /
        // 位图数据) 与**所有可达根的 inode 表元数据** (当前表 + 每张快照的表) ——
        // 万一位图缺了这几位 (例如上次回收失败留下的陈旧位图), 立刻纠正, 绝不会把
        // 元数据块分配出去。
        mfs_bmp_recount();
        let meta_end = mfs_data_start();
        for b in 0..meta_end {
            mfs_bmp_set(b);
        }
        mfs_mark_itab_meta(unsafe { MFS_ITAB });
        for i in 0..unsafe { MFS_SNAP_COUNT } {
            mfs_mark_itab_meta(mfs_snap(i).itab);
        }
    } else if !mfs_rebuild_bitmap() {
        return false;
    }
    true
}

/// 载入某个副本的位图: 校验头块 (magic / gen / total / bb) 与逐块 CRC32。
///
/// 校验通过时把数据块读进 `MFS_BMP_VADDR` 窗口 (直接以窗口页作 I/O 缓冲, 免拷贝),
/// 并把各块 CRC 填进常驻 `MFS_BMP_CRC`; 任一不符立即返回 false。
/// 副本 `copy` 的位图数据位于 `4 + copy*bb` 起的连续 `bb` 个块, 头块在块 `2+copy`。
fn mfs_load_bitmap_copy(copy: u32) -> bool {
    let bb = unsafe { MFS_BMP_DATA_BLOCKS };
    let total = unsafe { MFS_TOTAL_BLOCKS };
    let head = mfs_bmph_buf();
    if !mfs_read_blk(2 + copy, head) || !mfs_ok(head, MFS_MAGIC_BMPHDR) {
        return false;
    }
    let hp = MFS_HDR;
    if read_u64(mfs_at(head, hp + MFS_BMPH_GEN)) != unsafe { MFS_GEN } {
        return false;
    }
    if read_u32(mfs_at(head, hp + MFS_BMPH_TOTAL)) != total {
        return false;
    }
    if read_u32(mfs_at(head, hp + MFS_BMPH_DATA_BLOCKS)) != bb {
        return false;
    }
    let data_start = 4 + copy * bb;
    for c in 0..bb as usize {
        let dp = (MFS_BMP_VADDR as *mut u8).wrapping_add(c * MFS_BLOCK);
        if !mfs_read_blk(data_start + c as u32, dp) {
            return false;
        }
        let crc = unsafe { mfs_crc32(core::slice::from_raw_parts(dp as *const u8, MFS_BLOCK)) };
        if crc != read_u32(mfs_at(head, hp + MFS_BMPH_CRC + c * 4)) {
            return false;
        }
        unsafe {
            *core::ptr::addr_of_mut!(MFS_BMP_CRC).cast::<u32>().add(c) = crc;
        }
    }
    true
}

/// 位图两份副本都不可用时的兜底: 先把位图整体置「占用」(FREE = 0 的安全态), 再跑
/// `mfs_gc()` 从可达根重建 —— **不格式化**。
///
/// 全置占用是安全方向: 即使 GC 中途失败, 最坏结果是「空间没被回收」, 绝不会把仍在
/// 使用的块当成空闲发出去。盘上目录树完好、只是位图坏了的情形正是靠这条路径救回。
fn mfs_rebuild_bitmap() -> bool {
    let bb = unsafe { MFS_BMP_DATA_BLOCKS } as usize;
    for i in 0..bb * MFS_BLOCK {
        unsafe {
            *mfs_bmp_byte(i) = 0xFF;
        }
    }
    // 位图全为占用 -> 空闲块数为 0, 交给 GC 重建后重算。
    unsafe {
        MFS_FREE_BLOCKS = 0;
    }
    mfs_gc() != u64::MAX
}

/// 确保三个位图窗口至少各 `pages` 页 (**只增不缩**)。新增的 BMP 窗口页要共享给
/// block_srv (位图数据块直接以窗口页作 DMA 缓冲)。失败返回 false。
fn mfs_win_ensure(pages: u32) -> bool {
    while unsafe { MFS_BMP_PAGES } < pages {
        let n = unsafe { MFS_BMP_PAGES } as u64;
        let va = MFS_BMP_VADDR + n * MFS_BLOCK as u64;
        if sys_alloc_page(va) != 1 || sys_share_page(va, BLOCK_DOMAIN) != 1 {
            return false;
        }
        unsafe {
            MFS_BMP_PAGES += 1;
        }
    }
    while unsafe { MFS_MARK_PAGES } < pages {
        let n = unsafe { MFS_MARK_PAGES } as u64;
        let va = MFS_MARK_VADDR + n * MFS_BLOCK as u64;
        if sys_alloc_page(va) != 1 {
            return false;
        }
        unsafe {
            MFS_MARK_PAGES += 1;
        }
    }
    while unsafe { MFS_SEEN_PAGES } < pages {
        let n = unsafe { MFS_SEEN_PAGES } as u64;
        let va = MFS_SEEN_VADDR + n * MFS_BLOCK as u64;
        if sys_alloc_page(va) != 1 {
            return false;
        }
        unsafe {
            MFS_SEEN_PAGES += 1;
        }
    }
    true
}

/// 切换当前卷到 `vol` (必须是**已格式化**的 MFS 卷): 更新卷号 / 容量后重新载入
/// 内存态。成功返回 true。
///
/// 失败时内存态已不可信 —— 调用方必须放弃本次请求 (见服务循环), 不能继续用旧卷的
/// 位图去写新卷。
fn mfs_switch_vol(vol: u64) -> bool {
    // 换卷前把**当前卷**的暂存写落盘 (flush 用的是 `MFS_CUR_VOL`, 必须在切换之前)。
    if !mfs_commit_flush() {
        return false;
    }
    let sectors = vol_sectors(mfs_a(), vol);
    unsafe {
        MFS_CUR_VOL = vol;
        MFS_CUR_SECTORS = sectors;
        // 上一卷的叶子位置 (块号) 在新卷上没有意义, 清掉以免被误用。
        MFS_LEAF = MFS_LOC_EMPTY;
    }
    mfs_load_state()
}

/// 读卷 `vol` 的两份超级块, 按**卷首 magic** 做三态判定 (01 保护数据)。
///
/// 返回 `(state, magic)`: `state` ∈ {`MFS_MAGIC_NONE`, `MFS_MAGIC_MATCH`,
/// `MFS_MAGIC_FAMILY_MISMATCH`}; `magic` 是读到的第一个 MFS 系 magic 原值 (供日志,
/// 无则 0)。**只读**, 不写盘。
///
/// 判定顺序: 任一份是**本构建** magic (`MFS8`) 即判 MATCH (交给 `mfs_load_state` 选
/// 有效副本); 两份都不是 `MFS8` 但至少一份属 MFS 系 (`MFS0`..`MFS9`) 即判 MISMATCH;
/// 都不是则 NONE (空白 / 非 MFS)。
fn mfs_sb_magic_state(vol: u64) -> (u8, u32) {
    if !mfs_commit_flush() {
        return (MFS_MAGIC_FAMILY_MISMATCH, 0);
    }
    let buf = mfs_a();
    let mut mismatch = 0u32;
    for copy in 0..MFS_SB_COPIES {
        let lba = copy * MFS_SECTORS_PER_BLOCK as u32;
        if !block_read_dev(vol, lba, MFS_SECTORS_PER_BLOCK, buf) {
            continue;
        }
        let m = read_u32(buf);
        if m == MFS_MAGIC_SUPER {
            return (MFS_MAGIC_MATCH, m);
        }
        if m & 0xFFFF_FF00 == MFS_MAGIC_FAMILY_PREFIX
            && (0x30..=0x39).contains(&(m & 0xFF))
            && mismatch == 0
        {
            mismatch = m;
        }
    }
    if mismatch != 0 {
        (MFS_MAGIC_FAMILY_MISMATCH, mismatch)
    } else {
        (MFS_MAGIC_NONE, 0)
    }
}

/// 把 4 字节 ASCII magic 按字符打印, 后跟十进制原值 (日志里可读、可 grep)。
fn mfs_print_magic(m: u32) {
    let bytes = [(m >> 24) as u8, (m >> 16) as u8, (m >> 8) as u8, m as u8];
    for &c in bytes.iter() {
        // magic 约定为 ASCII; 非可打印字节用 '.' 兜底, 免得污染串口日志。
        let c = if (0x20..0x7F).contains(&c) { c } else { b'.' };
        let s = unsafe { core::str::from_utf8_unchecked(core::slice::from_raw_parts(&c, 1)) };
        print(s);
    }
    print(" (");
    print_u64(m as u64);
    print(")");
}

/// 挂载: 能载入就载入; 只有**空白卷**才自动格式化 (首次使用)。
///
/// 01 起不再「载入失败即重格」: 卷首若是 MFS 系 magic (更旧 / 更新的修订, 或本构建
/// `MFS8` 但已损坏), 一律**拒绝挂载且一个字节都不写盘** —— 旧盘要变成新格式必须显式
/// `mkfs.mfs`。返回 false 表示拒绝 (调用方放弃本次挂载)。
fn mfs_mount_or_format() -> bool {
    if mfs_load_state() {
        return true;
    }
    let (state, magic) = mfs_sb_magic_state(unsafe { MFS_VOL });
    if state == MFS_MAGIC_FAMILY_MISMATCH {
        print("mfs: refuse to mount: on-disk magic ");
        mfs_print_magic(magic);
        print(" != expected ");
        mfs_print_magic(MFS_MAGIC_SUPER);
        println("; run 'mkfs.mfs <vol>' to rebuild (data left untouched)");
        return false;
    }
    if state == MFS_MAGIC_MATCH {
        // magic 是本构建的, 但超级块 / 位图 / inode 表载入失败 (损坏): 同样拒绝重格。
        println("mfs: refuse to mount: MFS8 superblock present but unreadable (not reformatting)");
        return false;
    }
    // 空白卷 (卷首无任何 MFS 系 magic): 照旧自动格式化。
    // 自动格式化**不**认领主卷: 主卷标记只由显式 `mkfs.mfs` 设置 (见 `mfs_mkfs_volume`)。
    // 这里清掉可能残留在内存态里的上一卷序号, 免得把别的卷的标记写进这块新盘。
    unsafe {
        MFS_PRIMARY_SERIAL = 0;
    }
    mfs_format()
}

/// 首次格式化该用多少块: 按**卷的真实容量**算 (每块 4 KiB = 8 扇区), 夹在
/// [`MFS_MIN_TOTAL_BLOCKS`, `MFS_MAX_BLOCKS`] 之间; 容量未知时退回默认值。
///
/// 上界是硬约束: 位图头块的 CRC 数组只能放 `MFS_MAX_BMP_DATA_BLOCKS` 项, 每项覆盖
/// 32768 块 —— 即 `MFS_MAX_BLOCKS` (≈127.25 GiB)。更大的卷需要再加一层位图间接。
fn mfs_format_total_blocks() -> u32 {
    let sectors = unsafe { MFS_CUR_SECTORS } as u64;
    if sectors == 0 {
        return MFS_DEFAULT_TOTAL_BLOCKS;
    }
    let blocks = sectors / MFS_SECTORS_PER_BLOCK as u64;
    blocks.clamp(MFS_MIN_TOTAL_BLOCKS as u64, MFS_MAX_BLOCKS as u64) as u32
}

/// 首次格式化 (含旧格式升级): 清空位图 -> 占用元数据区 -> 建空根目录 (ino 1) ->
/// 建 inode 表 -> 提交 (写位图数据块 + 位图头块 + 超级块)。
fn mfs_format() -> bool {
    // 02b: 重建文件系统 → 所有目录索引作废。
    mfs_didx_invalidate_all();
    let total = mfs_format_total_blocks();
    unsafe {
        MFS_TOTAL_BLOCKS = total;
        MFS_BMP_DATA_BLOCKS = mfs_bb_for(total);
    }
    let bb = unsafe { MFS_BMP_DATA_BLOCKS } as usize;
    // 位图窗口按本卷的 bb 页铺开 (mfs_main 已按容量预算; 这里兜底「mkfs 到更大卷」)。
    if !mfs_win_ensure(bb as u32) {
        return false;
    }
    unsafe {
        MFS_ALLOC_NEXT = mfs_data_start();
        MFS_GEN = 0;
        MFS_SNAP_COUNT = 0;
        MFS_INO_COUNT = 0;
        MFS_INO_HINT = MFS_ROOT_INO + 1;
        MFS_ITAB = 0;
        MFS_ITAB_CACHE_IDX = u32::MAX;
        // 整个位图窗口清零 (含末尾余量), 让 CRC 可复现。
        for i in 0..bb * MFS_BLOCK {
            *mfs_bmp_byte(i) = 0;
        }
        MFS_FREE_BLOCKS = total;
    }
    // 位图内容整体重置: 所有数据块都必须落盘 (不能只靠脏位增量)。
    for c in 0..bb {
        unsafe {
            *core::ptr::addr_of_mut!(MFS_BMP_DIRTY)
                .cast::<u8>()
                .add(c >> 3) |= 1u8 << (c & 7);
        }
    }
    for k in 0..MFS_ITAB_SLOTS {
        mfs_set_itab_table(k as u32, 0); // 空索引镜像
    }
    // 元数据区 (超级块 0/1 + 位图头 2/3 + 两副本位图数据) 一律常驻占用。
    let meta_end = mfs_data_start();
    for b in 0..meta_end {
        mfs_bmp_set(b);
    }
    // 根目录节点块: 无扩展索引块 + 覆盖全区的单个大空槽 + 系统属主 (格式化时无调用者)。
    let buf = mfs_a();
    zero_bytes(buf, MFS_BLOCK);
    mfs_dir_init_empty(buf);
    mfs_init_meta(
        buf,
        true,
        MFS_FTYPE_DIR,
        0,
        Cred {
            uid: MFS_UID_ROOT,
            gid: MFS_GID_ROOT,
        },
        MFS_MODE_DIR,
    );
    let root = match mfs_commit(buf, MFS_MAGIC_DIR) {
        Some(b) => b,
        None => return false,
    };
    // 先落一个空索引块, 再登记 ino 1 (登记本身会分配表块并重新 COW 索引块)。
    if !mfs_itab_flush() {
        return false;
    }
    if !mfs_itab_set(MFS_ROOT_INO, root) {
        return false;
    }
    unsafe {
        MFS_INO_COUNT = 1;
        MFS_INO_HINT = MFS_ROOT_INO + 1;
    }
    mfs_bmp_flush()
}

// ---------------------------------------------------------------------------
// 节点元数据 (mode / owner / nlink / 时间戳) 与时间源
// ---------------------------------------------------------------------------
//
// 元数据布局见 `MFS_META_*` 常量。文件节点放在 inode 尾部保留区, 目录节点紧跟
// `ext` 指针之后 —— 位置不同但结构相同, 故访问函数按 `is_dir` 选偏移。

/// 节点元数据在块内的偏移 (`is_dir` 决定文件 / 目录两种布局)。
fn mfs_meta_off(is_dir: bool) -> usize {
    if is_dir {
        MFS_HDR + 8
    } else {
        MFS_FILE_RESERVED_OFF
    }
}

fn mfs_get_mode(buf: *const u8, is_dir: bool) -> u16 {
    read_u16(mfs_at(buf, mfs_meta_off(is_dir) + MFS_META_MODE))
}
/// 设置权限位 (低 12 位), **保留**高 4 位的节点类型 —— 类型随节点固定, `chmod` 不该
/// 把文件改成目录 (或被软链接的形式掩盖)。
fn mfs_set_mode(buf: *mut u8, is_dir: bool, v: u16) {
    let off = mfs_meta_off(is_dir) + MFS_META_MODE;
    let ty = read_u16(mfs_at(buf, off)) & MFS_FTYPE_MASK;
    write_u16(mfs_atm(buf, off), ty | (v & MFS_MODE_MASK));
}
fn mfs_get_owner(buf: *const u8, is_dir: bool) -> u16 {
    read_u16(mfs_at(buf, mfs_meta_off(is_dir) + MFS_META_OWNER))
}
fn mfs_set_owner(buf: *mut u8, is_dir: bool, v: u16) {
    write_u16(mfs_atm(buf, mfs_meta_off(is_dir) + MFS_META_OWNER), v);
}
/// 04b: 节点属主 uid (元数据 `+32`)。
fn mfs_get_uid(buf: *const u8, is_dir: bool) -> u16 {
    read_u16(mfs_at(buf, mfs_meta_off(is_dir) + MFS_META_UID))
}
fn mfs_set_uid(buf: *mut u8, is_dir: bool, v: u16) {
    write_u16(mfs_atm(buf, mfs_meta_off(is_dir) + MFS_META_UID), v);
}
/// 04b: 节点属组 gid (元数据 `+34`)。
fn mfs_get_gid(buf: *const u8, is_dir: bool) -> u16 {
    read_u16(mfs_at(buf, mfs_meta_off(is_dir) + MFS_META_GID))
}
fn mfs_set_gid(buf: *mut u8, is_dir: bool, v: u16) {
    write_u16(mfs_atm(buf, mfs_meta_off(is_dir) + MFS_META_GID), v);
}
fn mfs_get_nlink(buf: *const u8, is_dir: bool) -> u32 {
    read_u32(mfs_at(buf, mfs_meta_off(is_dir) + MFS_META_NLINK))
}
fn mfs_set_nlink(buf: *mut u8, is_dir: bool, v: u32) {
    write_u32(mfs_atm(buf, mfs_meta_off(is_dir) + MFS_META_NLINK), v);
}
fn mfs_get_mtime(buf: *const u8, is_dir: bool) -> u64 {
    read_u64(mfs_at(buf, mfs_meta_off(is_dir) + MFS_META_MTIME))
}
fn mfs_set_mtime(buf: *mut u8, is_dir: bool, v: u64) {
    write_u64(mfs_atm(buf, mfs_meta_off(is_dir) + MFS_META_MTIME), v);
}
fn mfs_get_ctime(buf: *const u8, is_dir: bool) -> u64 {
    read_u64(mfs_at(buf, mfs_meta_off(is_dir) + MFS_META_CTIME))
}
fn mfs_set_ctime(buf: *mut u8, is_dir: bool, v: u64) {
    write_u64(mfs_atm(buf, mfs_meta_off(is_dir) + MFS_META_CTIME), v);
}
fn mfs_get_atime(buf: *const u8, is_dir: bool) -> u64 {
    read_u64(mfs_at(buf, mfs_meta_off(is_dir) + MFS_META_ATIME))
}
fn mfs_set_atime(buf: *mut u8, is_dir: bool, v: u64) {
    write_u64(mfs_atm(buf, mfs_meta_off(is_dir) + MFS_META_ATIME), v);
}

/// 初始化一个新建节点的元数据 (nlink = 1, 三个时间同刻)。
///
/// `ftype` 是 `mode` 高 4 位的节点类型 (文件 / 目录 / 软链接); 节点刚清零过, 类型位
/// 必须在这里写入 —— `mfs_set_mode` 是"保留类型"的, 零值下它只能写权限。
fn mfs_init_meta(buf: *mut u8, is_dir: bool, ftype: u16, owner: u16, cred: Cred, mode: u16) {
    let now = mfs_now();
    let off = mfs_meta_off(is_dir) + MFS_META_MODE;
    write_u16(
        mfs_atm(buf, off),
        (ftype & MFS_FTYPE_MASK) | (mode & MFS_MODE_MASK),
    );
    mfs_set_owner(buf, is_dir, owner);
    // 04b: 新节点的属主 / 属组 = 发起者身份 (`owner` 仍是创建者域号, 仅供诊断)。
    mfs_set_uid(buf, is_dir, cred.uid);
    mfs_set_gid(buf, is_dir, cred.gid);
    mfs_set_nlink(buf, is_dir, 1);
    mfs_set_mtime(buf, is_dir, now);
    mfs_set_ctime(buf, is_dir, now);
    mfs_set_atime(buf, is_dir, now);
}

/// 内容或元数据变更后刷新 mtime / ctime。
fn mfs_touch(buf: *mut u8, is_dir: bool) {
    let now = mfs_now();
    mfs_set_mtime(buf, is_dir, now);
    mfs_set_ctime(buf, is_dir, now);
}
/// 仅刷新 ctime (权限等元数据变更)。
fn mfs_touch_ctime(buf: *mut u8, is_dir: bool) {
    mfs_set_ctime(buf, is_dir, mfs_now());
}

// --- 时间源: CMOS RTC ---
//
// 内核没有时间系统调用, 但 `SYS_PORT_IN8/OUT8` 已开放, 故用户态直接读 CMOS RTC
// (端口 0x70 选寄存器 / 0x71 读写)。读失败或时间明显不合理时返回 0 —— 元数据里
// 0 表示"时间未知", 不阻塞任何操作。

const CMOS_IDX: u16 = 0x70;
const CMOS_DAT: u16 = 0x71;
/// RTC 寄存器号。
const CMOS_SEC: u8 = 0x00;
const CMOS_MIN: u8 = 0x02;
const CMOS_HOUR: u8 = 0x04;
const CMOS_DAY: u8 = 0x07;
const CMOS_MON: u8 = 0x08;
const CMOS_YEAR: u8 = 0x09;
/// 状态寄存器 A (bit7 = update in progress) / B (bit2 = 二进制, bit1 = 24 小时制)。
const CMOS_STAT_A: u8 = 0x0A;
const CMOS_STAT_B: u8 = 0x0B;

fn cmos_read(reg: u8) -> u8 {
    sys_port_out8(CMOS_IDX, reg);
    sys_port_in8(CMOS_DAT)
}

/// BCD → 二进制 (状态寄存器 B 的 bit2 为 0 时 RTC 用 BCD 编码)。
fn cmos_bcd(v: u8) -> u8 {
    (v & 0x0F) + ((v >> 4) * 10)
}

/// 读 CMOS RTC 得到当前 Unix 秒 (UTC); 读取失败或字段不合理返回 0。
///
/// 连读两次并要求一致: RTC 更新周期内读到的字段可能跨秒, 两次相同才认为稳定。
fn mfs_now() -> u64 {
    let mut attempt = 0;
    while attempt < 4 {
        attempt += 1;
        let a = cmos_rtc_snapshot();
        let b = cmos_rtc_snapshot();
        let (sa, sb) = match (a, b) {
            (Some(x), Some(y)) => (x, y),
            _ => return 0,
        };
        if sa == sb {
            return sa;
        }
    }
    0
}

/// 单次读取 RTC 并转成 Unix 秒; 等 update-in-progress 清零后读, 字段不合法返回 None。
fn cmos_rtc_snapshot() -> Option<u64> {
    let mut guard = 0u32;
    while cmos_read(CMOS_STAT_A) & 0x80 != 0 {
        guard += 1;
        if guard > 1_000_000 {
            return None;
        }
    }
    let stat_b = cmos_read(CMOS_STAT_B);
    let binary = stat_b & 0x04 != 0;
    let h24 = stat_b & 0x02 != 0;
    let mut sec = cmos_read(CMOS_SEC);
    let mut min = cmos_read(CMOS_MIN);
    let mut hour = cmos_read(CMOS_HOUR);
    let mut day = cmos_read(CMOS_DAY);
    let mut mon = cmos_read(CMOS_MON);
    let mut year = cmos_read(CMOS_YEAR);
    if !binary {
        // 12 小时制时 bit7 是 PM 标志, 必须在 BCD 转换前摘掉。
        let pm = !h24 && (hour & 0x80) != 0;
        hour &= 0x7F;
        sec = cmos_bcd(sec);
        min = cmos_bcd(min);
        hour = cmos_bcd(hour);
        day = cmos_bcd(day);
        mon = cmos_bcd(mon);
        year = cmos_bcd(year);
        if pm && hour < 12 {
            hour += 12;
        }
    } else if !h24 && (hour & 0x80) != 0 {
        hour = ((hour & 0x7F) + 12) % 24;
    }
    // 两位数年份: 按 70..99 → 19xx, 00..69 → 20xx 归一。
    let full_year = if year >= 70 {
        1900 + year as i64
    } else {
        2000 + year as i64
    };
    if !(1..=12).contains(&mon) || !(1..=31).contains(&day) || hour > 23 || min > 59 || sec > 60 {
        return None;
    }
    let days = days_from_civil(full_year, mon as i64, day as i64);
    let secs = days * 86400 + hour as i64 * 3600 + min as i64 * 60 + sec as i64;
    if secs < 0 {
        return None;
    }
    Some(secs as u64)
}

// ---------------------------------------------------------------------------
// 目录 / 文件节点访问
// ---------------------------------------------------------------------------

/// 目录块的扩展索引块号 (0 = 无扩展)。
fn mfs_dir_ext(buf: *const u8) -> u32 {
    read_u32(mfs_at(buf, MFS_HDR))
}
fn mfs_dir_set_ext(buf: *mut u8, b: u32) {
    write_u32(mfs_atm(buf, MFS_HDR), b);
}
/// 把目录块初始化成「空目录」(无扩展 + 一个覆盖全区的大空槽)。
fn mfs_dir_init_empty(buf: *mut u8) {
    mfs_dir_set_ext(buf, 0);
    write_u32(mfs_atm(buf, MFS_HDR + 4), 0); // pad
    mfs_ent_clear(buf, MFS_HDR + MFS_DIR_HDR, MFS_DIR_AREA);
}

// --- 变长条目 (`off` = 条目在块缓冲内、相对块首的绝对偏移) ---
//
// 名字不再以 NUL 结尾: 长度由 `name_len` 给出, 条目按 `rec_len` 串联。

fn mfs_ent_rec_len(buf: *const u8, off: usize) -> usize {
    read_u16(mfs_at(buf, off + 6)) as usize
}
fn mfs_ent_name_len(buf: *const u8, off: usize) -> usize {
    unsafe { *mfs_at(buf, off + 5) as usize }
}
/// 条目指向的 **inode 号** (MFS6 起该字段不再是块号, 需经 inode 表翻译才能得到块)。
fn mfs_ent_ino(buf: *const u8, off: usize) -> u32 {
    read_u32(mfs_at(buf, off))
}
fn mfs_ent_type(buf: *const u8, off: usize) -> u32 {
    unsafe { *mfs_at(buf, off + 4) as u32 }
}
/// 条目名字是否等于 `comp`。
fn mfs_ent_name_eq(buf: *const u8, off: usize, comp: &[u8]) -> bool {
    let n = mfs_ent_name_len(buf, off);
    if n == 0 || n != comp.len() {
        return false;
    }
    for (i, &c) in comp.iter().enumerate() {
        if unsafe { *mfs_at(buf, off + MFS_DIR_ENT_HDR + i) } != c {
            return false;
        }
    }
    true
}
/// 名字长度 → 条目所需字节数 (4 字节对齐)。
fn mfs_ent_need(name_len: usize) -> usize {
    (MFS_DIR_ENT_HDR + name_len + 3) & !3
}
/// 写入一个条目 (`rec_len` 必须 >= `mfs_ent_need(comp.len())`)。
fn mfs_ent_fill(buf: *mut u8, off: usize, comp: &[u8], child: u32, typ: u32, rec_len: usize) {
    write_u32(mfs_atm(buf, off), child);
    unsafe {
        *mfs_atm(buf, off + 4) = typ as u8;
        *mfs_atm(buf, off + 5) = comp.len() as u8;
    }
    write_u16(mfs_atm(buf, off + 6), rec_len as u16);
    for (i, &c) in comp.iter().enumerate() {
        unsafe {
            *mfs_atm(buf, off + MFS_DIR_ENT_HDR + i) = c;
        }
    }
}
/// 把一段区域写成空槽 (`name_len == 0`)。
fn mfs_ent_clear(buf: *mut u8, off: usize, rec_len: usize) {
    write_u32(mfs_atm(buf, off), 0);
    unsafe {
        *mfs_atm(buf, off + 4) = 0;
        *mfs_atm(buf, off + 5) = 0;
    }
    write_u16(mfs_atm(buf, off + 6), rec_len as u16);
}
/// 按 `rec_len` 走到下一个条目; 结构损坏 (rec_len 太小 / 越界) 返回 None。
///
/// 走到区尾会返回恰好等于区尾偏移的值, 由调用方的 `off + MFS_DIR_ENT_HDR <= end`
/// 循环条件负责收尾 —— 这样「正常结束」与「损坏」不会混淆 (GC 需要区分二者)。
fn mfs_ent_step(buf: *const u8, off: usize) -> Option<usize> {
    let rl = mfs_ent_rec_len(buf, off);
    let end = MFS_HDR + MFS_PAYLOAD;
    if rl < MFS_DIR_ENT_MIN || off + rl > end {
        return None;
    }
    Some(off + rl)
}
/// 在一个目录块里扫描名字, 命中返回条目偏移。
fn mfs_dir_scan(buf: *const u8, comp: &[u8]) -> Option<usize> {
    let end = MFS_HDR + MFS_PAYLOAD;
    let mut off = MFS_HDR + MFS_DIR_HDR;
    while off + MFS_DIR_ENT_HDR <= end {
        if mfs_ent_name_eq(buf, off, comp) {
            return Some(off);
        }
        off = mfs_ent_step(buf, off)?;
    }
    None
}
/// 找可容纳 `need` 字节的插入点, 返回 `(条目前偏移, 该条目 rec_len, 该条目已用长度)`。
///
/// 空槽 (`name_len == 0`, used = 0) 与「有效条目 rec_len 里多出来的余量」都算可用空间
/// (ext2 first-fit)。余量来自删除时把空出长度并给了前一条目。
fn mfs_dir_slot(buf: *const u8, need: usize) -> Option<(usize, usize, usize)> {
    let end = MFS_HDR + MFS_PAYLOAD;
    let mut off = MFS_HDR + MFS_DIR_HDR;
    while off + MFS_DIR_ENT_HDR <= end {
        let rl = mfs_ent_rec_len(buf, off);
        if rl < MFS_DIR_ENT_MIN || off + rl > end {
            return None;
        }
        let nl = mfs_ent_name_len(buf, off);
        let used = if nl == 0 { 0 } else { mfs_ent_need(nl) };
        if rl >= used + need {
            return Some((off, rl, used));
        }
        off += rl;
    }
    None
}
/// 把条目放进 `(off, rl, used)`: 从 `off + used` 起占用, 余量切成空槽。
///
/// 若 `used > 0` (切的是某条有效条目的余量), 必须先把该条目的 rec_len 缩回 `used`,
/// 否则它的 rec_len 会越过新条目, 串联链就跳过了新条目 (插入成功却查不到)。
fn mfs_ent_place(
    buf: *mut u8,
    off: usize,
    rl: usize,
    used: usize,
    comp: &[u8],
    child: u32,
    typ: u32,
) {
    let need = mfs_ent_need(comp.len());
    let avail = rl - used;
    let take = if avail - need >= MFS_DIR_ENT_MIN {
        need
    } else {
        avail
    };
    let eoff = off + used;
    if used > 0 {
        write_u16(mfs_atm(buf, off + 6), used as u16);
    }
    mfs_ent_fill(buf, eoff, comp, child, typ, take);
    if take < avail {
        mfs_ent_clear(buf, eoff + take, avail - take);
    }
}
/// 目录块里是否存在有效条目。
fn mfs_dir_has_entry(buf: *const u8) -> bool {
    let end = MFS_HDR + MFS_PAYLOAD;
    let mut off = MFS_HDR + MFS_DIR_HDR;
    while off + MFS_DIR_ENT_HDR <= end {
        if mfs_ent_name_len(buf, off) != 0 {
            return true;
        }
        match mfs_ent_step(buf, off) {
            Some(n) => off = n,
            None => return false,
        }
    }
    false
}

// ---------------------------------------------------------------------------
// 目录项索引缓存 (02b)
//
// 动机: `mfs_dir_lookup` 每次都要读「节点块 + (如有) 索引块 + 逐扩展块」, 而每次
// `mfs_read_blk` 都是一次到 block_srv 的 IPC (≈1 tick ≈10 ms)。目录密集的负载 (如
// FS-12: 200 项目录逐项 open) 因此是 O(n²) 次块 IPC。这里按 `(卷, 目录 ino)` 把目录
// 条目解析成内存表 —— 命中即免 IPC。
//
// 正确性: 只缓存 `name -> (条目所在块, 块内偏移)`; 任何可能改变目录内容或块号的路径
// 都让对应目录失效 —— `mfs_itab_set` 是**唯一**改 inode 表槽的漏斗 (目录内容改动、
// chmod/chown、ino 复用、删除都要经它), GC 会搬块、mkfs 会重建, 二者整表失效。
// 缓存「不完整」(arena 装不下) 时, 未命中**必须回退**线性扫描 —— 绝不把"没缓存"
// 当成"不存在"。
// ---------------------------------------------------------------------------

/// 构建期开关 (02b 取证用: 唯一变量 = 目录索引缓存)。`false` 时全部走线性扫描 ——
/// 作为 before/after 对照。默认 `true`。
const MFS_DIDX_ENABLED: bool = true;
const MFS_DIDX_CACHE_DIRS: usize = 8;
const MFS_DIDX_ARENA: usize = 8 * 1024;

#[derive(Clone, Copy)]
struct DirIdxMeta {
    used: bool,
    complete: bool,
    vol: u64,
    dir_ino: u32,
    count: u16,
    arena_len: u16,
}
const DIRIDX_EMPTY: DirIdxMeta = DirIdxMeta {
    used: false,
    complete: false,
    vol: 0,
    dir_ino: 0,
    count: 0,
    arena_len: 0,
};

static mut MFS_DIDX_META: [DirIdxMeta; MFS_DIDX_CACHE_DIRS] = [DIRIDX_EMPTY; MFS_DIDX_CACHE_DIRS];
/// 每目录的条目数据 arena (变长打包: `[blk u32][off u16][name_len u8][name…]`)。
static mut MFS_DIDX_ARENAS: [[u8; MFS_DIDX_ARENA]; MFS_DIDX_CACHE_DIRS] =
    [[0u8; MFS_DIDX_ARENA]; MFS_DIDX_CACHE_DIRS];
static mut MFS_DIDX_EVICT: usize = 0;
static mut MFS_DIDX_LOOKUPS: u64 = 0;
static mut MFS_DIDX_HITS: u64 = 0;
static mut MFS_DIDX_BUILDS: u64 = 0;
static mut MFS_DIDX_EVICTS: u64 = 0;

fn mfs_didx_meta(i: usize) -> *mut DirIdxMeta {
    unsafe {
        core::ptr::addr_of_mut!(MFS_DIDX_META)
            .cast::<DirIdxMeta>()
            .add(i)
    }
}
fn mfs_didx_arena(i: usize) -> *mut u8 {
    unsafe {
        core::ptr::addr_of_mut!(MFS_DIDX_ARENAS)
            .cast::<[u8; MFS_DIDX_ARENA]>()
            .add(i)
            .cast::<u8>()
    }
}

/// 让某个目录 (任意卷) 的索引失效。
fn mfs_didx_invalidate(dir_ino: u32) {
    if dir_ino == 0 {
        return;
    }
    for i in 0..MFS_DIDX_CACHE_DIRS {
        let m = mfs_didx_meta(i);
        unsafe {
            if (*m).used && (*m).dir_ino == dir_ino {
                (*m).used = false;
            }
        }
    }
}

/// 整表失效 (GC 搬块 / mkfs 重建后)。
fn mfs_didx_invalidate_all() {
    for i in 0..MFS_DIDX_CACHE_DIRS {
        unsafe {
            (*mfs_didx_meta(i)).used = false;
        }
    }
}

/// `(卷, dir_ino)` 是否已缓存; 返回槽号。
fn mfs_didx_find(dir_ino: u32) -> Option<usize> {
    for i in 0..MFS_DIDX_CACHE_DIRS {
        let m = mfs_didx_meta(i);
        unsafe {
            if (*m).used && (*m).dir_ino == dir_ino && (*m).vol == MFS_CUR_VOL {
                return Some(i);
            }
        }
    }
    None
}

/// 取一个空槽; 没有则轮转淘汰一个 (clock)。
fn mfs_didx_slot() -> usize {
    for i in 0..MFS_DIDX_CACHE_DIRS {
        if !unsafe { (*mfs_didx_meta(i)).used } {
            return i;
        }
    }
    let i = unsafe { MFS_DIDX_EVICT } % MFS_DIDX_CACHE_DIRS;
    unsafe {
        MFS_DIDX_EVICT = (i + 1) % MFS_DIDX_CACHE_DIRS;
        MFS_DIDX_EVICTS += 1;
    }
    i
}

/// 把 `buf` (块号 `blk`) 里的有效条目追加进槽 `i`; 装不下则标记不完整并停止。
fn mfs_didx_fill_block(i: usize, blk: u32, buf: *const u8) {
    let m = mfs_didx_meta(i);
    let arena = mfs_didx_arena(i);
    let end = MFS_HDR + MFS_PAYLOAD;
    let mut off = MFS_HDR + MFS_DIR_HDR;
    while off + MFS_DIR_ENT_HDR <= end {
        let nl = mfs_ent_name_len(buf, off);
        if nl != 0 {
            // 名字必须真能装进本条目的 rec_len (防损坏条目越界读); 结构可疑就保守地
            // 把索引标成不完整 —— 未命中会回退线性扫描, 绝不误判"不存在"。
            if mfs_ent_need(nl) > mfs_ent_rec_len(buf, off) {
                unsafe { (*m).complete = false };
                return;
            }
            let used = unsafe { (*m).arena_len } as usize;
            if used + 7 + nl > MFS_DIDX_ARENA {
                unsafe { (*m).complete = false };
                return;
            }
            let blk_b = blk.to_le_bytes();
            let off_b = (off as u16).to_le_bytes();
            unsafe {
                let dst = arena.add(used);
                core::ptr::copy_nonoverlapping(blk_b.as_ptr(), dst, 4);
                core::ptr::copy_nonoverlapping(off_b.as_ptr(), dst.add(4), 2);
                *dst.add(6) = nl as u8;
                core::ptr::copy_nonoverlapping(mfs_at(buf, off + MFS_DIR_ENT_HDR), dst.add(7), nl);
                (*m).arena_len = (used + 7 + nl) as u16;
                (*m).count += 1;
            }
        }
        match mfs_ent_step(buf, off) {
            Some(n) => off = n,
            None => {
                // 走到损坏/异常边界: 索引不完整 (回退线性扫描), 不当作"目录到此结束"。
                unsafe { (*m).complete = false };
                return;
            }
        }
    }
}

/// 为目录 `dir_ino` 建索引 (当前卷)。失败返回 false → 调用方回退线性扫描。
fn mfs_didx_build(dir_ino: u32) -> bool {
    let base = match mfs_ino_block(dir_ino) {
        Some(b) if b != 0 => b,
        _ => return false,
    };
    let a = mfs_a();
    if !mfs_read_blk(base, a) || !mfs_ok(a, MFS_MAGIC_DIR) {
        return false;
    }
    let i = mfs_didx_slot();
    let m = mfs_didx_meta(i);
    unsafe {
        (*m).used = true;
        (*m).complete = true;
        (*m).vol = MFS_CUR_VOL;
        (*m).dir_ino = dir_ino;
        (*m).count = 0;
        (*m).arena_len = 0;
        MFS_DIDX_BUILDS += 1;
    }
    mfs_didx_fill_block(i, base, a);
    let ext = mfs_dir_ext(a);
    if ext != 0 {
        let b = mfs_b();
        if mfs_read_blk(ext, b) && mfs_ok(b, MFS_MAGIC_DIDX) {
            let c = mfs_c();
            for k in 0..MFS_DIR_SLOTS {
                let blk = read_u32(mfs_at(b, MFS_HDR + k * 4));
                if blk == 0 {
                    continue;
                }
                if mfs_read_blk(blk, c) && mfs_ok(c, MFS_MAGIC_DIR) {
                    mfs_didx_fill_block(i, blk, c);
                    if !unsafe { (*m).complete } {
                        break;
                    }
                }
            }
        }
    }
    true
}

/// 目录索引查找结果。
enum DidxLookup {
    Hit(MfsLoc),
    /// 索引**完整**且无此名字 → 可断定不存在。
    Absent,
    /// 没缓存 / 缓存不完整 / 建索引失败 → 必须回退线性扫描。
    Unknown,
}

fn mfs_didx_lookup(dir_ino: u32, comp: &[u8]) -> DidxLookup {
    if !MFS_DIDX_ENABLED {
        return DidxLookup::Unknown;
    }
    unsafe { MFS_DIDX_LOOKUPS += 1 };
    let i = match mfs_didx_find(dir_ino) {
        Some(i) => i,
        None => {
            if !mfs_didx_build(dir_ino) {
                return DidxLookup::Unknown;
            }
            match mfs_didx_find(dir_ino) {
                Some(k) => k,
                None => return DidxLookup::Unknown,
            }
        }
    };
    let m = mfs_didx_meta(i);
    let arena = mfs_didx_arena(i);
    let len = unsafe { (*m).arena_len } as usize;
    let mut p = 0usize;
    while p + 7 <= len {
        let (blk, off, nl) = unsafe {
            let q = arena.add(p);
            (
                u32::from_le_bytes([*q, *q.add(1), *q.add(2), *q.add(3)]),
                u16::from_le_bytes([*q.add(4), *q.add(5)]) as usize,
                *q.add(6) as usize,
            )
        };
        if nl == comp.len() {
            let hit = unsafe { core::slice::from_raw_parts(arena.add(p + 7), nl) == comp };
            if hit {
                unsafe { MFS_DIDX_HITS += 1 };
                return DidxLookup::Hit(MfsLoc { dir_ino, blk, off });
            }
        }
        p += 7 + nl;
    }
    if unsafe { (*m).complete } {
        DidxLookup::Absent
    } else {
        DidxLookup::Unknown
    }
}

/// 每 4096 次目录查找打印一次计数 (回归日志可 grep, 作为 02b 的取证)。
fn mfs_didx_maybe_log() {
    let n = unsafe { MFS_DIDX_LOOKUPS };
    if n != 0 && n % 4096 == 0 {
        print("mfs-didx: lookups=");
        print_u64(n);
        print(" hits=");
        print_u64(unsafe { MFS_DIDX_HITS });
        print(" builds=");
        print_u64(unsafe { MFS_DIDX_BUILDS });
        print(" evict=");
        print_u64(unsafe { MFS_DIDX_EVICTS });
        print(" entries=");
        let mut entries = 0u64;
        for i in 0..MFS_DIDX_CACHE_DIRS {
            entries += unsafe { (*mfs_didx_meta(i)).count } as u64;
        }
        print_u64(entries);
        println("");
    }
}

/// 在目录 (`dir_ino`, 含扩展块) 中查找条目。先查内存索引 (02b); 不命中再走线性扫描。
fn mfs_dir_lookup(dir_ino: u32, comp: &[u8]) -> Option<MfsLoc> {
    mfs_didx_maybe_log();
    match mfs_didx_lookup(dir_ino, comp) {
        DidxLookup::Hit(loc) => return Some(loc),
        DidxLookup::Absent => return None,
        DidxLookup::Unknown => {}
    }
    let base = mfs_ino_block(dir_ino)?;
    if base == 0 {
        return None;
    }
    let a = mfs_a();
    if !mfs_read_blk(base, a) || !mfs_ok(a, MFS_MAGIC_DIR) {
        return None;
    }
    if let Some(off) = mfs_dir_scan(a, comp) {
        return Some(MfsLoc {
            dir_ino,
            blk: base,
            off,
        });
    }
    let ext = mfs_dir_ext(a);
    if ext == 0 {
        return None;
    }
    let b = mfs_b();
    if !mfs_read_blk(ext, b) || !mfs_ok(b, MFS_MAGIC_DIDX) {
        return None;
    }
    let c = mfs_c();
    for i in 0..MFS_DIR_SLOTS {
        let blk = read_u32(mfs_at(b, MFS_HDR + i * 4));
        if blk == 0 {
            continue;
        }
        if !mfs_read_blk(blk, c) || !mfs_ok(c, MFS_MAGIC_DIR) {
            continue;
        }
        if let Some(off) = mfs_dir_scan(c, comp) {
            return Some(MfsLoc { dir_ino, blk, off });
        }
    }
    None
}

/// 把条目所在块读入 C 缓冲 (供修改)。
fn mfs_dir_load_loc(loc: &MfsLoc) -> bool {
    let c = mfs_c();
    mfs_read_blk(loc.blk, c) && mfs_ok(c, MFS_MAGIC_DIR)
}

/// 把已修改的条目所在块 (C) 写回。返回是否成功。
///
/// 条目就在目录节点块里 → 直接经 `mfs_commit_object` 提交 (换块 + 同步表槽);
/// 否则要 COW 扩展块 + 更新索引块槽位 + 回写节点块。层数固定 3 层 (不是沿链表级联),
/// 故修改代价与目录大小无关, 也与目录深度无关。
///
/// 目录内容被改动 → 顺带刷新其 mtime/ctime (节点块本来就要 COW, 不产生额外块)。
fn mfs_dir_store_loc(loc: &MfsLoc) -> bool {
    if Some(loc.blk) == mfs_ino_block(loc.dir_ino) {
        mfs_touch(mfs_c(), true);
        return mfs_commit_object(loc.dir_ino, mfs_c(), MFS_MAGIC_DIR).is_some();
    }
    let new_blk = match mfs_commit(mfs_c(), MFS_MAGIC_DIR) {
        Some(b) => b,
        None => return false,
    };
    let base = match mfs_ino_block(loc.dir_ino) {
        Some(b) if b != 0 => b,
        _ => return false,
    };
    let a = mfs_a();
    if !mfs_read_blk(base, a) || !mfs_ok(a, MFS_MAGIC_DIR) {
        return false;
    }
    mfs_touch(a, true);
    let idx = mfs_dir_ext(a);
    if idx == 0 {
        return false;
    }
    let b = mfs_b();
    if !mfs_read_blk(idx, b) || !mfs_ok(b, MFS_MAGIC_DIDX) {
        return false;
    }
    let mut found = false;
    for i in 0..MFS_DIR_SLOTS {
        if read_u32(mfs_at(b, MFS_HDR + i * 4)) == loc.blk {
            write_u32(mfs_atm(b, MFS_HDR + i * 4), new_blk);
            found = true;
            break;
        }
    }
    if !found {
        return false;
    }
    let new_idx = match mfs_commit(b, MFS_MAGIC_DIDX) {
        Some(x) => x,
        None => return false,
    };
    mfs_dir_set_ext(a, new_idx);
    mfs_commit_object(loc.dir_ino, a, MFS_MAGIC_DIR).is_some()
}

/// 目录是否为空 (无任何有效条目)。
fn mfs_dir_is_empty(dir_ino: u32) -> Option<bool> {
    let base = mfs_ino_block(dir_ino)?;
    if base == 0 {
        return None;
    }
    let a = mfs_a();
    if !mfs_read_blk(base, a) || !mfs_ok(a, MFS_MAGIC_DIR) {
        return None;
    }
    if mfs_dir_has_entry(a) {
        return Some(false);
    }
    let ext = mfs_dir_ext(a);
    if ext == 0 {
        return Some(true);
    }
    let b = mfs_b();
    if !mfs_read_blk(ext, b) || !mfs_ok(b, MFS_MAGIC_DIDX) {
        return None;
    }
    let c = mfs_c();
    for i in 0..MFS_DIR_SLOTS {
        let blk = read_u32(mfs_at(b, MFS_HDR + i * 4));
        if blk == 0 {
            continue;
        }
        if !mfs_read_blk(blk, c) || !mfs_ok(c, MFS_MAGIC_DIR) {
            continue;
        }
        if mfs_dir_has_entry(c) {
            return Some(false);
        }
    }
    Some(true)
}

/// 在目录 `dir_ino` 中插入条目 `comp -> child_ino`, 返回是否成功。
///
/// 内部完成扩展块 / 索引块的 COW, 最后经 `mfs_commit_object` 换掉目录节点块并同步
/// 它的 inode 表槽 —— 父目录条目存的是 ino, 故**不需要**回写任何祖先。
fn mfs_dir_insert(dir_ino: u32, comp: &[u8], child_ino: u32, typ: u32) -> bool {
    if comp.is_empty() || comp.len() > MFS_NAME_MAX {
        return false;
    }
    let need = mfs_ent_need(comp.len());
    let base = match mfs_ino_block(dir_ino) {
        Some(b) if b != 0 => b,
        _ => return false,
    };
    let a = mfs_a();
    if !mfs_read_blk(base, a) || !mfs_ok(a, MFS_MAGIC_DIR) {
        return false;
    }
    mfs_touch(a, true);
    // 1) 节点块内还有空间。
    if let Some((off, rl, used)) = mfs_dir_slot(a, need) {
        mfs_ent_place(a, off, rl, used, comp, child_ino, typ);
        return mfs_commit_object(dir_ino, a, MFS_MAGIC_DIR).is_some();
    }
    // 2) 取索引块 (不存在则物化一个空索引块并在 A 中登记)。
    let b = mfs_b();
    let mut idx = mfs_dir_ext(a);
    if idx == 0 {
        zero_bytes(b, MFS_BLOCK);
        idx = match mfs_commit(b, MFS_MAGIC_DIDX) {
            Some(x) => x,
            None => return false,
        };
        mfs_dir_set_ext(a, idx);
    } else if !mfs_read_blk(idx, b) || !mfs_ok(b, MFS_MAGIC_DIDX) {
        return false;
    }
    // 3) 已有扩展块里有空间。
    let c = mfs_c();
    for i in 0..MFS_DIR_SLOTS {
        let blk = read_u32(mfs_at(b, MFS_HDR + i * 4));
        if blk == 0 {
            continue;
        }
        if !mfs_read_blk(blk, c) || !mfs_ok(c, MFS_MAGIC_DIR) {
            continue;
        }
        let (off, rl, used) = match mfs_dir_slot(c, need) {
            Some(x) => x,
            None => continue,
        };
        mfs_ent_place(c, off, rl, used, comp, child_ino, typ);
        let new_ext = match mfs_commit(c, MFS_MAGIC_DIR) {
            Some(x) => x,
            None => return false,
        };
        write_u32(mfs_atm(b, MFS_HDR + i * 4), new_ext);
        let new_idx = match mfs_commit(b, MFS_MAGIC_DIDX) {
            Some(x) => x,
            None => return false,
        };
        mfs_dir_set_ext(a, new_idx);
        return mfs_commit_object(dir_ino, a, MFS_MAGIC_DIR).is_some();
    }
    // 4) 新增一个扩展块。
    zero_bytes(c, MFS_BLOCK);
    mfs_dir_init_empty(c);
    let (off, rl, used) = match mfs_dir_slot(c, need) {
        Some(x) => x,
        None => return false,
    };
    mfs_ent_place(c, off, rl, used, comp, child_ino, typ);
    let new_ext = match mfs_commit(c, MFS_MAGIC_DIR) {
        Some(x) => x,
        None => return false,
    };
    let mut placed = false;
    for i in 0..MFS_DIR_SLOTS {
        if read_u32(mfs_at(b, MFS_HDR + i * 4)) == 0 {
            write_u32(mfs_atm(b, MFS_HDR + i * 4), new_ext);
            placed = true;
            break;
        }
    }
    if !placed {
        return false; // 索引块满 (1022 个扩展块), 实际不可达
    }
    let new_idx = match mfs_commit(b, MFS_MAGIC_DIDX) {
        Some(x) => x,
        None => return false,
    };
    mfs_dir_set_ext(a, new_idx);
    mfs_commit_object(dir_ino, a, MFS_MAGIC_DIR).is_some()
}

/// 删除目录条目 (释放的长度并给前一项以回收碎片), 返回是否成功。
fn mfs_dir_delete(loc: &MfsLoc) -> bool {
    if !mfs_dir_load_loc(loc) {
        return false;
    }
    let c = mfs_c();
    let rl = mfs_ent_rec_len(c, loc.off);
    let end = MFS_HDR + MFS_PAYLOAD;
    let mut prev = None;
    let mut off = MFS_HDR + MFS_DIR_HDR;
    while off + MFS_DIR_ENT_HDR <= end && off < loc.off {
        let r = mfs_ent_rec_len(c, off);
        if r < MFS_DIR_ENT_MIN || off + r > end {
            return false;
        }
        prev = Some(off);
        off += r;
    }
    match prev {
        Some(p) => {
            let prl = mfs_ent_rec_len(c, p);
            write_u16(mfs_atm(c, p + 6), (prl + rl) as u16);
        }
        None => mfs_ent_clear(c, loc.off, rl),
    }
    mfs_dir_store_loc(loc)
}

fn mfs_file_size(buf: *const u8) -> u64 {
    read_u64(mfs_at(buf, MFS_FILE_SIZE_OFF))
}
fn mfs_file_set_size(buf: *mut u8, v: u64) {
    write_u64(mfs_atm(buf, MFS_FILE_SIZE_OFF), v);
}
fn mfs_file_nblocks(buf: *const u8) -> u32 {
    read_u32(mfs_at(buf, MFS_FILE_NBLOCKS_OFF))
}
fn mfs_file_set_nblocks(buf: *mut u8, v: u32) {
    write_u32(mfs_atm(buf, MFS_FILE_NBLOCKS_OFF), v);
}
/// 直接块指针 (i < `MFS_FILE_DIRECT`)。
fn mfs_file_direct(buf: *const u8, i: usize) -> u32 {
    read_u32(mfs_at(buf, MFS_FILE_DIRECT_OFF + i * 4))
}
fn mfs_file_set_direct(buf: *mut u8, i: usize, b: u32) {
    write_u32(mfs_atm(buf, MFS_FILE_DIRECT_OFF + i * 4), b);
}
fn mfs_file_ind1(buf: *const u8) -> u32 {
    read_u32(mfs_at(buf, MFS_FILE_IND1_OFF))
}
fn mfs_file_set_ind1(buf: *mut u8, b: u32) {
    write_u32(mfs_atm(buf, MFS_FILE_IND1_OFF), b);
}
fn mfs_file_ind2(buf: *const u8) -> u32 {
    read_u32(mfs_at(buf, MFS_FILE_IND2_OFF))
}
fn mfs_file_set_ind2(buf: *mut u8, b: u32) {
    write_u32(mfs_atm(buf, MFS_FILE_IND2_OFF), b);
}
fn mfs_file_ind3(buf: *const u8) -> u32 {
    read_u32(mfs_at(buf, MFS_FILE_IND3_OFF))
}
fn mfs_file_set_ind3(buf: *mut u8, b: u32) {
    write_u32(mfs_atm(buf, MFS_FILE_IND3_OFF), b);
}
/// 间接块内的第 `slot` 个指针。
fn mfs_ind_slot(buf: *const u8, slot: usize) -> u32 {
    read_u32(mfs_at(buf, MFS_HDR + slot * 4))
}
fn mfs_ind_set_slot(buf: *mut u8, slot: usize, b: u32) {
    write_u32(mfs_atm(buf, MFS_HDR + slot * 4), b);
}

// ---------------------------------------------------------------------------
// 逻辑块映射 (直接 -> 一级 -> 二级 -> 三级间接)
// ---------------------------------------------------------------------------

/// 逻辑块 `bi` 的映射位置。
///
/// `kind` 0 = inode 直接槽 (只用 `slot1`), 1 = 一级间接 (只用 `slot1`),
/// 2 = 二级间接 (`slot2` 定位一级块, `slot1` 定位块内槽位),
/// 3 = 三级间接 (`slot3` 定位二级块, `slot2` 定位一级块, `slot1` 定位块内槽位)。
#[derive(Clone, Copy)]
struct MfsMapPlan {
    kind: u32,
    slot3: usize,
    slot2: usize,
    slot1: usize,
}

/// 把逻辑块索引算成映射位置; 超出 `MFS_FILE_MAX_BLOCKS` 返回 None。
fn mfs_map_plan(bi: usize) -> Option<MfsMapPlan> {
    if bi < MFS_FILE_DIRECT {
        return Some(MfsMapPlan {
            kind: 0,
            slot3: 0,
            slot2: 0,
            slot1: bi,
        });
    }
    let idx = bi - MFS_FILE_DIRECT;
    if idx < MFS_IND_CAP {
        return Some(MfsMapPlan {
            kind: 1,
            slot3: 0,
            slot2: 0,
            slot1: idx,
        });
    }
    let idx = idx - MFS_IND_CAP;
    if idx < MFS_IND_CAP * MFS_IND_CAP {
        let slot2 = idx / MFS_IND_CAP;
        return Some(MfsMapPlan {
            kind: 2,
            slot3: 0,
            slot2,
            slot1: idx % MFS_IND_CAP,
        });
    }
    // 三级间接: idx3 先按二级块的容量 (CAP²) 切出 slot3, 余下再按一级块容量切。
    let idx3 = idx - MFS_IND_CAP * MFS_IND_CAP;
    let slot3 = idx3 / (MFS_IND_CAP * MFS_IND_CAP);
    if slot3 >= MFS_IND_CAP {
        return None;
    }
    let rest = idx3 % (MFS_IND_CAP * MFS_IND_CAP);
    Some(MfsMapPlan {
        kind: 3,
        slot3,
        slot2: rest / MFS_IND_CAP,
        slot1: rest % MFS_IND_CAP,
    })
}

/// 读路径: 逻辑块 `bi` 的物理块号 (0 = 空洞); 读盘/校验失败返回 None。
///
/// 用 B 缓冲承载间接块 (读路径 A = inode, C = 数据块, B 空闲); 每级读完立刻取走需要
/// 的槽值再复用, 故单缓冲即可逐级下降。
fn mfs_file_map(a: *const u8, bi: usize) -> Option<u32> {
    let p = mfs_map_plan(bi)?;
    if p.kind == 0 {
        return Some(mfs_file_direct(a, p.slot1));
    }
    let b = mfs_b();
    let l1 = if p.kind == 1 {
        mfs_file_ind1(a)
    } else if p.kind == 2 {
        let l2 = mfs_file_ind2(a);
        if l2 == 0 {
            return Some(0);
        }
        if !mfs_read_blk(l2, b) || !mfs_ok(b, MFS_MAGIC_IND2) {
            return None;
        }
        mfs_ind_slot(b, p.slot2)
    } else {
        // 三级: ind3 -> ind2 -> ind1, 任一指针为 0 即空洞。
        let l3 = mfs_file_ind3(a);
        if l3 == 0 {
            return Some(0);
        }
        if !mfs_read_blk(l3, b) || !mfs_ok(b, MFS_MAGIC_IND3) {
            return None;
        }
        let l2 = mfs_ind_slot(b, p.slot3);
        if l2 == 0 {
            return Some(0);
        }
        if !mfs_read_blk(l2, b) || !mfs_ok(b, MFS_MAGIC_IND2) {
            return None;
        }
        mfs_ind_slot(b, p.slot2)
    };
    if l1 == 0 {
        return Some(0);
    }
    if !mfs_read_blk(l1, b) || !mfs_ok(b, MFS_MAGIC_IND) {
        return None;
    }
    Some(mfs_ind_slot(b, p.slot1))
}

/// 写路径的「活动间接块」缓存。
///
/// 一次写调用常跨 1~2 个逻辑块且多落在同一个间接块内; 缓存它可避免对同一个间接块
/// 反复「读-改-COW」(间接块自身也是要写盘的 4 KiB 块)。缓冲同样借用 B:
/// 写数据阶段 A = inode、C = 数据块、B 归本缓存 (COW 上溯才用 B, 那时已 flush)。
#[derive(Clone, Copy)]
struct MfsIndCache {
    /// 0 = 未持有; 1 / 2 同 `MfsMapPlan::kind`。
    kind: u32,
    /// kind == 2 时: 该一级块在二级块中的槽位。
    slot2: usize,
    /// 缓冲内是否有未写回的修改。
    dirty: bool,
}
const MFS_IND_CACHE_EMPTY: MfsIndCache = MfsIndCache {
    kind: 0,
    slot2: 0,
    dirty: false,
};

/// 把缓存里的一级间接块 COW 出去并回写父级 (inode 的一级指针或二级块槽位)。
fn mfs_ind_flush(a: *mut u8, cache: &mut MfsIndCache) -> bool {
    if cache.kind == 0 || !cache.dirty {
        return true;
    }
    let b = mfs_b();
    let new_l1 = match mfs_commit(b, MFS_MAGIC_IND) {
        Some(x) => x,
        None => return false,
    };
    if cache.kind == 1 {
        mfs_file_set_ind1(a, new_l1);
    } else {
        // 二级: 读二级块 (B 已被 commit 占用, 一级内容已落盘, 可安全复用) -> 改槽 ->
        // COW 二级块 -> 回写 inode。二级块在加载该一级块时已保证存在。
        let l2 = mfs_file_ind2(a);
        if l2 == 0 || !mfs_read_blk(l2, b) || !mfs_ok(b, MFS_MAGIC_IND2) {
            return false;
        }
        mfs_ind_set_slot(b, cache.slot2, new_l1);
        let new_l2 = match mfs_commit(b, MFS_MAGIC_IND2) {
            Some(x) => x,
            None => return false,
        };
        mfs_file_set_ind2(a, new_l2);
    }
    cache.dirty = false;
    true
}

/// 让缓存持有覆盖 `bi` 的一级间接块 (必要时先 flush 旧组再加载新组)。
fn mfs_ind_load(a: *mut u8, bi: usize, cache: &mut MfsIndCache) -> bool {
    let p = match mfs_map_plan(bi) {
        Some(p) => p,
        None => return false,
    };
    if p.kind == 0 {
        // 直接槽不经间接块; 若缓存里还压着脏块则先落盘 (正常路径 bi 单调递增,
        // 先走直接区再走间接区, 不会碰到; 这里只为不给「静默丢改动」留口子)。
        if !mfs_ind_flush(a, cache) {
            return false;
        }
        cache.kind = 0;
        return true;
    }
    if cache.kind == p.kind && cache.slot2 == p.slot2 {
        return true;
    }
    if !mfs_ind_flush(a, cache) {
        return false;
    }
    let b = mfs_b();
    if p.kind == 1 {
        let l1 = mfs_file_ind1(a);
        if l1 == 0 {
            zero_bytes(b, MFS_BLOCK);
        } else if !mfs_read_blk(l1, b) || !mfs_ok(b, MFS_MAGIC_IND) {
            return false;
        }
    } else {
        // 二级: 二级块不存在就当场建一个空块 (flush 需要它存在), 再取其中的一级块。
        let mut l2 = mfs_file_ind2(a);
        if l2 == 0 {
            zero_bytes(b, MFS_BLOCK);
            l2 = match mfs_commit(b, MFS_MAGIC_IND2) {
                Some(x) => x,
                None => return false,
            };
            mfs_file_set_ind2(a, l2);
        } else if !mfs_read_blk(l2, b) || !mfs_ok(b, MFS_MAGIC_IND2) {
            return false;
        }
        let l1 = mfs_ind_slot(b, p.slot2);
        if l1 == 0 {
            zero_bytes(b, MFS_BLOCK);
        } else if !mfs_read_blk(l1, b) || !mfs_ok(b, MFS_MAGIC_IND) {
            return false;
        }
    }
    cache.kind = p.kind;
    cache.slot2 = p.slot2;
    cache.dirty = false;
    true
}

/// 写路径: 读逻辑块 `bi` 当前的物理块号 (0 = 空洞), 顺带把它的间接块载入缓存。
///
/// 三级间接块 (kind 3) 不由缓存承载: 先 flush 并清空缓存, 再走 `mfs_ind_peek3` ——
/// 否则缓存里的一级块内容会与三级链路共用的 B 缓冲相互覆盖。
fn mfs_ind_peek(a: *mut u8, bi: usize, cache: &mut MfsIndCache) -> Option<u32> {
    let p = mfs_map_plan(bi)?;
    if p.kind == 0 {
        return Some(mfs_file_direct(a, p.slot1));
    }
    if p.kind == 3 {
        if !mfs_ind_flush(a, cache) {
            return None;
        }
        *cache = MFS_IND_CACHE_EMPTY;
        return mfs_ind_peek3(a, &p);
    }
    if !mfs_ind_load(a, bi, cache) {
        return None;
    }
    Some(mfs_ind_slot(mfs_b(), p.slot1))
}

/// 写路径 (三级间接专用): 链式读取 ind3 -> ind2 -> ind1, 返回 ind1 里 `slot1` 的数据
/// 块号 (0 = 空洞)。
///
/// 三级只服务 >4 GiB 文件, 为保持缓存实现简单而走**无缓存链路** (常规文件不受影响);
/// 每级读完立刻取走所需槽值, 全程只用 B 一页缓冲。
fn mfs_ind_peek3(a: *mut u8, plan: &MfsMapPlan) -> Option<u32> {
    let b = mfs_b();
    let l3 = mfs_file_ind3(a);
    if l3 == 0 {
        return Some(0);
    }
    if !mfs_read_blk(l3, b) || !mfs_ok(b, MFS_MAGIC_IND3) {
        return None;
    }
    let l2 = mfs_ind_slot(b, plan.slot3);
    if l2 == 0 {
        return Some(0);
    }
    if !mfs_read_blk(l2, b) || !mfs_ok(b, MFS_MAGIC_IND2) {
        return None;
    }
    let l1 = mfs_ind_slot(b, plan.slot2);
    if l1 == 0 {
        return Some(0);
    }
    if !mfs_read_blk(l1, b) || !mfs_ok(b, MFS_MAGIC_IND) {
        return None;
    }
    Some(mfs_ind_slot(b, plan.slot1))
}

/// 写路径 (三级间接专用): 把 `bi` 指向 `db`, 自下而上逐级「读-改-COW」回写 ——
/// COW ind1 -> 写回父 ind2 的 `slot2` -> COW ind2 -> 写回 ind3 的 `slot3` -> COW ind3
/// -> 写回 inode 的 ind3 指针。中间级不存在时当场建空块 (与 `mfs_ind_load` 同策略)。
///
/// 全程只用 B 一页缓冲: 每级校验/建好之后立刻取走所需槽值, 再复用该页写下一级。
fn mfs_ind_link3(a: *mut u8, plan: &MfsMapPlan, db: u32) -> bool {
    let b = mfs_b();
    // 自上而下取出三级、二级、一级块号 (只读, 读完即取走槽值)。
    let l3 = mfs_file_ind3(a);
    let l2 = if l3 == 0 {
        0
    } else {
        if !mfs_read_blk(l3, b) || !mfs_ok(b, MFS_MAGIC_IND3) {
            return false;
        }
        mfs_ind_slot(b, plan.slot3)
    };
    let l1 = if l2 == 0 {
        0
    } else {
        if !mfs_read_blk(l2, b) || !mfs_ok(b, MFS_MAGIC_IND2) {
            return false;
        }
        mfs_ind_slot(b, plan.slot2)
    };
    // 一级: 载入 (不存在则清零当空块) -> 改 slot1 -> COW。
    if l1 == 0 {
        zero_bytes(b, MFS_BLOCK);
    } else if !mfs_read_blk(l1, b) || !mfs_ok(b, MFS_MAGIC_IND) {
        return false;
    }
    mfs_ind_set_slot(b, plan.slot1, db);
    let new_l1 = match mfs_commit(b, MFS_MAGIC_IND) {
        Some(x) => x,
        None => return false,
    };
    // 二级: 载入 -> 改 slot2 指向新一级块 -> COW。
    if l2 == 0 {
        zero_bytes(b, MFS_BLOCK);
    } else if !mfs_read_blk(l2, b) || !mfs_ok(b, MFS_MAGIC_IND2) {
        return false;
    }
    mfs_ind_set_slot(b, plan.slot2, new_l1);
    let new_l2 = match mfs_commit(b, MFS_MAGIC_IND2) {
        Some(x) => x,
        None => return false,
    };
    // 三级: 载入 -> 改 slot3 指向新二级块 -> COW -> 回写 inode 指针。
    if l3 == 0 {
        zero_bytes(b, MFS_BLOCK);
    } else if !mfs_read_blk(l3, b) || !mfs_ok(b, MFS_MAGIC_IND3) {
        return false;
    }
    mfs_ind_set_slot(b, plan.slot3, new_l2);
    let new_l3 = match mfs_commit(b, MFS_MAGIC_IND3) {
        Some(x) => x,
        None => return false,
    };
    mfs_file_set_ind3(a, new_l3);
    true
}

/// 写路径: 把逻辑块 `bi` 指向 `db` (直接槽写 inode; 三级块走无缓存链路; 否则写进缓存
/// 中的一级间接块)。
///
/// 调用前必须对同一个 `bi` 调过 `mfs_ind_peek` (保证目标组已载入缓存; 三级块除外)。
fn mfs_ind_link(a: *mut u8, bi: usize, db: u32, cache: &mut MfsIndCache) -> bool {
    let p = match mfs_map_plan(bi) {
        Some(p) => p,
        None => return false,
    };
    if p.kind == 0 {
        mfs_file_set_direct(a, p.slot1, db);
        return true;
    }
    if p.kind == 3 {
        return mfs_ind_link3(a, &p, db);
    }
    if cache.kind != p.kind || cache.slot2 != p.slot2 {
        return false;
    }
    mfs_ind_set_slot(mfs_b(), p.slot1, db);
    cache.dirty = true;
    true
}

/// 把存储名 (dotted 大写) 转成 11 字节 FAT 8.3 短名 (主名 8 + 扩展 3, 空格填充)。
///
/// MFS 名字不再以 NUL 结尾 (变长条目由 `name_len` 给出长度), 故这里收切片。
fn mfs_name_to_fat(name: &[u8], out: &mut [u8; 11]) {
    *out = [b' '; 11];
    let len = name.len().min(MFS_NAME_MAX);
    let seg = &name[..len];
    let mut dot = len;
    for (i, &c) in seg.iter().enumerate() {
        if c == b'.' {
            dot = i;
            break;
        }
    }
    let base = &seg[..dot];
    let ext = if dot < len {
        &seg[dot + 1..len]
    } else {
        &seg[len..len]
    };
    let bn = base.len().min(8);
    out[..bn].copy_from_slice(&base[..bn]);
    let en = ext.len().min(3);
    out[8..8 + en].copy_from_slice(&ext[..en]);
}

// ---------------------------------------------------------------------------
// 路径解析 + COW 上溯
// ---------------------------------------------------------------------------

/// 规范化 MFS 绝对路径: 处理 "." / ".." 与重复 '/', 分量**原样保留**。
///
/// 与 tmpfs 的 8.3 规整 (`tmp_normalize`) 不同: MFS v2 起名字是长度 ≤ `MFS_NAME_MAX`
/// 的任意字节串, 大小写敏感、按字节精确匹配 (与 ext2 一致), 不做大写化或截断。
/// 端到端长度另受单条 IPC 路径 (payload) 限制。返回长度; 越界/空分量返回 None。
fn mfs_normalize(path: &str, out: &mut [u8]) -> Option<usize> {
    let bytes = path.as_bytes();
    if bytes.first() != Some(&b'/') || out.is_empty() {
        return None;
    }
    out[0] = b'/';
    let mut n = 1usize;
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
            // 回退一级 (已在根时不动)。
            if n > 1 {
                let mut k = n - 1;
                while k > 0 && out[k - 1] != b'/' {
                    k -= 1;
                }
                n = if k > 1 { k - 1 } else { 1 };
            }
            continue;
        }
        if seg.len() > MFS_NAME_MAX {
            return None;
        }
        let sep = usize::from(n > 1);
        if n + sep + seg.len() > out.len() {
            return None;
        }
        if sep == 1 {
            out[n] = b'/';
            n += 1;
        }
        out[n..n + seg.len()].copy_from_slice(seg);
        n += seg.len();
    }
    Some(n)
}

/// 读软链接节点 `ino` 的目标路径到 `out`, 返回目标字节数 (不含 NUL)。
///
/// 目标**原样**存储在节点里 (以 '/' 开头 = 绝对路径, 否则相对链接所在目录); 这里也
/// 原样取出 —— 规范化与「绝对/相对」的判断都留给解析方。
fn mfs_link_target(ino: u32, out: &mut [u8]) -> Option<usize> {
    let blk = mfs_ino_block(ino)?;
    if blk == 0 {
        return None;
    }
    let a = mfs_a();
    if !mfs_read_blk(blk, a) || !mfs_ok(a, MFS_MAGIC_LINK) {
        return None;
    }
    let tlen = mfs_file_size(a) as usize;
    if tlen == 0 || tlen > MFS_LINK_MAX || tlen > out.len() {
        return None;
    }
    unsafe {
        core::ptr::copy_nonoverlapping(mfs_at(a, MFS_LINK_TARGET_OFF), out.as_mut_ptr(), tlen);
    }
    Some(tlen)
}

/// 解析规范化绝对路径, 返回叶子节点的 **inode 号**, 沿途**跟随软链接**。
///
/// 顺带把叶子条目位置记进 `MFS_LEAF` —— 需要改/删这个条目的调用方 (rename / unlink)
/// 直接用它。MFS6 起解析不再需要沿途记录整条链: 目录项存 ino, 改对象不影响祖先。
fn mfs_resolve(canon: &[u8]) -> Option<u32> {
    mfs_resolve_ex(canon, true)
}

/// 同 `mfs_resolve`, 但**不跟随最后一段**上的软链接。
///
/// `unlink` / `rmdir` / `rename` 作用在条目本身 (删除、移动的是链接而不是它的目标),
/// 用这个版本; 路径中间分量上的软链接仍然跟随 —— `/a/link/b` 必须走进 link 指到的
/// 那个目录才能找到 b。
fn mfs_resolve_no_follow(canon: &[u8]) -> Option<u32> {
    mfs_resolve_ex(canon, false)
}

/// 解析主体。`follow_leaf` = 最后一段是软链接时是否跟随。
///
/// 跟随的实现是「就地展开 + 整条重走」: 把路径里那个链接分量替换成它的目标 (绝对目标
/// 直接用, 相对目标接到链接所在目录之后), 保留其后的剩余分量, 重新规范化后从头再走
/// 一遍。不接着展开点往下走, 是因为目标里的 `..` 可能吃掉展开点**之前**的目录, 分量
/// 位置会整体变化; 从头重走只是多几次目录查找, 换来逻辑简单可靠。展开次数由
/// `MFS_SYMLINK_MAX_DEPTH` 兜底, 因此链接成环只会解析失败, 不会无限展开。
fn mfs_resolve_ex(canon: &[u8], follow_leaf: bool) -> Option<u32> {
    unsafe {
        MFS_LEAF = MFS_LOC_EMPTY;
    }
    if canon.len() > MFS_PATH_MAX {
        return None;
    }
    // 工作缓冲: 链接展开会就地重写整条路径。
    let mut buf = [0u8; MFS_PATH_MAX];
    let mut len = canon.len();
    buf[..len].copy_from_slice(canon);
    if len == 1 {
        return Some(MFS_ROOT_INO); // 根
    }
    let mut links = 0u32;
    loop {
        let mut i = 1usize;
        let mut cur = MFS_ROOT_INO;
        // 本轮走到的软链接分量 (需展开): (分量起点, 分量终点, 链接 ino)。
        let mut expand: Option<(usize, usize, u32)> = None;
        while i < len {
            let start = i;
            while i < len && buf[i] != b'/' {
                i += 1;
            }
            let comp_end = i;
            let comp_len = comp_end - start;
            if comp_len == 0 || comp_len > MFS_NAME_MAX {
                return None;
            }
            let loc = mfs_dir_lookup(cur, &buf[start..comp_end])?;
            // 条目可能落在扩展块里, 上一步的 C 缓冲已被覆盖 —— 重新读条目所在块。
            let (child_ino, is_link) = {
                let c = mfs_c();
                if !mfs_read_blk(loc.blk, c) || !mfs_ok(c, MFS_MAGIC_DIR) {
                    return None;
                }
                (
                    mfs_ent_ino(c, loc.off),
                    mfs_ent_type(c, loc.off) == MFS_TYPE_LINK,
                )
            };
            if child_ino == 0 {
                return None;
            }
            let is_leaf = comp_end >= len;
            if is_link && (!is_leaf || follow_leaf) {
                expand = Some((start, comp_end, child_ino));
                break;
            }
            unsafe {
                MFS_LEAF = loc;
            }
            cur = child_ino;
            if is_leaf {
                return Some(cur);
            }
            i = comp_end + 1; // 跳过 '/'
        }
        // 本轮没碰到软链接却走到了这里 —— 说明分量没走完 (规范化后的路径不该出现)。
        let (start, comp_end, link_ino) = expand?;
        links += 1;
        if links > MFS_SYMLINK_MAX_DEPTH {
            return None; // 链接成环 / 链过长
        }
        // 组装新路径: [父目录前缀] + 目标 + [剩余分量] (绝对目标不带父前缀)。
        let mut target = [0u8; MFS_LINK_MAX];
        let tlen = mfs_link_target(link_ino, &mut target)?;
        let parent = if target[0] == b'/' { 0 } else { start };
        let rest = &buf[comp_end..len]; // 以 '/' 开头, 或为空
        let mut merged = [0u8; MFS_PATH_MAX];
        let mut n = 0usize;
        if parent > 0 {
            merged[..parent].copy_from_slice(&buf[..parent]);
            n = parent;
        }
        if n + tlen + rest.len() > MFS_PATH_MAX {
            return None; // 展开后超长: 直接失败, 不截断成一条错路径
        }
        merged[n..n + tlen].copy_from_slice(&target[..tlen]);
        n += tlen;
        merged[n..n + rest.len()].copy_from_slice(rest);
        n += rest.len();
        // 目标里可能带 '.' / '..' / 重复 '/', 重新规范化后再整条重走。
        let mut canon2 = [0u8; MFS_PATH_MAX];
        let cn = mfs_normalize(
            unsafe { core::str::from_utf8_unchecked(&merged[..n]) },
            &mut canon2,
        )?;
        len = cn;
        buf[..len].copy_from_slice(&canon2[..len]);
        if len == 1 {
            return Some(MFS_ROOT_INO);
        }
    }
}

/// 读取文件 `ino` 的 [offset, offset+count) 区间到 `dst`, 返回读取字节数。
///
/// **空洞按 0 返回**: 逻辑块未分配 (`db == 0`) 或超出 `nblocks` 时, 该段视为稀疏空洞,
/// 填 0 后继续 —— 若在这里 break, 稀疏文件 (truncate 扩展出来的区段) 会读成短读。
///
/// 02b-2: 按「最多 `MFS_RDBUF_PAGES` 个连续逻辑块」为一个窗口做**批读** —— 把窗口内各
/// 已分配块的目标物理块填进共享描述符页, 一次 [`block_batch`] 下发; 空洞填 0、逐块
/// `mfs_ok` 校验在收到数据后处理; 批读失败退回逐块直读。元数据块 (inode / 间接块) 仍走
/// [`mfs_read_blk`] 的只读缓存 —— 它们被反复读, 缓存收益更大。
fn mfs_read_file(ino: u32, offset: u64, count: u32, dst: *mut u8) -> u64 {
    let a = mfs_a();
    let block = match mfs_ino_block(ino) {
        Some(b) if b != 0 => b,
        _ => return u64::MAX,
    };
    if !mfs_read_blk(block, a) || !mfs_ok(a, MFS_MAGIC_FILE) {
        return u64::MAX;
    }
    let size = mfs_file_size(a);
    if offset >= size {
        return 0;
    }
    let end = core::cmp::min(offset + count as u64, size);
    let n = (end - offset) as u32;
    let nblocks = mfs_file_nblocks(a) as usize;
    let vol = unsafe { MFS_CUR_VOL };

    let mut done = 0u32;
    while done < n {
        let pos = offset + done as u64;
        let bi0 = (pos as usize) / MFS_DATA_CAP;
        let first_boff = (pos as usize) % MFS_DATA_CAP;

        // 本窗逻辑块数: 按剩余字节跨的块数取, 不超过窗口容量。
        let mut jmax = ((n - done) as usize + first_boff).div_ceil(MFS_DATA_CAP);
        if jmax > MFS_RDBUF_PAGES {
            jmax = MFS_RDBUF_PAGES;
        }

        // 1) 收集本窗各逻辑块的目标物理块 (空洞 = 0, 不占读窗口槽)。
        let mut db_of = [0u32; MFS_RDBUF_PAGES];
        let mut slot_of = [usize::MAX; MFS_RDBUF_PAGES];
        let mut ndb = 0usize;
        let mut j = 0usize;
        while j < jmax {
            let bi = bi0 + j;
            if bi < nblocks {
                db_of[j] = match mfs_file_map(a, bi) {
                    Some(x) => x,
                    None => return u64::MAX,
                };
            }
            if db_of[j] != 0 {
                slot_of[j] = ndb;
                unsafe {
                    core::ptr::write_unaligned(
                        mfs_rdbuf_desc().add(ndb),
                        BatchEnt {
                            lba: db_of[j] as u64 * MFS_SECTORS_PER_BLOCK as u64,
                            sectors: MFS_SECTORS_PER_BLOCK as u64,
                            buf: mfs_rdbuf(ndb) as u64,
                        },
                    );
                }
                ndb += 1;
            }
            j += 1;
        }

        // 2) 一批读下全部非空块; 失败退回逐块直读。
        if ndb > 0 {
            let mut ok = block_batch(vol, mfs_rdbuf_desc(), ndb, false) == 1;
            if !ok {
                ok = true;
                let mut t = 0usize;
                while t < ndb {
                    let ent = unsafe { core::ptr::read_unaligned(mfs_rdbuf_desc().add(t)) };
                    if !block_raw_read(vol, ent.lba as u32, ent.sectors as u16, ent.buf as *mut u8)
                    {
                        ok = false;
                        break;
                    }
                    t += 1;
                }
            }
            if !ok {
                return u64::MAX;
            }
        }

        // 3) 逐块校验并拷贝到 dst (空洞填 0)。
        let mut j = 0usize;
        while j < jmax && done < n {
            let boff = if j == 0 { first_boff } else { 0 };
            let chunk = core::cmp::min(MFS_DATA_CAP - boff, (n - done) as usize);
            if db_of[j] == 0 {
                unsafe {
                    core::ptr::write_bytes(dst.add(done as usize), 0, chunk);
                }
            } else {
                let src = mfs_rdbuf(slot_of[j]);
                if !mfs_ok(src, MFS_MAGIC_DATA) {
                    return u64::MAX;
                }
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        mfs_at(src, MFS_HDR + boff),
                        dst.add(done as usize),
                        chunk,
                    );
                }
            }
            done += chunk as u32;
            j += 1;
        }
    }
    done as u64
}

/// 写文件 `ino` 的 [offset, offset+count) 区间, COW 数据块 (必要时含一/二/三级间接块) +
/// 文件节点, 再同步它在 inode 表里的槽位。返回写入字节数, 失败 `u64::MAX`。
fn mfs_write_file(ino: u32, offset: u64, count: u32, src: *const u8) -> u64 {
    let a = mfs_a();
    let block = match mfs_ino_block(ino) {
        Some(b) if b != 0 => b,
        _ => return u64::MAX,
    };
    if !mfs_read_blk(block, a) || !mfs_ok(a, MFS_MAGIC_FILE) {
        return u64::MAX;
    }
    let old_size = mfs_file_size(a);
    let mut nblocks = mfs_file_nblocks(a) as usize;
    let c = mfs_c();
    // 结束位置按 64 位算: 文件上限已与卷容量同量级, 只需挡住超出结构可寻址范围的写入。
    let end = offset + count as u64;
    let max_bytes = MFS_FILE_MAX_BLOCKS as u64 * MFS_DATA_CAP as u64;
    if end > max_bytes {
        return u64::MAX;
    }
    let mut cache = MFS_IND_CACHE_EMPTY;
    let mut done = 0u32;
    while done < count {
        let pos = offset + done as u64;
        let bi = (pos as usize) / MFS_DATA_CAP;
        let boff = (pos as usize) % MFS_DATA_CAP;
        let chunk = core::cmp::min(MFS_DATA_CAP - boff, (count - done) as usize);
        if bi >= MFS_FILE_MAX_BLOCKS {
            return u64::MAX;
        }
        // 读旧数据块 (存在则复制, 否则清零); 顺带把该块所属的一级间接块载入缓存。
        let old_db = match mfs_ind_peek(a, bi, &mut cache) {
            Some(x) => x,
            None => return u64::MAX,
        };
        if old_db != 0 {
            if !mfs_read_blk(old_db, c) || !mfs_ok(c, MFS_MAGIC_DATA) {
                return u64::MAX;
            }
        } else {
            zero_bytes(c, MFS_BLOCK);
        }
        unsafe {
            core::ptr::copy_nonoverlapping(
                src.add(done as usize),
                mfs_atm(c, MFS_HDR + boff),
                chunk,
            );
        }
        let new_db = match mfs_commit(c, MFS_MAGIC_DATA) {
            Some(b) => b,
            None => return u64::MAX,
        };
        if !mfs_ind_link(a, bi, new_db, &mut cache) {
            return u64::MAX;
        }
        // 追加块: 只需抬高逻辑块数 (未分配槽位恒为 0, 空洞无需显式填写)。
        if bi >= nblocks {
            nblocks = bi + 1;
            mfs_file_set_nblocks(a, nblocks as u32);
        }
        done += chunk as u32;
    }
    // 收尾: 把缓存里最后一个间接块 COW 落盘并回写 inode 指针, 再提交新 inode。
    if !mfs_ind_flush(a, &mut cache) {
        return u64::MAX;
    }
    if end > old_size {
        mfs_file_set_size(a, end);
    }
    mfs_touch(a, false); // 内容变更 -> mtime / ctime
    if mfs_commit_object(ino, a, MFS_MAGIC_FILE).is_none() {
        return u64::MAX;
    }
    count as u64
}

/// 清掉逻辑块 `bi` 的映射 (直接槽或间接块槽位置 0), 返回是否成功。
///
/// 三级块经无缓存链路 (`mfs_ind_peek3` / `mfs_ind_link3`) 处理; 一/二级仍走缓存。
/// 只清指针、不回收块 —— 块由 GC 按可达性回收, 故这里无需关心"谁还在用"。
fn mfs_unmap_block(a: *mut u8, bi: usize, cache: &mut MfsIndCache) -> bool {
    let old = match mfs_ind_peek(a, bi, cache) {
        Some(x) => x,
        None => return false,
    };
    if old == 0 {
        return true;
    }
    mfs_ind_link(a, bi, 0, cache)
}

/// 把文件 `ino` 的长度改到 `new_size` 字节, 成功返回 1, 失败 `u64::MAX`。
///
/// - `new_size < 现有长度`: 截短。保留前 `keep = ceil(new_size / MFS_DATA_CAP)` 个逻辑块,
///   其余映射一律清 0。整段不再需要时直接丢一/二/三级指针 (块本体交给 GC), 只有部分保留
///   的那一段才逐槽清理。
/// - `new_size > 现有长度`: 稀疏扩展 —— 只抬高 `size`, 不分配块 (未写过的区间读回 0)。
fn mfs_truncate(ino: u32, new_size: u64) -> u64 {
    let a = mfs_a();
    let block = match mfs_ino_block(ino) {
        Some(b) if b != 0 => b,
        _ => return u64::MAX,
    };
    if !mfs_read_blk(block, a) || !mfs_ok(a, MFS_MAGIC_FILE) {
        return u64::MAX;
    }
    let old_size = mfs_file_size(a);
    if new_size == old_size {
        return 1;
    }
    if new_size > old_size {
        mfs_file_set_size(a, new_size);
        mfs_touch(a, false);
        return if mfs_commit_object(ino, a, MFS_MAGIC_FILE).is_some() {
            1
        } else {
            u64::MAX
        };
    }
    // 截短: 保留前 `keep` 个逻辑块。
    let keep = (new_size as usize).div_ceil(MFS_DATA_CAP);
    let old_nb = mfs_file_nblocks(a) as usize;
    let dir_end = MFS_FILE_DIRECT;
    let ind1_end = dir_end + MFS_IND_CAP;
    let ind2_end = ind1_end + MFS_IND_CAP * MFS_IND_CAP;
    let mut cache = MFS_IND_CACHE_EMPTY;
    // 1) 直接区尾部逐槽清零。
    let mut bi = keep;
    while bi < dir_end && bi < old_nb {
        mfs_file_set_direct(a, bi, 0);
        bi += 1;
    }
    // 2) 一级间接区: 整组保留 / 整组丢弃 / 部分保留。
    if old_nb > dir_end {
        if keep <= dir_end {
            mfs_file_set_ind1(a, 0);
        } else {
            let mut b = keep.max(dir_end);
            while b < ind1_end && b < old_nb {
                if !mfs_unmap_block(a, b, &mut cache) {
                    return u64::MAX;
                }
                b += 1;
            }
        }
    }
    // 3) 二级间接区同理 (循环上界到 ind2_end, 三级区留给下一步)。
    if old_nb > ind1_end {
        if keep <= ind1_end {
            mfs_file_set_ind2(a, 0);
        } else {
            let mut b = keep.max(ind1_end);
            while b < ind2_end && b < old_nb {
                if !mfs_unmap_block(a, b, &mut cache) {
                    return u64::MAX;
                }
                b += 1;
            }
        }
    }
    // 4) 三级间接区: 不在保留范围内就整段丢指针; 否则逐槽清理 (走无缓存链路)。
    if old_nb > ind2_end {
        if keep <= ind2_end {
            mfs_file_set_ind3(a, 0);
        } else {
            let mut b = keep.max(ind2_end);
            while b < old_nb {
                if !mfs_unmap_block(a, b, &mut cache) {
                    return u64::MAX;
                }
                b += 1;
            }
        }
    }
    if !mfs_ind_flush(a, &mut cache) {
        return u64::MAX;
    }
    // 5) 最后一个保留块若只用到一半, 把尾部清零 —— 否则日后扩展回来会读出截断前的旧数据
    //    (Linux ftruncate 同样会清掉部分块的尾部, 这里与之一致; 只在非块对齐时付一次块 COW)。
    let tail = new_size as usize % MFS_DATA_CAP;
    if keep > 0 && tail != 0 {
        let last = keep - 1;
        let old_db = match mfs_ind_peek(a, last, &mut cache) {
            Some(x) => x,
            None => return u64::MAX,
        };
        if old_db != 0 {
            let c = mfs_c();
            if !mfs_read_blk(old_db, c) || !mfs_ok(c, MFS_MAGIC_DATA) {
                return u64::MAX;
            }
            zero_bytes(mfs_atm(c, MFS_HDR + tail), MFS_DATA_CAP - tail);
            let new_db = match mfs_commit(c, MFS_MAGIC_DATA) {
                Some(b) => b,
                None => return u64::MAX,
            };
            if !mfs_ind_link(a, last, new_db, &mut cache) {
                return u64::MAX;
            }
            if !mfs_ind_flush(a, &mut cache) {
                return u64::MAX;
            }
        }
    }
    mfs_file_set_size(a, new_size);
    mfs_file_set_nblocks(a, keep as u32);
    mfs_touch(a, false);
    if mfs_commit_object(ino, a, MFS_MAGIC_FILE).is_none() {
        return u64::MAX;
    }
    1
}

/// 把 `src` 重命名 / 移动到 `dst` (可跨目录, 必须在同一文件服务内), 成功返回 1。
///
/// 顺序刻意做成「先建新名 → 再删旧名」而不是反过来:
/// 前者中途最多是同一 inode 被两个名字引用 (无害), 后者会有一小段"inode 不可达"的
/// 窗口 —— 万一后续插入失败, 文件就真的丢了 (块会被下一次 GC 回收)。
///
/// 目标已存在时的语义: 都是文件 → 覆盖; 目标是非空目录 / 类型不匹配 → 拒绝。
/// 移动目录时拒绝把它移进自己的子孙 (会形成环)。
fn mfs_rename(src: &str, dst: &str, cred: Cred) -> u64 {
    let mut sc = [0u8; TMP_PATH_MAX];
    let mut dc = [0u8; TMP_PATH_MAX];
    let sn = match mfs_normalize(src, &mut sc) {
        Some(n) => n,
        None => return u64::MAX,
    };
    let dn = match mfs_normalize(dst, &mut dc) {
        Some(n) => n,
        None => return u64::MAX,
    };
    // 根既不能被移动也不能被覆盖。
    if sn == 1 || dn == 1 {
        return u64::MAX;
    }
    // 同一路径: 无事可做 (POSIX 里也算成功)。
    if sn == dn && sc[..sn] == dc[..dn] {
        return 1;
    }
    // 源必须存在。**不跟随末段软链接**: `mv link dst` 移动的是链接本身。
    let s_ino = match mfs_resolve_no_follow(&sc[..sn]) {
        Some(x) => x,
        None => return u64::MAX,
    };
    let sblock = match mfs_ino_block(s_ino) {
        Some(b) if b != 0 => b,
        _ => return u64::MAX,
    };
    // 类型取自节点魔数: 条目类型要与它一致, 否则改名会把软链接变成普通文件。
    let s_typ = match mfs_node_type(sblock) {
        Some(t) => t,
        None => return u64::MAX,
    };
    let s_is_dir = s_typ == MFS_TYPE_DIR;
    // 目录不能移进自己的子孙 (否则目录树成环, 解析会绕圈)。
    if s_is_dir && dn > sn && dc[..sn] == sc[..sn] && dc[sn] == b'/' {
        return u64::MAX;
    }
    // 04b: 源父目录 W+X (root 直通); sticky 时再限「节点属主 / 目录属主 / uid 0」。
    let mut ssplit = sn;
    while ssplit > 1 && sc[ssplit - 1] != b'/' {
        ssplit -= 1;
    }
    let sparent_end = if ssplit > 1 { ssplit - 1 } else { 1 };
    let sparent_ino = match mfs_resolve(&sc[..sparent_end]) {
        Some(x) => x,
        None => return mfs_err(MFS_ENOENT),
    };
    if !mfs_check_dir_ino(sparent_ino, cred, MFS_ACC_W | MFS_ACC_X) {
        return mfs_err(MFS_EACCES);
    }
    if mfs_get_mode(mfs_a(), true) & 0o1000 != 0 && cred.uid != MFS_UID_ROOT {
        let s_uid = if mfs_read_blk(sblock, mfs_s()) {
            mfs_get_uid(mfs_s(), s_is_dir)
        } else {
            MFS_UID_ROOT
        };
        if cred.uid != mfs_get_uid(mfs_a(), true) && cred.uid != s_uid {
            return mfs_err(MFS_EACCES);
        }
    }
    // 目标父路径与末分量。
    let mut split = dn;
    while split > 1 && dc[split - 1] != b'/' {
        split -= 1;
    }
    let dparent_end = if split > 1 { split - 1 } else { 1 };
    let dcomp = &dc[split..dn];
    if dcomp.is_empty() {
        return u64::MAX;
    }
    // 目标若已存在: 校验类型与空目录约束, 并在插入前删掉旧条目。
    // 同样**不跟随**: 目标是一个悬空软链接时它「已存在」, 必须按已存在处理, 否则
    // 会往目录里插出两条同名的条目。
    if let Some(d_ino) = mfs_resolve_no_follow(&dc[..dn]) {
        let dblk = match mfs_ino_block(d_ino) {
            Some(b) if b != 0 => b,
            _ => return u64::MAX,
        };
        let d_typ = match mfs_node_type(dblk) {
            Some(t) => t,
            None => return u64::MAX,
        };
        if d_typ != s_typ {
            return u64::MAX; // 类型不匹配 (文件 / 目录 / 软链接之间)
        }
        if d_typ == MFS_TYPE_DIR && mfs_dir_is_empty(d_ino) != Some(true) {
            return u64::MAX; // 目标目录非空
        }
        // 刚解析完目标, `MFS_LEAF` 就是指向它的那条条目。
        let dloc = unsafe { MFS_LEAF };
        if !mfs_dir_delete(&dloc) {
            return u64::MAX;
        }
        if !mfs_free_ino(d_ino) {
            return u64::MAX;
        }
    }
    // 1) 先在新位置建条目 (同一个 ino, 此时可能短暂存在两个名字)。
    let dparent_ino = match mfs_resolve(&dc[..dparent_end]) {
        Some(x) => x,
        None => return mfs_err(MFS_ENOENT),
    };
    // 04b: 目标父目录 W+X (root 直通)。
    if !mfs_check_dir_ino(dparent_ino, cred, MFS_ACC_W | MFS_ACC_X) {
        return mfs_err(MFS_EACCES);
    }
    if !mfs_dir_insert(dparent_ino, dcomp, s_ino, s_typ) {
        return u64::MAX;
    }
    // 2) 再删掉旧名字 (解析一次以取得条目位置, 上一步的改动不影响 ino)。
    if mfs_resolve_no_follow(&sc[..sn]).is_none() {
        return u64::MAX;
    }
    let sloc = unsafe { MFS_LEAF };
    if !mfs_dir_delete(&sloc) {
        return u64::MAX;
    }
    1
}

/// 修改 `path` 的权限位 (低 12 位), 成功返回 1。
///
/// 04b: 仅**节点属主**或 `uid 0` 可改 (否则 `EPERM`)。
fn mfs_chmod(path: &str, mode: u16, cred: Cred) -> u64 {
    let mut canon = [0u8; TMP_PATH_MAX];
    let n = match mfs_normalize(path, &mut canon) {
        Some(n) => n,
        None => return u64::MAX,
    };
    let ino = match mfs_resolve(&canon[..n]) {
        Some(x) => x,
        None => return mfs_err(MFS_ENOENT),
    };
    let block = match mfs_ino_block(ino) {
        Some(b) if b != 0 => b,
        _ => return u64::MAX,
    };
    let a = mfs_a();
    if !mfs_read_blk(block, a) {
        return mfs_err(MFS_EIO);
    }
    let (is_dir, magic) = if mfs_ok(a, MFS_MAGIC_DIR) {
        (true, MFS_MAGIC_DIR)
    } else if mfs_ok(a, MFS_MAGIC_FILE) {
        (false, MFS_MAGIC_FILE)
    } else if mfs_ok(a, MFS_MAGIC_LINK) {
        (false, MFS_MAGIC_LINK)
    } else {
        return mfs_err(MFS_EIO);
    };
    if cred.uid != MFS_UID_ROOT && cred.uid != mfs_get_uid(a, is_dir) {
        return mfs_err(MFS_EPERM);
    }
    mfs_set_mode(a, is_dir, mode);
    mfs_touch_ctime(a, is_dir);
    if mfs_commit_object(ino, a, magic).is_none() {
        return u64::MAX;
    }
    1
}

/// 修改 `path` 的属主 / 属组 (04b)。成功返回 1。
///
/// 仅 `uid 0` 可改 (最小实现; 演进项: 属主可把自己文件的 gid 改到所属组)。
fn mfs_chown(path: &str, uid: u16, gid: u16, cred: Cred) -> u64 {
    let mut canon = [0u8; TMP_PATH_MAX];
    let n = match mfs_normalize(path, &mut canon) {
        Some(n) => n,
        None => return u64::MAX,
    };
    let ino = match mfs_resolve(&canon[..n]) {
        Some(x) => x,
        None => return mfs_err(MFS_ENOENT),
    };
    let block = match mfs_ino_block(ino) {
        Some(b) if b != 0 => b,
        _ => return u64::MAX,
    };
    let a = mfs_a();
    if !mfs_read_blk(block, a) {
        return mfs_err(MFS_EIO);
    }
    let (is_dir, magic) = if mfs_ok(a, MFS_MAGIC_DIR) {
        (true, MFS_MAGIC_DIR)
    } else if mfs_ok(a, MFS_MAGIC_FILE) {
        (false, MFS_MAGIC_FILE)
    } else if mfs_ok(a, MFS_MAGIC_LINK) {
        (false, MFS_MAGIC_LINK)
    } else {
        return mfs_err(MFS_EIO);
    };
    if cred.uid != MFS_UID_ROOT {
        return mfs_err(MFS_EPERM);
    }
    mfs_set_uid(a, is_dir, uid);
    mfs_set_gid(a, is_dir, gid);
    mfs_touch_ctime(a, is_dir);
    if mfs_commit_object(ino, a, magic).is_none() {
        return u64::MAX;
    }
    1
}

/// 把一个目录块里的所有有效条目写成 `DirEntry`, 累加到 `count` (上限 `RESULT_MAX_ENTRIES`)。
///
/// 读子节点 inode 用 S 缓冲, 以免覆盖 A(节点)/B(索引)/C(扩展块) 三块目录状态。
fn mfs_dir_emit(buf: *const u8, out: *mut vfs::DirEntry, count: &mut usize) {
    let end = MFS_HDR + MFS_PAYLOAD;
    let mut off = MFS_HDR + MFS_DIR_HDR;
    while off + MFS_DIR_ENT_HDR <= end {
        if *count >= vfs::RESULT_MAX_ENTRIES {
            return;
        }
        let nl = mfs_ent_name_len(buf, off);
        if nl != 0 {
            let typ = mfs_ent_type(buf, off);
            let is_dir = typ == MFS_TYPE_DIR;
            let mut de = vfs::DirEntry::short([0u8; 11], 0, u32::from(is_dir));
            let name =
                unsafe { core::slice::from_raw_parts(mfs_at(buf, off + MFS_DIR_ENT_HDR), nl) };
            mfs_name_to_fat(name, &mut de.name);
            // MFS 名字直接以字节串存储, 原样回传 (截到 IPC 可达长度)。
            let ln = nl.min(vfs::DIR_LONG_MAX);
            de.long[..ln].copy_from_slice(&name[..ln]);
            de.long_len = ln as u8;
            // 读子节点 inode 补齐元数据 (用 S 缓冲, 不碰 A/B/C 的目录状态);
            // `ls -l` 因此只需一次 readdir, 不必逐条目 stat。条目存的是 ino,
            // 故先经 inode 表翻译成块号 (表块走单条目缓存, 连续 ino 只读一次)。
            let s = mfs_s();
            let child = mfs_ino_block(mfs_ent_ino(buf, off)).unwrap_or_default();
            if child != 0 && mfs_read_blk(child, s) {
                // 软链接也带出 size (= 目标路径长度, 同 `lstat`); 类型靠 `mode` 高位区分。
                if !is_dir && (mfs_ok(s, MFS_MAGIC_FILE) || mfs_ok(s, MFS_MAGIC_LINK)) {
                    de.size = mfs_file_size(s);
                }
                de.mode = mfs_get_mode(s, is_dir);
                de.owner = mfs_get_owner(s, is_dir);
                de.uid = mfs_get_uid(s, is_dir);
                de.gid = mfs_get_gid(s, is_dir);
                de.nlink = mfs_get_nlink(s, is_dir);
                de.mtime = mfs_get_mtime(s, is_dir);
            }
            unsafe {
                core::ptr::write_unaligned(out.add(*count), de);
            }
            *count += 1;
        }
        off = match mfs_ent_step(buf, off) {
            Some(n) => n,
            None => return,
        };
    }
}

/// 列出目录 `ino` 的条目, 写入 `out` (DirEntry 数组), 返回写入字节数。
///
/// 目录条目可能散在目录节点块与若干扩展块里 (M4), 按「节点块 → 索引块槽位顺序」遍历。
fn mfs_readdir(ino: u32, out: *mut vfs::DirEntry) -> u64 {
    let a = mfs_a();
    let block = match mfs_ino_block(ino) {
        Some(b) if b != 0 => b,
        _ => return u64::MAX,
    };
    if !mfs_read_blk(block, a) || !mfs_ok(a, MFS_MAGIC_DIR) {
        return u64::MAX;
    }
    // 结果页只有一页: 放不下就停在已写入的条目上 (mfs_dir_emit 内部即按上限截断)。
    let mut count = 0usize;
    mfs_dir_emit(a, out, &mut count);
    let ext = mfs_dir_ext(a);
    if ext != 0 && count < vfs::RESULT_MAX_ENTRIES {
        let b = mfs_b();
        if mfs_read_blk(ext, b) && mfs_ok(b, MFS_MAGIC_DIDX) {
            let c = mfs_c();
            for i in 0..MFS_DIR_SLOTS {
                if count >= vfs::RESULT_MAX_ENTRIES {
                    break;
                }
                let blk = read_u32(mfs_at(b, MFS_HDR + i * 4));
                if blk == 0 {
                    continue;
                }
                if !mfs_read_blk(blk, c) || !mfs_ok(c, MFS_MAGIC_DIR) {
                    continue;
                }
                mfs_dir_emit(c, out, &mut count);
            }
        }
    }
    (count * core::mem::size_of::<vfs::DirEntry>()) as u64
}

// ---------------------------------------------------------------------------
// fd 表 (按路径)
// ---------------------------------------------------------------------------

fn mfs_fd_alloc(path: &[u8], is_dir: bool, cred: Cred, perm: u8) -> u64 {
    for i in 0..MFS_MAX_FD {
        unsafe {
            let s = &mut *core::ptr::addr_of_mut!(MFS_FDS).cast::<MfsFd>().add(i);
            if !s.used {
                s.used = true;
                s.is_dir = is_dir;
                s.cred = cred;
                s.perm = perm;
                s.path_len = path.len() as u8;
                // 绑定分配时所在的卷: 之后这个 fd 上的请求可能落在别的卷被处理
                // (服务循环按请求切卷), 靠它把请求拉回本 fd 所属的那一卷。
                s.vol = MFS_CUR_VOL;
                s.path = [0; TMP_PATH_MAX];
                s.path[..path.len()].copy_from_slice(path);
                return i as u64;
            }
        }
    }
    u64::MAX
}
fn mfs_fd_get(fd: u32) -> Option<MfsFd> {
    if fd as usize >= MFS_MAX_FD {
        return None;
    }
    unsafe {
        let s = &*core::ptr::addr_of!(MFS_FDS)
            .cast::<MfsFd>()
            .add(fd as usize);
        if s.used {
            Some(*s)
        } else {
            None
        }
    }
}
fn mfs_fd_free(fd: u32) -> u64 {
    if fd as usize >= MFS_MAX_FD {
        return 0;
    }
    unsafe {
        let s = &mut *core::ptr::addr_of_mut!(MFS_FDS)
            .cast::<MfsFd>()
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
// 权限与多用户 (04b) —— 能力在前, 权限在后
// ---------------------------------------------------------------------------

/// 失败回复编码: 保留**高 16 位全 1** 的波段表示错误 (fd 把能力句柄放最高 16 位,
/// 但句柄索引 < 32, 永不落在本段; 字节数/条目数等成功值也远小于此)。
const MFS_ERR_BASE: u64 = 0xFFFF_FFFF_FFFF_0000;
const MFS_EPERM: u64 = 1;
const MFS_EACCES: u64 = 2;
const MFS_ENOENT: u64 = 3;
const MFS_EEXIST: u64 = 4;
const MFS_ENOTDIR: u64 = 5;
const MFS_EISDIR: u64 = 6;
const MFS_ENOTEMPTY: u64 = 7;
const MFS_EINVAL: u64 = 8;
const MFS_ENOSPC: u64 = 10;
const MFS_EIO: u64 = 11;

fn mfs_err(code: u64) -> u64 {
    MFS_ERR_BASE | (code & 0xFFFF)
}
/// `u64::MAX` (旧通用失败) 天然并入: `mfs_is_err(u64::MAX) == true`。
fn mfs_is_err(v: u64) -> bool {
    (v >> 48) == 0xFFFF
}

/// 主体身份 (`04b` 静态凭证表)。uid/gid 与落盘字段同宽 (u16)。
#[derive(Clone, Copy)]
struct Cred {
    uid: u16,
    gid: u16,
}

/// 引导期长期服务域数 (与内核 `kernel/src/domain.rs` 的 `BOOT_DOMAINS` 对齐)。
///
/// 域 id `< 本值` = 引导期服务域 => 系统身份 `0:0`; `>= 本值` = 运行期新建域 =>
/// 低权身份 `1000:1000`。**域号会复用**, 但本映射是按域号**现算**的确定性函数
/// (不缓存用户可选身份), 复用后仍是同一档身份, 不存在"继承旧身份"问题。
/// 目标形态改由认证服务签发 `Cred` 时, 才需要随域销毁失效的凭证表。
const MFS_BOOT_DOMAINS: u64 = 23;
const MFS_UID_ROOT: u16 = 0;
const MFS_GID_ROOT: u16 = 0;
const MFS_UID_USER: u16 = 1000;
const MFS_GID_USER: u16 = 1000;

fn mfs_cred_of(domain: u64) -> Cred {
    if domain < MFS_BOOT_DOMAINS {
        Cred {
            uid: MFS_UID_ROOT,
            gid: MFS_GID_ROOT,
        }
    } else {
        Cred {
            uid: MFS_UID_USER,
            gid: MFS_GID_USER,
        }
    }
}

/// 访问类型位 (与 rwx 位同序)。
const MFS_ACC_R: u8 = 0o4;
const MFS_ACC_W: u8 = 0o2;
const MFS_ACC_X: u8 = 0o1;

/// `cred` 对节点 (块已在缓冲中) 的**有效权限位**。`uid 0` => 全放行 (`0o7`)。
fn mfs_effective_perm(buf: *const u8, is_dir: bool, cred: Cred) -> u8 {
    if cred.uid == MFS_UID_ROOT {
        return 0o7;
    }
    let mode = mfs_get_mode(buf, is_dir) & 0o777;
    let shift = if cred.uid == mfs_get_uid(buf, is_dir) {
        6
    } else if cred.gid == mfs_get_gid(buf, is_dir) {
        3
    } else {
        0
    };
    ((mode >> shift) & 0o7) as u8
}

/// `cred` 是否具备 `want` (R/W/X 位组合)。
fn mfs_check_access(buf: *const u8, is_dir: bool, cred: Cred, want: u8) -> bool {
    mfs_effective_perm(buf, is_dir, cred) & want == want
}

/// 目录 `ino` 的节点块读进 A 缓冲, 返回 `(is_dir=true, ok)`。仅用于目录类检查。
fn mfs_load_dir(ino: u32) -> bool {
    match mfs_ino_block(ino) {
        Some(b) if b != 0 => mfs_read_blk(b, mfs_a()) && mfs_ok(mfs_a(), MFS_MAGIC_DIR),
        _ => false,
    }
}

/// 对**父目录 inode** 做 `want` 检查 (路径类操作的公共入口)。父目录读失败即拒绝。
fn mfs_check_dir_ino(dir_ino: u32, cred: Cred, want: u8) -> bool {
    mfs_load_dir(dir_ino) && mfs_check_access(mfs_a(), true, cred, want)
}

/// 路径可达性 (STAT / LSTAT / READLINK): 末段的父目录需具备 X (穿越)。root 直通。
fn mfs_check_traverse(canon: &[u8], cred: Cred) -> bool {
    if cred.uid == MFS_UID_ROOT {
        return true;
    }
    let mut split = canon.len();
    while split > 1 && canon[split - 1] != b'/' {
        split -= 1;
    }
    let parent_end = if split > 1 { split - 1 } else { 1 };
    match mfs_resolve(&canon[..parent_end]) {
        Some(p) => mfs_check_dir_ino(p, cred, MFS_ACC_X),
        None => false,
    }
}

// ---------------------------------------------------------------------------
// 服务循环
// ---------------------------------------------------------------------------

/// 域 11 — MFS 服务: 处理 VFS 协议 + MFS 快照 / 空间回收操作。
pub fn run() {
    // 先为块缓冲分配页 (固定虚拟地址, 避开程序镜像 / 用户栈 / 数据区),
    // 再把它们共享给 block_srv (它按 req.buf 写入读到的扇区), 之后才能做块 I/O。
    if sys_alloc_page(mfs_a() as u64) != 1
        || sys_alloc_page(mfs_b() as u64) != 1
        || sys_alloc_page(mfs_c() as u64) != 1
        || sys_alloc_page(mfs_s() as u64) != 1
        || sys_alloc_page(mfs_gc_buf() as u64) != 1
        || sys_alloc_page(mfs_itab_buf() as u64) != 1
        || sys_alloc_page(mfs_itabx_buf() as u64) != 1
        || sys_alloc_page(mfs_gc_tab_buf() as u64) != 1
        || sys_alloc_page(mfs_bmph_buf() as u64) != 1
    {
        println("mfs: alloc block buffers FAILED");
        return;
    }
    // 这些页都要「同地址」共享给 block_srv: 它按我们给的地址做 DMA 写入, 未共享的
    // 地址在目标域里无效 -> NVMe 命令会直接被判为非法字段。
    if sys_share_page(mfs_a() as u64, BLOCK_DOMAIN) != 1
        || sys_share_page(mfs_b() as u64, BLOCK_DOMAIN) != 1
        || sys_share_page(mfs_c() as u64, BLOCK_DOMAIN) != 1
        || sys_share_page(mfs_s() as u64, BLOCK_DOMAIN) != 1
        || sys_share_page(mfs_gc_buf() as u64, BLOCK_DOMAIN) != 1
        || sys_share_page(mfs_itab_buf() as u64, BLOCK_DOMAIN) != 1
        || sys_share_page(mfs_itabx_buf() as u64, BLOCK_DOMAIN) != 1
        || sys_share_page(mfs_gc_tab_buf() as u64, BLOCK_DOMAIN) != 1
        || sys_share_page(mfs_bmph_buf() as u64, BLOCK_DOMAIN) != 1
    {
        println("mfs: share block buffers FAILED");
        return;
    }
    // 02b-2 批 I/O 缓冲窗: 读窗 + 读描述符 + 写暂存窗 + 写描述符, 全部同址共享给 block_srv。
    let mut p = 0usize;
    while p < MFS_RDBUF_PAGES {
        if sys_alloc_page(mfs_rdbuf(p) as u64) != 1 || sys_alloc_page(mfs_wbuf(p) as u64) != 1 {
            println("mfs: alloc batch windows FAILED");
            return;
        }
        p += 1;
    }
    if sys_alloc_page(MFS_RDBUF_DESC_VADDR) != 1 || sys_alloc_page(MFS_WB_DESC_VADDR) != 1 {
        println("mfs: alloc batch descriptors FAILED");
        return;
    }
    p = 0;
    while p < MFS_RDBUF_PAGES {
        if sys_share_page(mfs_rdbuf(p) as u64, BLOCK_DOMAIN) != 1
            || sys_share_page(mfs_wbuf(p) as u64, BLOCK_DOMAIN) != 1
        {
            println("mfs: share batch windows FAILED");
            return;
        }
        p += 1;
    }
    if sys_share_page(MFS_RDBUF_DESC_VADDR, BLOCK_DOMAIN) != 1
        || sys_share_page(MFS_WB_DESC_VADDR, BLOCK_DOMAIN) != 1
    {
        println("mfs: share batch descriptors FAILED");
        return;
    }
    // 认领卷: 优先「主卷序号最大」的 MFS 卷 (= 最近一次 `mkfs.mfs` 过的那块), 其次
    // 第一个 MFS 卷; 空白盘没有 magic, 回退到约定卷号 1 (见 `mfs_vol_claim`)。
    unsafe {
        MFS_VOL = mfs_vol_claim(mfs_a(), 16);
        // 卷容量 (扇区数): 首次格式化按它决定文件系统大小; 也用于校验盘上记录的总
        // 块数没超出卷的实际容量。0 = 未知 (IDE 回退等), 上层按默认值兜底。
        MFS_VOL_SECTORS = vol_sectors(mfs_a(), MFS_VOL);
        // 服务起点就是主卷: 之后只有请求明确要求别的卷时才切。
        MFS_CUR_VOL = MFS_VOL;
        MFS_CUR_SECTORS = MFS_VOL_SECTORS;
    }
    // 位图窗口必须在**挂载前**铺开 (挂载路径要直接拿窗口页作位图 I/O 缓冲)。此刻盘上
    // 的 bb 还读不到, 故按**卷容量**预算上界: 每 32768 块占一个 4 KiB 位图数据块,
    // 即窗口页数 = ceil(卷块数 / 32768)。只增不缩: 之后切到更大卷由 `mfs_win_ensure` 补页。
    {
        let secs = unsafe { MFS_CUR_SECTORS } as u64;
        let vol_blocks = if secs == 0 {
            MFS_DEFAULT_TOTAL_BLOCKS as u64
        } else {
            secs / MFS_SECTORS_PER_BLOCK as u64
        };
        let blocks = vol_blocks.clamp(MFS_MIN_TOTAL_BLOCKS as u64, MFS_MAX_BLOCKS as u64);
        if !mfs_win_ensure(mfs_bb_for(blocks as u32)) {
            println("mfs: alloc bitmap windows FAILED");
            return;
        }
    }
    // 安全护栏: 只允许挂载「已是 MFS」或「整盘无文件系统 (UNKNOWN, 需格式化)」的卷。
    // 卷号回退一旦算错 (例如接了真 U 盘、换了镜像布局), 自动格式化会把别人的分区
    // 直接写掉 —— 这里宁可让服务不挂载 (上层会看到 FAILED), 也绝不动非 MFS 卷。
    let kind = vol_kind_of(mfs_a(), unsafe { MFS_VOL });
    if kind != VOL_KIND_UNKNOWN && kind != VOL_KIND_MFS {
        print("mfs: refuse to format non-MFS volume vol=");
        print_u64(unsafe { MFS_VOL });
        print(" kind=");
        print_u64(kind as u64);
        println("");
        return;
    }
    if !mfs_mount_or_format() {
        println("mfs: mount/format FAILED");
        return;
    }
    print("mfs-dbg: vol=");
    print_u64(unsafe { MFS_VOL });
    print(" total=");
    print_u64(unsafe { MFS_TOTAL_BLOCKS } as u64);
    print(" free=");
    print_u64(unsafe { MFS_FREE_BLOCKS } as u64);
    print(" gen=");
    print_u64(unsafe { MFS_GEN });
    print(" snap=");
    print_u64(unsafe { MFS_SNAP_COUNT } as u64);
    // 卷容量 (扇区数): `total * 8` 应等于它 —— 不等说明文件系统没铺满卷 (或卷被换过)。
    print(" volsec=");
    print_u64(unsafe { MFS_VOL_SECTORS } as u64);
    println("");

    // M1b: 把**额外**的 MFS 卷 (真盘上可以有多块) 挂到 `/usb<卷号>`。用 A 页暂存卷
    // 描述符 —— 超级块已解析完毕, 该页此刻只是块缓冲, 内容不留用。
    // 只认卷层探测为 MFS 的卷: 空白的额外卷不会被自动格式化 (要显式 `mkfs.mfs`)。
    mount_extra_volumes(mfs_a(), VOL_KIND_MFS, unsafe { MFS_VOL }, vfs::MFS_DOMAIN);

    let mut msg = Message {
        from: 0,
        to: 0,
        tag: 0,
        payload: [0; PAYLOAD_LEN],
    };
    let mut canon = [0u8; TMP_PATH_MAX];
    loop {
        sys_recv_msg(&mut msg as *mut Message as *mut u8);
        // 请求间隙无在建 COW, 是唯一安全的回收时机: 空闲块偏少就先整理一次。
        mfs_maybe_gc();
        // 04b: 发起者身份一律由服务侧按 `msg.from` (内核在 IPC 层保证) 查表得出,
        // 请求消息里**不携带** uid/gid —— 消息里的数字可伪造, 域号不可。
        let cred = mfs_cred_of(msg.from);
        let tag = vfs::tag_body(msg.tag);
        // 卷编码 (tag 高位, M1b) 决定路径类请求落在哪个卷; fd 类请求的卷由 fd 自己
        // 绑定 (fd 是这些请求 payload 的首字段), 先探一次 fd, 使两类请求都对。
        // 关 fd 不动卷 (只改内存里的 fd 表), 故不参与。
        let mut vol = vfs::vol_from_enc(vfs::tag_vol(msg.tag), unsafe { MFS_VOL });
        if matches!(
            tag,
            vfs::VFS_READ_TAG | vfs::VFS_WRITE_TAG | vfs::VFS_READDIR_TAG | vfs::VFS_TRUNCATE_TAG
        ) {
            if let Some(fd) = mfs_fd_get(read_u32(msg.payload.as_ptr())) {
                vol = fd.vol;
            }
        }
        // 换卷: 内存态只有一份, 必须先把新卷的超级块载回来 (位图 / inode 表 / 快照)。
        // 载不回来就放弃本次请求 —— 继续用旧卷的位图去写新卷会毁数据。
        if vol != unsafe { MFS_CUR_VOL } && !mfs_switch_vol(vol) {
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
                let fd = match mfs_normalize(path, &mut canon) {
                    Some(n) => match mfs_resolve(&canon[..n]).and_then(mfs_ino_block) {
                        Some(blk) if blk != 0 => {
                            let a = mfs_a();
                            if !mfs_read_blk(blk, a) {
                                mfs_err(MFS_EIO)
                            } else {
                                let is_dir = mfs_ok(a, MFS_MAGIC_DIR);
                                // 04b: 打开时判一次 (Unix 语义), 结果随 fd 快照。
                                // 文件需 R; 目录需 R+X (读条目列表 + 穿越)。
                                let want = if is_dir {
                                    MFS_ACC_R | MFS_ACC_X
                                } else {
                                    MFS_ACC_R
                                };
                                if !mfs_check_access(a, is_dir, cred, want) {
                                    mfs_err(MFS_EACCES)
                                } else {
                                    let perm = mfs_effective_perm(a, is_dir, cred);
                                    mfs_fd_alloc(&canon[..n], is_dir, cred, perm)
                                }
                            }
                        }
                        _ => mfs_err(MFS_ENOENT),
                    },
                    None => mfs_err(MFS_EINVAL),
                };
                sys_reply(fd);
            }
            vfs::VFS_READ_TAG => {
                let req: vfs::ReadReq = unsafe {
                    core::ptr::read_unaligned(msg.payload.as_ptr() as *const vfs::ReadReq)
                };
                let n = match mfs_fd_get(req.fd) {
                    Some(fd) if !fd.is_dir => {
                        if fd.perm & MFS_ACC_R == 0 {
                            mfs_err(MFS_EACCES)
                        } else {
                            let p = &fd.path[..fd.path_len as usize];
                            match mfs_resolve(p) {
                                Some(ino) => {
                                    mfs_read_file(ino, req.offset, req.count, req.buf as *mut u8)
                                }
                                None => mfs_err(MFS_ENOENT),
                            }
                        }
                    }
                    Some(_) => mfs_err(MFS_EISDIR),
                    None => mfs_err(MFS_EINVAL),
                };
                sys_reply(n);
            }
            vfs::VFS_WRITE_TAG => {
                let req: vfs::WriteReq = unsafe {
                    core::ptr::read_unaligned(msg.payload.as_ptr() as *const vfs::WriteReq)
                };
                let n = match mfs_fd_get(req.fd) {
                    Some(fd) if !fd.is_dir => {
                        if fd.perm & MFS_ACC_W == 0 {
                            mfs_err(MFS_EACCES)
                        } else {
                            // 复制路径 (解析会复用 canon/缓冲, 避免借用冲突)。
                            let mut p = [0u8; TMP_PATH_MAX];
                            let plen = fd.path_len as usize;
                            p[..plen].copy_from_slice(&fd.path[..plen]);
                            match mfs_resolve(&p[..plen]) {
                                Some(ino) => {
                                    mfs_write_file(ino, req.offset, req.count, req.buf as *const u8)
                                }
                                None => mfs_err(MFS_ENOENT),
                            }
                        }
                    }
                    Some(_) => mfs_err(MFS_EISDIR),
                    None => mfs_err(MFS_EINVAL),
                };
                sys_reply(n);
            }
            vfs::VFS_READDIR_TAG => {
                let req: vfs::DirReq = unsafe {
                    core::ptr::read_unaligned(msg.payload.as_ptr() as *const vfs::DirReq)
                };
                let n = match mfs_fd_get(req.fd) {
                    Some(fd) if fd.is_dir => {
                        if fd.perm & (MFS_ACC_R | MFS_ACC_X) != (MFS_ACC_R | MFS_ACC_X) {
                            mfs_err(MFS_EACCES)
                        } else {
                            let mut p = [0u8; TMP_PATH_MAX];
                            let plen = fd.path_len as usize;
                            p[..plen].copy_from_slice(&fd.path[..plen]);
                            match mfs_resolve(&p[..plen]) {
                                Some(ino) => mfs_readdir(ino, req.buf as *mut vfs::DirEntry),
                                None => mfs_err(MFS_ENOENT),
                            }
                        }
                    }
                    Some(_) => mfs_err(MFS_ENOTDIR),
                    None => mfs_err(MFS_EINVAL),
                };
                sys_reply(n);
            }
            vfs::VFS_CLOSE_TAG => {
                let fd = read_u32(msg.payload.as_ptr());
                sys_reply(mfs_fd_free(fd));
            }
            vfs::VFS_CREAT_TAG | vfs::VFS_MKDIR_TAG => {
                let is_dir = tag == vfs::VFS_MKDIR_TAG;
                let len = msg
                    .payload
                    .iter()
                    .position(|&b| b == 0)
                    .unwrap_or(PAYLOAD_LEN);
                let path = unsafe { core::str::from_utf8_unchecked(&msg.payload[..len]) };
                // 创建者域 id 作为 owner 记进元数据 (fire-and-forget 诊断用)。
                let fd = mfs_create(path, is_dir, msg.from as u16, cred);
                sys_reply(fd);
            }
            vfs::VFS_UNLINK_TAG | vfs::VFS_RMDIR_TAG => {
                let want_dir = tag == vfs::VFS_RMDIR_TAG;
                let len = msg
                    .payload
                    .iter()
                    .position(|&b| b == 0)
                    .unwrap_or(PAYLOAD_LEN);
                let path = unsafe { core::str::from_utf8_unchecked(&msg.payload[..len]) };
                sys_reply(mfs_remove(path, want_dir, cred));
            }
            vfs::VFS_TRUNCATE_TAG => {
                let req: vfs::TruncateReq = unsafe {
                    core::ptr::read_unaligned(msg.payload.as_ptr() as *const vfs::TruncateReq)
                };
                let n = match mfs_fd_get(req.fd) {
                    Some(fd) if !fd.is_dir => {
                        if fd.perm & MFS_ACC_W == 0 {
                            mfs_err(MFS_EACCES)
                        } else {
                            let mut p = [0u8; TMP_PATH_MAX];
                            let plen = fd.path_len as usize;
                            p[..plen].copy_from_slice(&fd.path[..plen]);
                            match mfs_resolve(&p[..plen]) {
                                Some(ino) => mfs_truncate(ino, req.size),
                                None => mfs_err(MFS_ENOENT),
                            }
                        }
                    }
                    Some(_) => mfs_err(MFS_EISDIR),
                    None => mfs_err(MFS_EINVAL),
                };
                sys_reply(n);
            }
            vfs::VFS_RENAME_TAG => {
                sys_reply(with_two_paths(msg.payload.as_ptr(), |a, b| {
                    mfs_rename(a, b, cred)
                }));
            }
            vfs::VFS_LINK_TAG => {
                sys_reply(with_two_paths(msg.payload.as_ptr(), |a, b| {
                    mfs_link(a, b, cred)
                }));
            }
            // 软链接 (M5c): 两条路径 = (目标, 链接自身)。目标的**原样**存储, 故只有
            // 链接自身的路径经挂载层路由 (见 `vfs::symlink_into`)。
            vfs::VFS_SYMLINK_TAG => {
                let owner = msg.from as u16;
                sys_reply(with_two_paths(msg.payload.as_ptr(), |t, l| {
                    mfs_symlink(t, l, owner, cred)
                }));
            }
            vfs::VFS_CHMOD_TAG => {
                let req: vfs::PathReq = unsafe {
                    core::ptr::read_unaligned(msg.payload.as_ptr() as *const vfs::PathReq)
                };
                let path = unsafe { core::str::from_utf8_unchecked(page_path(req.buf)) };
                sys_reply(mfs_chmod(path, req.aux as u16, cred));
            }
            // 改属主 / 属组 (04b): 路径与 `PathReq` 同款; `aux` = `uid << 16 | gid`。
            vfs::MFS_CHOWN_TAG => {
                let req: vfs::PathReq = unsafe {
                    core::ptr::read_unaligned(msg.payload.as_ptr() as *const vfs::PathReq)
                };
                let path = unsafe { core::str::from_utf8_unchecked(page_path(req.buf)) };
                let uid = (req.aux >> 16) as u16;
                let gid = (req.aux & 0xFFFF) as u16;
                sys_reply(mfs_chown(path, uid, gid, cred));
            }
            vfs::VFS_STAT_TAG => {
                let (buf, path) = parse_path_req(msg.payload.as_ptr());
                let n = match mfs_normalize(path, &mut canon) {
                    Some(nn) => match mfs_resolve(&canon[..nn]) {
                        Some(ino) if mfs_check_traverse(&canon[..nn], cred) => {
                            let blk = mfs_ino_block(ino).unwrap_or_default();
                            mfs_stat_into(blk, buf)
                        }
                        Some(_) => mfs_err(MFS_EACCES),
                        None => mfs_err(MFS_ENOENT),
                    },
                    None => mfs_err(MFS_EINVAL),
                };
                sys_reply(n);
            }
            // 软链接配套 (M5c): 读**链接自身**的目标串 (不跟随)。
            vfs::VFS_READLINK_TAG => {
                let (buf, path) = parse_path_req(msg.payload.as_ptr());
                let n = match mfs_normalize(path, &mut canon) {
                    Some(nn) if mfs_check_traverse(&canon[..nn], cred) => {
                        mfs_readlink(&canon[..nn], buf)
                    }
                    Some(_) => mfs_err(MFS_EACCES),
                    None => mfs_err(MFS_EINVAL),
                };
                sys_reply(n);
            }
            // 取**链接自身**的元数据 (不跟随末段): 与 STAT 只差「末段是否跟随」。
            vfs::VFS_LSTAT_TAG => {
                let (buf, path) = parse_path_req(msg.payload.as_ptr());
                let n = match mfs_normalize(path, &mut canon) {
                    Some(nn) => match mfs_resolve_no_follow(&canon[..nn]) {
                        Some(ino) if mfs_check_traverse(&canon[..nn], cred) => {
                            mfs_stat_into(mfs_ino_block(ino).unwrap_or_default(), buf)
                        }
                        Some(_) => mfs_err(MFS_EACCES),
                        None => mfs_err(MFS_ENOENT),
                    },
                    None => mfs_err(MFS_EINVAL),
                };
                sys_reply(n);
            }
            vfs::MFS_SNAP_TAG => {
                // 快照表是**环形**: 满 (MFS_MAX_SNAP) 时先淘汰最旧一条, 为新快照腾位,
                // 而不是直接失败 —— 表跨启动持久化在超级块里, 否则第 9 次起就再也建不出
                // 快照。被淘汰快照指向的旧块仍由 COW 永久保留, 只是不再有快照记录引用;
                // 淘汰后所有快照索引整体前移一位, 旧的索引随即失效。
                unsafe {
                    if MFS_SNAP_COUNT >= MFS_MAX_SNAP {
                        for i in 1..MFS_MAX_SNAP {
                            mfs_set_snap(i - 1, mfs_snap(i));
                        }
                        MFS_SNAP_COUNT = MFS_MAX_SNAP - 1;
                    }
                }
                let idx = unsafe { MFS_SNAP_COUNT };
                mfs_set_snap(
                    idx,
                    MfsSnap {
                        gen: unsafe { MFS_GEN },
                        itab: unsafe { MFS_ITAB },
                        ino_hint: unsafe { MFS_INO_HINT },
                        alloc_next: unsafe { MFS_ALLOC_NEXT },
                    },
                );
                unsafe {
                    MFS_SNAP_COUNT = idx + 1;
                }
                let r = if mfs_bmp_flush() {
                    idx as u64
                } else {
                    u64::MAX
                };
                sys_reply(r);
            }
            vfs::MFS_SNAPLIST_TAG => {
                let buf = read_u64(msg.payload.as_ptr()) as *mut u8;
                let n = unsafe { MFS_SNAP_COUNT };
                for i in 0..n {
                    let s = mfs_snap(i);
                    let dst = unsafe { buf.add(i * vfs::SNAP_REC_LEN) };
                    write_u64(dst, s.gen);
                    write_u32(unsafe { dst.add(8) }, s.itab);
                    write_u32(unsafe { dst.add(12) }, s.ino_hint);
                    write_u32(unsafe { dst.add(16) }, s.alloc_next);
                }
                sys_reply((n * vfs::SNAP_REC_LEN) as u64);
            }
            vfs::MFS_SNAPRESTORE_TAG => {
                let idx = read_u32(msg.payload.as_ptr()) as usize;
                let r = if idx < unsafe { MFS_SNAP_COUNT } {
                    let s = mfs_snap(idx);
                    unsafe {
                        MFS_ITAB = s.itab;
                        MFS_INO_HINT = s.ino_hint;
                        MFS_ALLOC_NEXT = s.alloc_next;
                        MFS_GEN = s.gen;
                    }
                    // 索引镜像必须跟着换成快照那一版, 否则 ino 会翻译到回滚后的对象上。
                    if !mfs_itab_reload() {
                        u64::MAX
                    } else if mfs_bmp_flush() {
                        1
                    } else {
                        u64::MAX
                    }
                } else {
                    u64::MAX
                };
                sys_reply(r);
            }
            vfs::MFS_GC_TAG => {
                let freed = mfs_gc();
                if freed == u64::MAX {
                    println("mfs: gc FAILED");
                    sys_reply(u64::MAX);
                } else {
                    sys_reply(freed);
                }
            }
            vfs::MFS_STAT_TAG => {
                let total = unsafe { MFS_TOTAL_BLOCKS } as u64;
                let free = unsafe { MFS_FREE_BLOCKS } as u64;
                sys_reply((total << 32) | free);
            }
            // 最小 fsck (01): 默认只报不修, payload 标志字 bit0 = 修复。
            vfs::MFS_FSCK_TAG => {
                let repair = read_u64(msg.payload.as_ptr()) & 1 != 0;
                sys_reply(mfs_fsck(repair));
            }
            // 显式 sync (01): 幂等落盘一次, 回复落盘后的代际 gen。
            vfs::MFS_SYNC_TAG => {
                let r = if mfs_bmp_flush() {
                    unsafe { MFS_GEN }
                } else {
                    u64::MAX
                };
                sys_reply(r);
            }
            // 显式格式化入口 (S2): 在指定卷上建 MFS。按卷号寻址, 不经挂载路由。
            // payload = 卷号 (偏移 +0) ++ 标志字 (偏移 +8); 只发 8 字节的老调用方那里是 0。
            vfs::VFS_MKFS_TAG => {
                let vol = read_u64(msg.payload.as_ptr());
                let flags = read_u64(unsafe { msg.payload.as_ptr().add(8) });
                sys_reply(mfs_mkfs_volume(vol, flags));
            }
            // 换主卷 (S2 补齐): 只改超级块里的主卷序号, 不动卷上的数据。
            vfs::MFS_SETPRIMARY_TAG => {
                let vol = read_u64(msg.payload.as_ptr());
                sys_reply(mfs_set_primary_volume(vol));
            }
            _ => {
                sys_reply(u64::MAX);
            }
        }
    }
}

/// 读取 `block` 是否为目录节点。
fn mfs_is_dir(block: u32) -> bool {
    let a = mfs_a();
    mfs_read_blk(block, a) && mfs_ok(a, MFS_MAGIC_DIR)
}

/// 读节点块, 按魔数返回它的类型 (`MFS_TYPE_*`); 魔数不认识时返回 None。
fn mfs_node_type(block: u32) -> Option<u32> {
    let a = mfs_a();
    if !mfs_read_blk(block, a) {
        return None;
    }
    if mfs_ok(a, MFS_MAGIC_DIR) {
        Some(MFS_TYPE_DIR)
    } else if mfs_ok(a, MFS_MAGIC_FILE) {
        Some(MFS_TYPE_FILE)
    } else if mfs_ok(a, MFS_MAGIC_LINK) {
        Some(MFS_TYPE_LINK)
    } else {
        None
    }
}

/// 读软链接 `path` **自身**的目标串到共享页 `buf`, 返回字节数 (不含 NUL)。
///
/// **不跟随**: 只对「末段是软链接」的路径有效, 普通文件 / 目录一律失败 (与 `readlink(2)`
/// 一致 —— 它不会跟着链接往下走)。目标串是**服务命名空间**里的路径, 形如 `/a`;
/// 把它还原成用户命名空间 (`/mfs/a`) 是客户端的事 (见 `vfs::readlink_into`)。
fn mfs_readlink(canon: &[u8], buf: u64) -> u64 {
    let ino = match mfs_resolve_no_follow(canon) {
        Some(x) => x,
        None => return u64::MAX,
    };
    // 类型用**条目**判断 (与 rm/mv 同口径): 刚解析完, `MFS_LEAF` 指向它的那条条目。
    let loc = unsafe { MFS_LEAF };
    if loc.dir_ino == 0 || !mfs_dir_load_loc(&loc) {
        return u64::MAX;
    }
    if mfs_ent_type(mfs_c(), loc.off) != MFS_TYPE_LINK {
        return u64::MAX;
    }
    let mut tmp = [0u8; MFS_LINK_MAX];
    let n = match mfs_link_target(ino, &mut tmp) {
        Some(n) => n,
        None => return u64::MAX,
    };
    // 单条 IPC 能回的路径上限; 创建时已受限, 这里兜底。
    let n = n.min(PAYLOAD_LEN - 1);
    unsafe {
        core::ptr::copy_nonoverlapping(tmp.as_ptr(), buf as *mut u8, n);
        *(buf as *mut u8).add(n) = 0;
    }
    n as u64
}

/// 把节点 `block` 的大小与元数据写成 `vfs::Stat` 到共享页 `buf`, 返回写入字节数。
fn mfs_stat_into(block: u32, buf: u64) -> u64 {
    let a = mfs_a();
    if !mfs_read_blk(block, a) {
        return u64::MAX;
    }
    // 类型信息不必单列一个字段: 它已在 `mode` 的高 4 位里 (见 `MFS_FTYPE_*`)。
    let (size, is_dir) = if mfs_ok(a, MFS_MAGIC_FILE) {
        (mfs_file_size(a), false)
    } else if mfs_ok(a, MFS_MAGIC_DIR) {
        (0, true)
    } else if mfs_ok(a, MFS_MAGIC_LINK) {
        // 软链接的 size 是目标路径字节数 (与 Unix `lstat` 一致)。
        (mfs_file_size(a), false)
    } else {
        return u64::MAX;
    };
    let st = vfs::Stat {
        size,
        is_dir: u32::from(is_dir),
        mode: mfs_get_mode(a, is_dir),
        owner: mfs_get_owner(a, is_dir),
        uid: mfs_get_uid(a, is_dir),
        gid: mfs_get_gid(a, is_dir),
        nlink: mfs_get_nlink(a, is_dir),
        mtime: mfs_get_mtime(a, is_dir),
        ctime: mfs_get_ctime(a, is_dir),
        atime: mfs_get_atime(a, is_dir),
    };
    unsafe {
        core::ptr::write_unaligned(buf as *mut vfs::Stat, st);
    }
    core::mem::size_of::<vfs::Stat>() as u64
}

/// 创建文件 (`is_dir=false`) 或目录 (`is_dir=true`)。成功返回 fd。
///
/// `owner` = 发起请求的域 id (只记进元数据供 `ls -l` 诊断); `cred` = 发起者身份,
/// 决定访问判定与新节点的 uid/gid。
fn mfs_create(path: &str, is_dir: bool, owner: u16, cred: Cred) -> u64 {
    let mut canon = [0u8; TMP_PATH_MAX];
    let n = match mfs_normalize(path, &mut canon) {
        Some(n) => n,
        None => return u64::MAX,
    };
    // 已存在: 文件直接打开; 目录按类型匹配 (creat 遇目录 / mkdir 遇任何已有项都失败)。
    if let Some(x) = mfs_resolve(&canon[..n]) {
        if is_dir {
            return mfs_err(MFS_EEXIST);
        }
        let blk = match mfs_ino_block(x) {
            Some(b) if b != 0 => b,
            _ => return u64::MAX,
        };
        if mfs_is_dir(blk) {
            return mfs_err(MFS_EISDIR);
        }
        // `creat` 对已存在文件 = 打开待写: 需文件可写 (root 直通)。
        if !mfs_read_blk(blk, mfs_a()) || !mfs_check_access(mfs_a(), false, cred, MFS_ACC_W) {
            return mfs_err(MFS_EACCES);
        }
        let perm = mfs_effective_perm(mfs_a(), false, cred);
        return mfs_fd_alloc(&canon[..n], false, cred, perm);
    }
    // 父目录路径与末分量。
    let mut split = n;
    while split > 1 && canon[split - 1] != b'/' {
        split -= 1;
    }
    let parent_end = if split > 1 { split - 1 } else { 1 };
    let comp = &canon[split..n];
    if comp.is_empty() {
        return u64::MAX;
    }
    let parent_ino = match mfs_resolve(&canon[..parent_end]) {
        Some(x) => x,
        None => return mfs_err(MFS_ENOENT),
    };
    // 04b: 在父目录内创建需父目录 W+X (root 直通)。
    if !mfs_check_dir_ino(parent_ino, cred, MFS_ACC_W | MFS_ACC_X) {
        return mfs_err(MFS_EACCES);
    }
    // 名称查重 (也顺带校验父目录可解析)。
    if mfs_dir_lookup(parent_ino, comp).is_some() {
        return mfs_err(MFS_EEXIST);
    }
    // 新建空节点 (用 C, 避免覆盖 A 中的父目录)。
    let c = mfs_c();
    zero_bytes(c, MFS_BLOCK);
    let mode = if is_dir { MFS_MODE_DIR } else { MFS_MODE_FILE };
    let ftype = if is_dir {
        MFS_FTYPE_DIR
    } else {
        MFS_FTYPE_FILE
    };
    if is_dir {
        mfs_dir_init_empty(c);
    } else {
        mfs_file_set_size(c, 0);
        mfs_file_set_nblocks(c, 0);
    }
    mfs_init_meta(c, is_dir, ftype, owner, cred, mode);
    let magic = if is_dir {
        MFS_MAGIC_DIR
    } else {
        MFS_MAGIC_FILE
    };
    // 先落对象块, 再为它登记一个 ino (登记时把表槽直接指向该块)。顺序反过来会先占
    // 一个空槽却还不知道块号, 需要写两次表。
    let obj = match mfs_commit(c, magic) {
        Some(b) => b,
        None => return u64::MAX,
    };
    let child_ino = match mfs_ino_alloc_for(obj) {
        Some(i) => i,
        None => return u64::MAX,
    };
    let typ = if is_dir { MFS_TYPE_DIR } else { MFS_TYPE_FILE };
    if !mfs_dir_insert(parent_ino, comp, child_ino, typ) {
        return u64::MAX;
    }
    if is_dir {
        1
    } else {
        // 创建者即属主: 有效权限 = 属主三位 (root 全放行)。
        let perm = if cred.uid == MFS_UID_ROOT {
            0o7
        } else {
            ((mode >> 6) & 0o7) as u8
        };
        mfs_fd_alloc(&canon[..n], false, cred, perm)
    }
}

/// 在 `linkpath` 建一个指向 `target` 的软链接 (M5c)。成功返回 1。
///
/// `target` **原样存储**: 不规范化、不做长度以外的校验, 也**不要求它存在** (允许悬空
/// 链接 —— 目标可以是之后才创建的文件)。绝对/相对在**解析时**判断: 以 '/' 开头 =
/// 从服务这棵树的根开始, 否则相对链接**所在目录**。
///
/// 注意绝对目标是**服务自己命名空间**里的路径, 不含挂载前缀 —— 客户端侧的
/// `vfs::symlink_into` 已经把同挂载点内的前缀剥掉了 (`/mfs/a` -> `/a`); 服务端
/// 只看得见自己这棵子树, 无从知道挂载点叫什么。
///
/// 软链接节点沿用文件布局 (目标内联在 payload 里, 不占数据块), 故元数据用
/// `is_dir = false` 读写; 它不参与硬链接, `nlink` 恒为 1。
fn mfs_symlink(target: &str, linkpath: &str, owner: u16, cred: Cred) -> u64 {
    let tb = target.as_bytes();
    if tb.is_empty() || tb.len() > MFS_LINK_MAX {
        return u64::MAX;
    }
    let mut canon = [0u8; MFS_PATH_MAX];
    let n = match mfs_normalize(linkpath, &mut canon) {
        Some(n) => n,
        None => return u64::MAX,
    };
    if n == 1 {
        return u64::MAX; // 不能把根变成软链接
    }
    // 已存在同名的任何条目都拒绝 (不覆盖) —— 含既有软链接 (含悬空的)。
    if mfs_resolve_no_follow(&canon[..n]).is_some() {
        return u64::MAX;
    }
    let mut split = n;
    while split > 1 && canon[split - 1] != b'/' {
        split -= 1;
    }
    let parent_end = if split > 1 { split - 1 } else { 1 };
    let comp = &canon[split..n];
    if comp.is_empty() {
        return u64::MAX;
    }
    let parent_ino = match mfs_resolve(&canon[..parent_end]) {
        Some(x) => x,
        None => return mfs_err(MFS_ENOENT),
    };
    // 04b: 建链接需链接自身父目录 W+X (root 直通); 目标路径不判。
    if !mfs_check_dir_ino(parent_ino, cred, MFS_ACC_W | MFS_ACC_X) {
        return mfs_err(MFS_EACCES);
    }
    if mfs_dir_lookup(parent_ino, comp).is_some() {
        return mfs_err(MFS_EEXIST);
    }
    // 目标内联进节点 (复用文件布局的 size 字段存目标长度)。
    let c = mfs_c();
    zero_bytes(c, MFS_BLOCK);
    mfs_file_set_size(c, tb.len() as u64);
    mfs_file_set_nblocks(c, 0);
    unsafe {
        core::ptr::copy_nonoverlapping(tb.as_ptr(), mfs_atm(c, MFS_LINK_TARGET_OFF), tb.len());
    }
    mfs_init_meta(c, false, MFS_FTYPE_LINK, owner, cred, MFS_MODE_LINK);
    let obj = match mfs_commit(c, MFS_MAGIC_LINK) {
        Some(b) => b,
        None => return u64::MAX,
    };
    let child_ino = match mfs_ino_alloc_for(obj) {
        Some(i) => i,
        None => return u64::MAX,
    };
    if !mfs_dir_insert(parent_ino, comp, child_ino, MFS_TYPE_LINK) {
        return u64::MAX;
    }
    1
}

/// 为已存在的文件 `src` 在 `dst` 再加一个名字 (硬链接)。成功返回 1。
///
/// 目录不允许硬链接 (会形成环)。有了 inode 表, 两个名字共享同一个 ino, 之后从任一个
/// 名字改写文件, 另一个名字都会看到新内容 —— 这正是 inode 间接层的核心收益:
/// 改对象只动它自己的表槽, 与"有多少个名字引用它"无关。
fn mfs_link(src: &str, dst: &str, cred: Cred) -> u64 {
    let mut sc = [0u8; TMP_PATH_MAX];
    let mut dc = [0u8; TMP_PATH_MAX];
    let sn = match mfs_normalize(src, &mut sc) {
        Some(n) => n,
        None => return u64::MAX,
    };
    let dn = match mfs_normalize(dst, &mut dc) {
        Some(n) => n,
        None => return u64::MAX,
    };
    if sn == 1 || dn == 1 {
        return u64::MAX;
    }
    let s_ino = match mfs_resolve(&sc[..sn]) {
        Some(x) => x,
        None => return u64::MAX,
    };
    let sblk = match mfs_ino_block(s_ino) {
        Some(b) if b != 0 => b,
        _ => return u64::MAX,
    };
    if mfs_is_dir(sblk) {
        return u64::MAX; // 目录不能硬链接
    }
    // 04b: 受保护硬链接 —— 无源写权限时, 仅当 uid 相同或 uid 0 才允许。
    if cred.uid != MFS_UID_ROOT {
        let a = mfs_a();
        if mfs_read_blk(sblk, a) {
            let s_uid = mfs_get_uid(a, false);
            if cred.uid != s_uid && !mfs_check_access(a, false, cred, MFS_ACC_W) {
                return mfs_err(MFS_EPERM);
            }
        }
    }
    // 目标必须不存在 (不覆盖)。
    if mfs_resolve(&dc[..dn]).is_some() {
        return mfs_err(MFS_EEXIST);
    }
    let mut split = dn;
    while split > 1 && dc[split - 1] != b'/' {
        split -= 1;
    }
    let dparent_end = if split > 1 { split - 1 } else { 1 };
    let dcomp = &dc[split..dn];
    if dcomp.is_empty() {
        return u64::MAX;
    }
    let dparent_ino = match mfs_resolve(&dc[..dparent_end]) {
        Some(x) => x,
        None => return mfs_err(MFS_ENOENT),
    };
    // 04b: 目标父目录 W+X (root 直通)。
    if !mfs_check_dir_ino(dparent_ino, cred, MFS_ACC_W | MFS_ACC_X) {
        return mfs_err(MFS_EACCES);
    }
    // 先建新名字再抬链接数: 反过来会留一段「计数已加但名字还不存在」的窗口, 崩溃后
    // 计数偏高 (只影响显示, 不丢数据)。
    if !mfs_dir_insert(dparent_ino, dcomp, s_ino, MFS_TYPE_FILE) {
        return u64::MAX;
    }
    let sblk = match mfs_ino_block(s_ino) {
        Some(b) if b != 0 => b,
        _ => return u64::MAX,
    };
    let a = mfs_a();
    if !mfs_read_blk(sblk, a) || !mfs_ok(a, MFS_MAGIC_FILE) {
        return u64::MAX;
    }
    let nlink = mfs_get_nlink(a, false).max(1);
    mfs_set_nlink(a, false, nlink + 1);
    mfs_touch_ctime(a, false);
    if mfs_commit_object(s_ino, a, MFS_MAGIC_FILE).is_none() {
        return u64::MAX;
    }
    1
}

/// 删除文件 (`want_dir=false`) 或空目录 (`want_dir=true`)。成功返回 1。
///
/// 文件是「摘名字」而不是「删对象」: 链接数减到 0 才释放它的 ino, 否则只是少了一个
/// 名字 (块交给 GC 按可达性回收)。
fn mfs_remove(path: &str, want_dir: bool, cred: Cred) -> u64 {
    let mut canon = [0u8; TMP_PATH_MAX];
    let n = match mfs_normalize(path, &mut canon) {
        Some(n) => n,
        None => return u64::MAX,
    };
    if n == 1 {
        return u64::MAX; // 不允许删除根
    }
    // 04b: 父目录 W+X (root 直通)。先把 sticky / 属主取出 (A 缓冲随后会被别名复用)。
    let mut split = n;
    while split > 1 && canon[split - 1] != b'/' {
        split -= 1;
    }
    let parent_end = if split > 1 { split - 1 } else { 1 };
    let parent_ino = match mfs_resolve(&canon[..parent_end]) {
        Some(x) => x,
        None => return mfs_err(MFS_ENOENT),
    };
    if !mfs_check_dir_ino(parent_ino, cred, MFS_ACC_W | MFS_ACC_X) {
        return mfs_err(MFS_EACCES);
    }
    let dir_sticky = mfs_get_mode(mfs_a(), true) & 0o1000 != 0;
    let dir_uid = mfs_get_uid(mfs_a(), true);
    let ino = match mfs_resolve_no_follow(&canon[..n]) {
        Some(x) => x,
        None => return mfs_err(MFS_ENOENT),
    };
    // 刚解析完, `MFS_LEAF` 就是指向它的那条条目 (可能在扩展块里)。
    // 用**不跟随**的解析: `rm`/`rmdir` 删的是条目本身 —— `rm link` 摘掉的是软链接,
    // 绝不能跟着目标去删目标文件。
    let loc = unsafe { MFS_LEAF };
    if loc.dir_ino == 0 {
        return mfs_err(MFS_ENOENT);
    }
    if !mfs_dir_load_loc(&loc) {
        return mfs_err(MFS_EIO);
    }
    let typ = mfs_ent_type(mfs_c(), loc.off);
    let is_dir = typ == MFS_TYPE_DIR;
    // 04b: sticky 目录里删除他人节点, 仅「节点属主 / 目录属主 / uid 0」可做。
    if dir_sticky && cred.uid != MFS_UID_ROOT {
        let nblk = mfs_ino_block(ino).unwrap_or_default();
        let n_uid = if nblk != 0 && mfs_read_blk(nblk, mfs_s()) {
            mfs_get_uid(mfs_s(), is_dir)
        } else {
            MFS_UID_ROOT
        };
        if cred.uid != dir_uid && cred.uid != n_uid {
            return mfs_err(MFS_EACCES);
        }
    }
    if want_dir != is_dir {
        return if is_dir {
            mfs_err(MFS_EISDIR)
        } else {
            mfs_err(MFS_ENOTDIR)
        };
    }
    if is_dir {
        // 目录必须为空 (条目可能散在扩展块里), 且目录不参与硬链接 -> 直接释放 ino。
        match mfs_dir_is_empty(ino) {
            Some(true) => {}
            _ => return u64::MAX,
        }
        if !mfs_dir_delete(&loc) || !mfs_free_ino(ino) {
            return u64::MAX;
        }
        return 1;
    }
    // 非目录 (普通文件 / 软链接): 先摘掉这个条目。
    if !mfs_dir_delete(&loc) {
        return u64::MAX;
    }
    if typ == MFS_TYPE_LINK {
        // 软链接不参与硬链接 (nlink 恒 1), 且它不占数据块 -> 直接释放 ino。
        return if mfs_free_ino(ino) { 1 } else { u64::MAX };
    }
    // 普通文件: 递减链接数; 还有别的名字就只更新计数, 否则释放 ino。
    let blk = match mfs_ino_block(ino) {
        Some(b) if b != 0 => b,
        _ => return u64::MAX,
    };
    let a = mfs_a();
    if !mfs_read_blk(blk, a) || !mfs_ok(a, MFS_MAGIC_FILE) {
        return u64::MAX;
    }
    let nlink = mfs_get_nlink(a, false);
    if nlink > 1 {
        mfs_set_nlink(a, false, nlink - 1);
        mfs_touch_ctime(a, false);
        if mfs_commit_object(ino, a, MFS_MAGIC_FILE).is_none() {
            return u64::MAX;
        }
    } else if !mfs_free_ino(ino) {
        return u64::MAX;
    }
    1
}

/// MFS 服务的卷认领 (取代通用 `vol_claim`)。
///
/// MFS 与其它文件系统不同: 一台机器上可能有多块 MFS 卷, 而**只有一块**该挂到 `/mfs`
/// —— 以前取「卷表里第一个」, 于是 `/mfs` 落在哪块卷上只由扫描顺序决定, 显式
/// `mkfs.mfs` 过谁毫无影响, 既不可控也无法解释。改为按**主卷序号**认领:
///
///   ① 序号最大且 > 0 的 MFS 卷 —— 序号由 `mkfs.mfs` 置成「现有最大 + 1」, 故它就是
///      「最近一次显式格式化过的卷」(见 `MFS_SB_PRIMARY`);
///   ② 都是 0 (老卷 / 只被首次挂载自动格式化过) 时退回卷表里第一个 MFS 卷;
///   ③ 一个 MFS 卷都没有时回退约定卷号 (空白盘没有 magic, 必须先格式化才会被探测到)。
fn mfs_vol_claim(scratch: *mut u8, max: u32) -> u64 {
    let n = block_list_volumes(scratch, max);
    if n == 0 || n == u64::MAX {
        return MFS_VOL_FALLBACK;
    }
    let esize = core::mem::size_of::<VolumeDesc>();
    let mut best: Option<(u64, u32)> = None;
    let mut first: Option<u32> = None;
    let mut i = 0u64;
    while i < n {
        let d = unsafe {
            core::ptr::read_unaligned(scratch.add(i as usize * esize) as *const VolumeDesc)
        };
        if d.kind == VOL_KIND_MFS {
            if first.is_none() {
                first = Some(d.id);
            }
            let serial = mfs_primary_of_vol(d.id as u64);
            if serial > 0 {
                match best {
                    Some((s, _)) if s >= serial => {}
                    _ => best = Some((serial, d.id)),
                }
            }
        }
        i += 1;
    }
    match best {
        Some((_, v)) => v as u64,
        None => first.map_or(MFS_VOL_FALLBACK, |v| v as u64),
    }
}
