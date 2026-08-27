//! 引导条目管理 — Nix 式声明式多代并存
//!
//! 每个可启动的系统/版本都是一个 Nix derivation (闭包)，
//! 引导菜单通过读取声明式条目文件动态生成。
//!
//! 条目文件格式 (Nix 风格):
//! ```ini
//! [entry]
//! title = "Morion OS"
//! version = "0.1.0-alpha"
//! hash = "abc123def456..."
//! path = "/nix/store/abc123-system-boot"
//! generation = 42
//!
//! [entry.kernel]
//! path = "/nix/store/abc123-system-boot/vmlinuz"
//! cmdline = "quiet loglevel=3"
//!
//! [entry.initrd]
//! path = "/nix/store/abc123-system-boot/initrd.img"
//!
//! [entry.security]
//! signature = "/nix/store/abc123-system-boot/kernel.sig"
//! pcr_policy = "sha256:0=abc...,4=def..."
//!
//! [entry.enclave]
//! passthrough_devices = ["0000:01:00.0"]
//! ```

use crate::gfx::framebuffer::Color;
use alloc::string::String;
use alloc::vec::Vec;

/// 引导条目类型
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryType {
    /// 当前默认世代
    CurrentGeneration,
    /// 历史世代 (可回滚)
    PreviousGeneration,
    /// 最新安装 (待首次启动确认)
    Latest,
    /// 救援模式 (特殊的 micro-桌面)
    Rescue,
    /// 其他操作系统 (通过 os-prober 检测)
    OtherOS,
    /// UEFI 固件设置
    UefiSettings,
}

/// 单个引导条目
#[derive(Debug, Clone)]
pub struct BootEntry {
    /// 显示标题
    pub title: String,
    /// OS 版本号
    pub version: String,
    /// Nix store 哈希路径
    pub store_hash: String,
    /// 世代编号 (数字越大越新)
    pub generation: u64,
    /// 条目类型
    pub entry_type: EntryType,
    /// 内核映像路径 (Nix store 中)
    pub kernel_path: String,
    /// 内核命令行参数
    pub kernel_cmdline: String,
    /// initrd 路径
    pub initrd_path: String,
    /// 签名文件路径 (Secure Boot 用)
    pub signature_path: Option<String>,
    /// PCR 策略 (TPM 测量要求)
    pub pcr_policy: Option<String>,
    /// 设备直通列表 (飞地用)
    pub passthrough_devices: Vec<String>,
    /// 是否可自动启动
    pub auto_boot: bool,
    /// 是否已验证签名
    pub signature_verified: bool,
    /// 是否已验证 PCR
    pub pcr_verified: bool,
}

impl BootEntry {
    /// 创建新的引导条目
    pub fn new(title: &str, gen: u64, entry_type: EntryType) -> Self {
        Self {
            title: title.into(),
            version: String::new(),
            store_hash: String::new(),
            generation: gen,
            entry_type,
            kernel_path: String::new(),
            kernel_cmdline: String::new(),
            initrd_path: String::new(),
            signature_path: None,
            pcr_policy: None,
            passthrough_devices: Vec::new(),
            auto_boot: matches!(entry_type, EntryType::CurrentGeneration),
            signature_verified: false,
            pcr_verified: false,
        }
    }

    /// 获取条目类型对应的图标颜色 (用于菜单渲染)
    pub fn type_color(&self) -> Color {
        match self.entry_type {
            EntryType::CurrentGeneration => Color::GREEN,
            EntryType::PreviousGeneration => Color::WHITE,
            EntryType::Latest => Color { red: 0x3A, green: 0x6A, blue: 0xFF, alpha: 255 },
            EntryType::Rescue => Color { red: 0xFF, green: 0xA5, blue: 0x00, alpha: 255 },
            EntryType::OtherOS => Color { red: 0x88, green: 0x88, blue: 0x88, alpha: 255 },
            EntryType::UefiSettings => Color { red: 0xAA, green: 0xAA, blue: 0xAA, alpha: 255 },
        }
    }

    /// 是否可被选择启动
    pub fn bootable(&self) -> bool {
        use EntryType::*;
        match self.entry_type {
            CurrentGeneration | PreviousGeneration | Latest | Rescue | OtherOS => true,
            _ => false,
        }
    }
}

/// 多代管理器 — Nix 风格的世代追踪
pub struct GenerationManager {
    /// 所有可引导条目 (按世代降序排列)
    entries: [Option<BootEntry>; 32],
    /// 活跃条目数量
    count: usize,
    /// 当前默认条目索引
    default_index: usize,
    /// 上次启动的条目索引
    last_booted_index: Option<usize>,
}

impl GenerationManager {
    pub fn new() -> Self {
        Self {
            entries: [const { None }; 32],
            count: 0,
            default_index: 0,
            last_booted_index: None,
        }
    }

    /// 添加一个引导条目
    pub fn add_entry(&mut self, entry: BootEntry) {
        if self.count >= self.entries.len() {
            return;
        }
        // 按世代降序插入
        let pos = self.entries[..self.count]
            .iter()
            .position(|e| e.as_ref().map_or(true, |e| entry.generation > e.generation))
            .unwrap_or(self.count);

        for i in (pos..self.count).rev() {
            self.entries[i + 1] = self.entries[i].take();
        }
        self.entries[pos] = Some(entry);
        self.count += 1;

        // 更新默认索引为当前世代 (首次)
        self.update_defaults();
    }

    fn update_defaults(&mut self) {
        // 找到标记为 CurrentGeneration 的条目
        for i in 0..self.count {
            if let Some(ref entry) = self.entries[i] {
                if entry.entry_type == EntryType::CurrentGeneration {
                    self.default_index = i;
                    return;
                }
            }
        }
    }

    /// 获取条目数量
    pub fn len(&self) -> usize {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// 获取指定条目
    pub fn get(&self, index: usize) -> Option<&BootEntry> {
        self.entries.get(index).and_then(|e| e.as_ref())
    }

    /// 获取默认条目
    pub fn default_entry(&self) -> Option<&BootEntry> {
        self.get(self.default_index)
    }

    /// 获取上次启动的条目
    pub fn last_booted(&self) -> Option<&BootEntry> {
        self.last_booted_index
            .and_then(|i| self.get(i))
    }

    /// 标记某条目为已启动
    pub fn mark_booted(&mut self, index: usize) {
        self.last_booted_index = Some(index);
    }

    /// 从声明式条目文件目录构建
    ///
    /// 扫描 `<config_dir>/entries/*.conf`，
    /// 解析每个文件为一个 BootEntry。
    pub fn from_config_dir(_config_dir: &str) -> Self {
        let mut mgr = Self::new();

        // 默认创建救援模式和固件设置条目
        mgr.add_entry(BootEntry {
            title: "救援模式 (Recovery)".into(),
            version: "built-in".into(),
            store_hash: "built-in".into(),
            generation: 0,
            entry_type: EntryType::Rescue,
            kernel_path: String::new(),
            kernel_cmdline: "rescue single".into(),
            initrd_path: String::new(),
            signature_path: None,
            pcr_policy: None,
            passthrough_devices: Vec::new(),
            auto_boot: false,
            signature_verified: true,
            pcr_verified: true,
        });

        mgr.add_entry(BootEntry {
            title: "UEFI 固件设置".into(),
            version: "firmware".into(),
            store_hash: "built-in".into(),
            generation: u64::MAX, // 始终排在最后
            entry_type: EntryType::UefiSettings,
            kernel_path: String::new(),
            kernel_cmdline: String::new(),
            initrd_path: String::new(),
            signature_path: None,
            pcr_policy: None,
            passthrough_devices: Vec::new(),
            auto_boot: false,
            signature_verified: true,
            pcr_verified: true,
        });

        mgr
    }

    /// 列出所有条目的标题 (用于菜单渲染)
    pub fn list_titles(&self) -> impl Iterator<Item = &str> {
        self.entries[..self.count]
            .iter()
            .flatten()
            .map(|e| e.title.as_str())
    }

    /// 找到 PreviousGeneration 条目的索引范围
    pub fn rollback_candidates(&self) -> Vec<usize> {
        self.entries[..self.count]
            .iter()
            .enumerate()
            .filter_map(|(i, e)| {
                e.as_ref().and_then(|entry| {
                    if entry.entry_type == EntryType::PreviousGeneration
                        || entry.entry_type == EntryType::Latest
                    {
                        Some(i)
                    } else {
                        None
                    }
                })
            })
            .collect()
    }
}
