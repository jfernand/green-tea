/* The kernel's memory layout, shared by every architecture. build.rs replaces @BASE@
 * with the load address for the target. Addresses are physical. */
ENTRY(_start)

SECTIONS
{
    . = @BASE@;

    /* x86_64 only: the PVH entry note QEMU looks for. It must stay a note section of its
     * own so the linker gives it a PT_NOTE program header. */
    .note.Xen : {
        KEEP(*(.note.Xen))
    }

    .text : {
        *(.text.entry)          /* _start must be first */
        *(.text .text.*)
    }

    .rodata : ALIGN(16) {
        *(.rodata .rodata.*)
        *(.srodata .srodata.*)
    }

    .data : ALIGN(16) {
        *(.data .data.*)
        *(.sdata .sdata.*)
    }

    .bss (NOLOAD) : ALIGN(16) {
        __bss_start = .;
        *(.sbss .sbss.*)
        *(.bss .bss.*)
        *(COMMON)
        . = ALIGN(16);
        __bss_end = .;
    }

    /* The boot stack: kmain and the scheduler run here; tasks get their own. */
    .stack (NOLOAD) : ALIGN(4096) {
        . += 64K;
        __stack_top = .;
    }

    /* Memory for the bump allocator (task stacks, closures, the run queue). */
    .heap (NOLOAD) : ALIGN(4096) {
        __heap_start = .;
        . += 4M;
        __heap_end = .;
    }
}
