# 网络演进规划：驱动矩阵 + IPv6（N10 起）

> 状态：**规划稿（待开工）**。承接 [plan-network.md](plan-network.md)（N5–N9 + N8.2 拆分已全部落地）。
> 本规划把下一批网络工作拆成两条线：**驱动矩阵**（补齐可仿真网卡 + 非网络设备 + 无线抽象）与
> **IPv6**（地址/邻居/传输/应用面/双栈）。目标是「**协议栈从"能用 v4"到"能用 v6、能挂更多类设备"**」。
> 相关：[dev-reference.md](dev-reference.md)（网络 API 段 + 阶段 90–99）、[roadmap-driver.md](roadmap-driver.md)、
> [architecture.md](architecture.md) §网络、[app-dev-guide.md](app-dev-guide.md)。

---

## 1. 现状与硬约束

| 项 | 现状 | 出处 |
|---|---|---|
| 帧级驱动契约 | `NET_REQ_TAG("NETW")` + `NetReq{op,len,buf}`（`NET_OP_TX/RX/INFO`），帧经共享页同址传递，`NET_FRAME_MAX=2048` | `user/srv/src/common.rs` |
| 网卡驱动 | `net_srv`(域 16, virtio-net) / `e1000e_srv`(域 22, Intel 82574L) | `user/srv/src/net_srv.rs`、`e1000e_srv.rs` |
| 协议栈服务 | `netstack_srv`(域 21)：ARP / IPv4 / ICMP / UDP / TCP + socket 服务 + 端口能力门禁 | `user/srv/src/netstack_srv.rs` |
| 多网卡 | **硬编码**：`NIC_COUNT = 2` + `Link{domain,io}` 常量表（域 16/IO `+0x1A_0000`、域 22/IO `+0x1A_1000`） | `netstack_srv.rs:31`、`user/srv/src/common.rs` |
| 客户端库 | `morion::net`（libnetv）：UDP/TCP socket + `tcp_listen/accept` + `getaddrinfo`；负载页**按域派生** | `user/libmorion/src/net.rs` |
| 设备底座 | `libdevice`：`DeviceGrant::load()/is_valid()/page()`（`DEVICE_CFG_VADDR = 0x80_0081_0000`）+ `mmio`/`msix`/`virtio` | `user/libdevice/src/` |

**硬约束（实测 `qemu-system-x86_64 -device help`，2026-10）**
- 可仿真**有线**网卡：`rtl8139`、`e1000`(82540EM/82544GC/82545EM)、`e1000e`、`vmxnet3`、`ne2k_pci`/`ne2k_isa`、`pcnet`、`usb-net`、`virtio-net-*`。
- **无任何 802.11 无线设备**（无 wifi / 802.11 / wireless）。→ **无线驱动无法在本机回归链里验证**。
- 可仿真非网络设备（节选）：`virtio-gpu`、`virtio-keyboard`、`virtio-mouse`、`virtio-serial`、`virtio-rng`、`virtio-9p`、`virtio-scsi`、`virtio-balloon`。
- slirp（`-netdev user`）支持 IPv6 → IPv6 线**可完整回归**。**R1 已取证**：slirp 应答 RS 并回 RA，**前缀 `fec0::/64`**（site-local，slirp 的选择）；链路本地由 MAC 派生（实测 `fe80::5054:ff:fe12:3456`）。尚未取证：DNSv6 地址、`ping6` 网关（留 V6.1/V6.2 补）。

**结论**：驱动线里「再加有线网卡 / 加设备驱动」可回归、成本可控；**无线不可回归**，本轮只备抽象。IPv6 是软硬件无关、价值最高、可完整回归的主攻方向。

---

## 2. 目标与非目标

**目标**
1. **v6 可用**：应用经 libnetv 用 IPv6 收发（ICMPv6 echo、UDPv6、TCPv6），有端到端自测与真实链路取证。
2. **双栈**：IPv4 与 IPv6 并存，socket 可指定 family，默认双栈（v4-mapped）。
3. **驱动可扩**：`netstack_srv` 的 NIC 接线**表驱动化**，加网卡驱动不再改协议栈常量；至少再落 **1–2 个可仿真网卡驱动**。
4. **设备面拓宽**：落 1–2 个非网络设备驱动（优先 `virtio-gpu` / `virtio-input`）。
5. **无线可接**：定义统一 **L2 链路接口**，无线以「station 关联后呈现为普通以太链路」接入（对上层透明）；具体驱动待真机。

**非目标（明确排除/顺延）**
- 无线具体驱动（iwlwifi / USB WiFi）的**实现**——待真机/直通条件（见 §3.4）。
- IPv6 完整 RFC（不做：IPsec、Mobile IPv6、IPv6 分片重组、DHCPv6 有状态、Privacy Extensions、路由协议）。只做：**地址（SLAAC/LL）+ NDP + ICMPv6 + UDP/TCP + DNS AAAA**。
- `ip_srv`/`tcp_srv`/`udp_srv` 分层拆分（仍保持合并式 `netstack_srv`）。
- TLS / HTTP 客户端全功能 / 拥塞控制进阶。

---

## 3. 驱动线

### 3.1 DRV-A — NIC 接线表驱动化（前置）

把 `netstack_srv` 的 `NIC_COUNT` 与 `Link[]` 从**硬编码常量**改为**引导期发现**：
- 方案 a：`BootInfo` 增加「网卡表」（域号 + IO 页 + MAC），netstack 启动时读入。
- 方案 b：内核给网卡域一条「自报」能力，netstack 逐个探测（`NET_OP_INFO` 取 MAC/型号）。
- **倾向 a**：与现有「声明式设备授权」一致；netstack 只需把 `Link` 从常量换成运行期数组。

marker：`netstack: links=1 (virtio-net)` 与 `links=2 (virtio-net+e1000e)` 由同一套代码产出（**回归应逐字不变**）。

### 3.2 DRV-B — 再加可仿真网卡驱动（低风险、高可见度）

复用现有帧级契约（**不改 `netstack_srv`**，只需 DRV-A 把新驱动挂上链路表）：

| 驱动 | QEMU 设备 | 寄存器模型 | 难度 |
|---|---|---|---|
| `rtl8139_srv` | `-device rtl8139` | PIO + 寄存器（Cfg9346/命令/收发环） | ★ 最经典、最好写 |
| `e1000_srv` | `-device e1000` | MMIO + 传统描述符环（82540EM，**与 e1000e 同厂商不同代际**） | ★★ |
| `vmxnet3_srv`（可选） | `-device vmxnet3` | MMIO + 多队列/offload 能力协商 | ★★★ |

marker：`NET14 rtl8139 OK` / `NET15 e1000(82540EM) OK`，各配 `-netdev user` 出口做端到端（DHCP + ping + `wget`）。

### 3.3 DRV-C — 非网络设备驱动（拓宽设备面）

| 驱动 | QEMU 设备 | 价值 |
|---|---|---|
| `virtio_gpu_srv` | `-device virtio-gpu` | 现 `gfx_srv` 是**软件 framebuffer**；升级到 virtio-gpu 的 2D 资源/命令流，为「真正的显示驱动」铺路 |
| `virtio_input` | `-device virtio-keyboard` / `virtio-mouse` | 与现有 PS/2 `kbd.rs` 并列，验证「多输入源」抽象 |
| `virtio_rng_srv` | `-device virtio-rng` | 最小设备驱动（D1 授权路径的第二个样板），做熵源/`/dev/random` 底座 |

### 3.4 无线（WIRELESS）— 只备抽象，驱动待真机

**约束**：QEMU 无 802.11 设备；真机需 **vfio 直通**或**裸机**；驱动体量 ≈ 一个子系统（扫描/关联/802.11 管理帧 + WPA2/3 四步握手 + 固件 `.ucode` 加载）。

**本轮只做抽象（可回归）**：
1. 在驱动层与协议栈之间固化 **L2 链路接口**（就是现有 `NET_REQ_TX/RX` 的语义化命名）：上层只见「一条以太链路」，**不知道是哪种 L2**。
2. 定义「无线 station → 关联成功后暴露为普通以太链路」的模型（`wifi_srv` 对 netstack 呈现为一条 `Link`，与 `net_srv` 同契约）。
3. 预留 **802.11 扫描/关联的控制面** IPC（`WIFI_OP_SCAN/ASSOC/STATUS`），本轮只定义**空实现 + 契约**。

**明确标注：无线驱动本体不可本机回归**，待真机条件（选定芯片，倾向 Intel iwlwifi AX200/AX210）再单独立项。

---

## 4. IPv6 线（可回归，主攻）

### V6.1 — 地址与邻居
- **地址**：IPv6 地址表（每链路 `{ll, global}`）；**link-local `fe80::/64`（EUI-64 由 MAC 派生）** 必做；**SLAAC**（收 RA → 取前缀 + 接口 id）必做（R1 实测前缀 `fec0::/64`）。
- **NDP**：NS / NA / RS / RA 替代 ARP（邻居缓存复用现有老化逻辑）；DAD（重复地址检测，最小）。
- **抽象**：引入 `IpAddr`(v4/v6) 落到 conn / socket，替换现有的裸 `u64` IPv4 表示（内部可延后，先做对外）。
- marker：`NET16 ipv6 slaac OK (ll=fe80::…, g=fec0::…)` + `NET16 ndp self-test OK (ns->na)`。**已过**。
- 实现：`Link` 增 `v6_global/v6_gw/gw6_mac/v6_up`；`icmpv6_input` 处理 **RA→SLAAC**（前缀 + EUI-64 组全局地址）/ **NS→NA** / **NA→记路由器**；RA 的 SLLAO 选项直取路由器 MAC（免一次 NS/NA）。踩坑：ICMPv6 构造器算校验和前**必须先清零校验和字段**（否则残留上一帧字节，真机拒收）。

### V6.2 — 传输
- **ICMPv6**：echo request/reply + 最小错误（端口不可达等）。
- **UDPv6**：复用现有 UDP，仅换寻址 + 伪首部（v6 伪首部长度/格式不同）。
- **TCPv6**：**复用现有 TCP 状态机**，只换地址比较、伪首部校验和、MSS/默认值。
- marker：`NET17 icmpv6 echo OK` + `NET17 udp6 OK` / `NET18 tcp6 OK`。**已过**。
- 实现：新增 IPv6 分用 `ipv6_input`（ICMPv6→处理器 / UDP→投递+错误 / TCP→状态机）；
  `build_echo6`/`echo6_reply`（echo 请求就地在 `icmpv6_input` 内改写成应答）；
  `build_udp6`（**v6 下 UDP 校验和必需**，不可置 0）+ `udp6_input`（校验→`deliver`，无人接收回
  `icmpv6_port_unreach`，调用包先挪到页内 scratch 再原地构造）；`tcp_build6`/`tcp_parse6`
  与 v4 共用 `tcp_handle`/`tcp_tick`——`TcpConn` 增 `family/remote6/local6`，`tcp_seg` 按
  `family` 分派，`tcp_input` 收 `SrcId`(V4/V6) 做地址匹配。三项均有**确定性自证**
  （`echo6_selftest`/`udp6_selftest`/`tcp6_selftest`），另加真实链路 ping6（slirp 回
  `NET17 ipv6 echo OK (router replied)`）。

### V6.3 — 应用面
- libnetv：`IpAddr` + `getaddrinfo` 支持 **AAAA**（与现有 A 并列）；socket API 增加 family 参数（或新 `*6` 变体）。
- 能力：`Capability::Net(lo,hi)` **与 v4/v6 正交**（端口门禁不变）；`SYS_NET_BIND/OWNER` 语义不变。
- app 自测：`NET19 dns AAAA OK`；shell 可选 `ping6`。
- marker：`app: NET19 dns AAAA OK`。**已过**。
- 实现：`libnetv` 增 `IpAddr{V4,V6}`；`dns_build` 参数化 QTYPE；新增 `dns_parse_aaaa`（QTYPE 28 /
  RDATA 16）+ 确定性自证 `dns_selftest_aaaa`（`2001:db8::1`）；`getaddrinfo6`（真实 AAAA 查询）+
  双栈 `resolve`（先 A 后 AAAA）；app 侧 `net19_dns_aaaa`（确定性 + 真实 best-effort）。
  端口门禁不变（DNS 仍走 v4 UDP，源端口 `DNS_LOCAL_PORT` 落在 app 的 `Net` 能力内）。
  **注**：socket 的 family 形参 / v6 收发（`sendto6` 等）随 V6.4 双栈落地，届时再接 `udp6_input`。

### V6.4 — 双栈
- socket 默认双栈（v4-mapped `::ffff:a.b.c.d`）；`socket_on(nic)` + family 可指定出口与协议族。

---

## 5. 关键决策

| # | 问题 | 候选 | 倾向 |
|---|---|---|---|
| C1 | v6 地址表示 | (a) 保持 u64 内部、只在外层转 v6；(b) 引入 `IpAddr` 枚举贯穿 | 先 (a) 快速落地，V6.4 再收敛到 (b) |
| C2 | 邻居发现 | (a) 只做 NDP（含 SLAAC）；(b) 额外支持静态 v6 | **(a)**：NDP 已含静态（手工填表） |
| C3 | NIC 表驱动方案 | (a) BootInfo 网卡表；(b) 内核自报能力探测 | **(a)**：与声明式授权一致 |
| C4 | 新网卡驱动优先级 | rtl8139 / e1000 / vmxnet3 | **rtl8139 先做**（最好写、覆盖最广的 QEMU 默认机） |
| C5 | 非网络设备优先 | virtio-gpu / virtio-input / virtio-rng | **virtio-input 先做**（最小、验证多源输入），gpu 次之 |
| C6 | 无线 | 抽象层 / 真机驱动 | **只备抽象**（本轮），驱动待真机 |

---

## 6. 执行顺序（每步独立可回归 + 逐条提交）

1. ✅ **R1 取证（已完成）**：发 RS → slirp 回 RA，**前缀 `fec0::/64`**；自派生链路本地 `fe80::5054:ff:fe12:3456`。证据 marker `NET16 ipv6 ll=… / ra rx, prefix=…`（已入 `scripts/fs-regress.sh` 判据）。
2. ✅ **V6.1 地址 + SLAAC + NDP（已完成）**：`NET16 ipv6 slaac OK (g=fec0:0:0:0:5054:ff:fe12:3456, gw mac learned)` + `NET16 ndp self-test OK (ns->na)`。
3. ✅ **V6.2 传输（已完成）**：ICMPv6 echo（`NET17 icmpv6 echo OK`，另 slirp 回 `NET17 ipv6 echo OK (router replied)`）+ UDPv6（`NET17 udp6 OK`）+ TCPv6（`NET18 tcp6 OK`）。
4. ✅ **V6.3 应用面（已完成）**：`IpAddr` + DNS AAAA（确定性解析器自证 + 双栈 `resolve`），`app: NET19 dns AAAA OK`。
5. **DRV-A** NIC 表驱动化（回归逐字不变）。
6. **DRV-B** `rtl8139_srv`（`NET14`）→ 视情况 `e1000_srv`（`NET15`）。
7. **DRV-C** `virtio_input` → `virtio_gpu_srv`。
8. **V6.4** 双栈收口。
9. 无线抽象层（WIRELESS 契约 + 空实现）。

> 每步：`cargo fmt --all && make fmt && make check && make clippy`（0 warning）→ `cargo test --lib -p morion-kernel` → `make OUT_DIR=build QEMU=/bin/true run-nvme` → `OUT_DIR=build bash scripts/fs-regress.sh`。

---

## 7. 验收口径

- **v4 不回归**：`NET1..NET13` 与既有 `app: NET7 app socket OK` 逐字不变（新增能力不得影响旧路径）。
- **v6 有取证**：每个 v6 marker 都要有「确定性自证 + 真实链路证据」两类（与 N6–N8 同口径）。
- **驱动**：新驱动各自有 `NETxx` marker + 端到端（DHCP/ping/`wget`）。
- **能力**：新路径同样过 `SYS_NET_OWNER` 端口门禁；`cap-audit` 无越权（`violations=0`）。
- **无线**：只有契约与空实现，**明确标注不可本机回归**。

---

## 8. 主要风险

| 风险 | 影响 | 缓解 |
|---|---|---|
| slirp 的 IPv6 前缀/可达性不确定 | V6.1 细节返工 | **R1 先取证**，按实测修正 |
| `IpAddr` 贯穿改动面大 | 触碰 v4 旧路径 → 回归 | 先 C1(a)（内部仍 u64），V6.4 再收敛 |
| TCPv6 复用状态机 | 伪首部/寻址改错 → 静默失败 | 以「确定性自测 + 真实链路」双重 marker 兜底 |
| 无线不可回归 | 易成"死代码" | 只做契约 + 空实现，不写未验证的驱动逻辑 |
| NIC 表驱动化动到启动路径 | 影响全部网卡 | 回归要求 v4 marker **逐字不变** |

---

## 9. 相关文档

- [plan-network.md](plan-network.md) — 上一批（N5–N9 + N8.2 拆分）规划与完成情况
- [dev-reference.md](dev-reference.md) — 网络 API 段 + 阶段 90–99
- [roadmap-driver.md](roadmap-driver.md) — 驱动路线（D0–D4 / 03c / 远期）
- [architecture.md](architecture.md) — §网络协议栈（用户态多服务架构）
- [dev-workflow.md](dev-workflow.md) — 加一个服务的 10 处接线
- [app-dev-guide.md](app-dev-guide.md) — 应用侧 API 与能力名
