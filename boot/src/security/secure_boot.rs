//! Secure Boot 签名验证 — SM2 (国密 GB/T 32918.2)
//!
//! 在引导过程中验证内核映像和 initrd 的 SM2 数字签名。
//! 使用 RustCrypto `sm2` crate (no_std 纯 Rust); 验签过程不需要随机数。
//!
//! 签名方案 (GB/T 32918.2):
//!   - 消息摘要 e = SM3(ZA || M), ZA 由签名者用户标识 (distid) 与公钥导出
//!   - 签名值 (r, s) 各 32 字节大端, 依次拼接为 64 字节
//!   - 签名文件约定: 映像路径 + ".sm2sig"

use crate::config::entries::BootEntry;
use sm2::dsa::signature::Verifier;
use sm2::dsa::{Signature, VerifyingKey};

/// 签名验证结果
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyResult {
    Valid,
    Invalid,
    MissingSignature,
    Skipped,
    Error,
}

/// SM2 签名者用户标识 (distinguishing identifier)。
/// GB/T 32918.2 默认值, 签名侧与验证侧必须保持一致。
const SM2_DISTID: &str = "1234567812345678";

/// 内置验证公钥: SEC1 未压缩编码 (0x04 || X || Y, 共 65 字节)。
///
/// 当前为开发测试公钥: 对应私钥由 SM3("morion-sm2-dev-key-0001") (首字节清零
/// 保证小于曲线阶) 派生, 仅限开发环境自签自验。
/// 发布前必须替换为发行方正式公钥 (私钥离线保管), 轮换时更新本常量并重签
/// 所有映像。
const BUILTIN_PUBLIC_KEY: [u8; 65] = [
    0x04, 0xc9, 0x9d, 0xe7, 0xe0, 0x86, 0x4c, 0x34, 0xc3, 0xc3, 0xdc, 0x5d, 0x74, 0x0d, 0xce, 0xd6,
    0x96, 0x9a, 0xcc, 0x46, 0x2e, 0x95, 0xb9, 0x29, 0xdc, 0xca, 0x72, 0xdf, 0x14, 0x48, 0xc7, 0x7c,
    0x7c, 0x74, 0xd6, 0x8f, 0x16, 0x91, 0x6f, 0x91, 0x26, 0xc3, 0xb6, 0x5e, 0xd4, 0xe3, 0x91, 0x63,
    0xef, 0xa1, 0x1b, 0xaf, 0xdf, 0x12, 0x4a, 0x98, 0x8e, 0x1d, 0xb4, 0xee, 0x39, 0x08, 0x87, 0x9a,
    0x7a,
];

/// 签名验证器
pub struct SignatureVerifier {
    enforce: bool,
    verifying_key: Option<VerifyingKey>,
}

impl SignatureVerifier {
    /// 创建签名验证器
    pub fn new(enforce: bool) -> Self {
        let verifying_key = VerifyingKey::from_sec1_bytes(SM2_DISTID, &BUILTIN_PUBLIC_KEY).ok();
        Self {
            enforce,
            verifying_key,
        }
    }

    /// 验证引导条目的内核签名
    ///
    /// BootEntry 目前不携带签名数据; 加载器读到 `<映像>.sm2sig` 后调用
    /// `verify_raw`。按 enforce 策略: 未开启时跳过, 开启但无签名数据视为缺失。
    pub fn verify_entry(&self, _entry: &BootEntry) -> VerifyResult {
        if !self.enforce {
            return VerifyResult::Skipped;
        }
        VerifyResult::MissingSignature
    }

    /// 验证原始数据的 SM2 签名 (r || s, 64 字节)
    pub fn verify_raw(&self, data: &[u8], signature: &[u8; 64]) -> VerifyResult {
        if !self.enforce {
            return VerifyResult::Skipped;
        }
        let verifying_key = match &self.verifying_key {
            Some(key) => key,
            None => return VerifyResult::Error, // 内置公钥未烧录或编码非法
        };
        match Signature::from_slice(signature)
            .ok()
            .map(|sig| verifying_key.verify(data, &sig))
        {
            Some(Ok(())) => VerifyResult::Valid,
            Some(Err(_)) => VerifyResult::Invalid,
            None => VerifyResult::Invalid, // 签名编码非法
        }
    }

    /// 检查 Secure Boot 是否处于 Setup Mode (待接 UEFI SecureBoot 协议查询)
    pub fn is_setup_mode(&self) -> bool {
        false
    }
}