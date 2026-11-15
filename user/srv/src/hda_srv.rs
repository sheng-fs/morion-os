//! 域 27 — hda_srv：通用 HDA 音频驱动服务。
//!
//! 驱动核心在 `user/drivers/hda`（Intel HDA / AMD 同规范），本文件只做三件事：
//!
//! 1. 从 `DeviceGrant` 取出 BAR0 与 DMA 块，填成 [`HdaResources`]；
//! 2. 用通用 codec 发现初始化控制器（不硬编码 QEMU/任何厂商的 NID）；
//! 3. 播放一小段方波作为自测，打 `HDA1` / `HDA2` marker。
//!
//! 厂商要换自己的 codec 配置时，实现 `morion_hda::CodecProfile` 并在这里替换
//! `GenericProfile` 即可，服务层与 IPC 契约不变。

use libdevice::grant::DeviceGrant;
use libdevmodel::{AudioFormat, AudioOps, DeviceInfo, DriverError};
use morion::syscall::{print, print_hex, print_u64, println, sys_sleep};
use morion_hda::{HdaLog, HdaResources, MmioHdaDriver};

/// 把驱动核心的日志转发到内核终端。
struct ScreenLog;

impl HdaLog for ScreenLog {
    fn log(&mut self, msg: &str) {
        println(msg);
    }
}

fn err_name(e: DriverError) -> &'static str {
    match e {
        DriverError::NotSupported => "NotSupported",
        DriverError::NotFound => "NotFound",
        DriverError::Timeout => "Timeout",
        DriverError::Io => "Io",
        DriverError::BadState => "BadState",
        DriverError::Busy => "Busy",
        DriverError::Invalid => "Invalid",
        DriverError::Other(s) => s,
    }
}

/// 服务常驻：驱动跑起来后不再退出，避免 init 反复重启。
fn idle() -> ! {
    loop {
        sys_sleep(1000);
    }
}

pub fn run() {
    let grant = DeviceGrant::load();
    if !grant.is_valid() {
        println("HDA0 hda_srv up (no HDA controller granted)");
        idle();
    }
    println("HDA0 hda_srv up (generic Intel/AMD HDA)");

    // 设备描述：内核的设备授权窗口不含 VID:PID，通用 HDA 驱动只按 class 认领。
    let dev = DeviceInfo::pci(
        0,
        0,
        0,
        0,
        0,
        0x04,
        0x03,
        0,
        grant.bar_paddr,
        grant.bar_bytes,
    );
    let resources = HdaResources {
        mmio_vaddr: grant.bar_vaddr,
        mmio_len: grant.bar_bytes,
        dma_vaddr: grant.dma_vaddr,
        dma_paddr: grant.dma_paddr,
        dma_len: grant.dma_bytes,
    };

    let mut drv = MmioHdaDriver::new_mmio(grant.bar_vaddr, ScreenLog);
    if let Err(e) = drv.start_with(&dev, resources) {
        print("HDA1 start FAILED: ");
        println(err_name(e));
        idle();
    }

    match drv.codec_info() {
        Some(ci) => {
            print("HDA1 codec=addr ");
            print_u64(ci.address as u64);
            print(" afg=");
            print_u64(ci.afg_nid as u64);
            print(" dac=");
            print_u64(ci.dac_nid as u64);
            print(" pin=");
            print_u64(ci.pin_nid as u64);
            print(" vid:did=");
            print_hex(ci.vendor_id as u64);
            print(":");
            print_hex(ci.device_id as u64);
            print(" name=");
            println(ci.name);
        }
        None => println("HDA1 no codec discovered"),
    }

    // 自测：开 48 kHz/16 bit/双声道，写 8 段 440 Hz 方波（整段约 0.17 s）。
    let fmt = AudioFormat::S16_48K_STEREO;
    if let Err(e) = drv.open(fmt) {
        print("HDA2 open FAILED: ");
        println(err_name(e));
        idle();
    }
    let _ = drv.set_volume(80, 80);

    let mut buf = [0i16; 2048];
    let mut written = 0u64;
    for _ in 0..8 {
        // 109 个样本 ≈ 440 Hz @ 48 kHz；左右声道相同。
        for frame in 0..buf.len() / 2 {
            let phase = (frame as u32) % 109;
            let s: i16 = if phase < 54 { 3000 } else { -3000 };
            buf[frame * 2] = s;
            buf[frame * 2 + 1] = s;
        }
        match drv.write_pcm(&buf) {
            Ok(n) => written += n as u64,
            Err(e) => {
                print("HDA2 write FAILED: ");
                println(err_name(e));
                break;
            }
        }
    }
    let drained = drv.drain().is_ok();
    drv.close();

    print("HDA2 play fmt=48k/16/2 samples=");
    print_u64(written);
    println(if drained {
        " drain=ok"
    } else {
        " drain=timeout"
    });

    idle();
}
