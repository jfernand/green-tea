/* The kernel's memory layout, shared by every architecture. build.rs replaces @BASE@
 * with the load address for the target. Addresses are physical.
 *
 * The __*_start and __*_end symbols are how the kernel finds each section: see
 * src/memory.rs, which must list the same sections with the same alignments. */
ENTRY(_start)

SECTIONS
{
    . = @BASE@;
    __kernel_start = .;

    /* x86_64 only: the PVH entry note QEMU looks for. It must stay a note section of its
     * own so the linker gives it a PT_NOTE program header. */
    .note.Xen : {
        KEEP(*(.note.Xen))
    }

    .text : {
        __text_start = .;
        *(.text.entry)          /* _start must be first */
        *(.text .text.*)
        __text_end = .;
    }

    .rodata : ALIGN(16) {
        __rodata_start = .;
        *(.rodata .rodata.*)
        *(.srodata .srodata.*)
        __rodata_end = .;
    }

    .data : ALIGN(16) {
        __data_start = .;
        *(.data .data.*)
        *(.sdata .sdata.*)
        __data_end = .;
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
        __stack_bottom = .;
        . += 64K;
        __stack_top = .;
    }

    /* Memory for the heap (tasks, closures, the run queue): a TLSF allocator, see heap.rs. */
    .heap (NOLOAD) : ALIGN(4096) {
        __heap_start = .;
        . += 4M;
        __heap_end = .;
    }

    /* Task stacks, each with a guard page under it (see src/stack.rs). One 2 MiB page,
     * so a board that pages can map just this region with 4 KiB pages. */
    .stacks (NOLOAD) : ALIGN(2M) {
        __stacks_start = .;
        . += 2M;
        __stacks_end = .;
    }
    __kernel_end = .;
}
