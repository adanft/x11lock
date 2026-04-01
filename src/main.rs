mod auth;
mod locker;
mod monitor;
mod render;
mod signals;

use anyhow::{Context, Result};
use x11rb::connection::Connection;
use x11rb::rust_connection::RustConnection;

fn main() -> Result<()> {
    signals::block_sigusr1().context("Failed to block SIGUSR1")?;
    signals::register_signal_handlers().context("Failed to register signal handlers")?;

    let (conn, screen_num) =
        RustConnection::connect(None).context("Failed to connect to X server")?;

    let monitors =
        monitor::detect_monitors(&conn, screen_num).context("Failed to detect monitors")?;

    if monitors.is_empty() {
        anyhow::bail!("No active monitors found");
    }

    let mut locker = locker::Locker::new(&conn, screen_num);

    locker
        .create_windows(&monitors)
        .context("Failed to create locker windows")?;

    locker
        .load_background()
        .context("Failed to load wallpaper from ~/.config/x11lock/wallpaper.png")?;

    locker.map_windows().context("Failed to map windows")?;

    conn.flush().context("Initial flush failed")?;

    locker
        .grab_input()
        .context("CRITICAL: Failed to grab input. Locker is not secure.")?;

    let result = locker.run_loop();

    // Cleanup: always release input grab and destroy windows, even on error
    // Note: ignoring cleanup errors since the main operation already completed
    let _ = locker.ungrab_input();
    let _ = locker.destroy_windows();

    result.context("Event loop failed")?;

    Ok(())
}
