//! 通用 WLAN 模型的单元测试。
//!
//! 覆盖：状态迁移（合法/非法）、扫描缓存去重与容量策略、join 参数校验、
//! 固件加载适配器（成功/越界）、回环收发、`Driver`/`WlanOps` 委托、注册表覆盖。

use libdevmodel::{
    BssEntry, Class, DeviceInfo, Driver, DriverDescriptor, DriverError, FirmwareLoader, LinkState,
    MacAddr, Registry, Security, Ssid, WlanOps,
};

use crate::driver::WlanDriver;
use crate::fake::{FakeWlanHw, LoopbackHw, FAKE_BSSID, FAKE_FIRMWARE, FAKE_MAC};
use crate::fw::{ChunkedLoader, FirmwareBuffer, SliceFirmware};
use crate::station::WlanStation;

// ------------------------------------------------------------------
// 辅助构造
// ------------------------------------------------------------------

fn wlan_dev(vendor_id: u16, device_id: u16) -> DeviceInfo {
    DeviceInfo::pci(
        0,
        0x1C,
        0,
        vendor_id,
        device_id,
        0x02,
        0x80,
        0,
        0xFEB0_0000,
        0x1000,
    )
}

fn ethernet_dev() -> DeviceInfo {
    DeviceInfo::pci(0, 0x19, 0, 0x8086, 0x15B8, 0x02, 0x00, 0, 0, 0)
}

fn bss(bssid: [u8; 6], ssid: &[u8], rssi: i8) -> BssEntry {
    BssEntry {
        bssid: MacAddr(bssid),
        ssid: Ssid::new(ssid).unwrap(),
        channel: 6,
        rssi,
        security: Security::Open,
    }
}

fn fake_with(results: &[BssEntry]) -> FakeWlanHw {
    let mut hw = FakeWlanHw::default();
    let mut i = 0;
    while i < results.len() {
        hw.push_scan(results[i]);
        i += 1;
    }
    hw
}

// ------------------------------------------------------------------
// 扫描：状态、缓存、去重、容量
// ------------------------------------------------------------------

#[test]
fn mac_is_delegated_to_hal() {
    let st = WlanStation::<FakeWlanHw, 4>::new(FakeWlanHw::default());
    assert_eq!(st.mac(), FAKE_MAC);
}

#[test]
fn scan_fills_cache_and_returns_to_down() {
    let hw = fake_with(&[
        bss([0x02, 0, 0, 0, 0, 0x11], b"home", -50),
        bss([0x02, 0, 0, 0, 0, 0x12], b"guest", -70),
    ]);
    let mut st = WlanStation::<FakeWlanHw, 4>::new(hw);
    let mut out = [BssEntry::EMPTY; 4];

    assert_eq!(st.scan(&mut out), Ok(2));
    assert_eq!(st.state(), LinkState::Down);
    assert!(st.is_scanned());
    assert_eq!(st.cache().len(), 2);
    assert_eq!(out[0].ssid.as_bytes(), b"home");
    assert_eq!(out[1].ssid.as_bytes(), b"guest");
    assert_eq!(st.stats().scan_rounds, 1);
}

#[test]
fn scan_incremental_reports_while_scanning() {
    let mut hw = fake_with(&[
        bss([0x02, 0, 0, 0, 0, 0x11], b"a", -40),
        bss([0x02, 0, 0, 0, 0, 0x12], b"b", -50),
    ]);
    hw.scan_batch = 1; // 每次 poll_scan 只吐一条，模拟增量上报

    let mut st = WlanStation::<FakeWlanHw, 4>::new(hw);
    let mut out = [BssEntry::EMPTY; 4];

    assert_eq!(st.scan(&mut out), Ok(1));
    assert_eq!(st.state(), LinkState::Scanning);
    assert!(!st.is_scanned());

    assert_eq!(st.scan(&mut out), Ok(2));
    assert_eq!(st.state(), LinkState::Down);
    assert!(st.is_scanned());
}

#[test]
fn scan_deduplicates_by_bssid_and_keeps_new_rssi() {
    let same = [0x02, 0, 0, 0, 0, 0x11];
    let hw = fake_with(&[
        bss(same, b"home", -60),
        bss([0x02, 0, 0, 0, 0, 0x12], b"guest", -70),
        bss(same, b"home", -45),
    ]);
    let mut st = WlanStation::<FakeWlanHw, 4>::new(hw);
    let mut out = [BssEntry::EMPTY; 4];

    assert_eq!(st.scan(&mut out), Ok(2));
    assert_eq!(out[0].bssid, MacAddr(same));
    assert_eq!(out[0].rssi, -45);
    assert_eq!(out[1].ssid.as_bytes(), b"guest");
}

#[test]
fn scan_capacity_rejects_weakest_new_entry() {
    let hw = fake_with(&[
        bss([0x02, 0, 0, 0, 0, 0x11], b"a", -40),
        bss([0x02, 0, 0, 0, 0, 0x12], b"b", -50),
        bss([0x02, 0, 0, 0, 0, 0x13], b"c", -70),
    ]);
    let mut st = WlanStation::<FakeWlanHw, 2>::new(hw);
    let mut out = [BssEntry::EMPTY; 2];

    assert_eq!(st.scan(&mut out), Ok(2));
    assert_eq!(out[0].ssid.as_bytes(), b"a");
    assert_eq!(out[1].ssid.as_bytes(), b"b");
    assert_eq!(st.stats().scan_dropped, 1);
}

#[test]
fn scan_capacity_evicts_weakest_entry() {
    let hw = fake_with(&[
        bss([0x02, 0, 0, 0, 0, 0x11], b"a", -40),
        bss([0x02, 0, 0, 0, 0, 0x12], b"b", -80),
        bss([0x02, 0, 0, 0, 0, 0x13], b"c", -50),
    ]);
    let mut st = WlanStation::<FakeWlanHw, 2>::new(hw);
    let mut out = [BssEntry::EMPTY; 2];

    assert_eq!(st.scan(&mut out), Ok(2));
    assert_eq!(out[0].ssid.as_bytes(), b"a");
    assert_eq!(out[1].ssid.as_bytes(), b"c");
}

#[test]
fn scan_output_is_truncated_to_out_len() {
    let hw = fake_with(&[
        bss([0x02, 0, 0, 0, 0, 0x11], b"a", -40),
        bss([0x02, 0, 0, 0, 0, 0x12], b"b", -50),
    ]);
    let mut st = WlanStation::<FakeWlanHw, 4>::new(hw);
    let mut one = [BssEntry::EMPTY; 1];

    assert_eq!(st.scan(&mut one), Ok(1));
    assert_eq!(one[0].ssid.as_bytes(), b"a");
    assert_eq!(st.cache().len(), 2);
}

// ------------------------------------------------------------------
// 关联状态机与参数校验
// ------------------------------------------------------------------

#[test]
fn join_before_scan_is_bad_state() {
    let mut st = WlanStation::<FakeWlanHw, 4>::new(FakeWlanHw::default());
    let ssid = Ssid::new(b"home").unwrap();
    assert_eq!(st.join(&ssid, b""), Err(DriverError::BadState));
    assert_eq!(st.state(), LinkState::Down);
}

#[test]
fn join_while_scanning_is_bad_state() {
    let mut hw = fake_with(&[
        bss([0x02, 0, 0, 0, 0, 0x11], b"a", -40),
        bss([0x02, 0, 0, 0, 0, 0x12], b"b", -50),
    ]);
    hw.scan_batch = 1;
    let mut st = WlanStation::<FakeWlanHw, 4>::new(hw);
    let mut out = [BssEntry::EMPTY; 4];
    assert_eq!(st.scan(&mut out), Ok(1));
    assert_eq!(st.state(), LinkState::Scanning);

    let ssid = Ssid::new(b"a").unwrap();
    assert_eq!(st.join(&ssid, b""), Err(DriverError::BadState));
}

#[test]
fn join_rejects_invalid_passphrase_lengths() {
    let hw = fake_with(&[bss([0x02, 0, 0, 0, 0, 0x11], b"home", -40)]);
    let mut st = WlanStation::<FakeWlanHw, 4>::new(hw);
    let mut out = [BssEntry::EMPTY; 4];
    assert_eq!(st.scan(&mut out), Ok(1));

    let ssid = Ssid::new(b"home").unwrap();
    assert_eq!(st.join(&ssid, b"12345"), Err(DriverError::Invalid));
    assert_eq!(st.join(&ssid, &[b'x'; 64]), Err(DriverError::Invalid));
    // 校验失败不应改变状态。
    assert_eq!(st.state(), LinkState::Down);
}

#[test]
fn join_accepts_open_and_psk_passphrase() {
    let hw = fake_with(&[bss([0x02, 0, 0, 0, 0, 0x11], b"home", -40)]);
    let mut st = WlanStation::<FakeWlanHw, 4>::new(hw);
    let mut out = [BssEntry::EMPTY; 4];
    assert_eq!(st.scan(&mut out), Ok(1));

    let ssid = Ssid::new(b"home").unwrap();
    assert_eq!(st.join(&ssid, b""), Ok(()));
    assert_eq!(st.state(), LinkState::Linked { bssid: FAKE_BSSID });
    // 参数确实交给了 HAL。
    let hw = st.hw();
    assert_eq!(hw.last_ssid.unwrap().as_bytes(), b"home");
    assert_eq!(hw.last_pass_len, 0);

    // 8..=63 字节的口令被接受。
    let mut st2 = WlanStation::<FakeWlanHw, 4>::new(fake_with(&[bss(
        [0x02, 0, 0, 0, 0, 0x11],
        b"home",
        -40,
    )]));
    assert_eq!(st2.scan(&mut out), Ok(1));
    assert_eq!(st2.join(&ssid, b"12345678"), Ok(()));
    assert_eq!(st2.hw().last_pass_len, 8);
}

#[test]
fn join_while_linked_is_bad_state_and_leave_resets() {
    let hw = fake_with(&[bss([0x02, 0, 0, 0, 0, 0x11], b"home", -40)]);
    let mut st = WlanStation::<FakeWlanHw, 4>::new(hw);
    let mut out = [BssEntry::EMPTY; 4];
    assert_eq!(st.scan(&mut out), Ok(1));

    let ssid = Ssid::new(b"home").unwrap();
    assert_eq!(st.join(&ssid, b""), Ok(()));
    assert_eq!(st.join(&ssid, b""), Err(DriverError::BadState));
    // 已关联时不允许扫描。
    assert_eq!(st.scan(&mut out), Err(DriverError::BadState));

    assert_eq!(st.leave(), Ok(()));
    assert_eq!(st.state(), LinkState::Down);
    assert_eq!(st.leave(), Ok(())); // 可重复
    assert_eq!(st.join(&ssid, b""), Ok(()));
}

#[test]
fn associate_failure_marks_failed_and_allows_retry() {
    let mut hw = fake_with(&[bss([0x02, 0, 0, 0, 0, 0x11], b"home", -40)]);
    hw.associate_fail = Some(DriverError::Timeout);
    let mut st = WlanStation::<FakeWlanHw, 4>::new(hw);
    let mut out = [BssEntry::EMPTY; 4];
    assert_eq!(st.scan(&mut out), Ok(1));

    let ssid = Ssid::new(b"home").unwrap();
    assert_eq!(st.join(&ssid, b""), Err(DriverError::Timeout));
    assert_eq!(st.state(), LinkState::Failed(DriverError::Timeout));

    // Failed 状态下允许重试扫描和关联。
    st.hw().associate_fail = None;
    assert_eq!(st.join(&ssid, b""), Ok(()));
    assert_eq!(st.state(), LinkState::Linked { bssid: FAKE_BSSID });
}

// ------------------------------------------------------------------
// 帧收发与统计
// ------------------------------------------------------------------

#[test]
fn tx_requires_link_and_delegates_to_hal() {
    let mut st = WlanStation::<FakeWlanHw, 4>::new(FakeWlanHw::default());
    assert_eq!(st.tx(b"frame"), Err(DriverError::BadState));

    let mut hw = fake_with(&[bss([0x02, 0, 0, 0, 0, 0x11], b"home", -40)]);
    hw.queue_rx(b"resp").unwrap();
    let mut st = WlanStation::<FakeWlanHw, 4>::new(hw);
    let mut scan_out = [BssEntry::EMPTY; 4];
    assert_eq!(st.scan(&mut scan_out), Ok(1));
    let ssid = Ssid::new(b"home").unwrap();
    assert_eq!(st.join(&ssid, b""), Ok(()));

    assert_eq!(st.tx(b"frame"), Ok(()));
    assert_eq!(st.hw().tx_len, 5);

    let mut out = [0u8; 8];
    assert_eq!(st.rx(&mut out), Ok(4));
    assert_eq!(&out[..4], b"resp");
    assert_eq!(st.rx(&mut out), Ok(0));

    let stats = st.stats();
    assert_eq!(stats.tx_frames, 1);
    assert_eq!(stats.rx_frames, 1);
    assert_eq!(stats.tx_errors, 0);
    assert_eq!(stats.rx_errors, 0);
}

#[test]
fn loopback_hw_roundtrips_frames() {
    let hw = LoopbackHw::new();
    let bssid = hw.bssid;
    let mut st = WlanStation::<LoopbackHw, 2>::new(hw);
    let mut out = [BssEntry::EMPTY; 2];
    assert_eq!(st.scan(&mut out), Ok(1));
    assert_eq!(out[0].ssid.as_bytes(), b"loopback");

    let ssid = Ssid::new(b"loopback").unwrap();
    assert_eq!(st.join(&ssid, b""), Ok(()));
    assert_eq!(st.state(), LinkState::Linked { bssid });

    assert_eq!(st.tx(b"hello"), Ok(()));
    let mut rx = [0u8; 16];
    assert_eq!(st.rx(&mut rx), Ok(5));
    assert_eq!(&rx[..5], b"hello");
    assert_eq!(st.stats().tx_frames, 1);
    assert_eq!(st.stats().rx_frames, 1);
}

#[test]
fn fake_loopback_mode_roundtrips_frames() {
    let mut hw = fake_with(&[bss([0x02, 0, 0, 0, 0, 0x11], b"home", -40)]);
    hw.loopback = true;
    let mut st = WlanStation::<FakeWlanHw, 4>::new(hw);
    let mut out = [BssEntry::EMPTY; 4];
    assert_eq!(st.scan(&mut out), Ok(1));
    let ssid = Ssid::new(b"home").unwrap();
    assert_eq!(st.join(&ssid, b""), Ok(()));

    assert_eq!(st.tx(b"ping"), Ok(()));
    let mut rx = [0u8; 8];
    assert_eq!(st.rx(&mut rx), Ok(4));
    assert_eq!(&rx[..4], b"ping");
}

// ------------------------------------------------------------------
// 固件加载契约
// ------------------------------------------------------------------

#[test]
fn slice_firmware_checks_name_and_capacity() {
    let mut fw = SliceFirmware::new("fw.bin", &[1, 2, 3, 4]);
    let mut out = [0u8; 8];
    assert_eq!(fw.load("fw.bin", &mut out), Ok(4));
    assert_eq!(&out[..4], &[1, 2, 3, 4]);

    let mut small = [0u8; 2];
    assert_eq!(fw.load("fw.bin", &mut small), Err(DriverError::Invalid));
    assert_eq!(fw.load("other.bin", &mut out), Err(DriverError::NotFound));
}

#[test]
fn firmware_buffer_stages_and_guards_bounds() {
    let mut src = SliceFirmware::new("fw.bin", &[9, 8, 7]);
    let mut staged = FirmwareBuffer::<4>::new();
    assert_eq!(staged.fetch(&mut src, "fw.bin"), Ok(3));
    assert_eq!(staged.len(), 3);
    assert_eq!(staged.as_slice(), &[9, 8, 7]);

    // 作为 loader 只认同名固件。
    let mut out = [0u8; 4];
    assert_eq!(staged.load("fw.bin", &mut out), Ok(3));
    assert_eq!(&out[..3], &[9, 8, 7]);
    assert_eq!(
        staged.load("nope.bin", &mut out),
        Err(DriverError::NotFound)
    );

    // 输出缓冲不足 / 读取越界都返回 Invalid。
    let mut small = [0u8; 2];
    assert_eq!(staged.load("fw.bin", &mut small), Err(DriverError::Invalid));
    let mut two = [0u8; 2];
    assert_eq!(staged.read(2, &mut two), Err(DriverError::Invalid));
    let mut one = [0u8; 1];
    assert_eq!(staged.read(2, &mut one), Ok(1));
    assert_eq!(one[0], 7);

    // 固件大于缓冲容量时拒绝，不截断。
    let mut tiny = FirmwareBuffer::<2>::new();
    assert_eq!(tiny.fetch(&mut src, "fw.bin"), Err(DriverError::Invalid));
    assert!(tiny.is_empty());
}

#[test]
fn chunked_loader_yields_chunks_and_whole_image() {
    let mut src = SliceFirmware::new("fw.bin", &[1, 2, 3, 4, 5]);
    let mut cl: ChunkedLoader<'_, 8> = ChunkedLoader::new(&mut src);
    assert_eq!(cl.prefetch("fw.bin"), Ok(5));

    let mut a = [0u8; 2];
    assert_eq!(cl.next_chunk(&mut a), Ok(2));
    assert_eq!(a, [1, 2]);
    let mut b = [0u8; 2];
    assert_eq!(cl.next_chunk(&mut b), Ok(2));
    assert_eq!(b, [3, 4]);
    let mut c = [0u8; 2];
    assert_eq!(cl.next_chunk(&mut c), Ok(1));
    assert_eq!(c[0], 5);
    assert_eq!(cl.next_chunk(&mut c), Ok(0));

    cl.rewind();
    assert_eq!(cl.remaining(), 5);
    let mut whole = [0u8; 8];
    assert_eq!(cl.load("fw.bin", &mut whole), Ok(5));
    assert_eq!(&whole[..5], &[1, 2, 3, 4, 5]);

    let mut small = [0u8; 3];
    assert_eq!(cl.load("fw.bin", &mut small), Err(DriverError::Invalid));
}

// ------------------------------------------------------------------
// Driver 接线
// ------------------------------------------------------------------

#[test]
fn driver_probes_starts_and_delegates() {
    let mut drv = WlanDriver::<FakeWlanHw, 4>::new(FakeWlanHw::default());
    assert_eq!(drv.name(), "wlan-generic");
    assert_eq!(drv.class(), Class::Wlan);

    let dev = wlan_dev(0x8086, 0x2723);
    assert_eq!(drv.probe(&dev), Ok(true));
    assert_eq!(drv.probe(&ethernet_dev()), Ok(false));
    assert!(drv.wlan().is_none());

    assert_eq!(drv.start(&dev), Ok(()));
    assert_eq!(drv.station().unwrap().hw().reset_calls, 1);
    assert!(drv.wlan().is_some());
    // start 幂等。
    assert_eq!(drv.start(&dev), Ok(()));
    assert_eq!(drv.station().unwrap().hw().reset_calls, 1);

    // 固件加载委托给 HAL。
    let mut fw = SliceFirmware::new(FAKE_FIRMWARE, &[1, 2, 3]);
    assert_eq!(drv.load_firmware(&mut fw), Ok(()));
    assert!(drv.station().unwrap().hw().fw_loaded);
    assert_eq!(drv.station().unwrap().hw().fw_len, 3);

    // 通过 WlanOps 扫描。
    let mut out = [BssEntry::EMPTY; 4];
    assert_eq!(drv.wlan().unwrap().scan(&mut out), Ok(0));
    assert_eq!(drv.station().unwrap().stats().scan_rounds, 1);

    drv.stop();
    assert!(drv.wlan().is_none());
    drv.stop(); // 可重复
}

#[test]
fn driver_accepts_explicit_vid_pid() {
    let dev = DeviceInfo::usb(1, 2).with_ids(0x0BDA, 0xC821);

    let mut generic = WlanDriver::<FakeWlanHw, 2>::new(FakeWlanHw::default());
    assert_eq!(generic.probe(&dev), Ok(false));

    let mut vendor =
        WlanDriver::<FakeWlanHw, 2>::new(FakeWlanHw::default()).with_ids(0x0BDA, 0xC821);
    assert_eq!(vendor.probe(&dev), Ok(true));
    assert_eq!(vendor.start(&dev), Ok(()));
}

// ------------------------------------------------------------------
// 注册表槽位与覆盖
// ------------------------------------------------------------------

#[test]
fn generic_descriptor_claims_only_other_network_controller() {
    let desc = crate::registry::generic_descriptor();
    assert_eq!(desc.class, Class::Wlan);
    assert_eq!(desc.priority, crate::registry::GENERIC_PRIORITY);
    assert!(desc.claims(&wlan_dev(0x8086, 0x2723)));
    assert!(!desc.claims(&ethernet_dev()));
}

#[test]
fn vendor_descriptor_overrides_generic_in_registry() {
    let dev = wlan_dev(0x8086, 0x2723);
    let mut reg = Registry::<8>::new();
    reg.register(crate::registry::generic_descriptor()).unwrap();
    assert_eq!(reg.resolve(Class::Wlan, &dev).unwrap().id, "wlan-generic");

    fn claims_vendor(_dev: &DeviceInfo) -> bool {
        true
    }
    reg.register_override(DriverDescriptor {
        id: "wlan-iwlwifi",
        class: Class::Wlan,
        priority: crate::registry::VENDOR_PRIORITY,
        vendor_id: 0x8086,
        device_id: 0x2723,
        matches: claims_vendor,
    })
    .unwrap();

    assert_eq!(reg.resolve(Class::Wlan, &dev).unwrap().id, "wlan-iwlwifi");
}
