//! TPM 2.0 测量与验证 (桩实现)
//!
//! 在引导过程中将每个加载项的哈希扩展进 PCR，构建完整可信计算链条。
//! 当前版本使用桩实现，不依赖 TCG2 协议。

/// TPM 2.0 测量器
pub struct TpmMeasurer {
    enabled: bool,
    tcg2_available: bool,
    measurements_count: u32,
}

impl TpmMeasurer {
    pub fn new() -> Self {
        Self {
            enabled: false,
            tcg2_available: false,
            measurements_count: 0,
        }
    }

    /// 尝试初始化 TPM 测量 (桩)
    pub fn initialize(&mut self, _boot_services: &uefi::table::boot::BootServices) -> Result<(), &'static str> {
        // 桩实现: 始终报告 TPM 未检测到
        Ok(())
    }

    /// 将数据哈希扩展到指定的 PCR (桩)
    pub fn extend_pcr(&mut self, _pcr_index: u32, _data: &[u8]) -> Option<[u8; 32]> {
        self.measurements_count += 1;
        Some([0u8; 32])
    }

    /// 测量引导器自身
    pub fn measure_boot_loader(&mut self) {
        self.measurements_count += 1;
    }

    /// 测量内核映像
    pub fn measure_kernel(&mut self, _kernel_data: &[u8]) -> Option<[u8; 32]> {
        self.measurements_count += 1;
        Some([0u8; 32])
    }

    /// 测量 initrd
    pub fn measure_initrd(&mut self, _initrd_data: &[u8]) -> Option<[u8; 32]> {
        self.measurements_count += 1;
        Some([0u8; 32])
    }

    /// 测量飞地驱动库
    pub fn measure_enclave_library(&mut self, _lib_data: &[u8]) -> Option<[u8; 32]> {
        self.measurements_count += 1;
        Some([0u8; 32])
    }

    /// 测量引导配置
    pub fn measure_boot_config(&mut self, _config_data: &[u8]) -> Option<[u8; 32]> {
        self.measurements_count += 1;
        Some([0u8; 32])
    }

    pub fn measurements_count(&self) -> u32 {
        self.measurements_count
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }
}