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

## Überblick

Morion OS ist ein modernes Betriebssystem, das von Grund auf in **Rust** entwickelt wird. Das Projekt befindet sich derzeit in einer **Redesign- & Rewrite-Phase**: Architektonische Konflikte aus früheren Implementierungen wurden eingehend neu bewertet und die **Hybridarchitektur aus Microkernel und Exokernel** als technische Kernrichtung neu etabliert.

Der Altcode wurde archiviert (Branch `legacy`) und die Mainline neu gestartet. Seitdem ist das Projekt von reinem Design zu einem **lauffähigen Microkernel-Prototypen** geworden: Er bootet unter QEMU (UEFI) in eine User-Space-Shell und treibt den vollständigen Datei-Lese-/Schreibpfad an — „Anwendung → libvfs → Dateisystemdienst → Blockgerätedienst → NVMe-Disk“.

### Kernprinzipien

- **Microkernel Trusted Computing Base**: Der Kernel bietet genau eine Grenze — die Systemaufruf-Schnittstelle; Dateisysteme, Netzwerk, Gerätetreiber, Grafik, die Shell und jede andere traditionelle Kernelfunktion sind als User-Space-Dienste implementiert. Nur eine Handvoll Kernprimitive lebt im Kernel (IPC, Adressraum-Mapping, Schutzdomänen, Capabilities) — und selbst innerhalb der vollständigen Syscall-Tabelle 0–63 machen diese nur einen kleinen Teil aus (siehe [docs/dev-reference.md](./docs/dev-reference.md) §5).
- **Exokernel-Performance-Pfad**: Über den Mechanismus „Performance Enclave“ erlauben IOMMU-/CHERI-Hardware-Capabilities leistungshungrigen Anwendungen (Spiele, KI) den direkten Hardwarezugriff — ohne Kernel-Traps, ohne Datenkopien.
- **Capability-basiertes Sicherheitsmodell**: Verzichtet auf traditionelle UID/GID-Berechtigungssysteme. Capabilities dienen als einziger Zugriffsausweis und beseitigen grundlegend die Schwachstelle „root darf alles“.
- **Anykernel-Dual-Mode-Treiber**: Derselbe Treiberquellcode lässt sich entweder zu einem User-Space-Dienstprozess (gemeinsames Szenario) oder zu einer Direct-Pass-Bibliothek (Hochleistungsszenario) kompilieren, wobei über 90 % des Codes geteilt werden.

> Ausführliches Architekturdesign: [docs/architecture.md](./docs/architecture.md).

---

## Aktueller Status

Das System hat sich von Design-Dokumenten zu einem **Microkernel-Prototypen entwickelt, der auf echter Hardware / QEMU bootet und läuft**. Die folgende Tabelle trennt „funktioniert“ von „nicht begonnen“, damit Designziele nicht mit vorhandenen Fähigkeiten verwechselt werden.

| Bereich | Status | Anmerkungen |
|---------|--------|-------------|
| Microkernel-Kern | ✅ Funktioniert | Schutzdomänen, synchrone/asynchrone IPC, präemptives Scheduling (inkl. **timeout-begrenztem Blockieren**), Adressräume und Demand Paging, Interrupt-Routing (PIC + **LAPIC/MSI-X**, inkl. **blockierender Interrupt-Waits** und **Multi-Vektor-`wait_any`**), Capability-System (inkl. **Capability-Transfer über IPC**) |
| Boot auf echter Hardware / Energieverwaltung | ✅ Funktioniert | Der UEFI-Loader ermittelt das Boot-Laufwerk über den Device Path des Boot-Volumes (`BootInfo` v5); die **`SAFE`-Variante exponiert nur Block-Controller in der Boot-Disk-Kette** (USB-Controller werden nie gefiltert); Shell-`poweroff` nutzt ACPI S5 (FADT + DSDT `_S5`), `reboot` fällt nacheinander auf i8042 → `0xCF9` → Triple Fault zurück; ein fehlender PS/2-Controller wird explizit gemeldet |
| Syscall-Schnittstelle | ✅ 56 (IDs 0–63) | Vollständige ID-Tabelle des Kernels: [docs/dev-reference.md](./docs/dev-reference.md) §5; die anwendungsseitige Teilmenge: [docs/app-dev-guide.md](./docs/app-dev-guide.md) §3 |
| Capability-Sicherheitsmodell | ✅ Funktioniert | Capability-Slots + **Capability-Handles** (bei open ausgestellt, vor jedem I/O geprüft, bei close widerrufen), standardmäßig null Capabilities; **Handle-Transfer** (Move) und **Capability-Delegation** (Kopie, keine Amplifikation) zur Laufzeit über IPC — nicht alles muss beim Boot statisch vergeben werden |
| User-Space-Treiber | ✅ Teilweise | Tastaturtreiber (IRQ1 / PS/2-Präsenzprüfung + USB-HID-Boot-Tastatur); Blockgerätedienst (NVMe-Treiber, mit IDE-PIO-Fallback); **Multi-Controller-xHCI** (bis zu 4 Controller, USB-Storage + USB-HID-Tastatur); Netzwerktreiber **`net_srv` (virtio-net, Domain 16)** — der N2/N3-Treiber funktioniert (Modern-virtio-Bring-up: virtio-Caps parsen / MAC lesen / RX-/TX-Virtqueues aufbauen / `DRIVER_OK` / **MSI-X-interrupt-getriebener RX**, und seine selbst ausgelöste ARP-Anfrage wird vom Gateway mit `NET1 … ARP reply OK` beantwortet). **Generisches Device-Grant (D1)**: Das `device.rs` des Kernels gibt ein `DeviceGrant` heraus (BAR + zusammenhängender DMA-Block + MSI-X-Parameter, keine Gerätesemantik); der Kernel **enthält keinen NVMe-spezifischen Code mehr** — Queue-Layout und Geräteprotokoll liegen in der Treiber-Domain; ein Treiber kann zudem die PCI-Capabilities seines eigenen Geräts über `SYS_DEVICE_CONFIG_READ` parsen, und der Kernel mappt die MSI-X-Tabelle je nach `BIR` aus einer anderen BAR |
| User-Space-Dateisysteme | ✅ Teilweise | FAT32 (inkl. VFAT-Langnamen), tmpfs, ursprüngliches MorionFS v2 (COW + Snapshots + Freibitmap/Speicherrückgewinnung + indirekte Blöcke für große Dateien + variable lange Dirents/Langnamen + Knoten-Metadaten + Inode-Nummern-Indirektionsschicht/Hardlinks/Symlinks + **Formatierung pro Volume-Geometrie** + **explizites `mkfs.mfs`, Multi-Volume- und Primär-Volume-Umschaltung**), ext2 **nur lesend**, exFAT (Lesen + Schreiben, große Volumes mit hoher Cluster-Anzahl) |
| Partitions- / Volume-Schicht | ✅ Funktioniert | block_srv parst die **MBR/GPT**-Partitionstabelle jeder Festplatte in eine Volume-Tabelle und ermittelt den FS-Typ anhand der führenden Signatur des jeweiligen Volumes; es kann auch **Partitionstabellen schreiben** (`part.create/del/wipe/reload`: Partitionen anlegen/löschen, wipen, neu einlesen — sowohl GPT als auch MBR); `dev` ist jetzt eine „Volume-Nummer“ und die Block-Schicht unterstützt Multi-Page-DMA (einzelner Befehl ≤ 128 KiB); **Multi-Volume-Mount**: zusätzliche Volumes desselben Typs werden automatisch unter `/usb<vol>` eingehängt, sodass eine Codebasis mehrere Festplatten bedient — der Weg zum Lesen echter USB-Partitionen |
| Shell & einheitlicher Verzeichnisbaum | ✅ Funktioniert | `help/echo/uname/version/pwd/ls/cat/run/wget/net/ping/ping6/dns/wifi/lspci/dmesg/cd/mkdir/touch/rm/mv/ln/ln -s/chmod/chown/truncate/stat/lstat/readlink/mkfs.mfs/mfs.primary/mfs.snap/mfs.snaps/mfs.rollback/mfs.gc/mfs.fsck/mfs.sync/df/part.create/part.del/part.wipe/part.reload/clear/poweroff/reboot` (Langformat `ls -l`, Symlinks werden als `l` dargestellt; `dmesg` unterstützt `-d/-a/-g/path` und `Ctrl+C`; `uname`/`version` melden den Versionsstring über `SYS_UNAME`); mehrere Dateisysteme werden von der Mount-Schicht zu einer einzigen `/`-Wurzel zusammengesetzt, Laufzeit-Mounting ist möglich |
| Grafik / GUI | 🚧 In Arbeit | **G1** Der Framebuffer wird exklusiv an den User-Space-`gfx_srv` (Domain 15) übergeben: ergänzt `Capability::Fb` + `SYS_FB_INFO/MAP/TAKEOVER`; nach der Übernahme schreibt das Kernel-Terminal nicht mehr auf den Bildschirm (Ausgabe bleibt auf COM1). **G2** Zeichenprimitive `fill/rect/blit` + gemeinsam genutzte Surfaces + Client-Bibliothek `libmorion::gfx`; `blit` liest den Framebuffer erneut und verifiziert vor der Antwort (Selbsttest GS-1). **G3a** Text-Rendering ausgelagert: Font (ASCII 8×16 + CJK 16×16 `cjk.bin` ≈276 KB) und Terminal-Zustand wandern vom Kernel nach `gfx/`, neues Protokoll `GFX_OP_TEXT/CLEAR/MOVE/QUERY`, Verifikation pro Pixel per Write-then-Readback (Selbsttest GT-1). **G3b** Shell-Ausgabe auf dem Bildschirm: `SYS_CONSOLE_READY(47)`; der Print-Sink `sink()` von `libmorion` kann die Ausgabe pro Prozess **auf die Bildschirmkonsole spiegeln** (nur die Shell aktiviert dies). **G3c** Der Kernel entfernt den CJK-Font (−276 KB, Kernel-ELF 347 KB → 69 KB); das Kernel-Terminal fällt auf ASCII + Tofu zurück, CJK-Rendering erfolgt nur noch im User Space. **G4** Eingabe aus dem Kernel ausgelagert: `SYS_KEY_PUSH(48)/SYS_KEY_READ(49)` als Key-Byte-Queue (der Kernel reicht nur Bytes weiter), Zeilenbearbeitung/Echo in der Client-Bibliothek `morion::console::readline`. **G6** Selbstheilung der Dienste: `ipc::call` hängt nicht mehr ewig (Timeout + Fail-Fast, wenn das Ziel keinen laufenden Task hat), Framebuffer als reservierter Kernel-Bereich registriert, veraltete Mailbox-Anfragen beim Neustart verworfen, Client baut gemeinsame Sessions neu auf, `gfx_srv` wird von init überwacht (Selbsttest GS-2). Die Textkonsole des Kernels bleibt nur für Boot- und Panic-Ausgaben erhalten |
| Netzwerk / Virtualisierung / Enclaves / Paketverwaltung | 🚧 In Arbeit | **Netzwerk**: Der virtio-net-Treiber `net_srv` funktioniert; der Stack ist in einen eigenen `netstack_srv` (Domain 21: TCP/IP + Socket-Dienst + Port-Capability-Gate) aufgeteilt, mit zweiter/dritter echter NIC `e1000e_srv`(22) / `e1000_srv`(24), WLAN `wifi_srv`(25) und einem In-Guest-`httpd_srv`(23). **Enclaves**: Die IOMMU/VT-d-Hardwareisolation ist fertig (seit E1c ist das Fenster des Zielgeräts eingeschränkt und DMA außerhalb des Fensters wird mit Nachweis abgelehnt); der Enclave-Manager E2/E3 ist nicht fertig. **Virtualisierung & Paketverwaltung**: Design abgeschlossen, noch keine Implementierung |
| Capability-Schnittstelle für System-KI | 📐 Spezifikation definiert | Wie eine Anwendung Funktionen für die System-KI bereitstellt: [docs/app-dev-guide.md](./docs/app-dev-guide.md) §9 |

> Schnellstart (Build- & Ausführbefehle): [docs/commands.md](./docs/commands.md);
> Kernel- und Schnittstellenreferenz: [docs/dev-reference.md](./docs/dev-reference.md);
> Anwendungsentwicklung (inkl. KI-aufrufbarer Capabilities): [docs/app-dev-guide.md](./docs/app-dev-guide.md);
> Dateisystem-Roadmap: [docs/roadmap-fs.md](./docs/roadmap-fs.md);
> Treiber- & Enclave-Roadmap: [docs/roadmap-driver.md](./docs/roadmap-driver.md).

---

## Version

Aktuelle Version **`0.4.0`**; die veröffentlichte Variante ohne Grafik trägt das Tag **`v0.4.0-nogui`**.

- Die **Single Source of Truth** für Versionskonstanten ist `kernel/src/version.rs` im Kernel (`SYSTEM_NAME` / `VERSION` / `MACHINE` / `VARIANT` / `BUILD`), die über `SYS_UNAME(51)` an den User Space gemeldet wird — die Shell-Befehle `uname` / `version` lesen sie (die Build-Nummer ist der vom `Makefile` injizierte kurze Git-Hash).
- Zwei Build-Varianten:
  - **Regulär (mit Grafik)**: `make ...`; die Shell-Ausgabe wird zusätzlich **auf die Bildschirmkonsole von `gfx_srv` gespiegelt**.
  - **Ohne Grafik**: `make NOGUI=1 ...`; die Shell öffnet keinen Bildschirm-Spiegel (Eingabe/Echo laufen über die serielle Schnittstelle), der Release-String meldet `0.4.0-nogui`, und die Artefakte landen in `build/nogui/`. Alle anderen Dienste und die vollständige Regressionsbasis bleiben unverändert.
- Die Änderungshistorie findet sich in [CHANGELOG.md](./CHANGELOG.md).

---

## Architekturüberblick (Ziel)

```
┌──────────────────────────────────────────────────────┐
│                  Anwendungsschicht                   │
│ POSIX-Schnittstelle (libc)  |  High-Perf Direct API  │
├──────────────────────────────────────────────────────┤
│            User-Space-Systemdienste                  │
│  Dateisystem │ Netzwerk │ Gerätedienst │ Sicherheit  │
│  ext4/vfat   │ TCP/IP   │ Treiber      │ Auth/Audit  │
├──────────────────────────────────────────────────────┤
│    Performance Enclave — optionale Beschleunigung    │
│ GPU Direct  │  NPU Direct  │  Userspace-NIC-Treiber  │
│   (IOMMU-erzwungene Isolation, CHERI-Grenzschutz)    │
├──────────────────────────────────────────────────────┤
│                     Microkernel                      │
│  IPC │ Scheduling │ Adressraum │ IRQ-Route │ Caps    │
└──────────────────────────────────────────────────────┘
```

---

## Kerndesign

### Microkernel-Primitive

Enthält nur die nicht weiter reduzierbare minimale Menge:

| Primitiv | Beschreibung |
|-----------|-------------|
| `send` / `receive` / `call` | Synchrone/asynchrone IPC mit Capability-Transfer |
| `map` / `unmap` | Verwaltung des Adressraum-Mappings |
| `create_domain` / `destroy_domain` | Lebenszyklus einer Schutzdomäne (Prozess) |
| `schedule` | CPU-Scheduling |
| `allocate_frame` / `free_frame` | Verwaltung physischer Speicherframes |
| `register_interrupt` / `ack_interrupt` | Interrupt-Autorisierung und -Bestätigung |
| `create_enclave` | Erzeugung einer hardwareisolierten Enclave |

### Externer Pager

- Der Kernel fängt Page Faults nur ab und leitet sie weiter; User-Space-Pager-Dienste entscheiden über Inhalt und Verdrängungsstrategie
- Jeder Prozess kann einen eigenen Pager bestimmen — On-Demand-Paging, komprimierte Speicherpools, Netzwerkspeicher usw.

### Anykernel-Dual-Mode-Treiber

| Modus | Zielgruppe | Eigenschaften |
|------|--------|-----------------|
| **Treiber-Dienstprozess** | Allgemeine Apps | Geräte-Sharing, sichere Isolation, indirekter Zugriff über IPC |
| **Direct-Treiberbibliothek (LibDevice)** | Gaming / KI | Zur Laufzeit gelinkt, keine Kernel-Traps bei MMIO-/DMA-Operationen |

Eine einzige Rust-Trait-Schnittstelle, Backend-Auswahl über Compile-Time-Feature-Flags, >90 % Code-Sharing.

### Performance Enclave

1. Die IOMMU mappt Geräte-MMIO- und DMA-Fenster in den Prozessadressraum
2. Die Direct-Treiberbibliothek LibDevice wird eingebunden
3. GPU-/NPU-Kommandos werden direkt ohne Eingriff des Kernels übermittelt

Der Blast Radius ist hardwareseitig auf die Ressourcengrenzen der Enclave begrenzt.

### Capability-basiertes Sicherheitsmodell

- Capabilities als einziger Zugriffsausweis, keine Abhängigkeit von UID/GID
- Neue Prozesse starten ohne Capabilities und erhalten sie explizit vom Parent
- POSIX-Berechtigungs-APIs (`chmod`/`chown`) werden von libc in Capability-Operationen übersetzt
- Sicherheitsrichtlinien werden von einer User-Space-Policy-Engine interpretiert, dynamische Aktualisierungen werden unterstützt

### User-Space-Dienste

Jede traditionelle Kernelfunktion läuft als unabhängiger User-Space-Prozess:

| Dienst | Aufgabe | Status |
|---------|---------------|--------|
| Dateisystemdienst | FAT32 (inkl. VFAT-Langnamen), tmpfs, ursprüngliches MorionFS, ext2 nur lesend, exFAT (Lesen + Schreiben), vereinheitlicht über libvfs | ✅ Implementiert (ext2 nur lesend / exFAT lesend und schreibend) |
| Gerätedienst | Blockgerät (NVMe-Treiber), Tastaturtreiber, Interrupt-Dispatch | ✅ Teilweise |
| Shell-Dienst | Kommandozeileninterpreter + einheitlicher Verzeichnisbaum / Laufzeit-Mounting | ✅ Implementiert |
| Netzwerk-Stack | User-Space-TCP/IP, Zero-Copy-Shared-Memory | ⏳ Geplant |
| Sicherheit / Audit | Authentifizierung, Policy-Engine, Intrusion Detection | ⏳ Geplant |
| Enclave-Manager | Enclave-Lebenszyklus, Log-Streams, Migration & Suspend | ⏳ Geplant |
| Paketmanager | Deklarative Builds im Nix-Stil, atomare Umschaltung, Versions-Rollback | ⏳ Geplant |
| GUI-Dienst | Acryl-transparenter Desktop, hochgradig anpassbar (`gfx_srv` Framebuffer / Textkonsole / Zeichenprimitive implementiert, Desktop-Umgebung nicht begonnen) | 🚧 In Arbeit |
| Audio / IME / Container / Time / Power / Log / Config | Infrastrukturdienste des Systems | ⏳ Geplant |
| Registrierungs- / Gateway-Dienst für KI-Capabilities | Registrierung & Discovery von Anwendungs-Capabilities, Autorisierung und Audit von KI-Aufrufen | 📐 Spezifikation definiert (siehe [app-dev-guide.md](./docs/app-dev-guide.md) §9) |

### Virtualisierung

- Der Microkernel fungiert zugleich als Hypervisor (Intel VT-x / AMD-V)
- Unterstützt Unikernels und unveränderte Linux-/Windows-Gäste
- PCIe-Geräte-Passthrough (VT-d / IOMMU), verschachtelte Enclaves

### Boot

- UEFI-nativ, keine Übergänge in den Legacy-Real-Mode
- Hochauflösendes GOP-Bootmenü, Acryl-Theme
- Boot-Einträge auf Basis von Nix-Closures, atomare Umschaltung & Rollback
- TPM-2.0-Messung + Secure-Boot-Verifikation
- kexec-Warmboot, Koexistenz mehrerer Betriebssysteme

---

## Repository-Struktur

### Aktuelle Struktur

Das Projekt ist ein Rust-Workspace (Wurzel-`Cargo.toml`), der derzeit die Crates `boot`, `kernel`, `user/srv`, `user/libmorion`, `user/libdevice`, `user/hello` und `kernel_test` enthält. Jeder Systemdienst außerhalb des Kernels (Blockgerät / Dateisystem / Mount / Shell / Tastaturtreiber, …) ist ein **eigenständiges User-Space-Programm** (ein `[[bin]]` pro Dienst in `user/srv` → ein eigenständiges ELF); der **Bootloader liest sie beim Booten aus der ESP (`EFI/morion/services/`)** und übergibt sie über die `BootInfo`-Modultabelle an den Kernel, der sie anschließend jeweils in ihre feste Domain lädt (E2b: kein einzelnes flaches Binary mehr, das per Domain-ID verteilt wird; E3b: Dienste sind nicht mehr in das Kernel-Image eingebettet). UI-Assets sind nach Zweck organisiert: Ressourcen zur Boot-Zeit unter `boot/loader/resources/`, systemweite Ressourcen unter `resources/system/`.

```
.
├── .github/
│   ├── workflows/            # GitHub Actions (automatische Synchronisierung zu Gitee)
│   ├── ISSUE_TEMPLATE/       # Issue-Vorlagen
│   └── PULL_REQUEST_TEMPLATE.md
├── boot/                     # UEFI-Bootloader (morion-boot)
│   ├── asm/
│   │   └── boot_stub.asm     #   Assembler-Stub für den Boot-Einstieg
│   ├── loader/               # Boot-Zeit-Ressourcen & -Konfiguration
│   │   ├── entries/          #   Boot-Einträge (.conf)
│   │   │   └── morion.conf
│   │   ├── resources/        #   Theme-Assets des Bootloaders (BMP/PNG)
│   │   │   ├── animation/    #     Frames der Ladeanimation
│   │   │   ├── background/   #     Hintergründe (dark/light/default/mask)
│   │   │   ├── icons/        #     Kategorie-Symbole (dialog/power/security/system/ui)
│   │   │   ├── logo/         #     Logo-Varianten (horizontal/monochrome/square/system)
│   │   │   ├── progress/     #     Fortschrittsbalken (bar_bg/bar_fill)
│   │   │   └── splash/       #     Splashscreen (background/logo)
│   │   ├── kernel_placeholder.bin
│   │   ├── loader.conf       #   Bootloader-Konfiguration
│   │   └── theme.toml        #   Acryl-Theme-Konfiguration
│   └── src/
│       ├── boot/             #   Boot-Ablauf (loader/menu/kexec)
│       ├── config/           #   Konfigurationsparsing (entries/theme)
│       ├── gfx/              #   Grafik-Rendering (framebuffer/font/renderer/animation)
│       ├── security/         #   Sicherheit (hash/secure_boot/tpm)
│       ├── lib.rs
│       └── main.rs
├── kernel/                   # Microkernel (morion-kernel, minimale TCB)
│   └── src/
│       ├── arch/             #   x86_64 (gdt/idt/pic/pit/keyboard/pci)
│       ├── memory/           #   Speicherverwaltung (paging/frame_allocator)
│       ├── scheduler/        #   Scheduler (Kontextwechsel)
│       ├── video/            #   Textkonsole des Kernels (framebuffer/font/logo/bg/unicode) — Boot- & Panic-Ausgabe
│       ├── bootinfo.rs       #   Boot-Info (Speicherkarte + GOP-Framebuffer)
│       ├── cap.rs            #   Capability-System (Slots + Handle-Tabelle)
│       ├── domain.rs         #   Schutzdomänen (Prozesse, Pagetables pro Domain)
│       ├── elf.rs            #   ELF64-Parsing & -Validierung (Vertrauensgrenze beim Laden von Executables)
│       ├── exec.rs           #   ELF zur Laufzeit laden → Domain erzeugen → mappen → Task starten
│       ├── ipc.rs            #   Interprozesskommunikation
│       ├── irq.rs            #   Interrupt-Routing (Interrupts als IPC)
│       ├── device.rs         #   Generisches Device-Grant (BAR / DMA / MSI-X → DeviceGrant)
│       ├── pager.rs          #   User-Space-Pager-Schnittstelle
│       ├── syscall.rs        #   Syscall-Einstieg und ID-Tabelle
│       ├── lib.rs
│       └── main.rs
├── user/                     # User Space: Runtime + gemeinsame Treiberbibliotheken + Dienste
│   ├── libmorion/            #   Runtime (Crate `morion`): Syscall / Print / libvfs / libgfx / Entry-Boilerplate
│   ├── libdevice/            #   Gemeinsame Treiberbibliothek (Crate `libdevice`, D2/D2b): Device-Grant / MMIO / MSI-X / virtio-Transport + vring
│   ├── hello/                #   Demo: ein **eigenständiges ELF-Programm** (zur Laufzeit von SYS_SPAWN_ELF geladen)
│   └── srv/                  #   Systemdienste (Crate `morion-srv`): ein [[bin]] pro Dienst → ein eigenständiges ELF
│       └── src/
│           ├── common.rs     #     Wire-Protokoll / Block-Client / Namenshelfer, von den Diensten gemeinsam genutzt
│           ├── block_srv.rs  #     Domain 5  Dienst für den Blockgerätetreiber
│           ├── fat32_srv.rs  #     Domain 6  FAT32-Dateisystemdienst
│           ├── app.rs        #     Domain 7  Selbsttestprogramm
│           ├── shell.rs      #     Domain 8  Kommandozeile
│           ├── mount_srv.rs  #     Domain 9  Mount-Dienst
│           ├── tmpfs_srv.rs  #     Domain 10 In-Memory-Dateisystem
│           ├── mfs_srv.rs    #     Domain 11 MorionFS
│           ├── ext2_srv.rs   #     Domain 12 ext2 nur lesend
│           ├── exfat_srv.rs  #     Domain 13 exFAT lesend und schreibend
│           ├── init.rs       #     Domain 14 Supervisor (überwacht die Dienst-Domains, startet sie in-place aus dem Speicherabbild neu)
│           ├── gfx_srv.rs    #     Domain 15 Grafikdienst (hält den Framebuffer, User-Space-Rendering: Primitive + Textterminal)
│           ├── net_srv.rs    #     Domain 16 Netzwerktreiber (virtio-net; N0–N3: MSI-X-Interrupts + ARP-Selbsttest)
│           ├── virtio_blk_srv.rs  # Domain 17 virtio-blk-Blocktreiber (D3: generisches Grant, Read-Signature-/Write-Readback-Selbsttest)
│           ├── gfx/          #     Interna des Grafikdienstes: Framebuffer-View + Fonts (font/glyphs/cjk.bin) + Terminal
│           ├── sender.rs / receiver.rs / pager.rs / echo.rs / kbd.rs  # Domains 0..4 Demos & Tastatur
│           └── bin/          #     18 Entry-Points (jeder schreibt morion_main → run() seines Moduls)
├── kernel_test/              # Früher Kernel für Boot-Integrationstests (vorerst beibehalten)
│   └── src/main.rs
├── resources/
│   └── system/               # Systemweite Ressourcen
│       ├── device/           #   Gerätesymbole (.ico)
│       ├── file/             #   Dateityp-Symbole (.ico)
│       ├── github/           #   GitHub-Cover (.png)
│       ├── icons/            #   Allgemeine UI-Symbole (.ico)
│       ├── logo/             #   System-Logo (.ico/.svg/.png)
│       ├── service/          #   Dienst-Symbole (.ico)
│       └── terminal/         #   Terminal-Hintergründe (.raw)
├── docs/
│   ├── architecture.md       #   Technisches Architekturdesign
│   ├── app-dev-guide.md      #   Anwendungsentwicklungsleitfaden (inkl. Spezifikation KI-aufrufbarer Capabilities)
│   ├── dev-reference.md      #   Kurzreferenz zu Kernel & Schnittstellen
│   ├── commands.md           #   Befehle zum Bauen / Ausführen / Verifizieren
│   ├── shell-reference.md    #   Referenz zur Shell-Nutzung
│   └── roadmap-fs.md         #   Dateisystem-Roadmap
├── Cargo.toml                # Rust-Workspace (boot/kernel/user/kernel_test)
├── Cargo.lock
├── Makefile                  # Build-System (make iso/run/run-nvme/debug/check/clippy)
├── flake.nix                 # Nix-Build-Integration
├── rust-toolchain.toml       # Rust-Nightly-Toolchain
├── linker.ld                 # Linker-Skript des Kernels
├── .gitattributes
├── .gitignore
├── CONTRIBUTING.md
├── LICENSE
├── README.md                 # Englisch (Hauptversion)
├── README.zh-CN.md           # 简体中文
├── README.ja.md              # 日本語
├── README.ko.md              # 한국어
├── README.es.md              # Español
├── README.fr.md              # Français
├── README.de.md              # Deutsch
└── README.ru.md              # Русский
```

### Zielstruktur der Entwicklung

```
├── bootloader/       # Bootloader (UEFI)
├── kernel/           # Kernel-Quellcode
│   ├── arch/         #   Architekturspezifisch (x86_64 / AArch64 / RISC-V)
│   ├── core/         #   Microkernel-Kern (IPC, Scheduler, Adressraum, Caps)
│   └── compat/       #   Kompatibilitätsschichten (POSIX / Linux / RTOS)
├── services/         # User-Space-Systemdienste
│   ├── fs/           #   Dateisystemdienst
│   ├── net/          #   Netzwerk-Stack
│   ├── device/       #   Gerätedienst + Treiber
│   ├── security/     #   Sicherheit / Auth / Audit
│   ├── enclave/      #   Enclave-Manager
│   ├── gui/          #   GUI-Dienst
│   ├── shell/        #   Shell-Dienst
│   ├── audio/        #   Audiodienst
│   ├── ime/          #   IME-Dienst
│   └── ...           #   Weitere Dienste
├── userland/         # User Space
│   ├── libs/         #   libc, libvfs, libdevice usw.
│   └── bin/          #   Basisbefehle (ls, cat, mkdir, rm)
├── resources/        # Ressourcendateien
├── docs/             # Dokumentation
└── pkg/              # Paketverwaltung (Nix-Stil)
```

---

## Roadmap

> Ausführliche Dateisystem-Roadmap: [docs/roadmap-fs.md](./docs/roadmap-fs.md); Roadmap des Grafiksubsystems: [docs/roadmap-gfx.md](./docs/roadmap-gfx.md). Abgehakte Punkte sind **unter QEMU verifiziert lauffähig**.

### Phase 1 — Microkernel-Kern (im Wesentlichen abgeschlossen)

- [x] Erzeugung von Schutzdomänen (Prozessen) und Adressraumisolation
- [x] Synchrone/asynchrone IPC (`send` / `recv` / `call` / `reply`; 96-Byte-Payload + Shared Memory für Massendaten)
- [x] Task-Scheduling und Kontextwechsel
- [x] Adressraum-Map/Unmap + User-Space-Pager (Demand Paging)
- [x] Interrupt-Routing („Interrupts als IPC“) + MMIO- / I/O-Port-Grants
- [x] Capability-System (Slots + Handles: ausstellen / prüfen / widerrufen)
- [x] **Capability-Transfer über IPC** (Handle-Transfer `SYS_HANDLE_SEND`: ein geöffnetes Objekt **in** die Ziel-Domain verschieben, Move-Semantik; Capability-Delegation `SYS_CAP_SEND`: eine gehaltene Capability **kopieren**, keine Amplifikation; beide erfordern `SendTo(to)`). Die Domains 0/1/3 decken beim Boot 8 positive/negative Selbsttestfälle ab

### Phase 2 — Basisdienste (in Arbeit)

- [x] PCI-Enumeration + User-Space-NVMe-Treiber (Blockgerätedienst, mit IDE-PIO-Fallback)
- [x] MBR/GPT-**Partitionsparsing + Volume-Schicht** (innerhalb von block_srv, `dev` = Volume-Nummer; FS-Typ über die führende Signatur des jeweiligen Volumes ermittelt)
- [x] **Multi-Volume-Mount**: Die oberen 32 Bit des Request-Tags tragen den Volume-Code, ein Open bindet genau ein Volume; jeder FS-Dienst meldet seine zusätzlichen Volumes automatisch (eingehängt unter `/usb<vol>`); fat32-Clusterpuffer 2 → 16 Pages (**64 KiB** Cluster-Obergrenze), ext2-Blockgruppenlimit 16 → 4096
- [x] Dateisysteme: FAT32 (inkl. VFAT-Langnamen) / tmpfs / ursprüngliches MorionFS (COW + Snapshots)
- [x] ext2-**Nur-Lese**-Kompatibilität (hängt vorhandene Linux-Partitionen ein)
- [x] exFAT-Kompatibilität (`exfat_srv` Domain 13, eingehängt unter `/usb`): **nur lesend** + **lesend und schreibend** (`CREAT/WRITE/MKDIR/UNLINK/RMDIR/TRUNCATE`) + **große Volumes** (Multi-Page-DMA, einzelner Befehl ≤ 128 KiB)
- [x] libvfs + Mount-Schicht: mehrere Dateisysteme zu einer einzigen `/`-Wurzel zusammengesetzt, Laufzeit-Mounting unterstützt
- [x] Shell (Builtins + cwd-relative Pfade) und User-Space-Tastaturtreiber
- [x] **MorionFS-v2-Format finalisiert + Speicherrückgewinnung / GC** (Freibitmap + Mark & Sweep)
- [x] **Große Dateien in MorionFS** (`MFS3`: indirekte Blöcke, bricht die alte Grenze von ≈4 MiB)
- [x] **MorionFS-Verzeichnisse & Langnamen** (`MFS4`: variable lange Dirents + Erweiterungsblöcke, Namen ≤255 Byte)
- [x] **MorionFS-Knoten-Metadaten** (`MFS5`: Zeitstempel / Berechtigungen / Eigentümer / Link-Zähler, `rename` / `truncate` / `chmod`)
- [x] **Inode-Nummern-Indirektion + Hardlinks in MorionFS** (`MFS6`)
- [x] **MorionFS-Symlinks** (`MFSL`: Pfadauflösung mit Folgen + Schleifenschutz, `readlink` / `lstat` / `ln -s`)
- [x] **MorionFS-Formatierung pro Volume-Geometrie** (**M7**)
- [x] **Explizites MorionFS-Format + Multi-Volume** (**M8 / S2**: `mkfs.mfs <vol>`, der Guard akzeptiert nur leere oder MFS-Volumes)
- [x] **MorionFS-Kapazitätsskalierung (Bitmap externalisiert)** (**S3a**: Kapazitätsobergrenze ≈119 MiB → **≈127.25 GiB**)
- [x] **MorionFS-Einzeldatei über 4 GiB** (**S3b**: VFS `offset`/`size` durchgängig u64 + indirekter Block dritter Stufe `MFI3`)
- [x] **MorionFS-Umschaltung des Primär-Volumes** (**S2**: Der Superblock speichert die Seriennummer des Primär-Volumes; die höchste Seriennummer gewinnt)
- [x] **MorionFS-Umhängen des Primär-Volumes nur per Marker** (**S2**: `mfs.primary <vol>` ohne Datenänderung)
- [x] **`df`-Speicherbelegung** (MorionFS meldet gesamt / belegt / frei)
- [x] **Abschluss der Volume-Verwaltung: Partitionstabellen schreiben** (**S2**: `part.create/del/wipe/reload`, GPT + MBR, gegen Host-`sgdisk -v` verifiziert)
- [x] **NVMe-Interrupts (MSI/MSI-X)** (**S3**: minimale LAPIC-Unterstützung + PCI-Capability-Walk + MSI-Vektorbereich `0x50..0x5F`)
- [x] **Blockierendes Warten auf Interrupts (Wait-Primitiv)** (**S4**: timeout-begrenztes Blockieren + `SYS_IRQ_WAIT`)
- [x] **Multi-Vektor + `wait_any`** (**S5**: Wait-Key pro Domain + Vektor-Maske, Admin- und 2 I/O-Queues mit jeweils eigenem Vektor)
- [x] **CJK-Rendering im Terminal (CJK / Vollbreite / gemischte Breite)** (die Kernel-Seite wurde später in G3c entfernt, der Font in den User-Space-`gfx_srv` verschoben)
- [x] **Laden von Executables (ELF + Runtime-Spawn)** (**E1**: ELF64-Loader + `SYS_SPAWN_ELF`; die Demo `user/hello` ist ein eigenständiges ELF)
- [x] **User-Space-Runtime libmorion + Befehl `run`** (**E2a**)
- [x] **Dienste als eigenständige Programme + Domain-Destroy / Frame-Reclaim** (**E2b**: 14 Dienste, jeweils ein ELF; `SYS_DOMAIN_DESTROY/COUNT/FRAME_FREE`)
- [x] **Abschluss des Dienst-Lebenszyklus** (**E3a/E3b/E3c**: User-Page-W^X; Dienste aus dem Kernel-Image ausgelagert; `init`-Supervisor + In-Place-Neustart aus dem Speicherabbild, keine Abhängigkeit von der Festplatte)
- [x] **Grafik G1 (Framebuffer in den User Space verschoben)**
- [x] **Grafik G2 (Zeichenprimitive + gemeinsam genutzte Surfaces + `libmorion::gfx`)**
- [x] **Grafik G3a (Terminal im Dienst + Text-Rendering ausgelagert)**
- [x] **Grafik G3b (Shell-Ausgabe auf dem Bildschirm)**
- [x] **Grafik G3c (Kernel entfernt den CJK-Font, −276 KB)**
- [x] **Grafik G4 (Eingabe aus dem Kernel ausgelagert)**
- [x] **Grafik G6 (Selbstheilung von gfx_srv: überwachter Neustart + Neuaufbau der Client-Session)**
- [ ] **Weitere Dateisystem-Kompatibilität** (ext4-Schreiben, UDF usw.)
- [ ] Netzwerk-Stack

### Phase 3 — Performance-Enclaves (in Arbeit)

- [x] **IOMMU-Passthrough** (**E1a** ACPI-DMAR-Probe / **E1b** Root Table + Context Table + Identitäts-Pagetables zweiter Ebene und `GCMD.TE` / **E1c** Das Fenster des Zielgeräts ist auf `[0, 3 GiB)` verengt, geräteinitiiertes DMA außerhalb des Fensters wird von der IOMMU mit Nachweis abgelehnt — alles erledigt, siehe [docs/roadmap-driver.md](./docs/roadmap-driver.md))
- [ ] **LibDevice-Passthrough-Form & Enclave-Manager** (E2/E3 ausstehend; die gemeinsame Bibliothek `user/libdevice` liegt bereits vor)

### Phase 4 — Netzwerk & Sicherheit (in Arbeit)

- [x] **NIC-Treiber (virtio-net)** (`net_srv` Domain 16 + generisches Device-Grant, siehe [docs/roadmap-driver.md](./docs/roadmap-driver.md) N0–N3: virtio-modern-Bring-up + MSI-X-Interrupt-RX + ARP-End-to-End-Selbsttest)
- [x] **Zweiter echter Treiber (virtio-blk)** (`virtio_blk_srv` Domain 17, weiterhin über generisches Device-Grant, keine gerätespezifische Kernellogik: D3 — Read-Signature- / Write-Readback-Selbsttest)
- [x] **Device-Grant zur Laufzeit (D1b)** (`SYS_DEVICE_INFO` / `SYS_DEVICE_GRANT` + `Mmio`-Capability-Gate; NVMe / `net_srv` / `virtio_blk_srv` beziehen ein `DeviceGrant` per Laufzeit-Syscall, Verhalten unverändert)
- [ ] TCP/IP-Stack, Capability-Auditing, Policy-Engine

### Phase 5 — GUI & Ökosystem (nicht begonnen)

- [ ] Desktop-Umgebung, Paketmanager, Virtualisierung

### Querschnitt — Capability-Schnittstelle für System-KI (Spezifikation definiert, Implementierung nicht begonnen)

- [x] Design-Spezifikation: Wie Apps Funktionen als **KI-aufrufbare Capabilities** annotieren / beschreiben (siehe [docs/app-dev-guide.md](./docs/app-dev-guide.md) §9)
- [ ] Dienst zur Capability-Registrierung & -Discovery
- [ ] KI-Aufruf-Gateway (Capability-Prüfungen / Audit-Log / Timeouts & Quotas)

---

## Lizenz

Dieses Projekt verwendet **Lizenzierung pro Modul**:

- Core (`boot/`, `kernel/`, `user/`): [MIT](./LICENSE)
- Enclave-Modul (`enclave/`): [Apache-2.0](./LICENSES/Apache-2.0.txt), einschließlich ausdrücklicher Patentlizenz und Beendigung bei Patentstreitigkeiten
- Treiber / Firmware von Drittanbietern: behalten ihre ursprünglichen Lizenzen; siehe [docs/licensing.md](./docs/licensing.md)

Verbindliche Formulierung: **Der Morion OS Core steht unter MIT; das Enclave-Modul unter Apache-2.0; Treiber und Firmware von Drittanbietern behalten ihre eigenen Lizenzen.** Das Repository steht nicht insgesamt unter einer einzigen MIT-Lizenz.

---

## Kontakt

- Projekt-Startseite: [github.com/sheng-fs/morion-os](https://github.com/sheng-fs/morion-os)
- E-Mail: 3555679134@qq.com
