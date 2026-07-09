# Makefile — RT24 rcpu1 MSIP probe
#
# 用法：
#   make            # 构建 msip_probe.elf / .bin / .asm（需 riscv64-elf-gcc，homebrew: brew install riscv64-elf-gcc）
#   make run        # 提示如何烧录（见下方说明）
#
# 产物 msip_probe.elf 可替换 esos_rt24.its 里的 rcpu1-fw 节点
# （data = .../msip_probe.elf.lzo，load/entry = <0x1 0x804000>），
# 或直接用 U-Boot 的 tftp/bootm 加载到 <0x1 0x804000>。
#
# 与 esOS baremetal Makefile 的唯一区别：工具链前缀改为 homebrew 的
# riscv64-elf-gcc（不带 -unknown-），栈改 8KB（trap 帧更大）。

PREFIX    = riscv64-elf-
CC        = $(PREFIX)gcc
AS        = $(PREFIX)gcc
LD        = $(PREFIX)gcc
OBJCOPY   = $(PREFIX)objcopy
OBJDUMP   = $(PREFIX)objdump
SIZE      = $(PREFIX)size

ENTRY_POINT = 0x100804000
LDSCRIPT  = msip_probe.ld

DEVICE  = -march=rv64imafdczifencei -mabi=lp64d -mcmodel=medany
CFLAGS  = $(DEVICE) -ffreestanding -fno-common -ffunction-sections -fdata-sections -Os -g2 \
          -Wall -Wextra
AFLAGS  = -c $(DEVICE) -x assembler-with-cpp -D__ASSEMBLY__
LFLAGS  = $(DEVICE) -nostartfiles -nostdlib -nodefaultlibs \
          -Wl,--no-whole-archive \
          -Xlinker --defsym=ENTRY_POINT=$(ENTRY_POINT) \
          -T $(LDSCRIPT) -Wl,-gc-sections -Wl,-Map=msip_probe.map

TARGET = msip_probe

.PHONY: all clean run lzo

all: $(TARGET).elf
	$(SIZE) $(TARGET).elf
	@echo ""
	@echo "产物："
	@echo "  $(TARGET).elf  → 塞进 esos_rt24.its 的 rcpu1-fw 节点"
	@echo "  （或 U-Boot: tftpboot <0x1804000> msip_probe.elf; bootm 不适用 ELF——用 .bin 或打包 itb）"

$(TARGET).elf: startup.o main.o $(LDSCRIPT)
	$(LD) $(LFLAGS) -o $@ startup.o main.o
	$(OBJCOPY) -O binary $@ $(TARGET).bin
	$(OBJDUMP) -D -S $@ > $(TARGET).asm

startup.o: startup.S
	$(AS) $(AFLAGS) -o $@ $<

main.o: main.c
	$(CC) $(CFLAGS) -o $@ -c $<

# lzo 压缩（esOS its 用 lzo 打包；若系统有 lzop 才可用，否则跳过手动压）
lzo: $(TARGET).bin
	lzop -f -o $(TARGET).bin.lzo $(TARGET).bin || echo "(lzop 未安装，直接用 .elf/.bin)"

run:
	@echo "上板方式见 README.md（本目录）。简述："
	@echo "  1. make && make lzo"
	@echo "  2. 把 msip_probe.bin.lzo 放进 esos_rt24.its 的 rcpu1-fw.data"
	@echo "  3. mkimage + 烧录到 K3，复位后从 R_UART0(115200 8N1) 看输出"

clean:
	rm -f *.o *.elf *.bin *.bin.lzo *.asm *.map
