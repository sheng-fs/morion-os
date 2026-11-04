<div align="center">

# Morion OS

[English](./README.md) | [简体中文](./README.zh-CN.md) | [日本語](./README.ja.md) | [한국어](./README.ko.md) | [Español](./README.es.md) | [Français](./README.fr.md) | [Deutsch](./README.de.md) | [Русский](./README.ru.md)

---

[![License](https://img.shields.io/badge/license-MIT%20%2B%20Apache--2.0-blue.svg)](./docs/licensing.md)
[![Language](https://img.shields.io/badge/language-Rust-orange.svg)](https://www.rust-lang.org)
[![Arch](https://img.shields.io/badge/arch-x86__64%20|%20AArch64%20|%20RISC--V-brightgreen.svg)]()
[![Stage](https://img.shields.io/badge/stage-design%20&%20rewrite-yellow.svg)]()
[![Platform](https://img.shields.io/badge/platform-UEFI-lightgrey.svg)]()
[![Security](https://img.shields.io/badge/security-CHERI%20|%20IOMMU-red.svg)]()
[![PRs](https://img.shields.io/badge/PRs-welcome-brightgreen.svg)]()

</div>

---

## 개요

Morion OS는 **Rust**로 처음부터 새로 만드는 현대적인 운영체제입니다. 이 프로젝트는 현재 **재설계 및 재작성 단계**에 있으며, 초기 구현에서 드러난 아키텍처 충돌을 철저히 재검토한 뒤 **마이크로커널 + 엑소커널 하이브리드 아키텍처**를 핵심 기술 방향으로 다시 확립했습니다.

기존 코드는 아카이브되었고(`legacy` 브랜치) 메인라인은 재시작되었습니다. 이후 순수 설계 단계를 벗어나 **실행 가능한 마이크로커널 프로토타입**으로 발전했습니다. QEMU(UEFI) 환경에서 부팅하여 사용자 공간 셸로 진입하며, "애플리케이션 → libvfs → 파일시스템 서비스 → 블록 디바이스 서비스 → NVMe 디스크"로 이어지는 전체 파일 읽기/쓰기 경로를 구동합니다.

### 핵심 원칙

- **마이크로커널 신뢰 컴퓨팅 기반(TCB)**: 커널이 노출하는 경계는 시스템 콜 인터페이스 하나뿐입니다. 파일시스템, 네트워킹, 디바이스 드라이버, 그래픽, 셸을 비롯한 모든 전통적 커널 기능은 사용자 공간 서비스로 구현됩니다. 커널에는 소수의 핵심 프리미티브(IPC, 주소 공간 매핑, 보호 도메인, capability)만 남아 있으며, 전체 0–63 시스템 콜 테이블 안에서도 이들은 일부에 불과합니다([docs/dev-reference.md](./docs/dev-reference.md) §5 참고).
- **엑소커널 성능 경로**: "Performance Enclave" 메커니즘을 통해 IOMMU / CHERI 하드웨어 capability로 고성능 애플리케이션(게임, AI)이 하드웨어를 직접 조작할 수 있습니다 — 커널 트랩 없음, 데이터 복사 없음.
- **Capability 기반 보안 모델**: 전통적인 UID/GID 권한 체계를 폐기합니다. capability가 유일한 접근 자격 증명이 되어, "root는 무엇이든 할 수 있다"는 취약점을 근본적으로 제거합니다.
- **Anykernel 이중 모드 드라이버**: 동일한 드라이버 소스가 사용자 공간 서비스 프로세스(공유 시나리오) 또는 직접 패스 라이브러리(고성능 시나리오)로 컴파일되며, 코드의 90% 이상을 공유합니다.

> 상세 아키텍처 설계: [docs/architecture.md](./docs/architecture.md).

---

## 현재 상태

시스템은 설계 문서 단계를 지나 **실제 하드웨어 / QEMU에서 부팅·실행되는 마이크로커널 프로토타입** 단계에 있습니다. 아래 표는 "동작 중"인 것과 "미착수"인 것을 구분하여, 설계 목표가 이미 존재하는 기능으로 오해되지 않도록 합니다.

| 영역 | 상태 | 비고 |
|------|--------|-------|
| 마이크로커널 코어 | ✅ 동작 | 보호 도메인, 동기/비동기 IPC, 선점형 스케줄링(**타임아웃 상한이 있는 블로킹** 포함), 주소 공간과 요구 페이징, 인터럽트 라우팅(PIC + **LAPIC/MSI-X**, **블로킹 인터럽트 대기**와 **다중 벡터 `wait_any`** 포함), capability 시스템(**IPC를 통한 capability 전달** 포함) |
| 실제 하드웨어 부팅 / 전원 | ✅ 동작 | UEFI 로더가 부팅 볼륨의 Device Path에서 부팅 디스크를 찾습니다(`BootInfo` v5). **`SAFE` 변형은 부팅 디스크 체인의 블록 컨트롤러만 노출**하며(USB 컨트롤러는 절대 필터링하지 않음), 셸 `poweroff`는 ACPI S5(FADT + DSDT `_S5`)를 사용하고 `reboot`는 i8042 → `0xCF9` → triple fault 순으로 폴백합니다. PS/2 컨트롤러가 없으면 이를 명시적으로 보고합니다 |
| 시스템 콜 인터페이스 | ✅ 56개 (ID 0–63) | 커널의 전체 ID 테이블: [docs/dev-reference.md](./docs/dev-reference.md) §5; 애플리케이션 공개 서브셋: [docs/app-dev-guide.md](./docs/app-dev-guide.md) §3 |
| Capability 보안 모델 | ✅ 동작 | capability 슬롯 + **capability 핸들**(open 시 발급, 모든 I/O 전에 검사, close 시 회수), 기본적으로 capability 0개; 런타임에 IPC를 통한 **핸들 전달**(이동)과 **capability 위임**(복사, 권한 확대 없음) — 모든 것이 부팅 시 정적으로 부여될 필요는 없습니다 |
| 사용자 공간 드라이버 | ✅ 부분 | 키보드 드라이버(IRQ1 / PS/2 존재 탐지 + USB HID 부트 키보드); 블록 디바이스 서비스(NVMe 드라이버, IDE PIO 폴백 포함); **멀티 컨트롤러 xHCI**(최대 4개 컨트롤러, USB 스토리지 + USB HID 키보드); 네트워크 드라이버 **`net_srv`(virtio-net, 도메인 16)** — N2/N3 드라이버가 동작합니다(모던 virtio 초기화: virtio caps 파싱 / MAC 읽기 / RX·TX virtqueue 구성 / `DRIVER_OK` / **MSI-X 인터럽트 기반 RX**, 그리고 자체 발신 ARP 요청에 게이트웨이가 응답하여 `NET1 … ARP reply OK`). **일반 디바이스 부여(D1)**: 커널의 `device.rs`가 `DeviceGrant`(BAR + 연속 DMA 블록 + MSI-X 파라미터, 디바이스 의미 없음)를 제공하며, 커널에는 **더 이상 NVMe 전용 코드가 없습니다** — 큐 레이아웃과 디바이스 프로토콜은 드라이버 도메인에 있습니다; 드라이버는 `SYS_DEVICE_CONFIG_READ`를 통해 자기 디바이스의 PCI capability를 직접 파싱할 수 있고, 커널은 `BIR`에 따라 서로 다른 BAR에서 MSI-X 테이블을 매핑합니다 |
| 사용자 공간 파일시스템 | ✅ 부분 | FAT32(VFAT 긴 이름 포함), tmpfs, 독자 MorionFS v2(COW + 스냅샷 + 프리 비트맵/공간 회수 + 대용량 파일용 간접 블록 + 가변 길이 dirent/긴 이름 + 노드 메타데이터 + inode 번호 간접 계층/하드 링크/심볼릭 링크 + **볼륨 지오메트리별 포맷** + **명시적 `mkfs.mfs`, 다중 볼륨 및 기본 볼륨 전환**), ext2 **읽기 전용**, exFAT(읽기 + 쓰기, 대용량 및 높은 클러스터 수 볼륨) |
| 파티션 / 볼륨 계층 | ✅ 동작 | block_srv는 각 디스크의 **MBR/GPT** 파티션 테이블을 볼륨 테이블로 파싱하고, 볼륨별 선두 시그니처로 FS 타입을 탐지합니다; 또한 **파티션 테이블을 쓸 수 있습니다**(`part.create/del/wipe/reload`: 파티션 생성/삭제, wipe, 재스캔 — GPT와 MBR 모두); `dev`는 이제 "볼륨 번호"이며 블록 계층은 다중 페이지 DMA를 지원합니다(단일 명령 ≤ 128 KiB); **다중 볼륨 마운트**: 같은 타입의 추가 볼륨은 `/usb<vol>`에 자동 마운트되므로 하나의 코드베이스로 여러 디스크를 서비스할 수 있습니다 — 실제 USB 파티션 읽기를 위한 기반입니다 |
| 셸 및 통합 디렉터리 트리 | ✅ 동작 | `help/echo/uname/version/pwd/ls/cat/run/wget/net/ping/ping6/dns/wifi/lspci/dmesg/cd/mkdir/touch/rm/mv/ln/ln -s/chmod/chown/truncate/stat/lstat/readlink/mkfs.mfs/mfs.primary/mfs.snap/mfs.snaps/mfs.rollback/mfs.gc/mfs.fsck/mfs.sync/df/part.create/part.del/part.wipe/part.reload/clear/poweroff/reboot` (`ls -l` 롱 포맷, 심볼릭 링크는 `l`로 표시; `dmesg`는 `-d/-a/-g/path`와 `Ctrl+C` 지원; `uname`/`version`은 `SYS_UNAME`으로 버전 문자열을 보고); 여러 파일시스템이 마운트 계층에 의해 단일 `/` 루트로 조립되며 런타임 마운트를 지원합니다 |
| 그래픽 / GUI | 🚧 진행 중 | **G1** 프레임버퍼를 사용자 공간 `gfx_srv`(도메인 15)에 전용으로 넘김: `Capability::Fb` + `SYS_FB_INFO/MAP/TAKEOVER` 추가; 인수 후 커널 터미널은 더 이상 화면에 쓰지 않습니다(출력은 COM1에 유지). **G2** 드로잉 프리미티브 `fill/rect/blit` + 공유 서피스 + 클라이언트 라이브러리 `libmorion::gfx`; `blit`는 응답 전에 프레임버퍼를 다시 읽어 검증합니다(셀프테스트 GS-1). **G3a** 텍스트 렌더링 이전: 폰트(ASCII 8×16 + CJK 16×16 `cjk.bin` ≈276 KB)와 터미널 상태가 커널에서 `gfx/`로 이동, 새 프로토콜 `GFX_OP_TEXT/CLEAR/MOVE/QUERY`, 픽셀 단위 쓰기 후 되읽기 검증(셀프테스트 GT-1). **G3b** 셸 출력 화면 표시: `SYS_CONSOLE_READY(47)`; `libmorion`의 출력 싱크 `sink()`가 프로세스별로 화면 콘솔에 출력을 **미러링**할 수 있습니다(셸만 옵트인). **G3c** 커널이 CJK 폰트를 제거(−276 KB, 커널 ELF 347 KB → 69 KB); 커널 터미널은 ASCII + tofu로 폴백하고 CJK 렌더링은 사용자 공간에서만 수행됩니다. **G4** 입력을 커널 밖으로 이동: `SYS_KEY_PUSH(48)/SYS_KEY_READ(49)` 키 바이트 큐(커널은 바이트만 전달), 행 편집/에코는 클라이언트 라이브러리 `morion::console::readline`에서 처리. **G6** 서비스 자가 복구: `ipc::call`이 더 이상 영원히 멈추지 않고(타임아웃 + 대상에 살아 있는 태스크가 없을 때 즉시 실패), 프레임버퍼를 커널 예약 영역으로 등록, 재시작 시 오래된 mailbox 요청 폐기, 클라이언트가 공유 세션을 재구축, `gfx_srv`는 init이 감독합니다(셀프테스트 GS-2). 커널 텍스트 콘솔은 부팅 및 panic 출력 전용으로만 남습니다 |
| 네트워킹 / 가상화 / enclave / 패키지 관리자 | 🚧 진행 중 | **네트워킹**: `net_srv` virtio-net 드라이버가 동작합니다; 스택은 전용 `netstack_srv`(도메인 21: TCP/IP + 소켓 서비스 + 포트 capability 게이트)로 분리되었고, 두 번째/세 번째 실제 NIC으로 `e1000e_srv`(22) / `e1000_srv`(24), 무선 `wifi_srv`(25), 게스트 내 `httpd_srv`(23)가 있습니다. **Enclave**: IOMMU/VT-d 하드웨어 격리는 완료되었습니다(E1c부터 대상 디바이스 윈도우가 제한되며 윈도우 밖 DMA는 증거와 함께 거부됨); enclave 관리자 E2/E3는 미완료입니다. **가상화 및 패키지 관리**: 설계는 확정되었으나 구현은 아직 없습니다 |
| 시스템-AI capability 인터페이스 | 📐 명세 정의됨 | 애플리케이션이 시스템 AI에 기능을 노출하는 방식: [docs/app-dev-guide.md](./docs/app-dev-guide.md) §9 |

> 빠른 시작(빌드 & 실행 명령): [docs/commands.md](./docs/commands.md);
> 커널 및 인터페이스 레퍼런스: [docs/dev-reference.md](./docs/dev-reference.md);
> 애플리케이션 개발(AI 호출 가능 capability 포함): [docs/app-dev-guide.md](./docs/app-dev-guide.md);
> 파일시스템 로드맵: [docs/roadmap-fs.md](./docs/roadmap-fs.md);
> 드라이버 & enclave 로드맵: [docs/roadmap-driver.md](./docs/roadmap-driver.md).

---

## 버전

현재 버전은 **`0.4.0`**이며, 배포된 그래픽 없는 변형은 **`v0.4.0-nogui`**로 태그되어 있습니다.

- 버전 상수의 **단일 진실 공급원(single source of truth)**은 커널의 `kernel/src/version.rs`(`SYSTEM_NAME` / `VERSION` / `MACHINE` / `VARIANT` / `BUILD`)이며, `SYS_UNAME(51)`을 통해 사용자 공간에 보고됩니다 — 셸의 `uname` / `version` 명령이 이를 읽습니다(빌드 번호는 `Makefile`이 주입하는 git short hash입니다).
- 두 가지 빌드 변형:
  - **일반(그래픽 포함)**: `make ...`; 셸 출력이 `gfx_srv`의 화면 콘솔로 추가 **미러링**됩니다.
  - **그래픽 없음**: `make NOGUI=1 ...`; 셸이 화면 미러를 열지 않으며(입력/에코는 시리얼 포트를 통해 처리), 릴리스 문자열이 `0.4.0-nogui`로 보고되고 산출물은 `build/nogui/`에 생성됩니다. 그 외 모든 서비스와 전체 회귀 기준선은 동일합니다.
- 변경 이력은 [CHANGELOG.md](./CHANGELOG.md)를 참고하세요.

---

## 아키텍처 개요(목표)

```
┌──────────────────────────────────────────────────────┐
│                  Application Layer                    │
│    POSIX Interface (libc)  |  High-Perf Direct API    │
├──────────────────────────────────────────────────────┤
│            User-Space System Services                 │
│  Filesystem │ Network Stack │ Device Svc │ Security   │
│  ext4/vfat  │    TCP/IP     │ Driver Svc │ Auth/Audit │
├──────────────────────────────────────────────────────┤
│       Performance Enclave — Optional Acceleration     │
│   GPU Direct  │  NPU Direct  │  Userspace NIC Driver  │
│   (IOMMU-enforced isolation, CHERI bounds protection) │
├──────────────────────────────────────────────────────┤
│                    Microkernel                        │
│  IPC │ Scheduling │ Address Space │ IRQ Route │ Caps  │
└──────────────────────────────────────────────────────┘
```

---

## 핵심 설계

### 마이크로커널 프리미티브

더 이상 줄일 수 없는 최소 집합만 포함합니다:

| 프리미티브 | 설명 |
|-----------|-------------|
| `send` / `receive` / `call` | capability 전달을 지원하는 동기/비동기 IPC |
| `map` / `unmap` | 주소 공간 매핑 관리 |
| `create_domain` / `destroy_domain` | 보호 도메인(프로세스) 생명주기 |
| `schedule` | CPU 스케줄링 |
| `allocate_frame` / `free_frame` | 물리 메모리 프레임 관리 |
| `register_interrupt` / `ack_interrupt` | 인터럽트 승인 및 확인 |
| `create_enclave` | 하드웨어 격리 enclave 생성 |

### 외부 페이저(External Pager)

- 커널은 페이지 폴트를 잡아 전달할 뿐이며, 내용과 교체 정책은 사용자 공간 페이저 서비스가 결정합니다
- 각 프로세스는 전용 페이저를 지정할 수 있습니다 — 요구 페이징, 압축 메모리 풀, 네트워크 스토리지 등

### Anykernel 이중 모드 드라이버

| 모드 | 대상 | 특성 |
|------|--------|-----------------|
| **드라이버 서비스 프로세스** | 일반 애플리케이션 | 디바이스 공유, 안전한 격리, IPC를 통한 간접 접근 |
| **직접 드라이버 라이브러리(LibDevice)** | 게임 / AI | 런타임 링크, MMIO/DMA 작업에 대한 커널 트랩 없음 |

단일 Rust trait 인터페이스, 컴파일 타임 feature 플래그로 백엔드 선택, 90% 이상의 코드 공유.

### Performance Enclave

1. IOMMU가 디바이스 MMIO 및 DMA 윈도우를 프로세스 주소 공간에 매핑합니다
2. LibDevice 직접 드라이버 라이브러리가 링크됩니다
3. GPU/NPU 명령이 커널 개입 없이 직접 제출됩니다

폭발 반경(blast radius)은 enclave의 리소스 경계에 하드웨어적으로 고정됩니다.

### Capability 기반 보안 모델

- capability가 유일한 접근 자격 증명이며, UID/GID에 의존하지 않습니다
- 새 프로세스는 capability 0개로 시작하며, 부모가 명시적으로 부여합니다
- POSIX 권한 API(`chmod`/`chown`)는 libc가 capability 연산으로 변환합니다
- 보안 정책은 사용자 공간 정책 엔진이 해석하며, 동적 업데이트를 지원합니다

### 사용자 공간 서비스

모든 전통적 커널 기능은 독립적인 사용자 공간 프로세스로 실행됩니다:

| 서비스 | 역할 | 상태 |
|---------|---------------|--------|
| 파일시스템 서비스 | FAT32(VFAT 긴 이름 포함), tmpfs, 독자 MorionFS, ext2 읽기 전용, exFAT(읽기 + 쓰기), libvfs를 통해 통합 | ✅ 구현됨(ext2 읽기 전용 / exFAT 읽기-쓰기) |
| 디바이스 서비스 | 블록 디바이스(NVMe 드라이버), 키보드 드라이버, 인터럽트 디스패치 | ✅ 부분 |
| 셸 서비스 | 명령줄 인터프리터 + 통합 디렉터리 트리 / 런타임 마운트 | ✅ 구현됨 |
| 네트워크 스택 | 사용자 공간 TCP/IP, 제로 카피 공유 메모리 | ⏳ 계획됨 |
| 보안 / 감사 | 인증, 정책 엔진, 침입 탐지 | ⏳ 계획됨 |
| Enclave 관리자 | enclave 생명주기, 로그 스트림, 마이그레이션 및 일시 중단 | ⏳ 계획됨 |
| 패키지 관리자 | Nix 스타일 선언적 빌드, 원자적 전환, 버전 롤백 | ⏳ 계획됨 |
| GUI 서비스 | 아크릴 반투명 데스크톱, 높은 커스터마이즈 가능성(`gfx_srv` 프레임버퍼 / 텍스트 콘솔 / 드로잉 프리미티브 구현됨, 데스크톱 환경은 미착수) | 🚧 진행 중 |
| 오디오 / IME / 컨테이너 / 시간 / 전원 / 로그 / 설정 | 시스템 인프라 서비스 | ⏳ 계획됨 |
| AI capability 등록 / 게이트웨이 서비스 | 애플리케이션 capability 등록 및 검색, AI 호출 인가와 감사 | 📐 명세 정의됨([app-dev-guide.md](./docs/app-dev-guide.md) §9 참고) |

### 가상화

- 마이크로커널이 Hypervisor 역할도 수행합니다(Intel VT-x / AMD-V)
- unikernel 및 수정하지 않은 Linux/Windows 게스트를 지원합니다
- PCIe 디바이스 패스스루(VT-d / IOMMU), 중첩 enclave

### 부팅

- UEFI 네이티브, 레거시 리얼 모드 전환 없음
- GOP 고해상도 부팅 메뉴, 아크릴 테마
- Nix closure 기반 부팅 항목, 원자적 전환 및 롤백
- TPM 2.0 측정 + Secure Boot 검증
- kexec 웜 부팅, 다중 OS 공존

---

## 저장소 구조

### 현재 구조

이 프로젝트는 Rust 워크스페이스(루트 `Cargo.toml`)이며, 현재 `boot`, `kernel`, `user/srv`, `user/libmorion`, `user/libdevice`, `user/hello`, `kernel_test` 크레이트를 포함합니다. 커널 밖의 모든 시스템 서비스(블록 디바이스 / 파일시스템 / 마운트 / 셸 / 키보드 드라이버, …)는 **별도의 사용자 공간 프로그램**입니다(`user/srv`의 서비스당 하나의 `[[bin]]` → 하나의 독립 ELF). **부트로더는 부팅 시 ESP(`EFI/morion/services/`)에서 이들을 읽어** `BootInfo` 모듈 테이블로 커널에 전달하고, 이후 커널이 각각을 고정된 도메인에 로드합니다(E2b: 더 이상 도메인 id로 디스패치되는 단일 플랫 바이너리가 아님; E3b: 서비스가 더 이상 커널 이미지에 내장되지 않음). UI 자산은 용도별로 정리되어 있습니다: 부팅 시 리소스는 `boot/loader/resources/`, 시스템 전역 리소스는 `resources/system/` 아래에 있습니다.

```
.
├── .github/
│   ├── workflows/            # GitHub Actions (Gitee 자동 동기화)
│   ├── ISSUE_TEMPLATE/       # 이슈 템플릿
│   └── PULL_REQUEST_TEMPLATE.md
├── boot/                     # UEFI 부트로더 (morion-boot)
│   ├── asm/
│   │   └── boot_stub.asm     #   부팅 진입 어셈블리 스텁
│   ├── loader/               # 부팅 시 리소스 및 설정
│   │   ├── entries/          #   부팅 항목 (.conf)
│   │   │   └── morion.conf
│   │   ├── resources/        #   부트로더 테마 자산 (BMP/PNG)
│   │   │   ├── animation/    #     로딩 애니메이션 프레임
│   │   │   ├── background/   #     배경 (dark/light/default/mask)
│   │   │   ├── icons/        #     카테고리 아이콘 (dialog/power/security/system/ui)
│   │   │   ├── logo/         #     로고 변형 (horizontal/monochrome/square/system)
│   │   │   ├── progress/     #     진행 표시줄 (bar_bg/bar_fill)
│   │   │   └── splash/       #     스플래시 화면 (background/logo)
│   │   ├── kernel_placeholder.bin
│   │   ├── loader.conf       #   부트로더 설정
│   │   └── theme.toml        #   아크릴 테마 설정
│   └── src/
│       ├── boot/             #   부팅 흐름 (loader/menu/kexec)
│       ├── config/           #   설정 파싱 (entries/theme)
│       ├── gfx/              #   그래픽 렌더링 (framebuffer/font/renderer/animation)
│       ├── security/         #   보안 (hash/secure_boot/tpm)
│       ├── lib.rs
│       └── main.rs
├── kernel/                   # 마이크로커널 (morion-kernel, 최소 TCB)
│   └── src/
│       ├── arch/             #   x86_64 (gdt/idt/pic/pit/keyboard/pci)
│       ├── memory/           #   메모리 관리 (paging/frame_allocator)
│       ├── scheduler/        #   스케줄러 (컨텍스트 스위칭)
│       ├── video/            #   커널 텍스트 콘솔 (framebuffer/font/logo/bg/unicode) — 부팅 및 panic 출력
│       ├── bootinfo.rs       #   부팅 정보 (메모리 맵 + GOP 프레임버퍼)
│       ├── cap.rs            #   capability 시스템 (슬롯 + 핸들 테이블)
│       ├── domain.rs         #   보호 도메인 (프로세스, 도메인별 페이지 테이블)
│       ├── elf.rs            #   ELF64 파싱 및 검증 (실행 파일 로딩의 신뢰 경계)
│       ├── exec.rs           #   런타임에 ELF 로드 → 도메인 생성 → 매핑 → 태스크 시작
│       ├── ipc.rs            #   프로세스 간 통신
│       ├── irq.rs            #   인터럽트 라우팅 (인터럽트를 IPC로)
│       ├── device.rs         #   일반 디바이스 부여 (BAR / DMA / MSI-X → DeviceGrant)
│       ├── pager.rs          #   사용자 공간 페이저 인터페이스
│       ├── syscall.rs        #   시스템 콜 진입점 및 ID 테이블
│       ├── lib.rs
│       └── main.rs
├── user/                     # 사용자 공간: 런타임 + 드라이버 공용 라이브러리 + 서비스
│   ├── libmorion/            #   런타임 (크레이트 `morion`): syscall / print / libvfs / libgfx / 진입점 보일러플레이트
│   ├── libdevice/            #   드라이버 공용 라이브러리 (크레이트 `libdevice`, D2/D2b): device grant / MMIO / MSI-X / virtio 전송 + vring
│   ├── hello/                #   데모: **독립 ELF 프로그램** (SYS_SPAWN_ELF로 런타임 로드)
│   └── srv/                  #   시스템 서비스 (크레이트 `morion-srv`): 서비스당 하나의 [[bin]] → 하나의 독립 ELF
│       └── src/
│           ├── common.rs     #     서비스들이 공유하는 와이어 프로토콜 / 블록 클라이언트 / 이름 헬퍼
│           ├── block_srv.rs  #     도메인 5  블록 디바이스 드라이버 서비스
│           ├── fat32_srv.rs  #     도메인 6  FAT32 파일시스템 서비스
│           ├── app.rs        #     도메인 7  셀프테스트 프로그램
│           ├── shell.rs      #     도메인 8  명령줄
│           ├── mount_srv.rs  #     도메인 9  마운트 서비스
│           ├── tmpfs_srv.rs  #     도메인 10 인메모리 파일시스템
│           ├── mfs_srv.rs    #     도메인 11 MorionFS
│           ├── ext2_srv.rs   #     도메인 12 ext2 읽기 전용
│           ├── exfat_srv.rs  #     도메인 13 exFAT 읽기-쓰기
│           ├── init.rs       #     도메인 14 감독자 (서비스 도메인을 감시하고 메모리 이미지에서 제자리 재시작)
│           ├── gfx_srv.rs    #     도메인 15 그래픽 서비스 (프레임버퍼 보유, 사용자 공간 렌더링: 프리미티브 + 텍스트 터미널)
│           ├── net_srv.rs    #     도메인 16 네트워크 드라이버 (virtio-net; N0–N3: MSI-X 인터럽트 + ARP 셀프테스트)
│           ├── virtio_blk_srv.rs  # 도메인 17 virtio-blk 블록 드라이버 (D3: 일반 grant, 시그니처 읽기/쓰기-되읽기 셀프테스트)
│           ├── gfx/          #     그래픽 서비스 내부: 프레임버퍼 뷰 + 폰트 (font/glyphs/cjk.bin) + 터미널
│           ├── sender.rs / receiver.rs / pager.rs / echo.rs / kbd.rs  # 도메인 0..4 데모 및 키보드
│           └── bin/          #     18개 진입점 (각각 morion_main → 해당 모듈의 run()을 작성)
├── kernel_test/              # 초기 부팅 통합 테스트 커널 (임시 유지)
│   └── src/main.rs
├── resources/
│   └── system/               # 시스템 전역 리소스
│       ├── device/           #   디바이스 아이콘 (.ico)
│       ├── file/             #   파일 타입 아이콘 (.ico)
│       ├── github/           #   GitHub 커버 (.png)
│       ├── icons/            #   일반 UI 아이콘 (.ico)
│       ├── logo/             #   시스템 로고 (.ico/.svg/.png)
│       ├── service/          #   서비스 아이콘 (.ico)
│       └── terminal/         #   터미널 배경 (.raw)
├── docs/
│   ├── architecture.md       #   기술 아키텍처 설계
│   ├── app-dev-guide.md      #   애플리케이션 개발 가이드 (AI 호출 가능 capability 명세 포함)
│   ├── dev-reference.md      #   커널 및 인터페이스 퀵 레퍼런스
│   ├── commands.md           #   빌드 / 실행 / 검증 명령
│   ├── shell-reference.md    #   셸 사용 레퍼런스
│   └── roadmap-fs.md         #   파일시스템 로드맵
├── Cargo.toml                # Rust 워크스페이스 (boot/kernel/user/kernel_test)
├── Cargo.lock
├── Makefile                  # 빌드 시스템 (make iso/run/run-nvme/debug/check/clippy)
├── flake.nix                 # Nix 빌드 통합
├── rust-toolchain.toml       # Rust nightly 툴체인
├── linker.ld                 # 커널 링커 스크립트
├── .gitattributes
├── .gitignore
├── CONTRIBUTING.md
├── LICENSE
├── README.md                 # English (main)
├── README.zh-CN.md           # 简体中文
├── README.ja.md              # 日本語
├── README.ko.md              # 한국어
├── README.es.md              # Español
├── README.fr.md              # Français
├── README.de.md              # Deutsch
└── README.ru.md              # Русский
```

### 목표 개발 구조

```
├── bootloader/       # 부트로더 (UEFI)
├── kernel/           # 커널 소스
│   ├── arch/         #   아키텍처별 (x86_64 / AArch64 / RISC-V)
│   ├── core/         #   마이크로커널 코어 (IPC, 스케줄러, 주소 공간, caps)
│   └── compat/       #   호환 계층 (POSIX / Linux / RTOS)
├── services/         # 사용자 공간 시스템 서비스
│   ├── fs/           #   파일시스템 서비스
│   ├── net/          #   네트워크 스택
│   ├── device/       #   디바이스 서비스 + 드라이버
│   ├── security/     #   보안 / 인증 / 감사
│   ├── enclave/      #   Enclave 관리자
│   ├── gui/          #   GUI 서비스
│   ├── shell/        #   셸 서비스
│   ├── audio/        #   오디오 서비스
│   ├── ime/          #   IME 서비스
│   └── ...           #   추가 서비스
├── userland/         # 사용자 공간
│   ├── libs/         #   libc, libvfs, libdevice 등
│   └── bin/          #   기본 명령 (ls, cat, mkdir, rm)
├── resources/        # 리소스 파일
├── docs/             # 문서
└── pkg/              # 패키지 관리 (Nix 스타일)
```

---

## 로드맵

> 상세 파일시스템 로드맵: [docs/roadmap-fs.md](./docs/roadmap-fs.md); 그래픽 서브시스템 로드맵: [docs/roadmap-gfx.md](./docs/roadmap-gfx.md). 체크된 항목은 **QEMU에서 실행 검증됨**을 의미합니다.

### Phase 1 — 마이크로커널 코어 (기본 완료)

- [x] 보호 도메인(프로세스) 생성 및 주소 공간 격리
- [x] 동기/비동기 IPC (`send` / `recv` / `call` / `reply`; 96바이트 페이로드 + 대량 데이터용 공유 메모리)
- [x] 태스크 스케줄링 및 컨텍스트 스위칭
- [x] 주소 공간 map/unmap + 사용자 공간 페이저(요구 페이징)
- [x] 인터럽트 라우팅("인터럽트를 IPC로") + MMIO / I/O 포트 부여
- [x] capability 시스템(슬롯 + 핸들: 발급 / 검사 / 회수)
- [x] **IPC를 통한 capability 전달**(핸들 전달 `SYS_HANDLE_SEND`: 열린 객체를 대상 도메인 **안으로** 이동, 이동 시맨틱스; capability 위임 `SYS_CAP_SEND`: 보유한 capability를 **복사**, 권한 확대 없음; 둘 다 `SendTo(to)` 필요). 도메인 0/1/3이 부팅 시 8개의 긍정/부정 셀프테스트 케이스를 커버합니다

### Phase 2 — 기반 서비스 (진행 중)

- [x] PCI 열거 + 사용자 공간 NVMe 드라이버(블록 디바이스 서비스, IDE PIO 폴백 포함)
- [x] MBR/GPT **파티션 파싱 + 볼륨 계층**(block_srv 내부, `dev` = 볼륨 번호; FS 타입은 볼륨별 선두 시그니처로 탐지)
- [x] **다중 볼륨 마운트**: 요청 태그 상위 32비트가 볼륨 코드를 전달하고, 하나의 open이 하나의 볼륨을 바인딩합니다; 각 FS 서비스는 추가 볼륨을 자동 보고합니다(`/usb<vol>`에 마운트); fat32 클러스터 버퍼 2 → 16페이지(**64 KiB** 클러스터 상한), ext2 블록 그룹 한도 16 → 4096
- [x] 파일시스템: FAT32(VFAT 긴 이름 포함) / tmpfs / 독자 MorionFS(COW + 스냅샷)
- [x] ext2 **읽기 전용** 호환성(기존 Linux 파티션 마운트)
- [x] exFAT 호환성(`exfat_srv` 도메인 13, `/usb`에 마운트): **읽기 전용** + **읽기-쓰기**(`CREAT/WRITE/MKDIR/UNLINK/RMDIR/TRUNCATE`) + **대용량 볼륨**(다중 페이지 DMA, 단일 명령 ≤ 128 KiB)
- [x] libvfs + 마운트 계층: 여러 파일시스템을 단일 `/` 루트로 조립, 런타임 마운트 지원
- [x] 셸(빌트인 + cwd 상대 경로) 및 사용자 공간 키보드 드라이버
- [x] **MorionFS v2 포맷 확정 + 공간 회수 / GC**(프리 비트맵 + mark & sweep)
- [x] **MorionFS 대용량 파일**(`MFS3`: 간접 블록, 기존 ≈4 MiB 한도 극복)
- [x] **MorionFS 디렉터리 및 긴 이름**(`MFS4`: 가변 길이 dirent + 확장 블록, 이름 ≤255 바이트)
- [x] **MorionFS 노드 메타데이터**(`MFS5`: 타임스탬프 / 권한 / 소유자 / 링크 수, `rename` / `truncate` / `chmod`)
- [x] **MorionFS inode 번호 간접화 + 하드 링크**(`MFS6`)
- [x] **MorionFS 심볼릭 링크**(`MFSL`: 팔로잉 + 루프 보호를 포함한 경로 해석, `readlink` / `lstat` / `ln -s`)
- [x] **MorionFS 볼륨 지오메트리별 포맷**(**M7**)
- [x] **MorionFS 명시적 포맷 + 다중 볼륨**(**M8 / S2**: `mkfs.mfs <vol>`, 가드는 빈 볼륨 또는 MFS 볼륨만 허용)
- [x] **MorionFS 용량 확장(비트맵 외부화)**(**S3a**: 용량 상한 ≈119 MiB → **≈127.25 GiB**)
- [x] **MorionFS 4 GiB 초과 단일 파일**(**S3b**: VFS `offset`/`size` u64 엔드투엔드 + 3단계 간접 블록 `MFI3`)
- [x] **MorionFS 기본 볼륨 전환**(**S2**: 슈퍼블록이 기본 볼륨 시리얼을 저장; 가장 높은 시리얼이 우선)
- [x] **MorionFS 마커만으로 기본 볼륨 재지정**(**S2**: 데이터를 건드리지 않는 `mfs.primary <vol>`)
- [x] **`df` 공간 사용량**(MorionFS가 전체 / 사용 / 여유 보고)
- [x] **볼륨 관리 마무리: 파티션 테이블 쓰기**(**S2**: `part.create/del/wipe/reload`, GPT + MBR, 호스트 `sgdisk -v`로 검증)
- [x] **NVMe 인터럽트(MSI/MSI-X)**(**S3**: LAPIC 최소 지원 + PCI capability 순회 + MSI 벡터 구간 `0x50..0x5F`)
- [x] **블로킹 인터럽트 대기(대기 프리미티브)**(**S4**: 타임아웃 상한 블로킹 + `SYS_IRQ_WAIT`)
- [x] **다중 벡터 + `wait_any`**(**S5**: 도메인별 대기 키 + 벡터 마스크, admin + 2개의 I/O 큐가 각자 벡터를 가짐)
- [x] **터미널 CJK 렌더링(CJK / 전각 / 혼합 폭)**(커널 측은 이후 G3c에서 제거, 폰트는 사용자 공간 `gfx_srv`로 이동)
- [x] **실행 파일 로딩(ELF + 런타임 spawn)**(**E1**: ELF64 로더 + `SYS_SPAWN_ELF`; 데모 `user/hello`는 독립 ELF)
- [x] **사용자 공간 런타임 libmorion + `run` 명령**(**E2a**)
- [x] **서비스의 독립 프로그램 분리 + 도메인 파괴 / 프레임 회수**(**E2b**: 14개 서비스, 각각 하나의 ELF; `SYS_DOMAIN_DESTROY/COUNT/FRAME_FREE`)
- [x] **서비스 생명주기 마무리**(**E3a/E3b/E3c**: 사용자 페이지 W^X; 서비스가 커널 이미지에서 분리; `init` 감독자 + 메모리 이미지에서 제자리 재시작, 디스크 의존 없음)
- [x] **그래픽 G1(프레임버퍼를 사용자 공간으로 이전)**
- [x] **그래픽 G2(드로잉 프리미티브 + 공유 서피스 + `libmorion::gfx`)**
- [x] **그래픽 G3a(서비스 내 터미널 + 텍스트 렌더링 이전)**
- [x] **그래픽 G3b(셸 출력 화면 표시)**
- [x] **그래픽 G3c(커널이 CJK 폰트 제거, −276 KB)**
- [x] **그래픽 G4(입력을 커널 밖으로 이전)**
- [x] **그래픽 G6(gfx_srv 자가 복구: 감독 재시작 + 클라이언트 세션 재구축)**
- [ ] **더 많은 파일시스템 호환성**(ext4 쓰기, UDF 등)
- [ ] 네트워크 스택

### Phase 3 — 성능 enclave (진행 중)

- [x] **IOMMU 패스스루**(**E1a** ACPI DMAR 탐지 / **E1b** 루트 테이블 + 컨텍스트 테이블 + 아이덴티티 2단계 페이지 테이블 및 `GCMD.TE` / **E1c** 대상 디바이스 윈도우를 `[0, 3 GiB)`로 축소, 윈도우 밖 디바이스 DMA는 IOMMU가 증거와 함께 거부 — 모두 완료, [docs/roadmap-driver.md](./docs/roadmap-driver.md) 참고)
- [ ] **LibDevice 패스스루 형태 & enclave 관리자**(E2/E3 대기 중; 공용 라이브러리 `user/libdevice`는 이미 마련됨)

### Phase 4 — 네트워킹 & 보안 (진행 중)

- [x] **NIC 드라이버(virtio-net)**(`net_srv` 도메인 16 + 일반 디바이스 부여, [docs/roadmap-driver.md](./docs/roadmap-driver.md) N0–N3 참고: virtio-modern 초기화 + MSI-X 인터럽트 RX + ARP 엔드투엔드 셀프테스트)
- [x] **두 번째 실제 드라이버(virtio-blk)**(`virtio_blk_srv` 도메인 17, 여전히 일반 디바이스 부여를 통해 동작하며 커널에 디바이스 전용 로직 없음: D3 — 시그니처 읽기 / 쓰기-되읽기 셀프테스트)
- [x] **런타임 디바이스 부여(D1b)**(`SYS_DEVICE_INFO` / `SYS_DEVICE_GRANT` + `Mmio` capability 게이트; NVMe / `net_srv` / `virtio_blk_srv`가 런타임 시스템 콜로 `DeviceGrant`를 획득, 동작 변화 없음)
- [ ] TCP/IP 스택, capability 감사, 정책 엔진

### Phase 5 — GUI & 생태계 (미착수)

- [ ] 데스크톱 환경, 패키지 관리자, 가상화

### 횡단 관심사 — 시스템-AI capability 인터페이스 (명세 정의됨, 구현 미착수)

- [x] 설계 명세: 앱이 기능을 **AI 호출 가능 capability**로 주석/설명하는 방식([docs/app-dev-guide.md](./docs/app-dev-guide.md) §9 참고)
- [ ] capability 등록 & 검색 서비스
- [ ] AI 호출 게이트웨이(capability 검사 / 감사 로그 / 타임아웃 및 할당량)

---

## 라이선스

이 프로젝트는 **모듈별 라이선스**를 사용합니다:

- 코어(`boot/`, `kernel/`, `user/`): [MIT](./LICENSE)
- Enclave 모듈(`enclave/`): [Apache-2.0](./LICENSES/Apache-2.0.txt), 명시적 특허 허여 및 특허 소송 종료 조항 포함
- 서드파티 드라이버 / 펌웨어: 원래 라이선스를 유지하며, [docs/licensing.md](./docs/licensing.md) 참고

표준 문구: **Morion OS 코어는 MIT 라이선스이고, enclave 모듈은 Apache-2.0이며, 서드파티 드라이버와 펌웨어는 각자의 라이선스를 유지합니다.** 이 저장소는 단일 MIT 라이선스가 아닙니다.

---

## 연락처

- 프로젝트 홈페이지: [github.com/sheng-fs/morion-os](https://github.com/sheng-fs/morion-os)
- 이메일: 3555679134@qq.com
