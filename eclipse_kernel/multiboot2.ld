/* Linker script for Eclipse Kernel with Multiboot2 support */

ENTRY(_start)

/* Memory layout for Multiboot2 */
MEMORY
{
    /* Start at 1MB to avoid conflicts with BIOS */
    kernel_start = 0x100000;
    kernel_end = 0x400000;
}

SECTIONS
{
    /* Multiboot2 header must be in the first 8KB */
    .multiboot2_header : {
        *(.multiboot2_header)
    } > kernel_start

    /* Kernel code section */
    .text : {
        *(.text)
        *(.text.*)
    } > kernel_start

    /* Read-only data */
    .rodata : {
        *(.rodata)
        *(.rodata.*)
    } > kernel_start

    /* Read-write data */
    .data : {
        *(.data)
        *(.data.*)
    } > kernel_start

    /* Uninitialized data */
    .bss : {
        *(.bss)
        *(.bss.*)
        *(COMMON)
    } > kernel_start

    /* Ensure the kernel fits in the allocated space */
    . = ALIGN(4K);
    kernel_size = . - kernel_start;
}

/* Symbols for Multiboot2 */
PROVIDE(kernel_start = kernel_start);
PROVIDE(kernel_end = kernel_end);
PROVIDE(kernel_size = kernel_size);
