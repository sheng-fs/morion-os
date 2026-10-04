#!/bin/bash
# 模拟镜像全量 FS 回归: 7 个 namespace (FAT32 / MFS / ext2 / 分区盘 / exFAT / 空白盘 / 分区表盘)
# 上跑 app 的 FS-1..FS-29 自测, 由日志判定通过与否。
#
#   bash scripts/fs-regress.sh [日志路径]
#
# 环境变量: MFS_KEEP=1 保留既有 MFS 卷; REGRESS_TIMEOUT_S 放宽等待上限 (无 KVM 的 CI);
#           QEMU_EXTRA 追加 QEMU 参数; BIOS 覆盖 OVMF 路径;
#           REGRESS_ISO 指定别的 ISO (镜像变体: 如 REGRESS_ISO=build/install/morion-os.iso
#           验安装盘 —— 磁盘镜像仍用 $OUT_DIR/*.img, 变体之间共用测试盘)。
#
# ⏱️ 有 KVM 时整套约 6 分钟: 约 2 万个块请求, IPC 一跳 ≈ 一个时钟 tick, 故有效吞吐
# 约 100 请求/s。期间日志会长时间「只有 shell 提示符、没有新行」, 这不是卡死。
# 耗时集中在 FS-12 (MFS 目录与长名: 200 项 + 每次 3 遍显式全卷 GC), 单它就 ~170 s。
#
# 退出码: 0 = 自测跑完且无 FAILED/PANIC; 1 = 有失败或没等到结论。
set -u
cd "$(dirname "$0")/.."

OUT_DIR=${OUT_DIR:-build}
log=${1:-/tmp/morion-fs-regress.log}
rm -f "$log"

# MFS 是**持久卷**, 而 FS-5 / FS-10 每轮各留下一个快照 (环形槽位上限 8), 约 4 轮就饱和。
# 快照会永久钉住它当时的可达块, 而 `mfs_gc` 的可达根 = 当前 inode 表 **+ 每个快照**,
# 且对每个根完整重走一遍 —— 于是同一个二进制**越跑越慢** (实测: 空白卷自测 258 s,
# 快照环饱和后 524 s; FS-12 由 171 s 涨到 274 s)。故默认把卷重置成空白
# (mfs_srv 首次挂载会自动格式化), 让每轮耗时可比。想留着上一轮的卷用 MFS_KEEP=1。
# 空白盘 (spare.img) 每轮都重置: FS-22 要验证的正是「格式化一块**没有文件系统**的卷」,
# 保留上一轮格好的卷会让这条路径根本没被走到。
dd if=/dev/zero of="$OUT_DIR/spare.img" bs=1M count="${SPARE_MIB:-16}" status=none
# 分区表测试盘同理: FS-26 会建/删分区表, 保留上一轮的 GPT/MBR 会让「在空白盘上建表」
# 这条路径没被走到 (FS-26 自己也会先 wipe 一次, 但重置镜像让起点更干净)。
dd if=/dev/zero of="$OUT_DIR/pt.img" bs=1M count="${PT_MIB:-64}" status=none
# virtio-blk 测试盘 (驱动路线 D3): 每轮重建 —— 扇区 0 写**已知签名**, 供域 17 的
# virtio_blk_srv 自测「读扇区 0 校验签名 + 写扇区 1 读回」。盘内容必须确定 (签名是判据)。
dd if=/dev/zero of="$OUT_DIR/vblk.img" bs=1M count="${VBLK_MIB:-1}" status=none
printf 'MORION-VBLK-TST!' | dd of="$OUT_DIR/vblk.img" bs=512 count=1 conv=notrunc,sync status=none
# AHCI/SATA 测试盘 (驱动路线 D4): 同 vblk —— 每轮重建 + 扇区 0 写已知签名, 供域 18 的
# ahci_srv 做「IDENTIFY + DMA 读扇区 0 校验签名」自测 (只读: 驱动全程不发写命令)。
dd if=/dev/zero of="$OUT_DIR/ahci.img" bs=1M count="${AHCI_MIB:-1}" status=none
printf 'MORION-AHCI-TST!' | dd of="$OUT_DIR/ahci.img" bs=512 count=1 conv=notrunc,sync status=none
# USB 存储测试盘 (驱动路线 03c): 空白 raw + 扇区 0 已知签名, 供域 19 的 xhci_srv 经
# qemu-xhci + usb-storage 用 SCSI READ(10) 读回校验 (只读 bring-up)。
dd if=/dev/zero of="$OUT_DIR/usb.img" bs=1M count="${USB_MIB:-1}" status=none
printf 'MORION-USB-TST!!' | dd of="$OUT_DIR/usb.img" bs=512 count=1 conv=notrunc,sync status=none

# FAT32 卷 (nvme.img) 是**持久卷**, 脚本一直沿用既有的那份 (它同时被交互式验证用)。
# 但可执行文件加载 (FS-27 / shell `run`) 需要卷根目录里有 hello.mex —— 就地补进去,
# 免去"跑回归前必须先 make 一遍镜像"的隐含前提。mtools 是既有依赖 (Makefile 也在用)。
if [ -f "$OUT_DIR/nvme.img" ]; then
  if [ -f "$OUT_DIR/user/hello.elf" ]; then
    mcopy -o -i "$OUT_DIR/nvme.img" "$OUT_DIR/user/hello.elf" ::/hello.mex 2>/dev/null \
      && echo "== 已注入可执行文件: /hello.mex"
  else
    echo "== 警告: $OUT_DIR/user/hello.elf 不存在, FS-27 (可执行文件加载) 会失败"
  fi
fi
# 服务镜像 (E3c): 监督者 init 的**重启源**在同一张根盘上 (`/system/services/*.elf`)。
# 与 hello.mex 同样的理由就地补齐 —— 否则拿一份旧的 nvme.img 跑回归时, FS-29 (退出→重启)
# 会因为盘上没有服务镜像而失败, 而现象看着像"监督者坏了"。
if [ -f "$OUT_DIR/nvme.img" ] && [ -d "$OUT_DIR/user/srv" ]; then
  mmd -i "$OUT_DIR/nvme.img" ::/system 2>/dev/null
  mmd -i "$OUT_DIR/nvme.img" ::/system/services 2>/dev/null
  srv_n=0
  for f in "$OUT_DIR"/user/srv/*.elf; do
    mcopy -o -i "$OUT_DIR/nvme.img" "$f" "::/system/services/$(basename "$f")" 2>/dev/null \
      && srv_n=$((srv_n + 1))
  done
  echo "== 已注入服务镜像: /system/services/*.elf ($srv_n)"
fi
if [ "${MFS_KEEP:-0}" = "1" ]; then
  echo "== 保留既有 MFS 卷: $OUT_DIR/mfs.img (MFS_KEEP=1)"
else
  dd if=/dev/zero of="$OUT_DIR/mfs.img" bs=1M count="${MFS_MIB:-256}" status=none
  echo "== 重置 MFS 卷: $OUT_DIR/mfs.img -> 空白 (首次挂载自动格式化)"
fi

# 记录耗时: 整套自测是一长串 IPC 往返 (每次请求 ≈ 一个时钟 tick), 加自测用例就会变慢,
# 写进输出便于对比「是变慢了还是卡住了」。
t0=$(date +%s)

QEMU=${QEMU:-qemu-system-x86_64}
BIOS=${BIOS:-/usr/share/edk2/x64/OVMF.4m.fd}
# 默认跑本目录构建的镜像; 变体 (如安装盘) 用 REGRESS_ISO 指过去。
ISO=${REGRESS_ISO:-$OUT_DIR/morion-os.iso}

# 加速: 有 KVM 就用 (-enable-kvm); 没有 (多数 CI runner) 退回 TCG —— 结果一样但要慢
# 好几倍, 故等待上限用 REGRESS_TIMEOUT_S 放宽 (默认 600 s, 按 KVM 下 ~6 分钟定的)。
accel=""
if [ -r /dev/kvm ] && [ -w /dev/kvm ]; then
  accel="-enable-kvm"
else
  echo "== 无可用 /dev/kvm: QEMU 走 TCG, 会明显变慢 (用 REGRESS_TIMEOUT_S 放宽等待)"
fi
timeout_s=${REGRESS_TIMEOUT_S:-600}

# 可选: IOMMU=1 时让 QEMU 暴露 Intel VT-d —— 校验 E1b 的 DMA 重映射路径 (翻译打开后
# 设备 DMA 仍通)。新版 QEMU (11.x) 已移除 `-machine ...,intel-iommu=on`, 须用 `-device intel-iommu`。
iommu_arg=""
if [ -n "${IOMMU:-}" ]; then
  iommu_arg="-device intel-iommu"
  echo "== QEMU 暴露 Intel VT-d (IOMMU=1): 校验 E1b DMA 重映射"
fi

# ISO9660 只读副本 (03c 续): `morion-os.iso` 同一文件不能既作 `-cdrom` 又被 QEMU 当块设备
# 打开, 故拷一份接成 nvme-ns nsid=8 供域 20 的 iso9660_srv 读整盘 ISO9660。
cp -f "$ISO" "$OUT_DIR/iso.img"

$QEMU \
  -machine q35 ${iommu_arg} -m "${QEMU_MEM:-2G}" -bios "$BIOS" \
  -cdrom "$ISO" \
  -device nvme,serial=MORION,id=nvme0 \
  -drive file="$OUT_DIR/nvme.img",if=none,id=n1,format=raw -device nvme-ns,drive=n1,bus=nvme0,nsid=1 \
  -drive file="$OUT_DIR/mfs.img",if=none,id=n2,format=raw -device nvme-ns,drive=n2,bus=nvme0,nsid=2 \
  -drive file="$OUT_DIR/ext2.img",if=none,id=n3,format=raw -device nvme-ns,drive=n3,bus=nvme0,nsid=3 \
  -drive file="$OUT_DIR/parts.img",if=none,id=n4,format=raw -device nvme-ns,drive=n4,bus=nvme0,nsid=4 \
  -drive file="$OUT_DIR/exfat.img",if=none,id=n5,format=raw -device nvme-ns,drive=n5,bus=nvme0,nsid=5 \
  -drive file="$OUT_DIR/spare.img",if=none,id=n6,format=raw -device nvme-ns,drive=n6,bus=nvme0,nsid=6 \
  -drive file="$OUT_DIR/pt.img",if=none,id=n7,format=raw -device nvme-ns,drive=n7,bus=nvme0,nsid=7 \
  -drive file="$OUT_DIR/iso.img",if=none,id=n8,format=raw,readonly=on -device nvme-ns,drive=n8,bus=nvme0,nsid=8 \
  -netdev user,id=n0 -device virtio-net-pci,netdev=n0,mac=52:54:00:12:34:56 \
  -netdev user,id=n1 -device e1000e,netdev=n1,mac=52:54:00:aa:bb:cc \
  -drive file="$OUT_DIR/vblk.img",if=none,id=vblk0,format=raw \
  -device virtio-blk-pci,drive=vblk0 \
  -drive file="$OUT_DIR/ahci.img",if=none,id=ahci0,format=raw \
  -device ide-hd,drive=ahci0 \
  -drive file="$OUT_DIR/usb.img",if=none,id=usb0,format=raw \
  -device qemu-xhci,id=xhci \
  -device usb-storage,bus=xhci.0,drive=usb0 \
  -display none -monitor none -serial file:"$log" -no-reboot ${accel} ${QEMU_EXTRA:-} \
  >/dev/null 2>&1 &
pid=$!

# 出现结论或失败即提前收工; 否则最多等 timeout_s 秒 (每 5 s 轮询一次)。
for _ in $(seq 1 $((timeout_s / 5))); do
  sleep 5
  if grep -qE 'SELFTEST DONE|KERNEL PANIC' "$log" 2>/dev/null; then break; fi
  # ⚠️ 别拿**任何** FAILED 当收工信号: shell 启动时那句
  # `shell: screen console mirror FAILED (gfx_srv cursor not advanced)` 是 gfx_srv 尚未就绪时
  # 的无害竞态提示 (usb-rw.sh 也踩过同一个坑), 它一出现就退出会把整轮误判成失败
  # (实测: 13 秒即退、SELFTEST DONE 0)。故把这一句滤掉再判。
  if grep -E 'FAILED' "$log" 2>/dev/null | grep -qv 'screen console mirror FAILED'; then break; fi
  kill -0 "$pid" 2>/dev/null || break
done
sleep 3
kill "$pid" 2>/dev/null
wait "$pid" 2>/dev/null

done_n=$(grep -c 'SELFTEST DONE' "$log" 2>/dev/null || true)
# 判定用的 FAILED/PANIC 计数同样要滤掉那句无害竞态提示 (理由见上面收工判据), 否则一轮
# 正常的回归会被它判成败 —— 实测: 整轮只有这一句, 也会让退出码变 1。
harmless='screen console mirror FAILED'
fail_n=$(grep -E 'FAILED|PANIC' "$log" 2>/dev/null | grep -cv "$harmless" || true)
echo "== 日志: $log"
echo "== 耗时: $(($(date +%s) - t0)) 秒 (出现结论即停, 最长等 ${timeout_s} 秒)"
echo "== SELFTEST DONE 次数: $done_n"
echo "== FAILED/PANIC 次数: $fail_n"
echo "== 卷表 (block_srv vol: 行) =="
grep -nE '^vol: ' "$log" 2>/dev/null || echo "(无)"
echo "== 额外卷挂载 (mount-dbg) =="
grep -nE 'mount-dbg' "$log" 2>/dev/null || echo "(无)"
echo "== 显式格式化 (mkfs) =="
grep -nE 'mfs: mkfs refused|mfs: mkfs FAILED|mfs: reload state after mkfs' "$log" 2>/dev/null || echo "(无拒绝/无失败)"
echo "== 分区表写入 (part) =="
grep -nE 'part-dbg:|block: part .*refused' "$log" 2>/dev/null || echo "(无)"
# 跨实现校验: FS-26 结束时在 pt.img 上留了一张 GPT, 用宿主工具独立验证它**符合 GPT 规范**
# —— 客户机里复算 CRC32 只能证明这张表「自洽」, 证不了 GUID 字节序 / 头字段这些规范细节。
# sgdisk 缺失时跳过 (判定口径不变; 有它时「不是 no problems found」即判失败)。
host_bad=0
echo "== 分区表宿主校验 (pt.img) =="
if command -v sgdisk >/dev/null 2>&1; then
  sgdisk_out=$(sgdisk -v "$OUT_DIR/pt.img" 2>&1)
  echo "$sgdisk_out" | sed 's/^/  /'
  sgdisk -p "$OUT_DIR/pt.img" 2>&1 | sed 's/^/  /'
  echo "$sgdisk_out" | grep -qi 'no problems found' || host_bad=1
else
  echo "  (无 sgdisk, 跳过宿主校验)"
fi
# 驱动路线 D3: 第二个真实驱动 virtio-blk 的端到端取证 —— 域 17 的 virtio_blk_srv 起来后
# 读扇区 0 校验宿主预写的签名 + 写扇区 1 读回校验, 打 `VBLK1` marker。缺 marker (或 sig/rw
# 不是 ok) 即判失败: 这条链路覆盖描述符链 + avail/used 环 + MSI-X 中断整条通路。
vblk_bad=0
echo "== virtio-blk 驱动自测 (D3) =="
grep -nE 'vblk:|VBLK1' "$log" 2>/dev/null || echo "(无)"
grep -qE 'VBLK1 virtio-blk OK.*sig=ok.*rw=ok' "$log" 2>/dev/null || vblk_bad=1
# 驱动路线 D4: AHCI/SATA 驱动 (域 18) —— ① IDENTIFY + LBA48 DMA 读扇区 0 校验宿主预写签名
# (`AHCI1`); ② 03b 起把盘**挂进 block_srv 的卷层** (`block: ahci volume attached`), 并经卷层
# 转发做一次写回读自测 (`AHCI2`)。三者缺任一即判失败。
ahci_bad=0
echo "== AHCI/SATA 驱动自测 + 卷层挂载 (D4/03b) =="
grep -nE 'ahci:|AHCI1|AHCI2|block: ahci volume' "$log" 2>/dev/null || echo "(无)"
grep -qE 'AHCI1 ahci OK.*sig=ok' "$log" 2>/dev/null || ahci_bad=1
grep -qE 'block: ahci volume attached.*sig=ok' "$log" 2>/dev/null || ahci_bad=1
grep -qE 'AHCI2 ahci volume rw OK.*rw=ok' "$log" 2>/dev/null || ahci_bad=1
# 驱动路线 03c: xHCI/USB 存储驱动 (域 19) —— ① 枚举 + BOT/SCSI 读扇区 0 校验宿主签名 (`USB1`);
# ② 把 U 盘**挂进 block_srv 的卷层** (`block: usb volume attached`), 并经卷层转发做一次写回读
# 自测 (`USB2`)。三者缺任一即判失败。
usb_bad=0
echo "== xHCI/USB 存储驱动自测 + 卷层挂载 (03c) =="
grep -nE 'xhci:|USB1|USB2|block: usb volume' "$log" 2>/dev/null || echo "(无)"
grep -qE 'USB1 xhci OK.*sig=ok' "$log" 2>/dev/null || usb_bad=1
grep -qE 'block: usb volume attached.*sig=ok' "$log" 2>/dev/null || usb_bad=1
grep -qE 'USB2 usb volume rw OK.*rw=ok' "$log" 2>/dev/null || usb_bad=1
# 03c 续: ISO9660 只读文件服务 (域 20) —— 把 morion-os.iso 的只读副本接成 nsid=8, 服务读整盘
# ISO9660 并校验根下 EFIBOOT.IMG 的 FAT 引导签名 (0xEB/0xE9 跳转 + 尾 0x55AA) → `ISO1`。
iso_bad=0
echo "== ISO9660 只读驱动自测 (03c 续) =="
grep -nE 'iso9660:|ISO1' "$log" 2>/dev/null || echo "(无)"
grep -qE 'ISO1 iso9660 OK.*read=ok' "$log" 2>/dev/null || iso_bad=1
# ② 能力审计: init 监督者按最小权限策略核对引导期的能力授权 (Spawn / Mmio / Fb / IoPort)。
# 判据是 `cap-audit: OK` —— 出现 VIOLATION / MISSING / FAILED 都判失败 (审计本身也读日志)。
cap_bad=0
echo "== 能力审计 (② cap-audit) =="
grep -nE 'cap-audit:' "$log" 2>/dev/null || echo "(无)"
grep -qE 'cap-audit: OK' "$log" 2>/dev/null || cap_bad=1
# 网络能力 (N5–N6): net_srv 帧级 NIC 驱动自测 (NET1 ARP / NET2 ipv4+icmp / NET3 DHCP / NET4 TCP)
# + netstack_srv 协议栈起来 (frame link) + N6.6 应用经 libnetv 的 UDP socket 端到端
# (端口能力门禁拒绝越权 + 回环收发 + 真实 TX 触发 ICMP 不可达回程)。任一缺即判失败。
net_bad=0
echo "== 网络能力自测 (N5/N6: NET1..NET4 + netstack + NET5) =="
grep -nE 'NET1|NET2|NET3|NET4|NET6|NET7|NET8|NET9|NET11|NET12|NET13|netstack:|app: NET5|e1000e:' "$log" 2>/dev/null || echo "(无)"
grep -qE 'NET1 virtio-net up, MAC=.*ARP reply OK' "$log" 2>/dev/null || net_bad=1
grep -qE 'NET3 dhcp OK, ip=' "$log" 2>/dev/null || net_bad=1
grep -qE 'NET2 ipv4/icmp OK.*udp TX 10.0.2.2:9999 -> icmp unreachable' "$log" 2>/dev/null || net_bad=1
grep -qE 'NET4 tcp OK, handshake\+data selftest OK' "$log" 2>/dev/null || net_bad=1
grep -qE 'netstack: up \(frame link to net_srv OK\)' "$log" 2>/dev/null || net_bad=1
grep -qE 'app: NET5 udp socket OK' "$log" 2>/dev/null || net_bad=1
grep -qE 'netstack: nic rx \(icmp unreachable\) OK' "$log" 2>/dev/null || net_bad=1
# N9 (第二台真网卡): e1000e 读 MAC + 广播 ARP 收应答 → `NET9 e1000e OK … ARP reply OK`。
grep -qE 'NET9 e1000e OK.*ARP reply OK' "$log" 2>/dev/null || net_bad=1
# N9.2 (多网卡出口): 协议栈把 e1000e 当 NIC1 拉起 (同一套帧级 IPC), 应用经 NIC1 用同一套
# socket API 收发 (回环 + 真实 TX 触发 ICMP 不可达回程)。上层 socket API 不变。
grep -qE 'netstack: nic1 up \(e1000e OK\)' "$log" 2>/dev/null || net_bad=1
grep -qE 'app: NET11 udp via e1000e \(nic1\) OK' "$log" 2>/dev/null || net_bad=1
grep -qE 'netstack: nic1 rx \(icmp unreachable\) OK' "$log" 2>/dev/null || net_bad=1
# N7 (TCP 完整化): netstack 的 TCP 连接状态机 + 重传的确定性自证。
grep -qE 'NET6 tcp conn OK' "$log" 2>/dev/null || net_bad=1
# N7.2 (TCP socket): 应用经 socket API 主动连接真实对端 → 真实 SYN 发出 + 对端 RST 收回。
grep -qE 'netstack: tcp peer refused \(RST\) OK' "$log" 2>/dev/null || net_bad=1
grep -qE 'app: NET12 tcp client OK' "$log" 2>/dev/null || net_bad=1
# N8 (应用面 + DNS): DNS 解析器确定性自证 + 经 slirp 内置 DNS 的真实 A 查询; 应用面 socket 汇总。
grep -qE 'app: NET8 dns parser OK, A=' "$log" 2>/dev/null || net_bad=1
grep -qE 'app: NET8 dns OK, A=' "$log" 2>/dev/null || net_bad=1
grep -qE 'app: NET7 app socket OK' "$log" 2>/dev/null || net_bad=1
# N8.2 (回环 TCP + 客户机内建 HTTP): 应用对 10.0.2.15:80 发起真实 HTTP GET, 返回 200 + body。
grep -qE 'app: NET13 http OK' "$log" 2>/dev/null || net_bad=1
echo "== 可执行文件加载 + 退出即回收 (E1/E2b: FS-27 / FS-28) =="
# app 自测把一份独立编译的 ELF 写进 /tmp 再读回来, 交给内核载入**新域**运行;
# 子程序 (user/hello) 自己打印 `exec:` 行 —— 两行都在才说明"加载 + 真的跑起来"。
# FS-28 进一步验证子程序退出后内核**自动回收**该域 (域号复用、空闲帧回到稳态)。
grep -nE 'FS27|FS28|FS29|GS1|GT1|exec: |init: restarted|gfx: |screen console' "$log" 2>/dev/null || echo "(无)"
echo "== 失败明细 =="
grep -nE 'FAILED|PANIC' "$log" 2>/dev/null | grep -v "$harmless" || echo "(无)"

[ "${done_n:-0}" -ge 1 ] && [ "${fail_n:-0}" -eq 0 ] && [ "${host_bad:-0}" -eq 0 ] && [ "${vblk_bad:-0}" -eq 0 ] && [ "${ahci_bad:-0}" -eq 0 ] && [ "${usb_bad:-0}" -eq 0 ] && [ "${iso_bad:-0}" -eq 0 ] && [ "${cap_bad:-0}" -eq 0 ] && [ "${net_bad:-0}" -eq 0 ]
