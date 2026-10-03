use crate::common::*;
use morion::syscall::*;
use morion::vfs;

/// 大块写测试缓冲 (4KB, 跨簇扩展验证用)。
static mut BIG_WRITE_BUF: [u8; 4096] = [0u8; 4096];

/// FS-11 辅助: 从 `off` 起写 `pages` 个 4 KiB 页, 第 i 页整页填 `tag + i`。
fn fs11_write_pages(fd: u64, off: u32, pages: u32, tag: u8) -> bool {
    let mut i = 0u32;
    while i < pages {
        let want = tag.wrapping_add(i as u8);
        let n = unsafe {
            let buf = core::slice::from_raw_parts_mut(
                core::ptr::addr_of_mut!(BIG_WRITE_BUF) as *mut u8,
                4096,
            );
            buf.fill(want);
            let slice =
                core::slice::from_raw_parts(core::ptr::addr_of!(BIG_WRITE_BUF) as *const u8, 4096);
            vfs::write(fd, (off + i * 4096) as u64, slice)
        };
        if n != 4096 {
            return false;
        }
        i += 1;
    }
    true
}

/// FS-11 辅助: 读回 `pages` 页并逐字节比对 (期望与 `fs11_write_pages` 一致)。
fn fs11_verify_pages(fd: u64, off: u32, pages: u32, tag: u8) -> bool {
    let mut i = 0u32;
    while i < pages {
        let n = vfs::read(fd, (off + i * 4096) as u64, 4096);
        if n != 4096 {
            return false;
        }
        let want = tag.wrapping_add(i as u8);
        let got = unsafe { core::slice::from_raw_parts(vfs::RESULT_BUF as *const u8, 4096) };
        if got.iter().any(|&b| b != want) {
            return false;
        }
        i += 1;
    }
    true
}

/// FS-12 辅助: 生成 16 字节长名 `LONGFILE_nnn.TXT` (nnn = 3 位十进制)。
///
/// 16 字节日志式短名 + 8 字节条目头 = 24 字节/项 —— 目录节点块 (4080 字节条目区)
/// 恰好只容 170 项, 故第 171 项起必然落进扩展目录块 (M4 要验证的路径)。
fn fs12_name(i: u32, out: &mut [u8]) {
    out[..9].copy_from_slice(b"LONGFILE_");
    out[9] = b'0' + ((i / 100) % 10) as u8;
    out[10] = b'0' + ((i / 10) % 10) as u8;
    out[11] = b'0' + (i % 10) as u8;
    out[12..16].copy_from_slice(b".TXT");
}

/// FS-13 辅助: 取 `path` 的元数据 (`stat` 结果落在 `RESULT_BUF`)。
fn fs13_stat(path: &str) -> Option<vfs::Stat> {
    if vfs::stat(path) != core::mem::size_of::<vfs::Stat>() as u64 {
        return None;
    }
    Some(unsafe { core::ptr::read_unaligned(vfs::RESULT_BUF as *const vfs::Stat) })
}

/// FS-13 辅助: 从 `mode` 取**权限位** (低 12 位)。
///
/// M5c 起 `mode` 的高 4 位是节点类型 (见 `vfs::MODE_FTYPE_*`), 自测比较权限时统一用它
/// 剥掉类型位, 否则「0o644」这类断言会被类型位顶掉。
fn fs13_perm(mode: u16) -> u16 {
    mode & !vfs::MODE_FTYPE_MASK
}

/// FS-13 辅助: 读 `fd` 的 [off, off+count) 并确认整段为 0 (稀疏区验证用)。
fn fs13_all_zero(fd: u64, off: u32, count: u32) -> bool {
    if vfs::read(fd, off as u64, count) != count as u64 {
        return false;
    }
    let got = unsafe { core::slice::from_raw_parts(vfs::RESULT_BUF as *const u8, count as usize) };
    got.iter().all(|&b| b == 0)
}

/// FS-19 辅助: 在目录 `dir` 的 readdir 结果里按**长名**取条目, 用于检查类型位与 size。
fn fs19_entry(dir: &str, name: &str) -> Option<vfs::DirEntry> {
    let fd = vfs::open(dir);
    if fd == u64::MAX {
        return None;
    }
    let n = vfs::readdir(fd);
    vfs::close(fd);
    if n == u64::MAX {
        return None;
    }
    let entry_size = core::mem::size_of::<vfs::DirEntry>();
    let count = n as usize / entry_size;
    let list =
        unsafe { core::slice::from_raw_parts(vfs::RESULT_BUF as *const vfs::DirEntry, count) };
    let q = name.as_bytes();
    let mut found = None;
    for de in list {
        let llen = de.long_len as usize;
        if llen == q.len() && &de.long[..llen] == q {
            found = Some(*de);
        }
    }
    found
}

/// FS-20 辅助: `readlink` 并逐字节比对目标串。
fn fs20_readlink_is(path: &str, want: &str) -> bool {
    let n = vfs::readlink(path);
    if n == u64::MAX || n as usize != want.len() {
        return false;
    }
    let got = unsafe { core::slice::from_raw_parts(vfs::RESULT_BUF as *const u8, n as usize) };
    got == want.as_bytes()
}

/// FS-20 辅助: 取 `path` **自身** (不跟随软链接) 的元数据。
fn fs20_lstat(path: &str) -> Option<vfs::Stat> {
    if vfs::lstat(path) != core::mem::size_of::<vfs::Stat>() as u64 {
        return None;
    }
    Some(unsafe { core::ptr::read_unaligned(vfs::RESULT_BUF as *const vfs::Stat) })
}

/// E1/E2 自测用的可执行文件：在 FAT32 卷 (`/`) 根目录里的 `hello.mex`。
///
/// 由 `make` 从独立 crate `user/hello` 编译出的 ELF 拷进去（见 Makefile 的 `$(NVME_IMG)` 规则），
/// 是**真正的磁盘文件** —— 自测与 shell 的 `run` 走的是同一条 `morion::exec::spawn_file` 路径。
const HELLO_MEX: &str = "/hello.mex";

/// 域 7 — 测试应用: 经 libvfs 走通 read/write/readdir + FS-2/FS-3 全链路自测。
/// 成功路径完全静默 (只保留失败信息), 避免刷屏打断 shell 提示符。
pub fn run() {
    // 分配结果页并共享给各文件服务 (同地址映射), 供其写入文件/目录内容。
    if sys_alloc_page(vfs::RESULT_BUF) != 1 {
        println("app: alloc result buf FAILED");
        return;
    }
    if sys_share_page(vfs::RESULT_BUF, vfs::FAT32_DOMAIN) != 1
        || sys_share_page(vfs::RESULT_BUF, vfs::TMPFS_DOMAIN) != 1
        || sys_share_page(vfs::RESULT_BUF, vfs::MFS_DOMAIN) != 1
        || sys_share_page(vfs::RESULT_BUF, vfs::EXT2_DOMAIN) != 1
        || sys_share_page(vfs::RESULT_BUF, vfs::EXFAT_DOMAIN) != 1
        || sys_share_page(vfs::RESULT_BUF, BLOCK_DOMAIN) != 1
    {
        println("app: share result buf FAILED");
        return;
    }

    // 图形自测 (GS-1 / GT-1) 放在**最前**: 它是唯一"看屏幕"的取证, 提前跑就不必等后面
    // 几分钟的 NVMe FS 压测 —— 每次回归/开发都能立刻拿到图形结果 (g3b-reorder)。
    // `gfx_srv` (域 15) 是后启动的服务, 帧缓冲要等它接管后才写得进去; 这里不必自旋等待 ——
    // 首个 `ipc::call` 会一直等到它有回 (接管 + 进入请求循环) 为止。
    // GS-1 (G2): 图形原语 + 共享表面 —— 画色带 → blit 上屏 → 服务端回读校验。
    if gs1_gfx_primitives().is_none() {
        return;
    }
    // GT-1 (G3a): 文本渲染外移 —— 服务内终端按显示列排版, 逐像素写后回读校验。
    if gt1_text_rendering().is_none() {
        return;
    }
    // GS-2 (G6): 图形服务自愈 —— 杀掉 gfx_srv, init 监督重启, 客户端重建共享会话后恢复。
    if gs2_gfx_srv_restart().is_none() {
        return;
    }
    // GS-3 (G5): surface 合成 / 多窗口 —— 两个部分重叠的窗口按 z 序合成上屏 (含边界裁剪)。
    if gs3_window_compositor().is_none() {
        return;
    }
    // D0: I/O 端口能力门禁 —— 本域 (app) 未持任何 `IoPort` 能力, 读 CMOS 数据口必须被拒
    // (内核回 `u64::MAX`; 端口读只可能是 0..=0xFF, 不会与真实值混淆)。反面证据在回归里:
    // mfs_srv / exfat_srv 仍能写时间戳, 说明持有 0x70..0x72 的域照常放行。
    if sys_port_in8_raw(0x71) == u64::MAX {
        println("app: D0 port capability gate OK (ungranted I/O port denied)");
    } else {
        println("app: D0 port capability gate FAILED (read a port without IoPort)");
    }

    // 1. open -> read -> close: 读整个文件。
    let fd = vfs::open("/hello.txt");
    if fd == u64::MAX {
        println("app: open \"/hello.txt\" -> NOT FOUND");
        return;
    }
    if vfs::read(fd, 0, 4096) == u64::MAX {
        println("app: read \"/hello.txt\" -> FAILED");
    }
    vfs::close(fd);

    // 2. 分配并共享写缓冲页, 供各文件服务读取要写入的数据。
    if sys_alloc_page(vfs::WRITE_BUF) != 1 {
        println("app: alloc write buf FAILED");
        return;
    }
    if sys_share_page(vfs::WRITE_BUF, vfs::FAT32_DOMAIN) != 1
        || sys_share_page(vfs::WRITE_BUF, vfs::TMPFS_DOMAIN) != 1
        || sys_share_page(vfs::WRITE_BUF, vfs::MFS_DOMAIN) != 1
        || sys_share_page(vfs::WRITE_BUF, vfs::EXFAT_DOMAIN) != 1
        || sys_share_page(vfs::WRITE_BUF, vfs::EXT2_DOMAIN) != 1
    {
        println("app: share write buf FAILED");
        return;
    }

    // 3. write -> read: 覆盖并扩展 /hello.txt, 再读回验证。
    let wfd = vfs::open("/hello.txt");
    if wfd == u64::MAX {
        println("app: open \"/hello.txt\" for write -> NOT FOUND");
        return;
    }
    let data = b"OVERWRITTEN BY MORION OS WRITE TEST 0123456789";
    if vfs::write(wfd, 0, data) == u64::MAX {
        println("app: write \"/hello.txt\" -> FAILED");
    }
    if vfs::read(wfd, 0, 4096) == u64::MAX {
        println("app: read back -> FAILED");
    }
    vfs::close(wfd);

    // 3b. 跨簇扩展: 写满一页 4096 字节, 强制分配多个簇, 再读回逐字节校验。
    unsafe {
        let buf = core::slice::from_raw_parts_mut(
            core::ptr::addr_of_mut!(BIG_WRITE_BUF) as *mut u8,
            4096,
        );
        for (i, slot) in buf.iter_mut().enumerate() {
            *slot = b'A' + (i % 26) as u8;
        }
    }
    let bfd = vfs::open("/hello.txt");
    if bfd == u64::MAX {
        println("app: open \"/hello.txt\" for big write -> NOT FOUND");
        return;
    }
    let bwrite = unsafe {
        let slice =
            core::slice::from_raw_parts(core::ptr::addr_of!(BIG_WRITE_BUF) as *const u8, 4096);
        vfs::write(bfd, 0, slice)
    };
    let bread = vfs::read(bfd, 0, 4096);
    if bwrite != 4096 || bread != 4096 {
        println("app: big write/read FAILED");
    } else {
        let bcontent =
            unsafe { core::slice::from_raw_parts(vfs::RESULT_BUF as *const u8, bread as usize) };
        let mut ok = true;
        for (i, &b) in bcontent.iter().enumerate() {
            if b != (b'A' + (i % 26) as u8) {
                ok = false;
                break;
            }
        }
        if !ok {
            println("app: big write verify MISMATCH");
        }
    }
    vfs::close(bfd);

    // 4. open("/") -> readdir -> close: 列出根目录。
    let dfd = vfs::open("/");
    if dfd == u64::MAX {
        println("app: open \"/\" -> FAILED");
        return;
    }
    if vfs::readdir(dfd) == u64::MAX {
        println("app: readdir \"/\" -> FAILED");
    }
    vfs::close(dfd);

    // 5. FS-2 自测: mkdir → stat → creat → write → stat → unlink → rmdir 全链路。
    if vfs::mkdir("/DIRT") != 1 {
        println("app: FS2 mkdir \"/DIRT\" FAILED");
        return;
    }
    if vfs::stat("/DIRT") == u64::MAX {
        println("app: FS2 stat \"/DIRT\" FAILED");
        return;
    }
    {
        let dstat = unsafe { *(vfs::RESULT_BUF as *const vfs::Stat) };
        if dstat.is_dir != 1 {
            println("app: FS2 stat \"/DIRT\" not dir FAILED");
            return;
        }
    }

    let cfd = vfs::creat("/DIRT/NEWFILE");
    if cfd == u64::MAX {
        println("app: FS2 creat \"/DIRT/NEWFILE\" FAILED");
        return;
    }
    if vfs::write(cfd, 0, b"hello") != 5 {
        println("app: FS2 write \"/DIRT/NEWFILE\" FAILED");
        return;
    }
    vfs::close(cfd);

    if vfs::stat("/DIRT/NEWFILE") == u64::MAX {
        println("app: FS2 stat \"/DIRT/NEWFILE\" FAILED");
        return;
    }
    {
        let fstat = unsafe { *(vfs::RESULT_BUF as *const vfs::Stat) };
        if fstat.size != 5 || fstat.is_dir != 0 {
            println("app: FS2 stat \"/DIRT/NEWFILE\" size/dir FAILED");
            return;
        }
    }

    if vfs::unlink("/DIRT/NEWFILE") != 1 {
        println("app: FS2 unlink \"/DIRT/NEWFILE\" FAILED");
        return;
    }
    if vfs::rmdir("/DIRT") != 1 {
        println("app: FS2 rmdir \"/DIRT\" FAILED");
        return;
    }
    if vfs::stat("/DIRT") != u64::MAX {
        println("app: FS2 stat \"/DIRT\" still exists FAILED");
        return;
    }

    // 6. FS-3 自测: mkdir/touch/rm 后 readdir 验证目录结构变化。
    //    与 FS-2 (用 stat 校验) 不同, 这里用 readdir 校验父/子目录的可见性变化。
    if vfs::mkdir("/FS3DIR") != 1 {
        println("app: FS3 mkdir \"/FS3DIR\" FAILED");
        return;
    }
    let rfd = vfs::open("/");
    if rfd == u64::MAX || !readdir_has(rfd, "FS3DIR", true) {
        println("app: FS3 readdir \"/\" missing FS3DIR FAILED");
        return;
    }
    vfs::close(rfd);

    // touch: 创建空文件后立即关闭。
    let tfd = vfs::creat("/FS3DIR/TOUCH.TXT");
    if tfd == u64::MAX {
        println("app: FS3 creat \"/FS3DIR/TOUCH.TXT\" FAILED");
        return;
    }
    vfs::close(tfd);

    let d1 = vfs::open("/FS3DIR");
    if d1 == u64::MAX || !readdir_has(d1, "TOUCH.TXT", false) {
        println("app: FS3 readdir \"/FS3DIR\" missing TOUCH.TXT FAILED");
        return;
    }
    vfs::close(d1);

    // rm: 删除文件后 readdir 应不再可见。
    if vfs::unlink("/FS3DIR/TOUCH.TXT") != 1 {
        println("app: FS3 unlink \"/FS3DIR/TOUCH.TXT\" FAILED");
        return;
    }
    let d2 = vfs::open("/FS3DIR");
    if d2 == u64::MAX || readdir_has(d2, "TOUCH.TXT", false) {
        println("app: FS3 readdir \"/FS3DIR\" still has TOUCH.TXT FAILED");
        return;
    }
    vfs::close(d2);

    // rmdir: 删除空目录后根目录 readdir 应不再可见。
    if vfs::rmdir("/FS3DIR") != 1 {
        println("app: FS3 rmdir \"/FS3DIR\" FAILED");
        return;
    }
    let rfd2 = vfs::open("/");
    if rfd2 == u64::MAX || readdir_has(rfd2, "FS3DIR", true) {
        println("app: FS3 readdir \"/\" still has FS3DIR FAILED");
        return;
    }
    vfs::close(rfd2);

    // 7. FS-4 自测 (阶段 C2): 挂载层把 `/tmp/**` 路由到 tmpfs_srv, 其余仍走 fat32。
    //    走通 mkdir → readdir → creat → write → read → unlink → rmdir 全链路。
    let tfd_root = vfs::open("/tmp");
    if tfd_root == u64::MAX {
        println("app: FS4 open /tmp FAILED (mount routing)");
        return;
    }
    vfs::close(tfd_root);

    if vfs::mkdir("/tmp/D1") != 1 {
        println("app: FS4 mkdir /tmp/D1 FAILED");
        return;
    }
    let tdir = vfs::open("/tmp");
    if tdir == u64::MAX || !readdir_has(tdir, "D1", true) {
        println("app: FS4 readdir /tmp missing D1 FAILED");
        return;
    }
    vfs::close(tdir);

    let tf = vfs::creat("/tmp/D1/F1");
    if tf == u64::MAX {
        println("app: FS4 creat /tmp/D1/F1 FAILED");
        return;
    }
    if vfs::write(tf, 0, b"tmpfs!") != 6 {
        println("app: FS4 write FAILED");
        return;
    }
    if vfs::read(tf, 0, 64) != 6 {
        println("app: FS4 read FAILED");
        return;
    }
    {
        let got = unsafe { core::slice::from_raw_parts(vfs::RESULT_BUF as *const u8, 6) };
        if got != b"tmpfs!" {
            println("app: FS4 read content MISMATCH");
            return;
        }
    }
    vfs::close(tf);

    if vfs::unlink("/tmp/D1/F1") != 1 {
        println("app: FS4 unlink /tmp/D1/F1 FAILED");
        return;
    }
    if vfs::rmdir("/tmp/D1") != 1 {
        println("app: FS4 rmdir /tmp/D1 FAILED");
        return;
    }

    // 路由正确性: 同一调用序列在 `/` 下仍由 fat32 服务 (hello.txt 可见)。
    let rootfd = vfs::open("/");
    if rootfd == u64::MAX || !readdir_has(rootfd, "hello.txt", false) {
        println("app: FS4 readdir / missing hello.txt FAILED (routing)");
        return;
    }
    vfs::close(rootfd);

    // 8. FS-5 自测 (阶段 C3): MorionFS (块设备后端, 挂载 /mfs)。
    //    - 空白 mfs.img 首次挂载 -> mfs_srv 自动格式化;
    //    - mkdir/creat/write/read 走通 (每块 CRC32 校验);
    //    - 快照: 记录根 -> 覆盖文件 -> 回滚 -> 旧内容恢复 (验证 COW 语义);
    //    - 持久化: 留下 /mfs/PERSIST.TXT, 二次启动读回即证明数据已落盘。

    // 持久化检查 (第二次及以后启动)。
    let pfd = vfs::open("/mfs/PERSIST.TXT");
    if pfd != u64::MAX {
        let n = vfs::read(pfd, 0, 64);
        vfs::close(pfd);
        if n != 6 {
            println("app: FS5 persist read FAILED");
            return;
        }
        let got = unsafe { core::slice::from_raw_parts(vfs::RESULT_BUF as *const u8, 6) };
        if got != b"MFS-OK" {
            println("app: FS5 persist content MISMATCH");
            return;
        }
    }

    // 幂等准备: 上一轮若在本段清理之前提前返回 (例如快照失败), `/mfs/D` 会残留在盘上;
    // 这里先清掉残留, 否则本次 `mkdir` 会因为目录已存在而失败, 把上一次的失败传染到本次。
    vfs::unlink("/mfs/D/F");
    vfs::rmdir("/mfs/D");

    if vfs::mkdir("/mfs/D") != 1 {
        println("app: FS5 mkdir /mfs/D FAILED");
        return;
    }
    let mroot = vfs::open("/mfs");
    if mroot == u64::MAX || !readdir_has(mroot, "D", true) {
        println("app: FS5 readdir /mfs missing D FAILED");
        return;
    }
    vfs::close(mroot);

    let mf = vfs::creat("/mfs/D/F");
    if mf == u64::MAX || vfs::write(mf, 0, b"MFS-V1") != 6 {
        println("app: FS5 create/write /mfs/D/F FAILED");
        return;
    }
    vfs::close(mf);

    // 快照当前状态 (D/F = "MFS-V1")。
    let snap = vfs::mfs_snapshot();
    if snap == u64::MAX {
        println("app: FS5 snapshot FAILED");
        return;
    }

    // 覆盖为 V2。
    let mf2 = vfs::open("/mfs/D/F");
    if mf2 == u64::MAX || vfs::write(mf2, 0, b"MFS-V2") != 6 {
        println("app: FS5 overwrite V2 FAILED");
        return;
    }
    vfs::close(mf2);

    // 回滚到快照: COW 未覆盖旧块, 故快照树仍完好, 应读回 V1。
    if vfs::mfs_snapshot_restore(snap as u32) != 1 {
        println("app: FS5 snapshot restore FAILED");
        return;
    }
    let mf3 = vfs::open("/mfs/D/F");
    if mf3 == u64::MAX {
        println("app: FS5 reopen after restore FAILED");
        return;
    }
    let rn = vfs::read(mf3, 0, 64);
    vfs::close(mf3);
    if rn != 6 {
        println("app: FS5 read after restore FAILED");
        return;
    }
    {
        let got = unsafe { core::slice::from_raw_parts(vfs::RESULT_BUF as *const u8, 6) };
        if got != b"MFS-V1" {
            println("app: FS5 snapshot did not preserve old content FAILED");
            return;
        }
    }

    // 清理 (COW 只增不回收: 旧块保留给快照, 逻辑上删除即可)。
    if vfs::unlink("/mfs/D/F") != 1 {
        println("app: FS5 unlink FAILED");
        return;
    }
    if vfs::rmdir("/mfs/D") != 1 {
        println("app: FS5 rmdir FAILED");
        return;
    }

    // 持久化标记 (仅首次创建; 二次启动由上面的检查读回)。
    if vfs::open("/mfs/PERSIST.TXT") == u64::MAX {
        let pfd2 = vfs::creat("/mfs/PERSIST.TXT");
        if pfd2 == u64::MAX || vfs::write(pfd2, 0, b"MFS-OK") != 6 {
            println("app: FS5 create persist marker FAILED");
            return;
        }
        vfs::close(pfd2);
    }

    // 9. FS-6 自测 (阶段 C3): 运行时挂载 —— 挂载表不再只由编译期常量决定。
    //    - `/mnt0` 未挂载时应不可路由 (open 失败);
    //    - `MNTA` 空前缀 -> mount_srv 自动分配 `/mnt0` 给 mfs_srv;
    //    - 同一服务经新挂载点可达 (证明最长前缀匹配 + 前缀剥离都对);
    //    - `MNTD` 卸载后 `/mnt0` 又不可路由, 而 `/mfs` 不受影响。
    if vfs::open("/mnt0") != u64::MAX {
        println("app: FS6 /mnt0 reachable before mount FAILED");
        return;
    }
    if vfs::mount("", vfs::MFS_DOMAIN) == u64::MAX {
        println("app: FS6 runtime mount (auto) FAILED");
        return;
    }
    let a0 = vfs::open("/mnt0");
    if a0 == u64::MAX {
        println("app: FS6 open /mnt0 after mount FAILED");
        return;
    }
    vfs::close(a0);

    if vfs::umount("/mnt0") != 1 {
        println("app: FS6 umount /mnt0 FAILED");
        return;
    }
    if vfs::open("/mnt0") != u64::MAX {
        println("app: FS6 /mnt0 still routed after umount FAILED");
        return;
    }
    let mfs_still = vfs::open("/mfs");
    if mfs_still == u64::MAX {
        println("app: FS6 /mfs broken by umount FAILED");
        return;
    }
    vfs::close(mfs_still);

    // 句柄生命周期: 反复 open/close 不应耗尽内核句柄槽 (close 会撤销句柄,
    // 槽位复用)。循环次数 > 内核 HANDLE_SLOTS (32), 泄漏即在此暴露。
    for _ in 0..40 {
        let f = vfs::open("/mfs");
        if f == u64::MAX {
            println("app: FS6 capability handle slot leak FAILED");
            return;
        }
        vfs::close(f);
    }

    // 10. FS-7 自测 (阶段 C3): ext2 兼容 (挂载既有 Linux 分区)。
    //     - 根目录可达且列出宿主预置的 hello.txt / subdir (长名字段 = ext2 名字);
    //     - 读文件内容与宿主预置一致;
    //     - 子目录递归可达;
    //     - 大小写不敏感回退 (ext2 本身大小写敏感, 便于交互才加这一层);
    //     - 有限写: creat/write/read/unlink 往返自证 (结束 unlink, 逻辑状态复原);
    //     - 不存在的路径必须失败。
    let efd = vfs::open("/ext2");
    if efd == u64::MAX {
        println("app: FS7 open /ext2 FAILED");
        return;
    }
    if !readdir_has_long(efd, "hello.txt", false) || !readdir_has_long(efd, "subdir", true) {
        println("app: FS7 ext2 root listing FAILED");
        vfs::close(efd);
        return;
    }
    vfs::close(efd);

    let hfd = vfs::open("/ext2/hello.txt");
    if hfd == u64::MAX {
        println("app: FS7 open /ext2/hello.txt FAILED");
        return;
    }
    let hn = vfs::read(hfd, 0, 4096);
    vfs::close(hfd);
    if hn == u64::MAX {
        println("app: FS7 read /ext2/hello.txt FAILED");
        return;
    }
    {
        let want = b"Hello from ext2!\n";
        let got = unsafe { core::slice::from_raw_parts(vfs::RESULT_BUF as *const u8, hn as usize) };
        if got.len() < want.len() || &got[..want.len()] != want {
            println("app: FS7 /ext2/hello.txt content MISMATCH");
            return;
        }
    }

    // stat 也走 ext2 服务 (app 的结果页已共享给 ext2 域)。
    if vfs::stat("/ext2/hello.txt") == u64::MAX {
        println("app: FS7 stat /ext2/hello.txt FAILED");
        return;
    }
    {
        let st = unsafe { *(vfs::RESULT_BUF as *const vfs::Stat) };
        if st.is_dir != 0 || st.size == 0 {
            println("app: FS7 stat /ext2/hello.txt wrong metadata FAILED");
            return;
        }
    }
    if vfs::stat("/ext2/subdir") == u64::MAX {
        println("app: FS7 stat /ext2/subdir FAILED");
        return;
    }
    {
        let st = unsafe { *(vfs::RESULT_BUF as *const vfs::Stat) };
        if st.is_dir != 1 {
            println("app: FS7 stat /ext2/subdir not dir FAILED");
            return;
        }
    }

    let sfd = vfs::open("/ext2/subdir");
    if sfd == u64::MAX {
        println("app: FS7 open /ext2/subdir FAILED");
        return;
    }
    if !readdir_has_long(sfd, "nested.txt", false) {
        println("app: FS7 /ext2/subdir listing FAILED");
        vfs::close(sfd);
        return;
    }
    vfs::close(sfd);

    let nfd = vfs::open("/ext2/subdir/nested.txt");
    if nfd == u64::MAX {
        println("app: FS7 open /ext2/subdir/nested.txt FAILED");
        return;
    }
    let nn = vfs::read(nfd, 0, 4096);
    vfs::close(nfd);
    if nn == u64::MAX || nn < 3 {
        println("app: FS7 read /ext2/subdir/nested.txt FAILED");
        return;
    }

    // 大小写不敏感回退: 盘上名字已改为小写, 大写路径也应命中。
    let cfd7 = vfs::open("/ext2/HELLO.TXT");
    if cfd7 == u64::MAX {
        println("app: FS7 case-insensitive fallback FAILED");
        return;
    }
    vfs::close(cfd7);

    // 有限写往返自证: creat -> write -> close -> open -> read -> unlink -> open 必须失败。
    // 结束前 unlink 掉新建文件, 使 ext2 镜像逻辑状态 (free counts) 复原, 回归可反复跑。
    // 幂等防护: 回归复用同一 ext2.img (fs-regress.sh 不重建它), 上一轮若中途崩溃可能残留
    // 同名文件; 先尽力清掉 (干净镜像上该调用失败, 忽略即可), 否则 creat 会因重名而被拒。
    let _ = vfs::unlink("/ext2/NEW.TXT");
    let wfd7 = vfs::creat("/ext2/NEW.TXT");
    if wfd7 == u64::MAX {
        println("app: FS7 creat /ext2/NEW.TXT FAILED");
        return;
    }
    let wdata7 = b"MORION EXT2 WRITE OK 0123456789";
    if vfs::write(wfd7, 0, wdata7) != wdata7.len() as u64 {
        println("app: FS7 write /ext2/NEW.TXT FAILED");
        vfs::close(wfd7);
        return;
    }
    vfs::close(wfd7);

    let rfd7 = vfs::open("/ext2/NEW.TXT");
    if rfd7 == u64::MAX {
        println("app: FS7 reopen /ext2/NEW.TXT FAILED");
        return;
    }
    let rn7 = vfs::read(rfd7, 0, 4096);
    vfs::close(rfd7);
    if rn7 != wdata7.len() as u64 {
        println("app: FS7 read back /ext2/NEW.TXT FAILED");
        return;
    }
    {
        let got =
            unsafe { core::slice::from_raw_parts(vfs::RESULT_BUF as *const u8, rn7 as usize) };
        if got != &wdata7[..] {
            println("app: FS7 /ext2/NEW.TXT content MISMATCH");
            return;
        }
    }

    if vfs::unlink("/ext2/NEW.TXT") == u64::MAX {
        println("app: FS7 unlink /ext2/NEW.TXT FAILED");
        return;
    }
    if vfs::open("/ext2/NEW.TXT") != u64::MAX {
        println("app: FS7 unlinked /ext2/NEW.TXT still openable FAILED");
        return;
    }
    if vfs::open("/ext2/NOPE.TXT") != u64::MAX {
        println("app: FS7 missing path NOT rejected FAILED");
        return;
    }
    println("app: FS7 ext2 write round-trip OK (creat/write/read/unlink)");

    // 11. FS-8 自测 (阶段 C3): VFAT 长名 —— 读取 + 条目长名字段 + 按长名打开。
    let rfd8 = vfs::open("/");
    if rfd8 == u64::MAX {
        println("app: FS8 open / FAILED");
        return;
    }
    if !readdir_has_long(rfd8, "Long File Name.txt", false) {
        println("app: FS8 long name missing from readdir FAILED");
        vfs::close(rfd8);
        return;
    }
    vfs::close(rfd8);

    let lfd = vfs::open("/Long File Name.txt");
    if lfd == u64::MAX {
        println("app: FS8 open by long name FAILED");
        return;
    }
    let ln = vfs::read(lfd, 0, 4096);
    vfs::close(lfd);
    if ln == u64::MAX {
        println("app: FS8 read by long name FAILED");
        return;
    }
    {
        let want = b"long name read via VFAT LFN!\n";
        let got = unsafe { core::slice::from_raw_parts(vfs::RESULT_BUF as *const u8, ln as usize) };
        if got != want {
            println("app: FS8 long name content MISMATCH");
            return;
        }
    }
    // 长名匹配对大小写不敏感 (VFAT LFN 语义)。
    let lc = vfs::open("/long file name.txt");
    if lc == u64::MAX {
        println("app: FS8 case-insensitive long name open FAILED");
        return;
    }
    vfs::close(lc);

    // 12. FS-9 自测 (阶段 D/M1): 卷层 —— block_srv 解析 MBR 分区表并按卷首签名探测 FS 类型。
    //     - 向后兼容: 卷 0 = ns1 整盘 FAT、卷 2 = ns3 整盘 ext2 (start_lba 均为 0);
    //     - 新增的第 4 张盘有 MBR 两个主分区, 必须分别被识别为 FAT(@2048) 与 ext2(@34816)。
    let nvol = block_list_volumes(vfs::RESULT_BUF as *mut u8, 16);
    if nvol < 6 {
        println("app: FS9 volume count < 6 FAILED");
        return;
    }
    {
        // exFAT 盘 (nsid 5) 必须被卷层按签名识别为 EXFAT。
        let mut exfat_vol = false;
        let mut i = 0u64;
        while i < nvol {
            let d = vol_desc(vfs::RESULT_BUF as *const u8, i as usize);
            if d.kind == VOL_KIND_EXFAT && d.nsid == 5 && d.start_lba == 0 {
                exfat_vol = true;
            }
            i += 1;
        }
        if !exfat_vol {
            println("app: FS9 exFAT volume missing FAILED");
            return;
        }
    }
    {
        let v0 = vol_desc(vfs::RESULT_BUF as *const u8, 0);
        if v0.kind != VOL_KIND_FAT || v0.nsid != 1 || v0.start_lba != 0 {
            println("app: FS9 vol0 (whole-disk FAT32) FAILED");
            return;
        }
        let v2 = vol_desc(vfs::RESULT_BUF as *const u8, 2);
        if v2.kind != VOL_KIND_EXT2 || v2.nsid != 3 || v2.start_lba != 0 {
            println("app: FS9 vol2 (whole-disk ext2) FAILED");
            return;
        }
    }
    {
        // 分区盘 (nsid 4) 的两个分区。
        let mut fat_part = false;
        let mut ext2_part = false;
        let mut i = 0u64;
        while i < nvol {
            let d = vol_desc(vfs::RESULT_BUF as *const u8, i as usize);
            if d.nsid == 4 {
                if d.kind == VOL_KIND_FAT && d.start_lba == 2048 {
                    fat_part = true;
                }
                if d.kind == VOL_KIND_EXT2 && d.start_lba == 34816 {
                    ext2_part = true;
                }
            }
            i += 1;
        }
        if !fat_part || !ext2_part {
            println("app: FS9 MBR partition scan FAILED");
        }
    }

    // 13. FS-10 自测 (阶段 D/M2): MFS v2 空闲位图 + 空间回收 (GC)。
    //     - 反复覆盖同一文件时 COW 持续吃新块 -> 空闲块下降;
    //     - 删除后 GC 按可达性重建位图 -> 这些垃圾块被归还;
    //     - 快照根也是可达根: GC 不得回收快照仍引用的历史版本。
    let st0 = vfs::mfs_stat();
    if st0 == u64::MAX {
        println("app: FS10 mfs_stat FAILED");
        return;
    }
    let free0 = st0 & 0xFFFF_FFFF;
    if (st0 >> 32) == 0 || free0 == 0 {
        println("app: FS10 stat sanity FAILED");
        return;
    }

    // 64 KiB 文件覆盖写 4 轮 (每轮 COW 出 32 个数据块 + inode + 目录链)。
    let gfd = vfs::creat("/mfs/GC.BIN");
    if gfd == u64::MAX {
        println("app: FS10 creat /mfs/GC.BIN FAILED");
        return;
    }
    let chunk = unsafe {
        core::slice::from_raw_parts_mut(core::ptr::addr_of_mut!(BIG_WRITE_BUF) as *mut u8, 4096)
    };
    for round in 0..4u32 {
        for (i, slot) in chunk.iter_mut().enumerate() {
            *slot = b'0' + (round as u8) + (i % 8) as u8;
        }
        let mut off = 0u32;
        while off < 65536 {
            let n = unsafe {
                let slice = core::slice::from_raw_parts(
                    core::ptr::addr_of!(BIG_WRITE_BUF) as *const u8,
                    4096,
                );
                vfs::write(gfd, off as u64, slice)
            };
            if n != 4096 {
                println("app: FS10 chunked write FAILED");
                vfs::close(gfd);
                return;
            }
            off += 4096;
        }
    }
    vfs::close(gfd);

    let st1 = vfs::mfs_stat();
    if st1 == u64::MAX {
        println("app: FS10 stat after overwrite FAILED");
        return;
    }
    let free1 = st1 & 0xFFFF_FFFF;
    if free1 + 100 > free0 {
        println("app: FS10 COW did not consume space FAILED");
        return;
    }

    // 删除 + 回收: 该文件的数据块 / inode / 中间目录块都应被归还。
    if vfs::unlink("/mfs/GC.BIN") != 1 {
        println("app: FS10 unlink /mfs/GC.BIN FAILED");
        return;
    }
    let freed = vfs::mfs_gc();
    if freed == u64::MAX || freed < 100 {
        println("app: FS10 GC did not reclaim space FAILED");
        return;
    }

    // 快照安全: 快照之后覆盖写, GC 仍保留快照引用的旧版本, 回滚后内容应为旧值。
    let sfd10 = vfs::creat("/mfs/GCS.BIN");
    if sfd10 == u64::MAX || vfs::write(sfd10, 0, b"SNAP-A") != 6 {
        println("app: FS10 create snapshot file FAILED");
        return;
    }
    vfs::close(sfd10);
    let snap10 = vfs::mfs_snapshot();
    if snap10 == u64::MAX {
        println("app: FS10 snapshot FAILED");
        return;
    }
    let s2fd = vfs::open("/mfs/GCS.BIN");
    if s2fd == u64::MAX || vfs::write(s2fd, 0, b"SNAP-B") != 6 {
        println("app: FS10 overwrite after snapshot FAILED");
        return;
    }
    vfs::close(s2fd);
    if vfs::mfs_gc() == u64::MAX {
        println("app: FS10 GC with snapshot FAILED");
        return;
    }
    // 分配扰动: 若 GC 误释放了快照引用的块, 这里的分配会立刻把它覆盖, 从而在下面
    // 的回滚读回中暴露 (否则可能侥幸读到还没被复用的旧内容)。
    let cfd10 = vfs::creat("/mfs/CHURN.BIN");
    if cfd10 == u64::MAX {
        println("app: FS10 creat churn file FAILED");
        return;
    }
    let mut coff = 0u32;
    while coff < 32768 {
        let n = unsafe {
            let slice =
                core::slice::from_raw_parts(core::ptr::addr_of!(BIG_WRITE_BUF) as *const u8, 4096);
            vfs::write(cfd10, coff as u64, slice)
        };
        if n != 4096 {
            println("app: FS10 churn write FAILED");
            vfs::close(cfd10);
            return;
        }
        coff += 4096;
    }
    vfs::close(cfd10);
    if vfs::mfs_snapshot_restore(snap10 as u32) != 1 {
        println("app: FS10 restore after GC FAILED");
        return;
    }
    let r3 = vfs::open("/mfs/GCS.BIN");
    if r3 == u64::MAX {
        println("app: FS10 reopen after restore+GC FAILED");
        return;
    }
    let rn3 = vfs::read(r3, 0, 64);
    vfs::close(r3);
    if rn3 != 6 {
        println("app: FS10 read after restore+GC FAILED");
        return;
    }
    {
        let got = unsafe { core::slice::from_raw_parts(vfs::RESULT_BUF as *const u8, 6) };
        if got != b"SNAP-A" {
            println("app: FS10 GC broke snapshot history FAILED");
            return;
        }
    }

    // 清理 (旧版本仍被快照钉住, 回收不掉的块有限且随快照淘汰自然释放)。
    if vfs::unlink("/mfs/GCS.BIN") != 1 {
        println("app: FS10 cleanup unlink FAILED");
        return;
    }
    if vfs::mfs_gc() == u64::MAX {
        println("app: FS10 final GC FAILED");
        return;
    }

    // 14. FS-11 自测 (阶段 D/M3): MFS v2 大文件 —— 直接 / 一级 / 二级间接块映射。
    //     - 起点放在直接区末尾, 连写 5 页必然从直接区跨进一级间接区;
    //     - 再在二级间接区写一页, 文件大小突破旧的 ≈4 MiB 上限;
    //     - GC 必须把间接块及其指向的数据块都当作可达, 否则稀疏大文件读回会失败。
    let bfd11 = vfs::creat("/mfs/BIG.BIN");
    if bfd11 == u64::MAX {
        println("app: FS11 creat /mfs/BIG.BIN FAILED");
        return;
    }
    let cross_off = (MFS_FILE_DIRECT - 3) as u32 * MFS_DATA_CAP as u32;
    let l2_off = (MFS_FILE_DIRECT + MFS_IND_CAP) as u32 * MFS_DATA_CAP as u32;
    if !fs11_write_pages(bfd11, cross_off, 5, 0x40) {
        println("app: FS11 cross-boundary write FAILED");
        vfs::close(bfd11);
        return;
    }
    if !fs11_write_pages(bfd11, l2_off, 1, 0x80) {
        println("app: FS11 second-level write FAILED");
        vfs::close(bfd11);
        return;
    }
    // 大小必须超过 4 MiB (直接区上限) —— 这正是 M3 要突破的边界。
    if vfs::stat("/mfs/BIG.BIN") != core::mem::size_of::<vfs::Stat>() as u64 {
        println("app: FS11 stat FAILED");
        vfs::close(bfd11);
        return;
    }
    let size11 = unsafe { core::ptr::read_unaligned(vfs::RESULT_BUF as *const vfs::Stat) }.size;
    if size11 <= 4 * 1024 * 1024 {
        println("app: FS11 size still capped at 4 MiB FAILED");
        vfs::close(bfd11);
        return;
    }
    if !fs11_verify_pages(bfd11, cross_off, 5, 0x40) || !fs11_verify_pages(bfd11, l2_off, 1, 0x80) {
        println("app: FS11 read back FAILED");
        vfs::close(bfd11);
        return;
    }
    // 回收 + 分配扰动: GC 若误回收间接块/数据块, 扰动会把它们覆盖, 读回即暴露。
    if vfs::mfs_gc() == u64::MAX {
        println("app: FS11 GC FAILED");
        vfs::close(bfd11);
        return;
    }
    let chfd11 = vfs::creat("/mfs/CHURN3.BIN");
    if chfd11 == u64::MAX || !fs11_write_pages(chfd11, 0, 8, 0xC0) {
        println("app: FS11 churn FAILED");
        vfs::close(bfd11);
        return;
    }
    vfs::close(chfd11);
    if !fs11_verify_pages(bfd11, cross_off, 5, 0x40) || !fs11_verify_pages(bfd11, l2_off, 1, 0x80) {
        println("app: FS11 read back after GC FAILED");
        vfs::close(bfd11);
        return;
    }
    vfs::close(bfd11);

    // 清理: 删除大文件与扰动文件, 回收它们的数据块与间接块。
    if vfs::unlink("/mfs/BIG.BIN") != 1 || vfs::unlink("/mfs/CHURN3.BIN") != 1 {
        println("app: FS11 cleanup unlink FAILED");
        return;
    }
    if vfs::mfs_gc() == u64::MAX {
        println("app: FS11 final GC FAILED");
    }

    // 15. FS-12 自测 (阶段 D/M4): MFS v2 目录与长名。
    //     - 单目录 200 项、名长 16 字节: 节点块只容 170 项, 其余必进扩展目录块;
    //     - 长名 (57 字节) 全程走 IPC payload: 建 / 查 / 读 / readdir 回传;
    //     - 深目录 20 级 (旧上限 12), 验证目录嵌套不再受结构限制;
    //     - GC + 分配扰动后逐项读回: 目录扩展块/索引块若被误回收, 扰动会覆盖它们。
    const FS12_DIR: &str = "/mfs/DIR12";
    const FS12_FILES: u32 = 200;
    const FS12_LONG: &str = "/mfs/LONGFILE_WITH_A_REALLY_LONG_NAME_0123456789ABCDEFGHIJ.TXT";
    // 幂等准备: 上一轮若在本段清理之前提前返回 (失败 / 被中断), 残留在盘上的目录与文件
    // 会让本次 `mkdir` 因「目录已存在」而失败, 把上一次的失败传染到本次 —— 与 FS-5 的
    // 处理相同 (MFS 是持久卷, 自测必须能反复重跑)。
    {
        let mut cb = [0u8; 64];
        let nb = FS12_DIR.len() + 1;
        cb[..FS12_DIR.len()].copy_from_slice(FS12_DIR.as_bytes());
        cb[FS12_DIR.len()] = b'/';
        let mut k = 0u32;
        while k < FS12_FILES {
            fs12_name(k, &mut cb[nb..nb + 16]);
            let s = unsafe { core::str::from_utf8_unchecked(&cb[..nb + 16]) };
            vfs::unlink(s);
            k += 1;
        }
        vfs::unlink("/mfs/CHURN4.BIN");
        vfs::unlink(FS12_LONG);
        vfs::rmdir(FS12_DIR);
        // 深目录 (18 级, 名字按 lvl % 26 循环) 自叶向上删。
        let mut db = [0u8; 64];
        db[..8].copy_from_slice(b"/mfs/D12");
        let mut dl0 = 8usize;
        let mut lvl0 = 0u32;
        while lvl0 < 18 {
            db[dl0] = b'/';
            db[dl0 + 1] = b'a' + (lvl0 % 26) as u8;
            dl0 += 2;
            lvl0 += 1;
        }
        db[dl0] = b'/';
        db[dl0 + 1] = b'H';
        let hp0 = unsafe { core::str::from_utf8_unchecked(&db[..dl0 + 2]) };
        vfs::unlink(hp0);
        let mut d0 = dl0;
        while d0 > 8 {
            let s = unsafe { core::str::from_utf8_unchecked(&db[..d0]) };
            vfs::rmdir(s);
            d0 -= 2;
        }
        vfs::rmdir("/mfs/D12");
    }
    if vfs::mkdir(FS12_DIR) != 1 {
        println("app: FS12 mkdir /mfs/DIR12 FAILED");
        return;
    }
    let mut pbuf = [0u8; 64];
    let nbase = FS12_DIR.len() + 1;
    pbuf[..FS12_DIR.len()].copy_from_slice(FS12_DIR.as_bytes());
    pbuf[FS12_DIR.len()] = b'/';
    // 建 200 个 16 字节名文件, 每个写入唯一字节; 中途回收以压住 COW 垃圾水位。
    let mut i = 0u32;
    while i < FS12_FILES {
        fs12_name(i, &mut pbuf[nbase..nbase + 16]);
        let s = unsafe { core::str::from_utf8_unchecked(&pbuf[..nbase + 16]) };
        let fd = vfs::creat(s);
        if fd == u64::MAX {
            println("app: FS12 creat FAILED");
            return;
        }
        let n = unsafe {
            let buf = core::slice::from_raw_parts_mut(
                core::ptr::addr_of_mut!(BIG_WRITE_BUF) as *mut u8,
                4096,
            );
            buf.fill(i as u8);
            let slice =
                core::slice::from_raw_parts(core::ptr::addr_of!(BIG_WRITE_BUF) as *const u8, 4096);
            vfs::write(fd, 0, slice)
        };
        vfs::close(fd);
        if n != 4096 {
            println("app: FS12 write FAILED");
            return;
        }
        i += 1;
        if i.is_multiple_of(64) && vfs::mfs_gc() == u64::MAX {
            println("app: FS12 mid GC FAILED");
            return;
        }
    }
    // 逐项按名查回 (第 171 项起的查找必须穿过扩展目录块) 并校验内容。
    i = 0;
    while i < FS12_FILES {
        fs12_name(i, &mut pbuf[nbase..nbase + 16]);
        let s = unsafe { core::str::from_utf8_unchecked(&pbuf[..nbase + 16]) };
        let fd = vfs::open(s);
        if fd == u64::MAX {
            println("app: FS12 open FAILED");
            return;
        }
        let n = vfs::read(fd, 0, 512);
        vfs::close(fd);
        let got = unsafe { core::slice::from_raw_parts(vfs::RESULT_BUF as *const u8, 512) };
        if n != 512 || got.iter().any(|&b| b != i as u8) {
            println("app: FS12 read back FAILED");
            return;
        }
        i += 1;
    }
    // readdir: 一页放不下 200 项, 必须正好写满 RESULT_MAX_ENTRIES 条且长名回传完整。
    let dfd = vfs::open(FS12_DIR);
    if dfd == u64::MAX {
        println("app: FS12 open dir FAILED");
        return;
    }
    let dn = vfs::readdir(dfd);
    vfs::close(dfd);
    if dn == u64::MAX {
        println("app: FS12 readdir FAILED");
        return;
    }
    let dents = dn as usize / core::mem::size_of::<vfs::DirEntry>();
    if dents != vfs::RESULT_MAX_ENTRIES {
        println("app: FS12 readdir count FAILED");
        return;
    }
    let drr =
        unsafe { core::slice::from_raw_parts(vfs::RESULT_BUF as *const vfs::DirEntry, dents) };
    for de in drr.iter() {
        if de.long_len as usize != 16 {
            println("app: FS12 readdir long name FAILED");
            return;
        }
    }
    // 长名 (57 字节) 端到端: 建 → 关 → 按全名重开 → 读回 → readdir 原样回传。
    let lfd = vfs::creat(FS12_LONG);
    if lfd == u64::MAX {
        println("app: FS12 creat long name FAILED");
        return;
    }
    let ln = unsafe {
        let buf = core::slice::from_raw_parts_mut(
            core::ptr::addr_of_mut!(BIG_WRITE_BUF) as *mut u8,
            4096,
        );
        buf.fill(0xA5);
        let slice =
            core::slice::from_raw_parts(core::ptr::addr_of!(BIG_WRITE_BUF) as *const u8, 4096);
        vfs::write(lfd, 0, slice)
    };
    vfs::close(lfd);
    if ln != 4096 {
        println("app: FS12 long name write FAILED");
        return;
    }
    let lfd2 = vfs::open(FS12_LONG);
    if lfd2 == u64::MAX {
        println("app: FS12 long name reopen FAILED");
        return;
    }
    let lrn = vfs::read(lfd2, 0, 4096);
    vfs::close(lfd2);
    let lgot = unsafe { core::slice::from_raw_parts(vfs::RESULT_BUF as *const u8, 4096) };
    if lrn != 4096 || lgot.iter().any(|&b| b != 0xA5) {
        println("app: FS12 long name read FAILED");
        return;
    }
    let mfd = vfs::open("/mfs");
    if mfd == u64::MAX {
        println("app: FS12 open /mfs FAILED");
        return;
    }
    let mn = vfs::readdir(mfd);
    vfs::close(mfd);
    if mn == u64::MAX {
        println("app: FS12 readdir /mfs FAILED");
        return;
    }
    let ments = mn as usize / core::mem::size_of::<vfs::DirEntry>();
    let marr =
        unsafe { core::slice::from_raw_parts(vfs::RESULT_BUF as *const vfs::DirEntry, ments) };
    let lwant = &FS12_LONG.as_bytes()[5..]; // 去掉 "/mfs/" 前缀
    if !marr
        .iter()
        .any(|de| de.long_len as usize == lwant.len() && &de.long[..lwant.len()] == lwant)
    {
        println("app: FS12 readdir long name FAILED");
        return;
    }
    // 深目录: /mfs/D12 起逐级建 18 个单字母目录 -> 叶子深度 20 (旧上限 12)。
    if vfs::mkdir("/mfs/D12") != 1 {
        println("app: FS12 mkdir D12 FAILED");
        return;
    }
    let mut dp = [0u8; 64];
    dp[..8].copy_from_slice(b"/mfs/D12");
    let mut dl = 8usize;
    let mut lvl = 0u32;
    while lvl < 18 {
        dp[dl] = b'/';
        dp[dl + 1] = b'a' + (lvl % 26) as u8;
        dl += 2;
        let s = unsafe { core::str::from_utf8_unchecked(&dp[..dl]) };
        if vfs::mkdir(s) != 1 {
            println("app: FS12 deep mkdir FAILED");
            return;
        }
        lvl += 1;
    }
    dp[dl] = b'/';
    dp[dl + 1] = b'H';
    let hfile = dl + 2;
    let hpath = unsafe { core::str::from_utf8_unchecked(&dp[..hfile]) };
    let hfd = vfs::creat(hpath);
    if hfd == u64::MAX {
        println("app: FS12 deep creat FAILED");
        return;
    }
    let hn = unsafe {
        let buf = core::slice::from_raw_parts_mut(
            core::ptr::addr_of_mut!(BIG_WRITE_BUF) as *mut u8,
            4096,
        );
        buf.fill(0x5A);
        let slice =
            core::slice::from_raw_parts(core::ptr::addr_of!(BIG_WRITE_BUF) as *const u8, 4096);
        vfs::write(hfd, 0, slice)
    };
    vfs::close(hfd);
    if hn != 4096 {
        println("app: FS12 deep write FAILED");
        return;
    }
    let hfd2 = vfs::open(hpath);
    if hfd2 == u64::MAX {
        println("app: FS12 deep reopen FAILED");
        return;
    }
    let hrn = vfs::read(hfd2, 0, 4096);
    vfs::close(hfd2);
    let hgot = unsafe { core::slice::from_raw_parts(vfs::RESULT_BUF as *const u8, 4096) };
    if hrn != 4096 || hgot.iter().any(|&b| b != 0x5A) {
        println("app: FS12 deep read FAILED");
        return;
    }
    // 回收 + 分配扰动: GC 若漏标目录扩展块/索引块, 扰动会覆盖它们, 抽样读回即暴露。
    if vfs::mfs_gc() == u64::MAX {
        println("app: FS12 GC FAILED");
        return;
    }
    let chfd = vfs::creat("/mfs/CHURN4.BIN");
    if chfd == u64::MAX || !fs11_write_pages(chfd, 0, 8, 0xE0) {
        println("app: FS12 churn FAILED");
        return;
    }
    vfs::close(chfd);
    i = 0;
    while i < FS12_FILES {
        if i.is_multiple_of(17) {
            fs12_name(i, &mut pbuf[nbase..nbase + 16]);
            let s = unsafe { core::str::from_utf8_unchecked(&pbuf[..nbase + 16]) };
            let fd = vfs::open(s);
            if fd == u64::MAX {
                println("app: FS12 reopen after GC FAILED");
                return;
            }
            let n = vfs::read(fd, 0, 512);
            vfs::close(fd);
            let got = unsafe { core::slice::from_raw_parts(vfs::RESULT_BUF as *const u8, 512) };
            if n != 512 || got.iter().any(|&b| b != i as u8) {
                println("app: FS12 content after GC FAILED");
                return;
            }
        }
        i += 1;
    }
    // 清理: 深目录自叶向上删, 再删长名文件、200 项与扰动文件。
    if vfs::unlink(hpath) != 1 {
        println("app: FS12 cleanup deep file FAILED");
        return;
    }
    let mut d = dl;
    while d > 8 {
        let s = unsafe { core::str::from_utf8_unchecked(&dp[..d]) };
        if vfs::rmdir(s) != 1 {
            println("app: FS12 cleanup deep rmdir FAILED");
            return;
        }
        d -= 2;
    }
    if vfs::rmdir("/mfs/D12") != 1 {
        println("app: FS12 cleanup D12 FAILED");
        return;
    }
    if vfs::unlink(FS12_LONG) != 1 {
        println("app: FS12 cleanup long name FAILED");
        return;
    }
    i = 0;
    while i < FS12_FILES {
        fs12_name(i, &mut pbuf[nbase..nbase + 16]);
        let s = unsafe { core::str::from_utf8_unchecked(&pbuf[..nbase + 16]) };
        if vfs::unlink(s) != 1 {
            println("app: FS12 cleanup unlink FAILED");
            return;
        }
        i += 1;
    }
    if vfs::rmdir(FS12_DIR) != 1 || vfs::unlink("/mfs/CHURN4.BIN") != 1 {
        println("app: FS12 cleanup FAILED");
        return;
    }
    if vfs::mfs_gc() == u64::MAX {
        println("app: FS12 final GC FAILED");
    }

    // 16. FS-13 自测 (阶段 D/M5): MFS v2 节点元数据 + rename + truncate。
    //     - 时间戳来自 CMOS RTC, 必须落在合理区间 (而不是 0 或垃圾值);
    //     - chmod 改 mode 后经 GC 仍保持 (元数据随 inode 走 COW);
    //     - truncate 截短释放尾部块, 扩展为稀疏 (读回 0);
    //     - rename 跨目录 + 覆盖已存在文件 + 拒绝把目录移进自己的子孙;
    //     - 全程混入 GC 与分配扰动, 验证元数据不影响可达性判定。
    const FS13_DIR: &str = "/mfs/M5";
    const FS13_SUB: &str = "/mfs/M5/SUB";
    const FS13_FILE: &str = "/mfs/M5/A.TXT";
    const FS13_MOVED: &str = "/mfs/M5/SUB/B.TXT";
    const FS13_DST: &str = "/mfs/M5/C.TXT";
    /// 2020-01-01T00:00:00Z —— RTC 只要正常就应大于它。
    const FS13_EPOCH_FLOOR: u64 = 1_577_836_800;

    // 幂等准备: `/mfs` 是持久卷, 上一轮若在自测中途被打断 (或被 kill), 这些对象会留在
    // 卷上, 让下面的 `mkdir` 因「已存在」失败并传染后续所有步骤。先清成干净状态。
    // 顺序: 先摘文件, 再自底向上删目录 (目录非空删不掉)。
    vfs::unlink("/mfs/M5/SUB2/B.TXT");
    vfs::unlink("/mfs/M5/SUB/B.TXT");
    vfs::unlink("/mfs/M5/C.TXT");
    vfs::unlink("/mfs/M5/A.TXT");
    vfs::rmdir("/mfs/M5/SUB2/INNER");
    vfs::rmdir("/mfs/M5/SUB2");
    vfs::rmdir("/mfs/M5/SUB");
    vfs::rmdir("/mfs/M5");

    if vfs::mkdir(FS13_DIR) != 1 || vfs::mkdir(FS13_SUB) != 1 {
        println("app: FS13 mkdir FAILED");
        return;
    }
    let fd13 = vfs::creat(FS13_FILE);
    if fd13 == u64::MAX || !fs11_write_pages(fd13, 0, 3, 0x50) {
        println("app: FS13 write FAILED");
        return;
    }
    vfs::close(fd13);

    // 元数据: 大小 / 属主 / 权限 / 链接数 / 时间戳。
    let st13 = match fs13_stat(FS13_FILE) {
        Some(s) => s,
        None => {
            println("app: FS13 stat FAILED");
            return;
        }
    };
    if st13.size != 3 * 4096 {
        println("app: FS13 size FAILED");
        return;
    }
    if st13.owner != APP_DOMAIN as u16 {
        println("app: FS13 owner FAILED");
        return;
    }
    if fs13_perm(st13.mode) != 0o644 || st13.nlink != 1 || st13.is_dir != 0 {
        println("app: FS13 default meta FAILED");
        return;
    }
    if st13.mtime < FS13_EPOCH_FLOOR || st13.ctime < FS13_EPOCH_FLOOR {
        println("app: FS13 RTC timestamp FAILED");
        return;
    }
    if st13.atime == 0 {
        println("app: FS13 atime FAILED");
        return;
    }

    // chmod 后 mode 必须改; 再经一次 GC 仍保持。
    if vfs::chmod(FS13_FILE, 0o600) != 1 {
        println("app: FS13 chmod FAILED");
        return;
    }
    if fs13_stat(FS13_FILE).map(|s| fs13_perm(s.mode)) != Some(0o600) {
        println("app: FS13 chmod readback FAILED");
        return;
    }
    if vfs::mfs_gc() == u64::MAX {
        println("app: FS13 GC after chmod FAILED");
        return;
    }
    let st13b = match fs13_stat(FS13_FILE) {
        Some(s) => s,
        None => {
            println("app: FS13 stat after GC FAILED");
            return;
        }
    };
    if fs13_perm(st13b.mode) != 0o600 || st13b.ctime < FS13_EPOCH_FLOOR {
        println("app: FS13 meta lost after GC FAILED");
        return;
    }

    // truncate 截短: 12 KiB -> 4 KiB。保留页内容不变, 越过新末尾读到 0 字节。
    let fd13 = vfs::open(FS13_FILE);
    if fd13 == u64::MAX || vfs::truncate(fd13, 4096) != 1 {
        println("app: FS13 truncate down FAILED");
        vfs::close(fd13);
        return;
    }
    if !fs11_verify_pages(fd13, 0, 1, 0x50) {
        println("app: FS13 kept page FAILED");
        vfs::close(fd13);
        return;
    }
    if vfs::read(fd13, 4096, 512) != 0 {
        println("app: FS13 read past new end FAILED");
        vfs::close(fd13);
        return;
    }
    // truncate 扩展: 4 KiB -> 8 KiB, 新区域必须是稀疏的 0。
    if vfs::truncate(fd13, 8192) != 1 {
        println("app: FS13 truncate up FAILED");
        vfs::close(fd13);
        return;
    }
    vfs::close(fd13);
    let st13c = match fs13_stat(FS13_FILE) {
        Some(s) => s,
        None => {
            println("app: FS13 stat after truncate FAILED");
            return;
        }
    };
    if st13c.size != 8192 {
        println("app: FS13 sparse size FAILED");
        return;
    }
    let fd13 = vfs::open(FS13_FILE);
    if fd13 == u64::MAX || !fs13_all_zero(fd13, 4096, 4096) {
        println("app: FS13 sparse zeros FAILED");
        vfs::close(fd13);
        return;
    }
    vfs::close(fd13);
    // 截到 0 再写一页, 给后面的 rename 测试准备可辨识内容。
    let fd13 = vfs::open(FS13_FILE);
    if fd13 == u64::MAX || vfs::truncate(fd13, 0) != 1 || !fs11_write_pages(fd13, 0, 1, 0x60) {
        println("app: FS13 truncate zero FAILED");
        vfs::close(fd13);
        return;
    }
    vfs::close(fd13);

    // rename 跨目录: /mfs/M5/A.TXT -> /mfs/M5/SUB/B.TXT (移动的 inode 保持原 mode)。
    if vfs::rename(FS13_FILE, FS13_MOVED) != 1 {
        println("app: FS13 rename FAILED");
        return;
    }
    if vfs::open(FS13_FILE) != u64::MAX {
        println("app: FS13 old name still there FAILED");
        return;
    }
    let moved = vfs::open(FS13_MOVED);
    if moved == u64::MAX {
        println("app: FS13 rename target missing FAILED");
        return;
    }
    if !fs11_verify_pages(moved, 0, 1, 0x60) {
        println("app: FS13 renamed content FAILED");
        vfs::close(moved);
        return;
    }
    vfs::close(moved);
    if fs13_stat(FS13_MOVED).map(|s| (fs13_perm(s.mode), s.size)) != Some((0o600, 4096)) {
        println("app: FS13 renamed meta FAILED");
        return;
    }

    // rename 目录 + 拒绝把目录移进自己的子孙。
    if vfs::rename(FS13_SUB, "/mfs/M5/SUB2") != 1 {
        println("app: FS13 rename dir FAILED");
        return;
    }
    if vfs::open("/mfs/M5/SUB2/B.TXT") == u64::MAX {
        println("app: FS13 renamed dir child FAILED");
        return;
    }
    if vfs::mkdir("/mfs/M5/SUB2/INNER") != 1 {
        println("app: FS13 mkdir inner FAILED");
        return;
    }
    if vfs::rename("/mfs/M5/SUB2", "/mfs/M5/SUB2/INNER/X") != u64::MAX {
        println("app: FS13 dir-into-own-descendant allowed FAILED");
        return;
    }

    // rename 覆盖已存在文件: 目标内容应变成源的 (0x60), 源名字消失。
    let dstfd = vfs::creat(FS13_DST);
    if dstfd == u64::MAX || !fs11_write_pages(dstfd, 0, 1, 0x70) {
        println("app: FS13 overwrite prep FAILED");
        return;
    }
    vfs::close(dstfd);
    if vfs::rename("/mfs/M5/SUB2/B.TXT", FS13_DST) != 1 {
        println("app: FS13 overwrite rename FAILED");
        return;
    }
    if vfs::open("/mfs/M5/SUB2/B.TXT") != u64::MAX {
        println("app: FS13 overwrite src still there FAILED");
        return;
    }
    let ovw = vfs::open(FS13_DST);
    if ovw == u64::MAX {
        println("app: FS13 overwrite target missing FAILED");
        return;
    }
    let ok_ovw = fs11_verify_pages(ovw, 0, 1, 0x60);
    vfs::close(ovw);
    if !ok_ovw {
        println("app: FS13 overwrite content FAILED");
        return;
    }

    // readdir 长格式所需字段: 目录条目的 mode / mtime 也要有值。
    let dfd13 = vfs::open(FS13_DIR);
    if dfd13 == u64::MAX {
        println("app: FS13 open dir FAILED");
        return;
    }
    let dn13 = vfs::readdir(dfd13);
    vfs::close(dfd13);
    if dn13 == u64::MAX {
        println("app: FS13 readdir FAILED");
        return;
    }
    let dent13 = dn13 as usize / core::mem::size_of::<vfs::DirEntry>();
    let dlist13 =
        unsafe { core::slice::from_raw_parts(vfs::RESULT_BUF as *const vfs::DirEntry, dent13) };
    if dent13 != 2
        || !dlist13.iter().all(|de| {
            fs13_perm(de.mode) != 0 && de.mtime >= FS13_EPOCH_FLOOR && de.owner == APP_DOMAIN as u16
        })
    {
        println("app: FS13 readdir meta FAILED");
        return;
    }

    // GC + 分配扰动后重读: 元数据与内容都不受影响。
    if vfs::mfs_gc() == u64::MAX {
        println("app: FS13 GC FAILED");
        return;
    }
    let chfd13 = vfs::creat("/mfs/CHURN5.BIN");
    if chfd13 == u64::MAX || !fs11_write_pages(chfd13, 0, 8, 0xA0) {
        println("app: FS13 churn FAILED");
        return;
    }
    vfs::close(chfd13);
    let after = vfs::open(FS13_DST);
    if after == u64::MAX {
        println("app: FS13 reopen after GC FAILED");
        return;
    }
    let ok_after = fs11_verify_pages(after, 0, 1, 0x60);
    vfs::close(after);
    if !ok_after || fs13_stat(FS13_DST).map(|s| fs13_perm(s.mode)) != Some(0o600) {
        println("app: FS13 content after GC FAILED");
        return;
    }

    // 清理: 自叶向上删目录, 再删文件与扰动文件, 最后回收。
    if vfs::rmdir("/mfs/M5/SUB2/INNER") != 1
        || vfs::rmdir("/mfs/M5/SUB2") != 1
        || vfs::unlink(FS13_DST) != 1
        || vfs::rmdir(FS13_DIR) != 1
        || vfs::unlink("/mfs/CHURN5.BIN") != 1
    {
        println("app: FS13 cleanup FAILED");
        return;
    }
    if vfs::mfs_gc() == u64::MAX {
        println("app: FS13 final GC FAILED");
    }

    // 17. FS-14 自测 (阶段 D/M5b): inode 号间接层 + 硬链接。
    //     核心断言只有一条: **从任一个名字改写文件, 另一个名字立刻看到新内容** ——
    //     旧实现 (目录项直接存块号) 在这一步会把两个名字的内容写分叉。
    //     另外覆盖 nlink 计数、摘掉一个名字后数据仍在、目录不可链接、GC 后一致性。
    const FS14_A: &str = "/mfs/L1.TXT";
    const FS14_B: &str = "/mfs/L2.TXT";
    const FS14_C: &str = "/mfs/L3.TXT";
    const FS14_D: &str = "/mfs/L4.TXT";
    const FS14_DIR: &str = "/mfs/DIR14";

    let fd = vfs::creat(FS14_A);
    if fd == u64::MAX || !fs11_write_pages(fd, 0, 1, 0x30) {
        println("app: FS14 create FAILED");
        return;
    }
    vfs::close(fd);
    // 建第二个名字。
    if vfs::link(FS14_A, FS14_B) != 1 {
        println("app: FS14 link FAILED");
        return;
    }
    // 两个名字必须看到同一份元数据 (同一个 inode)。
    let sa = match fs13_stat(FS14_A) {
        Some(s) => s,
        None => {
            println("app: FS14 stat A FAILED");
            return;
        }
    };
    let sb = match fs13_stat(FS14_B) {
        Some(s) => s,
        None => {
            println("app: FS14 stat B FAILED");
            return;
        }
    };
    if sa.nlink != 2 || sb.nlink != 2 || sa.size != 4096 || fs13_perm(sa.mode) != 0o644 {
        println("app: FS14 nlink FAILED");
        return;
    }
    if (sa.mtime, sa.owner) != (sb.mtime, sb.owner) {
        println("app: FS14 shared inode FAILED");
        return;
    }
    // 从 L2 改写, L1 必须看到新内容 (硬链接的关键语义)。
    let fd = vfs::open(FS14_B);
    if fd == u64::MAX || !fs11_write_pages(fd, 0, 1, 0x40) {
        println("app: FS14 write via B FAILED");
        vfs::close(fd);
        return;
    }
    vfs::close(fd);
    let fd = vfs::open(FS14_A);
    if fd == u64::MAX {
        println("app: FS14 reopen A FAILED");
        return;
    }
    let ok = fs11_verify_pages(fd, 0, 1, 0x40);
    vfs::close(fd);
    if !ok {
        println("app: FS14 write-through-link FAILED");
        return;
    }
    // GC + 分配扰动后两个名字仍指向同一份数据。
    if vfs::mfs_gc() == u64::MAX {
        println("app: FS14 GC FAILED");
        return;
    }
    let churn = vfs::creat("/mfs/CHURN6.BIN");
    if churn == u64::MAX || !fs11_write_pages(churn, 0, 8, 0xB0) {
        println("app: FS14 churn FAILED");
        return;
    }
    vfs::close(churn);
    let fd = vfs::open(FS14_B);
    if fd == u64::MAX {
        println("app: FS14 reopen B after GC FAILED");
        return;
    }
    let ok = fs11_verify_pages(fd, 0, 1, 0x40);
    vfs::close(fd);
    if !ok || fs13_stat(FS14_A).map(|s| s.nlink) != Some(2) {
        println("app: FS14 after GC FAILED");
        return;
    }
    // 摘掉一个名字: 数据必须还在 (nlink 减到 1)。
    if vfs::unlink(FS14_B) != 1 {
        println("app: FS14 unlink B FAILED");
        return;
    }
    if vfs::open(FS14_B) != u64::MAX {
        println("app: FS14 B still there FAILED");
        return;
    }
    let fd = vfs::open(FS14_A);
    if fd == u64::MAX {
        println("app: FS14 A lost after unlink B FAILED");
        return;
    }
    let ok = fs11_verify_pages(fd, 0, 1, 0x40);
    vfs::close(fd);
    if !ok || fs13_stat(FS14_A).map(|s| s.nlink) != Some(1) {
        println("app: FS14 survive unlink FAILED");
        return;
    }
    // 再链一次, 这次摘掉"原来的名字", 剩余名字仍然可用; 顺带验证改名后仍可读。
    if vfs::link(FS14_A, FS14_C) != 1 || vfs::unlink(FS14_A) != 1 {
        println("app: FS14 relink FAILED");
        return;
    }
    if vfs::rename(FS14_C, FS14_D) != 1 {
        println("app: FS14 rename linked FAILED");
        return;
    }
    let fd = vfs::open(FS14_D);
    if fd == u64::MAX {
        println("app: FS14 D missing FAILED");
        return;
    }
    let ok = fs11_verify_pages(fd, 0, 1, 0x40);
    vfs::close(fd);
    if !ok {
        println("app: FS14 content after rename FAILED");
        return;
    }
    // 负向: 目录不能硬链接; 目标已存在 / 源不存在都必须失败。
    if vfs::mkdir(FS14_DIR) != 1 {
        println("app: FS14 mkdir FAILED");
        return;
    }
    if vfs::link(FS14_DIR, "/mfs/DIR14B") != u64::MAX {
        println("app: FS14 dir link allowed FAILED");
        return;
    }
    if vfs::link(FS14_D, FS14_D) != u64::MAX {
        println("app: FS14 link onto existing allowed FAILED");
        return;
    }
    if vfs::link("/mfs/NOPE14", "/mfs/NOPE14B") != u64::MAX {
        println("app: FS14 link missing src allowed FAILED");
        return;
    }
    // 清理: 最后一个名字摘掉后 inode 槽释放, 块由 GC 回收。
    if vfs::unlink(FS14_D) != 1 || vfs::rmdir(FS14_DIR) != 1 || vfs::unlink("/mfs/CHURN6.BIN") != 1
    {
        println("app: FS14 cleanup FAILED");
        return;
    }
    if vfs::mfs_gc() == u64::MAX {
        println("app: FS14 final GC FAILED");
    }

    // 18. FS-15 自测 (阶段 D/M6a): exFAT 只读兼容。
    //     宿主 `mkfs.exfat` 预格式化的卷 (nsid 5) 自动挂载于 /usb, 服务必须完成:
    //     引导扇区 + boot checksum 校验 → 系统项 (0x81/0x82) 解析 → 位图/upcase 载入。
    //     这里只要求列举成功 (系统项 / 卷标不会被当成文件) 与类型正确;
    //     「卷已清空」的强校验放在 FS-16 收尾 (那里刚删掉自己创建的对象)。
    let efd15 = vfs::open("/usb");
    if efd15 == u64::MAX {
        println("app: FS15 open /usb FAILED");
        return;
    }
    let rn15 = vfs::readdir(efd15);
    vfs::close(efd15);
    if rn15 == u64::MAX {
        println("app: FS15 readdir /usb FAILED");
        return;
    }
    if !(rn15 as usize).is_multiple_of(core::mem::size_of::<vfs::DirEntry>()) {
        println("app: FS15 listing not whole entries FAILED");
        return;
    }
    {
        if vfs::stat("/usb") == u64::MAX {
            println("app: FS15 stat /usb FAILED");
            return;
        }
        let st = unsafe { core::ptr::read_unaligned(vfs::RESULT_BUF as *const vfs::Stat) };
        if st.is_dir != 1 {
            println("app: FS15 /usb not dir FAILED");
            return;
        }
    }
    if vfs::open("/usb/NOPE.TXT") != u64::MAX {
        println("app: FS15 missing path NOT rejected FAILED");
    }

    // 19. FS-16 自测 (阶段 D/M6b): exFAT 读写。
    //     覆盖 creat/write/读回/stat/mkdir/readdir/rmdir(非空拒绝)/truncate(缩+扩)/unlink
    //     全链路, 并在末尾把卷清空 —— 下一次启动的 FS-15 因此仍看到空卷。
    //     写入 100000 字节: 4 KiB 簇下跨 25 簇、32 KiB 簇下跨 4 簇, 都能验证簇链扩展;
    //     建 45 个文件以验证目录块扩容 (4 KiB 簇下 135 条目 > 单簇 128 项)。
    {
        // 幂等: 先清掉上次可能残留的自测对象。
        vfs::unlink("/usb/D16/A.TXT");
        vfs::rmdir("/usb/D16");
        vfs::unlink("/usb/FS16.TXT");
        let pat = |i: usize| (i % 251) as u8;

        // --- 写 100000 字节 ---
        let fd = vfs::creat("/usb/FS16.TXT");
        if fd == u64::MAX {
            println("app: FS16 creat FAILED");
            return;
        }
        let mut buf = [0u8; 4096];
        let total = 100_000usize;
        let mut written = 0usize;
        while written < total {
            let n = (total - written).min(buf.len());
            let mut i = 0usize;
            while i < n {
                buf[i] = pat(written + i);
                i += 1;
            }
            if vfs::write(fd, written as u64, &buf[..n]) != n as u64 {
                println("app: FS16 write FAILED");
                return;
            }
            written += n;
        }
        vfs::close(fd);
        if vfs::stat("/usb/FS16.TXT") == u64::MAX {
            println("app: FS16 stat FAILED");
            return;
        }
        let st = unsafe { core::ptr::read_unaligned(vfs::RESULT_BUF as *const vfs::Stat) };
        if st.size as usize != total {
            println("app: FS16 size mismatch FAILED");
            return;
        }
        let rfd = vfs::open("/usb/FS16.TXT");
        if rfd == u64::MAX {
            println("app: FS16 reopen FAILED");
            return;
        }
        let mut off = 0usize;
        while off < total {
            let n = (total - off).min(4096);
            if vfs::read(rfd, off as u64, n as u32) != n as u64 {
                println("app: FS16 read back FAILED");
                return;
            }
            let r = unsafe { core::slice::from_raw_parts(vfs::RESULT_BUF as *const u8, n) };
            let mut i = 0usize;
            while i < n {
                if r[i] != pat(off + i) {
                    println("app: FS16 content mismatch FAILED");
                    return;
                }
                i += 1;
            }
            off += n;
        }

        // --- 截短到 1000 (保留首簇前缀, 释放第二簇) ---
        if vfs::truncate(rfd, 1000) == u64::MAX {
            println("app: FS16 truncate down FAILED");
            return;
        }
        if vfs::read(rfd, 900, 200) != 100 {
            println("app: FS16 read after shrink FAILED");
            return;
        }
        {
            let r = unsafe { core::slice::from_raw_parts(vfs::RESULT_BUF as *const u8, 100) };
            let mut i = 0usize;
            while i < 100 {
                if r[i] != pat(900 + i) {
                    println("app: FS16 shrink kept prefix FAILED");
                    return;
                }
                i += 1;
            }
        }
        // --- 扩展回 100000 (新区必须读到 0, exFAT 无稀疏) ---
        if vfs::truncate(rfd, 100_000) == u64::MAX {
            println("app: FS16 truncate up FAILED");
            return;
        }
        if vfs::read(rfd, 1000, 200) != 200 {
            println("app: FS16 read after grow FAILED");
            return;
        }
        {
            let r = unsafe { core::slice::from_raw_parts(vfs::RESULT_BUF as *const u8, 200) };
            let mut i = 0usize;
            while i < 200 {
                if r[i] != 0 {
                    println("app: FS16 grow not zero FAILED");
                    return;
                }
                i += 1;
            }
        }
        vfs::close(rfd);

        // --- 目录: 建 / 列举 / 非空 rmdir 被拒 / unlink 目录被拒 ---
        if vfs::mkdir("/usb/D16") == u64::MAX {
            println("app: FS16 mkdir FAILED");
            return;
        }
        let dfd = vfs::open("/usb/D16");
        if dfd == u64::MAX {
            println("app: FS16 open dir FAILED");
            return;
        }
        if vfs::readdir(dfd) != 0 {
            println("app: FS16 new dir not empty FAILED");
            return;
        }
        vfs::close(dfd);
        let afd = vfs::creat("/usb/D16/A.TXT");
        if afd == u64::MAX || vfs::write(afd, 0, b"hi") != 2 {
            println("app: FS16 write in dir FAILED");
            return;
        }
        vfs::close(afd);
        let rfd2 = vfs::open("/usb/D16/A.TXT");
        if rfd2 == u64::MAX || vfs::read(rfd2, 0, 16) != 2 {
            println("app: FS16 read in dir FAILED");
            return;
        }
        vfs::close(rfd2);
        if vfs::rmdir("/usb/D16") != u64::MAX {
            println("app: FS16 rmdir non-empty NOT rejected FAILED");
            return;
        }
        if vfs::unlink("/usb/D16") != u64::MAX {
            println("app: FS16 unlink dir NOT rejected FAILED");
            return;
        }

        // --- 目录扩容: 45 个 5 字符名 → 135 个条目 > 单簇 128 项 ---
        let mut k = 0usize;
        while k < 45 {
            let (a, b) = (b'0' + (k / 10) as u8, b'0' + (k % 10) as u8);
            let mut nm = [0u8; 16];
            nm[..5].copy_from_slice(b"/usb/");
            nm[5] = b'X';
            nm[6] = a;
            nm[7] = b;
            nm[8] = b'.';
            nm[9] = b'T';
            nm[10] = b'X';
            nm[11] = b'T';
            let p = unsafe { core::str::from_utf8_unchecked(&nm[..12]) };
            let f = vfs::creat(p);
            if f == u64::MAX {
                println("app: FS16 bulk creat FAILED");
                return;
            }
            vfs::close(f);
            k += 1;
        }
        let dfd2 = vfs::open("/usb");
        let n2 = vfs::readdir(dfd2);
        vfs::close(dfd2);
        // 45 个文件 + 1 个目录远超一页条目上限, 必须正好写满结果页。
        if n2 == u64::MAX
            || n2 as usize / core::mem::size_of::<vfs::DirEntry>() != vfs::RESULT_MAX_ENTRIES
        {
            println("app: FS16 bulk listing FAILED");
            return;
        }

        // --- 清理 (先摘名字再核对空卷) ---
        if vfs::unlink("/usb/D16/A.TXT") == u64::MAX || vfs::rmdir("/usb/D16") == u64::MAX {
            println("app: FS16 cleanup dir FAILED");
            return;
        }
        let mut k = 0usize;
        while k < 45 {
            let (a, b) = (b'0' + (k / 10) as u8, b'0' + (k % 10) as u8);
            let mut nm = [0u8; 16];
            nm[..5].copy_from_slice(b"/usb/");
            nm[5] = b'X';
            nm[6] = a;
            nm[7] = b;
            nm[8] = b'.';
            nm[9] = b'T';
            nm[10] = b'X';
            nm[11] = b'T';
            let p = unsafe { core::str::from_utf8_unchecked(&nm[..12]) };
            if vfs::unlink(p) == u64::MAX {
                println("app: FS16 bulk unlink FAILED");
                return;
            }
            k += 1;
        }
        if vfs::unlink("/usb/FS16.TXT") == u64::MAX {
            println("app: FS16 cleanup file FAILED");
            return;
        }
        let efd = vfs::open("/usb");
        let left = vfs::readdir(efd);
        vfs::close(efd);
        if left != 0 {
            println("app: FS16 cleanup left entries FAILED");
            return;
        }
    }

    // 20. FS-17 自测 (阶段 D/M1b): **额外卷**自动挂载。
    //     分区测试盘 (nsid 4) 上的 FAT32 / ext2 分区都不是「第一个匹配卷」, 卷层按
    //     M1b 把它们作为额外卷挂到 `/usb<卷号>`, 由同一个文件服务**按卷切换几何**来
    //     服务。这里验证:
    //       - 卷表里能查到这两个分区的卷号, 并据此拼出挂载点;
    //       - FAT32 分区可打开目录, 且能读出宿主预置的 part1.txt;
    //       - ext2 分区可读出 part2.txt;
    //     全程只读: 不向额外卷写任何数据。
    {
        let nvol = block_list_volumes(vfs::RESULT_BUF as *mut u8, 16);
        if nvol == u64::MAX {
            println("app: FS17 list volumes FAILED");
            return;
        }
        let mut fat_vol = u64::MAX;
        let mut ext_vol = u64::MAX;
        let mut i = 0u64;
        while i < nvol {
            let d = vol_desc(vfs::RESULT_BUF as *const u8, i as usize);
            if d.nsid == 4 && d.start_lba == 2048 && d.kind == VOL_KIND_FAT {
                fat_vol = d.id as u64;
            }
            if d.nsid == 4 && d.start_lba == 34816 && d.kind == VOL_KIND_EXT2 {
                ext_vol = d.id as u64;
            }
            i += 1;
        }
        if fat_vol == u64::MAX || ext_vol == u64::MAX {
            println("app: FS17 partition volumes missing FAILED");
            return;
        }

        // 额外卷的挂载点 = `/usb<卷号>` (与 mount_srv 的命名规则一致)。
        let mut base = [0u8; 8];
        base[..4].copy_from_slice(b"/usb");
        let mut path = [0u8; 32];
        path[..4].copy_from_slice(b"/usb");

        // --- FAT32 分区: 目录可打开, 且能读出宿主 mcopy 预置的 part1.txt ---
        let dn = dec_to_str(fat_vol, &mut base[4..]);
        let root = unsafe { core::str::from_utf8_unchecked(&base[..4 + dn]) };
        let dfd = vfs::open(root);
        if dfd == u64::MAX {
            println("app: FS17 open extra FAT volume FAILED");
            return;
        }
        vfs::close(dfd);
        path[4..4 + dn].copy_from_slice(&base[4..4 + dn]);
        path[4 + dn..4 + dn + 10].copy_from_slice(b"/part1.txt");
        let fpath = unsafe { core::str::from_utf8_unchecked(&path[..4 + dn + 10]) };
        let f = vfs::open(fpath);
        if f == u64::MAX {
            println("app: FS17 open part1.txt on extra FAT volume FAILED");
            return;
        }
        let n = vfs::read(f, 0, 4096);
        vfs::close(f);
        if n == u64::MAX || n < 10 {
            println("app: FS17 read part1.txt FAILED");
            return;
        }
        {
            let head = unsafe { core::slice::from_raw_parts(vfs::RESULT_BUF as *const u8, 10) };
            if head != &b"partition "[..] {
                println("app: FS17 part1.txt content FAILED");
                return;
            }
        }

        // --- ext2 分区: 同样读一个宿主预置的文件 ---
        let dn2 = dec_to_str(ext_vol, &mut base[4..]);
        path = [0u8; 32];
        path[..4].copy_from_slice(b"/usb");
        path[4..4 + dn2].copy_from_slice(&base[4..4 + dn2]);
        path[4 + dn2..4 + dn2 + 10].copy_from_slice(b"/part2.txt");
        let epath = unsafe { core::str::from_utf8_unchecked(&path[..4 + dn2 + 10]) };
        let e = vfs::open(epath);
        if e == u64::MAX {
            println("app: FS17 open part2.txt on extra ext2 volume FAILED");
            return;
        }
        let en = vfs::read(e, 0, 4096);
        vfs::close(e);
        if en == u64::MAX || en < 10 {
            println("app: FS17 read part2.txt FAILED");
            return;
        }
        {
            let head = unsafe { core::slice::from_raw_parts(vfs::RESULT_BUF as *const u8, 10) };
            if head != &b"partition "[..] {
                println("app: FS17 part2.txt content FAILED");
                return;
            }
        }
    }

    // 21. FS-18 自测: fat32 大簇写路径 (跨簇写入 + 读回 + 删除释放)。
    //     fat32 没有 truncate, 故重点覆盖 WRITE 触发簇链扩展、逐簇读回、UNLINK 释放
    //     簇链这三条「目录项增删 / 簇分配 / FAT 链维护」的写路径。写入 100000 字节:
    //     512 B 簇下跨 196 簇、32 KiB 簇下跨 4 簇 —— 无论哪种几何都会真实跨簇。
    //     这一条是 fat32 写路径里**唯一**的大文件用例, 且能在 `NVME_CLU=64` 造出的
    //     32 KiB 簇镜像上验证 M1b 大簇写路径。
    {
        // 幂等: 清掉上一轮被中断可能残留的对象。
        vfs::unlink("/FS18.BIN");
        let pat = |i: usize| (i % 251) as u8;
        let total = 100_000usize;

        let fd = vfs::creat("/FS18.BIN");
        if fd == u64::MAX {
            println("app: FS18 creat FAILED");
            return;
        }
        let mut buf = [0u8; 4096];
        let mut written = 0usize;
        while written < total {
            let n = (total - written).min(buf.len());
            let mut i = 0usize;
            while i < n {
                buf[i] = pat(written + i);
                i += 1;
            }
            if vfs::write(fd, written as u64, &buf[..n]) != n as u64 {
                println("app: FS18 write FAILED");
                return;
            }
            written += n;
        }
        vfs::close(fd);

        if vfs::stat("/FS18.BIN") == u64::MAX {
            println("app: FS18 stat FAILED");
            return;
        }
        {
            let st = unsafe { core::ptr::read_unaligned(vfs::RESULT_BUF as *const vfs::Stat) };
            if st.size as usize != total {
                println("app: FS18 size mismatch FAILED");
                return;
            }
        }

        let rfd = vfs::open("/FS18.BIN");
        if rfd == u64::MAX {
            println("app: FS18 reopen FAILED");
            return;
        }
        let mut off = 0usize;
        while off < total {
            let n = (total - off).min(4096);
            if vfs::read(rfd, off as u64, n as u32) != n as u64 {
                println("app: FS18 read back FAILED");
                return;
            }
            let r = unsafe { core::slice::from_raw_parts(vfs::RESULT_BUF as *const u8, n) };
            let mut i = 0usize;
            while i < n {
                if r[i] != pat(off + i) {
                    println("app: FS18 content mismatch FAILED");
                    return;
                }
                i += 1;
            }
            off += n;
        }
        vfs::close(rfd);

        if vfs::unlink("/FS18.BIN") != 1 {
            println("app: FS18 unlink FAILED");
            return;
        }
        if vfs::stat("/FS18.BIN") != u64::MAX {
            println("app: FS18 still present after unlink FAILED");
            return;
        }
    }

    // 22. FS-19 自测 (阶段 D/M5c): 软链接 —— 新节点类型 MFSL + 解析跟随 + 限深防环。
    //     全在 `/mfs` 下做 (软链接只有 MFS 支持)。三类断言:
    //       a) 跟随: 读链接等于读目标 (绝对目标 / 同目录相对目标 / 带 `..` 的相对目标 /
    //          路径**中间**分量是目录链接); `stat` 链接跟随到目标类型 (不是 "symbolic
    //          link"); readdir 里它才是链接 (mode 高位 = LINK, size = 目标串长度)。
    //       b) 不跟随: `rm`/`mv`/`rmdir` 作用于链接自身 —— 摘掉链接后目标内容必须还在,
    //          且 `rmdir <指向目录的链接>` 必须失败 (它不是目录条目)。
    //       c) 防环: 互相指向 / 自指的链接在解析时报错, 不挂死、不无限展开。
    {
        const F19_T: &str = "/mfs/S19T.TXT";
        const F19_D: &str = "/mfs/D19";
        const F19_F: &str = "/mfs/D19/F.TXT";
        // 幂等: /mfs 是持久卷, 清掉上一轮被中断可能残留的对象 (软链接用 unlink 摘)。
        // 顺序: 先摘链接再删目录, 否则目录非空删不掉。
        for p in [
            "/mfs/S19L1",
            "/mfs/S19L2",
            "/mfs/S19L2R",
            "/mfs/D19/S19L3",
            "/mfs/S19LD",
            "/mfs/S19L5",
            "/mfs/C19A",
            "/mfs/C19B",
            "/mfs/C19C",
        ] {
            vfs::unlink(p);
        }
        vfs::unlink(F19_F);
        vfs::rmdir(F19_D);
        vfs::unlink(F19_T);

        // 目标文件 (1 页, 内容标记 0x50)。
        let fd = vfs::creat(F19_T);
        if fd == u64::MAX || !fs11_write_pages(fd, 0, 1, 0x50) {
            println("app: FS19 create target FAILED");
            return;
        }
        vfs::close(fd);

        // (a1) 绝对目标: 读链接 == 读目标。
        if vfs::symlink("/mfs/S19T.TXT", "/mfs/S19L1") != 1 {
            println("app: FS19 symlink abs FAILED");
            return;
        }
        let lfd = vfs::open("/mfs/S19L1");
        if lfd == u64::MAX {
            println("app: FS19 open through abs link FAILED");
            return;
        }
        let ok = fs11_verify_pages(lfd, 0, 1, 0x50);
        vfs::close(lfd);
        if !ok {
            println("app: FS19 read through abs link FAILED");
            return;
        }

        // (a2) stat 跟随: 报的是**目标**的类型与大小, 不是链接。
        match fs13_stat("/mfs/S19L1") {
            Some(st)
                if st.size == 4096
                    && st.is_dir == 0
                    && st.mode & vfs::MODE_FTYPE_MASK == vfs::MODE_FTYPE_FILE => {}
            _ => {
                println("app: FS19 stat through link FAILED");
                return;
            }
        }

        // (a3) readdir 看到的是**链接本身**: 类型位 = LINK, size = 目标串长度。
        //      存下的是服务命名空间里的目标: 客户端已把挂载前缀 `/mfs` 剥掉,
        //      "/mfs/S19T.TXT" -> "/S19T.TXT" (9 字节)。
        match fs19_entry("/mfs", "S19L1") {
            Some(de)
                if de.mode & vfs::MODE_FTYPE_MASK == vfs::MODE_FTYPE_LINK
                    && de.is_dir == 0
                    && de.size == 9 => {}
            _ => {
                println("app: FS19 readdir link entry FAILED");
                return;
            }
        }

        // (a4) 同目录相对目标 ("S19T.TXT" 相对链接所在目录 /mfs)。
        if vfs::symlink("S19T.TXT", "/mfs/S19L2") != 1 {
            println("app: FS19 symlink rel FAILED");
            return;
        }
        let lfd = vfs::open("/mfs/S19L2");
        if lfd == u64::MAX {
            println("app: FS19 open through rel link FAILED");
            return;
        }
        let ok = fs11_verify_pages(lfd, 0, 1, 0x50);
        vfs::close(lfd);
        if !ok {
            println("app: FS19 read through rel link FAILED");
            return;
        }

        // (a5) 带 `..` 的相对目标: 链接在 /mfs/D19 里, 目标 "../D19/F.TXT"。
        //      相对基准是**链接所在目录** (/mfs/D19), 展开后为 /mfs/D19/../D19/F.TXT,
        //      必须重新规范化成 /mfs/D19/F.TXT 才找得到。
        if vfs::mkdir(F19_D) != 1 {
            println("app: FS19 mkdir D19 FAILED");
            return;
        }
        let fd = vfs::creat(F19_F);
        if fd == u64::MAX || !fs11_write_pages(fd, 0, 1, 0x60) {
            println("app: FS19 create D19/F FAILED");
            return;
        }
        vfs::close(fd);
        if vfs::symlink("../D19/F.TXT", "/mfs/D19/S19L3") != 1 {
            println("app: FS19 symlink dotdot FAILED");
            return;
        }
        let lfd = vfs::open("/mfs/D19/S19L3");
        if lfd == u64::MAX {
            println("app: FS19 open through dotdot link FAILED");
            return;
        }
        let ok = fs11_verify_pages(lfd, 0, 1, 0x60);
        vfs::close(lfd);
        if !ok {
            println("app: FS19 dotdot link target FAILED");
            return;
        }

        // (a6) **中间分量**是目录链接: /mfs/S19LD -> /mfs/D19, 打开 /mfs/S19LD/F.TXT。
        if vfs::symlink("/mfs/D19", "/mfs/S19LD") != 1 {
            println("app: FS19 symlink dir FAILED");
            return;
        }
        let lfd = vfs::open("/mfs/S19LD/F.TXT");
        if lfd == u64::MAX {
            println("app: FS19 open via dir link FAILED");
            return;
        }
        let ok = fs11_verify_pages(lfd, 0, 1, 0x60);
        vfs::close(lfd);
        if !ok {
            println("app: FS19 dir-link traversal FAILED");
            return;
        }

        // (b1) rmdir 一个**指向目录的链接**必须失败 (它不是目录条目, 不能跟随)。
        if vfs::rmdir("/mfs/S19LD") != u64::MAX {
            println("app: FS19 rmdir dir-link should FAIL");
            return;
        }
        // (b2) rm 摘掉链接; 目标目录与其中的文件都不受影响。
        if vfs::unlink("/mfs/S19LD") != 1 {
            println("app: FS19 unlink dir-link FAILED");
            return;
        }
        if vfs::stat("/mfs/D19/F.TXT") == u64::MAX {
            println("app: FS19 target removed by link unlink FAILED");
            return;
        }

        // (b3) 摘掉链接后, 目标文件内容必须原封不动。
        if vfs::unlink("/mfs/S19L1") != 1 {
            println("app: FS19 unlink link FAILED");
            return;
        }
        let tfd = vfs::open(F19_T);
        if tfd == u64::MAX {
            println("app: FS19 target gone after unlink FAILED");
            return;
        }
        let ok = fs11_verify_pages(tfd, 0, 1, 0x50);
        vfs::close(tfd);
        if !ok {
            println("app: FS19 unlink touched target FAILED");
            return;
        }
        // 链接已不存在: 经它打开必须失败。
        if vfs::open("/mfs/S19L1") != u64::MAX {
            println("app: FS19 link still resolvable FAILED");
            return;
        }

        // (b4) mv 移动的是链接自身 (不跟随): 改名后仍是链接, 且仍能读到目标。
        if vfs::rename("/mfs/S19L2", "/mfs/S19L2R") != 1 {
            println("app: FS19 rename link FAILED");
            return;
        }
        match fs19_entry("/mfs", "S19L2R") {
            Some(de) if de.mode & vfs::MODE_FTYPE_MASK == vfs::MODE_FTYPE_LINK => {}
            _ => {
                println("app: FS19 link type lost after rename FAILED");
                return;
            }
        }
        let lfd = vfs::open("/mfs/S19L2R");
        if lfd == u64::MAX {
            println("app: FS19 open renamed link FAILED");
            return;
        }
        let ok = fs11_verify_pages(lfd, 0, 1, 0x50);
        vfs::close(lfd);
        if !ok {
            println("app: FS19 renamed link target FAILED");
            return;
        }

        // (c1) 悬空链接: 建得出来, 但解析 (open) 失败; 条目本身仍在 (不能再建同名)。
        if vfs::symlink("/mfs/S19NOPE.TXT", "/mfs/S19L5") != 1 {
            println("app: FS19 symlink dangling FAILED");
            return;
        }
        if vfs::open("/mfs/S19L5") != u64::MAX {
            println("app: FS19 dangling link should FAIL to open");
            return;
        }
        if vfs::mkdir("/mfs/S19L5") != u64::MAX {
            println("app: FS19 dangling link name should be taken");
            return;
        }
        if vfs::unlink("/mfs/S19L5") != 1 {
            println("app: FS19 unlink dangling link FAILED");
            return;
        }

        // (c2) 互相指向 C19A <-> C19B: 解析必须失败 (限深), 且不能挂死。
        if vfs::symlink("/mfs/C19B", "/mfs/C19A") != 1
            || vfs::symlink("/mfs/C19A", "/mfs/C19B") != 1
        {
            println("app: FS19 symlink cycle setup FAILED");
            return;
        }
        if vfs::open("/mfs/C19A") != u64::MAX || vfs::open("/mfs/C19B") != u64::MAX {
            println("app: FS19 link cycle should FAIL to resolve");
            return;
        }
        // (c3) 自指链接。
        if vfs::symlink("/mfs/C19C", "/mfs/C19C") != 1 {
            println("app: FS19 symlink self FAILED");
            return;
        }
        if vfs::open("/mfs/C19C") != u64::MAX {
            println("app: FS19 self link should FAIL to resolve");
            return;
        }

        // 清理 (幂等准备里同样的清单)。
        vfs::unlink("/mfs/S19L2R");
        vfs::unlink("/mfs/D19/S19L3");
        vfs::unlink("/mfs/C19A");
        vfs::unlink("/mfs/C19B");
        vfs::unlink("/mfs/C19C");
        vfs::unlink(F19_F);
        vfs::rmdir(F19_D);
        vfs::unlink(F19_T);
    }

    // 23. FS-20 自测 (阶段 D/M5c 配套): `readlink` + `lstat`。
    //     补上 M5c 落地时留下的两个缺口: 界面看不到链接指向哪里 (只有 readlink 能看),
    //     以及悬空链接根本无法 stat (`stat` 一律跟随)。
    {
        const F20_T: &str = "/mfs/S20T.TXT";
        const F20_L1: &str = "/mfs/S20L1";
        const F20_L2: &str = "/mfs/S20L2";
        const F20_L3: &str = "/mfs/S20L3";
        const F20_L5: &str = "/mfs/S20L5";
        // 幂等准备。
        for p in [F20_L1, F20_L2, F20_L3, F20_L5, "/mfs/S20L4"] {
            vfs::unlink(p);
        }
        vfs::unlink(F20_T);

        let fd = vfs::creat(F20_T);
        if fd == u64::MAX || !fs11_write_pages(fd, 0, 1, 0x70) {
            println("app: FS20 create target FAILED");
            return;
        }
        vfs::close(fd);

        // (a) readlink 与 `ln -s` 的输入**逐字节往返**: 绝对目标原样回来
        //     (服务端存的是剥掉挂载前缀的 `/S20T.TXT`, 客户端把前缀加回去)。
        if vfs::symlink("/mfs/S20T.TXT", F20_L1) != 1 {
            println("app: FS20 symlink abs FAILED");
            return;
        }
        if !fs20_readlink_is(F20_L1, "/mfs/S20T.TXT") {
            println("app: FS20 readlink abs FAILED");
            return;
        }
        // (b) 相对目标**不加**前缀 (它相对链接所在目录, 与服务命名空间无关)。
        if vfs::symlink("S20T.TXT", F20_L2) != 1 {
            println("app: FS20 symlink rel FAILED");
            return;
        }
        if !fs20_readlink_is(F20_L2, "S20T.TXT") {
            println("app: FS20 readlink rel FAILED");
            return;
        }
        // (c) 带 `..` 的相对目标同样原样返回。
        if vfs::symlink("../S20T.TXT", F20_L5) != 1 {
            println("app: FS20 symlink dotdot FAILED");
            return;
        }
        if !fs20_readlink_is(F20_L5, "../S20T.TXT") {
            println("app: FS20 readlink dotdot FAILED");
            return;
        }

        // (d) lstat 看**链接自身**: 类型位 = LINK, size = 目标串长度 (存的是 `/S20T.TXT`
        //     —— 9 字节, 挂载前缀已剥掉); 同一路径的 stat 则跟随到目标 (普通文件 / 4096)。
        match fs20_lstat(F20_L1) {
            Some(st)
                if st.mode & vfs::MODE_FTYPE_MASK == vfs::MODE_FTYPE_LINK
                    && st.is_dir == 0
                    && st.size == 9 => {}
            _ => {
                println("app: FS20 lstat link FAILED");
                return;
            }
        }
        match fs13_stat(F20_L1) {
            Some(st)
                if st.mode & vfs::MODE_FTYPE_MASK == vfs::MODE_FTYPE_FILE && st.size == 4096 => {}
            _ => {
                println("app: FS20 stat-through-link FAILED");
                return;
            }
        }

        // (e) 悬空链接: `stat` 必然失败 (跟随不到), 而 `lstat` / `readlink` 都正常 ——
        //     这正是 `lstat` 存在的意义。
        if vfs::symlink("/mfs/S20NOPE.TXT", F20_L3) != 1 {
            println("app: FS20 symlink dangling FAILED");
            return;
        }
        if vfs::stat(F20_L3) != u64::MAX {
            println("app: FS20 stat dangling should FAIL");
            return;
        }
        if !fs20_readlink_is(F20_L3, "/mfs/S20NOPE.TXT") {
            println("app: FS20 readlink dangling FAILED");
            return;
        }
        match fs20_lstat(F20_L3) {
            // "/S20NOPE.TXT" = 12 字节。
            Some(st) if st.mode & vfs::MODE_FTYPE_MASK == vfs::MODE_FTYPE_LINK && st.size == 12 => {
            }
            _ => {
                println("app: FS20 lstat dangling FAILED");
                return;
            }
        }

        // (f) 非软链接上 readlink 必须失败 (普通文件 / 目录都不行)。
        if vfs::readlink(F20_T) != u64::MAX || vfs::readlink("/mfs") != u64::MAX {
            println("app: FS20 readlink non-link should FAIL");
            return;
        }
        // (g) lstat 对普通文件 / 目录与 stat 等价。
        match fs20_lstat(F20_T) {
            Some(st)
                if st.mode & vfs::MODE_FTYPE_MASK == vfs::MODE_FTYPE_FILE && st.size == 4096 => {}
            _ => {
                println("app: FS20 lstat file FAILED");
                return;
            }
        }
        match fs20_lstat("/mfs") {
            Some(st) if st.is_dir == 1 && st.size == 0 => {}
            _ => {
                println("app: FS20 lstat dir FAILED");
                return;
            }
        }

        // (h) 跨文件系统的绝对目标**拒绝创建**: 服务端解析不到别的挂载点, 与其留个
        //     静默悬空链接, 不如当场失败 (这正是 readlink 能安全加回前缀的前提 --
        //     绝对目标必然是同一个挂载点内的)。
        if vfs::symlink("/usb/ANY.TXT", "/mfs/S20L4") != u64::MAX
            || vfs::symlink("/tmp/ANY.TXT", "/mfs/S20L4") != u64::MAX
            || vfs::symlink("/nosuchmount/A.TXT", "/mfs/S20L4") != u64::MAX
        {
            println("app: FS20 cross-fs symlink should FAIL");
            return;
        }

        // 清理。
        vfs::unlink(F20_L1);
        vfs::unlink(F20_L2);
        vfs::unlink(F20_L3);
        vfs::unlink(F20_L5);
        vfs::unlink(F20_T);
    }
    // 24. FS-21 自测 (阶段 D/M7): 文件系统**铺满卷** —— 格式化尺寸按卷几何定。
    //     盯的是一个极易回退的默认值: 早先 `mfs_format` 无论卷多大都写死 4096 块
    //     (16 MiB), 于是整块新盘也只会格式出 16 MiB。断言只有一条关系式:
    //       MFS 总块数 × 8 扇区/块 == 它所在卷的 sectors
    //     它同时证明两件事: (a) Identify Namespace 的 NSZE 真填进了卷表 —— 整盘卷
    //     此前 `sectors` 恒为 0(容量未知); (b) 格式化确实按卷几何取尺寸。
    //     MFS7 起位图已外置到独立数据块 (见 FS-23(a)), 上界提到 ≈127.25 GiB
    //     (`MFS_MAX_BLOCKS`), 故 256 MiB 测试卷不再触发 clamp —— 这条等号在测试卷
    //     尺寸下恒成立。
    {
        let usage = vfs::mfs_stat();
        if usage == u64::MAX {
            println("app: FS21 mfs_stat FAILED");
            return;
        }
        let total = usage >> 32;
        let free = usage & 0xFFFF_FFFF;
        if total == 0 || free > total {
            println("app: FS21 usage sanity FAILED");
            return;
        }
        // 卷表里定位 MFS 那张盘 (整盘卷: mfs.img 是 nsid=2, start_lba=0)。
        let n = block_list_volumes(vfs::RESULT_BUF as *mut u8, 16);
        if n == 0 || n == u64::MAX {
            println("app: FS21 list volumes FAILED");
            return;
        }
        let mut i = 0u64;
        let mut found = false;
        while i < n {
            let d = vol_desc(vfs::RESULT_BUF as *const u8, i as usize);
            if d.nsid == 2 && d.start_lba == 0 {
                found = true;
                if d.sectors == 0 {
                    println("app: FS21 whole-disk volume still reports no capacity FAILED");
                    return;
                }
                if total * 8 != d.sectors as u64 {
                    println("app: FS21 fs does not fill its volume FAILED");
                    return;
                }
            }
            i += 1;
        }
        if !found {
            println("app: FS21 mfs disk missing from volume table FAILED");
        }
    }

    // 25. FS-22 自测 (S2): 显式格式化 (`mkfs.mfs`) + 额外 MFS 卷挂载与按卷服务。
    //     新增的空白盘 (nsid 6) 启动时卷层探测为 unknown —— 正是真盘上「刚买一块盘」的样子。
    //     验证四件事:
    //       (a) 护栏: 对 FAT / ext2 / 不存在的卷号调 mkfs 必须被拒 (绝不吞别人的分区);
    //       (b) 格式化空白卷成功, 该卷作为额外卷挂到 `/usb<卷号>` 后可独立读写;
    //       (c) 格式化**别的**卷之后, 主卷 `/mfs` 的数据仍完好 —— 证明 mfs_srv 把内存态
    //           (位图 / inode 表 / 快照) 正确重建回了主卷, 而不是继续用新卷的位图;
    //       (d) 在额外卷与主卷之间交替读写, 两边内容都不串 —— 证明按请求切卷生效。
    {
        let nvol = block_list_volumes(vfs::RESULT_BUF as *mut u8, 16);
        if nvol == u64::MAX || nvol < 6 {
            println("app: FS22 list volumes FAILED");
            return;
        }
        let mut fat_vol = u64::MAX;
        let mut ext2_vol = u64::MAX;
        let mut spare_vol = u64::MAX;
        let mut i = 0u64;
        while i < nvol {
            let d = vol_desc(vfs::RESULT_BUF as *const u8, i as usize);
            if d.nsid == 1 && d.kind == VOL_KIND_FAT {
                fat_vol = d.id as u64;
            }
            if d.nsid == 3 && d.kind == VOL_KIND_EXT2 {
                ext2_vol = d.id as u64;
            }
            if d.nsid == 6 {
                // 首次启动是空白 (unknown); 若保留上一轮的盘则是已格式化的 MFS。
                if d.kind != VOL_KIND_UNKNOWN && d.kind != VOL_KIND_MFS {
                    println("app: FS22 spare volume has unexpected kind FAILED");
                    return;
                }
                spare_vol = d.id as u64;
            }
            i += 1;
        }
        if fat_vol == u64::MAX || ext2_vol == u64::MAX || spare_vol == u64::MAX {
            println("app: FS22 test volumes missing FAILED");
            return;
        }

        // (a) 护栏: 别人的分区与不存在的卷号都不允许格式化。
        //     ⚠️ 安装盘变体 (`INSTALL=1`) 里护栏对**非空白卷**故意放开 (装机要覆盖的正是盘上
        //     原有的文件系统) —— 那种镜像下**绝不能**照旧去碰 FAT 卷: 它是正在跑的根文件系统,
        //     放行就等于当场把自己格掉。故这两条断言只在日常镜像里跑, 护栏本身由日常镜像的
        //     全量回归守着 (FS-22 的常规断言)。
        if INSTALL_MODE {
            println(
                "app: FS22 guard checks SKIPPED (install image: non-blank volumes are formattable)",
            );
        } else {
            if vfs::mfs_mkfs(fat_vol) != u64::MAX {
                println("app: FS22 mkfs on FAT volume NOT refused FAILED");
                return;
            }
            if vfs::mfs_mkfs(ext2_vol) != u64::MAX {
                println("app: FS22 mkfs on ext2 volume NOT refused FAILED");
                return;
            }
        }
        // 「不存在的卷号」在任何变体里都必须被拒 —— 这一条没有放宽。
        if vfs::mfs_mkfs(4242) != u64::MAX {
            println("app: FS22 mkfs on nonexistent volume NOT refused FAILED");
            return;
        }

        // (c) 先在主卷落一个标记, 格式化完别的卷后它必须还在。
        let keep = "/mfs/FS22KEEP.TXT";
        let kfd = vfs::creat(keep);
        if kfd == u64::MAX || vfs::write(kfd, 0, b"KEEP") != 4 {
            println("app: FS22 write marker on primary FAILED");
            return;
        }
        vfs::close(kfd);

        // (b) 格式化空白卷; 成功后 mfs_srv 会把它挂到 `/usb<卷号>`。
        // 回复是落盘后的主卷序号 (>0) —— 格式化同时把这块卷标记为主卷, 但那要**下次
        // 启动**才生效 (FS-24 会专门盯这条链路), 本次运行 `/mfs` 仍是原主卷。
        if vfs::mfs_mkfs(spare_vol) == u64::MAX {
            println("app: FS22 mkfs on blank volume FAILED");
            return;
        }

        // 拼出额外卷的挂载点 `/usb<卷号>` 与新卷上的目标路径。
        let mut pbuf = [0u8; 32];
        pbuf[..4].copy_from_slice(b"/usb");
        let rl = 4 + dec_to_str(spare_vol, &mut pbuf[4..]);
        let suffix = b"/NEW.TXT";
        pbuf[rl..rl + suffix.len()].copy_from_slice(suffix);
        let newpath = unsafe { core::str::from_utf8_unchecked(&pbuf[..rl + suffix.len()]) };

        let nfd = vfs::creat(newpath);
        if nfd == u64::MAX || vfs::write(nfd, 0, b"SPARE") != 5 {
            println("app: FS22 write on new volume FAILED");
            return;
        }
        vfs::close(nfd);

        // (d) 额外卷 -> 主卷 -> 额外卷 交替读, 各自内容不能串。
        let nfd = vfs::open(newpath);
        if nfd == u64::MAX || vfs::read(nfd, 0, 5) != 5 {
            println("app: FS22 reopen on new volume FAILED");
            return;
        }
        vfs::close(nfd);
        {
            let got = unsafe { core::slice::from_raw_parts(vfs::RESULT_BUF as *const u8, 5) };
            if got != b"SPARE" {
                println("app: FS22 new volume content mismatch FAILED");
                return;
            }
        }

        // (c) 主卷标记仍完好。
        let kfd = vfs::open(keep);
        if kfd == u64::MAX || vfs::read(kfd, 0, 4) != 4 {
            println("app: FS22 primary volume LOST after mkfs FAILED");
            return;
        }
        vfs::close(kfd);
        {
            let got = unsafe { core::slice::from_raw_parts(vfs::RESULT_BUF as *const u8, 4) };
            if got != b"KEEP" {
                println("app: FS22 primary volume content mismatch FAILED");
                return;
            }
        }
    }

    // 26. FS-23(a) 自测 (S3a): 位图外置后的**多块位图**容量路径。
    //     MFS7 把空闲位图从超级块里挪出来 (独立位图数据块 + 位图头块), 容量上限从
    //     内联位图的 30656 块 (≈119 MiB) 提到 `MFS_MAX_BLOCKS` = 1018 × 32768
    //     ≈ 127.25 GiB。一个 4 KiB 位图数据块覆盖 32768 块 (= 128 MiB), 故卷超过
    //     128 MiB 时 bb ≥ 2 —— 这正是本自测要走的路径 (默认测试卷 256 MiB → bb = 2)。
    //     断言 (只盯几何关系, 不真写满 128 MiB 数据: IPC 往返代价不可接受):
    //       (a) MFS 总块数 × 8 扇区/块 == 该卷 sectors —— 格式化按卷几何定尺寸仍成立;
    //       (b) 总块数 > 一个位图数据块的覆盖范围 (32768), 即 bb ≥ 2 —— 多块位图确实在用。
    //     多块位图的**读回 / CRC 校验 / 重建**由每次挂载校验 (逐 chunk 比对 CRC32) 与
    //     GC 全量重建覆盖: FS-12 每轮做 3 次全卷 GC, 会把所有 chunk 置脏并整体落盘。
    {
        // `mfs_stat` 报的是**当前卷**, 而 FS-22 刚在额外卷上折腾过 —— 先对主卷做一次
        // 操作把服务的内存态锚回主卷, 免得量到的是一块小卷。
        let probe = vfs::creat("/mfs/FS23PROBE.TXT");
        if probe == u64::MAX {
            println("app: FS23 pin primary volume FAILED");
            return;
        }
        vfs::close(probe);
        let usage = vfs::mfs_stat();
        if usage == u64::MAX {
            println("app: FS23 mfs_stat FAILED");
            return;
        }
        let total = usage >> 32;
        // 主卷必须跨过单个位图数据块的覆盖范围 (32768 块) -> bb ≥ 2, 多块位图在用。
        if total <= 32768 {
            println("app: FS23 primary volume too small for multi-chunk bitmap FAILED");
            return;
        }
        let n = block_list_volumes(vfs::RESULT_BUF as *mut u8, 16);
        if n == 0 || n == u64::MAX {
            println("app: FS23 list volumes FAILED");
            return;
        }
        let mut i = 0u64;
        let mut found = false;
        while i < n {
            let d = vol_desc(vfs::RESULT_BUF as *const u8, i as usize);
            if d.nsid == 2 && d.start_lba == 0 {
                found = true;
                if total * 8 != d.sectors as u64 {
                    println("app: FS23 fs does not fill its volume FAILED");
                    return;
                }
            }
            i += 1;
        }
        if !found {
            println("app: FS23 mfs disk missing from volume table FAILED");
        }
    }

    // 27. FS-23(b) 自测 (S3b): 单文件 >4 GiB (稀疏) —— u64 offset 端到端 + 三级间接块。
    //     在 /mfs 上建文件并 truncate 到 5 GiB + 12345 字节 (稀疏扩展, 不真写数据, 代价可
    //     接受); 再在**跨过 4 GiB 边界**的偏移 (4 GiB + 4 KiB) 写 16 字节已知内容, 同偏移
    //     读回逐字节校验。二级间接区的字节上限约 3.98 GiB
    //     ((1005 + 1022 + 1022²) × 4088), 故该偏移必然落在**三级间接区** —— 这条断言同时
    //     覆盖「u64 offset 端到端」与「三级间接块被真正使用」。
    const FS23B_OFF: u64 = 4 * 1024 * 1024 * 1024 + 4096;
    const FS23B_SIZE: u64 = 5 * 1024 * 1024 * 1024 + 12345;
    const FS23B_PAT: [u8; 16] = [0xA5; 16];
    {
        let fd = vfs::creat("/mfs/FS23BIG.BIN");
        if fd == u64::MAX {
            println("app: FS23 creat big file FAILED");
            return;
        }
        if vfs::truncate(fd, FS23B_SIZE) != 1 {
            println("app: FS23 truncate to 5GiB FAILED");
            vfs::close(fd);
            return;
        }
        // size 必须如实报出 5 GiB + 12345 (u64, 而不是被截窄到 32 位的值)。
        match fs13_stat("/mfs/FS23BIG.BIN") {
            Some(s) if s.size == FS23B_SIZE => {}
            _ => {
                println("app: FS23 big file size FAILED");
                vfs::close(fd);
                return;
            }
        }
        // 起始处仍是空洞: 整页读回必须全 0 (稀疏扩展不分配块)。
        if !fs13_all_zero(fd, 0, 4096) {
            println("app: FS23 head hole FAILED");
            vfs::close(fd);
            return;
        }
        // 跨 4 GiB 边界写 16 字节 (count ≤ 一页), 同偏移读回逐字节校验。
        if vfs::write(fd, FS23B_OFF, &FS23B_PAT) != 16 {
            println("app: FS23 write past 4GiB FAILED");
            vfs::close(fd);
            return;
        }
        if vfs::read(fd, FS23B_OFF, 16) != 16 {
            println("app: FS23 read past 4GiB FAILED");
            vfs::close(fd);
            return;
        }
        {
            let got = unsafe { core::slice::from_raw_parts(vfs::RESULT_BUF as *const u8, 16) };
            if got != FS23B_PAT {
                println("app: FS23 past-4GiB content mismatch FAILED");
                vfs::close(fd);
                return;
            }
        }
        vfs::close(fd);
    }

    // 28. FS-23(c) 自测 (S3b): 三级块在重新打开与 GC 后仍可达。
    //     重新 open 后读同一偏移内容一致 -> size/指针已正确持久化, 不依赖内存态;
    //     再调 MFS 显式 GC 后复读一致 -> GC 的可达性标记正确覆盖了 MFI3 (漏标会把三级块
    //     当垃圾回收并重新分配出去, 这条读取就会失败或读到错内容)。
    {
        let fd = vfs::open("/mfs/FS23BIG.BIN");
        if fd == u64::MAX {
            println("app: FS23 reopen FAILED");
            return;
        }
        if vfs::read(fd, FS23B_OFF, 16) != 16 {
            println("app: FS23 read after reopen FAILED");
            vfs::close(fd);
            return;
        }
        {
            let got = unsafe { core::slice::from_raw_parts(vfs::RESULT_BUF as *const u8, 16) };
            if got != FS23B_PAT {
                println("app: FS23 content lost after reopen FAILED");
                vfs::close(fd);
                return;
            }
        }
        vfs::close(fd);
        if vfs::mfs_gc() == u64::MAX {
            println("app: FS23 GC FAILED");
            return;
        }
        let fd = vfs::open("/mfs/FS23BIG.BIN");
        if fd == u64::MAX {
            println("app: FS23 reopen after GC FAILED");
            return;
        }
        if vfs::read(fd, FS23B_OFF, 16) != 16 {
            println("app: FS23 read after GC FAILED");
            vfs::close(fd);
            return;
        }
        {
            let got = unsafe { core::slice::from_raw_parts(vfs::RESULT_BUF as *const u8, 16) };
            if got != FS23B_PAT {
                println("app: FS23 content lost after GC FAILED");
                vfs::close(fd);
                return;
            }
        }
        vfs::close(fd);
        // 清理: 删掉这个 5 GiB 稀疏文件, 避免污染后续回归与下一轮持久卷。
        if vfs::unlink("/mfs/FS23BIG.BIN") != 1 {
            println("app: FS23 cleanup unlink FAILED");
            return;
        }
    }

    // 29. FS-24 自测 (S2 补齐): **主卷切换的持久化标记**。
    //     `mkfs.mfs <卷号>` 把该卷标记为主卷 (超级块 `+256` 的序号 = 现有最大 + 1),
    //     从此**下次启动** `/mfs` 认领的就是它 —— 与卷表扫描顺序无关 (M8 未覆盖项的收口)。
    //     会话内不能重启, 故把这条链路拆成三个可观测环节:
    //       (a) mkfs 回复的序号是**从盘上回读**的 (>0) —— 标记确实写进超级块且能再读出;
    //       (b) 再次 mkfs 同一卷, 序号严格变大 —— 这就是「最近 mkfs 的卷胜出」的判据;
    //       (c) 两次 mkfs 之间对该卷做一次**普通写提交**, 序号仍继续变大 —— 证明标记在
    //           普通提交里被保留 (`mfs_load_state` 回读 + `mfs_build_super` 回写)。
    //           若标记被一次普通写盘抹成 0, (b)(c) 的序号会掉回 1, 断言立刻失败。
    //       (d) 全程主卷 `/mfs` 照常可读 —— 给别的卷换标不扰动正在服务的卷。
    {
        let nvol = block_list_volumes(vfs::RESULT_BUF as *mut u8, 16);
        if nvol == u64::MAX {
            println("app: FS24 list volumes FAILED");
            return;
        }
        let mut spare_vol = u64::MAX;
        let mut i = 0u64;
        while i < nvol {
            let d = vol_desc(vfs::RESULT_BUF as *const u8, i as usize);
            if d.nsid == 6 {
                spare_vol = d.id as u64;
            }
            i += 1;
        }
        if spare_vol == u64::MAX {
            println("app: FS24 spare volume missing FAILED");
            return;
        }

        // (a) 序号来自盘上回读 (FS-22 已格式化过一次, 这里再格一次同样应拿到正序号)。
        let s1 = vfs::mfs_mkfs(spare_vol);
        if s1 == u64::MAX || s1 == 0 {
            println("app: FS24 primary mark not on disk FAILED");
            return;
        }
        // (b) 同一卷再格式化: 序号必须严格变大。
        let s2 = vfs::mfs_mkfs(spare_vol);
        if s2 == u64::MAX || s2 <= s1 {
            println("app: FS24 primary serial not increasing FAILED");
            return;
        }

        // (c) 对该卷做一次普通写提交 (走 /usb<卷号> 挂载点, 会切到该卷并提交超级块)。
        let mut pbuf = [0u8; 32];
        pbuf[..4].copy_from_slice(b"/usb");
        let rl = 4 + dec_to_str(spare_vol, &mut pbuf[4..]);
        let suffix = b"/FS24.TXT";
        pbuf[rl..rl + suffix.len()].copy_from_slice(suffix);
        let path = unsafe { core::str::from_utf8_unchecked(&pbuf[..rl + suffix.len()]) };
        let fd = vfs::creat(path);
        if fd == u64::MAX || vfs::write(fd, 0, b"M") != 1 {
            println("app: FS24 write on marked volume FAILED");
            return;
        }
        vfs::close(fd);

        let s3 = vfs::mfs_mkfs(spare_vol);
        if s3 == u64::MAX || s3 <= s2 {
            println("app: FS24 primary mark lost across commit FAILED");
            return;
        }

        // (d) 主卷未被扰动 (FS-23(a) 落下的探针文件仍在)。
        let fd = vfs::open("/mfs/FS23PROBE.TXT");
        if fd == u64::MAX {
            println("app: FS24 primary volume damaged by mkfs FAILED");
            return;
        }
        vfs::close(fd);
    }

    // 30. FS-25 自测 (S2 补齐): **只改标记、不动数据**地换主卷 (`mfs.primary`)。
    //     mkfs 也能换主卷, 但它会**擦掉**卷上的文件 —— 日常换主卷必须有一条不动数据的
    //     路, 否则「把已有数据的盘升为主卷」就等于「把数据删掉」。本自测自给自足:
    //       (a) 先 `mfs_mkfs` 把目标卷置成确定状态 (幂等准备, 不依赖 FS-24 的残留),
    //           它同时给出**主卷序号基线**;
    //       (b) 在目标卷落一个文件 —— 建立「改标前数据完好」的基线;
    //       (c) `mfs_set_primary` 后: 序号必须**严格大于** (a) 的基线 —— 证明 mkfs 与
    //           set-primary 共用同一个只增计数器 (各自计数的话「最近的胜出」会失效);
    //           且该文件必须**原样可读、逐字节一致** —— 这是与 mkfs 的本质区别, 实现里
    //           若误走格式化路径, 这里立刻失败;
    //       (d) 对**非 MFS 卷 / 不存在的卷号**一律拒绝 —— 这条护栏比 mkfs 更严, 因为
    //           目标卷上放着用户的文件, 认错卷号绝不能有破坏性后果。
    {
        let nvol = block_list_volumes(vfs::RESULT_BUF as *mut u8, 16);
        if nvol == u64::MAX {
            println("app: FS25 list volumes FAILED");
            return;
        }
        let mut spare_vol = u64::MAX;
        let mut fat_vol = u64::MAX;
        let mut i = 0u64;
        while i < nvol {
            let d = vol_desc(vfs::RESULT_BUF as *const u8, i as usize);
            if d.nsid == 6 {
                spare_vol = d.id as u64;
            }
            if d.nsid == 1 {
                fat_vol = d.id as u64;
            }
            i += 1;
        }
        if spare_vol == u64::MAX || fat_vol == u64::MAX {
            println("app: FS25 test volumes missing FAILED");
            return;
        }

        // (a) 幂等准备 + 序号基线。
        let s_mkfs = vfs::mfs_mkfs(spare_vol);
        if s_mkfs == u64::MAX || s_mkfs == 0 {
            println("app: FS25 prep mkfs FAILED");
            return;
        }

        // (b) 在目标卷上落一个文件。
        let mut pbuf = [0u8; 32];
        pbuf[..4].copy_from_slice(b"/usb");
        let rl = 4 + dec_to_str(spare_vol, &mut pbuf[4..]);
        let suffix = b"/FS25.DAT";
        pbuf[rl..rl + suffix.len()].copy_from_slice(suffix);
        let path = unsafe { core::str::from_utf8_unchecked(&pbuf[..rl + suffix.len()]) };
        let fd = vfs::creat(path);
        if fd == u64::MAX || vfs::write(fd, 0, b"FS25MARK") != 8 {
            println("app: FS25 write payload FAILED");
            return;
        }
        vfs::close(fd);

        // (c) 只改标记: 序号递增 (与 mkfs 同一计数器)。
        let s_set = vfs::mfs_set_primary(spare_vol);
        if s_set == u64::MAX || s_set == 0 {
            println("app: FS25 set-primary FAILED");
            return;
        }
        if s_set <= s_mkfs {
            println("app: FS25 serial not increasing across paths FAILED");
            return;
        }

        // (c) 数据必须原样还在 —— 与 mkfs 的本质区别。
        let fd = vfs::open(path);
        if fd == u64::MAX || vfs::read(fd, 0, 8) != 8 {
            println("app: FS25 payload LOST after set-primary FAILED");
            return;
        }
        vfs::close(fd);
        {
            let got = unsafe { core::slice::from_raw_parts(vfs::RESULT_BUF as *const u8, 8) };
            if got != b"FS25MARK" {
                println("app: FS25 payload content mismatch FAILED");
                return;
            }
        }

        // (d) 护栏: 非 MFS 卷与不存在的卷号都必须被拒。
        if vfs::mfs_set_primary(fat_vol) != u64::MAX {
            println("app: FS25 set-primary on FAT volume NOT refused FAILED");
            return;
        }
        if vfs::mfs_set_primary(4242) != u64::MAX {
            println("app: FS25 set-primary on nonexistent volume NOT refused FAILED");
            return;
        }
    }
    // 31. FS-26 自测 (S2 卷管理收口): block_srv **写**分区表 (`part.*`) —— 建/删 GPT 与 MBR。
    //     目标盘是 Makefile 里挂的第 7 个 namespace (`build/pt.img`, 64 MiB 纯空白, 见
    //     `run-nvme` / `fs-regress.sh`)。分四段:
    //       (a) 起点干净: `part.wipe` 后该盘回到「无分区表」的**整盘卷** (容量 64 MiB);
    //       (b) 强制 MBR: 建 16 MiB 分区 -> 卷表出现它, 且 LBA 0 的**字节**对得上
    //           (0x55AA / 类型 0x83 / 起点 2048 / 大小 32768); 删掉 -> 盘回空白;
    //       (c) GPT (空白盘默认): 建 16 MiB 分区 -> 保护性 MBR(0xEE) + 头("EFI PART") +
    //           头 CRC32 + 项数组 CRC32 全部自洽, 项里的起点/大小与卷表一致; 再验「已有
    //           GPT 时强制 MBR 必须被拒」, 然后在这个分区上 `mkfs.mfs` -> 卷类型变 mfs
    //           (一条龙: 建分区 -> 格式化), 最后删掉它;
    //       (d) 空白盘上的护栏: 无表时删分区、请求超出剩余空间都必须被拒。
    //     为什么能读裸字节: 建完分区后 LBA 0/1 已不属于任何卷 (整盘卷没了、新分区从 2048 起),
    //     只能按 nsid 直读 (`block_disk_read`) —— 这也正是本自测**独立**校验写盘结果
    //     (而不是只信卷表) 的关键: 卷表是同一份代码扫出来的, 光看它可能「自证成功」。
    {
        const PT_NSID: u64 = 7;
        // 分区大小: 16 MiB。
        const PT_PART_SECTORS: u64 = 32768;
        // 起点: 1 MiB 对齐。
        const PT_PART_START: u32 = 2048;
        // `build/pt.img` 容量: 64 MiB。
        const PT_DISK_SECTORS: u32 = 131072;

        let raw = vfs::RESULT_BUF as *mut u8;

        // (a) 起点干净 (幂等: 复用镜像时上一轮留下的 GPT/MBR 也在这里被清掉)。
        if block_part_wipe(PT_NSID) != 1 {
            println("app: FS26 wipe FAILED");
            return;
        }
        let n = block_list_volumes(raw, 16);
        match vol_desc_find(raw as *const u8, n, PT_NSID as u32, 0) {
            Some(d) if d.sectors == PT_DISK_SECTORS => {}
            Some(_) => {
                println("app: FS26 blank disk capacity wrong FAILED");
                return;
            }
            None => {
                println("app: FS26 blank disk volume missing FAILED");
                return;
            }
        }

        // (b) 强制 MBR (空白盘上才允许):
        let vol_mbr = block_part_create(PT_NSID, PT_PART_SECTORS, true);
        if vol_mbr == u64::MAX {
            println("app: FS26 create MBR partition FAILED");
            return;
        }
        let n = block_list_volumes(raw, 16);
        match vol_desc_find(raw as *const u8, n, PT_NSID as u32, PT_PART_START) {
            Some(d) if d.sectors == PT_PART_SECTORS as u32 && d.id as u64 == vol_mbr => {}
            Some(_) => {
                println("app: FS26 MBR volume geometry mismatch FAILED");
                return;
            }
            None => {
                println("app: FS26 MBR partition not registered FAILED");
                return;
            }
        }
        // LBA 0 的字节必须真的是刚写的那张 MBR。
        if !block_disk_read(PT_NSID, 0, raw) {
            println("app: FS26 MBR raw read FAILED");
            return;
        }
        if unsafe { *raw.add(510) } != 0x55 || unsafe { *raw.add(511) } != 0xAA {
            println("app: FS26 MBR signature wrong FAILED");
            return;
        }
        if unsafe { *raw.add(446 + 4) } != PART_MBR_TYPE_LINUX
            || read_u32(unsafe { raw.add(446 + 8) }) != PT_PART_START
            || read_u32(unsafe { raw.add(446 + 12) }) != PT_PART_SECTORS as u32
        {
            println("app: FS26 MBR entry wrong FAILED");
            return;
        }
        if block_part_delete(PT_NSID, 0) != 1 {
            println("app: FS26 delete MBR partition FAILED");
            return;
        }
        let n = block_list_volumes(raw, 16);
        if vol_desc_find(raw as *const u8, n, PT_NSID as u32, PT_PART_START).is_some()
            || vol_desc_find(raw as *const u8, n, PT_NSID as u32, 0).is_none()
        {
            println("app: FS26 MBR delete did not restore blank disk FAILED");
            return;
        }

        // (c) GPT (空白盘默认风格):
        let vol_gpt = block_part_create(PT_NSID, PT_PART_SECTORS, false);
        if vol_gpt == u64::MAX {
            println("app: FS26 create GPT partition FAILED");
            return;
        }
        let n = block_list_volumes(raw, 16);
        match vol_desc_find(raw as *const u8, n, PT_NSID as u32, PT_PART_START) {
            Some(d) if d.sectors == PT_PART_SECTORS as u32 && d.id as u64 == vol_gpt => {}
            Some(_) => {
                println("app: FS26 GPT volume geometry mismatch FAILED");
                return;
            }
            None => {
                println("app: FS26 GPT partition not registered FAILED");
                return;
            }
        }
        // 保护性 MBR: LBA0 第一条项类型必须是 0xEE。
        if !block_disk_read(PT_NSID, 0, raw) || unsafe { *raw.add(446 + 4) } != 0xEE {
            println("app: FS26 GPT protective MBR wrong FAILED");
            return;
        }
        // GPT 头: 签名 / header_size / 自校验 CRC32 (算之前把 CRC 字段清零, 算完还原)。
        if !block_disk_read(PT_NSID, 1, raw) {
            println("app: FS26 GPT header read FAILED");
            return;
        }
        if unsafe { core::slice::from_raw_parts(raw as *const u8, 8) } != b"EFI PART"
            || read_u32(unsafe { raw.add(0x0C) }) != 92
        {
            println("app: FS26 GPT header layout wrong FAILED");
            return;
        }
        let hdr_crc = read_u32(unsafe { raw.add(0x10) });
        let entry_crc = read_u32(unsafe { raw.add(0x58) });
        write_u32(unsafe { raw.add(0x10) }, 0);
        let hdr_calc = mfs_crc32(unsafe { core::slice::from_raw_parts(raw as *const u8, 92) });
        write_u32(unsafe { raw.add(0x10) }, hdr_crc);
        if hdr_calc != hdr_crc {
            println("app: FS26 GPT header CRC32 mismatch FAILED");
            return;
        }
        // 项数组 CRC32: 128 项 × 128 B = 32 扇区, 一扇区一读地流式累加。
        let mut reg: u32 = 0xFFFF_FFFF;
        let mut i = 0u64;
        while i < 32 {
            if !block_disk_read(PT_NSID, 2 + i, raw) {
                println("app: FS26 GPT entries read FAILED");
                return;
            }
            reg = crc32_update(reg, unsafe {
                core::slice::from_raw_parts(raw as *const u8, 512)
            });
            i += 1;
        }
        if !reg != entry_crc {
            println("app: FS26 GPT entry array CRC32 mismatch FAILED");
            return;
        }
        // 第 0 项的字段: 类型 GUID 与起止 LBA 必须与卷表看到的一致。
        if !block_disk_read(PT_NSID, 2, raw) {
            println("app: FS26 GPT entry read FAILED");
            return;
        }
        if read_u64(unsafe { raw.add(0x20) }) != PT_PART_START as u64
            || read_u64(unsafe { raw.add(0x28) }) != PT_PART_START as u64 + PT_PART_SECTORS - 1
        {
            println("app: FS26 GPT entry geometry wrong FAILED");
            return;
        }
        let mut k = 0usize;
        while k < 16 {
            if unsafe { *raw.add(k) } != PART_GPT_TYPE_LINUX[k] {
                println("app: FS26 GPT entry type GUID wrong FAILED");
                return;
            }
            k += 1;
        }
        // 护栏: 盘上已是 GPT 时, 强制 MBR 必须被拒 (那会毁掉整张表)。
        if block_part_create(PT_NSID, PT_PART_SECTORS, true) != u64::MAX {
            println("app: FS26 force-MBR on GPT disk NOT refused FAILED");
            return;
        }
        // 一条龙: 在这个新分区上建 MorionFS, 卷类型必须变成 mfs。
        if vfs::mfs_mkfs(vol_gpt) == u64::MAX {
            println("app: FS26 mkfs on new partition FAILED");
            return;
        }
        // 卷表的 kind 是**扫描时**探出来的, 所以格式化后要重读一次才会更新。
        if block_part_reload() == 0 {
            println("app: FS26 reload after mkfs FAILED");
            return;
        }
        let n = block_list_volumes(raw, 16);
        match vol_desc_find(raw as *const u8, n, PT_NSID as u32, PT_PART_START) {
            Some(d) if d.kind == VOL_KIND_MFS => {}
            Some(_) => {
                println("app: FS26 new partition not detected as MFS FAILED");
                return;
            }
            None => {
                println("app: FS26 partition lost after mkfs FAILED");
                return;
            }
        }
        // 删掉它 (GPT 表的项数组与两个头都会被重算 CRC), 盘回到空白。
        if block_part_delete(PT_NSID, 0) != 1 {
            println("app: FS26 delete GPT partition FAILED");
            return;
        }
        let n = block_list_volumes(raw, 16);
        if vol_desc_find(raw as *const u8, n, PT_NSID as u32, PT_PART_START).is_some()
            || vol_desc_find(raw as *const u8, n, PT_NSID as u32, 0).is_none()
        {
            println("app: FS26 GPT delete did not restore blank disk FAILED");
            return;
        }

        // (d) 空白盘上的护栏: 无表可删 / 请求超出剩余空间都必须被拒 (且一个字都不写)。
        if block_part_delete(PT_NSID, 0) != 0 {
            println("app: FS26 delete without a partition table NOT refused FAILED");
            return;
        }
        if block_part_create(PT_NSID, 1 << 40, false) != u64::MAX {
            println("app: FS26 oversized create NOT refused FAILED");
            return;
        }
        if block_part_create(4242, PT_PART_SECTORS, false) != u64::MAX {
            println("app: FS26 create on nonexistent disk NOT refused FAILED");
            return;
        }

        // (e) 收尾: 再建一个 GPT 分区并**留在盘上**。自测里复算 CRC32 只能证明这张表「自洽」,
        //     证明不了 GUID 字节序 / 头字段这些**规范细节**; 留在盘上是为了让宿主工具
        //     (`sgdisk -v` / `parted -l`, 见 `fs-regress.sh`) 能跨实现地校验它。
        //     下一轮开头那句 `part.wipe` 会清掉它, 所以反复跑仍然确定。
        if block_part_create(PT_NSID, PT_PART_SECTORS, false) == u64::MAX {
            println("app: FS26 final GPT create FAILED");
            return;
        }
    }
    // 04b: 清掉可能由上一轮启动遗留的权限自测开关 —— 保证本轮 FS-27/28 的 hello 实例
    // 不会去共享自测缓冲页 (`SYS_SHARE_PAGE` 的同地址映射在服务域里是持久的, 重复共享
    // 会撞 `PageAlreadyMapped` 内核 panic; 每轮启动只允许 FS-34 那次 hello 共享一次)。
    vfs::unlink("/mfs/pub/perm.go");

    // 32. FS-27 自测 (E1/E2 可执行文件加载): 从**磁盘上的文件**加载一个独立编译的程序 ——
    //     `morion::exec::spawn_file` 打开路径、分块读进本域内存、交给内核 `SYS_SPAWN_ELF`,
    //     内核校验后建**新域**、按 ELF 段映射、起任务。走的是与 shell `run` 命令**同一条**
    //     代码路径, 所以这条自测覆盖的就是用户实际用的加载链。
    //     子程序 (`user/hello`) 会自己打印 `exec:` 开头的标记行 —— 它真的跑起来了, 由那行
    //     标记 + 回归脚本判定; 这里只断言加载调用本身成功。
    let mut hello_domain = u64::MAX;
    // 基线: 加载子程序**之前**的存活域数 —— 子程序退出后必须回到这个数 (域被回收)。
    let alive_base = sys_domain_count();
    {
        match morion::exec::spawn_file(HELLO_MEX) {
            Some(domain) => {
                hello_domain = domain;
                print("app: FS27 exec ");
                print(HELLO_MEX);
                print(" -> domain ");
                print_u64(domain);
                println(" (child prints its own line next)");
            }
            None => println("app: FS27 spawn_file(\"/hello.mex\") FAILED"),
        }
    }

    // 33. FS-28 自测 (E2b 退出即回收): 子程序返回即 `SYS_EXIT` → 内核**自动销毁**它的域
    //     (回收地址空间 + 栈 + 页表帧 + 任务槽, 域号归还)。这里不再显式 destroy, 而是断言
    //     "子程序退出后存活域数回到基线", 再反复"加载 → 等退出"验证**域号被复用**且
    //     空闲帧数**每轮都回到同一个稳态值** (掉下去 = 有帧漏了)。
    //     这是"退出即回收真的发生了"唯一的端到端取证 —— 页表遍历与逐页记账在宿主单测里跑不了
    //     (单测是用户态进程, 读不了 CR3、也没有真页表), 那侧只覆盖纯策略。
    if hello_domain == u64::MAX {
        println("app: FS28 skipped (FS27 did not load)");
    } else if fs28_domain_reclaim(hello_domain, alive_base).is_none() {
        return;
    }

    // FS-29 (E3c): 监督者原地重启 —— 让 echo (域 3) 退出, init 应在巡检周期内把它拉起来。
    if fs29_supervisor_restart().is_none() {
        return;
    }

    // 34. FS-31 自测 (01 健壮性收口): 最小 fsck 对账口径的稳定性。
    //     客户机内无法制造「已分配但不可达」的 inode 泄漏 (那要在目录项插入与 inode 登记
    //     之间掉电), 故本自测断言**对一份结构一致的卷**: 报泄漏 inode = 0、可回收块 = 0;
    //     再跑 `--repair` 仍回 0 (不误伤、不改盘) —— 这正是回归里能稳定复现的那半。
    //     真正「拒绝挂载旧 magic 且盘未变」的端到端取证由 FS-30 在宿主侧单独做。
    {
        let r = vfs::mfs_fsck(false);
        if r == u64::MAX {
            println("app: FS31 fsck report FAILED");
            return;
        }
        if r != 0 {
            println("app: FS31 healthy volume reported leaks FAILED");
            return;
        }
        let rr = vfs::mfs_fsck(true);
        if rr == u64::MAX {
            println("app: FS31 fsck repair FAILED");
            return;
        }
        if rr != 0 {
            println("app: FS31 repair not idempotent FAILED");
            return;
        }
        println("app: FS31 fsck reconciled (0 leaked inodes; --repair idempotent)");
    }

    // 35. FS-32 自测 (01): 显式 `sync` 的落盘断言 —— `write → sync → 读回`, 且 sync
    //     回复的 gen **就是盘上**超级块里的 gen (裸读扇区 0 校验, 绕开服务内存态)。
    //     这是「崩溃一致性」自测能断言的落盘点: 回复的 gen 在盘上可独立复算出来。
    {
        const F32: &str = "/mfs/FS32SYNC.TXT";
        vfs::unlink(F32);
        let fd = vfs::creat(F32);
        if fd == u64::MAX || vfs::write(fd, 0, b"MFS-SYNC-1") != 10 {
            println("app: FS32 create/write FAILED");
            return;
        }
        vfs::close(fd);

        let gen = vfs::mfs_sync();
        if gen == u64::MAX || gen == 0 {
            println("app: FS32 mfs_sync FAILED");
            return;
        }
        // 主 MFS 卷 = nvme nsid 2 (build/mfs.img 整盘; 与 FS-23 的判据一致)。
        // 裸读扇区 0 = 超级块副本 A: +0 magic "MFS8", 块头 8 字节后 +24 是 gen(u64)。
        let raw = vfs::RESULT_BUF as *mut u8;
        if !block_disk_read(2, 0, raw) {
            println("app: FS32 raw superblock read FAILED");
            return;
        }
        if read_u32(raw) != 0x4D46_5338 {
            println("app: FS32 on-disk magic mismatch FAILED");
            return;
        }
        let disk_gen = read_u64(unsafe { raw.add(8 + 24) });
        if disk_gen != gen {
            println("app: FS32 on-disk gen != sync reply FAILED");
            return;
        }
        // sync 之后数据仍可读回 (重新打开, 不依赖内存里的 fd)。
        let fd = vfs::open(F32);
        if fd == u64::MAX || vfs::read(fd, 0, 10) != 10 {
            println("app: FS32 reopen/read FAILED");
            return;
        }
        {
            let got = unsafe { core::slice::from_raw_parts(vfs::RESULT_BUF as *const u8, 10) };
            if got != b"MFS-SYNC-1" {
                println("app: FS32 readback mismatch FAILED");
                vfs::close(fd);
                return;
            }
        }
        vfs::close(fd);
        vfs::unlink(F32);
        print("app: FS32 sync gen ");
        print_u64(gen);
        println(" persisted on disk (raw superblock match, readback ok)");
    }

    // 36. FS-34..37 自测 (04b 权限与多用户): 低权身份端到端。
    //     `mfs_srv` 按**发起域**静态映射身份 —— 引导期服务域 = uid 0, 运行期新建域 = uid 1000。
    //     app 本身是引导期服务域 (root), 故先以 root 造夹具, 再 `spawn_file` 一个**新域**里的
    //     `user/hello`(拿到 uid 1000)去跑权限用例; hello 把 4 个结果字节写进
    //     `/mfs/pub/perm.result`, 这里读回断言。详见 `fs34_perm_suite`。
    if fs34_perm_suite().is_none() {
        return;
    }

    println("app: SELFTEST DONE");
}

/// FS-34..37 (04b): 权限与多用户的端到端自测 (见调用点注释)。
///
/// 夹具开关 `perm.go` 决定 hello 是否跑权限用例 —— FS-27/28 复用的 hello 实例看不到它。
fn fs34_perm_suite() -> Option<()> {
    const PUB: &str = "/mfs/pub";
    const CASE34: &str = "/mfs/pub/case34.txt";
    const CASE35: &str = "/mfs/pub/case35.txt";
    const GATE: &str = "/mfs/pub/perm.go";
    const RESULT: &str = "/mfs/pub/perm.result";
    const STICKY: &str = "/mfs/sticky";
    const STICKY_ROOT: &str = "/mfs/sticky/rootfile.txt";
    const HELLO: &str = "/hello.mex";

    // 1) root 造夹具 (已存在时 mkdir/creat 的失败可忽略, 靠 chmod 把模式定死)。
    vfs::mkdir(PUB);
    if vfs::chmod_into(PUB, 0o777, vfs::RESULT_BUF) == u64::MAX {
        println("app: FS34 chmod /mfs/pub FAILED");
        return None;
    }
    let fd = vfs::creat(CASE34);
    if fd == u64::MAX {
        println("app: FS34 creat case34 FAILED");
        return None;
    }
    vfs::close(fd);
    if vfs::chmod_into(CASE34, 0o000, vfs::RESULT_BUF) == u64::MAX {
        println("app: FS34 chmod case34 FAILED");
        return None;
    }
    let fd = vfs::creat(CASE35);
    if fd == u64::MAX || vfs::write(fd, 0, b"hello") != 5 {
        println("app: FS34 creat/write case35 FAILED");
        return None;
    }
    vfs::close(fd);
    if vfs::chmod_into(CASE35, 0o666, vfs::RESULT_BUF) == u64::MAX {
        println("app: FS34 chmod case35 FAILED");
        return None;
    }
    vfs::mkdir(STICKY);
    if vfs::chmod_into(STICKY, 0o1777, vfs::RESULT_BUF) == u64::MAX {
        println("app: FS34 chmod /mfs/sticky FAILED");
        return None;
    }
    let fd = vfs::creat(STICKY_ROOT);
    if fd == u64::MAX {
        println("app: FS34 creat sticky/rootfile FAILED");
        return None;
    }
    vfs::close(fd);
    // 夹具开关 + 结果位图 (先清零, 免得读到上一轮)。
    let fd = vfs::creat(GATE);
    if fd == u64::MAX {
        println("app: FS34 creat perm.go FAILED");
        return None;
    }
    vfs::close(fd);
    let fd = vfs::creat(RESULT);
    if fd == u64::MAX || vfs::write(fd, 0, b"0000") != 4 {
        println("app: FS34 creat/write perm.result FAILED");
        return None;
    }
    vfs::close(fd);
    // 结果文件要给低权身份**可写** (hello 在 uid 1000 下写回位图), 故放开到 0666。
    if vfs::chmod_into(RESULT, 0o666, vfs::RESULT_BUF) == u64::MAX {
        println("app: FS34 chmod perm.result FAILED");
        return None;
    }

    // 2) 以**新域**跑 hello (uid 1000)。
    let base = sys_domain_count();
    let child = match morion::exec::spawn_file(HELLO) {
        Some(d) => d,
        None => {
            println("app: FS34 spawn hello FAILED");
            return None;
        }
    };
    if !wait_domain_alive(child, false, 4000) {
        println("app: FS34 hello did not exit FAILED");
        return None;
    }
    if sys_domain_count() != base {
        println("app: FS34 domain count drift FAILED");
        return None;
    }

    // 3) 读回结果位图并断言 (hello 侧已把低权域的真实结果写进来)。
    let fd = vfs::open(RESULT);
    if fd == u64::MAX {
        println("app: FS34 open perm.result FAILED");
        return None;
    }
    let n = vfs::read(fd, 0, 4);
    vfs::close(fd);
    if n != 4 {
        println("app: FS34 read perm.result FAILED");
        return None;
    }
    let got = unsafe { core::slice::from_raw_parts(vfs::RESULT_BUF as *const u8, 4) };
    if got != b"1111" {
        print("app: FS34-37 permission suite FAILED (FS34..37=");
        print(unsafe { core::str::from_utf8_unchecked(got) });
        println(")");
        return None;
    }
    println("app: FS34-37 permission suite OK (low-priv uid 1000 end-to-end)");
    // 关掉开关: 之后的 hello 实例 (如 shell `run`) 不再重复共享自测缓冲页。
    vfs::unlink(GATE);
    Some(())
}

/// GT-1 取证 (G3a 图形子系统): 文本渲染搬到用户态后的端到端验证。
///
/// 清屏 → 写 ASCII → 写汉字, 每步都用 `SYS_CALL` 问 `gfx_srv` 的光标位置来断言**排版列数**:
/// `"GT-1 "` 是 5 个 ASCII 字符 = 5 列; `"汉字宽字符"` 是 5 个汉字 × 2 列 = 10 列。列数由
/// **服务端字库的宽度表**算出, 所以这条断言同时证明了 UTF-8 解码、宽窄混排与光标推进;
/// 而"真的画到帧缓冲上了"由服务端**逐像素写后回读**保证 (`GFX_OP_TEXT` 回 1 才算过)。
///
/// ⚠️ 屏幕是**多客户端共享**的 (shell 也镜像打印到同一终端、共用一条光标): shell 的启动
/// 横幅/提示符可能正好插在"清屏 → 写 → 问光标"之间, 把光标挪走, 让本次测得列数偏大 ——
/// 那不是排版错了。故测量**重试到干净窗口**为止 (shell 打完启动输出就阻塞在等输入, 之后
/// 屏幕安静, 一定能测到); 这样不必给服务端加"原子返回列数"的协议。
fn gt1_text_rendering() -> Option<()> {
    /// 写 `text` 后光标应前进 `want_col` 列; 在并发写者留下的空隙里测量 (最多重试 ~2s)。
    fn measure(text: &str, want_col: u32) -> bool {
        for _ in 0..100 {
            if !morion::gfx::clear_screen() || !morion::gfx::print(text) {
                return false;
            }
            if morion::gfx::cursor() == Some((want_col, 0)) {
                return true;
            }
            sys_sleep(20);
        }
        false
    }
    if !measure("GT-1 ", 5) {
        println("app: GT1 ascii advance isn't 5 cols (ansi 8x16?)");
        return None;
    }
    if !measure("汉字宽字符", 10) {
        println("app: GT1 cjk advance isn't 10 cols (16x16 wide glyphs?)");
        return None;
    }
    // 换行 + 混排一行, 然后跳到第 20 行再写一行 —— 顺带把屏幕留成"人眼可核对"的样子。
    if !morion::gfx::print("\n服务内终端: 光标 / 换行 / 宽窄混排\n") {
        println("app: GT1 print mixed line FAILED");
        return None;
    }
    if !morion::gfx::move_cursor(0, 20) {
        println("app: GT1 move_cursor(0,20) FAILED");
        return None;
    }
    if !morion::gfx::print("GT-1 OK: ascii 5 cols, cjk 10 cols, cursor moved") {
        println("app: GT1 print at moved cursor FAILED");
        return None;
    }
    // 越界定位必须被**拒** (服务端不夹取): 若被接受, 说明边界检查是假的。
    if morion::gfx::move_cursor(9999, 9999) {
        println("app: GT1 out-of-range move_cursor was accepted");
        return None;
    }
    println("app: GT1 text console OK (layout cols verified, framebuffer pixel readback matched)");
    Some(())
}

/// GS-1 取证 (G2 图形子系统): 屏幕级原语 + 客户端共享表面。
///
/// 三条一起才算过:
///   ① `gfx_srv` 在跑 (`ping`);
///   ② 屏幕级原语 (`fill_screen` / `screen_rect`) 被接受;
///   ③ 客户端表面 (本域分配 + `SYS_SHARE_PAGE` 给 gfx_srv) 经 `blit` 画上屏 —— `gfx_srv`
///      **回读帧缓冲**校验通过才回 1, 这是"真的画上去了"的端到端证据 (无显示器也可断言);
///      另有客户端侧表面回读, 确认自己写进去的就是预期像素。
fn gs1_gfx_primitives() -> Option<()> {
    const W: u32 = 160;
    const H: u32 = 120;
    /// 四条竖直色带 (红 / 绿 / 蓝 / 黄)。
    const BAND: [u32; 4] = [0xC0_20_20, 0x20_C0_20, 0x20_20_C0, 0xE0_E0_20];

    if !morion::gfx::ping() {
        println("app: GS1 gfx_srv ping FAILED");
        return None;
    }
    // 屏幕级原语: 铺底色 + 画一个矩形。
    if !morion::gfx::fill_screen(0x08_08_10) {
        println("app: GS1 fill_screen FAILED");
        return None;
    }
    if !morion::gfx::screen_rect(0, 0, 320, 240, 0x40_40_60) {
        println("app: GS1 screen_rect FAILED");
        return None;
    }

    // 共享表面: 竖向色带。
    let surf = match morion::gfx::Surface::new(W, H) {
        Some(s) => s,
        None => {
            println("app: GS1 surface alloc FAILED");
            return None;
        }
    };
    let bw = W / 4;
    for (i, &c) in BAND.iter().enumerate() {
        surf.rect((i as u32) * bw, 0, bw, H, c);
    }
    // 客户端侧回读: 表面在 app 本域, 可直接读。
    if surf.read(0, 0) != BAND[0] || surf.read(W - 1, H - 1) != BAND[3] {
        println("app: GS1 surface readback FAILED");
        return None;
    }
    // blit 上屏: 服务端拷完会回读校验, 通过才回 1。
    if !surf.blit(16, 16) {
        println("app: GS1 blit FAILED");
        return None;
    }
    println("app: GS1 gfx primitives + shared surface OK (blit verified on framebuffer)");
    Some(())
}

/// GS-2 取证 (图形服务自愈): `gfx_srv` 退出 → init 监督**就地重启** → 客户端**重建共享会话**恢复。
///
/// 走的是与 FS-29 同款路径: 用一条控制请求让服务自己 `SYS_EXIT` (不是内核杀进程)。这里要
/// 覆盖"服务重启会清空它域内的页表"带来的连锁反应, 分两轮:
///
/// ① **客户端没在空窗期调用**: 客户端的共享页标志仍为"已共享", 但服务侧映射已随重启消失 ——
///    服务端对这块悬空页回 [`morion::gfx::GFX_REPLY_NO_SESSION`], 客户端据此**重建共享**
///    (重发 `SYS_SHARE_PAGE`, 不重新分配) 后重试成功。这正是 shell 的情形 (它多半在空窗期
///    正阻塞等输入, 不会调用)。
/// ② **客户端在空窗期调用**: 此时服务域无存活任务, `ipc::call` 必须**快速失败返回**而不是
///    让客户端永久挂起 (老行为会卡死), 并顺带作废会话。
fn gs2_gfx_srv_restart() -> Option<()> {
    const GFX: u64 = morion::gfx::GFX_DOMAIN;

    if !morion::gfx::ping() {
        println("app: GS2 gfx_srv not serving before test FAILED");
        return None;
    }

    // ---- ① 空窗期不调用: 靠服务端 NO_SESSION + 客户端重建共享恢复 ----
    if !morion::gfx::exit_server() {
        println("app: GS2 exit request FAILED");
        return None;
    }
    if !wait_domain_alive(GFX, false, 2000) {
        println("app: GS2 gfx_srv did not exit FAILED");
        return None;
    }
    if !wait_domain_alive(GFX, true, 4000) {
        println("app: GS2 init did not restart gfx_srv FAILED");
        return None;
    }
    if !morion::gfx::print("GS-2 screen recovered via session rebuild") {
        println("app: GS2 print after restart FAILED");
        return None;
    }
    // 串尾不带换行: 换行会把列归零, 而这里正是要看"写完之后光标确实前进了"。
    match morion::gfx::cursor() {
        Some((col, _)) if col > 0 => {}
        _ => {
            println("app: GS2 cursor after restart FAILED");
            return None;
        }
    }

    // ---- ② 空窗期调用: 必须快速失败、不挂起, 重建后又能用 ----
    if !morion::gfx::exit_server() {
        println("app: GS2 second exit request FAILED");
        return None;
    }
    if !wait_domain_alive(GFX, false, 2000) {
        println("app: GS2 gfx_srv did not exit (2nd) FAILED");
        return None;
    }
    if morion::gfx::ping() {
        println("app: GS2 ping on dead gfx_srv unexpectedly succeeded FAILED");
        return None;
    }
    if !wait_domain_alive(GFX, true, 4000) {
        println("app: GS2 init did not restart gfx_srv (2nd) FAILED");
        return None;
    }
    if !morion::gfx::ping() {
        println("app: GS2 restarted gfx_srv does not serve FAILED");
        return None;
    }

    println("app: GS2 gfx_srv restart + client session rebuild OK (screen recovered)");
    Some(())
}

/// GS-3 取证 (G5 surface 合成 / 多窗口): 合成器按 z 序把窗口表面合成到帧缓冲, 带边界裁剪。
///
/// 建两个**部分重叠**的窗口 (先建的 A 在下、后建的 B 在上), 各铺不同颜色, 然后逐点从**真的
/// 帧缓冲**回读 (`morion::gfx::read_screen_pixel`) 断言:
///   ① 重叠区显示**上层** B 的颜色 (z 序);
///   ② 非重叠区各显示自己的颜色 (窗口边界裁剪);
///   ③ 两个窗口之外是**桌面背景色** (合成器底色, 与控制台窗口底色不同 —— 可逐像素区分);
/// 再把 A **置顶** (`raise`) → 重叠区颜色**翻转**为 A。
///
/// 屏幕其余部分 (控制台窗口 0) 与真显示器无关, 全部断言都来自服务端回读帧缓冲, 无显示器可断言。
fn gs3_window_compositor() -> Option<()> {
    // 与 `gfx_srv` 的 `DESK_BG` 保持一致: 窗口外应是这个桌面底色。
    const DESK_BG: u32 = 0x20_28_38;
    const COL_A: u32 = 0xD0_30_30; // 下层: 红
    const COL_B: u32 = 0x30_60_D0; // 上层: 蓝

    let (sw, sh) = match morion::gfx::screen_size() {
        Some(s) => s,
        None => {
            println("app: GS3 screen_size FAILED");
            return None;
        }
    };

    // 两个部分重叠的窗口 (都在控制台窗口之上)。
    let a = match morion::gfx::Window::create(700, 300, 300, 300) {
        Some(w) => w,
        None => {
            println("app: GS3 window A create FAILED");
            return None;
        }
    };
    let b = match morion::gfx::Window::create(800, 400, 300, 300) {
        Some(w) => w,
        None => {
            println("app: GS3 window B create FAILED");
            return None;
        }
    };
    a.surface().fill(COL_A);
    b.surface().fill(COL_B);
    // 往表面里画完后上屏 (present = 重合成该窗口); 再整屏合成一次铺好桌面背景与所有窗口。
    if !a.present() || !b.present() {
        println("app: GS3 window present FAILED");
        return None;
    }
    if !morion::gfx::compose() {
        println("app: GS3 compose FAILED");
        return None;
    }

    // 重叠区 (两窗口都在): 上层 B 胜。
    if morion::gfx::read_screen_pixel(900, 500) != Some(COL_B) {
        println("app: GS3 overlap should show upper window FAILED");
        return None;
    }
    // 非重叠区各显其色 (窗口边界裁剪)。
    if morion::gfx::read_screen_pixel(720, 320) != Some(COL_A) {
        println("app: GS3 non-overlap A color FAILED");
        return None;
    }
    if morion::gfx::read_screen_pixel(1060, 660) != Some(COL_B) {
        println("app: GS3 non-overlap B color FAILED");
        return None;
    }
    // 窗口之外: 桌面背景色 (屏幕右下角, 在控制台窗口之外)。
    if morion::gfx::read_screen_pixel(sw - 4, sh - 4) != Some(DESK_BG) {
        println("app: GS3 outside-window should be desk background FAILED");
        return None;
    }

    // 把下层 A 置顶 → 重叠区颜色翻转。
    if !a.raise() {
        println("app: GS3 raise FAILED");
        return None;
    }
    if morion::gfx::read_screen_pixel(900, 500) != Some(COL_A) {
        println("app: GS3 overlap did not flip to raised window FAILED");
        return None;
    }

    println("app: GS3 window compositor OK (z-order + clipping verified on framebuffer)");
    Some(())
}

/// FS-29 取证: 服务实例退出后, 监督者 `init` 把它**原地**重启。
///
/// 三条一起才算过:
///   ① 退出是真的 —— 域 3 一度"没有存活任务" (引导域槽位还在, 所以只能问任务);
///   ② 重启后**域号仍是 3** —— 域号是 ABI (libvfs 里写死, shell 直接 `SendTo`),
///      所以监督者只能"原地重启", 不能换个新域号;
///   ③ 新实例**能正常服务** (`call` 得到 `tag + 1` 的回显), 且存活域数没有漂移。
///
/// 触发方式是给 echo 发一条控制消息让它自己 `SYS_EXIT` —— 走的是正常的退出即回收路径,
/// 不是内核杀进程。
fn fs29_supervisor_restart() -> Option<()> {
    const ECHO: u64 = 3;
    const INIT_TIMEOUT_MS: u64 = 4000;

    if sys_domain_alive(ECHO) == 0 {
        println("app: FS29 echo not alive before test FAILED");
        return None;
    }
    let base = sys_domain_count();

    // 单向控制消息: 不等回复 (echo 收到后直接退出, 不会有人 reply)。
    if sys_send(ECHO, ECHO_QUIT_TAG) != 1 {
        println("app: FS29 send quit to echo FAILED");
        return None;
    }
    if !wait_domain_alive(ECHO, false, 2000) {
        println("app: FS29 echo did not exit FAILED");
        return None;
    }
    if !wait_domain_alive(ECHO, true, INIT_TIMEOUT_MS) {
        println("app: FS29 init did not restart echo FAILED");
        return None;
    }
    if sys_call(ECHO, 0x5155) != 0x5156 {
        println("app: FS29 restarted echo does not serve FAILED");
        return None;
    }
    if sys_domain_count() != base {
        println("app: FS29 domain count drift FAILED");
        return None;
    }
    println("app: FS29 supervisor restart OK (echo exited, revived at domain 3)");
    Some(())
}

/// 轮询等待某域"有/没有存活任务", 最多等 `budget_ms`。
fn wait_domain_alive(domain: u64, want: bool, budget_ms: u64) -> bool {
    let mut waited = 0u64;
    while waited < budget_ms {
        if (sys_domain_alive(domain) != 0) == want {
            return true;
        }
        sys_sleep(20);
        waited += 20;
    }
    (sys_domain_alive(domain) != 0) == want
}

/// FS-28: 退出即回收。
///
/// `first` 是 FS-27 载入的那个域 —— 它的子程序 (`user/hello`) 打印一行后即返回 → `SYS_EXIT`,
/// 内核应当**自己**把该域销毁 (父域不调用 `SYS_DOMAIN_DESTROY`)。这里先等它退出, 断言
/// 存活域数回到 `alive_base`; 然后取此时**稳态**的空闲帧数作基线 (此刻子程序已回收、
/// app 自己的暂存缓冲也已就位, 不再变化), 反复"加载 → 等退出"若干轮。
/// 每一轮都要求: 域号复用成同一个 id、存活域数与空闲帧数都回到基线 —— 若销毁漏了帧,
/// 每轮都会再掉一批, 第一轮就会被抓出来。
fn fs28_domain_reclaim(first: u64, alive_base: u64) -> Option<()> {
    // 等子程序跑完自行退出 (它就是打印一行)。
    sys_sleep(100);

    // 退出即回收: 该域应已被内核销毁 —— 父域没做任何事。域数掉回去就是证据。
    if sys_domain_count() != alive_base {
        println("app: FS28 child domain not reclaimed on exit FAILED");
        return None;
    }
    // 稳态基线 (回收之后, app 的暂存缓冲也已分配完毕)。
    let frames_base = sys_frame_free();
    if frames_base == 0 {
        println("app: FS28 frame accounting FAILED");
        return None;
    }

    for _ in 0..8u32 {
        let domain = match morion::exec::spawn_file(HELLO_MEX) {
            Some(d) => d,
            None => {
                println("app: FS28 spawn FAILED");
                return None;
            }
        };
        if domain != first {
            println("app: FS28 domain id not reused FAILED");
            return None;
        }
        sys_sleep(100); // 让子程序跑完退出 → 内核回收
        if sys_domain_count() != alive_base {
            println("app: FS28 domain count drift FAILED");
            return None;
        }
        if sys_frame_free() != frames_base {
            println("app: FS28 frame leak after round FAILED");
            return None;
        }
    }

    print("app: FS28 exit-reclaim OK (domain ");
    print_u64(first);
    print(" reused 8x, frames stable at ");
    print_u64(frames_base);
    println(" free)");
    Some(())
}
