#!/usr/bin/env bash
# Crea o recrea la VM EclipseOS en VirtualBox.
#
# Uso:
#   ./scripts/vbox-eclipse.sh --live <esp-dir>   # escritorio labwc (make vbox)
#   ./scripts/vbox-eclipse.sh --iso              # ISO instalador (consola, sin labwc)
#   ./scripts/vbox-eclipse.sh --disk-only        # VDI post-install-eclipse
#   ./scripts/vbox-eclipse.sh --no-start         # solo crear/actualizar la VM
#
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
VM_NAME="${VBOX_VM_NAME:-EclipseOS}"
BUILD_DIR="${ROOT}/build/vbox"
ISO="${ROOT}/dist/eclipse-x86_64.iso"
VDI="${BUILD_DIR}/eclipse-disk.vdi"
LIVE_VDI="${BUILD_DIR}/eclipse-live.vdi"
LIVE_RAW="${BUILD_DIR}/eclipse-live.img"
SERIAL_LOG="${BUILD_DIR}/serial.log"
# Match QEMU (`-m 4G`). 1 GiB is not enough: rboot allocates the kernel BSS
# (~512 MiB heap) page-by-page from UEFI *plus* the ELF and initramfs, and
# VirtualBox EFI then returns OUT_OF_RESOURCES (panic in allocate_frame).
RAM_MB="${VBOX_RAM_MB:-4096}"
CPUS="${VBOX_CPUS:-2}"
DISK_ONLY=0
NO_START=0
ISO_MODE=0
LIVE_ESP=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --live)
      LIVE_ESP="${2:?--live requiere el directorio ESP (target/x86_64/release/esp)}"
      shift 2
      ;;
    --iso)
      ISO_MODE=1
      shift
      ;;
    --disk-only)
      DISK_ONLY=1
      shift
      ;;
    --no-start)
      NO_START=1
      shift
      ;;
    -h|--help)
      sed -n '2,10p' "$0"
      exit 0
      ;;
    *)
      echo "opción desconocida: $1" >&2
      exit 1
      ;;
  esac
done

command -v VBoxManage >/dev/null || { echo "falta VBoxManage (VirtualBox)"; exit 1; }

mkdir -p "$BUILD_DIR"

if [[ -n "$LIVE_ESP" && "$ISO_MODE" -eq 1 ]]; then
  echo "usa --live o --iso, no ambos" >&2
  exit 1
fi

if [[ -n "$LIVE_ESP" && "$DISK_ONLY" -eq 1 ]]; then
  echo "usa --live o --disk-only, no ambos" >&2
  exit 1
fi

if [[ -z "$LIVE_ESP" && "$DISK_ONLY" -eq 0 ]]; then
  # Default without --live is the installer ISO (console, desktop=none).
  ISO_MODE=1
fi

if [[ "$ISO_MODE" -eq 1 && ! -f "$ISO" ]]; then
  echo "no existe $ISO — ejecuta: make iso ARCH=x86_64" >&2
  exit 1
fi

build_live_vdi() {
  local esp="$1"
  [[ -d "$esp/EFI" ]] || { echo "ESP incompleto: falta $esp/EFI" >&2; exit 1; }
  command -v sgdisk >/dev/null || { echo "falta sgdisk (paquete: gdisk)"; exit 1; }
  command -v mformat >/dev/null || { echo "falta mformat (paquete: mtools)"; exit 1; }
  command -v mcopy >/dev/null || { echo "falta mcopy (paquete: mtools)"; exit 1; }
  command -v mmd >/dev/null || { echo "falta mmd (paquete: mtools)"; exit 1; }

  local esp_mb disk_mb
  esp_mb=$(du -sm "$esp/EFI" | cut -f1)
  esp_mb=$((esp_mb + 128))
  disk_mb=$((esp_mb + 8))
  echo "ESP live: ${esp_mb} MiB (disco ${disk_mb} MiB, GPT + partición EFI)"

  rm -f "$LIVE_RAW"
  dd if=/dev/zero of="$LIVE_RAW" bs=1M count="$disk_mb" status=none
  sgdisk -o "$LIVE_RAW" >/dev/null
  sgdisk -n "1:2048:+${esp_mb}M" -t 1:ef00 -c 1:EFI "$LIVE_RAW" >/dev/null
  mformat -i "${LIVE_RAW}@@1048576" -F -v EFI :: >/dev/null
  mmd -i "${LIVE_RAW}@@1048576" ::/EFI ::/EFI/Boot ::/EFI/zCore >/dev/null
  mcopy -i "${LIVE_RAW}@@1048576" -s "$esp/EFI" ::/ >/dev/null

  VBoxManage closemedium disk "$LIVE_VDI" 2>/dev/null || true
  rm -f "$LIVE_VDI"
  VBoxManage convertdd "$LIVE_RAW" "$LIVE_VDI" --format VDI
}

# Recreate the VM config every run. Do NOT use `unregistervm --delete`: that
# also deletes attached HDDs, so a second `make vbox` would create the VDI,
# then wipe it with the old VM, then fail to attach
# (`VERR_FILE_NOT_FOUND` on eclipse-disk.vdi).
if VBoxManage list vms | grep -q "\"${VM_NAME}\""; then
  VBoxManage controlvm "$VM_NAME" poweroff 2>/dev/null || true
  for _ in 1 2 3 4 5; do
    if ! VBoxManage showvminfo "$VM_NAME" --machinereadable 2>/dev/null | grep -q '^VMState="running"'; then
      break
    fi
    sleep 0.2
  done
  VBoxManage unregistervm "$VM_NAME"
fi
rm -rf "${BUILD_DIR}/${VM_NAME}"

BOOT_DISK="$VDI"
if [[ -n "$LIVE_ESP" ]]; then
  build_live_vdi "$LIVE_ESP"
  BOOT_DISK="$LIVE_VDI"
elif [[ ! -f "$VDI" ]]; then
  VBoxManage closemedium disk "$VDI" 2>/dev/null || true
  echo "creando VDI vacío de 8 GiB en $VDI"
  VBoxManage createmedium disk --filename "$VDI" --size 8192 --format VDI
fi

if [[ ! -f "$BOOT_DISK" ]]; then
  echo "no se pudo crear $BOOT_DISK" >&2
  exit 1
fi

# Live labwc is software-GL over GOP; 32 MiB VRAM covers 1920x1080 with headroom.
# The installer console is fine with 16 MiB.
if [[ -n "$LIVE_ESP" ]]; then
  VRAM_MB="${VBOX_VRAM_MB:-32}"
else
  VRAM_MB="${VBOX_VRAM_MB:-16}"
fi

VBoxManage createvm --name "$VM_NAME" --basefolder "$BUILD_DIR" --register
VBoxManage modifyvm "$VM_NAME" \
  --memory "$RAM_MB" \
  --vram "$VRAM_MB" \
  --firmware efi \
  --chipset ich9 \
  --graphicscontroller vmsvga \
  --accelerate3d off \
  --ioapic on \
  --rtcuseutc on \
  --cpus "$CPUS" \
  --uart1 0x3F8 4 \
  --uartmode1 file "$SERIAL_LOG" \
  --nic1 nat \
  --nictype1 82540EM \
  --cableconnected1 on \
  --audio-driver none \
  --usbxhci on \
  --keyboard usb \
  --mouse usbtablet

# Pin EFI GOP. rboot `resolution=auto` (ISO antiguo) still picks the
# largest firmware-offered mode, so VRAM must be small enough that 5K/8K
# cannot exist: 16 MiB → ~1080p, seven console shadows fit in the 512 MiB
# heap. 64 MiB still allowed 5120×3200 (~62 MiB/VT × 7 = OOM). Override
# with VBOX_VRAM_MB / VBOX_GOP.
VBoxManage setextradata "$VM_NAME" "VBoxInternal2/EfiGraphicsResolution" \
  "${VBOX_GOP:-1920x1080}"

VBoxManage storagectl "$VM_NAME" --name "SATA" --add sata --controller IntelAhci --portcount 2
VBoxManage storageattach "$VM_NAME" --storagectl "SATA" --port 0 --device 0 --type hdd --medium "$BOOT_DISK"

if [[ -n "$LIVE_ESP" ]]; then
  VBoxManage storageattach "$VM_NAME" --storagectl "SATA" --port 1 --device 0 --type dvddrive --medium emptydrive
  VBoxManage modifyvm "$VM_NAME" --boot1 disk --boot2 none --boot3 none --boot4 none
elif [[ "$DISK_ONLY" -eq 0 ]]; then
  VBoxManage storageattach "$VM_NAME" --storagectl "SATA" --port 1 --device 0 --type dvddrive --medium "$ISO"
  VBoxManage modifyvm "$VM_NAME" --boot1 dvd --boot2 disk --boot3 none --boot4 none
else
  VBoxManage storageattach "$VM_NAME" --storagectl "SATA" --port 1 --device 0 --type dvddrive --medium emptydrive
  VBoxManage modifyvm "$VM_NAME" --boot1 disk --boot2 none --boot3 none --boot4 none
fi

echo ""
echo "VM '$VM_NAME' lista."
echo "  Serial log: $SERIAL_LOG"
echo "  Disco:      $BOOT_DISK"
if [[ -n "$LIVE_ESP" ]]; then
  echo "  Modo:       live (labwc / initramfs QEMU, no el ISO instalador)"
  echo "  ESP:        $LIVE_ESP"
elif [[ "$DISK_ONLY" -eq 0 ]]; then
  echo "  ISO:        $ISO"
  echo ""
  echo "Instalación: arranca desde el ISO (consola, sin escritorio), ejecuta install-eclipse, apaga y vuelve a lanzar con:"
  echo "  $0 --disk-only"
else
  echo ""
  echo "Arranque desde disco (sin ISO)."
fi
echo ""
echo "Tras arrancar, revisa el log serial:"
echo "  strings \"$SERIAL_LOG\" | grep eclipse | tail -20"

if [[ "$NO_START" -eq 0 ]]; then
  : >"$SERIAL_LOG"
  VBoxManage startvm "$VM_NAME" --type gui
fi
