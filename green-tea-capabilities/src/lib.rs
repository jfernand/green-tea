#![no_std]

#[cfg(all(feature = "green", feature = "black"))]
compile_error!("Green and Black are mutually exclusive base profiles");

/// A capability that a platform backend and runtime provide together.
///
/// These describe implemented guarantees, not CPU features or authorization to use a
/// device. In particular, page tables alone do not provide protected address spaces.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Capability {
    PageTables,
    MemoryProtection,
    GuardPages,
    DemandPaging,
    ProtectedAddressSpaces,
    DmaIsolation,
    RealtimeExecution,
}

/// A compact set of guarantees implemented by the selected board/runtime combination.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Capabilities(u8);

impl Capabilities {
    pub const NONE: Self = Self(0);
    pub const PAGE_TABLES: Self = Self(1 << 0);
    pub const MEMORY_PROTECTION: Self = Self(1 << 1);
    pub const GUARD_PAGES: Self = Self(1 << 2);
    pub const DEMAND_PAGING: Self = Self(1 << 3);
    pub const PROTECTED_ADDRESS_SPACES: Self = Self(1 << 4);
    pub const DMA_ISOLATION: Self = Self(1 << 5);
    pub const REALTIME_EXECUTION: Self = Self(1 << 6);

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

    const fn bit(capability: Capability) -> u8 {
        match capability {
            Capability::PageTables => Self::PAGE_TABLES.0,
            Capability::MemoryProtection => Self::MEMORY_PROTECTION.0,
            Capability::GuardPages => Self::GUARD_PAGES.0,
            Capability::DemandPaging => Self::DEMAND_PAGING.0,
            Capability::ProtectedAddressSpaces => Self::PROTECTED_ADDRESS_SPACES.0,
            Capability::DmaIsolation => Self::DMA_ISOLATION.0,
            Capability::RealtimeExecution => Self::REALTIME_EXECUTION.0,
        }
    }
}

/// A requested guarantee is absent from the selected board/runtime combination.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UnsupportedCapability {
    pub capability: Capability,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BaseProfile {
    Green,
    Black,
}

/// A statically selected base profile with optional realtime composition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Profile {
    pub base: BaseProfile,
    pub realtime: bool,
}

impl Profile {
    pub const GREEN: Self = Self {
        base: BaseProfile::Green,
        realtime: false,
    };
    pub const BLACK: Self = Self {
        base: BaseProfile::Black,
        realtime: false,
    };
    pub const GREEN_WHITE: Self = Self {
        base: BaseProfile::Green,
        realtime: true,
    };
    pub const BLACK_WHITE: Self = Self {
        base: BaseProfile::Black,
        realtime: true,
    };

    pub const fn name(self) -> &'static str {
        match (self.base, self.realtime) {
            (BaseProfile::Green, false) => "Green",
            (BaseProfile::Black, false) => "Black",
            (BaseProfile::Green, true) => "Green+White",
            (BaseProfile::Black, true) => "Black+White",
        }
    }

    /// Check that all guarantees promised by this profile are actually implemented.
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
}

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

#[cfg(test)]
mod tests {
    use super::{BUILD_PROFILE, BaseProfile, Capabilities, Capability, Profile};

    const IDENTITY_PAGING: Capabilities = Capabilities::PAGE_TABLES
        .union(Capabilities::GUARD_PAGES)
        .union(Capabilities::DEMAND_PAGING);

    #[test]
    fn green_profile_needs_no_protection_backend() {
        assert_eq!(Profile::GREEN.validate(Capabilities::NONE), Ok(()));
        assert_eq!(Profile::GREEN.name(), "Green");
    }

    #[test]
    fn page_tables_do_not_imply_protected_address_spaces() {
        assert!(IDENTITY_PAGING.supports(Capability::PageTables));
        assert_eq!(
            Profile::BLACK.validate(IDENTITY_PAGING),
            Err(super::UnsupportedCapability {
                capability: Capability::ProtectedAddressSpaces,
            })
        );
        let protected = IDENTITY_PAGING.union(Capabilities::PROTECTED_ADDRESS_SPACES);
        assert_eq!(Profile::BLACK.validate(protected), Ok(()));
    }

    #[test]
    fn white_requires_the_realtime_runtime() {
        assert_eq!(
            Profile::GREEN_WHITE.validate(IDENTITY_PAGING),
            Err(super::UnsupportedCapability {
                capability: Capability::RealtimeExecution,
            })
        );
        assert_eq!(Profile::GREEN_WHITE.name(), "Green+White");
    }

    #[test]
    fn black_white_checks_process_isolation_before_realtime() {
        assert_eq!(
            Profile::BLACK_WHITE.validate(Capabilities::REALTIME_EXECUTION),
            Err(super::UnsupportedCapability {
                capability: Capability::ProtectedAddressSpaces,
            })
        );
        let protected = Capabilities::PROTECTED_ADDRESS_SPACES;
        assert_eq!(
            Profile::BLACK_WHITE.validate(protected),
            Err(super::UnsupportedCapability {
                capability: Capability::RealtimeExecution,
            })
        );
        assert_eq!(Profile::BLACK_WHITE.name(), "Black+White");
    }

    #[test]
    fn dma_isolation_is_requested_separately() {
        assert_eq!(
            IDENTITY_PAGING.require(Capability::DmaIsolation),
            Err(super::UnsupportedCapability {
                capability: Capability::DmaIsolation,
            })
        );
        assert_eq!(Profile::BLACK.base, BaseProfile::Black);
    }

    #[cfg(any(
        all(feature = "green", not(feature = "black")),
        all(feature = "black", not(feature = "green"))
    ))]
    #[test]
    fn cargo_selects_a_static_profile() {
        #[cfg(feature = "green")]
        let expected_base = BaseProfile::Green;
        #[cfg(feature = "black")]
        let expected_base = BaseProfile::Black;
        assert_eq!(BUILD_PROFILE.base, expected_base);
        assert_eq!(BUILD_PROFILE.realtime, cfg!(feature = "white"));
    }
}
