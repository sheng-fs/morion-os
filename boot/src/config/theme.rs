//! 主题配置解析器
//!
//! 解析 theme.toml 配置文件，提供声明式的 UI 参数。
//! 编译期嵌入默认主题，运行时可从 ESP 加载覆盖。
//! 所有路径指向 Nix store 中的不可变资源。

use crate::gfx::framebuffer::Color;
use alloc::string::String;
use alloc::vec::Vec;

/// 主题配置顶层结构 — 完全映射 theme.toml 的所有 section
#[derive(Debug, Clone)]
pub struct ThemeConfig {
    /// [theme] 元数据
    pub meta: ThemeMeta,
    /// [render] 渲染参数
    pub render: RenderConfig,
    /// [splash] 启动画面
    pub splash: SplashConfig,
    /// [logo] 系统 logo
    pub logo: LogoConfig,
    /// [background] 背景配置
    pub background: BackgroundConfig,
    /// [menu] 菜单面板
    pub menu: MenuConfig,
    /// [scroll] 滚动指示器
    pub scroll: ScrollConfig,
    /// [cursor] 鼠标光标
    pub cursor: CursorConfig,
    /// [loading] 加载动画
    pub loading: LoadingConfig,
    /// [progress] 进度条
    pub progress: ProgressConfig,
    /// [security] 安全指示器图标路径
    pub security: SecurityIconPaths,
    /// [dialog] 对话框
    pub dialog: DialogConfig,
    /// [icons.system] 系统类型图标
    pub system_icons: SystemIcons,
    /// [icons.ui] 操作图标
    pub ui_icons: UiIconPaths,
    /// [power] 电源图标
    pub power_icons: PowerIconPaths,
    /// [timeout] 超时配置
    pub timeout: TimeoutConfig,
}

impl Default for ThemeConfig {
    fn default() -> Self {
        // 默认值完全对应 theme.toml 的内容
        Self {
            meta: ThemeMeta::default(),
            render: RenderConfig::default(),
            splash: SplashConfig::default(),
            logo: LogoConfig::default(),
            background: BackgroundConfig::default(),
            menu: MenuConfig::default(),
            scroll: ScrollConfig::default(),
            cursor: CursorConfig::default(),
            loading: LoadingConfig::default(),
            progress: ProgressConfig::default(),
            security: SecurityIconPaths::default(),
            dialog: DialogConfig::default(),
            system_icons: SystemIcons::default(),
            ui_icons: UiIconPaths::default(),
            power_icons: PowerIconPaths::default(),
            timeout: TimeoutConfig::default(),
        }
    }
}

// ============================================================
// [theme]
// ============================================================
#[derive(Debug, Clone)]
pub struct ThemeMeta {
    pub name: String,
    pub author: String,
    pub version: String,
    pub description: String,
}

impl Default for ThemeMeta {
    fn default() -> Self {
        Self {
            name: "default".into(),
            author: "Morion OS".into(),
            version: "1.0.0".into(),
            description: "Declarative immutable micro-bootloader theme".into(),
        }
    }
}

// ============================================================
// [render]
// ============================================================
#[derive(Debug, Clone)]
pub struct RenderConfig {
    pub resolution: String,
    pub force_gop: bool,
    pub fallback_text_mode: bool,
    pub acrylic_blur_strength: u8,
    pub ui_scale: f32,
    pub animation_enabled: bool,
    pub animation_speed: f32,
}

impl Default for RenderConfig {
    fn default() -> Self {
        Self {
            resolution: "auto".into(),
            force_gop: true,
            fallback_text_mode: true,
            acrylic_blur_strength: 3,
            ui_scale: 1.0,
            animation_enabled: true,
            animation_speed: 1.0,
        }
    }
}

// ============================================================
// [splash]
// ============================================================
#[derive(Debug, Clone)]
pub struct SplashConfig {
    pub background: String,
    pub logo: String,
    pub align: String,
    pub duration_ms: u64,
    pub fade_in_ms: u64,
    pub fade_out_ms: u64,
}

impl Default for SplashConfig {
    fn default() -> Self {
        Self {
            background: "resources/splash/background.png".into(),
            logo: "resources/splash/logo.png".into(),
            align: "center".into(),
            duration_ms: 1200,
            fade_in_ms: 300,
            fade_out_ms: 200,
        }
    }
}

// ============================================================
// [logo]
// ============================================================
#[derive(Debug, Clone)]
pub struct LogoConfig {
    pub horizontal: String,
    pub square: String,
    pub monochrome: String,
    pub position: String,
    pub margin_top: u32,
}

impl Default for LogoConfig {
    fn default() -> Self {
        Self {
            horizontal: "resources/logo/horizontal.png".into(),
            square: "resources/logo/square.png".into(),
            monochrome: "resources/logo/monochrome.png".into(),
            position: "top-center".into(),
            margin_top: 32,
        }
    }
}

// ============================================================
// [background]
// ============================================================
#[derive(Debug, Clone)]
pub struct BackgroundConfig {
    pub default_bg: String,
    pub dark: String,
    pub light: String,
    pub acrylic_mask: String,
}

impl Default for BackgroundConfig {
    fn default() -> Self {
        Self {
            default_bg: "resources/background/default.png".into(),
            dark: "resources/background/dark.png".into(),
            light: "resources/background/light.png".into(),
            acrylic_mask: "resources/background/mask.png".into(),
        }
    }
}

// ============================================================
// [menu]
// ============================================================
#[derive(Debug, Clone)]
pub struct MenuConfig {
    pub width: u32,
    pub height: u32,
    pub position_x: String,
    pub position_y: String,
    pub radius: u32,
    pub alpha: f32,
    pub item_height: u32,
    pub spacing: u32,
    pub text_color: Color,
    pub font_size: u32,
    pub highlight: HighlightConfig,
}

impl Default for MenuConfig {
    fn default() -> Self {
        Self {
            width: 800,
            height: 540,
            position_x: "center".into(),
            position_y: "center".into(),
            radius: 16,
            alpha: 0.85,
            item_height: 60,
            spacing: 8,
            text_color: Color::from_hex("#ffffff").unwrap_or(Color::WHITE),
            font_size: 18,
            highlight: HighlightConfig::default(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct HighlightConfig {
    pub color: Color,
    pub alpha: f32,
    pub text_color: Color,
    pub icon: String,
}

impl Default for HighlightConfig {
    fn default() -> Self {
        Self {
            color: Color::from_hex("#3a6aff").unwrap_or(Color::from_rgb(0x3A, 0x6A, 0xFF)),
            alpha: 0.9,
            text_color: Color::WHITE,
            icon: "resources/icons/ui/selected.png".into(),
        }
    }
}

// ============================================================
// [scroll]
// ============================================================
#[derive(Debug, Clone)]
pub struct ScrollConfig {
    pub up: String,
    pub down: String,
    pub size: u32,
}

impl Default for ScrollConfig {
    fn default() -> Self {
        Self {
            up: "resources/icons/ui/arrow_up.png".into(),
            down: "resources/icons/ui/arrow_down.png".into(),
            size: 32,
        }
    }
}

// ============================================================
// [cursor]
// ============================================================
#[derive(Debug, Clone)]
pub struct CursorConfig {
    pub default_cursor: String,
    pub hover: String,
    pub loading_cursor: String,
    pub size: u32,
}

impl Default for CursorConfig {
    fn default() -> Self {
        Self {
            default_cursor: "resources/cursor/default.png".into(),
            hover: "resources/cursor/hover.png".into(),
            loading_cursor: "resources/cursor/loading.png".into(),
            size: 24,
        }
    }
}

// ============================================================
// [loading]
// ============================================================
#[derive(Debug, Clone)]
pub struct LoadingConfig {
    pub frames: u32,
    pub pattern: String,
    pub interval_ms: u64,
}

impl Default for LoadingConfig {
    fn default() -> Self {
        Self {
            frames: 6,
            pattern: "resources/animation/loading/frame_%02d.png".into(),
            interval_ms: 100,
        }
    }
}

// ============================================================
// [progress]
// ============================================================
#[derive(Debug, Clone)]
pub struct ProgressConfig {
    pub background: String,
    pub fill: String,
    pub height: u32,
    pub width: u32,
    pub radius: u32,
}

impl Default for ProgressConfig {
    fn default() -> Self {
        Self {
            background: "resources/progress/bar_bg.png".into(),
            fill: "resources/progress/bar_fill.png".into(),
            height: 12,
            width: 700,
            radius: 6,
        }
    }
}

// ============================================================
// [security] — 图标路径
// ============================================================
#[derive(Debug, Clone)]
pub struct SecurityIconPaths {
    pub secure_boot: String,
    pub tpm: String,
    pub verify_ok: String,
    pub warn: String,
    pub error: String,
    pub lock: String,
    pub enclave: String,
}

impl Default for SecurityIconPaths {
    fn default() -> Self {
        Self {
            secure_boot: "resources/icons/security/secure_boot.png".into(),
            tpm: "resources/icons/security/tpm.png".into(),
            verify_ok: "resources/icons/security/verify_ok.png".into(),
            warn: "resources/icons/security/warn.png".into(),
            error: "resources/icons/security/error.png".into(),
            lock: "resources/icons/security/lock.png".into(),
            enclave: "resources/icons/security/enclave.png".into(),
        }
    }
}

// ============================================================
// [dialog]
// ============================================================
#[derive(Debug, Clone)]
pub struct DialogConfig {
    pub bg: String,
    pub overlay: String,
    pub overlay_alpha: f32,
    pub warning: String,
    pub error: String,
    pub info: String,
}

impl Default for DialogConfig {
    fn default() -> Self {
        Self {
            bg: "resources/icons/dialog/bg.png".into(),
            overlay: "resources/icons/dialog/overlay.png".into(),
            overlay_alpha: 0.6,
            warning: "resources/icons/dialog/warning.png".into(),
            error: "resources/icons/dialog/error.png".into(),
            info: "resources/icons/dialog/info.png".into(),
        }
    }
}

// ============================================================
// [icons.system]
// ============================================================
#[derive(Debug, Clone)]
pub struct SystemIcons {
    pub latest: String,
    pub default: String,
    pub old: String,
    pub rescue: String,
    pub windows: String,
    pub linux: String,
    pub efi_fallback: String,
    pub uefi_settings: String,
}

impl Default for SystemIcons {
    fn default() -> Self {
        Self {
            latest: "resources/icons/system/latest.png".into(),
            default: "resources/icons/system/default.png".into(),
            old: "resources/icons/system/old.png".into(),
            rescue: "resources/icons/system/rescue.png".into(),
            windows: "resources/icons/system/windows.png".into(),
            linux: "resources/icons/system/linux.png".into(),
            efi_fallback: "resources/icons/system/efi_fallback.png".into(),
            uefi_settings: "resources/icons/system/uefi_settings.png".into(),
        }
    }
}

// ============================================================
// [icons.ui]
// ============================================================
#[derive(Debug, Clone)]
pub struct UiIconPaths {
    pub settings: String,
    pub about: String,
    pub refresh: String,
    pub console: String,
    pub log: String,
    pub rollback: String,
}

impl Default for UiIconPaths {
    fn default() -> Self {
        Self {
            settings: "resources/icons/ui/settings.png".into(),
            about: "resources/icons/ui/about.png".into(),
            refresh: "resources/icons/ui/refresh.png".into(),
            console: "resources/icons/ui/console.png".into(),
            log: "resources/icons/ui/log.png".into(),
            rollback: "resources/icons/ui/rollback.png".into(),
        }
    }
}

// ============================================================
// [power]
// ============================================================
#[derive(Debug, Clone)]
pub struct PowerIconPaths {
    pub shutdown: String,
    pub reboot: String,
    pub kexec: String,
}

impl Default for PowerIconPaths {
    fn default() -> Self {
        Self {
            shutdown: "resources/icons/power/shutdown.png".into(),
            reboot: "resources/icons/power/reboot.png".into(),
            kexec: "resources/icons/power/kexec.png".into(),
        }
    }
}

// ============================================================
// [timeout]
// ============================================================
#[derive(Debug, Clone)]
pub struct TimeoutConfig {
    pub enable: bool,
    pub default_seconds: u32,
    pub text_template: String,
    pub color: Color,
    pub font_size: u32,
}

impl Default for TimeoutConfig {
    fn default() -> Self {
        Self {
            enable: true,
            default_seconds: 5,
            text_template: "Automatic boot in {sec} seconds...".into(),
            color: Color::from_hex("#cccccc").unwrap_or(Color::from_rgb(0xCC, 0xCC, 0xCC)),
            font_size: 16,
        }
    }
}
