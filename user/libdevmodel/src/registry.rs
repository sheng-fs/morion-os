//! 驱动注册表 —— **通用驱动与厂商专有驱动的可替换槽位**。
//!
//! 每个条目是一份**纯数据描述**（不含驱动实例）：谁、属于哪个功能类、优先级多高、
//! 认领哪些设备。服务层启动时按"设备 → 功能类"查询，取优先级最高的条目构造驱动。
//!
//! 替换规则：
//!
//! - 同一台设备被多个条目认领时，**优先级高者胜**；
//! - 优先级相同则**后注册者胜**（`register_override` 正是靠这条实现"覆盖"）;
//! - 表满时 `register_override` 会顶掉同功能类里优先级最低的条目，所以厂商驱动
//!   总能挤进固定容量的槽位，不需要加表。
//!
//! 于是"用户用自己的专有驱动替换通用驱动"= 以更高优先级注册同一 `matches` 的条目，
//! 上层逻辑不变。

use crate::device::{Class, DeviceInfo, DriverError};

/// 一个驱动注册条目。
#[derive(Clone, Copy)]
pub struct DriverDescriptor {
    /// 驱动标识（日志、`/proc` 式诊断用）。
    pub id: &'static str,
    /// 功能类。
    pub class: Class,
    /// 优先级：越大越优先。通用驱动建议 10..=49，厂商专有驱动建议 >= 50。
    pub priority: u8,
    /// 期望的 PCI 厂商号（0 = 不限制；非 PCI 设备保持 0）。
    pub vendor_id: u16,
    /// 期望的 PCI 设备号（0 = 不限制）。
    pub device_id: u16,
    /// 额外的设备判定（class/子类/传输层等）。
    pub matches: fn(&DeviceInfo) -> bool,
}

impl DriverDescriptor {
    /// 本条目是否认领 `dev`。
    pub fn claims(&self, dev: &DeviceInfo) -> bool {
        dev.matches_ids(self.vendor_id, self.device_id) && (self.matches)(dev)
    }
}

/// 固定容量注册表（`N` 为槽位数）。
pub struct Registry<const N: usize> {
    slots: [Option<DriverDescriptor>; N],
    len: usize,
}

impl<const N: usize> Registry<N> {
    /// 空表。
    pub const fn new() -> Self {
        Self {
            slots: [None; N],
            len: 0,
        }
    }

    /// 当前条目数。
    pub fn len(&self) -> usize {
        self.len
    }

    /// 是否是空表。
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// 注册一个条目（不覆盖）。表满返回 [`DriverError::Busy`]。
    pub fn register(&mut self, desc: DriverDescriptor) -> Result<(), DriverError> {
        if self.len >= N {
            return Err(DriverError::Busy);
        }
        self.slots[self.len] = Some(desc);
        self.len += 1;
        Ok(())
    }

    /// 覆盖注册：表满时顶掉同功能类中优先级**最低**的条目（新条目优先级不低时才替换）。
    pub fn register_override(&mut self, desc: DriverDescriptor) -> Result<(), DriverError> {
        if self.len < N {
            return self.register(desc);
        }
        let mut victim: Option<usize> = None;
        for i in 0..N {
            if let Some(cur) = self.slots[i] {
                if cur.class != desc.class {
                    continue;
                }
                match victim {
                    None => victim = Some(i),
                    Some(v) => {
                        if cur.priority < self.slots[v].map(|d| d.priority).unwrap_or(u8::MAX) {
                            victim = Some(i);
                        }
                    }
                }
            }
        }
        match victim {
            Some(i) if desc.priority >= self.slots[i].map(|d| d.priority).unwrap_or(0) => {
                self.slots[i] = Some(desc);
                Ok(())
            }
            _ => Err(DriverError::Busy),
        }
    }

    /// 为 `class` 类设备挑选条目：认领该设备且优先级最高者；同优先级取后注册者。
    pub fn resolve(&self, class: Class, dev: &DeviceInfo) -> Option<&DriverDescriptor> {
        let mut best: Option<&DriverDescriptor> = None;
        let mut i = 0;
        while i < N {
            if let Some(d) = self.slots[i].as_ref() {
                if d.class == class && d.claims(dev) {
                    best = match best {
                        None => Some(d),
                        Some(b) if d.priority >= b.priority => Some(d),
                        Some(b) => Some(b),
                    };
                }
            }
            i += 1;
        }
        best
    }

    /// 列出某功能类的全部条目（按注册顺序）。
    pub fn class_entries(&self, class: Class) -> impl Iterator<Item = &DriverDescriptor> {
        self.slots
            .iter()
            .flatten()
            .filter(move |d| d.class == class)
    }
}

impl<const N: usize> Default for Registry<N> {
    fn default() -> Self {
        Self::new()
    }
}
