//! 固件加载适配器。
//!
//! 真实网卡固件有版权且体积从几十 KB 到几 MB 不等，本 crate **不附带任何镜像**。
//! 这里只提供三个与来源无关的 [`FirmwareLoader`] 实现，厂商 HAL 可以按自己的习惯挑：
//!
//! - [`SliceFirmware`]：直接引用一段固定字节切片（内置于仓库的测试用小镜像、编译期
//!   嵌入的数组等）；
//! - [`FirmwareBuffer<N>`]：把上游 loader 的固件读进固定容量缓冲后，自己作为 loader
//!   继续提供字节（避免 HAL 重复访问文件系统）；
//! - [`ChunkedLoader<N>`]：在 [`FirmwareBuffer`] 之上再提供"按块取"的游标，适合把
//!   固件分块写进芯片寄存器的 HAL。
//!
//! 所有实现都只用固定容量缓冲，越界一律返回 [`DriverError::Invalid`]，
//! 不依赖堆分配。

use libdevmodel::{DriverError, FirmwareLoader};

/// 固件名的最大长度（[`FirmwareBuffer`] 内部保存名字用）。
pub const FIRMWARE_NAME_MAX: usize = 32;

/// 固定字节切片固件。
///
/// 典型用法：内置在驱动里的测试镜像，或服务层已经读进静态数组的固件。
pub struct SliceFirmware<'a> {
    name: &'a str,
    data: &'a [u8],
}

impl<'a> SliceFirmware<'a> {
    /// `name` 是 HAL 请求固件时使用的名字，`data` 是固件字节。
    pub const fn new(name: &'a str, data: &'a [u8]) -> Self {
        Self { name, data }
    }

    /// 固件长度。
    pub fn len(&self) -> usize {
        self.data.len()
    }

    /// 是否为空镜像。
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// 固件原始字节。
    pub fn as_slice(&self) -> &'a [u8] {
        self.data
    }
}

impl<'a> FirmwareLoader for SliceFirmware<'a> {
    fn load(&mut self, name: &str, out: &mut [u8]) -> Result<usize, DriverError> {
        if name != self.name {
            return Err(DriverError::NotFound);
        }
        if out.len() < self.data.len() {
            return Err(DriverError::Invalid);
        }
        out[..self.data.len()].copy_from_slice(self.data);
        Ok(self.data.len())
    }
}

/// 固定容量固件缓冲。
///
/// 先用 [`FirmwareBuffer::fetch`] 把上游 loader 的镜像读进来，之后它本身就是一个
/// [`FirmwareLoader`]（只认同一个名字），可以反复交给 HAL 使用而不用再碰文件系统。
/// 超过 `N` 字节的固件返回 [`DriverError::Invalid`]，不会截断。
pub struct FirmwareBuffer<const N: usize> {
    data: [u8; N],
    len: usize,
    name: [u8; FIRMWARE_NAME_MAX],
    name_len: usize,
}

impl<const N: usize> FirmwareBuffer<N> {
    /// 空缓冲。
    pub const fn new() -> Self {
        Self {
            data: [0; N],
            len: 0,
            name: [0; FIRMWARE_NAME_MAX],
            name_len: 0,
        }
    }

    /// 从 `loader` 读入固件 `name`，返回字节数。
    ///
    /// 固件超过 `N`、或名字超过 [`FIRMWARE_NAME_MAX`]，返回 [`DriverError::Invalid`]；
    /// 失败时缓冲保持空。
    pub fn fetch(
        &mut self,
        loader: &mut dyn FirmwareLoader,
        name: &str,
    ) -> Result<usize, DriverError> {
        self.len = 0;
        self.name_len = 0;
        let name_bytes = name.as_bytes();
        if name_bytes.len() > FIRMWARE_NAME_MAX {
            return Err(DriverError::Invalid);
        }
        let n = loader.load(name, &mut self.data)?;
        if n > N {
            return Err(DriverError::Invalid);
        }
        self.name[..name_bytes.len()].copy_from_slice(name_bytes);
        self.name_len = name_bytes.len();
        self.len = n;
        Ok(n)
    }

    /// 已缓存的固件长度。
    pub fn len(&self) -> usize {
        self.len
    }

    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// 已缓存的固件字节。
    pub fn as_slice(&self) -> &[u8] {
        &self.data[..self.len]
    }

    /// 清空缓冲（同时忘掉名字）。
    pub fn clear(&mut self) {
        self.len = 0;
        self.name_len = 0;
    }

    /// 从 `offset` 起读 `out.len()` 字节；越界返回 [`DriverError::Invalid`]。
    pub fn read(&self, offset: usize, out: &mut [u8]) -> Result<usize, DriverError> {
        let end = offset.checked_add(out.len()).ok_or(DriverError::Invalid)?;
        if end > self.len {
            return Err(DriverError::Invalid);
        }
        out.copy_from_slice(&self.data[offset..end]);
        Ok(out.len())
    }
}

impl<const N: usize> Default for FirmwareBuffer<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> FirmwareLoader for FirmwareBuffer<N> {
    fn load(&mut self, name: &str, out: &mut [u8]) -> Result<usize, DriverError> {
        if self.name_len == 0 || &self.name[..self.name_len] != name.as_bytes() {
            return Err(DriverError::NotFound);
        }
        if out.len() < self.len {
            return Err(DriverError::Invalid);
        }
        out[..self.len].copy_from_slice(&self.data[..self.len]);
        Ok(self.len)
    }
}

/// 带游标的分块固件加载器。
///
/// 先用 [`ChunkedLoader::prefetch`] 把固件读进内部固定缓冲，再用
/// [`ChunkedLoader::next_chunk`] 逐块取出；`next_chunk` 返回 `0` 表示取完。
/// 越界（固件大于 `N`）由 `prefetch` 返回 [`DriverError::Invalid`]。
///
/// 它也实现 [`FirmwareLoader`]：`load` 会重新预读并一次性返回整包（用于把本适配器
/// 继续传给下层代码）。
pub struct ChunkedLoader<'a, const N: usize> {
    upstream: &'a mut dyn FirmwareLoader,
    buf: FirmwareBuffer<N>,
    cursor: usize,
}

impl<'a, const N: usize> ChunkedLoader<'a, N> {
    /// 包装一个上游 loader。
    pub fn new(upstream: &'a mut dyn FirmwareLoader) -> Self {
        Self {
            upstream,
            buf: FirmwareBuffer::new(),
            cursor: 0,
        }
    }

    /// 把固件 `name` 读进固定缓冲，返回字节数；游标归零。
    pub fn prefetch(&mut self, name: &str) -> Result<usize, DriverError> {
        let n = self.buf.fetch(&mut *self.upstream, name)?;
        self.cursor = 0;
        Ok(n)
    }

    /// 已预读的固件长度。
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// 还剩多少字节没取。
    pub fn remaining(&self) -> usize {
        self.buf.len() - self.cursor
    }

    /// 游标归零。
    pub fn rewind(&mut self) {
        self.cursor = 0;
    }

    /// 取下一块到 `out`，返回字节数；`0` 表示已取完。
    pub fn next_chunk(&mut self, out: &mut [u8]) -> Result<usize, DriverError> {
        if out.is_empty() {
            return Ok(0);
        }
        let n = core::cmp::min(self.remaining(), out.len());
        out[..n].copy_from_slice(&self.buf.as_slice()[self.cursor..self.cursor + n]);
        self.cursor += n;
        Ok(n)
    }

    /// 查看内部缓冲。
    pub fn buffer(&self) -> &FirmwareBuffer<N> {
        &self.buf
    }
}

impl<'a, const N: usize> FirmwareLoader for ChunkedLoader<'a, N> {
    fn load(&mut self, name: &str, out: &mut [u8]) -> Result<usize, DriverError> {
        let n = self.prefetch(name)?;
        if out.len() < n {
            return Err(DriverError::Invalid);
        }
        out[..n].copy_from_slice(&self.buf.as_slice()[..n]);
        Ok(n)
    }
}
