# First-Boot Detection — Concept & Options

**Date:** 2026-05-21  
**Status:** Draft for team discussion  
**Scope:** omnect Secure OS / omnect-os-init (initramfs PID 1)

---

## 1. Problem Statement

The initramfs PID 1 needs to reliably detect whether the current boot is the
**first boot after a device has been flashed**. Several initramfs operations
behave differently on first boot (e.g. data partition resize, cert directory
initialisation, factory-defaults copy). Currently, first-boot is detected
inside a single function and is not visible to the rest of the boot sequence.

### 1.1 What "first boot" means in this context

> **First boot after flashing** — the very first run of the OS on a freshly
> imaged device. Not triggered by a factory reset, a software update, or a
> normal reboot.

This is distinct from related concepts:

| Event | Should trigger first-boot detection? |
|-------|--------------------------------------|
| Fresh flash (new device) | **Yes** |
| Software (OTA) update + reboot | No |
| Normal reboot | No |
| Factory reset + reboot | **No** (device identity unchanged) |
| Power failure during first boot + retry | **Yes** (incomplete first boot) |

### 1.2 Current state

`setup_etc_overlay()` in `src/filesystem/overlayfs.rs` detects first boot by
checking whether the overlayfs upper directory (on the `/etc` partition, mounted
at `mnt/etc`) is empty:

```rust
let is_first_boot = is_directory_empty(&upper_dir)?;
```

This works today because the `/etc` partition upper directory is empty on a
fresh flash. The value is a local variable — not exposed anywhere outside that
function.

**Known gap:** if factory reset is ever implemented and it wipes the `/etc`
partition (to restore etc to factory defaults), this check would falsely trigger
first-boot detection on every post-reset boot.

---

## 2. Design Dimensions

Two orthogonal decisions must be made:

### Dimension 1 — Where is the "first-boot done" sentinel stored?

The sentinel must survive a factory reset (when that feature is implemented),
meaning it cannot live on `/data`.

| Location | Survives factory reset? | Writable from initramfs? | Notes |
|----------|------------------------|--------------------------|-------|
| `/etc` partition upper dir | ✗ (wiped on reset) | ✓ (read-write mount) | Current implicit approach |
| `/factory` partition | ✓ (device identity) | Read-only by default; requires remount | |
| Bootloader environment | ✓ (separate partition) | ✓ (grub-editenv / fw_setenv) | Unavailable in degraded mode |

### Dimension 2 — Who writes the sentinel and when?

A successful "first boot" can be defined at different levels:

| Level | Definition | Writer |
|-------|-----------|--------|
| **initramfs** | All initramfs operations completed without error (`run_init()` returned `Ok`) | initramfs (just before `switch_root`) |
| **OS startup** | The running OS came up and confirmed operational health | `omnect-device-service` (ODS) after startup |

---

## 3. Options

### Option A — Bootloader environment key (initramfs writes)

**Sentinel:** absence of `omnect_first_boot_done=1` in bootloader env  
**Set by:** initramfs, at the end of `run_init()` success path

```
Fresh flash:
  boot → omnect_first_boot_done absent → first_boot = true
  init completes → set omnect_first_boot_done=1

Subsequent boots:
  boot → omnect_first_boot_done=1 present → first_boot = false

Factory reset:
  bootloader env untouched → omnect_first_boot_done=1 still present → first_boot = false ✓
```

**Pros:**
- No partition remount needed
- Consistent with existing bootloader env usage (`ResizedData`, fsck diagnostics)
- Atomic from initramfs perspective

**Cons:**
- Does not work in **degraded mode** — if bootloader env is unavailable on the very
  first boot (broken env on a fresh device), the marker can never be set and every
  subsequent boot looks like first boot
- Initramfs success ≠ full system operational; OS may still fail to come up

---

### Option B — `/factory` partition sentinel file (initramfs writes)

**Sentinel:** absence of `/factory/.omnect_first_boot_done`  
**Set by:** initramfs, at the end of `run_init()` success path  
**Mechanism:** remount `/factory` read-write, create file, remount read-only

```
Fresh flash:
  boot → /factory/.omnect_first_boot_done absent → first_boot = true
  init completes → remount rw, touch file, remount ro

Subsequent boots:
  boot → file present → first_boot = false

Factory reset:
  /factory untouched → file still present → first_boot = false ✓
```

**Pros:**
- Works in degraded mode (no bootloader dependency)
- `/factory` is device-identity storage — semantically correct home for a
  "device has been initialized" marker
- Simple, inspectable (file can be examined on the device)

**Cons:**
- Requires a brief read-write remount of a normally read-only partition
- Initramfs success ≠ full system operational; OS may still fail to come up

---

### Option C — `/factory` sentinel file (ODS writes, two-phase)

**Sentinel:** absence of `/factory/.omnect_first_boot_done`  
**Detect:** initramfs sets `first_boot_candidate: true` in ODS runtime JSON  
**Set by:** `omnect-device-service`, once it has confirmed the system is healthy

```
Fresh flash:
  initramfs: file absent → set first_boot_candidate=true in ODS JSON
  ODS starts, detects candidate flag → performs first-boot work → writes file to /factory

Subsequent boots:
  initramfs: file present → first_boot_candidate=false
  ODS: no first-boot work

Factory reset:
  /factory untouched → file present → no first-boot work ✓
```

**Pros:**
- Full system health required before first boot is "confirmed"
- Works in degraded mode (detection is file-based)

**Cons:**
- Two-phase protocol: initramfs detects, ODS confirms — more complex
- ODS gains a new responsibility (writing to `/factory`)
- If ODS never starts (broken rootfs image), device is permanently in first-boot
  candidate state
- Cross-component coupling: ODS must understand and honour the first-boot contract

---

### Option D — Hybrid: current detection now, `/factory` sentinel later

**Phase 1 (now):** keep the overlayfs-upper-empty detection; expose `is_first_boot`
properly inside the initramfs boot flow (return value from `setup_etc_overlay`).  
**Phase 2 (when factory reset is implemented):** migrate to Option B or A.

**Pros:**
- No change to storage semantics today
- Defers the factory-reset question to when it is actually needed
- Minimal code change

**Cons:**
- Technical debt: the detection is still semantically fragile
- Requires a second migration effort when factory reset lands

---

## 4. Decision Matrix

| | Sentinel location | Writer | When set | Factory-reset safe | Degraded-mode safe | Full-system confirmation | If sentinel is lost | Complexity |
|--|---|---|---|:-:|:-:|:-:|---|:-:|
| **A** — Boot env key | bootloader env | initramfs | end of `run_init()` | ✓ | ✗ | ✗ | Every subsequent boot looks like first boot | Low |
| **B** — `/factory` file | `/factory` partition | initramfs | end of `run_init()` | ✓ | ✓ | ✗ | Every subsequent boot looks like first boot | Low |
| **C** — `/factory` file (ODS) | `/factory` partition | ODS | after OS health confirmed | ✓ | ✓ | ✓ | Device stuck in first-boot candidate state if ODS never starts | High |
| **D** — `/etc` upper dir *(current)* | `/etc` partition upper dir | implicit | first write to `/etc` upper | ✗ | ✓ | ✗ | n/a — detection is structural, not sentinel-based | Very low |

---

## 5. Open Questions for Team Discussion

1. **Is degraded-mode first boot a realistic scenario?**  
   If a fresh device can arrive with a broken bootloader env, Option A silently
   fails. How do we want to handle that?

2. **What does factory reset actually wipe?**  
   Not yet implemented, but the design should anticipate it. Which partitions
   will be affected? This determines whether Option A or B is necessary, or
   whether Option D is sufficient until reset is designed.

3. **What level of "first boot done" guarantee do we need?**  
   Initramfs success (Options A/B/D) vs. full OS operational (Option C)?

4. **Should first-boot detection be visible to the running OS?**  
   Currently scoped to initramfs only. If ODS or application containers also
   need to know, the mechanism changes.

5. **Scope of this change: now or when factory reset is designed?**  
   Option D (expose the existing check) is a safe, low-risk step now.
   Options A/B are larger changes that make more sense alongside the factory
   reset feature design.
