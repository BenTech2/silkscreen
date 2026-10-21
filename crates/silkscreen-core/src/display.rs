//! A running display: the network service plus its LAN advertisement.

use std::sync::Arc;

use anyhow::Result;
use silkscreen_proto::DisplayCaps;
use tracing::info;

use crate::ctx::Ctx;
use crate::discovery;
use crate::service::{self, FrameSink, ServiceHandle};

/// Dropping it takes the display off the network (ending any session).
pub struct DisplayRole {
    pub service: ServiceHandle,
    _advertiser: discovery::Advertiser,
}

impl DisplayRole {
    /// Listens on `port` (video on `port + 1`) and advertises `caps`.
    /// `fixed_code` replaces the random pairing code (tests).
    pub fn start(
        port: u16,
        fixed_code: Option<String>,
        ctx: Arc<Ctx>,
        caps: DisplayCaps,
        sink: Arc<dyn FrameSink>,
    ) -> Result<Self> {
        let service = service::spawn(
            service::ServiceOptions { port, fixed_code },
            caps.clone(),
            ctx.clone(),
            sink,
        )?;
        let advertiser = discovery::advertise(
            &caps.name,
            port,
            &ctx.identity.public,
            &discovery::Summary {
                width: caps.width,
                height: caps.height,
                refresh_mhz: caps.refresh_mhz,
                touch: caps.touch,
            },
        )?;
        info!(
            "\"{}\" ({}) is available as a {}x{} @ {:.2} Hz display{}",
            caps.name,
            ctx.identity.fingerprint(),
            caps.width,
            caps.height,
            caps.refresh_mhz as f32 / 1000.0,
            if caps.touch {
                format!(" with {}-point touch", caps.max_touch_points)
            } else {
                String::new()
            }
        );
        Ok(DisplayRole {
            service,
            _advertiser: advertiser,
        })
    }
}
