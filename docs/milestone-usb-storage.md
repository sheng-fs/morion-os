# 里程碑：真机 USB 3.2 10Gbps 存储 + 持久化分区打通

日期：2026-11-08

## 结果

在 ASUS/AMD 笔记本 + thinkplus 256GB（`090c:2320`）上，MorionOS 首次完整跑通
**xHCI → BOT/SCSI → block_srv → FAT32/exFAT → 根文件系统 + 数据分区**：

```
port 4 auto-ss: ccs=1 ped=1 pp=1 pls=0 spd=5
address slot=1 port=4 spd=5 mps=512
device descriptor vid=0x90c pid=0x2320 bMaxPacket0=9
config class=0x8 sub=0x6 proto=0x50 bulk_in=1 bulk_out=1
bulk-in dci=5 mps=1024 out dci=2 mps=1024
inquiry vendor=SMI USB DISK
capacity sectors=500118192 blk=512
USB1 xhci OK, cap=500118192, sector0 sig=BAD
block: usb volume attached (vol=2, sectors=500118192, sig=BAD)
fat32: volume vol=2 cluster=512 fatsz=1847 root=2
exfat-dbg: vol=2 cluster=32768 clusters=16320 root=4 bitmap=2040 upcase=5836
```

- `spd=5` = SuperSpeedPlus 10Gbps。
- BOT alt0 的 **EP1 OUT / EP2 IN** 被正确选中（`dci=2 / 5`）。
- INQUIRY、READ CAPACITY(10)、READ(10) 全部成功，识别为 256GB（500,118,192 扇区）。
- 64MiB FAT32 ESP 挂到 `/`（`ls /` → `EFI`）。
- 512MiB exFAT `PERSIST` 分区挂到 `/usb`（`ls /usb` 可打开）。
- 在 shell 里成功执行 `touch /usb/ok.txt`、`dmesg /usb/usb.log`（报告 8810 字节）。

## 这一路修了什么

1. **SuperSpeed 链路训练**：控制器 `RS=1` 后等 RxDetect → Polling → U0，
   不再刚启动就把 `CCS=0` 的 SS 口当空端口跳过。
2. **Warm Port Reset**：已连接但未使能的 USB3 端口先写 `PORT_WPR`，让链路重新训练；
   USB2 端口退回普通 `PORT_PR`。
3. **UAS 盘的 alt0 选择**：UAS 盘同时暴露 `alt0=BOT(0x50)` / `alt1=UAS(0x62)`，
   只解析 alt0 的端点，避免把 EP3/EP4 当成 Bulk 对导致 INQUIRY 超时。
4. **exFAT 迟挂载**：`exfat_srv` 改用 `vol_claim_wait`，等 USB 卷出现再认领；
   之前立即回退卷 5，导致根 FAT32 挂上、`/usb` 永远不出现。
5. **持久化数据分区**：新增 `make SAFE=1 usbimg-persist`，在 ESP 后追加 512MiB exFAT；
   ESP 仍是唯一 FAT32 卷，因此 `/` 不会误落到数据分区。

## 已知问题

- **PERSIST 写入跨关机/拔盘不持久**：MorionOS 内 `touch` / `dmesg` 报告成功，
  但把 U 盘接回 Linux 后 `/usb` 是空的。需要补：
  - exFAT 写回缓存的显式 flush；
  - BOT 写路径的 `SYNCHRONIZE CACHE(10)`；
  - 关机/重启前的文件系统 sync。
- 每台控制器目前只枚举一台设备：06:00.3 上的 U 盘优先，外接键盘让位；
  内置键盘（i8042）可用。
- 只打印 Hub（class 09），不扫描下游；没有 Route String / TT。
- 只支持 BOT，不支持 UAS；xHCI 仍为全轮询（未启用 MSI-X）。
- 根 `/` 仍是 64MiB ESP；大容量数据区只有 512MiB PERSIST，其余 237GB 未分配。

## 下一步

1. 写入持久化：exFAT flush + SYNCHRONIZE CACHE + 关机 sync，再验证 `ok.txt` 跨重启。
2. 多设备 / 多 slot：一台控制器同时保留 U 盘与键盘。
3. Hub + TT + Hub 中断端点：扫描内置 Hub 下游的键盘/鼠标/蓝牙。
4. UAS 后端；端口变更事件 / 热插拔；可选 MSI-X。
5. `USB CENSUS` 诊断模式：一次启动输出完整控制器/端口/Hub/设备树。

> 构建说明：真机验证镜像的版本串是 `20a5dd4`；exFAT 修复在构建前已进入工作树，
> 随后提交为 `25ff28b`。
