//! libvfs — 用户态文件系统客户端库 (阶段 3)
//!
//! 把 `open` / `read` / `readdir` / `close` 封装成对 fat32_srv (域 6) 的同步
//! IPC 调用。数据经共享结果页 `RESULT_BUF` 零拷贝回传 (IPC 仅传控制信息与
//! 返回字节数), 与微内核「数据走共享内存、控制走消息」的约定一致。

use crate::syscall::sys_call_payload;

/// fat32 文件服务域 id (与内核 `main.rs` 创建顺序一致)。
pub const FAT32_DOMAIN: u64 = 6;

/// 结果页虚拟地址: app 分配并共享给 fat32_srv, fat32_srv 在此写入文件内容或
/// 目录列表。`read` / `readdir` 返回的字节数即该页内的有效数据长度。
/// 地址须避开程序镜像 / 用户栈 / NVMe MMIO 区域, 与 fat32_srv 缓冲页同置于 1MB 偏移处。
pub const RESULT_BUF: u64 = 0x0000_0080_0010_4000;

/// 写缓冲页虚拟地址: app 把要写的数据拷入此页 (已共享给 fat32_srv), 再发
/// `write` 请求; fat32_srv 从此页读取数据写盘。最多一页 (4096 字节)。
pub const WRITE_BUF: u64 = 0x0000_0080_0010_5000;

/// VFS 操作 tag (4 字节 ASCII, 与 fat32_srv 服务循环的分发一致)。
pub const VFS_OPEN_TAG: u64 = 0x4F50_454E; // "OPEN"
pub const VFS_READ_TAG: u64 = 0x5245_4144; // "READ"
pub const VFS_READDIR_TAG: u64 = 0x5244_4952; // "RDIR"
pub const VFS_CLOSE_TAG: u64 = 0x434C_5345; // "CLSE"
pub const VFS_WRITE_TAG: u64 = 0x5752_4954; // "WRIT"

/// READ 请求 (序列化进 IPC payload 前 12 字节)。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ReadReq {
    pub fd: u32,
    pub offset: u32,
    pub count: u32,
}

/// WRITE 请求 (序列化进 IPC payload 前 12 字节)。数据在共享写缓冲 `WRITE_BUF` 中。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct WriteReq {
    pub fd: u32,
    pub offset: u32,
    pub count: u32,
}

/// 结构化目录条目 — `readdir` 写入 `RESULT_BUF` 的固定大小记录。
/// 返回字节数 = 条目数 × `size_of::<DirEntry>()`。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct DirEntry {
    /// 原始 8.3 短名: 主名 8 字节 + 扩展名 3 字节, 空格填充 (无 '.' 分隔)。
    pub name: [u8; 11],
    /// 文件大小 (目录为 0)。
    pub size: u32,
    /// 1 = 目录, 0 = 普通文件。
    pub is_dir: u32,
}

/// 打开文件或目录, 成功返回 fd (0..), 失败返回 `u64::MAX`。
pub fn open(path: &str) -> u64 {
    let mut payload = [0u8; 32];
    let n = path.len().min(31);
    payload[..n].copy_from_slice(&path.as_bytes()[..n]);
    // payload 其余字节为 0, 故路径恒以 NUL 结尾 (服务端按 NUL 取长度)。
    sys_call_payload(FAT32_DOMAIN, VFS_OPEN_TAG, &payload)
}

/// 从 fd 的 `offset` 起读最多 `count` 字节到 `RESULT_BUF`。
/// 返回实际读取字节数, 失败返回 `u64::MAX`。
pub fn read(fd: u64, offset: u32, count: u32) -> u64 {
    let req = ReadReq {
        fd: fd as u32,
        offset,
        count,
    };
    let payload = unsafe {
        core::slice::from_raw_parts(
            &req as *const ReadReq as *const u8,
            core::mem::size_of::<ReadReq>(),
        )
    };
    sys_call_payload(FAT32_DOMAIN, VFS_READ_TAG, payload)
}

/// 把 `data` (最多一页) 拷入共享写缓冲 `WRITE_BUF`, 并从 fd 的 `offset` 起写
/// `data.len()` 字节。返回实际写入字节数, 失败返回 `u64::MAX`。
pub fn write(fd: u64, offset: u32, data: &[u8]) -> u64 {
    let n = data.len().min(4096);
    unsafe {
        core::ptr::copy_nonoverlapping(data.as_ptr(), WRITE_BUF as *mut u8, n);
    }
    let req = WriteReq {
        fd: fd as u32,
        offset,
        count: n as u32,
    };
    let payload = unsafe {
        core::slice::from_raw_parts(
            &req as *const WriteReq as *const u8,
            core::mem::size_of::<WriteReq>(),
        )
    };
    sys_call_payload(FAT32_DOMAIN, VFS_WRITE_TAG, payload)
}

/// 列出 fd 指向目录的条目, 以 `DirEntry` 记录数组写入 `RESULT_BUF`。
/// 返回写入字节数 (= 条目数 × `size_of::<DirEntry>()`), 失败返回 `u64::MAX`。
pub fn readdir(fd: u64) -> u64 {
    let payload = (fd as u32).to_le_bytes();
    sys_call_payload(FAT32_DOMAIN, VFS_READDIR_TAG, &payload)
}

/// 关闭 fd, 成功返回 1, 失败返回 0。
pub fn close(fd: u64) -> u64 {
    let payload = (fd as u32).to_le_bytes();
    sys_call_payload(FAT32_DOMAIN, VFS_CLOSE_TAG, &payload)
}
