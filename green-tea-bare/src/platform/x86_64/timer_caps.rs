#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum X86TimerMode {
    ApicCountdown,
    TscDeadline,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct X86TimerCapabilities {
    pub local_apic: bool,
    pub tsc_deadline: bool,
    pub invariant_tsc: bool,
    pub tsc_clock: bool,
    pub usable_tsc_deadline: bool,
}

impl X86TimerCapabilities {
    pub const fn from_cpuid(
        max_basic: u32,
        leaf1_ecx: u32,
        leaf1_edx: u32,
        max_extended: u32,
        extended_leaf7_edx: u32,
        tsc_clock: bool,
    ) -> Self {
        let local_apic = max_basic >= 1 && leaf1_edx & (1 << 9) != 0;
        let tsc_deadline = max_basic >= 1 && leaf1_ecx & (1 << 24) != 0;
        let invariant_tsc = max_extended >= 0x8000_0007 && extended_leaf7_edx & (1 << 8) != 0;
        Self {
            local_apic,
            tsc_deadline,
            invariant_tsc,
            tsc_clock,
            usable_tsc_deadline: local_apic && tsc_deadline && invariant_tsc && tsc_clock,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::X86TimerCapabilities;

    const APIC: u32 = 1 << 9;
    const DEADLINE: u32 = 1 << 24;
    const INVARIANT: u32 = 1 << 8;

    #[test]
    fn deadline_mode_requires_the_cpu_features_and_a_tsc_board_clock() {
        let caps =
            X86TimerCapabilities::from_cpuid(1, DEADLINE, APIC, 0x8000_0007, INVARIANT, true);
        assert!(caps.usable_tsc_deadline);

        let hpet =
            X86TimerCapabilities::from_cpuid(1, DEADLINE, APIC, 0x8000_0007, INVARIANT, false);
        assert!(!hpet.usable_tsc_deadline);
    }

    #[test]
    fn missing_cpuid_leaves_and_features_are_reported() {
        let caps =
            X86TimerCapabilities::from_cpuid(0, DEADLINE, APIC, 0x8000_0006, INVARIANT, true);
        assert!(!caps.local_apic);
        assert!(!caps.tsc_deadline);
        assert!(!caps.invariant_tsc);
        assert!(!caps.usable_tsc_deadline);

        let no_invariant =
            X86TimerCapabilities::from_cpuid(1, DEADLINE, APIC, 0x8000_0007, 0, true);
        assert!(!no_invariant.usable_tsc_deadline);
    }
}
