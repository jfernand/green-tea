//! What QEMU's machines have and a physical one doesn't: fw_cfg, the device through which
//! QEMU passes files to the guest, and ramfb, the display set up through it.

use super::Platform;
use crate::{Framebuffer, println, ramfb};
use green_tea_fw_cfg::{FwCfg, FwCfgPort};

/// A board that runs only in QEMU.
pub trait Qemu: Platform {
    /// How this board reaches fw_cfg.
    type FwCfg: FwCfgPort;
    const FW_CFG: Self::FwCfg;

    /// The fw_cfg device, ready to use.
    fn fw_cfg() -> FwCfg<Self::FwCfg> {
        FwCfg(Self::FW_CFG)
    }

    /// `Platform::display` on such a board: find fw_cfg, and turn on ramfb through it.
    fn ramfb_display() -> Result<Framebuffer, &'static str> {
        let fw_cfg = Self::fw_cfg();
        fw_cfg.probe()?;
        println!("fw_cfg files:");
        fw_cfg.for_each_file(|f| println!("  {:#06x} {:6} {}", f.select, f.size, f.name()));
        let fb = ramfb::init(&fw_cfg)?;
        println!("ramfb: {}x{} XRGB8888", fb.width(), fb.height());
        Ok(fb)
    }

    /// `Platform::test_name` on such a board: the fw_cfg file xtask passes with `-fw_cfg`.
    fn fw_cfg_test_name(name: &mut [u8]) -> usize {
        Self::fw_cfg().read_file("opt/green-tea/test", name).unwrap_or(0)
    }
}
