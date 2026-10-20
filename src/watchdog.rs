use crate::control::Camera;
use anyhow::Result;

#[derive(Default)]
pub struct Watchdog {
    level: bool,
    claimed: bool,
    pub feeds: u64,
}

impl Watchdog {
    fn property(camera: &mut Camera, property: u8, value: u8) -> Result<()> {
        camera.command(0x8400, &[0, 0, 0, 0x10, property, value], 3)?;
        Ok(())
    }
    pub fn start(&mut self, camera: &mut Camera) -> Result<()> {
        Self::property(camera, 0, 1)?;
        self.claimed = true;
        Self::property(camera, 1, 0)?;
        Self::property(camera, 2, 1)?;
        self.feed(camera)
    }
    pub fn feed(&mut self, camera: &mut Camera) -> Result<()> {
        self.level = !self.level;
        camera.command(
            0x8402,
            &[0, 0, 0, 0x10, 0, 0, 0, if self.level { 0x10 } else { 0 }],
            3,
        )?;
        self.feeds += 1;
        Ok(())
    }
    pub fn release(&mut self, camera: &mut Camera) -> Result<()> {
        if self.claimed {
            Self::property(camera, 2, 0)?;
            Self::property(camera, 1, 1)?;
            Self::property(camera, 0, 0)?;
            self.claimed = false;
        }
        Ok(())
    }
}
