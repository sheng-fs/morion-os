//! 声明式配置解析
//!
//! 负责解析 TOML 格式的主题配置和 Nix-style 引导条目配置文件。
//! 所有配置都被视为不可变的声明——引导菜单就是这些配置的 UI 渲染。

pub mod theme;
pub mod entries;

pub use theme::{
    ThemeConfig, ThemeMeta, RenderConfig, SplashConfig, LogoConfig,
    BackgroundConfig, MenuConfig, HighlightConfig, ScrollConfig,
    CursorConfig, LoadingConfig, ProgressConfig, SecurityIconPaths,
    DialogConfig, SystemIcons, UiIconPaths, PowerIconPaths, TimeoutConfig,
};
pub use entries::{BootEntry, EntryType, GenerationManager};
