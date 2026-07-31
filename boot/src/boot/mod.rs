//! 引导子系统
//!
//! 包含内核加载器、kexec 热启动机制、菜单控制器。

pub mod loader;
pub mod kexec;
pub mod menu;

pub use loader::KernelLoader;
pub use kexec::KexecBoot;
pub use menu::MenuUI;
