#![no_std]

use core::marker::PhantomData;
use green_tea_capabilities::{Capabilities, Capability};

/// A checked half-open physical-address range.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PhysicalRange {
    start: usize,
    end: usize,
}

impl PhysicalRange {
    /// Create a nonempty range whose start satisfies `alignment`.
    pub const fn new(start: usize, size: usize, alignment: usize) -> Result<Self, RegionError> {
        if size == 0 || alignment == 0 || !alignment.is_power_of_two() || start % alignment != 0 {
            return Err(RegionError::InvalidRange);
        }
        let Some(end) = start.checked_add(size) else {
            return Err(RegionError::InvalidRange);
        };
        Ok(Self { start, end })
    }

    /// Create a range from its exclusive bounds, checking alignment and ordering.
    pub const fn from_bounds(
        start: usize,
        end: usize,
        alignment: usize,
    ) -> Result<Self, RegionError> {
        let Some(size) = end.checked_sub(start) else {
            return Err(RegionError::InvalidRange);
        };
        Self::new(start, size, alignment)
    }

    pub const fn start(self) -> usize {
        self.start
    }

    pub const fn end(self) -> usize {
        self.end
    }

    pub const fn len(self) -> usize {
        self.end - self.start
    }

    pub const fn is_empty(self) -> bool {
        self.start == self.end
    }

    pub const fn contains(self, other: Self) -> bool {
        self.start <= other.start && other.end <= self.end
    }

    pub const fn overlaps(self, other: Self) -> bool {
        self.start < other.end && other.start < self.end
    }

    pub const fn contains_address(self, address: usize) -> bool {
        self.start <= address && address < self.end
    }

    pub const fn as_range(self) -> core::ops::Range<usize> {
        self.start..self.end
    }
}

/// A unique ownership claim for a physical region.
///
/// This type is only a contract token: it does not allocate or free memory, and it exposes
/// no raw pointer or safe dereference operation. Backends create it after transferring an
/// allocated range to the caller; dropping the token ends the claim but does not release the
/// allocation to its source.
///
/// A request borrows the claim, so it cannot outlive or be used after the owner is dropped:
///
/// ```compile_fail
/// use green_tea_memory::{OwnedRange, PhysicalRange, Residency};
/// let range = PhysicalRange::new(0x1000, 0x1000, 0x1000).unwrap();
/// let mut owned = unsafe { OwnedRange::new(range) };
/// let request = owned.request(Residency::Resident, None);
/// drop(owned);
/// request.range();
/// ```
pub struct OwnedRange<'a> {
    range: PhysicalRange,
    _ownership: PhantomData<&'a mut [u8]>,
}

impl<'a> OwnedRange<'a> {
    /// Claim a range whose backing memory is owned by the caller for `'a`.
    ///
    /// # Safety
    /// `range` must describe valid RAM that is exclusively owned by this claim for the
    /// entire lifetime `'a`. No independent overlapping `OwnedRange` may exist; a child made
    /// with [`subrange`](Self::subrange) is allowed only while the parent is mutably borrowed.
    /// The backing allocation must not be released while this value or one of its borrowed
    /// requests is live. This constructor does not make the range safe to dereference.
    pub const unsafe fn new(range: PhysicalRange) -> Self {
        Self {
            range,
            _ownership: PhantomData,
        }
    }

    pub const fn range(&self) -> PhysicalRange {
        self.range
    }

    /// End this ownership claim and pass its physical range back to the caller.
    ///
    /// This does not release the backing allocation; the caller must transfer it to its
    /// allocator or another owner before the range can be claimed again.
    pub fn into_range(self) -> PhysicalRange {
        self.range
    }

    /// Borrow a contained subrange without duplicating its ownership claim.
    pub fn subrange(&mut self, range: PhysicalRange) -> Result<OwnedRange<'_>, RegionError> {
        if !self
            .range
            .contains(range)
        {
            return Err(RegionError::InvalidRange);
        }
        // SAFETY: the parent claim proves this range is valid and exclusive, and the
        // returned lifetime is bounded by the mutable borrow of the parent.
        Ok(unsafe { OwnedRange::new(range) })
    }

    /// Describe a residency or permission operation on this owned range.
    pub fn request(
        &mut self,
        residency: Residency,
        permissions: Option<Permissions>,
    ) -> RegionRequest<'_> {
        RegionRequest {
            range: self.range,
            residency,
            permissions,
            _ownership: PhantomData,
        }
    }
}

/// Whether a backend must keep the range resident, fault it in on demand, or leave it
/// unmapped (for example, as a stack guard).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Residency {
    Resident,
    DemandPaged,
    Unmapped,
}

/// Explicit access permissions requested from a backend that can enforce them.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Permissions(u8);

impl Permissions {
    pub const NONE: Self = Self(0);
    pub const READ: Self = Self(1 << 0);
    pub const WRITE: Self = Self(1 << 1);
    pub const EXECUTE: Self = Self(1 << 2);
    pub const READ_WRITE: Self = Self(Self::READ.0 | Self::WRITE.0);

    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

/// A validated operation requested for an owned range.
pub struct RegionRequest<'a> {
    range: PhysicalRange,
    residency: Residency,
    permissions: Option<Permissions>,
    _ownership: PhantomData<&'a mut [u8]>,
}

impl RegionRequest<'_> {
    pub const fn range(&self) -> PhysicalRange {
        self.range
    }

    pub const fn residency(&self) -> Residency {
        self.residency
    }

    pub const fn permissions(&self) -> Option<Permissions> {
        self.permissions
    }

    /// Reject requests that the selected backend cannot enforce.
    pub const fn validate(&self, capabilities: Capabilities) -> Result<(), RegionError> {
        match self.residency {
            Residency::Resident => {}
            Residency::DemandPaged => {
                if let Err(missing) = capabilities.require(Capability::DemandPaging) {
                    return Err(RegionError::Unsupported(missing.capability));
                }
            }
            Residency::Unmapped => {
                if self
                    .permissions
                    .is_some()
                {
                    return Err(RegionError::InvalidRange);
                }
                if let Err(missing) = capabilities.require(Capability::GuardPages) {
                    return Err(RegionError::Unsupported(missing.capability));
                }
            }
        }
        if self
            .permissions
            .is_some()
        {
            if let Err(missing) = capabilities.require(Capability::MemoryProtection) {
                return Err(RegionError::Unsupported(missing.capability));
            }
        }
        Ok(())
    }
}

/// Why a region request or allocation was rejected.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RegionError {
    InvalidRange,
    Unsupported(Capability),
    OutOfMemory,
}

#[cfg(test)]
mod tests {
    use super::{OwnedRange, Permissions, PhysicalRange, RegionError, Residency};
    use green_tea_capabilities::{Capabilities, Capability};

    #[test]
    fn ranges_check_alignment_bounds_and_overflow() {
        let aligned = PhysicalRange::new(0x1000, 0x2000, 0x1000).unwrap();
        assert_eq!(aligned.as_range(), 0x1000..0x3000);
        assert_eq!(
            PhysicalRange::new(0x1001, 0x1000, 0x1000),
            Err(RegionError::InvalidRange)
        );
        assert_eq!(
            PhysicalRange::new(0x1000, 0, 0x1000),
            Err(RegionError::InvalidRange)
        );
        assert_eq!(
            PhysicalRange::new(usize::MAX - 0xfff, 0x2000, 0x1000),
            Err(RegionError::InvalidRange)
        );
        assert_eq!(
            PhysicalRange::new(0x1000, 0x1000, 3),
            Err(RegionError::InvalidRange)
        );
    }

    #[test]
    fn ranges_validate_containment() {
        let whole = PhysicalRange::new(0x1000, 0x4000, 0x1000).unwrap();
        let inside = PhysicalRange::new(0x2000, 0x1000, 0x1000).unwrap();
        let outside = PhysicalRange::new(0x5000, 0x2000, 0x1000).unwrap();
        assert!(whole.contains(inside));
        assert!(!whole.contains(outside));
        assert!(whole.overlaps(inside));
        assert!(!whole.overlaps(outside));
        assert!(whole.contains_address(0x4fff));
        assert!(!whole.contains_address(0x5000));
    }

    #[test]
    fn subranges_cannot_escape_the_owned_range() {
        let whole = PhysicalRange::new(0x1000, 0x4000, 0x1000).unwrap();
        let outside = PhysicalRange::new(0x4000, 0x2000, 0x1000).unwrap();
        let mut owned = unsafe { OwnedRange::new(whole) };
        assert_eq!(
            owned
                .subrange(outside)
                .err(),
            Some(RegionError::InvalidRange)
        );
        assert_eq!(
            owned
                .subrange(PhysicalRange::new(0x2000, 0x1000, 0x1000).unwrap())
                .unwrap()
                .range(),
            PhysicalRange::new(0x2000, 0x1000, 0x1000).unwrap()
        );
    }

    #[test]
    fn resident_identity_regions_need_no_protection_backend() {
        let range = PhysicalRange::new(0x1000, 0x1000, 0x1000).unwrap();
        let mut owned = unsafe { OwnedRange::new(range) };
        assert_eq!(
            owned
                .request(Residency::Resident, None)
                .validate(Capabilities::NONE),
            Ok(())
        );
    }

    #[test]
    fn unavailable_mapping_and_permission_requests_fail_explicitly() {
        let range = PhysicalRange::new(0x1000, 0x1000, 0x1000).unwrap();
        let mut owned = unsafe { OwnedRange::new(range) };
        assert_eq!(
            owned
                .request(Residency::DemandPaged, None)
                .validate(Capabilities::NONE),
            Err(RegionError::Unsupported(Capability::DemandPaging)),
        );
        assert_eq!(
            owned
                .request(Residency::Resident, Some(Permissions::READ_WRITE))
                .validate(Capabilities::NONE),
            Err(RegionError::Unsupported(Capability::MemoryProtection)),
        );
        assert_eq!(
            owned
                .request(Residency::Resident, Some(Permissions::READ))
                .validate(Capabilities::PAGE_TABLES),
            Err(RegionError::Unsupported(Capability::MemoryProtection)),
        );
        assert_eq!(
            owned
                .request(Residency::Unmapped, None)
                .validate(Capabilities::NONE),
            Err(RegionError::Unsupported(Capability::GuardPages)),
        );
    }

    #[test]
    fn supported_requests_are_checked_against_exact_guarantees() {
        let range = PhysicalRange::new(0x1000, 0x1000, 0x1000).unwrap();
        let supported = Capabilities::DEMAND_PAGING
            .union(Capabilities::GUARD_PAGES)
            .union(Capabilities::MEMORY_PROTECTION);
        let mut owned = unsafe { OwnedRange::new(range) };
        assert_eq!(
            owned
                .request(Residency::DemandPaged, None)
                .validate(supported),
            Ok(())
        );
        assert_eq!(
            owned
                .request(Residency::Unmapped, None)
                .validate(supported),
            Ok(())
        );
        assert_eq!(
            owned
                .request(
                    Residency::Resident,
                    Some(Permissions::READ.union(Permissions::WRITE))
                )
                .validate(supported),
            Ok(())
        );
    }
}
