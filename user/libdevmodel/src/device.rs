//! 设备描述与错误类型 —— 驱动**看见设备**的唯一形态。
//!
//! 这些结构是"内核授权描述"之上的一层**归一化视图**：PCI 设备填 BDF/VID:PID/class，
//! PS/2、I2C-HID、USB HID 等非 PCI 设备用各自的 transport 表达。驱动只依赖本视图，
//! 不直接解析 PCI 配置空间，于是同一份驱动核心可以被不同总线复用。

/// 设备所属的**功能类**（不是总线类型）。驱动注册表按它分槽。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Class {
    /// 音频（HDA / AC'97 等）。
    Audio,
    /// 输入（PS/2 鼠标、I2C-HID 触摸板、USB HID 等）。
    Input,
    /// 无线网卡（WLAN station）。
    Wlan,
}

/// 设备挂在哪条总线上。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Transport {
    /// PCI / PCIe：`bus/device/function` 有效。
    Pci,
    /// PS/2（i8042 的 AUX 口），无枚举 ID。
    Ps2,
    /// I2C-HID：`controller` 是控制器序号，`address` 是 7 位从地址。
    I2cHid { controller: u8, address: u8 },
    /// USB HID：`bus`/`address` 为 xHCI 视角的地址。
    Usb { bus: u8, address: u8 },
    /// 平台设备（ACPI/设备树描述，无标准枚举 ID）。
    Platform,
}

/// 归一化设备信息。`vendor_id`/`device_id` 为 0 表示"该总线没有这个概念"。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DeviceInfo {
    pub transport: Transport,
    /// PCI 位置；非 PCI 设备为 0。
    pub bus: u8,
    pub device: u8,
    pub function: u8,
    pub vendor_id: u16,
    pub device_id: u16,
    /// PCI class code（0x04 = 多媒体，0x02 = 网络，0x0C = 串行总线 …）；非 PCI 为 0。
    pub class_code: u8,
    pub subclass: u8,
    pub prog_if: u8,
    /// 首个 MMIO BAR 的物理基址与长度（0 = 无）。
    pub bar0: u64,
    pub bar0_len: u64,
}

impl DeviceInfo {
    /// 构造一个 PCI 设备描述。
    ///
    /// 参数是 PCI 配置空间的扁平投影（BDF / VID:PID / class / BAR），刻意不做成嵌套
    /// 结构 —— 驱动侧逐字段读取，嵌套只会多一层解包。
    #[allow(clippy::too_many_arguments)]
    pub const fn pci(
        bus: u8,
        device: u8,
        function: u8,
        vendor_id: u16,
        device_id: u16,
        class_code: u8,
        subclass: u8,
        prog_if: u8,
        bar0: u64,
        bar0_len: u64,
    ) -> Self {
        Self {
            transport: Transport::Pci,
            bus,
            device,
            function,
            vendor_id,
            device_id,
            class_code,
            subclass,
            prog_if,
            bar0,
            bar0_len,
        }
    }

    /// PS/2 设备（i8042 AUX 口的鼠标/触摸板）。
    pub const fn ps2() -> Self {
        Self {
            transport: Transport::Ps2,
            bus: 0,
            device: 0,
            function: 0,
            vendor_id: 0,
            device_id: 0,
            class_code: 0,
            subclass: 0,
            prog_if: 0,
            bar0: 0,
            bar0_len: 0,
        }
    }

    /// I2C-HID 设备。
    pub const fn i2c_hid(controller: u8, address: u8) -> Self {
        Self {
            transport: Transport::I2cHid { controller, address },
            bus: 0,
            device: 0,
            function: 0,
            vendor_id: 0,
            device_id: 0,
            class_code: 0,
            subclass: 0,
            prog_if: 0,
            bar0: 0,
            bar0_len: 0,
        }
    }

    /// USB HID 设备。
    pub const fn usb(bus: u8, address: u8) -> Self {
        Self {
            transport: Transport::Usb { bus, address },
            bus: 0,
            device: 0,
            function: 0,
            vendor_id: 0,
            device_id: 0,
            class_code: 0,
            subclass: 0,
            prog_if: 0,
            bar0: 0,
            bar0_len: 0,
        }
    }

    /// 填上 USB 设备的 VID:PID（USB HID 也有这两个字段）。
    pub const fn with_ids(mut self, vendor_id: u16, device_id: u16) -> Self {
        self.vendor_id = vendor_id;
        self.device_id = device_id;
        self
    }

    /// PCI 位置号 `bus << 8 | device << 3 | function`；非 PCI 返回 0。
    pub const fn bdf(&self) -> u32 {
        match self.transport {
            Transport::Pci => {
                ((self.bus as u32) << 8) | ((self.device as u32) << 3) | self.function as u32
            }
            _ => 0,
        }
    }

    /// 是否是 PCI 设备。
    pub const fn is_pci(&self) -> bool {
        matches!(self.transport, Transport::Pci)
    }

    /// VID:PID 精确匹配（任一为 0 时该字段不参与匹配）。
    pub const fn matches_ids(&self, vendor_id: u16, device_id: u16) -> bool {
        (vendor_id == 0 || self.vendor_id == vendor_id)
            && (device_id == 0 || self.device_id == device_id)
    }

    /// PCI class/subclass 匹配（`subclass` 传 0xFF 表示忽略）。
    pub const fn matches_class(&self, class_code: u8, subclass: u8) -> bool {
        self.is_pci()
            && self.class_code == class_code
            && (subclass == 0xFF || self.subclass == subclass)
    }
}

/// 驱动错误。`Other` 只放**静态字符串**，避免驱动层依赖分配器。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DriverError {
    /// 本驱动不认这台设备。
    NotSupported,
    /// 设备不存在 / 已掉线。
    NotFound,
    /// 等待硬件状态超时。
    Timeout,
    /// 寄存器或 DMA 访问失败。
    Io,
    /// 调用顺序不对（未 start、已占用等）。
    BadState,
    /// 资源忙。
    Busy,
    /// 参数越界或格式不支持。
    Invalid,
    /// 其它，附静态说明。
    Other(&'static str),
}
