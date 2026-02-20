/* Linker script para el microkernel Eclipse OS */
OUTPUT_FORMAT(elf64-x86-64)
OUTPUT_ARCH(i386:x86-64)
ENTRY(_start)

/* Dirección de carga del kernel (después del bootloader) */
/* Enlazamos en 2MB pero en HIGHER HALF para separar kernel/user */
KERNEL_BASE = 0xFFFF800000000000;
KERNEL_PHYS_BASE = 0x200000;

SECTIONS
{
    . = KERNEL_BASE + KERNEL_PHYS_BASE;

    /* Sección de inicio - debe ser la primera */
    .init : AT(ADDR(.init) - KERNEL_BASE)
    {
        KEEP(*(.init))
    }

    /* Trampolín para SMP (Application Processors) */
    .trampoline ALIGN(4096) : AT(ADDR(.trampoline) - KERNEL_BASE)
    {
        trampoline_start = .;
        *(.trampoline)
        trampoline_end = .;
    }

    /* Sección de tablas de páginas */
    .page_tables ALIGN(4096) : AT(ADDR(.page_tables) - KERNEL_BASE)
    {
        *(.page_tables*)
    }

    /* Código ejecutable */
    .text ALIGN(4K) : AT(ADDR(.text) - KERNEL_BASE)
    {
        *(.text*)
    }

    /* Datos de solo lectura */
    .rodata ALIGN(4K) : AT(ADDR(.rodata) - KERNEL_BASE)
    {
        *(.rodata*)
    }

    /* Datos inicializados */
    .data ALIGN(4K) : AT(ADDR(.data) - KERNEL_BASE)
    {
        *(.data*)
    }

    /* Datos no inicializados (BSS) */
    .bss ALIGN(4K) : AT(ADDR(.bss) - KERNEL_BASE)
    {
        *(COMMON)
        *(.bss*)
    }

    /* Símbolos de fin de kernel */
    . = ALIGN(4K);
    _kernel_end = .;
    _kernel_phys_end = . - KERNEL_BASE;

    /* Descartar secciones innecesarias */
    /DISCARD/ :
    {
        *(.comment)
        *(.eh_frame)
        *(.note*)
    }
}
