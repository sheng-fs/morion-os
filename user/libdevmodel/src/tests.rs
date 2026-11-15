//! 契约自测：设备判定、注册表覆盖规则、能力接口默认实现。

use crate::device::{Class, DeviceInfo, DriverError};
use crate::driver::Driver;
use crate::ops::{AudioFormat, AudioOps};
use crate::registry::{DriverDescriptor, Registry};

fn hda_like(dev: &DeviceInfo) -> bool {
    dev.matches_class(0x04, 0x03)
}

fn any_pci(dev: &DeviceInfo) -> bool {
    dev.is_pci()
}

#[test]
fn pci_bdf_and_class_match() {
    let dev = DeviceInfo::pci(0, 31, 3, 0x8086, 0x293E, 0x04, 0x03, 0x00, 0xFEBF_0000, 0x4000);
    assert_eq!(dev.bdf(), (31 << 3) | 3);
    assert!(dev.is_pci());
    assert!(dev.matches_class(0x04, 0x03));
    assert!(dev.matches_ids(0x8086, 0x293E));
    assert!(dev.matches_ids(0, 0x293E));
    assert!(!dev.matches_ids(0x1002, 0));
}

#[test]
fn ssid_and_mac_validation() {
    assert!(crate::ops::Ssid::new(b"").is_err());
    let long = [b'x'; 33];
    assert!(crate::ops::Ssid::new(&long).is_err());
    let s = crate::ops::Ssid::new(b"Morion-5G").unwrap();
    assert_eq!(s.as_bytes(), b"Morion-5G");
    assert_eq!(s.len(), 9);

    assert!(crate::ops::MacAddr([0x02, 0, 0, 0, 0, 1]).is_unicast());
    assert!(!crate::ops::MacAddr([0xFF; 6]).is_unicast());
    assert!(!crate::ops::MacAddr::ZERO.is_unicast());
    // 组播位置 1
    assert!(!crate::ops::MacAddr([0x01, 0, 0, 0, 0, 1]).is_unicast());
}

#[test]
fn registry_prefers_higher_priority() {
    let mut reg: Registry<8> = Registry::new();
    reg.register(DriverDescriptor {
        id: "hda-generic",
        class: Class::Audio,
        priority: 10,
        vendor_id: 0,
        device_id: 0,
        matches: hda_like,
    })
    .unwrap();
    reg.register(DriverDescriptor {
        id: "hda-vendor",
        class: Class::Audio,
        priority: 50,
        vendor_id: 0x8086,
        device_id: 0x293E,
        matches: any_pci,
    })
    .unwrap();

    let dev = DeviceInfo::pci(0, 31, 3, 0x8086, 0x293E, 0x04, 0x03, 0x00, 0, 0);
    assert_eq!(reg.resolve(Class::Audio, &dev).unwrap().id, "hda-vendor");

    let amd = DeviceInfo::pci(0, 31, 3, 0x1002, 0x1234, 0x04, 0x03, 0x00, 0, 0);
    assert_eq!(reg.resolve(Class::Audio, &amd).unwrap().id, "hda-generic");

    // 非音频设备不应被音频条目认领
    let nic = DeviceInfo::pci(0, 25, 0, 0x8086, 0x1533, 0x02, 0x00, 0x00, 0, 0);
    assert!(reg.resolve(Class::Audio, &nic).is_none());
}

#[test]
fn registry_tie_later_wins() {
    let mut reg: Registry<4> = Registry::new();
    for id in ["first", "second"] {
        reg.register(DriverDescriptor {
            id,
            class: Class::Audio,
            priority: 20,
            vendor_id: 0,
            device_id: 0,
            matches: hda_like,
        })
        .unwrap();
    }
    let dev = DeviceInfo::pci(0, 31, 3, 0x8086, 0x293E, 0x04, 0x03, 0x00, 0, 0);
    assert_eq!(reg.resolve(Class::Audio, &dev).unwrap().id, "second");
}

#[test]
fn registry_override_evicts_lowest_when_full() {
    let mut reg: Registry<1> = Registry::new();
    reg.register(DriverDescriptor {
        id: "generic",
        class: Class::Audio,
        priority: 10,
        vendor_id: 0,
        device_id: 0,
        matches: hda_like,
    })
    .unwrap();

    // 低优先级不能顶掉高优先级
    assert_eq!(
        reg.register_override(DriverDescriptor {
            id: "worse",
            class: Class::Audio,
            priority: 5,
            vendor_id: 0,
            device_id: 0,
            matches: hda_like,
        }),
        Err(DriverError::Busy)
    );

    reg.register_override(DriverDescriptor {
        id: "vendor",
        class: Class::Audio,
        priority: 80,
        vendor_id: 0,
        device_id: 0,
        matches: hda_like,
    })
    .unwrap();
    assert_eq!(reg.len(), 1);
    let dev = DeviceInfo::pci(0, 31, 3, 0x8086, 0x293E, 0x04, 0x03, 0x00, 0, 0);
    assert_eq!(reg.resolve(Class::Audio, &dev).unwrap().id, "vendor");
}

#[test]
fn input_device_constructors() {
    let ps2 = DeviceInfo::ps2();
    assert!(!ps2.is_pci());
    assert_eq!(ps2.bdf(), 0);

    let i2c = DeviceInfo::i2c_hid(0, 0x2C);
    assert!(matches!(
        i2c.transport,
        crate::device::Transport::I2cHid {
            controller: 0,
            address: 0x2C
        }
    ));

    let usb = DeviceInfo::usb(1, 3).with_ids(0x046D, 0xC52B);
    assert_eq!(usb.vendor_id, 0x046D);
}

/// 一个最小驱动实现，验证 `Driver` → `AudioOps` 的能力转发是可行的。
struct FakeAudio {
    opened: Option<AudioFormat>,
}

impl AudioOps for FakeAudio {
    fn name(&self) -> &'static str {
        "fake"
    }
    fn formats(&self) -> &'static [AudioFormat] {
        &[AudioFormat::S16_48K_STEREO]
    }
    fn open(&mut self, fmt: AudioFormat) -> Result<(), DriverError> {
        self.opened = Some(fmt);
        Ok(())
    }
    fn write_pcm(&mut self, samples: &[i16]) -> Result<usize, DriverError> {
        if self.opened.is_none() {
            return Err(DriverError::BadState);
        }
        Ok(samples.len())
    }
    fn drain(&mut self) -> Result<(), DriverError> {
        Ok(())
    }
    fn close(&mut self) {
        self.opened = None;
    }
    fn volume(&self) -> (u8, u8) {
        (80, 80)
    }
    fn set_volume(&mut self, left: u8, right: u8) -> Result<(), DriverError> {
        let _ = (left.min(100), right.min(100));
        Ok(())
    }
}

impl Driver for FakeAudio {
    fn name(&self) -> &'static str {
        "fake-audio"
    }
    fn class(&self) -> Class {
        Class::Audio
    }
    fn probe(&mut self, dev: &DeviceInfo) -> Result<bool, DriverError> {
        Ok(dev.matches_class(0x04, 0x03))
    }
    fn start(&mut self, _dev: &DeviceInfo) -> Result<(), DriverError> {
        Ok(())
    }
    fn stop(&mut self) {
        self.opened = None;
    }
    fn audio(&mut self) -> Option<&mut dyn AudioOps> {
        Some(self)
    }
}

#[test]
fn driver_exposes_audio_ops() {
    let dev = DeviceInfo::pci(0, 31, 3, 0x8086, 0x293E, 0x04, 0x03, 0x00, 0, 0);
    let mut drv = FakeAudio { opened: None };
    assert!(drv.probe(&dev).unwrap());
    drv.start(&dev).unwrap();
    let ops = drv.audio().unwrap();
    assert_eq!(ops.formats()[0], AudioFormat::S16_48K_STEREO);
    assert_eq!(ops.write_pcm(&[0i16; 4]), Err(DriverError::BadState));
    ops.open(AudioFormat::S16_48K_STEREO).unwrap();
    assert_eq!(ops.write_pcm(&[0i16; 4]).unwrap(), 4);
    assert!(drv.wlan().is_none());
}
