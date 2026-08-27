//! 引导菜单 UI 控制器
//!
//! 负责引导菜单的完整生命周期管理：
//!   1. 键盘/鼠标输入事件循环
//!   2. 菜单选择逻辑
//!   3. 超时自动启动

use crate::config::entries::GenerationManager;
use crate::gfx::Renderer;

/// 菜单操作结果
pub enum MenuAction {
    /// 选择并启动指定条目
    Boot(usize),
    /// 打开固件设置
    UefiSettings,
    /// 关机
    Shutdown,
    /// 热启动 (kexec)
    KexecReboot,
    /// 普通重启
    Reboot,
    /// 无操作
    None,
}

/// 菜单 UI 状态
pub struct MenuUI {
    /// 当前高亮索引
    selected: usize,
    /// 条目数量
    entry_count: usize,
    /// 是否显示帮助文本
    show_help: bool,
    /// 是否显示安全信息
    show_security_info: bool,
    /// 超时剩余秒数
    timeout_remaining: i32,
    /// 是否已在倒计时
    counting_down: bool,
}

impl MenuUI {
    pub fn new(entry_count: usize) -> Self {
        Self {
            selected: 0,
            entry_count: entry_count.max(1),
            show_help: false,
            show_security_info: false,
            timeout_remaining: 5,
            counting_down: true,
        }
    }

    /// 处理方向键向上
    pub fn move_up(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
            self.reset_timeout();
        }
    }

    /// 处理方向键向下
    pub fn move_down(&mut self) {
        if self.selected + 1 < self.entry_count {
            self.selected += 1;
            self.reset_timeout();
        }
    }

    /// 处理 Page Up
    pub fn page_up(&mut self) {
        self.selected = self.selected.saturating_sub(5);
        self.reset_timeout();
    }

    /// 处理 Page Down
    pub fn page_down(&mut self) {
        self.selected = (self.selected + 5).min(self.entry_count.saturating_sub(1));
        self.reset_timeout();
    }

    /// 处理 Home 键
    pub fn move_home(&mut self) {
        self.selected = 0;
        self.reset_timeout();
    }

    /// 处理 End 键
    pub fn move_end(&mut self) {
        self.selected = self.entry_count.saturating_sub(1);
        self.reset_timeout();
    }

    /// 处理 Enter 选择
    pub fn select(&mut self, entries: &GenerationManager) -> MenuAction {
        if let Some(entry) = entries.get(self.selected) {
            match entry.entry_type {
                crate::config::entries::EntryType::UefiSettings => MenuAction::UefiSettings,
                _ if entry.bootable() => MenuAction::Boot(self.selected),
                _ => MenuAction::None,
            }
        } else {
            MenuAction::None
        }
    }

    /// 更新超时 (每 100ms 调用一次)
    pub fn update_timeout(&mut self, enabled: bool, _total_secs: u32) -> MenuAction {
        if !self.counting_down || !enabled {
            return MenuAction::None;
        }

        self.timeout_remaining -= 1;
        if self.timeout_remaining <= 0 {
            return MenuAction::Boot(self.selected);
        }
        MenuAction::None
    }

    fn reset_timeout(&mut self) {
        self.counting_down = true;
        // 用户交互后重置倒计时为更长的时间
        self.timeout_remaining = 10;
    }

    pub fn toggle_help(&mut self) {
        self.show_help = !self.show_help;
    }

    pub fn toggle_security(&mut self) {
        self.show_security_info = !self.show_security_info;
    }

    /// 获取当前选中的条目索引
    pub fn selected_index(&self) -> usize {
        self.selected
    }

    /// 当前超时剩余
    pub fn timeout(&self) -> i32 {
        self.timeout_remaining
    }

    /// 是否显示帮助
    pub fn show_help(&self) -> bool {
        self.show_help
    }

    /// 是否显示安全信息
    pub fn show_security_info(&self) -> bool {
        self.show_security_info
    }

    pub fn entry_count(&self) -> usize {
        self.entry_count
    }

    /// 渲染菜单
    pub fn render(
        &mut self,
        _renderer: &mut Renderer,
        _entries: &GenerationManager,
    ) {
        // 渲染由 menu::ui 模块处理
        // 这里只是一个钩子点
    }
}
