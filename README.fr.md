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

## Vue d'ensemble

Morion OS est un système d'exploitation moderne développé de zéro en **Rust**. Le projet se trouve actuellement dans une **phase de refonte et de réécriture** : les conflits architecturaux des implémentations précédentes ont été réexaminés en profondeur et l'**architecture hybride micro-noyau + exo-noyau** a été rétablie comme orientation technique centrale.

Le code historique a été archivé (branche `legacy`) et la branche principale a redémarré. Le projet est depuis passé d'une conception pure à un **prototype de micro-noyau exécutable** : il démarre sous QEMU (UEFI) jusqu'à un shell en espace utilisateur et parcourt l'intégralité du chemin de lecture/écriture de fichiers — « application → libvfs → service de système de fichiers → service de périphérique bloc → disque NVMe ».

### Principes fondamentaux

- **Base de confiance de calcul du micro-noyau** : le noyau n'expose qu'une seule frontière — l'interface d'appels système ; les systèmes de fichiers, le réseau, les pilotes de périphériques, le graphisme, le shell et toutes les autres fonctions traditionnellement assurées par le noyau sont implémentés comme services en espace utilisateur. Seule une poignée de primitives essentielles résident dans le noyau (IPC, mappage d'espace d'adressage, domaines de protection, capacités) — et même au sein de la table complète des appels système 0–63, celles-ci ne représentent qu'une petite partie (voir [docs/dev-reference.md](./docs/dev-reference.md) §5).
- **Chemin de performance exo-noyau** : grâce au mécanisme « Performance Enclave », les capacités matérielles IOMMU / CHERI permettent aux applications hautes performances (jeux, IA) d'exploiter directement le matériel — zéro piège noyau, zéro copie de données.
- **Modèle de sécurité fondé sur les capacités** : abandonne les systèmes de permissions UID/GID traditionnels. Les capacités constituent l'unique justificatif d'accès, éliminant fondamentalement les vulnérabilités du type « root peut tout faire ».
- **Pilotes bi-mode Anykernel** : le même code de pilote se compile soit en processus de service en espace utilisateur (scénario partagé), soit en bibliothèque en accès direct (scénario haute performance), avec plus de 90 % de code commun.

> Conception détaillée de l'architecture : [docs/architecture.md](./docs/architecture.md).

---

## État actuel

Le système est passé des documents de conception à un **prototype de micro-noyau qui démarre et s'exécute sur du matériel réel / QEMU**. Le tableau ci-dessous distingue ce qui « fonctionne » de ce qui « n'est pas commencé », afin de ne pas confondre les objectifs de conception avec les capacités existantes.

| Domaine | État | Remarques |
|------|--------|-------|
| Cœur du micro-noyau | ✅ Fonctionnel | Domaines de protection, IPC synchrone/asynchrone, ordonnancement préemptif (y compris **blocage à durée limitée**), espaces d'adressage et pagination à la demande, routage des interruptions (PIC + **LAPIC/MSI-X**, y compris **attentes d'interruption bloquantes** et **`wait_any` multi-vecteurs**), système de capacités (y compris **transfert de capacités via IPC**) |
| Démarrage sur matériel réel / alimentation | ✅ Fonctionnel | Le chargeur UEFI localise le disque de démarrage à partir du Device Path du volume d'amorçage (`BootInfo` v5) ; la variante **`SAFE` n'expose que les contrôleurs bloc de la chaîne du disque de démarrage** (les contrôleurs USB ne sont jamais filtrés) ; la commande shell `poweroff` utilise ACPI S5 (FADT + DSDT `_S5`), `reboot` se rabat sur i8042 → `0xCF9` → triple fault ; l'absence de contrôleur PS/2 est signalée explicitement |
| Interface d'appels système | ✅ 56 (ID 0–63) | Table complète des ID du noyau : [docs/dev-reference.md](./docs/dev-reference.md) §5 ; sous-ensemble destiné aux applications : [docs/app-dev-guide.md](./docs/app-dev-guide.md) §3 |
| Modèle de sécurité par capacités | ✅ Fonctionnel | Slots de capacités + **handles de capacités** (émis à l'ouverture, vérifiés avant chaque E/S, révoqués à la fermeture), zéro capacité par défaut ; **transfert de handle** (move) et **délégation de capacité** (copie, sans amplification) à l'exécution via IPC — tout n'a pas besoin d'être accordé statiquement au démarrage |
| Pilotes en espace utilisateur | ✅ Partiel | Pilote clavier (IRQ1 / détection de présence PS/2 + clavier USB HID en mode boot) ; service de périphérique bloc (pilote NVMe, avec repli IDE PIO) ; **xHCI multi-contrôleur** (jusqu'à 4 contrôleurs, stockage USB + clavier USB HID) ; pilote réseau **`net_srv` (virtio-net, domaine 16)** — le pilote N2/N3 fonctionne (bring-up virtio moderne : analyse des capacités virtio / lecture de la MAC / construction des virtqueues RX·TX / `DRIVER_OK` / **RX piloté par interruptions MSI-X**, et sa requête ARP auto-initiée reçoit une réponse de la passerelle `NET1 … ARP reply OK`). **Octroi générique de périphérique (D1)** : le module noyau `device.rs` délivre un `DeviceGrant` (BAR + bloc DMA contigu + paramètres MSI-X, sans sémantique de périphérique) ; le noyau **ne contient plus de code spécifique NVMe** — la disposition des files et le protocole du périphérique résident dans le domaine du pilote ; un pilote peut aussi analyser les capacités PCI de son propre périphérique via `SYS_DEVICE_CONFIG_READ`, et le noyau mappe la table MSI-X depuis une BAR différente selon `BIR` |
| Systèmes de fichiers en espace utilisateur | ✅ Partiel | FAT32 (y compris noms longs VFAT), tmpfs, MorionFS v2 original (COW + snapshots + bitmap libre/récupération d'espace + blocs indirects pour les gros fichiers + dirents de longueur variable/noms longs + métadonnées de nœud + couche d'indirection des numéros d'inode/liens physiques/liens symboliques + **formatage selon la géométrie du volume** + **`mkfs.mfs` explicite, multi-volume et basculement de volume principal**), ext2 **en lecture seule**, exFAT (lecture + écriture, volumes volumineux et à nombre élevé de clusters) |
| Couche partitions / volumes | ✅ Fonctionnel | block_srv analyse la table de partitions **MBR/GPT** de chaque disque en une table de volumes et déduit le type de FS d'après la signature de tête de chaque volume ; il peut aussi **écrire les tables de partitions** (`part.create/del/wipe/reload` : créer/supprimer des partitions, effacer, rebalayer — GPT comme MBR) ; `dev` est désormais un « numéro de volume » et la couche bloc prend en charge le DMA multi-pages (commande unique ≤ 128 KiB) ; **montage multi-volume** : les volumes supplémentaires de même type sont montés automatiquement sous `/usb<vol>`, une même base de code dessert donc plusieurs disques — ouvrant la voie à la lecture de vraies partitions USB |
| Shell et arborescence de répertoires unifiée | ✅ Fonctionnel | `help/echo/uname/version/pwd/ls/cat/run/wget/net/ping/ping6/dns/wifi/lspci/dmesg/cd/mkdir/touch/rm/mv/ln/ln -s/chmod/chown/truncate/stat/lstat/readlink/mkfs.mfs/mfs.primary/mfs.snap/mfs.snaps/mfs.rollback/mfs.gc/mfs.fsck/mfs.sync/df/part.create/part.del/part.wipe/part.reload/clear/poweroff/reboot` (format long `ls -l`, liens symboliques affichés comme `l` ; `dmesg` prend en charge `-d/-a/-g/path` et `Ctrl+C` ; `uname`/`version` rapportent la chaîne de version via `SYS_UNAME`) ; plusieurs systèmes de fichiers sont assemblés en une racine `/` unique par la couche de montage, avec montage à l'exécution |
| Graphismes / GUI | 🚧 En cours | **G1** framebuffer confié exclusivement à `gfx_srv` en espace utilisateur (domaine 15) : ajout de `Capability::Fb` + `SYS_FB_INFO/MAP/TAKEOVER` ; après la prise en charge, le terminal du noyau n'écrit plus à l'écran (la sortie reste sur COM1). **G2** primitives de dessin `fill/rect/blit` + surfaces partagées + bibliothèque client `libmorion::gfx` ; `blit` relit le framebuffer et vérifie avant de répondre (autotest GS-1). **G3a** rendu de texte déplacé : la police (ASCII 8×16 + CJK 16×16 `cjk.bin` ≈276 KB) et l'état du terminal passent du noyau à `gfx/`, nouveau protocole `GFX_OP_TEXT/CLEAR/MOVE/QUERY`, vérification par écriture puis relecture pixel par pixel (autotest GT-1). **G3b** sortie du shell à l'écran : `SYS_CONSOLE_READY(47)` ; le puits d'impression `sink()` de `libmorion` peut **recopier** la sortie vers la console écran par processus (seul le shell l'active). **G3c** le noyau abandonne la police CJK (−276 KB, ELF du noyau 347 KB → 69 KB) ; le terminal du noyau se rabat sur ASCII + tofu et le rendu CJK n'a lieu qu'en espace utilisateur. **G4** entrée déplacée hors du noyau : file d'octets de touches `SYS_KEY_PUSH(48)/SYS_KEY_READ(49)` (le noyau ne fait que transporter des octets), édition de ligne/écho dans la bibliothèque client `morion::console::readline`. **G6** auto-rétablissement du service : `ipc::call` ne bloque plus indéfiniment (timeout + échec rapide lorsque la cible n'a aucune tâche vivante), framebuffer enregistré comme plage noyau réservée, requêtes de boîte aux lettres obsolètes écartées au redémarrage, le client reconstruit les sessions partagées, `gfx_srv` supervisé par init (autotest GS-2). La console texte du noyau ne reste que pour la sortie de démarrage et de panic |
| Réseau / virtualisation / enclaves / gestionnaire de paquets | 🚧 En cours | **Réseau** : le pilote virtio-net `net_srv` fonctionne ; la pile est scindée en un `netstack_srv` dédié (domaine 21 : TCP/IP + service de sockets + porte de capacité par port), avec des deuxième/troisième NIC réelles `e1000e_srv`(22) / `e1000_srv`(24), le sans-fil `wifi_srv`(25) et un `httpd_srv`(23) dans l'invité. **Enclaves** : l'isolation matérielle IOMMU/VT-d est faite (depuis E1c, la fenêtre du périphérique cible est contrainte et le DMA hors fenêtre est refusé avec preuve) ; le gestionnaire d'enclaves E2/E3 n'est pas fait. **Virtualisation et gestion de paquets** : conception arrêtée, aucune implémentation pour l'instant |
| Interface de capacités pour l'IA système | 📐 Spécification définie | Comment une application expose ses fonctionnalités à l'IA système : [docs/app-dev-guide.md](./docs/app-dev-guide.md) §9 |

> Démarrage rapide (commandes de compilation et d'exécution) : [docs/commands.md](./docs/commands.md) ;
> référence du noyau et des interfaces : [docs/dev-reference.md](./docs/dev-reference.md) ;
> développement d'applications (y compris les capacités appelables par l'IA) : [docs/app-dev-guide.md](./docs/app-dev-guide.md) ;
> feuille de route des systèmes de fichiers : [docs/roadmap-fs.md](./docs/roadmap-fs.md) ;
> feuille de route des pilotes et enclaves : [docs/roadmap-driver.md](./docs/roadmap-driver.md).

---

## Version

Version actuelle **`0.4.0`** ; la variante sans interface graphique publiée est étiquetée **`v0.4.0-nogui`**.

- La **source unique de vérité** pour les constantes de version est `kernel/src/version.rs` dans le noyau (`SYSTEM_NAME` / `VERSION` / `MACHINE` / `VARIANT` / `BUILD`), rapportée à l'espace utilisateur via `SYS_UNAME(51)` — les commandes `uname` / `version` du shell la lisent (le numéro de build est le hash court git injecté par le `Makefile`).
- Deux variantes de compilation :
  - **Standard (avec graphismes)** : `make ...` ; la sortie du shell est en outre **recopiée** vers la console écran de `gfx_srv`.
  - **Sans graphismes** : `make NOGUI=1 ...` ; le shell n'ouvre pas le miroir écran (entrée/écho passent par le port série), la chaîne de version indique `0.4.0-nogui` et les artefacts atterrissent dans `build/nogui/`. Tous les autres services et l'intégralité de la base de référence de régression sont inchangés.
- Voir [CHANGELOG.md](./CHANGELOG.md) pour l'historique des modifications.

---

## Aperçu de l'architecture (cible)

```
┌──────────────────────────────────────────────────────┐
│                  Couche applicative                   │
│  Interface POSIX (libc)  |  API directe haute perf.   │
├──────────────────────────────────────────────────────┤
│        Services système en espace utilisateur         │
│ Système de fichiers │ Réseau │ Svc périph. │ Sécurité │
│ ext4/vfat  │    TCP/IP     │ Svc pilotes │ Auth/Audit │
├──────────────────────────────────────────────────────┤
│   Enclave de performance — accélération optionnelle   │
│ GPU direct  │  NPU direct  │  Pilote NIC utilisateur  │
│   (isolation via IOMMU, bornes protégées par CHERI)   │
├──────────────────────────────────────────────────────┤
│                      Micro-noyau                      │
│ IPC │ Ordonnancement │ Adressage │ Routage IRQ │ Caps │
└──────────────────────────────────────────────────────┘
```

---

## Conception centrale

### Primitives du micro-noyau

Contient uniquement l'ensemble minimal irréductible :

| Primitive | Description |
|-----------|-------------|
| `send` / `receive` / `call` | IPC synchrone/asynchrone avec transfert de capacités |
| `map` / `unmap` | Gestion du mappage d'espace d'adressage |
| `create_domain` / `destroy_domain` | Cycle de vie des domaines de protection (processus) |
| `schedule` | Ordonnancement CPU |
| `allocate_frame` / `free_frame` | Gestion des frames de mémoire physique |
| `register_interrupt` / `ack_interrupt` | Autorisation et acquittement des interruptions |
| `create_enclave` | Création d'enclave isolée matériellement |

### Pager externe

- Le noyau ne fait que capturer et transmettre les défauts de page ; les services de pagination en espace utilisateur décident du contenu et de la politique de remplacement
- Chaque processus peut désigner un pager dédié — pagination à la demande, pools de mémoire compressée, stockage réseau, etc.

### Pilotes bi-mode Anykernel

| Mode | Cible | Caractéristiques |
|------|--------|-----------------|
| **Processus de service de pilote** | Applications générales | Partage de périphérique, isolation sécurisée, accès indirect via IPC |
| **Bibliothèque de pilote directe (LibDevice)** | Jeu / IA | Liée à l'exécution, zéro piège noyau pour les opérations MMIO/DMA |

Interface unique sous forme de trait Rust, sélection du backend via des feature flags à la compilation, plus de 90 % de code partagé.

### Enclave de performance

1. L'IOMMU mappe les fenêtres MMIO et DMA du périphérique dans l'espace d'adressage du processus
2. La bibliothèque de pilote direct LibDevice est liée
3. Les commandes GPU/NPU sont soumises directement, sans aucune intervention du noyau

Le rayon d'impact est verrouillé matériellement aux limites des ressources de l'enclave.

### Modèle de sécurité fondé sur les capacités

- Les capacités comme unique justificatif d'accès, sans dépendance à UID/GID
- Les nouveaux processus démarrent sans aucune capacité, accordées explicitement par le parent
- Les API de permissions POSIX (`chmod`/`chown`) traduites en opérations sur capacités par la libc
- Politique de sécurité interprétée par un moteur de politiques en espace utilisateur, avec prise en charge des mises à jour dynamiques

### Services en espace utilisateur

Toutes les fonctionnalités traditionnelles du noyau s'exécutent comme des processus indépendants en espace utilisateur :

| Service | Responsabilité | État |
|---------|---------------|--------|
| Service de système de fichiers | FAT32 (y compris noms longs VFAT), tmpfs, MorionFS original, ext2 en lecture seule, exFAT (lecture + écriture), unifiés via libvfs | ✅ Implémenté (ext2 lecture seule / exFAT lecture-écriture) |
| Service de périphériques | Périphérique bloc (pilote NVMe), pilote clavier, distribution des interruptions | ✅ Partiel |
| Service shell | Interpréteur de ligne de commandes + arborescence unifiée / montage à l'exécution | ✅ Implémenté |
| Pile réseau | TCP/IP en espace utilisateur, mémoire partagée zéro-copie | ⏳ Prévu |
| Sécurité / Audit | Authentification, moteur de politiques, détection d'intrusion | ⏳ Prévu |
| Gestionnaire d'enclaves | Cycle de vie des enclaves, flux de journaux, migration et suspension | ⏳ Prévu |
| Gestionnaire de paquets | Builds déclaratifs de style Nix, basculement atomique, retour arrière de version | ⏳ Prévu |
| Service GUI | Bureau translucide acrylique, hautement personnalisable (`gfx_srv` framebuffer / console texte / primitives de dessin implémentés, environnement de bureau non commencé) | 🚧 En cours |
| Audio / IME / conteneur / temps / alimentation / journaux / configuration | Services d'infrastructure système | ⏳ Prévu |
| Service d'enregistrement / passerelle de capacités IA | Enregistrement et découverte des capacités applicatives, autorisation et audit des appels IA | 📐 Spécification définie (voir [app-dev-guide.md](./docs/app-dev-guide.md) §9) |

### Virtualisation

- Le micro-noyau fait aussi office d'hyperviseur (Intel VT-x / AMD-V)
- Prend en charge les unikernels et les invités Linux/Windows non modifiés
- Passthrough de périphériques PCIe (VT-d / IOMMU), enclaves imbriquées

### Démarrage

- UEFI natif, aucune transition legacy en mode réel
- Menu de démarrage haute résolution GOP, thème acrylique
- Entrées de démarrage fondées sur les closures Nix, basculement atomique et retour arrière
- Mesure TPM 2.0 + vérification Secure Boot
- Démarrage à chaud kexec, coexistence multi-OS

---

## Structure du dépôt

### Structure actuelle

Le projet est un workspace Rust (`Cargo.toml` racine) contenant actuellement les crates `boot`, `kernel`, `user/srv`, `user/libmorion`, `user/libdevice`, `user/hello` et `kernel_test`. Chaque service système hors du noyau (périphérique bloc / système de fichiers / montage / shell / pilote clavier, …) est un **programme en espace utilisateur distinct** (un `[[bin]]` par service dans `user/srv` → un ELF autonome) ; le **chargeur d'amorçage les lit depuis l'ESP (`EFI/morion/services/`) au démarrage** et les transmet au noyau via la table de modules `BootInfo`, après quoi le noyau charge chacun d'eux dans son domaine fixe (E2b : plus un binaire plat unique réparti par ID de domaine ; E3b : les services ne sont plus intégrés à l'image du noyau). Les ressources d'interface sont organisées par usage : ressources de démarrage sous `boot/loader/resources/`, ressources à l'échelle du système sous `resources/system/`.

```
.
├── .github/
│   ├── workflows/            # GitHub Actions (synchronisation automatique vers Gitee)
│   ├── ISSUE_TEMPLATE/       # Modèles de tickets
│   └── PULL_REQUEST_TEMPLATE.md
├── boot/                     # Chargeur d'amorçage UEFI (morion-boot)
│   ├── asm/
│   │   └── boot_stub.asm     #   Stub d'assemblage d'entrée de démarrage
│   ├── loader/               # Ressources et configuration de démarrage
│   │   ├── entries/          #   Entrées de démarrage (.conf)
│   │   │   └── morion.conf
│   │   ├── resources/        #   Ressources de thème du chargeur (BMP/PNG)
│   │   │   ├── animation/    #     Images de l'animation de chargement
│   │   │   ├── background/   #     Arrière-plans (dark/light/default/mask)
│   │   │   ├── icons/        #     Icônes de catégorie (dialog/power/security/system/ui)
│   │   │   ├── logo/         #     Variantes du logo (horizontal/monochrome/square/system)
│   │   │   ├── progress/     #     Barre de progression (bar_bg/bar_fill)
│   │   │   └── splash/       #     Écran de démarrage (background/logo)
│   │   ├── kernel_placeholder.bin
│   │   ├── loader.conf       #   Configuration du chargeur d'amorçage
│   │   └── theme.toml        #   Configuration du thème acrylique
│   └── src/
│       ├── boot/             #   Flux de démarrage (loader/menu/kexec)
│       ├── config/           #   Analyse de configuration (entries/theme)
│       ├── gfx/              #   Rendu graphique (framebuffer/font/renderer/animation)
│       ├── security/         #   Sécurité (hash/secure_boot/tpm)
│       ├── lib.rs
│       └── main.rs
├── kernel/                   # Micro-noyau (morion-kernel, TCB minimal)
│   └── src/
│       ├── arch/             #   x86_64 (gdt/idt/pic/pit/keyboard/pci)
│       ├── memory/           #   Gestion mémoire (paging/frame_allocator)
│       ├── scheduler/        #   Ordonnanceur (commutation de contexte)
│       ├── video/            #   Console texte du noyau (framebuffer/font/logo/bg/unicode) — sortie de démarrage et de panic
│       ├── bootinfo.rs       #   Informations de démarrage (carte mémoire + framebuffer GOP)
│       ├── cap.rs            #   Système de capacités (slots + table de handles)
│       ├── domain.rs         #   Domaines de protection (processus, tables de pages par domaine)
│       ├── elf.rs            #   Analyse et validation ELF64 (frontière de confiance pour le chargement d'exécutables)
│       ├── exec.rs           #   Charger un ELF à l'exécution → créer un domaine → mapper → démarrer une tâche
│       ├── ipc.rs            #   Communication inter-processus
│       ├── irq.rs            #   Routage des interruptions (interruptions comme IPC)
│       ├── device.rs         #   Octroi générique de périphérique (BAR / DMA / MSI-X → DeviceGrant)
│       ├── pager.rs          #   Interface de pagination en espace utilisateur
│       ├── syscall.rs        #   Point d'entrée des appels système et table des ID
│       ├── lib.rs
│       └── main.rs
├── user/                     # Espace utilisateur : runtime + bibliothèques communes de pilotes + services
│   ├── libmorion/            #   Runtime (crate `morion`) : appels système / affichage / libvfs / libgfx / code d'entrée
│   ├── libdevice/            #   Bibliothèque commune de pilotes (crate `libdevice`, D2/D2b) : octroi de périphérique / MMIO / MSI-X / transport virtio + vring
│   ├── hello/                #   Démo : un **programme ELF autonome** (chargé à l'exécution par SYS_SPAWN_ELF)
│   └── srv/                  #   Services système (crate `morion-srv`) : un [[bin]] par service → un ELF autonome
│       └── src/
│           ├── common.rs     #     Protocole de fil / client bloc / aides de nommage partagés par les services
│           ├── block_srv.rs  #     domaine 5  service de pilote de périphérique bloc
│           ├── fat32_srv.rs  #     domaine 6  service de système de fichiers FAT32
│           ├── app.rs        #     domaine 7  programme d'autotest
│           ├── shell.rs      #     domaine 8  ligne de commande
│           ├── mount_srv.rs  #     domaine 9  service de montage
│           ├── tmpfs_srv.rs  #     domaine 10 système de fichiers en mémoire
│           ├── mfs_srv.rs    #     domaine 11 MorionFS
│           ├── ext2_srv.rs   #     domaine 12 ext2 en lecture seule
│           ├── exfat_srv.rs  #     domaine 13 exFAT lecture-écriture
│           ├── init.rs       #     domaine 14 superviseur (surveille les domaines de service, redémarre sur place depuis l'image mémoire)
│           ├── gfx_srv.rs    #     domaine 15 service graphique (détient le framebuffer, rendu en espace utilisateur : primitives + terminal texte)
│           ├── net_srv.rs    #     domaine 16 pilote réseau (virtio-net ; N0–N3 : interruptions MSI-X + autotest ARP)
│           ├── virtio_blk_srv.rs  # domaine 17 pilote bloc virtio-blk (D3 : octroi générique, autotest lecture-signature/écriture-relecture)
│           ├── gfx/          #     internes du service graphique : vue framebuffer + polices (font/glyphs/cjk.bin) + terminal
│           ├── sender.rs / receiver.rs / pager.rs / echo.rs / kbd.rs  # domaines 0..4 démos et clavier
│           └── bin/          #     18 points d'entrée (chacun écrit morion_main → run() de son module)
├── kernel_test/              # Noyau de test d'intégration de démarrage précoce (conservé temporairement)
│   └── src/main.rs
├── resources/
│   └── system/               # Ressources à l'échelle du système
│       ├── device/           #   Icônes de périphériques (.ico)
│       ├── file/             #   Icônes de types de fichiers (.ico)
│       ├── github/           #   Couverture GitHub (.png)
│       ├── icons/            #   Icônes d'interface générales (.ico)
│       ├── logo/             #   Logo système (.ico/.svg/.png)
│       ├── service/          #   Icônes de services (.ico)
│       └── terminal/         #   Arrière-plans du terminal (.raw)
├── docs/
│   ├── architecture.md       #   Conception de l'architecture technique
│   ├── app-dev-guide.md      #   Guide de développement d'applications (incl. spécification des capacités appelables par l'IA)
│   ├── dev-reference.md      #   Référence rapide du noyau et des interfaces
│   ├── commands.md           #   Commandes de compilation / exécution / vérification
│   ├── shell-reference.md    #   Référence d'utilisation du shell
│   └── roadmap-fs.md         #   Feuille de route des systèmes de fichiers
├── Cargo.toml                # Workspace Rust (boot/kernel/user/kernel_test)
├── Cargo.lock
├── Makefile                  # Système de build (make iso/run/run-nvme/debug/check/clippy)
├── flake.nix                 # Intégration du build Nix
├── rust-toolchain.toml       # Chaîne d'outils Rust nightly
├── linker.ld                 # Script de linker du noyau
├── .gitattributes
├── .gitignore
├── CONTRIBUTING.md
├── LICENSE
├── README.md                 # Anglais (principal)
├── README.zh-CN.md           # 简体中文
├── README.ja.md              # 日本語
├── README.ko.md              # 한국어
├── README.es.md              # Español
├── README.fr.md              # Français
├── README.de.md              # Deutsch
└── README.ru.md              # Русский
```

### Structure de développement cible

```
├── bootloader/       # Chargeur d'amorçage (UEFI)
├── kernel/           # Sources du noyau
│   ├── arch/         #   Spécifique à l'architecture (x86_64 / AArch64 / RISC-V)
│   ├── core/         #   Cœur du micro-noyau (IPC, ordonnanceur, espace d'adressage, caps)
│   └── compat/       #   Couches de compatibilité (POSIX / Linux / RTOS)
├── services/         # Services système en espace utilisateur
│   ├── fs/           #   Service de système de fichiers
│   ├── net/          #   Pile réseau
│   ├── device/       #   Service de périphériques + pilotes
│   ├── security/     #   Sécurité / auth / audit
│   ├── enclave/      #   Gestionnaire d'enclaves
│   ├── gui/          #   Service GUI
│   ├── shell/        #   Service shell
│   ├── audio/        #   Service audio
│   ├── ime/          #   Service IME
│   └── ...           #   D'autres services
├── userland/         # Espace utilisateur
│   ├── libs/         #   libc, libvfs, libdevice, etc.
│   └── bin/          #   Commandes de base (ls, cat, mkdir, rm)
├── resources/        # Fichiers de ressources
├── docs/             # Documentation
└── pkg/              # Gestion de paquets (style Nix)
```

---

## Feuille de route

> Feuille de route détaillée des systèmes de fichiers : [docs/roadmap-fs.md](./docs/roadmap-fs.md) ; feuille de route du sous-système graphique : [docs/roadmap-gfx.md](./docs/roadmap-gfx.md). Les éléments cochés sont **vérifiés en exécution sous QEMU**.

### Phase 1 — Cœur du micro-noyau (quasiment terminé)

- [x] Création des domaines de protection (processus) et isolation des espaces d'adressage
- [x] IPC synchrone/asynchrone (`send` / `recv` / `call` / `reply` ; charge utile de 96 octets + mémoire partagée pour les données en masse)
- [x] Ordonnancement des tâches et commutation de contexte
- [x] map/unmap d'espace d'adressage + pager en espace utilisateur (pagination à la demande)
- [x] Routage des interruptions (« interruptions comme IPC ») + octrois MMIO / ports d'E/S
- [x] Système de capacités (slots + handles : émission / vérification / révocation)
- [x] **Transfert de capacités via IPC** (transfert de handle `SYS_HANDLE_SEND` : déplacer un objet ouvert **dans** le domaine cible, sémantique de déplacement ; délégation de capacité `SYS_CAP_SEND` : **copier** une capacité détenue, sans amplification ; les deux requièrent `SendTo(to)`). Les domaines 0/1/3 couvrent 8 cas d'autotest positifs/négatifs au démarrage

### Phase 2 — Services de base (en cours)

- [x] Énumération PCI + pilote NVMe en espace utilisateur (service de périphérique bloc, avec repli IDE PIO)
- [x] **Analyse de partitions MBR/GPT + couche de volumes** (dans block_srv, `dev` = numéro de volume ; type de FS déduit par signature de tête de volume)
- [x] **Montage multi-volume** : les 32 bits de poids fort du tag de requête portent le code de volume, une ouverture lie un volume ; chaque service FS signale automatiquement ses volumes supplémentaires (montés sous `/usb<vol>`) ; tampon de cluster fat32 2 → 16 pages (**plafond de cluster 64 KiB**), limite de groupes de blocs ext2 16 → 4096
- [x] Systèmes de fichiers : FAT32 (y compris noms longs VFAT) / tmpfs / MorionFS original (COW + snapshots)
- [x] Compatibilité ext2 **lecture seule** (monte les partitions Linux existantes)
- [x] Compatibilité exFAT (`exfat_srv` domaine 13, monté sous `/usb`) : **lecture seule** + **lecture-écriture** (`CREAT/WRITE/MKDIR/UNLINK/RMDIR/TRUNCATE`) + **volumes volumineux** (DMA multi-pages, commande unique ≤ 128 KiB)
- [x] libvfs + couche de montage : plusieurs systèmes de fichiers assemblés en une racine `/` unique, montage à l'exécution pris en charge
- [x] Shell (builtins + chemins relatifs au cwd) et pilote clavier en espace utilisateur
- [x] **Format MorionFS v2 finalisé + récupération d'espace / GC** (bitmap libre + mark & sweep)
- [x] **Gros fichiers MorionFS** (`MFS3` : blocs indirects, brisant l'ancienne limite d'≈4 MiB)
- [x] **Répertoires et noms longs MorionFS** (`MFS4` : dirents de longueur variable + blocs d'extension, noms ≤255 octets)
- [x] **Métadonnées de nœud MorionFS** (`MFS5` : horodatages / permissions / propriétaire / nombre de liens, `rename` / `truncate` / `chmod`)
- [x] **Indirection des numéros d'inode + liens physiques MorionFS** (`MFS6`)
- [x] **Liens symboliques MorionFS** (`MFSL` : résolution de chemin avec suivi + protection contre les boucles, `readlink` / `lstat` / `ln -s`)
- [x] **Formatage MorionFS selon la géométrie du volume** (**M7**)
- [x] **Formatage explicite + multi-volume MorionFS** (**M8 / S2** : `mkfs.mfs <vol>`, la garde n'accepte que les volumes vierges ou MFS)
- [x] **Montée en capacité MorionFS (bitmap externalisé)** (**S3a** : plafond ≈119 MiB → **≈127,25 GiB**)
- [x] **Fichier unique MorionFS au-delà de 4 GiB** (**S3b** : `offset`/`size` VFS en u64 de bout en bout + bloc indirect de troisième niveau `MFI3`)
- [x] **Basculement de volume principal MorionFS** (**S2** : le superbloc stocke le numéro de série du volume principal ; le numéro de série le plus élevé l'emporte)
- [x] **Repointage du volume principal MorionFS par simple marqueur** (**S2** : `mfs.primary <vol>` sans toucher aux données)
- [x] **Utilisation de l'espace `df`** (MorionFS rapporte total / utilisé / libre)
- [x] **Clôture de la gestion des volumes : écriture des tables de partitions** (**S2** : `part.create/del/wipe/reload`, GPT + MBR, vérifié par rapport à `sgdisk -v` de l'hôte)
- [x] **Interruptions NVMe (MSI/MSI-X)** (**S3** : support minimal LAPIC + parcours des capacités PCI + segment de vecteurs MSI `0x50..0x5F`)
- [x] **Attente d'interruption bloquante (primitive wait)** (**S4** : blocage borné par timeout + `SYS_IRQ_WAIT`)
- [x] **Multi-vecteurs + `wait_any`** (**S5** : clé d'attente par domaine + masque de vecteurs, admin + 2 files d'E/S chacune avec son propre vecteur)
- [x] **Rendu CJK du terminal (CJK / pleine chasse / chasse mixte)** (côté noyau supprimé plus tard en G3c, police déplacée vers `gfx_srv` en espace utilisateur)
- [x] **Chargement d'exécutables (ELF + spawn à l'exécution)** (**E1** : chargeur ELF64 + `SYS_SPAWN_ELF` ; la démo `user/hello` est un ELF autonome)
- [x] **Runtime libmorion en espace utilisateur + commande `run`** (**E2a**)
- [x] **Services scindés en programmes autonomes + destruction de domaine / récupération de frames** (**E2b** : 14 services, un ELF chacun ; `SYS_DOMAIN_DESTROY/COUNT/FRAME_FREE`)
- [x] **Clôture du cycle de vie des services** (**E3a/E3b/E3c** : W^X sur les pages utilisateur ; services sortis de l'image du noyau ; superviseur `init` + redémarrage sur place depuis l'image mémoire, sans dépendance au disque)
- [x] **Graphismes G1 (framebuffer déplacé en espace utilisateur)**
- [x] **Graphismes G2 (primitives de dessin + surfaces partagées + `libmorion::gfx`)**
- [x] **Graphismes G3a (terminal dans le service + rendu de texte déplacés)**
- [x] **Graphismes G3b (sortie du shell à l'écran)**
- [x] **Graphismes G3c (le noyau abandonne la police CJK, −276 KB)**
- [x] **Graphismes G4 (entrée déplacée hors du noyau)**
- [x] **Graphismes G6 (auto-rétablissement de gfx_srv : redémarrage supervisé + reconstruction de session côté client)**
- [ ] **Compatibilité de systèmes de fichiers étendue** (écriture ext4, UDF, etc.)
- [ ] Pile réseau

### Phase 3 — Enclaves de performance (en cours)

- [x] **Passthrough IOMMU** (**E1a** sonde ACPI DMAR / **E1b** table racine + table de contexte + tables de pages de second niveau en identité et `GCMD.TE` / **E1c** fenêtre du périphérique cible restreinte à `[0, 3 GiB)`, DMA hors fenêtre refusé par l'IOMMU avec preuve — tout est fait, voir [docs/roadmap-driver.md](./docs/roadmap-driver.md))
- [ ] **Forme passthrough LibDevice et gestionnaire d'enclaves** (E2/E3 en attente ; la bibliothèque commune `user/libdevice` est déjà en place)

### Phase 4 — Réseau et sécurité (en cours)

- [x] **Pilote NIC (virtio-net)** (`net_srv` domaine 16 + octroi générique de périphérique, voir [docs/roadmap-driver.md](./docs/roadmap-driver.md) N0–N3 : bring-up virtio-modern + RX par interruptions MSI-X + autotest ARP de bout en bout)
- [x] **Second pilote réel (virtio-blk)** (`virtio_blk_srv` domaine 17, toujours via l'octroi générique de périphérique, aucune logique noyau spécifique au périphérique : D3 — autotest lecture de signature / écriture-relecture)
- [x] **Octroi de périphérique à l'exécution (D1b)** (`SYS_DEVICE_INFO` / `SYS_DEVICE_GRANT` + porte de capacité `Mmio` ; NVMe / `net_srv` / `virtio_blk_srv` obtiennent un `DeviceGrant` via un appel système à l'exécution, comportement inchangé)
- [ ] Pile TCP/IP, audit des capacités, moteur de politiques

### Phase 5 — GUI et écosystème (non commencé)

- [ ] Environnement de bureau, gestionnaire de paquets, virtualisation

### Transversal — interface de capacités pour l'IA système (spécification définie, implémentation non commencée)

- [x] Spécification de conception : comment les applications annotent / décrivent des fonctions comme **capacités appelables par l'IA** (voir [docs/app-dev-guide.md](./docs/app-dev-guide.md) §9)
- [ ] Service d'enregistrement et de découverte de capacités
- [ ] Passerelle d'appels IA (vérification des capacités / journal d'audit / délais et quotas)

---

## Licence

Ce projet utilise une **licence par module** :

- Cœur (`boot/`, `kernel/`, `user/`) : [MIT](./LICENSE)
- Module enclave (`enclave/`) : [Apache-2.0](./LICENSES/Apache-2.0.txt), incluant une concession de brevet expresse et une clause de résiliation en cas de litige en matière de brevets
- Pilotes / firmware tiers : conservent leurs licences d'origine ; voir [docs/licensing.md](./docs/licensing.md)

Formulation canonique : **le cœur de Morion OS est sous licence MIT ; le module enclave est sous Apache-2.0 ; les pilotes et firmware tiers conservent leurs propres licences.** Le dépôt n'est pas sous licence unique MIT.

---

## Contact

- Page d'accueil du projet : [github.com/sheng-fs/morion-os](https://github.com/sheng-fs/morion-os)
- E-mail : 3555679134@qq.com
