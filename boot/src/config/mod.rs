//! 声明式配置解析
//!
//! 负责解析 TOML 格式的主题配置和 Nix-style 引导条目配置文件。
//! 所有配置都被视为不可变的声明——引导菜单就是这些配置的 UI 渲染。

pub mod entries;
pub mod theme;

pub use entries::{BootEntry, EntryType, GenerationManager};
pub use theme::{
    BackgroundConfig, CursorConfig, DialogConfig, HighlightConfig, LoadingConfig, LogoConfig,
    MenuConfig, PowerIconPaths, ProgressConfig, RenderConfig, ScrollConfig, SecurityIconPaths,
    SplashConfig, SystemIcons, ThemeConfig, ThemeMeta, TimeoutConfig, UiIconPaths,
};
