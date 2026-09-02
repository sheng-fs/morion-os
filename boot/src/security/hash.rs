//! 映像哈希计算
//!
//! 使用 SM3 (国密 GB/T 32905) 对内核映像、initrd、驱动库等计算完整性摘要。
//! 基于 RustCrypto `sm3` crate (no_std 纯 Rust)。

use alloc::vec::Vec;
use core::fmt;
use sm3::{Digest, Sm3};

/// SM3 哈希值 (256 位)
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

/// 把 `sm3` 的 32 字节输出转换为 `Hash256`。
fn digest_to_hash(digest: &[u8]) -> Hash256 {
    let mut bytes = [0u8; 32];
    bytes.copy_from_slice(digest);
    Hash256 { bytes }
}

/// 映像哈希计算器 (SM3)。
pub struct ImageHasher {
    hasher: Sm3,
    processed_bytes: u64,
    finalized: bool,
    result: Option<Hash256>,
}

impl ImageHasher {
    pub fn new() -> Self {
        Self {
            hasher: Sm3::new(),
            processed_bytes: 0,
            finalized: false,
            result: None,
        }
    }

    /// 增量更新哈希 (流式处理)。
    pub fn update(&mut self, data: &[u8]) {
        self.hasher.update(data);
        self.processed_bytes += data.len() as u64;
    }

    /// 完成哈希计算。多次调用返回同一结果。
    pub fn finalize(&mut self) -> Hash256 {
        if let Some(result) = self.result {
            return result;
        }
        let digest = self.hasher.clone().finalize();
        let result = digest_to_hash(digest.as_slice());
        self.result = Some(result);
        self.finalized = true;
        result
    }

    /// 一次性计算数据的 SM3 哈希。
    pub fn hash(data: &[u8]) -> Hash256 {
        let mut hasher = Sm3::new();
        hasher.update(data);
        digest_to_hash(hasher.finalize().as_slice())
    }

    /// 计算二进制 Merkle 树根哈希:
    /// 按 `chunk_size` 分块求叶哈希, 再逐层两两拼接求父哈希, 奇数节点上提。
    pub fn merkle_root(data: &[u8], chunk_size: usize) -> Hash256 {
        if data.is_empty() {
            return Self::hash(&[]);
        }
        let cs = if chunk_size == 0 { data.len() } else { chunk_size };
        let mut level: Vec<Hash256> = data.chunks(cs).map(Self::hash).collect();

        while level.len() > 1 {
            let mut next = Vec::with_capacity((level.len() + 1) / 2);
            let mut it = level.chunks_exact(2);
            for pair in &mut it {
                let mut buf = [0u8; 64];
                buf[..32].copy_from_slice(&pair[0].bytes);
                buf[32..].copy_from_slice(&pair[1].bytes);
                next.push(Self::hash(&buf));
            }
            let rem = it.remainder();
            if !rem.is_empty() {
                next.push(rem[0]);
            }
            level = next;
        }
        level[0]
    }

    pub fn bytes_processed(&self) -> u64 {
        self.processed_bytes
    }

    pub fn verify(data: &[u8], expected: &Hash256) -> bool {
        Self::hash(data).matches(expected)
    }
}
