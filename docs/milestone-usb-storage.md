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

## 持久化验证（2026-11-09）

真机 `dmesg usb/usb.log` 报告写入 13597 字节；把同一块 U 盘接回 Linux 后：

```
/run/media/jinjun/PERSIST/usb.log   13587 字节
head: Morion OS Kernel / Stage 1: CPU initialization
tail: [morion@morion /]$ dmesg usb/usb.log
```

文件可见、可读，说明写入已经真正落盘。修复点：

- **shell**：分配/共享 `SHELL_WRITE_BUF`，`dmesg` 写文件改走它（之前误用 app 的
  `WRITE_BUF`，exFAT 收到全 0 数据，目录项正常但内容为空）。
- **exFAT NameHash**：按 UTF-16 低/高字节分两次 rotate + add（对齐内核/exfatprogs）；
  之前把整个 u16 一次累加，Linux/Windows 会判目录项损坏，表现为空分区。
- **exFAT / FAT32**：每个请求处理完也 `block_wb_flush`，最后一批写不再滞留。
- **xHCI**：`WRITE(10)` 成功后发 `SYNCHRONIZE CACHE(10)`，把 U 盘内部缓存刷到闪存。
- **签名诊断**：扇区 0 分类为 `test/gpt/mbr/exfat/fat32/ntfs/none`；真机日志现在是
  `sector0=gpt` / `sig=gpt`，并明确打印 `write self-test skipped`，不再误报 `sig=BAD`。

QEMU 回归（AHCI + qemu-xhci + usb-storage exFAT）：写入后 `fsck.exfat` 为
`clean. directories 1, files 1`，Linux 挂载可读到 `log.txt`。

## USB CENSUS（2026-11-10）

启动时 `xhci_srv` 会先做一次**只读普查**：遍历所有已授权控制器的全部根端口，
对每台已连接设备走一遍 Enable Slot → Address Device → 读设备/配置描述符，打印一行后
立即 `Disable Slot` —— 不挂卷、不写盘。之后才进入正常的存储/键盘探测，行为与之前一致。

每台设备一行（拍照即可还原整机拓扑）：

```
xhci: census ctrl#0 port=1 spd=4 vid=0x46f4 pid=0x1 class=0x0 ifaces=1 bulk=1 hid=0 result=storage
xhci: product="QEMU USB HARDDRIVE"
xhci: census ctrl#0 port=6 spd=3 vid=0x627 pid=0x1 class=0x0 ifaces=1 bulk=0 hid=1 result=hid
xhci: product="QEMU USB Keyboard"
```

最后一行是整机汇总（`hub` 目前只计数、不扫下游）：

```
USB REPORT: ctrls=1 ports=8 connected=2 storage=1 hid=1 hub=0 other=0 errors=0
```

失败也会留痕：端口连上了但链路没起来是 `result=link-fail`，描述符读不出是
`result=enum-fail`，两者都计入 `errors`。换机器/换拓扑后先看这几行，不必再靠猜。

QEMU 回归（`qemu-xhci` + `usb-storage` + `usb-kbd`）：普查同时识别出 port 1 的
storage 与 port 6 的 HID，正常存储链路仍是 `USB1 xhci OK` / `USB2 usb volume rw OK`。

## 启动提速与同控制器多设备（2026-11-11）

**链路训练等待批量化**：原先 13 台控制器各自 `wait_connect()`（空控制器 250 ms 串行叠加），
启动到 `USB REPORT` 实测 **12325 ms**。现在把“初始化全部控制器 → 一轮全局轮询 → 逐个探测”
拆成三阶段（`init_controller` / `wait_connect_all` / `probe_started`），13 台降到
**约 8.0–8.2 s**；单控制器仍约 7.5 s（无回归）。

**轮询延迟**：`wait_event` 的空轮询从“每次 `sys_sleep(1)`”改成“先 512 次有界自旋，
再让出时间片”。批量传输完成事件通常几十微秒就到，原实现每笔 4 KiB 传输凭空多 3–5 ms。

**每控制器 2 个 slot**：`Xhci` 只保留控制器级状态，新增 `Slot`（device context / EP0 环 /
Bin / Bout / data 页各一页），传输与 SCSI 接口显式接收 `&mut Slot`。同一台控制器上的
U 盘 + HID 键盘可以**同时**被服务；其它 slot 的完成事件由 `wait_event` 暂存，交由
`KeyboardDev::poll` 取走。

**DMA 扩容**：驱动 `DMA_PAGES` 16 → 24（页 0–11 控制器级 + 5 页 scratchpad，
12–16 / 17–21 两组 slot，22–23 预留）；内核 xHCI 授权 `dma_pages: 24`，
`DEVICE_MULTI_DMA_STRIDE` 16 → 24 页。16 个实例窗口
`[0x65_0000, 0x65_0000 + 16×0x1_8000 = 0x7D_0000)`，仍不与单实例窗口
（`0x81_0000` 起）或 wifi 共享页（`0xA0_0000`）重叠。

QEMU 验收（storage + kbd 同挂 `xhci.0`）：同时出现 `USB1 xhci OK`、
`xhci: USB HID keyboard ready`、`block: usb volume attached`、`USB2 usb volume rw OK`；
13 控制器 bench 仍是 `elapsed_ms≈8.0 s`、`ports=104`。

## Hub 下游遍历（2026-11-11）

支持**一层 USB2 Hub**：Hub 自身占一个 slot，下游设备占另一个 slot。

- 读 Hub 描述符（0x29）拿 `bNbrPorts` / `wHubCharacteristics` / `bPwrOn2PwrGood` / `bDeviceProtocol`；
- 下游端口上电 → 等 `bPwrOn2PwrGood×2 ms` → 轮询 `GET_PORT_STATUS` 等 CCS → `SET_PORT_FEATURE(PORT_RESET)`
  → 清 `C_PORT_RESET` / `C_PORT_CONNECTION`；
- 下游设备的 slot 上下文写入 **Route String**（只含 Hub 下游端口号；根端口仍放 DWORD1 的
  Root Hub Port Number —— 按“根端口也在 Route String 里”写会被控制器回 `cc=5`）、Hub 位、MTT 位、
  Number of Ports 与 TT 三字段（HS Hub 下挂 FS/LS 设备必需）。

QEMU 验收（`qemu-xhci` + `usb-hub` + 下游 `usb-kbd`）：

```
xhci: census ctrl#0 port=5 route=0x0 spd=1 vid=0x409 pid=0x55aa class=0x9 ... result=hub
xhci: hub port 2 connected status=0x10101
xhci: USB HID keyboard ready (port=2, slot=2, dci=3) interval=10
USB REPORT: ctrls=1 ports=8 connected=2 storage=0 hid=1 hub=1 other=0 errors=0
```

直连存储 + 键盘的回归不受影响（`USB1 xhci OK` / `USB2 usb volume rw OK`）。

## 运行期热插拔（2026-11-11）

服务循环在**两笔 block 请求之间**轮询根端口 PORTSC 的 CSC / PRC：

- 接入：清理变化位后走完整 `enumerate_port` + 存储/键盘配置，成功即加入服务；
- 拔出：打印 `xhci: hotplug ... removed`，把该 slot 标记为不可用，后续读写直接失败（不 panic）；
- 没有空闲 slot 时打印 `result=no-free-slot` 并跳过。

QEMU 实测（启动后 `device_add usb-kbd,bus=xhci.0` / `device_del`）：

```
xhci: hotplug ctrl#0 port=6 ccs=1
xhci: USB HID keyboard ready (port=6, slot=2, dci=3) interval=7
xhci: hotplug ctrl#0 port=6 ccs=0
xhci: hotplug ctrl#0 port=6 removed
```

## 已知问题

- 每台控制器最多 **2 个 slot**（一存储 + 一 HID）；第 3 台设备只出现在 census 行里
  （`result=no-free-slot`），不参与服务。
- 只支持**一层 USB2 Hub**；多层 Hub / SuperSpeed Hub 未做；Hub 下游端口暂未做运行期轮询
  （根端口已支持热插拔），Hub 中断端点未启用。
- 只支持 BOT，不支持 UAS；xHCI 仍为全轮询（未启用 MSI-X）。
- 根 `/` 仍是 64MiB ESP；大容量数据区只有 512MiB PERSIST，其余 237GB 未分配。

## 下一步

1. 多层 Hub / USB3 Hub；Hub 下游端口的运行期轮询 + 中断端点。
2. UAS 后端；可选 MSI-X。

> 构建说明：2026-11-08 真机验证镜像的版本串是 `20a5dd4`；exFAT 修复随后提交为
> `25ff28b`。2026-11-09 的持久化/签名修复提交为 `474e22a`。
