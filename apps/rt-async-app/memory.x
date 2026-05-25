ENTRY(__start);

/* StarryOS: 0x80200000 ~ 0x826bf0c0 (~38MB)
 * rt-async: 0x82800000 (after StarryOS, 2MB aligned)
 * SHM IPC:  0x88000000
 */
MEMORY
{
    RAM : ORIGIN = 0x82800000, LENGTH = 8M
}

_max_hart_id = 0;
_hart_stack_size = 4096;
