#![no_std]

use green_tea_memory::{OwnedRange, PhysicalRange};

pub const PAGE_SIZE: usize = 4096;
pub const MAX_MAPPINGS: usize = 32;

/// A PCI function's requester identity, before any platform-specific isolation-group check.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeviceId {
    segment: u16,
    bus: u8,
    device: u8,
    function: u8,
}

impl DeviceId {
    pub const fn new(segment: u16, bus: u8, device: u8, function: u8) -> Result<Self, DmaError> {
        if device >= 32 || function >= 8 {
            return Err(DmaError::InvalidDevice);
        }
        Ok(Self {
            segment,
            bus,
            device,
            function,
        })
    }

    pub const fn segment(self) -> u16 {
        self.segment
    }

    pub const fn bus(self) -> u8 {
        self.bus
    }

    pub const fn device(self) -> u8 {
        self.device
    }

    pub const fn function(self) -> u8 {
        self.function
    }
}

/// Permissions granted to the device (not to a CPU process).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DmaPermissions(u8);

impl DmaPermissions {
    pub const DEVICE_READ: Self = Self(1 << 0);
    pub const DEVICE_WRITE: Self = Self(1 << 1);
    pub const READ_WRITE: Self = Self(Self::DEVICE_READ.0 | Self::DEVICE_WRITE.0);

    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    const fn valid(self) -> bool {
        self.0 != 0 && self.0 & !Self::READ_WRITE.0 == 0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DmaError {
    Unsupported,
    InvalidDevice,
    DeviceBusy,
    InvalidRange,
    MappingOverlap,
    Capacity,
    NotQuiesced,
    InvalidMapping,
    Backend,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MappingHandle(u64);

struct Mapping<'memory> {
    id: MappingHandle,
    iova: usize,
    owner: OwnedRange<'memory>,
}

/// Architecture-specific enforcement for one device domain.
///
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

/// An exclusively assigned device with fixed-capacity, explicitly owned DMA buffers.
#[must_use = "a DMA domain must be explicitly quiesced and detached"]
pub struct DmaDomain<'backend, 'memory, B: IommuBackend, const N: usize = MAX_MAPPINGS> {
    backend: &'backend mut B,
    device: DeviceId,
    domain: B::Domain,
    mappings: [Option<Mapping<'memory>>; N],
    next_id: u64,
    running: bool,
    closed: bool,
}

impl<'backend, 'memory, B: IommuBackend, const N: usize> DmaDomain<'backend, 'memory, B, N> {
    pub fn assign(backend: &'backend mut B, device: DeviceId) -> Result<Self, DmaError> {
        if N == 0 {
            return Err(DmaError::Capacity);
        }
        let domain = backend.attach(device)?;
        Ok(Self {
            backend,
            device,
            domain,
            mappings: core::array::from_fn(|_| None),
            next_id: 1,
            running: false,
            closed: false,
        })
    }

    pub const fn device(&self) -> DeviceId {
        self.device
    }

    /// Map an exclusively owned physical region while the device is stopped.
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
        if !permissions.valid()
            || !physical
                .start()
                .is_multiple_of(PAGE_SIZE)
            || !physical
                .len()
                .is_multiple_of(PAGE_SIZE)
        {
            return Err((owner, DmaError::InvalidRange));
        }
        let iova_range = match PhysicalRange::new(iova, physical.len(), PAGE_SIZE) {
            Ok(range) => range,
            Err(_) => return Err((owner, DmaError::InvalidRange)),
        };
        if self
            .mappings
            .iter()
            .flatten()
            .any(|mapping| {
                let existing = PhysicalRange::new(
                    mapping.iova,
                    mapping
                        .owner
                        .range()
                        .len(),
                    PAGE_SIZE,
                )
                .expect("stored IOVA geometry was validated");
                existing.overlaps(iova_range)
                    || mapping
                        .owner
                        .range()
                        .overlaps(physical)
            })
        {
            return Err((owner, DmaError::MappingOverlap));
        }
        let Some(slot) = self
            .mappings
            .iter()
            .position(Option::is_none)
        else {
            return Err((owner, DmaError::Capacity));
        };
        let Some(next_id) = self
            .next_id
            .checked_add(1)
        else {
            return Err((owner, DmaError::Capacity));
        };
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
    }

    pub fn resume(&mut self) -> Result<(), DmaError> {
        if self.running {
            return Ok(());
        }
        self.backend
            .resume(self.domain)?;
        self.running = true;
        Ok(())
    }

    /// Stop the device and wait until it has no outstanding DMA transactions.
    pub fn quiesce(&mut self) -> Result<(), DmaError> {
        if !self.running {
            return Ok(());
        }
        self.backend
            .quiesce(self.domain)?;
        self.running = false;
        Ok(())
    }

    /// Revoke one mapping after quiescence and return its ownership token to the caller.
    pub fn unmap_buffer(&mut self, handle: MappingHandle) -> Result<OwnedRange<'memory>, DmaError> {
        if self.running {
            return Err(DmaError::NotQuiesced);
        }
        let Some(slot) = self
            .mappings
            .iter()
            .position(|mapping| {
                mapping
                    .as_ref()
                    .is_some_and(|m| m.id == handle)
            })
        else {
            return Err(DmaError::InvalidMapping);
        };
        let mapping = self.mappings[slot]
            .as_ref()
            .expect("located mapping");
        self.backend
            .unmap(
                self.domain,
                mapping.iova,
                mapping
                    .owner
                    .range()
                    .len(),
            )?;
        Ok(self.mappings[slot]
            .take()
            .expect("unmap retains mapping on failure")
            .owner)
    }

    /// Quiesce, revoke all mappings and detach the device. On failure, return the still-owned
    /// domain so the caller can retry; dropping that domain intentionally leaks mapped owners.
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
}

impl<B: IommuBackend, const N: usize> Drop for DmaDomain<'_, '_, B, N> {
    fn drop(&mut self) {
        if self.closed {
            return;
        }
        if self.running
            && self
                .backend
                .quiesce(self.domain)
                .is_ok()
        {
            self.running = false;
        }
        if !self.running {
            let mut all_revoked = true;
            for slot in 0..N {
                let Some(mapping) = self.mappings[slot].as_ref() else {
                    continue;
                };
                if self
                    .backend
                    .unmap(
                        self.domain,
                        mapping.iova,
                        mapping
                            .owner
                            .range()
                            .len(),
                    )
                    .is_ok()
                {
                    self.mappings[slot].take();
                } else if let Some(mapping) = self.mappings[slot].take() {
                    all_revoked = false;
                    core::mem::forget(mapping.owner);
                }
            }
            if all_revoked
                && self
                    .mappings
                    .iter()
                    .all(Option::is_none)
            {
                let _ = self
                    .backend
                    .detach(self.domain);
            }
        } else {
            for mapping in &mut self.mappings {
                if let Some(mapping) = mapping.take() {
                    core::mem::forget(mapping.owner);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::{
        DeviceId, DmaDomain, DmaError, DmaPermissions, IommuBackend, MappingHandle, PAGE_SIZE,
    };
    use green_tea_memory::{OwnedRange, PhysicalRange};

    const DEVICE: DeviceId = DeviceId {
        segment: 0,
        bus: 0,
        device: 2,
        function: 0,
    };
    const FIRST: usize = 0x1000_0000;
    const SECOND: usize = 0x1000_4000;

    struct Fake {
        mappings: [(usize, PhysicalRange, DmaPermissions); 4],
        used: usize,
        attached: bool,
        running: bool,
        quiesces: usize,
        fail_unmap: bool,
    }

    impl Fake {
        fn new() -> Self {
            Self {
                mappings: [(
                    0,
                    PhysicalRange::new(FIRST, PAGE_SIZE, PAGE_SIZE).unwrap(),
                    DmaPermissions::DEVICE_READ,
                ); 4],
                used: 0,
                attached: false,
                running: false,
                quiesces: 0,
                fail_unmap: false,
            }
        }
    }

    // SAFETY: this fake records mappings in an array and has no device capable of DMA.
    unsafe impl IommuBackend for Fake {
        type Domain = u8;

        fn attach(&mut self, _: DeviceId) -> Result<Self::Domain, DmaError> {
            if self.attached {
                return Err(DmaError::DeviceBusy);
            }
            self.attached = true;
            Ok(1)
        }

        fn map(
            &mut self,
            _: Self::Domain,
            iova: usize,
            physical: PhysicalRange,
            access: DmaPermissions,
        ) -> Result<(), DmaError> {
            if self.used
                == self
                    .mappings
                    .len()
            {
                return Err(DmaError::Capacity);
            }
            self.mappings[self.used] = (iova, physical, access);
            self.used += 1;
            Ok(())
        }

        fn quiesce(&mut self, _: Self::Domain) -> Result<(), DmaError> {
            self.running = false;
            self.quiesces += 1;
            Ok(())
        }

        fn resume(&mut self, _: Self::Domain) -> Result<(), DmaError> {
            self.running = true;
            Ok(())
        }

        fn unmap(&mut self, _: Self::Domain, iova: usize, _: usize) -> Result<(), DmaError> {
            if self.fail_unmap {
                return Err(DmaError::Backend);
            }
            let Some(index) = self.mappings[..self.used]
                .iter()
                .position(|entry| entry.0 == iova)
            else {
                return Err(DmaError::InvalidMapping);
            };
            self.mappings[index] = self.mappings[self.used - 1];
            self.used -= 1;
            Ok(())
        }

        fn detach(&mut self, _: Self::Domain) -> Result<(), DmaError> {
            if self.running || self.used != 0 {
                return Err(DmaError::Backend);
            }
            self.attached = false;
            Ok(())
        }
    }

    fn owned(base: usize) -> OwnedRange<'static> {
        // SAFETY: these are distinct, page-aligned test ranges and the fake backend never
        // dereferences them or performs hardware DMA.
        unsafe { OwnedRange::new(PhysicalRange::new(base, PAGE_SIZE, PAGE_SIZE).unwrap()) }
    }

    fn domain(fake: &mut Fake) -> DmaDomain<'_, 'static, Fake, 4> {
        DmaDomain::assign(fake, DEVICE).unwrap()
    }

    #[test]
    fn device_ids_validate_pci_geometry() {
        assert!(DeviceId::new(0, 0, 31, 7).is_ok());
        assert_eq!(DeviceId::new(0, 0, 32, 0), Err(DmaError::InvalidDevice));
        assert_eq!(DeviceId::new(0, 0, 0, 8), Err(DmaError::InvalidDevice));
    }

    #[test]
    fn mappings_require_page_geometry_and_nonempty_permissions() {
        let mut fake = Fake::new();
        let mut domain = domain(&mut fake);
        let (owner, error) = domain
            .map_buffer(owned(FIRST), 0x3001, DmaPermissions::DEVICE_READ)
            .unwrap_err();
        assert_eq!(error, DmaError::InvalidRange);
        assert_eq!(
            owner
                .into_range()
                .start(),
            FIRST
        );
        let (owner, error) = domain
            .map_buffer(owned(FIRST), 0x3000, DmaPermissions(0))
            .unwrap_err();
        assert_eq!(error, DmaError::InvalidRange);
        drop(owner);
    }

    #[test]
    fn overlaps_and_capacity_are_rejected_before_backend_mapping() {
        let mut fake = Fake::new();
        let mut domain = domain(&mut fake);
        let first = domain
            .map_buffer(owned(FIRST), 0x3000, DmaPermissions::DEVICE_READ)
            .unwrap_or_else(|(_, error)| panic!("map failed: {error:?}"));
        let (_, error) = domain
            .map_buffer(owned(SECOND), 0x3000, DmaPermissions::DEVICE_READ)
            .unwrap_err();
        assert_eq!(error, DmaError::MappingOverlap);
        assert_eq!(
            domain
                .backend
                .used,
            1
        );
        assert_eq!(first, MappingHandle(1));
    }

    #[test]
    fn mappings_can_be_revoked_only_after_quiescence() {
        let mut fake = Fake::new();
        let mut domain = domain(&mut fake);
        let handle = domain
            .map_buffer(owned(FIRST), 0x3000, DmaPermissions::READ_WRITE)
            .unwrap_or_else(|(_, error)| panic!("map failed: {error:?}"));
        domain
            .resume()
            .unwrap();
        assert_eq!(
            domain
                .unmap_buffer(handle)
                .err(),
            Some(DmaError::NotQuiesced)
        );
        domain
            .quiesce()
            .unwrap();
        assert_eq!(
            domain
                .unmap_buffer(handle)
                .unwrap()
                .into_range()
                .start(),
            FIRST
        );
        assert_eq!(
            domain
                .backend
                .used,
            0
        );
        assert_eq!(
            domain
                .backend
                .quiesces,
            1
        );
    }

    #[test]
    fn teardown_quiesces_revokes_and_detaches() {
        let mut fake = Fake::new();
        let mut domain = domain(&mut fake);
        domain
            .map_buffer(owned(FIRST), 0x3000, DmaPermissions::DEVICE_WRITE)
            .unwrap_or_else(|(_, error)| panic!("map failed: {error:?}"));
        domain
            .resume()
            .unwrap();
        assert!(
            domain
                .teardown()
                .is_ok()
        );
        assert!(!fake.attached);
        assert_eq!(fake.used, 0);
        assert_eq!(fake.quiesces, 1);
    }

    #[test]
    fn failed_revoke_keeps_the_owned_mapping_for_retry() {
        let mut fake = Fake::new();
        let mut domain = domain(&mut fake);
        let handle = domain
            .map_buffer(owned(FIRST), 0x3000, DmaPermissions::DEVICE_READ)
            .unwrap_or_else(|(_, error)| panic!("map failed: {error:?}"));
        domain
            .quiesce()
            .unwrap();
        domain
            .backend
            .fail_unmap = true;
        assert_eq!(
            domain
                .unmap_buffer(handle)
                .err(),
            Some(DmaError::Backend)
        );
        assert!(domain.mappings[0].is_some());
        domain
            .backend
            .fail_unmap = false;
        assert_eq!(
            domain
                .unmap_buffer(handle)
                .unwrap()
                .into_range()
                .start(),
            FIRST
        );
    }
}
