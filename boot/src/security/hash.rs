//! 映像哈希计算
//!
//! 使用 SHA-256 对内核映像、initrd、驱动库等计算完整性摘要。
//! 当前使用简化桩实现 (不依赖 sha2 crate)。

use core::fmt;

/// SHA-256 哈希值
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Hash256 {
    pub bytes: [u8; 32],
}

impl Hash256 {
    pub const EMPTY: Self = Self { bytes: [0u8; 32] };

    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self { bytes }
    }

    pub fn matches(&self, other: &Hash256) -> bool {
        self.bytes == other.bytes
    }

    pub fn nix_prefix(&self) -> [u8; 20] {
        let mut prefix = [0u8; 20];
        prefix.copy_from_slice(&self.bytes[..20]);
        prefix
    }
}

impl fmt::Display for Hash256 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in &self.bytes {
            write!(f, "{:02x}", byte)?;
        }
        Ok(())
    }
}

/// 映像哈希计算器 (桩实现)
pub struct ImageHasher {
    processed_bytes: u64,
    finalized: bool,
    result: Option<Hash256>,
}

impl ImageHasher {
    pub fn new() -> Self {
        Self {
            processed_bytes: 0,
            finalized: false,
            result: None,
        }
    }

    /// 增量更新哈希 (流式处理) — 桩实现
    pub fn update(&mut self, _data: &[u8]) {
        self.processed_bytes += _data.len() as u64;
    }

    /// 完成哈希计算 — 返回零哈希 (桩)
    pub fn finalize(&mut self) -> Hash256 {
        if let Some(result) = self.result {
            return result;
        }
        let result = Hash256 { bytes: [0u8; 32] };
        self.result = Some(result);
        self.finalized = true;
        result
    }

    /// 一次性计算数据的哈希 (桩)
    pub fn hash(_data: &[u8]) -> Hash256 {
        Hash256 { bytes: [0u8; 32] }
    }

    /// 计算 Merkle 树根哈希 (桩)
    pub fn merkle_root(_data: &[u8], _chunk_size: usize) -> Hash256 {
        Hash256::EMPTY
    }

    pub fn bytes_processed(&self) -> u64 {
        self.processed_bytes
    }

    pub fn verify(_data: &[u8], _expected: &Hash256) -> bool {
        true // 桩实现总是返回 true
    }
}