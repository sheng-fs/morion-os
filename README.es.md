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

## Descripción general

Morion OS es un sistema operativo moderno construido desde cero en **Rust**. El proyecto se encuentra actualmente en una **fase de rediseño y reescritura**, tras haber reexaminado a fondo los conflictos arquitectónicos de implementaciones anteriores y haber restablecido la **arquitectura híbrida de microkernel + exokernel** como dirección técnica central.

El código heredado se ha archivado (la rama `legacy`) y la línea principal se ha reiniciado. Desde entonces, ha pasado del diseño puro a un **prototipo de microkernel ejecutable**: arranca bajo QEMU (UEFI) en un shell de espacio de usuario y ejecuta la ruta completa de lectura/escritura de archivos — "aplicación → libvfs → servicio de sistema de archivos → servicio de dispositivo de bloque → disco NVMe".

### Principios fundamentales

- **Base de computación de confianza del microkernel**: El kernel expone un único límite — la interfaz de llamadas al sistema; los sistemas de archivos, la red, los controladores de dispositivos, los gráficos, el shell y cualquier otra función tradicional del kernel se implementan como servicios de espacio de usuario. Solo un puñado de primitivas centrales residen en el kernel (IPC, mapeo de espacios de direcciones, dominios de protección, capacidades) — e incluso dentro de la tabla completa de syscalls 0–63 estas constituyen solo una pequeña parte (véase [docs/dev-reference.md](./docs/dev-reference.md) §5).
- **Ruta de rendimiento del exokernel**: Mediante el mecanismo de "Performance Enclave", las capacidades de hardware IOMMU / CHERI permiten que las aplicaciones de alto rendimiento (juegos, IA) operen directamente sobre el hardware — cero traps del kernel, cero copias de datos.
- **Modelo de seguridad basado en capacidades**: Abandona los sistemas tradicionales de permisos UID/GID. Las capacidades son la única credencial de acceso, eliminando de raíz las vulnerabilidades del tipo "root puede hacer cualquier cosa".
- **Controladores de doble modo Anykernel**: El mismo código fuente de controlador se compila como un proceso de servicio de espacio de usuario (escenario compartido) o como una biblioteca de paso directo (escenario de alto rendimiento), compartiendo más del 90 % del código.

> Diseño arquitectónico detallado: [docs/architecture.md](./docs/architecture.md).

---

## Estado actual

El sistema ha pasado de los documentos de diseño a un **prototipo de microkernel que arranca y se ejecuta en hardware real / QEMU**. La siguiente tabla separa lo que "funciona" de lo que "no se ha comenzado", para que los objetivos de diseño no se confundan con las capacidades existentes.

| Área | Estado | Notas |
|------|--------|-------|
| Núcleo del microkernel | ✅ Funcionando | Dominios de protección, IPC síncrona/asíncrona, planificación expulsiva (incl. **bloqueo acotado por timeout**), espacios de direcciones y paginación bajo demanda, enrutamiento de interrupciones (PIC + **LAPIC/MSI-X**, incl. **esperas bloqueantes de interrupción** y **`wait_any` multi-vector**), sistema de capacidades (incl. **transferencia de capacidades por IPC**) |
| Arranque en hardware real / energía | ✅ Funcionando | El cargador UEFI localiza el disco de arranque a partir del Device Path del volumen de arranque (`BootInfo` v5); la **variante `SAFE` expone únicamente los controladores de bloque en la cadena del disco de arranque** (los controladores USB nunca se filtran); el `poweroff` del shell usa ACPI S5 (FADT + DSDT `_S5`), `reboot` recurre a i8042 → `0xCF9` → triple fault; la ausencia de un controlador PS/2 se informa explícitamente |
| Interfaz de syscalls | ✅ 56 (IDs 0–63) | Tabla completa de IDs del kernel: [docs/dev-reference.md](./docs/dev-reference.md) §5; subconjunto orientado a aplicaciones: [docs/app-dev-guide.md](./docs/app-dev-guide.md) §3 |
| Modelo de seguridad por capacidades | ✅ Funcionando | Slots de capacidad + **handles de capacidad** (emitidos al abrir, verificados antes de cada E/S, revocados al cerrar), cero capacidades por defecto; **transferencia de handles** (move) y **delegación de capacidades** (copy, sin amplificación) en tiempo de ejecución sobre IPC — no todo debe concederse estáticamente en el arranque |
| Controladores en espacio de usuario | ✅ Parcial | Controlador de teclado (IRQ1 / sonda de presencia PS/2 + teclado USB HID en modo boot); servicio de dispositivo de bloque (controlador NVMe, con respaldo IDE PIO); **xHCI multi-controlador** (hasta 4 controladores, almacenamiento USB + teclado USB HID); controlador de red **`net_srv` (virtio-net, dominio 16)** — el controlador N2/N3 funciona (bring-up de virtio moderno: analizar las capacidades de virtio / leer la MAC / construir las virtqueues RX·TX / `DRIVER_OK` / **RX dirigida por interrupciones MSI-X**, y su solicitud ARP autocontenida es respondida por la puerta de enlace `NET1 … ARP reply OK`). **Concesión genérica de dispositivo (D1)**: el `device.rs` del kernel entrega un `DeviceGrant` (BAR + bloque DMA contiguo + parámetros MSI-X, sin semántica de dispositivo); el kernel **ya no contiene código específico de NVMe** — el diseño de colas y el protocolo del dispositivo residen en el dominio del controlador; un controlador también puede analizar las capacidades PCI de su propio dispositivo mediante `SYS_DEVICE_CONFIG_READ`, y el kernel mapea la tabla MSI-X desde una BAR distinta según `BIR` |
| Sistemas de archivos en espacio de usuario | ✅ Parcial | FAT32 (incl. nombres largos VFAT), tmpfs, MorionFS v2 original (COW + snapshots + bitmap de libres/recuperación de espacio + bloques indirectos para archivos grandes + dirents de longitud variable/nombres largos + metadatos de nodo + capa de indirección de números de inodo/enlaces duros/enlaces simbólicos + **formateo según la geometría del volumen** + **`mkfs.mfs` explícito, multi-volumen y conmutación de volumen primario**), ext2 **de solo lectura**, exFAT (lectura + escritura, volúmenes grandes y con alto número de clústeres) |
| Capa de particiones / volúmenes | ✅ Funcionando | block_srv analiza la tabla de particiones **MBR/GPT** de cada disco en una tabla de volúmenes y sondea el tipo de FS por la firma inicial de cada volumen; también puede **escribir tablas de particiones** (`part.create/del/wipe/reload`: crear/eliminar particiones, borrar, reexplorar — tanto GPT como MBR); `dev` es ahora un "número de volumen" y la capa de bloque admite DMA multi-página (comando único ≤ 128 KiB); **montaje multi-volumen**: los volúmenes adicionales del mismo tipo se montan automáticamente en `/usb<vol>`, de modo que una sola base de código sirve para varios discos — allanando el camino para leer particiones USB reales |
| Shell y árbol de directorios unificado | ✅ Funcionando | `help/echo/uname/version/pwd/ls/cat/run/wget/net/ping/ping6/dns/wifi/lspci/dmesg/cd/mkdir/touch/rm/mv/ln/ln -s/chmod/chown/truncate/stat/lstat/readlink/mkfs.mfs/mfs.primary/mfs.snap/mfs.snaps/mfs.rollback/mfs.gc/mfs.fsck/mfs.sync/df/part.create/part.del/part.wipe/part.reload/clear/poweroff/reboot` (formato largo `ls -l`, los enlaces simbólicos se muestran como `l`; `dmesg` admite `-d/-a/-g/path` y `Ctrl+C`; `uname`/`version` informan la cadena de versión mediante `SYS_UNAME`); varios sistemas de archivos se ensamblan en una única raíz `/` mediante la capa de montaje, con montaje en tiempo de ejecución |
| Gráficos / GUI | 🚧 En progreso | **G1** framebuffer entregado en exclusiva al espacio de usuario `gfx_srv` (dominio 15): añade `Capability::Fb` + `SYS_FB_INFO/MAP/TAKEOVER`; tras la toma de control, la terminal del kernel ya no escribe en pantalla (la salida permanece en COM1). **G2** primitivas de dibujo `fill/rect/blit` + superficies compartidas + biblioteca cliente `libmorion::gfx`; `blit` relee el framebuffer y verifica antes de responder (autotest GS-1). **G3a** renderizado de texto trasladado fuera: la fuente (ASCII 8×16 + CJK 16×16 `cjk.bin` ≈276 KB) y el estado de la terminal pasan del kernel a `gfx/`, nuevo protocolo `GFX_OP_TEXT/CLEAR/MOVE/QUERY`, verificación píxel a píxel de escritura y relectura (autotest GT-1). **G3b** salida del shell en pantalla: `SYS_CONSOLE_READY(47)`; el sumidero de impresión `sink()` de `libmorion` puede **duplicar** la salida a la consola de pantalla por proceso (solo el shell lo habilita). **G3c** el kernel elimina la fuente CJK (−276 KB, ELF del kernel 347 KB → 69 KB); la terminal del kernel recurre a ASCII + tofu y el renderizado CJK ocurre solo en espacio de usuario. **G4** entrada trasladada fuera del kernel: cola de bytes de tecla `SYS_KEY_PUSH(48)/SYS_KEY_READ(49)` (el kernel solo transporta bytes), edición de línea/eco en la biblioteca cliente `morion::console::readline`. **G6** autorreparación de servicios: `ipc::call` ya no se bloquea indefinidamente (timeout + fallo rápido cuando el destino no tiene tarea viva), framebuffer registrado como rango reservado del kernel, las solicitudes de buzón obsoletas se descartan al reiniciar, el cliente reconstruye las sesiones compartidas, `gfx_srv` supervisado por init (autotest GS-2). La consola de texto del kernel permanece solo para el arranque y la salida de panic |
| Redes / virtualización / enclaves / gestor de paquetes | 🚧 En progreso | **Redes**: el controlador virtio-net `net_srv` funciona; la pila se divide en un `netstack_srv` dedicado (dominio 21: TCP/IP + servicio de sockets + puerta de capacidades de puerto), con segundo/tercer NIC reales `e1000e_srv`(22) / `e1000_srv`(24), inalámbrico `wifi_srv`(25) y un `httpd_srv`(23) dentro del guest. **Enclaves**: el aislamiento por hardware IOMMU/VT-d está hecho (desde E1c la ventana del dispositivo objetivo está restringida y el DMA fuera de ventana se rechaza con evidencia); el gestor de enclaves E2/E3 no está hecho. **Virtualización y gestión de paquetes**: diseño decidido, aún sin implementación |
| Interfaz de capacidades del sistema de IA | 📐 Especificación definida | Cómo expone una aplicación su funcionalidad a la IA del sistema: [docs/app-dev-guide.md](./docs/app-dev-guide.md) §9 |

> Inicio rápido (comandos de compilación y ejecución): [docs/commands.md](./docs/commands.md);
> referencia del kernel y de las interfaces: [docs/dev-reference.md](./docs/dev-reference.md);
> desarrollo de aplicaciones (incl. capacidades invocables por IA): [docs/app-dev-guide.md](./docs/app-dev-guide.md);
> hoja de ruta del sistema de archivos: [docs/roadmap-fs.md](./docs/roadmap-fs.md);
> hoja de ruta de controladores y enclaves: [docs/roadmap-driver.md](./docs/roadmap-driver.md).

---

## Versión

Versión actual **`0.4.0`**; la variante publicada sin gráficos está etiquetada como **`v0.4.0-nogui`**.

- La **única fuente de verdad** para las constantes de versión es `kernel/src/version.rs` del kernel (`SYSTEM_NAME` / `VERSION` / `MACHINE` / `VARIANT` / `BUILD`), que se informa al espacio de usuario mediante `SYS_UNAME(51)` — los comandos `uname` / `version` del shell la leen (el número de compilación es el hash corto de git inyectado por el `Makefile`).
- Dos variantes de compilación:
  - **Normal (con gráficos)**: `make ...`; la salida del shell se **duplica** además en la consola de pantalla de `gfx_srv`.
  - **Sin gráficos**: `make NOGUI=1 ...`; el shell no abre el espejo de pantalla (la entrada/eco van por el puerto serie), la cadena de versión informa `0.4.0-nogui` y los artefactos se generan en `build/nogui/`. Todos los demás servicios y la línea base completa de regresión permanecen sin cambios.
- Consulte [CHANGELOG.md](./CHANGELOG.md) para el historial de cambios.

---

## Visión general de la arquitectura (objetivo)

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

## Diseño central

### Primitivas del microkernel

Contiene solo el conjunto mínimo irreducible:

| Primitiva | Descripción |
|-----------|-------------|
| `send` / `receive` / `call` | IPC síncrona/asíncrona con transferencia de capacidades |
| `map` / `unmap` | Gestión del mapeo de espacios de direcciones |
| `create_domain` / `destroy_domain` | Ciclo de vida de dominios de protección (procesos) |
| `schedule` | Planificación de CPU |
| `allocate_frame` / `free_frame` | Gestión de marcos de memoria física |
| `register_interrupt` / `ack_interrupt` | Autorización y confirmación de interrupciones |
| `create_enclave` | Creación de enclaves aislados por hardware |

### Paginador externo

- El kernel solo captura y reenvía los page faults; los servicios paginadores de espacio de usuario deciden el contenido y la política de reemplazo
- Cada proceso puede designar un paginador dedicado — paginación bajo demanda, pools de memoria comprimida, almacenamiento en red, etc.

### Controladores de doble modo Anykernel

| Modo | Destino | Características |
|------|--------|-----------------|
| **Proceso de servicio de controlador** | Aplicaciones generales | Compartición de dispositivos, aislamiento seguro, acceso indirecto vía IPC |
| **Biblioteca de controlador directo (LibDevice)** | Juegos / IA | Enlazada en tiempo de ejecución, cero traps del kernel para operaciones MMIO/DMA |

Una única interfaz de trait de Rust, selección de backend mediante feature flags de compilación, >90 % de código compartido.

### Performance Enclave

1. IOMMU mapea las ventanas MMIO y DMA del dispositivo en el espacio de direcciones del proceso
2. Se enlaza la biblioteca de controlador directo LibDevice
3. Los comandos de GPU/NPU se envían directamente sin intervención del kernel

El radio de impacto queda confinado por hardware a los límites de recursos del enclave.

### Modelo de seguridad basado en capacidades

- Las capacidades como única credencial de acceso, sin dependencia de UID/GID
- Los procesos nuevos comienzan con cero capacidades, concedidas explícitamente por el padre
- Las API de permisos POSIX (`chmod`/`chown`) se traducen a operaciones de capacidades mediante libc
- La política de seguridad la interpreta un motor de políticas en espacio de usuario, con soporte de actualizaciones dinámicas

### Servicios en espacio de usuario

Toda la funcionalidad tradicional del kernel se ejecuta como procesos independientes de espacio de usuario:

| Servicio | Responsabilidad | Estado |
|---------|---------------|--------|
| Servicio de sistema de archivos | FAT32 (incl. nombres largos VFAT), tmpfs, MorionFS original, ext2 de solo lectura, exFAT (lectura + escritura), unificados mediante libvfs | ✅ Implementado (ext2 de solo lectura / exFAT lectura-escritura) |
| Servicio de dispositivos | Dispositivo de bloque (controlador NVMe), controlador de teclado, despacho de interrupciones | ✅ Parcial |
| Servicio de shell | Intérprete de línea de comandos + árbol de directorios unificado / montaje en tiempo de ejecución | ✅ Implementado |
| Pila de red | TCP/IP en espacio de usuario, memoria compartida de copia cero | ⏳ Planificado |
| Seguridad / Auditoría | Autenticación, motor de políticas, detección de intrusiones | ⏳ Planificado |
| Gestor de enclaves | Ciclo de vida de enclaves, flujos de logs, migración y suspensión | ⏳ Planificado |
| Gestor de paquetes | Compilaciones declarativas estilo Nix, conmutación atómica, reversión de versiones | ⏳ Planificado |
| Servicio de GUI | Escritorio translúcido acrílico, altamente personalizable (`gfx_srv` framebuffer / consola de texto / primitivas de dibujo implementados, entorno de escritorio no iniciado) | 🚧 En progreso |
| Audio / IME / Contenedor / Hora / Energía / Log / Config | Servicios de infraestructura del sistema | ⏳ Planificado |
| Registro de capacidades de IA / servicio de puerta de enlace | Registro y descubrimiento de capacidades de aplicaciones, autorización y auditoría de llamadas de IA | 📐 Especificación definida (véase [app-dev-guide.md](./docs/app-dev-guide.md) §9) |

### Virtualización

- El microkernel actúa también como Hypervisor (Intel VT-x / AMD-V)
- Admite unikernels y guests Linux/Windows sin modificar
- Passthrough de dispositivos PCIe (VT-d / IOMMU), enclaves anidados

### Arranque

- UEFI nativo, sin transiciones heredadas a modo real
- Menú de arranque GOP de alta resolución, tema acrílico
- Entradas de arranque basadas en closures de Nix, conmutación atómica y reversión
- Medición TPM 2.0 + verificación de Secure Boot
- Arranque en caliente kexec, coexistencia de múltiples sistemas operativos

---

## Estructura del repositorio

### Estructura actual

El proyecto es un workspace de Rust (`Cargo.toml` en la raíz) que actualmente contiene los crates `boot`, `kernel`, `user/srv`, `user/libmorion`, `user/libdevice`, `user/hello` y `kernel_test`. Cada servicio del sistema fuera del kernel (dispositivo de bloque / sistema de archivos / montaje / shell / controlador de teclado, …) es un **programa independiente de espacio de usuario** (un `[[bin]]` por servicio en `user/srv` → un ELF autónomo); el **cargador de arranque los lee desde la ESP (`EFI/morion/services/`) al arrancar** y los pasa al kernel mediante la tabla de módulos de `BootInfo`, tras lo cual el kernel carga cada uno en su dominio fijo (E2b: ya no es un único binario plano despachado por id de dominio; E3b: los servicios ya no están embebidos en la imagen del kernel). Los recursos de interfaz se organizan por propósito: recursos de arranque en `boot/loader/resources/`, recursos de todo el sistema en `resources/system/`.

```
.
├── .github/
│   ├── workflows/            # Acciones de GitHub (sincronización automática a Gitee)
│   ├── ISSUE_TEMPLATE/       # Plantillas de issues
│   └── PULL_REQUEST_TEMPLATE.md
├── boot/                     # Cargador de arranque UEFI (morion-boot)
│   ├── asm/
│   │   └── boot_stub.asm     #   Stub de ensamblador de entrada de arranque
│   ├── loader/               # Recursos de arranque y configuración
│   │   ├── entries/          #   Entradas de arranque (.conf)
│   │   │   └── morion.conf
│   │   ├── resources/        #   Recursos del tema del cargador (BMP/PNG)
│   │   │   ├── animation/    #     Fotogramas de animación de carga
│   │   │   ├── background/   #     Fondos (dark/light/default/mask)
│   │   │   ├── icons/        #     Iconos de categoría (dialog/power/security/system/ui)
│   │   │   ├── logo/         #     Variantes del logo (horizontal/monochrome/square/system)
│   │   │   ├── progress/     #     Barra de progreso (bar_bg/bar_fill)
│   │   │   └── splash/       #     Pantalla de bienvenida (background/logo)
│   │   ├── kernel_placeholder.bin
│   │   ├── loader.conf       #   Configuración del cargador de arranque
│   │   └── theme.toml        #   Configuración del tema acrílico
│   └── src/
│       ├── boot/             #   Flujo de arranque (loader/menu/kexec)
│       ├── config/           #   Análisis de configuración (entries/theme)
│       ├── gfx/              #   Renderizado gráfico (framebuffer/font/renderer/animation)
│       ├── security/         #   Seguridad (hash/secure_boot/tpm)
│       ├── lib.rs
│       └── main.rs
├── kernel/                   # Microkernel (morion-kernel, TCB mínimo)
│   └── src/
│       ├── arch/             #   x86_64 (gdt/idt/pic/pit/keyboard/pci)
│       ├── memory/           #   Gestión de memoria (paging/frame_allocator)
│       ├── scheduler/        #   Planificador (conmutación de contexto)
│       ├── video/            #   Consola de texto del kernel (framebuffer/font/logo/bg/unicode) — arranque y salida de panic
│       ├── bootinfo.rs       #   Información de arranque (mapa de memoria + framebuffer GOP)
│       ├── cap.rs            #   Sistema de capacidades (slots + tabla de handles)
│       ├── domain.rs         #   Dominios de protección (procesos, tablas de páginas por dominio)
│       ├── elf.rs            #   Análisis y validación de ELF64 (límite de confianza para la carga de ejecutables)
│       ├── exec.rs           #   Cargar ELF en tiempo de ejecución → crear dominio → mapear → iniciar tarea
│       ├── ipc.rs            #   Comunicación entre procesos
│       ├── irq.rs            #   Enrutamiento de interrupciones (interrupciones como IPC)
│       ├── device.rs         #   Concesión genérica de dispositivo (BAR / DMA / MSI-X → DeviceGrant)
│       ├── pager.rs          #   Interfaz del paginador de espacio de usuario
│       ├── syscall.rs        #   Entrada de syscalls y tabla de IDs
│       ├── lib.rs
│       └── main.rs
├── user/                     # Espacio de usuario: runtime + bibliotecas comunes de controladores + servicios
│   ├── libmorion/            #   Runtime (crate `morion`): syscall / print / libvfs / libgfx / código de entrada
│   ├── libdevice/            #   Biblioteca común de controladores (crate `libdevice`, D2/D2b): concesión de dispositivo / MMIO / MSI-X / transporte virtio + vring
│   ├── hello/                #   Demo: un **programa ELF autónomo** (cargado en tiempo de ejecución por SYS_SPAWN_ELF)
│   └── srv/                  #   Servicios del sistema (crate `morion-srv`): un [[bin]] por servicio → un ELF autónomo
│       └── src/
│           ├── common.rs     #     Protocolo de enlace / cliente de bloque / helpers de nombres compartidos por los servicios
│           ├── block_srv.rs  #     dominio 5  servicio de controlador de dispositivo de bloque
│           ├── fat32_srv.rs  #     dominio 6  servicio de sistema de archivos FAT32
│           ├── app.rs        #     dominio 7  programa de autotest
│           ├── shell.rs      #     dominio 8  línea de comandos
│           ├── mount_srv.rs  #     dominio 9  servicio de montaje
│           ├── tmpfs_srv.rs  #     dominio 10 sistema de archivos en memoria
│           ├── mfs_srv.rs    #     dominio 11 MorionFS
│           ├── ext2_srv.rs   #     dominio 12 ext2 de solo lectura
│           ├── exfat_srv.rs  #     dominio 13 exFAT lectura-escritura
│           ├── init.rs       #     dominio 14 supervisor (vigila los dominios de servicio, reinicia en el mismo lugar desde la imagen en memoria)
│           ├── gfx_srv.rs    #     dominio 15 servicio gráfico (posee el framebuffer, renderizado en espacio de usuario: primitivas + terminal de texto)
│           ├── net_srv.rs    #     dominio 16 controlador de red (virtio-net; N0–N3: interrupciones MSI-X + autotest ARP)
│           ├── virtio_blk_srv.rs  # dominio 17 controlador de bloque virtio-blk (D3: concesión genérica, autotest de firma de lectura/relectura tras escritura)
│           ├── gfx/          #     interior del servicio gráfico: vista de framebuffer + fuentes (font/glyphs/cjk.bin) + terminal
│           ├── sender.rs / receiver.rs / pager.rs / echo.rs / kbd.rs  # dominios 0..4 demos y teclado
│           └── bin/          #     18 puntos de entrada (cada uno escribe morion_main → run() de su módulo)
├── kernel_test/              # Kernel de pruebas de integración de arranque temprano (conservado temporalmente)
│   └── src/main.rs
├── resources/
│   └── system/               # Recursos de todo el sistema
│       ├── device/           #   Iconos de dispositivos (.ico)
│       ├── file/             #   Iconos de tipos de archivo (.ico)
│       ├── github/           #   Portada de GitHub (.png)
│       ├── icons/            #   Iconos generales de interfaz (.ico)
│       ├── logo/             #   Logo del sistema (.ico/.svg/.png)
│       ├── service/          #   Iconos de servicios (.ico)
│       └── terminal/         #   Fondos de terminal (.raw)
├── docs/
│   ├── architecture.md       #   Diseño de la arquitectura técnica
│   ├── app-dev-guide.md      #   Guía de desarrollo de aplicaciones (incl. especificación de capacidades invocables por IA)
│   ├── dev-reference.md      #   Referencia rápida del kernel y de las interfaces
│   ├── commands.md           #   Comandos de compilación / ejecución / verificación
│   ├── shell-reference.md    #   Referencia de uso del shell
│   └── roadmap-fs.md         #   Hoja de ruta del sistema de archivos
├── Cargo.toml                # Workspace de Rust (boot/kernel/user/kernel_test)
├── Cargo.lock
├── Makefile                  # Sistema de compilación (make iso/run/run-nvme/debug/check/clippy)
├── flake.nix                 # Integración de compilación con Nix
├── rust-toolchain.toml       # Toolchain nightly de Rust
├── linker.ld                 # Script de enlazado del kernel
├── .gitattributes
├── .gitignore
├── CONTRIBUTING.md
├── LICENSE
├── README.md                 # Inglés (principal)
├── README.zh-CN.md           # 简体中文
├── README.ja.md              # 日本語
├── README.ko.md              # 한국어
├── README.es.md              # Español
├── README.fr.md              # Français
├── README.de.md              # Deutsch
└── README.ru.md              # Русский
```

### Estructura de desarrollo objetivo

```
├── bootloader/       # Cargador de arranque (UEFI)
├── kernel/           # Código fuente del kernel
│   ├── arch/         #   Específico de arquitectura (x86_64 / AArch64 / RISC-V)
│   ├── core/         #   Núcleo del microkernel (IPC, planificador, espacio de direcciones, caps)
│   └── compat/       #   Capas de compatibilidad (POSIX / Linux / RTOS)
├── services/         # Servicios del sistema en espacio de usuario
│   ├── fs/           #   Servicio de sistema de archivos
│   ├── net/          #   Pila de red
│   ├── device/       #   Servicio de dispositivos + controladores
│   ├── security/     #   Seguridad / autenticación / auditoría
│   ├── enclave/      #   Gestor de enclaves
│   ├── gui/          #   Servicio de GUI
│   ├── shell/        #   Servicio de shell
│   ├── audio/        #   Servicio de audio
│   ├── ime/          #   Servicio de IME
│   └── ...           #   Más servicios
├── userland/         # Espacio de usuario
│   ├── libs/         #   libc, libvfs, libdevice, etc.
│   └── bin/          #   Comandos básicos (ls, cat, mkdir, rm)
├── resources/        # Archivos de recursos
├── docs/             # Documentación
└── pkg/              # Gestión de paquetes (estilo Nix)
```

---

## Hoja de ruta

> Hoja de ruta detallada del sistema de archivos: [docs/roadmap-fs.md](./docs/roadmap-fs.md); hoja de ruta del subsistema gráfico: [docs/roadmap-gfx.md](./docs/roadmap-gfx.md). Los elementos marcados están **verificados en ejecución bajo QEMU**.

### Fase 1 — Núcleo del microkernel (básicamente completa)

- [x] Creación de dominios de protección (procesos) y aislamiento de espacios de direcciones
- [x] IPC síncrona/asíncrona (`send` / `recv` / `call` / `reply`; carga útil de 96 bytes + memoria compartida para datos masivos)
- [x] Planificación de tareas y conmutación de contexto
- [x] map/unmap de espacios de direcciones + paginador en espacio de usuario (paginación bajo demanda)
- [x] Enrutamiento de interrupciones ("interrupciones como IPC") + concesiones de MMIO / puertos de E/S
- [x] Sistema de capacidades (slots + handles: emisión / verificación / revocación)
- [x] **Transferencia de capacidades por IPC** (transferencia de handle `SYS_HANDLE_SEND`: mover un objeto abierto **hacia** el dominio destino, semántica move; delegación de capacidades `SYS_CAP_SEND`: **copiar** una capacidad poseída, sin amplificación; ambas requieren `SendTo(to)`). Los dominios 0/1/3 cubren 8 casos de autotest positivos/negativos en el arranque

### Fase 2 — Servicios fundamentales (en progreso)

- [x] Enumeración PCI + controlador NVMe en espacio de usuario (servicio de dispositivo de bloque, con respaldo IDE PIO)
- [x] **Análisis de particiones MBR/GPT + capa de volúmenes** (dentro de block_srv, `dev` = número de volumen; tipo de FS sondeado por la firma inicial de cada volumen)
- [x] **Montaje multi-volumen**: los 32 bits altos de la etiqueta de solicitud transportan el código de volumen, un open vincula un volumen; cada servicio de FS informa automáticamente sus volúmenes adicionales (montados en `/usb<vol>`); búfer de clúster de fat32 2 → 16 páginas (techo de clúster de **64 KiB**), límite de grupos de bloques de ext2 16 → 4096
- [x] Sistemas de archivos: FAT32 (incl. nombres largos VFAT) / tmpfs / MorionFS original (COW + snapshots)
- [x] Compatibilidad ext2 **de solo lectura** (monta particiones Linux existentes)
- [x] Compatibilidad exFAT (`exfat_srv` dominio 13, montado en `/usb`): **solo lectura** + **lectura-escritura** (`CREAT/WRITE/MKDIR/UNLINK/RMDIR/TRUNCATE`) + **volúmenes grandes** (DMA multi-página, comando único ≤ 128 KiB)
- [x] libvfs + capa de montaje: varios sistemas de archivos ensamblados en una única raíz `/`, con soporte de montaje en tiempo de ejecución
- [x] Shell (builtins + rutas relativas al cwd) y controlador de teclado en espacio de usuario
- [x] **Formato MorionFS v2 finalizado + recuperación de espacio / GC** (bitmap de libres + mark & sweep)
- [x] **Archivos grandes en MorionFS** (`MFS3`: bloques indirectos, rompiendo el antiguo límite de ≈4 MiB)
- [x] **Directorios y nombres largos en MorionFS** (`MFS4`: dirents de longitud variable + bloques de extensión, nombres ≤255 bytes)
- [x] **Metadatos de nodo en MorionFS** (`MFS5`: marcas de tiempo / permisos / propietario / contador de enlaces, `rename` / `truncate` / `chmod`)
- [x] **Indirección de números de inodo + enlaces duros en MorionFS** (`MFS6`)
- [x] **Enlaces simbólicos en MorionFS** (`MFSL`: resolución de rutas con seguimiento + protección contra bucles, `readlink` / `lstat` / `ln -s`)
- [x] **Formateo de MorionFS según la geometría del volumen** (**M7**)
- [x] **Formato explícito + multi-volumen en MorionFS** (**M8 / S2**: `mkfs.mfs <vol>`, la protección solo acepta volúmenes en blanco o MFS)
- [x] **Escalado de capacidad de MorionFS (bitmap externalizado)** (**S3a**: techo de capacidad ≈119 MiB → **≈127.25 GiB**)
- [x] **Archivo único de más de 4 GiB en MorionFS** (**S3b**: `offset`/`size` u64 de VFS de extremo a extremo + bloque indirecto de tercer nivel `MFI3`)
- [x] **Conmutación de volumen primario en MorionFS** (**S2**: el superbloque almacena el número de serie del volumen primario; gana el número de serie más alto)
- [x] **Reapuntar el volumen primario solo mediante marcador en MorionFS** (**S2**: `mfs.primary <vol>` sin tocar los datos)
- [x] **Uso de espacio `df`** (MorionFS informa total / usado / libre)
- [x] **Cierre de la gestión de volúmenes: escritura de tablas de particiones** (**S2**: `part.create/del/wipe/reload`, GPT + MBR, verificado contra `sgdisk -v` del host)
- [x] **Interrupciones NVMe (MSI/MSI-X)** (**S3**: soporte mínimo de LAPIC + recorrido de capacidades PCI + segmento de vectores MSI `0x50..0x5F`)
- [x] **Espera bloqueante de interrupción (primitiva de espera)** (**S4**: bloqueo acotado por timeout + `SYS_IRQ_WAIT`)
- [x] **Multi-vector + `wait_any`** (**S5**: clave de espera por dominio + máscara de vectores, admin + 2 colas de E/S cada una con su propio vector)
- [x] **Renderizado CJK en terminal (CJK / ancho completo / ancho mixto)** (el lado del kernel se eliminó posteriormente en G3c, la fuente se trasladó al espacio de usuario `gfx_srv`)
- [x] **Carga de ejecutables (ELF + spawn en tiempo de ejecución)** (**E1**: cargador ELF64 + `SYS_SPAWN_ELF`; la demo `user/hello` es un ELF autónomo)
- [x] **Runtime de espacio de usuario libmorion + comando `run`** (**E2a**)
- [x] **Servicios divididos en programas autónomos + destrucción de dominios / recuperación de marcos** (**E2b**: 14 servicios, un ELF cada uno; `SYS_DOMAIN_DESTROY/COUNT/FRAME_FREE`)
- [x] **Cierre del ciclo de vida de servicios** (**E3a/E3b/E3c**: W^X de páginas de usuario; servicios sacados de la imagen del kernel; supervisor `init` + reinicio en el mismo lugar desde la imagen en memoria, sin dependencia del disco)
- [x] **Gráficos G1 (framebuffer trasladado al espacio de usuario)**
- [x] **Gráficos G2 (primitivas de dibujo + superficies compartidas + `libmorion::gfx`)**
- [x] **Gráficos G3a (terminal en servicio + renderizado de texto trasladados fuera)**
- [x] **Gráficos G3b (salida del shell en pantalla)**
- [x] **Gráficos G3c (el kernel elimina la fuente CJK, −276 KB)**
- [x] **Gráficos G4 (entrada trasladada fuera del kernel)**
- [x] **Gráficos G6 (autorreparación de gfx_srv: reinicio supervisado + reconstrucción de sesión del cliente)**
- [ ] **Más compatibilidad de sistemas de archivos** (escritura ext4, UDF, etc.)
- [ ] Pila de red

### Fase 3 — Enclaves de rendimiento (en progreso)

- [x] **Passthrough IOMMU** (**E1a** sonda DMAR de ACPI / **E1b** tabla raíz + tabla de contexto + tablas de páginas de segundo nivel de identidad y `GCMD.TE` / **E1c** ventana del dispositivo objetivo estrechada a `[0, 3 GiB)`, DMA de dispositivo fuera de ventana rechazado por el IOMMU con evidencia — todo hecho, véase [docs/roadmap-driver.md](./docs/roadmap-driver.md))
- [ ] **Forma de passthrough de LibDevice y gestor de enclaves** (E2/E3 pendientes; la biblioteca común `user/libdevice` ya está en su sitio)

### Fase 4 — Redes y seguridad (en progreso)

- [x] **Controlador de NIC (virtio-net)** (`net_srv` dominio 16 + concesión genérica de dispositivo, véase [docs/roadmap-driver.md](./docs/roadmap-driver.md) N0–N3: bring-up de virtio moderno + RX por interrupciones MSI-X + autotest ARP de extremo a extremo)
- [x] **Segundo controlador real (virtio-blk)** (`virtio_blk_srv` dominio 17, también mediante concesión genérica de dispositivo, sin lógica de kernel específica del dispositivo: D3 — autotest de firma de lectura / relectura tras escritura)
- [x] **Concesión de dispositivo en tiempo de ejecución (D1b)** (`SYS_DEVICE_INFO` / `SYS_DEVICE_GRANT` + puerta de capacidad `Mmio`; NVMe / `net_srv` / `virtio_blk_srv` obtienen un `DeviceGrant` mediante syscall en tiempo de ejecución, comportamiento sin cambios)
- [ ] Pila TCP/IP, auditoría de capacidades, motor de políticas

### Fase 5 — GUI y ecosistema (no iniciada)

- [ ] Entorno de escritorio, gestor de paquetes, virtualización

### Transversal — Interfaz de capacidades del sistema de IA (especificación definida, implementación no iniciada)

- [x] Especificación de diseño: cómo las aplicaciones anotan / describen funciones como **capacidades invocables por IA** (véase [docs/app-dev-guide.md](./docs/app-dev-guide.md) §9)
- [ ] Servicio de registro y descubrimiento de capacidades
- [ ] Puerta de enlace de llamadas de IA (verificaciones de capacidades / registro de auditoría / timeouts y cuotas)

---

## Licencia

Este proyecto utiliza **licencias por módulo**:

- Núcleo (`boot/`, `kernel/`, `user/`): [MIT](./LICENSE)
- Módulo de enclave (`enclave/`): [Apache-2.0](./LICENSES/Apache-2.0.txt), incluye una concesión expresa de patentes y terminación por litigio de patentes
- Controladores / firmware de terceros: conservan sus licencias originales; véase [docs/licensing.md](./docs/licensing.md)

Redacción canónica: **El núcleo de Morion OS tiene licencia MIT; el módulo de enclave es Apache-2.0; los controladores y el firmware de terceros conservan sus propias licencias.** El repositorio no tiene una única licencia MIT.

---

## Contacto

- Página principal del proyecto: [github.com/sheng-fs/morion-os](https://github.com/sheng-fs/morion-os)
- Correo electrónico: 3555679134@qq.com
