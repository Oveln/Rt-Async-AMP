# rt-async-amp

QEMU RISC-V virt dual-core AMP: rt-async (hart 1, M-mode) + StarryOS (hart 0, S-mode via OpenSBI).

## Architecture

```
QEMU virt (-smp 2 -m 256M)
├─ hart 0: OpenSBI → sret S-mode → StarryOS @ 0x80200000 (UART0)
└─ hart 1: OpenSBI → mret M-mode → rt-async @ 0x82800000 (UART1)

Shared memory IPC @ 0x88000000 (ov_channal)
```

## Quick Start

```bash
# 1. Init submodules
git submodule update --init --recursive

# 2. Clone + patch OpenSBI and QEMU
make setup

# 3. Build everything
make all

# 4. Run
make run
```

### Prerequisites

- `rustup target add riscv64imac-unknown-none-elf`
- `riscv64-elf-gcc` (Homebrew: `brew install riscv64-elf-gcc`)
- `riscv64-linux-musl-objcopy` (for StarryOS: see note below)
- Ninja / Meson (QEMU build: `brew install ninja meson`)
- Python 3

### Build individual components

```bash
make rt-async    # rt-async RTOS binary
make opensbi     # Patched OpenSBI firmware
make starryos    # StarryOS kernel (needs StarryOS submodule)
make qemu        # Custom QEMU with UART1
```

## Directory Structure

```
rt-async/              rt-async RTOS (submodule)
StarryOS/              StarryOS kernel (submodule)
apps/rt-async-app/     rt-async side application
apps/user-test-ipc/    StarryOS userspace IPC test (Rust, cross-compiled)
modules/
  chip-qemu-virt-rt/   QEMU virt chip support (UART1, CLINT, IPI)
  axplat-riscv64-qemu-virt/  axplat platform config
patches/
  opensbi-amp.patch    OpenSBI: hart routing + PIE fix + IPI forwarding
  qemu-uart1.patch     QEMU: second UART at 0x10002000
amp.config             Shared address constants (single source of truth)
docs/IPC-DESIGN.md     IPC mechanism design document
```

## Patches

### OpenSBI (`opensbi-amp.patch`)

Applied on top of upstream OpenSBI (pinned commit in Makefile):

1. **Hart routing** (`fw_base.S`): hart 1 mrets directly to rt-async @ 0x82800000 in M-mode
2. **Default next address** (`fw_dynamic.S`): set to 0x80200000 for StarryOS
3. **Disable PIE**: bare-metal toolchain doesn't support PIE linking
4. **IPI forwarding** (`sbi_ipi.c`): forward direct MSIP writes to SSIP for S-mode
5. **CLINT S/U access** (`aclint_mswi.c`): allow S-mode to write MSIP registers

### QEMU (`qemu-uart1.patch`)

Adds a second NS16550A UART at 0x10002000 (IRQ 12) for rt-async output.

## IPC Flow

```
StarryOS → rt-async:  SBI ecall → OpenSBI → MSIP hart 1 → MachineSoft ISR
rt-async → StarryOS:  CLINT MSIP0 → OpenSBI → SSIP → S-mode SWI handler
```

Userspace access via `/dev/rt_shm` device: `open` → `mmap` → `ioctl(NOTIFY/AWAIT)`.

## Notes

- `riscv64-linux-musl-objcopy` for StarryOS: install via musl cross-make or adjust the path in the Makefile
- `make distclean` removes cloned opensbi/ and qemu/ directories
- `amp.config` is the single source of truth for all address constants
