//! 分页器 (Pager) — 缺页异常转发与按需分页 (Stage 14)
//!
//! 微内核「外部页管理器模型」:
//!   - 每个域登记一个分页器域 (默认由 `init` 统一指定)。
//!   - 缺页处理器捕获 CR2 后, 把缺页信息封装成一条 IPC 消息投递到该域的分页器
//!     邮箱 (复用通用 `ipc::deliver`, 不再使用专用缺页队列)。
//!   - 分页器经通用 `SYS_RECV` 阻塞取消息, 从 payload 解出缺页信息, 映射页面后
//!     经 `SYS_PAGE_FAULT_REPLY` 唤醒缺页任务 (回复目标由 `receive` 记录)。
//!
//! 缺页信息为固定大小结构体 (24 字节, 放入消息 payload), 与架构文档一致。

use alloc::vec::Vec;
use spin::Mutex;

/// 缺页消息 tag 标记 (区分普通 IPC 与缺页异常转发)。
pub const FAULT_TAG: u64 = 0x5046_4155_4C54; // "PF AULT" 魔术值

/// 缺页信息 (固定大小结构, 序列化进 IPC 消息 payload)。
/// `#[repr(C)]` 保证与用户态同名字段结构体布局一致。
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct PageFaultInfo {
    pub fault_domain: u64,
    pub fault_addr: u64,
    pub error_code: u64,
}

/// 每域的分页器域 id (`None` = 该域没有分页器, 或已被销毁)。
static PAGERS: Mutex<Vec<Option<u64>>> = Mutex::new(Vec::new());

/// 初始化分页器映射 (创建 `domain_count` 个域, 统一指向 `pager_domain`)。
pub fn init(domain_count: usize, pager_domain: u64) {
    let mut pagers = PAGERS.lock();
    pagers.clear();
    for _ in 0..domain_count {
        pagers.push(Some(pager_domain));
    }
}

/// 运行时登记域 `id` 的分页器 (ELF 加载建新域时调用)。
///
/// 新域的分页器 = **加载它的那个域**（loader 自然就是该程序的缺页后端）。
/// 按域 id 索引, 须在 `domain::create()` 之后调用; 域表槽位会被复用
/// (见 `domain::slot_for`), 所以这里也是"覆盖上一轮遗留值"。
pub fn add_domain(id: u64, pager_domain: u64) {
    set_pager(id, Some(pager_domain));
}

/// 域销毁时调用: 摘掉它的分页器登记。
pub fn remove_domain(id: u64) {
    set_pager(id, None);
}

/// 把 `id` 的分页器设为 `pager` —— 不够长就在表尾补齐 (`None`)。
fn set_pager(id: u64, pager: Option<u64>) {
    let mut pagers = PAGERS.lock();
    let idx = id as usize;
    while pagers.len() <= idx {
        pagers.push(None);
    }
    pagers[idx] = pager;
}

/// 查询指定域的分页器域 id; 该域不存在 / 已销毁 / 无分页器时返回 `None`。
///
/// 返回 `None` 时缺页无人能补 —— 调用方 (缺页处理器) 必须停下来而不能无限重试。
pub fn of(domain: u64) -> Option<u64> {
    PAGERS.lock().get(domain as usize).copied().flatten()
}

/// 投递一条缺页消息给分页器并唤醒它。
///
/// 缺页信息序列化进消息 payload, 经通用 IPC 邮箱投递; `from` 记为缺页域,
/// 使分页器 `receive` 后回复目标正确路由回缺页域。
/// 调用者须保证中断关闭 (IF=0); 本函数不改变中断使能位。
pub fn deliver_fault(pager: u64, info: PageFaultInfo) {
    let payload = unsafe {
        core::slice::from_raw_parts(
            &info as *const PageFaultInfo as *const u8,
            core::mem::size_of::<PageFaultInfo>(),
        )
    };
    crate::ipc::deliver(info.fault_domain, pager, FAULT_TAG, payload);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 分页器登记的建/销/复用: 销毁后查不到, 槽位复用能被覆盖, 表不增长。
    #[test]
    fn destroy_and_reuse_domain_resets_pager() {
        init(2, 7);
        assert_eq!(of(0), Some(7));
        assert_eq!(of(1), Some(7));
        assert_eq!(of(2), None, "表外的域没有分页器");

        // 运行时建新域 (域表槽位 2) → 分页器 = 加载者。
        add_domain(2, 9);
        assert_eq!(of(2), Some(9));
        assert_eq!(PAGERS.lock().len(), 3);

        // 销毁域 1 → 摘掉登记; 槽位复用 (同一个 id) 能被新值覆盖。
        remove_domain(1);
        assert_eq!(of(1), None);
        add_domain(1, 4);
        assert_eq!(of(1), Some(4));
        assert_eq!(PAGERS.lock().len(), 3, "复用槽位不应新增行");
    }
}
