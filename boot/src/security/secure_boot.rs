//! Secure Boot 签名验证 (桩实现)
//!
//! 在引导过程中验证内核映像和 initrd 的 Ed25519 数字签名。
//! 当前使用桩实现，实际验证功能待加密 feature 启用。

use crate::config::entries::BootEntry;

/// 签名验证结果
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyResult {
    Valid,
    Invalid,
    MissingSignature,
    Skipped,
    Error,
}

/// 签名验证器
pub struct SignatureVerifier {
    enforce: bool,
}

impl SignatureVerifier {
    /// 创建签名验证器
    pub fn new(enforce: bool) -> Self {
        Self { enforce }
    }

    /// 验证引导条目的内核签名 (桩)
    pub fn verify_entry(&self, _entry: &BootEntry) -> VerifyResult {
        if !self.enforce {
            return VerifyResult::Skipped;
        }
        VerifyResult::Skipped // 桩: 签名验证尚未实现
    }

    /// 验证原始数据的签名 (桩)
    pub fn verify_raw(&self, _data: &[u8], _signature: &[u8; 64]) -> VerifyResult {
        VerifyResult::Skipped
    }

    /// 检查 Secure Boot 是否处于 Setup Mode
    pub fn is_setup_mode(&self) -> bool {
        false
    }
}