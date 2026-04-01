use anyhow::{Context, Result};
use x11rb::connection::Connection;
use x11rb::protocol::randr::ConnectionExt as RandrExt;
use x11rb::rust_connection::RustConnection;

#[derive(Debug, Clone)]
pub(crate) struct Monitor {
    pub(crate) x: i16,
    pub(crate) y: i16,
    pub(crate) width: u16,
    pub(crate) height: u16,
}

pub(crate) fn detect_monitors(conn: &RustConnection, screen_num: usize) -> Result<Vec<Monitor>> {
    let screen = &conn.setup().roots[screen_num];
    let root = screen.root;

    let randr_version = conn
        .randr_query_version(1, 5)
        .context("RandR not available")?
        .reply()
        .context("Failed to get RandR version")?;

    if randr_version.major_version < 1
        || (randr_version.major_version == 1 && randr_version.minor_version < 2)
    {
        return Ok(fallback_monitor(screen));
    }

    let resources = conn
        .randr_get_screen_resources_current(root)
        .context("Failed to get screen resources")?
        .reply()
        .context("Screen resources reply failed")?;

    let monitors: Vec<Monitor> = resources
        .crtcs
        .iter()
        .filter_map(|&crtc| {
            let crtc_info = conn
                .randr_get_crtc_info(crtc, resources.config_timestamp)
                .ok()?
                .reply()
                .ok()?;

            // Skip disabled monitors
            if crtc_info.width == 0 || crtc_info.height == 0 || crtc_info.outputs.is_empty() {
                return None;
            }

            Some(Monitor {
                x: crtc_info.x,
                y: crtc_info.y,
                width: crtc_info.width,
                height: crtc_info.height,
            })
        })
        .collect();

    if monitors.is_empty() {
        Ok(fallback_monitor(screen))
    } else {
        Ok(monitors)
    }
}

/// Fallback: return full screen as single monitor
fn fallback_monitor(screen: &x11rb::protocol::xproto::Screen) -> Vec<Monitor> {
    vec![Monitor {
        x: 0,
        y: 0,
        width: screen.width_in_pixels,
        height: screen.height_in_pixels,
    }]
}
