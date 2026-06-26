//! Hardware side: open the Beacn Mix through `beacn-lib` and surface its
//! input events. All of the raw USB work (claiming the vendor interface,
//! reading the 64-byte interrupt reports, parsing dial deltas / button bits)
//! already lives in beacn-lib's control-device handler — we just subscribe.

use anyhow::{anyhow, Result};
use beacn_lib::controller::{
    open_control_device, BeacnControlDevice, Buttons, Dials, Interactions,
};
use beacn_lib::crossbeam::channel::{bounded, unbounded, Receiver};
use beacn_lib::manager::get_beacn_mix_device;
use std::time::Duration;

/// A connected Beacn Mix. Hold onto this for the lifetime of the program:
/// dropping `_device` tears down the control thread (and the screen goes idle).
pub struct Mix {
    device: Box<dyn BeacnControlDevice>,
    /// Kept alive so the library's health channel sender doesn't disconnect.
    _health_rx: Receiver<()>,
    pub events: Receiver<Interactions>,
}

impl Mix {
    /// Find the first attached Beacn Mix and open it for interaction.
    pub fn open() -> Result<Self> {
        let location = get_beacn_mix_device()
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("No Beacn Mix found (expected USB 33ae:0004)"))?;

        let (event_tx, events) = unbounded::<Interactions>();
        let (health_tx, health_rx) = bounded::<()>(1);

        let device = open_control_device(location, Some(event_tx), health_tx)
            .map_err(|e| anyhow!("Failed to open Beacn Mix: {e}"))?;

        log::info!(
            "Opened Beacn Mix: serial={}, version={}",
            device.get_serial(),
            device.get_version()
        );

        Ok(Self {
            device,
            _health_rx: health_rx,
            events,
        })
    }

    /// Set the panel brightness (1..=100) and push the dim timer to its max, so
    /// the screen stays readable while the mixer runs.
    pub fn init_display(&self) {
        if let Err(e) = self.device.set_display_brightness(80) {
            log::warn!("set brightness failed: {e}");
        }
        self.keep_awake();
    }

    /// Re-arm the dim timer (call periodically). beacn-lib caps this at 5 min.
    pub fn keep_awake(&self) {
        if let Err(e) = self.device.set_dim_timeout(Duration::from_secs(300)) {
            log::debug!("set dim timeout failed: {e}");
        }
    }

    /// Force the panel back on after the device's firmware screen-off. beacn-lib
    /// doesn't track that deep sleep, so we re-send the enable/brightness/wake
    /// sequence; the caller should then redraw.
    pub fn wake(&self) {
        let _ = self.device.set_enabled(true);
        let _ = self.device.set_display_brightness(80);
        let _ = self.device.send_keepalive();
    }

    /// Draw a full-screen JPEG to the panel.
    pub fn set_screen(&self, jpeg: &[u8]) -> Result<()> {
        self.device
            .set_image(0, 0, jpeg)
            .map_err(|e| anyhow!("set_image failed: {e}"))
    }
}

/// Logical channel 0..=3, matching the four faders/encoders left-to-right.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct Channel(pub usize);

impl Channel {
    pub const ALL: [Channel; 4] = [Channel(0), Channel(1), Channel(2), Channel(3)];

    /// 1-based label as the user sees it on the device / CLI.
    pub fn human(self) -> usize {
        self.0 + 1
    }
}

/// Which channel does a turned dial belong to?
pub fn channel_for_dial(dial: Dials) -> Channel {
    match dial {
        Dials::Dial1 => Channel(0),
        Dials::Dial2 => Channel(1),
        Dials::Dial3 => Channel(2),
        Dials::Dial4 => Channel(3),
    }
}

/// Which channel does a pressed encoder belong to? `None` for non-encoder
/// buttons (page/audience), which we ignore for now.
pub fn channel_for_button(button: Buttons) -> Option<Channel> {
    match button {
        Buttons::Dial1 => Some(Channel(0)),
        Buttons::Dial2 => Some(Channel(1)),
        Buttons::Dial3 => Some(Channel(2)),
        Buttons::Dial4 => Some(Channel(3)),
        _ => None,
    }
}
