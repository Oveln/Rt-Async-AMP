# rt-async-amp Makefile
#
# 一键构建 + 运行 QEMU virt 双核 (rt-async + StarryOS)
#
# 用法:
#   make qemu          # 编译自定义 QEMU (带 UART1)
#   make rt-async      # 编译 rt-async app
#   make run           # 启动 QEMU (需要 OpenSBI + StarryOS 已就绪)
#   make all           # 编译全部

TARGET  := riscv64imac-unknown-none-elf

# ── 从 amp.config 读取约定地址 ─────────────────────────────────────────────
AMP_CONFIG := amp.config
RTASYNCBASE := $(shell sed -n 's/RTASYNCBASE=//p' $(AMP_CONFIG))
SHMBASE     := $(shell sed -n 's/SHMBASE=//p' $(AMP_CONFIG))
SHMSIZE     := $(shell sed -n 's/SHMSIZE=//p' $(AMP_CONFIG))
QEMUSMP     := $(shell sed -n 's/QEMUSMP=//p' $(AMP_CONFIG))
QEMURAM     := $(shell sed -n 's/QEMURAM=//p' $(AMP_CONFIG))

# ── 路径 ──────────────────────────────────────────────────────────────────────

RT_ASYNC_DIR  := rt-async
BUILD_DIR     := build
APP_ELF       := target/$(TARGET)/release/demo
APP_BIN       := $(BUILD_DIR)/rt-async.bin

OPENSBI_DIR   := opensbi
OPENSBI_FW    := $(OPENSBI_DIR)/build/platform/generic/firmware/fw_dynamic.bin

STARRYOS_DIR  := StarryOS
STARRYOS_BIN  := $(BUILD_DIR)/starryos.bin
STARRYOS_TARGET := riscv64gc-unknown-none-elf
STARRYOS_FEATURES := axfeat/myplat axfeat/bus-pci axfeat/display axfeat/fs-ng-times starry-kernel/input starry-kernel/vsock starry-kernel/dev-log qemu

QEMU_SRC_DIR  := qemu
QEMU_BUILD    := $(QEMU_SRC_DIR)/build
QEMU_BIN      := $(QEMU_BUILD)/qemu-system-riscv64
UART_LOG      := $(BUILD_DIR)/rt-async-uart.log
QEMU_FLAGS    := -machine virt -display none -serial mon:stdio -serial file:$(UART_LOG) -smp $(QEMUSMP) -m $(QEMURAM)

# ── Targets ───────────────────────────────────────────────────────────────────

.PHONY: all qemu rt-async opensbi starryos run clean

all: rt-async
	@echo "Build complete. Run 'make run' to start QEMU."

# ── 自定义 QEMU (带 UART1 @ 0x10002000) ──────────────────────────────────────

qemu: $(QEMU_BIN)

$(QEMU_BIN):
	@if [ ! -f "$(QEMU_SRC_DIR)/configure" ]; then \
		echo "QEMU source not found. Run 'git submodule update --init qemu' first."; \
		exit 1; \
	fi
	mkdir -p $(QEMU_BUILD)
	cd $(QEMU_BUILD) && ../configure \
		--target-list=riscv64-softmmu \
		--disable-docs \
		--disable-tools \
		--disable-guest-agent \
		--python=python3
	$(MAKE) -C $(QEMU_BUILD) -j$$(nproc)
	@echo "QEMU → $(QEMU_BIN)"

rt-async: $(APP_BIN)

$(APP_BIN): $(APP_ELF)
	@mkdir -p $(BUILD_DIR)
	riscv64-elf-objcopy -O binary $< $@
	@echo "rt-async → $@"

$(APP_ELF):
	cd apps/rt-async-app && \
	cargo build --target $(TARGET) --release -p rt-async-app

opensbi:
	cd $(OPENSBI_DIR) && \
	make -j$$(nproc) PLATFORM=generic CROSS_COMPILE=riscv64-elf- O=build FW_TEXT_START=0x80000000
	@mkdir -p $(BUILD_DIR)
	cp $(OPENSBI_FW) $(BUILD_DIR)/fw_dynamic.bin
	@echo "OpenSBI → build/fw_dynamic.bin"

starryos:
	@if [ ! -d "$(STARRYOS_DIR)" ]; then \
		echo "StarryOS not found at $(STARRYOS_DIR)/"; \
		echo "Clone StarryOS first. See README."; \
		exit 1; \
	fi
	@mkdir -p $(BUILD_DIR)
	cd $(STARRYOS_DIR) && AX_CONFIG_PATH=$$PWD/.axconfig.toml \
		RUSTFLAGS='-C link-arg=-Ttarget/$(STARRYOS_TARGET)/release/linker_riscv64-qemu-virt.lds -C link-arg=-no-pie -C link-arg=-znostart-stop-gc' \
		cargo build -Z unstable-options \
		--target $(STARRYOS_TARGET) --target-dir target --release \
		--features '$(STARRYOS_FEATURES)'
	riscv64-elf-objcopy -O binary \
		$(STARRYOS_DIR)/target/$(STARRYOS_TARGET)/release/starryos $(STARRYOS_BIN)
	@echo "StarryOS → $(STARRYOS_BIN)"

run:
	@if [ ! -f "$(BUILD_DIR)/fw_dynamic.bin" ]; then \
		echo "OpenSBI firmware not found. Run 'make opensbi' first."; \
		exit 1; \
	fi
	@if [ ! -f "$(APP_BIN)" ]; then \
		echo "rt-async binary not found. Run 'make rt-async' first."; \
		exit 1; \
	fi
	@if [ ! -f "$(STARRYOS_BIN)" ]; then \
		echo "Warning: StarryOS binary not found. Starting without it."; \
	fi
	@echo "Starting QEMU virt (2 cores, 256M RAM)..."
	@echo "  UART0 (serial0) → stdio  (OpenSBI/StarryOS)"
	@echo "  UART1 (serial1) → $(UART_LOG)  (rt-async)"
	$(QEMU_BIN) $(QEMU_FLAGS) \
		-bios $(BUILD_DIR)/fw_dynamic.bin \
		$${STARRYOS_BIN:+-kernel $(STARRYOS_BIN)} \
		-device loader,addr=$(RTASYNCBASE),file=$(APP_BIN) \
		-drive file=$(STARRYOS_DIR)/rootfs-riscv64.img,format=raw,if=none,id=hd0 \
		-device virtio-blk-pci,drive=hd0

clean:
	cargo clean
	rm -rf $(BUILD_DIR)

qemu-clean:
	$(MAKE) -C $(QEMU_BUILD) clean

distclean: clean
	rm -rf $(QEMU_BUILD)
	@echo "Run 'git submodule deinit --all' manually if needed."
