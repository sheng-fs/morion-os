//! 引导子系统
//!
//! 包含内核加载器、kexec 热启动机制、菜单控制器。

pub mod kexec;
pub mod loader;
pub mod menu;

pub use kexec::KexecBoot;
pub use loader::KernelLoader;
pub use menu::MenuUI;
