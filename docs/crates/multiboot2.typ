#import "../lib.typ": *

= green-tea-multiboot2 <crate-multiboot2>

`green-tea-multiboot2` reads the information a Multiboot2 loader such as GRUB leaves in memory: the command line, the memory map, the framebuffer and the ACPI root pointer. On QEMU's boards the kernel knows these by address or asks fw_cfg. The `pc` board has neither, and this is where it starts.

#spec(
  ("Source", [#repo("drivers/multiboot2/src/lib.rs"), 245 lines]),
  ("Depends on", [nothing; `no_std`]),
  ("Used by", [`green-tea-bare` (the `pc` board)]),
  ("Tests", [8 host tests, on boot information built in a vector; every `pc` boot]),
)

#dtable(columns: (auto, 1fr), ("Item", "What it does"),
  [`BOOT_MAGIC`], [`0x36d7_6289`, what the loader leaves in `eax`],
  [`Info::new(bytes)`], [the information in a slice, if its total size fits],
  [`Info::from_addr(addr)`], [the same from the address the loader passed; `unsafe`],
  [`cmdline()`], [the command line, or `""`],
  [`arg(key)`], [the value of `key=value` on the command line],
  [`regions()`], [an iterator over the memory map's `Region`s: `base`, `len`, `available`],
  [`free_end(addr)`], [the end of the free RAM that `addr` is in],
  [`display()`], [the `Display` the loader set up: `addr`, `pitch`, `width`, `height`, `bpp`, `xrgb`],
  [`rsdp()`], [the loader's copy of the ACPI root pointer, the newer form if there is one],
)

== The tag list

The information is a total size, a reserved word, and then a list of tags. Each tag starts with its type and its size, header included, and the next one starts at the following 8-byte boundary. Type 0 ends the list.

#bytefield(caption: "The start of the boot information, and one tag",
  ("total", "u32", 4), ("reserved", "u32", 4), ("type", "u32", 4), ("size", "u32", 4), ("body", "size − 8 bytes", 8),
)

Every accessor goes through one walk, over a slice, so a wrong size can't read past the information:

#srcfile("drivers/multiboot2/src/lib.rs", lines: ((74, 86),), notes: ((77, [`u32_at` returns `None` past the slice, and the `?` ends the walk.]), (78, [A size under 8 would never advance, so it ends the walk too.]), (82, [`get` returns `None` for a tag that claims to run past the end.]), (84, [The size excludes the padding, so it is rounded up to the next 8.]),))[```rust
fn tag(&self, kind: u32) -> Option<&'a [u8]> {
    let mut at = 8;
    loop {
        let (found, size) = (u32_at(self.0, at)?, u32_at(self.0, at + 4)? as usize);
        if found == TAG_END || size < 8 {
            return None;
        }
        if found == kind {
            return self.0.get(at + 8..at + size);
        }
        at = at.checked_add(size)?.checked_add(7)? & !7;
    }
}
```]

The walk reads five types: 1 (the command line), 6 (the memory map), 8 (the framebuffer), and 14 and 15 (the old and new ACPI root pointers). `rsdp` tries 15 first.

== The command line and memory map

`cmdline` takes the tag's body up to its NUL, and returns `""` if that isn't UTF-8. `arg` splits it on whitespace and looks for a word of the form `key=value`, so `arg("tes")` doesn't match `test=slab`, and a bare `quiet` has no value. `cargo xtask kernel --board pc` writes `test=<name>` into GRUB's configuration, and this is how a kernel learns which test to run.

The memory map's body gives the size of one entry, then a version, then the entries. `regions` steps by that size rather than by the 24 bytes it reads, and an entry size under 24 yields nothing. Type 1 is RAM that is free to use, and `available` is that test. `free_end` finds the free range holding an address. The `pc` board calls it with the start of the kernel's heap to learn how far its RAM goes, and with `0x8000` to check that the page where other CPUs start is free.

== The framebuffer

The framebuffer tag gives the address, the pitch (bytes per row), the size and the pixel format:

#bytefield(caption: "The framebuffer tag's body, as read here",
  ("addr", "u64", 8), ("pitch", "u32", 4), ("width", "u32", 4), ("height", "u32", 4), ("bpp", "u8", 1), ("type", "u8", 1), ("pad", "2", 2), ("colors", "6 bytes", 6),
)

#srcfile("drivers/multiboot2/src/lib.rs", lines: ((117, 129),), notes: ((120, [Six bytes of position and width for red, green and blue. 16, 8, 8, 8, 0, 8 is a little-endian `0x00RRGGBB`. The `&&` means a non-RGB tag, which has no such bytes, isn't asked for them.]), (122, [64 bits: the tests read a display above 4 GiB, and the `pc` board maps it with `map_high`.]),))[```rust
pub fn display(&self) -> Option<Display> {
    let b = self.tag(TAG_FRAMEBUFFER)?;
    let (bpp, kind) = (*b.get(20)?, *b.get(21)?);
    let xrgb = kind == FRAMEBUFFER_RGB && bpp == 32 && matches!(b.get(24..30)?, [16, 8, 8, 8, 0, 8]);
    Some(Display {
        addr: u64_at(b, 0)?,
        pitch: u32_at(b, 8)?,
        width: u32_at(b, 12)?,
        height: u32_at(b, 16)?,
        bpp,
        xrgb,
    })
}
```]

`xrgb` is what the rest of the kernel needs to know. A display with red and blue swapped is reported, with `xrgb` false. The `pc` board doesn't use such a display: its kernels draw into plain memory instead (@bare-pc).

== Exercised by

The tests build boot information in a vector, with a helper that pads each tag: a command line, a tag that needs padding, a memory map with three ranges, a display above 4 GiB, a swapped one, both ACPI pointers, a total size past the buffer and a tag shorter than its header.

#output(title: "cargo test -p green-tea-multiboot2")[
```
test result: ok. 8 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```
]

Every boot of the `pc` board uses it. This run got its RAM limit and its screen from GRUB's information, under OVMF:

#output(title: "cargo xtask kernel --board pc --kernel shell --target x86_64 --headless")[
```
green-tea-shell on x86_64 (pc)
framebuffer: 1280x800 XRGB8888
type `help` for the commands
```
]

== Limits

- Exercised only on QEMU with OVMF firmware, through GRUB, and on fake information in the host tests. It has not been run on physical hardware.
- It reads four kinds of information. Modules, the ELF sections, the EFI memory map and every other tag are skipped.
- Only the first tag of a type is seen.
- `from_addr` trusts the first word: it builds a slice of that length before anything has been checked.
- `free_end` adds a range's base and length unchecked, so a map entry that runs past the top of the address space overflows.

Five tags tell a kernel what kind of machine it woke up on. @crate-acpi reads the one that points further.
// chapter-status:begin
// stage: research-done
// revision: 2
// body: 2b4b5d0ac93f
// activity: 2026-11-22 | r2 | stage | green-tea | note | tracking started at early-draft
// activity: 2026-11-23 | r2 | stage | green-tea | note | early-draft -> research-done: not yet reviewed by Javier
// chapter-status:end
