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

# ── 路径 ──────────────────────────────────────────────────────────────────────

RT_ASYNC_DIR  := rt-async
BUILD_DIR     := build
APP_ELF       := target/$(TARGET)/release/demo
APP_BIN       := $(BUILD_DIR)/rt-async.bin

OPENSBI_DIR   := opensbi
OPENSBI_FW    := $(OPENSBI_DIR)/build/platform/generic/firmware/fw_dynamic.bin

STARRYOS_DIR  := StarryOS
STARRYOS_BIN  := $(BUILD_DIR)/starryos.bin

QEMU_SRC_DIR  := qemu
QEMU_BUILD    := $(QEMU_SRC_DIR)/build
QEMU_BIN      := $(QEMU_BUILD)/qemu-system-riscv64
UART_LOG      := $(BUILD_DIR)/rt-async-uart.log
QEMU_FLAGS    := -machine virt -display none -serial mon:stdio -serial file:$(UART_LOG) -smp 2 -m 256M

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
	cd $(STARRYOS_DIR) && make qemu_riscv64 LOG=info
	@mkdir -p $(BUILD_DIR)
	@if [ -f "$(STARRYOS_DIR)/StarryOS_qemu.bin" ]; then \
		cp $(STARRYOS_DIR)/StarryOS_qemu.bin $(STARRYOS_BIN); \
	elif [ -f "$(STARRYOS_DIR)/StarryOS_qemu.elf" ]; then \
		riscv64-elf-objcopy -O binary \
			$(STARRYOS_DIR)/StarryOS_qemu.elf $(STARRYOS_BIN); \
	else \
		echo "StarryOS binary not found"; exit 1; \
	fi
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
		-device loader,addr=0x80800000,file=$(APP_BIN) \
		$${STARRYOS_BIN:+-device loader,addr=0x80200000,file=$(STARRYOS_BIN)}

clean:
	cargo clean
	rm -rf $(BUILD_DIR)

qemu-clean:
	$(MAKE) -C $(QEMU_BUILD) clean

distclean: clean
	rm -rf $(QEMU_BUILD)
	@echo "Run 'git submodule deinit --all' manually if needed."
