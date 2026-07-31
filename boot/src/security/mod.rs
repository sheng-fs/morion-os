//! 安全链模块 — 完整可信计算链条的引导端实现
//!
//! 实现：
//!   - Secure Boot 映像签名验证 (Ed25519)
//!   - TPM 2.0 PCR 测量与验证
//!   - 自加密镜像的密钥解封
//!   - 飞地预认证 (驱动库哈希测量)
//!
//! 注: 密码学模块需要 feature="crypto" 启用。
//! 当前版本使用桩实现，不需要外部密码库。

pub mod secure_boot;
pub mod tpm;
pub mod hash;

pub use secure_boot::SignatureVerifier;
pub use tpm::TpmMeasurer;
pub use hash::ImageHasher;