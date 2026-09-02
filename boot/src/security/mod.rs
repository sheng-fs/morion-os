//! 安全链模块 — 完整可信计算链条的引导端实现
//!
//! 实现：
//!   - Secure Boot 映像签名验证 (SM2, 国密 GB/T 32918.2)
//!   - TPM 2.0 PCR 测量与验证 (桩, 待接 EFI_TCG2_PROTOCOL)
//!   - 自加密镜像的密钥解封
//!   - 飞地预认证 (驱动库哈希测量)
//!
//! 注: SM3 映像哈希与 SM2 验签已启用 (RustCrypto no_std 纯 Rust)。

pub mod secure_boot;
pub mod tpm;
pub mod hash;

pub use secure_boot::SignatureVerifier;
pub use tpm::TpmMeasurer;
pub use hash::ImageHasher;