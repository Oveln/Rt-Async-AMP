# rt-async-amp Makefile
#
# Build + run QEMU virt dual-core AMP (rt-async + StarryOS)
#
# Usage:
#   make setup      # Clone + patch opensbi/qemu (first time)
#   make all        # Build rt-async, opensbi, starryos
#   make run        # Launch QEMU
#   make clean

# ── Read address layout from amp.toml ────────────────────────────────────────
AMP_CONFIG   := amp.toml
RTASYNCBASE  := $(shell sed -n 's/^RTASYNCBASE.*=.*"\(.*\)".*/\1/p' $(AMP_CONFIG))
SHMBASE      := $(shell sed -n 's/^SHMBASE.*=.*"\(.*\)".*/\1/p' $(AMP_CONFIG))
SHMSIZE      := $(shell sed -n 's/^SHMSIZE.*= *\([0-9]*\)/\1/p' $(AMP_CONFIG))
QEMUSMP      := $(shell sed -n 's/^QEMUSMP.*= *\([0-9]*\)/\1/p' $(AMP_CONFIG))
QEMURAM      := $(shell sed -n 's/^QEMURAM.*=.*"\(.*\)".*/\1/p' $(AMP_CONFIG))
OPENSBIBASE  := $(shell sed -n 's/^OPENSBIBASE.*=.*"\(.*\)".*/\1/p' $(AMP_CONFIG))

# ── Upstream repo pins ───────────────────────────────────────────────────────
OPENSBI_REPO  := https://github.com/riscv-software-src/opensbi.git
OPENSBI_COMMIT := 547a5bbda7c3ec0096a6c87809851f8c2df047d1

QEMU_REPO     := https://github.com/qemu/qemu.git
QEMU_COMMIT   := f5a2438405d4ae8b62de7c9b39fac0b2155ee544

# ── Paths ────────────────────────────────────────────────────────────────────
TARGET       := riscv64imac-unknown-none-elf
BUILD_DIR    := build
RT_ASYNC_DIR := rt-async
APP_ELF      := target/$(TARGET)/release/demo
APP_BIN      := $(BUILD_DIR)/rt-async.bin

OPENSBI_DIR  := opensbi
OPENSBI_FW   := $(BUILD_DIR)/fw_dynamic.bin

STARRYOS_DIR   := StarryOS
STARRYOS_BIN   := $(BUILD_DIR)/starryos.bin
STARRYOS_TARGET := riscv64gc-unknown-none-elf
STARRYOS_FEATURES := axfeat/myplat axfeat/bus-pci axfeat/display axfeat/fs-ng-times starry-kernel/input starry-kernel/vsock starry-kernel/dev-log qemu

QEMU_SRC_DIR := qemu
QEMU_BUILD   := $(QEMU_SRC_DIR)/build
QEMU_BIN     := $(QEMU_BUILD)/qemu-system-riscv64-unsigned
UART_LOG     := $(BUILD_DIR)/rt-async-uart.log

QEMU_FLAGS   := -machine virt -display none \
                -serial mon:stdio -serial file:$(UART_LOG) \
                -smp $(QEMUSMP) -m $(QEMURAM)

# ── Phony targets ────────────────────────────────────────────────────────────
DEBUGFS      := /opt/homebrew/opt/e2fsprogs/sbin/debugfs
ROOTFS       := $(STARRYOS_DIR)/rootfs-riscv64.img

.PHONY: all setup rt-async opensbi starryos qemu user-test install run clean distclean

all: rt-async opensbi starryos
	@echo "Build complete. Run 'make run' to start QEMU."

# ── Setup: clone + patch external repos ──────────────────────────────────────
setup: $(OPENSBI_DIR)/.patched $(QEMU_SRC_DIR)/.patched
	@echo "Setup complete."

$(OPENSBI_DIR)/.patched:
	git clone --filter=blob:none $(OPENSBI_REPO) $(OPENSBI_DIR)
	cd $(OPENSBI_DIR) && git checkout -f $(OPENSBI_COMMIT)
	cd $(OPENSBI_DIR) && git apply --whitespace=nowarn $(CURDIR)/patches/opensbi-amp.patch
	@touch $@

$(QEMU_SRC_DIR)/.patched:
	git clone --filter=blob:none $(QEMU_REPO) $(QEMU_SRC_DIR)
	cd $(QEMU_SRC_DIR) && git checkout -f $(QEMU_COMMIT)
	cd $(QEMU_SRC_DIR) && git apply --whitespace=nowarn $(CURDIR)/patches/qemu-uart1.patch
	@touch $@

# ── Build: QEMU ──────────────────────────────────────────────────────────────
qemu: $(QEMU_BIN)

$(QEMU_BIN): $(QEMU_SRC_DIR)/.patched
	mkdir -p $(QEMU_BUILD)
	cd $(QEMU_BUILD) && ../configure \
		--target-list=riscv64-softmmu \
		--disable-docs \
		--disable-tools \
		--disable-guest-agent \
		--python=python3
	$(MAKE) -C $(QEMU_BUILD) -j$$(nproc)
	@echo "QEMU → $(QEMU_BIN)"

# ── Build: rt-async ──────────────────────────────────────────────────────────
rt-async: $(APP_BIN)

$(APP_BIN): $(APP_ELF)
	@mkdir -p $(BUILD_DIR)
	riscv64-elf-objcopy -O binary $< $@
	@echo "rt-async → $@"

$(APP_ELF):
	cd apps/rt-async-app && \
	cargo build --target $(TARGET) --release -p rt-async-app

# ── Build: OpenSBI ───────────────────────────────────────────────────────────
opensbi: $(OPENSBI_FW)

$(OPENSBI_FW): $(OPENSBI_DIR)/.patched
	cd $(OPENSBI_DIR) && \
		make -j$$(nproc) PLATFORM=generic CROSS_COMPILE=riscv64-elf- \
		O=build FW_TEXT_START=$(OPENSBIBASE)
	@mkdir -p $(BUILD_DIR)
	cp $(OPENSBI_DIR)/build/platform/generic/firmware/fw_dynamic.bin $@
	@echo "OpenSBI → $@"

# ── Build: StarryOS ─────────────────────────────────────────────────────────
starryos: $(STARRYOS_BIN)

$(STARRYOS_BIN):
	@if [ ! -d "$(STARRYOS_DIR)" ]; then \
		echo "StarryOS not found. Run 'git submodule update --init StarryOS'."; \
		exit 1; \
	fi
	@mkdir -p $(BUILD_DIR)
	cd $(STARRYOS_DIR) && AX_CONFIG_PATH=$$PWD/.axconfig.toml \
		RUSTFLAGS='-C link-arg=-Ttarget/$(STARRYOS_TARGET)/release/linker_riscv64-qemu-virt.lds -C link-arg=-no-pie -C link-arg=-znostart-stop-gc' \
		cargo build -Z unstable-options \
		--target $(STARRYOS_TARGET) --target-dir target --release \
		--features '$(STARRYOS_FEATURES)'
	riscv64-elf-objcopy -O binary \
		$(STARRYOS_DIR)/target/$(STARRYOS_TARGET)/release/starryos $@
	@echo "StarryOS → $@"

# ── Build: userspace programs ────────────────────────────────────────────────
USER_TEST_TARGET := riscv64gc-unknown-linux-musl
USER_TEST_BIN    := $(BUILD_DIR)/user-test-ipc

user-test: $(USER_TEST_BIN)

$(USER_TEST_BIN):
	@mkdir -p $(BUILD_DIR)
	cd apps/user-test-ipc && \
	cargo build --target $(USER_TEST_TARGET) --release
	cp apps/user-test-ipc/target/$(USER_TEST_TARGET)/release/user-test-ipc $@
	@echo "user-test-ipc → $@"

# install-<binary> — install a file from build/ into the StarryOS rootfs
# Usage: make install-user-test-ipc   or   make install-build/some-binary
# Generic: make install FILE=build/some-binary DST=/some-binary

install:
	@if [ -z "$(FILE)" ]; then echo "Usage: make install FILE=build/<name> [DST=/<name>]"; exit 1; fi
	@DST=$${DST:-/$(notdir $(FILE))}; \
	if [ ! -f "$(FILE)" ]; then echo "File not found: $(FILE)"; exit 1; fi; \
	if [ ! -f "$(ROOTFS)" ]; then echo "Rootfs not found: $(ROOTFS)"; exit 1; fi; \
	pkill -9 qemu-system-riscv64 2>/dev/null || true; \
	sleep 0.5; \
	$(DEBUGFS) -w -R "rm $$DST" $(ROOTFS) 2>/dev/null || true; \
	$(DEBUGFS) -w -R "write $(FILE) $$DST" $(ROOTFS); \
	echo "Installed $(FILE) → $$DST in $(ROOTFS)"

# ── Run ──────────────────────────────────────────────────────────────────────
run:
	@if [ ! -f "$(OPENSBI_FW)" ]; then echo "Run 'make opensbi' first."; exit 1; fi
	@if [ ! -f "$(APP_BIN)" ]; then echo "Run 'make rt-async' first."; exit 1; fi
	@if [ ! -f "$(STARRYOS_BIN)" ]; then echo "Warning: no StarryOS binary."; fi
	@echo "Starting QEMU ($(QEMUSMP) cores, $(QEMURAM) RAM)..."
	@echo "  UART0 → stdio (OpenSBI/StarryOS)"
	@echo "  UART1 → $(UART_LOG) (rt-async)"
	$(QEMU_BIN) $(QEMU_FLAGS) \
		-bios $(OPENSBI_FW) \
		-kernel $(STARRYOS_BIN) \
		-device loader,addr=$(RTASYNCBASE),file=$(APP_BIN) \
		-drive file=$(STARRYOS_DIR)/rootfs-riscv64.img,format=raw,if=none,id=hd0 \
		-device virtio-blk-pci,drive=hd0

# ── Clean ────────────────────────────────────────────────────────────────────
clean:
	cargo clean
	rm -rf $(BUILD_DIR)

distclean: clean
	rm -rf $(OPENSBI_DIR) $(QEMU_SRC_DIR)
	@echo "Removed opensbi/ and qemu/. Run 'make setup' to re-clone."
