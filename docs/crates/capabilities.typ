#import "../lib.typ": *

= green-tea-capabilities <crate-capabilities>

`green-tea-capabilities` names what a kernel build promises and what a board delivers, and checks one against the other. A promise is a *profile*: Green, the kernels as they have always been, with every task in one address space; Black, where code also runs in protected user processes; and White, an addition to either that says the build runs the realtime scheduler. A delivery is a set of *capabilities*: page tables, guard pages, protected address spaces, and so on. A Black kernel on a board without protected address spaces stops at boot instead of running unprotected.

#spec(
  ("Source", [#repo("green-tea-capabilities/src/lib.rs"), 221 lines, 140 without its tests]),
  ("Depends on", [nothing; `no_std`. Cargo features `green` (the default), `black` and `white`]),
  ("Used by", [@crate-memory, `green-tea-bare` (every board's `CAPABILITIES`, the check in `boot`, the user processes of @crate-black)]),
  ("Tests", [6 host tests; the profile check runs in every kernel's `boot`]),
)

#dtable(columns: (auto, 1fr), ("Item", "What it does"),
  [`Capability`], [one guarantee: `PageTables`, `MemoryProtection`, `GuardPages`, `DemandPaging`, `ProtectedAddressSpaces`, `DmaIsolation`, `RealtimeExecution`],
  [`Capabilities(u8)`], [a set of them, one bit each: `NONE`, `PAGE_TABLES`, `MEMORY_PROTECTION`, `GUARD_PAGES`, `DEMAND_PAGING`, `PROTECTED_ADDRESS_SPACES`, `DMA_ISOLATION`, `REALTIME_EXECUTION`],
  [`union(other)`], [both sets],
  [`supports(capability)`], [whether the bit is set],
  [`require(capability)`], [`Ok(())`, or `Err(UnsupportedCapability)` naming the missing one],
  [`UnsupportedCapability`], [the error: a public `capability` field],
  [`BaseProfile`], [`Green` or `Black`],
  [`Profile`], [a base and a `realtime` flag; `GREEN`, `BLACK`, `GREEN_WHITE`, `BLACK_WHITE`],
  [`Profile::name()`], [`"Green"`, `"Black"`, `"Green+White"` or `"Black+White"`, for messages],
  [`Profile::validate(capabilities)`], [the first guarantee the profile needs that the set lacks],
  [`BUILD_PROFILE`], [the profile the Cargo features chose; present only when exactly one base is on],
)

== Guarantees, not hardware

A capability names something the board's code *implements*, not something the CPU has. Every `x86_64` CPU has page tables and privilege rings, but only a backend that builds a separate address space per process and keeps the kernel's mappings out of reach of user code can claim `ProtectedAddressSpaces`. The doc comment on `Capability` says so, and one test is named after it: `page_tables_do_not_imply_protected_address_spaces`.

Every method is a `const fn`, so a board's set and a profile's check can be constants. `require` turns a missing bit into an error that names it:

#srcfile("green-tea-capabilities/src/lib.rs", lines: ((35, 49),), notes: ((40, [`bit` maps each `Capability` to its constant's bit, by a `match`, so the enum and the set can't drift apart silently: a new variant without a bit doesn't compile.]),))[```rust
pub const fn union(self, other: Self) -> Self {
    Self(self.0 | other.0)
}

pub const fn supports(self, capability: Capability) -> bool {
    self.0 & Self::bit(capability) != 0
}

pub const fn require(self, capability: Capability) -> Result<(), UnsupportedCapability> {
    if self.supports(capability) {
        Ok(())
    } else {
        Err(UnsupportedCapability { capability })
    }
}
```]

The board sets in `green-tea-bare` at this commit are:

#dtable(columns: (auto, 1fr), ("Board", "`CAPABILITIES`"),
  [`aarch64` `virt`], [`NONE`: the MMU is off],
  [`riscv64` `virt`], [`PAGE_TABLES`, `GUARD_PAGES`, `DEMAND_PAGING`],
  [`x86_64` `q35` and `pc`, Green], [`PAGE_TABLES`, `GUARD_PAGES`, `DEMAND_PAGING`],
  [`x86_64` `q35` and `pc`, Black], [the same, plus `PROTECTED_ADDRESS_SPACES`],
)

No board claims `MemoryProtection` or `DmaIsolation`. `green-tea-bare` derives its older `GUARD_PAGES` and `DEMAND_PAGING` booleans from the set, and adds `REALTIME_EXECUTION` when the build has the realtime runtime (its `rt-runtime` feature, which @crate-rt-bare turns on).

== Profiles

`validate` checks the base first and the realtime addition second, so a Black+White build on a board with neither reports the missing isolation:

#srcfile("green-tea-capabilities/src/lib.rs", lines: ((111, 123),), notes: ((112, [Green asks for nothing. The Green kernels ran before any of this existed, and still do on `aarch64` with an empty set.]), (117, [White is additive: it adds one requirement to whichever base it is on.]),))[```rust
pub const fn validate(self, capabilities: Capabilities) -> Result<(), UnsupportedCapability> {
    if matches!(self.base, BaseProfile::Black) {
        if let Err(missing) = capabilities.require(Capability::ProtectedAddressSpaces) {
            return Err(missing);
        }
    }
    if self.realtime {
        if let Err(missing) = capabilities.require(Capability::RealtimeExecution) {
            return Err(missing);
        }
    }
    Ok(())
}
```]

The `if let` chains stand in for `?`, which a `const fn` can't use.

`BUILD_PROFILE` is chosen by features. `green` is on by default, a kernel that wants Black turns off default features and turns on `black`, and `white` sets the `realtime` flag. Both bases at once is a compile error. Neither base leaves `BUILD_PROFILE` undefined, which is how @crate-memory depends on this crate without choosing a profile; `green-tea-bare` refuses that case with its own `compile_error!`.

#srcfile("green-tea-capabilities/src/lib.rs", lines: ((3, 4), (126, 139),))[```rust
#[cfg(all(feature = "green", feature = "black"))]
compile_error!("Green and Black are mutually exclusive base profiles");
// …
#[cfg(all(feature = "green", not(feature = "black")))]
const BUILD_BASE: BaseProfile = BaseProfile::Green;
#[cfg(all(feature = "black", not(feature = "green")))]
const BUILD_BASE: BaseProfile = BaseProfile::Black;

/// The profile selected by Cargo features for this build; `white` adds realtime to its base.
#[cfg(any(
    all(feature = "green", not(feature = "black")),
    all(feature = "black", not(feature = "green"))
))]
pub const BUILD_PROFILE: Profile = Profile {
    base: BUILD_BASE,
    realtime: cfg!(feature = "white"),
};
```]

== The check at boot

`green-tea-bare`'s `boot` brings the board up, and then, before the banner, checks the build's profile against the board's set and the runtime's. A failure panics with the profile's name and the missing guarantee, so a Black kernel built for `riscv64` would stop with `Black profile requires unsupported capability ProtectedAddressSpaces`:

#srcfile("green-tea-bare/src/lib.rs", lines: ((86, 92), (100, 103),), notes: ((91, [The second check, of the heap and stack regions, is @crate-memory's.]), (101, [The realtime runtime is a capability of the build, not of the board.]),))[```rust
pub fn boot(kernel: &str) -> Framebuffer {
    Board::init();
    capabilities::BUILD_PROFILE
        .validate(Board::CAPABILITIES.union(RUNTIME_CAPABILITIES))
        .unwrap_or_else(|missing| panic!("{} profile requires unsupported capability {:?}",
»            capabilities::BUILD_PROFILE.name(), missing.capability));
    memory::validate_kernel_regions()
        .unwrap_or_else(|error| panic!("{} kernel memory regions are unsupported: {error:?}",
»            Board::NAME));
// …
#[cfg(feature = "rt-runtime")]
const RUNTIME_CAPABILITIES: capabilities::Capabilities =
»            capabilities::Capabilities::REALTIME_EXECUTION;
#[cfg(not(feature = "rt-runtime"))]
const RUNTIME_CAPABILITIES: capabilities::Capabilities = capabilities::Capabilities::NONE;
```]

`green-tea-bare` re-exports the crate as `green_tea_bare::capabilities`, and its boards also consult `BUILD_PROFILE` directly: under Black, the `q35` board refuses to hand out virtio PCI devices and the `pc` board refuses its USB and NVMe controllers, because nothing confines their DMA (@crate-dma, @crate-pci).

== Exercised by

The host tests check that Green needs nothing, that page tables alone don't satisfy Black, that White needs the realtime runtime, the order of Black+White's two checks, that `DmaIsolation` is asked for separately, and that the features select the expected `BUILD_PROFILE`. The last test runs under whichever features the test build has, so we run the suite twice:

#output(title: "cargo test -p green-tea-capabilities")[
```
test tests::black_white_checks_process_isolation_before_realtime ... ok
test tests::cargo_selects_a_static_profile ... ok
test tests::dma_isolation_is_requested_separately ... ok
test tests::green_profile_needs_no_protection_backend ... ok
test tests::page_tables_do_not_imply_protected_address_spaces ... ok
test tests::white_requires_the_realtime_runtime ... ok
test result: ok. 6 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```
]

#output(title: "cargo test -p green-tea-capabilities --no-default-features --features black,white")[
```
test result: ok. 6 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```
]

Adding `black` to the default `green` doesn't build:

#output(title: "cargo build -p green-tea-capabilities --features black")[
```
error: Green and Black are mutually exclusive base profiles
```
]

Every kernel runs the check in `boot`, so every recorded run in this report passed it; the Black one is @crate-black's.

== Limits

- The check is only as true as the board's set, which is a constant a person wrote. On `x86_64` the set includes `PROTECTED_ADDRESS_SPACES` exactly when the build is Black, so there the check can't fail; it rejects Black only on `riscv64` and `aarch64`.
- `MemoryProtection` and `DmaIsolation` are named and claimed by no board. `Black+White` doesn't build: `green-tea-bare` has a `compile_error!` for Black with the realtime runtime.
- The profile is fixed at compile time; one image can't be Green on one machine and Black on another.
- The Black profile has run only in QEMU.

Seven bits and a `match`. @crate-memory uses them to decide which requests on a region of memory a board can honour.
// chapter-status:begin
// stage: research-done
// revision: 1
// body: 49432b2460e0
// activity: 2026-11-27 | r1 | stage | green-tea | note | tracking started at research-done
// chapter-status:end
