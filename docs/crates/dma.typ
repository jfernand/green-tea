#import "../lib.typ": *

= green-tea-dma <crate-dma>

A device that does DMA reads and writes memory without the CPU, at addresses the driver gives it. Page tables don't apply to it: a disk controller told to write at address 0 writes at address 0, whatever process or kernel owns it. The hardware that confines a device is an *IOMMU* (Intel's VT-d, AMD-Vi), which translates each device's addresses through tables of its own, so a device sees only the buffers mapped for it. `green-tea-dma` is the contract a driver would use with such a unit: a *domain* for one device, into which the driver maps buffers it owns, as @crate-memory tokens, and from which it gets them back only after the device has stopped. It contains no IOMMU driver. The crate defines the trait one would implement, and the rules around it.

#spec(
  ("Source", [#repo("drivers/dma/src/lib.rs"), 633 lines, 371 without its tests]),
  ("Depends on", [@crate-memory; `no_std`]),
  ("Used by", [nothing yet: no `IommuBackend` exists, which is why the Black profile refuses DMA devices (@crate-pci)]),
  ("Tests", [6 host tests, against a fake backend that records mappings in an array]),
)

#dtable(columns: (auto, 1fr), ("Item", "What it does"),
  [`PAGE_SIZE`, `MAX_MAPPINGS`], [4096, and the default number of mappings a domain holds, 32],
  [`DeviceId::new(segment, bus, device, function)`], [a PCI requester id, with `device` below 32 and `function` below 8; `segment()`, `bus()`, `device()`, `function()` read it back],
  [`DmaPermissions`], [what the *device* may do: `DEVICE_READ`, `DEVICE_WRITE`, `READ_WRITE`, and `union`],
  [`DmaError`], [`Unsupported`, `InvalidDevice`, `DeviceBusy`, `InvalidRange`, `MappingOverlap`, `Capacity`, `NotQuiesced`, `InvalidMapping`, `Backend`],
  [`MappingHandle`], [names one mapping in its domain],
  [`IommuBackend`], [an `unsafe` trait: `attach`, `map`, `quiesce`, `resume`, `unmap`, `detach`, over an associated `Domain` type],
  [`DmaDomain<'backend, 'memory, B, N>`], [one device's domain, holding up to `N` mappings; `assign(backend, device)`, `device()`],
  [`map_buffer(owner, iova, permissions)`], [map an owned range at a device address; on failure the owner comes back with the error],
  [`resume()`, `quiesce()`], [let the device run; stop it and drain its DMA],
  [`unmap_buffer(handle)`], [revoke one mapping and return its owner, only while stopped],
  [`teardown()`], [stop, revoke everything and detach; on failure the domain comes back for a retry],
)

== The backend contract

`IommuBackend` is what an architecture would implement: Intel VT-d, AMD-Vi, or an Arm SMMU. The trait is `unsafe` to implement because the domain's guarantees are only as good as the backend's promises, and they are the comment above it:

#srcfile("drivers/dma/src/lib.rs", lines: ((89, 113),), size: 6.8pt, notes: ((90, [An IOMMU translates per *isolation group*: devices behind one PCIe bridge without access control can reach each other's mappings. Attaching one device of a shared group is refused.]), (92, [IOTLB: the IOMMU's cache of translations. A mapping that isn't yet visible, or an unmap the cache still remembers, leaves the device with stale access.]), (98, [`Copy`: a small handle, such as a domain number.]),))[```rust
/// # Safety
/// Implementations must verify that the device is isolated from other devices or explicitly
/// reject assignment. A successful `map` must install only the requested translation and
/// complete the required IOTLB invalidation before returning. `quiesce` must stop the device
/// and drain all in-flight DMA before returning. A successful `unmap` must remove and
/// invalidate the translation. A failed `map` must leave the range unmapped; a failed
/// `unmap` must leave the domain attached and the range owned by the caller. `detach` must
/// prevent the requester from using this domain.
pub unsafe trait IommuBackend {
    type Domain: Copy;

    /// Attach only if the device's isolation group is known and exclusively owned.
    fn attach(&mut self, device: DeviceId) -> Result<Self::Domain, DmaError>;
    fn map(
        &mut self,
        domain: Self::Domain,
        iova: usize,
        physical: PhysicalRange,
        permissions: DmaPermissions,
    ) -> Result<(), DmaError>;
    fn quiesce(&mut self, domain: Self::Domain) -> Result<(), DmaError>;
    fn resume(&mut self, domain: Self::Domain) -> Result<(), DmaError>;
    fn unmap(&mut self, domain: Self::Domain, iova: usize, size: usize) -> Result<(), DmaError>;
    fn detach(&mut self, domain: Self::Domain) -> Result<(), DmaError>;
}
```]

The *IOVA* (I/O virtual address) is the address the device uses. It needn't equal the physical one; the domain maps one to the other.

== A domain

`DmaDomain` borrows the backend mutably for `'backend` and holds the owners of its mapped buffers for `'memory`, in a fixed array of `N` slots, so it needs no heap. A new domain is stopped (`running: false`), and every change to its mappings needs it stopped. That is the whole discipline: *mappings change only while the device is stopped*, so a buffer never changes hands while the device might be using it.

`map_buffer` takes the `OwnedRange` by value, so once a buffer is mapped the driver no longer has its token. It checks everything it can before the backend touches the tables: the domain is stopped, the permissions are one of the three valid sets, the physical range is whole pages, the IOVA is page-aligned, and neither the IOVA range nor the physical range overlaps an existing mapping. Any failure returns the owner with the error, so a refused buffer isn't lost:

#srcfile("drivers/dma/src/lib.rs", lines: ((149, 158), (167, 172), (209, 222),), size: 6.8pt, notes: ((154, [The error type carries the token back. A failed map never consumes the caller's ownership.]), (169, [The IOVA range gets the same checks as a physical one, in `PhysicalRange::new`.]), (209, [The handle is the next id; ids are never reused.]), (216, [Only after the backend succeeded does the domain take the owner.]),))[```rust
pub fn map_buffer(
    &mut self,
    owner: OwnedRange<'memory>,
    iova: usize,
    permissions: DmaPermissions,
) -> Result<MappingHandle, (OwnedRange<'memory>, DmaError)> {
    if self.running {
        return Err((owner, DmaError::NotQuiesced));
    }
    let physical = owner.range();
// …
        return Err((owner, DmaError::InvalidRange));
    }
    let iova_range = match PhysicalRange::new(iova, physical.len(), PAGE_SIZE) {
        Ok(range) => range,
        Err(_) => return Err((owner, DmaError::InvalidRange)),
    };
// …
    let handle = MappingHandle(self.next_id);
    if let Err(error) = self
        .backend
        .map(self.domain, iova, physical, permissions)
    {
        return Err((owner, error));
    }
    self.mappings[slot] = Some(Mapping {
        id: handle,
        iova,
        owner,
    });
    self.next_id = next_id;
    Ok(handle)
```]

`resume` and `quiesce` call the backend and track `running`; each does nothing if the domain is already in that state. `unmap_buffer` refuses while running, finds the handle, asks the backend to unmap, and only if that succeeds takes the mapping out of its slot and returns the owner. A failed unmap leaves the mapping where it was, and the caller can retry.

== Teardown and drop

A domain is `#[must_use]`, and the intended end is `teardown`: quiesce, unmap each mapping, detach. Each step can fail, and on failure `teardown` returns the domain itself with the error, in a consistent state: the mappings already revoked are gone, the rest are still owned by the domain.

#srcfile("drivers/dma/src/lib.rs", lines: ((282, 312),), size: 6.6pt, notes: ((286, [`N` is a constant, so this is a loop over the fixed array of slots.]), (301, [The slot is emptied only after its unmap succeeded.]), (310, [`closed` tells `Drop` there is nothing left to do.]),))[```rust
pub fn teardown(mut self) -> Result<(), (Self, DmaError)> {
    if let Err(error) = self.quiesce() {
        return Err((self, error));
    }
    for slot in 0..N {
        if let Some(mapping) = self.mappings[slot].as_ref() {
            if let Err(error) = self
                .backend
                .unmap(
                    self.domain,
                    mapping.iova,
                    mapping
                        .owner
                        .range()
                        .len(),
                )
            {
                return Err((self, error));
            }
            self.mappings[slot].take();
        }
    }
    if let Err(error) = self
        .backend
        .detach(self.domain)
    {
        return Err((self, error));
    }
    self.closed = true;
    Ok(())
}
```]

A domain dropped without a successful `teardown` does the same work on a best-effort basis in `Drop`. It quiesces if it can; if the device is stopped, it unmaps each mapping, and detaches only if every unmap succeeded. A mapping it can't revoke, or every mapping if the device wouldn't stop, has its owner passed to `core::mem::forget`, so the token is never handed back to anyone as if the memory were free again.

== Exercised by

The tests use `Fake`, a backend whose `map` records the mapping in a four-entry array, whose `detach` fails while it is running or still has mappings, and whose `unmap` can be told to fail. They check the PCI geometry of `DeviceId`, that a misaligned IOVA and empty permissions are refused with the owner returned, that an overlapping IOVA is refused before the backend sees it, that `unmap_buffer` waits for `quiesce`, that `teardown` leaves the fake detached and empty after one quiesce, and that a failed revoke keeps the mapping for a retry that then succeeds:

#output(title: "cargo test -p green-tea-dma")[
```
test tests::device_ids_validate_pci_geometry ... ok
test tests::failed_revoke_keeps_the_owned_mapping_for_retry ... ok
test tests::mappings_can_be_revoked_only_after_quiescence ... ok
test tests::mappings_require_page_geometry_and_nonempty_permissions ... ok
test tests::overlaps_and_capacity_are_rejected_before_backend_mapping ... ok
test tests::teardown_quiesces_revokes_and_detaches ... ok
test result: ok. 6 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```
]

No kernel uses the crate. What the kernels do instead is fail closed: a Black build refuses every DMA device it would otherwise drive, as @crate-black's run on the `pc` board shows with `usb: refusing DMA-capable assignment without IOMMU enforcement`.

== Limits

- No `IommuBackend` is implemented, for any architecture, and no board claims `DmaIsolation` (@crate-capabilities). Green kernels still give their devices bus mastering over all of memory (@crate-pci).
- The test named `overlaps_and_capacity_are_rejected_before_backend_mapping` checks an overlap only; nothing tests a full domain (`DmaError::Capacity`), the overlap of physical ranges, or the leak path in `Drop`. No code returns `DmaError::Unsupported`.
- Forgetting an `OwnedRange` doesn't hold back its memory. The token never owned the allocation (@crate-memory), so an owner that a dropped domain couldn't revoke can still be freed by whoever allocated it, while the device keeps its mapping.
- `assign` doesn't stop the device: a new domain is marked stopped on the assumption that the device hasn't been allowed to run yet.
- Mapping and unmapping need the whole device stopped, which suits setup and teardown, not a driver that recycles buffers while it runs.

A trait with six methods, a fixed array, and a rule about when it may change. @crate-black is the first kernel built on the profile that needs it.
// chapter-status:begin
// stage: research-done
// revision: 1
// body: 73effdaf1608
// activity: 2026-11-27 | r1 | stage | green-tea | note | tracking started at research-done
// chapter-status:end
