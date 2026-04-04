use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use nix::libc;
use std::os::fd::AsRawFd;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    ConfigureWindowAux, ConnectionExt as XprotoExt, CreateWindowAux, EventMask, GrabMode,
    GrabStatus, KeyPressEvent, Screen, StackMode, Window, WindowClass,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use zeroize::Zeroize;

use crate::auth;
use crate::render::{self, AuthFeedback, LockState, LockWindow, RenderContext};
use crate::signals;

/// X11 keysym constants for special keys
mod keysyms {
    /// X11 modifier state flags
    pub const MOD_SHIFT: u16 = 0x1;
    pub const MOD_CONTROL: u16 = 0x4;
    pub const MOD_ALT: u16 = 0x8; // Common X11 mapping: Alt on Mod1

    pub const KEY_ENTER: u32 = 0xff0d; // Return key
    pub const KEY_KP_ENTER: u32 = 0xff8d; // Numeric keypad Enter
    pub const KEY_BACKSPACE: u32 = 0xff08; // Backspace
    pub const KEY_DELETE: u32 = 0xffff; // Delete
    pub const KEY_ESCAPE: u32 = 0xff1b; // Escape
    pub const KEY_U: u32 = 0x75; // 'u' key (for Ctrl+Alt+U unlock)

    /// ASCII printable range (space to ~)
    pub const ASCII_PRINTABLE_START: u32 = 0x20;
    pub const ASCII_PRINTABLE_END: u32 = 0x7f;

    /// Check if Ctrl+Alt+<keysym> was pressed.
    pub fn is_ctrl_alt_key(state: u16, keysym: u32, expected_keysym: u32) -> bool {
        state & MOD_CONTROL != 0 && state & MOD_ALT != 0 && keysym == expected_keysym
    }

    /// Check if keysym is a printable ASCII character
    pub fn is_printable_ascii(keysym: u32) -> bool {
        (ASCII_PRINTABLE_START..ASCII_PRINTABLE_END).contains(&keysym)
    }
}

const GRAB_RETRY_COUNT: u32 = 10;
const GRAB_RETRY_DELAY_MS: u64 = 50;
const CLOCK_REDRAW_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LoopStatus {
    NoEvents,
    Unlocked,
}

pub(crate) struct Locker<'a> {
    conn: &'a RustConnection,
    screen_num: usize,
    windows: Vec<LockWindow>,
    password_buf: String,
    state: LockState,
    cursor: u32,
    render_ctx: Option<RenderContext>,
    last_render: Instant,
    auth_feedback: AuthFeedback,
    auth_message: Option<String>,
}

impl<'a> Locker<'a> {
    pub(crate) fn new(conn: &'a RustConnection, screen_num: usize) -> Self {
        Self {
            conn,
            screen_num,
            windows: Vec::new(),
            password_buf: String::new(),
            state: LockState::Idle,
            cursor: x11rb::NONE,
            render_ctx: None,
            last_render: Instant::now(),
            auth_feedback: AuthFeedback::None,
            auth_message: None,
        }
    }

    pub(crate) fn create_windows(&mut self, monitors: &[crate::monitor::Monitor]) -> Result<()> {
        let screen = &self.conn.setup().roots[self.screen_num];

        // Initialize cursor first (separate responsibility)
        self.init_cursor(screen)?;

        // Then create lock windows for each monitor
        for monitor in monitors {
            let window = self.create_lock_window(screen, monitor)?;
            self.windows.push(LockWindow {
                id: window,
                width: monitor.width,
                height: monitor.height,
            });
        }

        Ok(())
    }

    /// Initialize invisible cursor (single responsibility)
    fn init_cursor(&mut self, screen: &Screen) -> Result<()> {
        self.cursor = self.create_invisible_cursor(screen).unwrap_or(x11rb::NONE);
        Ok(())
    }

    pub(crate) fn load_background(&mut self) -> Result<()> {
        self.render_ctx = Some(RenderContext::load(&self.windows)?);
        Ok(())
    }

    pub(crate) fn map_windows(&self) -> Result<()> {
        for win in &self.windows {
            self.conn.map_window(win.id)?;
            self.conn.configure_window(
                win.id,
                &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
            )?;
        }
        self.conn.flush()?;
        Ok(())
    }

    pub(crate) fn grab_input(&self) -> Result<()> {
        self.grab_keyboard()?;
        self.grab_pointer()?;
        Ok(())
    }

    pub(crate) fn ungrab_input(&self) -> Result<()> {
        self.conn
            .ungrab_keyboard(x11rb::CURRENT_TIME)
            .context("UngrabKeyboard failed")?;
        self.conn
            .ungrab_pointer(x11rb::CURRENT_TIME)
            .context("UngrabPointer failed")?;
        self.conn.flush()?;
        Ok(())
    }

    pub(crate) fn destroy_windows(&self) -> Result<()> {
        for win in &self.windows {
            self.conn.destroy_window(win.id)?;
        }
        if self.cursor != x11rb::NONE {
            self.conn.free_cursor(self.cursor)?;
        }
        self.conn.flush()?;
        Ok(())
    }

    fn render(&mut self) -> Result<()> {
        if let Some(ref ctx) = self.render_ctx {
            render::render_frame(
                self.conn,
                &self.windows,
                ctx,
                self.state,
                self.password_buf.len(),
                self.auth_feedback,
                self.auth_message.as_deref(),
                self.screen_num,
            )?;
            self.last_render = Instant::now();
        }
        Ok(())
    }

    pub(crate) fn run_loop(&mut self) -> Result<()> {
        self.render()?;

        loop {
            if signals::termination_requested() {
                return Ok(());
            }

            // Check if we need to re-render (clock update)
            if self.last_render.elapsed() >= CLOCK_REDRAW_INTERVAL {
                self.render()?;
            }

            // Process all pending X11 events - early exit if unlocked
            if self.handle_pending_events()? == LoopStatus::Unlocked {
                return Ok(());
            }

            // If no events and not time to render yet, sleep until next render or X11 activity
            if self.last_render.elapsed() < CLOCK_REDRAW_INTERVAL {
                let timeout = self
                    .time_until_next_render()
                    .map(|duration| duration.as_millis().min(i32::MAX as u128) as i32)
                    .unwrap_or(-1); // -1 = infinite wait

                let fd = self.conn.stream().as_raw_fd();
                let mut poll_fd = libc::pollfd {
                    fd,
                    events: libc::POLLIN,
                    revents: 0,
                };

                let result = unsafe { libc::poll(&mut poll_fd, 1, timeout) };
                if result < 0 {
                    let err = std::io::Error::last_os_error();
                    if err.kind() != std::io::ErrorKind::Interrupted {
                        return Err(err).context("poll() failed while waiting for X11 events");
                    }
                }
            }
        }
    }

    fn handle_pending_events(&mut self) -> Result<LoopStatus> {
        while let Some(event) = self.conn.poll_for_event()? {
            match event {
                Event::KeyPress(e) => {
                    if self.handle_key_press(e)? {
                        return Ok(LoopStatus::Unlocked);
                    }
                }
                Event::ButtonPress(_) => {}
                Event::Expose(_) => {
                    self.render()?;
                }
                _ => {}
            }
        }

        Ok(LoopStatus::NoEvents)
    }

    fn time_until_next_render(&self) -> Option<Duration> {
        CLOCK_REDRAW_INTERVAL.checked_sub(self.last_render.elapsed())
    }

    fn create_invisible_cursor(&self, screen: &Screen) -> Option<u32> {
        let bitmap = self.conn.generate_id().ok()?;
        let mask = self.conn.generate_id().ok()?;
        self.conn.create_pixmap(1, bitmap, screen.root, 1, 1).ok()?;
        self.conn.create_pixmap(1, mask, screen.root, 1, 1).ok()?;

        let cursor = self.conn.generate_id().ok()?;
        self.conn
            .create_cursor(cursor, bitmap, mask, 0, 0, 0, 0, 0, 0, 0, 0)
            .ok()?;

        // Note: we intentionally ignore failures when freeing temp resources
        let _ = self.conn.free_pixmap(bitmap);
        let _ = self.conn.free_pixmap(mask);
        Some(cursor)
    }

    fn create_lock_window(
        &self,
        screen: &Screen,
        monitor: &crate::monitor::Monitor,
    ) -> Result<Window> {
        let window = self.conn.generate_id()?;

        self.conn.create_window(
            screen.root_depth,
            window,
            screen.root,
            monitor.x,
            monitor.y,
            monitor.width,
            monitor.height,
            0,
            WindowClass::INPUT_OUTPUT,
            screen.root_visual,
            &CreateWindowAux::new()
                .background_pixel(render::COLOR_BASE)
                .override_redirect(1)
                .event_mask(EventMask::KEY_PRESS | EventMask::BUTTON_PRESS | EventMask::EXPOSURE),
        )?;

        Ok(window)
    }

    fn grab_keyboard(&self) -> Result<()> {
        self.retry_grab_keyboard()
    }

    fn grab_pointer(&self) -> Result<()> {
        self.retry_grab_pointer()
    }

    /// Get the window to use for grabbing input (first lock window or screen root)
    fn grab_window(&self) -> Window {
        let screen = &self.conn.setup().roots[self.screen_num];
        self.windows.first().map(|w| w.id).unwrap_or(screen.root)
    }

    fn retry_grab_keyboard(&self) -> Result<()> {
        let grab_window = self.grab_window();

        for attempt in 0..GRAB_RETRY_COUNT {
            let reply = self
                .conn
                .grab_keyboard(
                    true,
                    grab_window,
                    x11rb::CURRENT_TIME,
                    GrabMode::ASYNC,
                    GrabMode::ASYNC,
                )
                .context("Failed to send GrabKeyboard")?
                .reply()
                .context("GrabKeyboard reply failed")?;

            if reply.status == GrabStatus::SUCCESS {
                return Ok(());
            }

            if attempt == GRAB_RETRY_COUNT - 1 {
                bail!(
                    "Failed to grab keyboard after {} attempts. Status: {:?}",
                    GRAB_RETRY_COUNT,
                    reply.status
                );
            }

            thread::sleep(Duration::from_millis(GRAB_RETRY_DELAY_MS));
        }
        Ok(())
    }

    fn retry_grab_pointer(&self) -> Result<()> {
        let grab_window = self.grab_window();

        for attempt in 0..GRAB_RETRY_COUNT {
            let reply = self
                .conn
                .grab_pointer(
                    true,
                    grab_window,
                    EventMask::BUTTON_PRESS | EventMask::BUTTON_RELEASE | EventMask::POINTER_MOTION,
                    GrabMode::ASYNC,
                    GrabMode::ASYNC,
                    x11rb::NONE,
                    self.cursor,
                    x11rb::CURRENT_TIME,
                )
                .context("Failed to send GrabPointer")?
                .reply()
                .context("GrabPointer reply failed")?;

            if reply.status == GrabStatus::SUCCESS {
                return Ok(());
            }

            if attempt == GRAB_RETRY_COUNT - 1 {
                bail!(
                    "Failed to grab pointer after {} attempts. Status: {:?}",
                    GRAB_RETRY_COUNT,
                    reply.status
                );
            }

            thread::sleep(Duration::from_millis(GRAB_RETRY_DELAY_MS));
        }
        Ok(())
    }

    fn handle_key_press(&mut self, event: KeyPressEvent) -> Result<bool> {
        let keysym = self.keycode_to_keysym(event.detail, u16::from(event.state))?;

        match keysym {
            keysyms::KEY_ENTER | keysyms::KEY_KP_ENTER => return self.try_authenticate(),
            keysyms::KEY_BACKSPACE | keysyms::KEY_DELETE => {
                self.password_buf.pop();
                self.clear_feedback();
                let new_state = if self.password_buf.is_empty() {
                    LockState::Idle
                } else {
                    LockState::Typing
                };
                self.set_state(new_state)?;
            }
            keysyms::KEY_ESCAPE => {
                self.password_buf.zeroize();
                self.clear_feedback();
                self.set_state(LockState::Idle)?;
            }
            // Ctrl+Alt+U clears the current input and resets the visual state.
            keysym if keysyms::is_ctrl_alt_key(u16::from(event.state), keysym, keysyms::KEY_U) => {
                self.password_buf.clear();
                self.clear_feedback();
                self.set_state(LockState::Idle)?;
            }
            keysym if keysyms::is_printable_ascii(keysym) => {
                if let Some(ch) = char::from_u32(keysym) {
                    self.password_buf.push(ch);
                    self.clear_feedback();
                    self.set_state(LockState::Typing)?;
                }
            }
            _ => {}
        }

        Ok(false)
    }

    fn try_authenticate(&mut self) -> Result<bool> {
        let mut password = self.password_buf.clone();
        self.password_buf.zeroize();

        let result = auth::authenticate(&password);
        password.zeroize();
        let result = result?;

        match result {
            auth::AuthResult::Success => Ok(true),
            auth::AuthResult::Failure(message) => {
                self.auth_feedback = AuthFeedback::Message;
                self.auth_message = Some(message);
                self.set_state(LockState::Error)?;
                Ok(false)
            }
        }
    }

    fn set_state(&mut self, state: LockState) -> Result<()> {
        self.state = state;
        self.render()
    }

    fn clear_feedback(&mut self) {
        self.auth_feedback = AuthFeedback::None;
        self.auth_message = None;
    }

    fn keycode_to_keysym(&self, keycode: u8, state: u16) -> Result<u32> {
        let mapping = self
            .conn
            .get_keyboard_mapping(keycode, 1)
            .context("Failed to get keyboard mapping")?
            .reply()
            .context("Keyboard mapping reply failed")?;

        if mapping.keysyms.is_empty() {
            return Ok(0);
        }

        // Intentionally keep keyboard resolution minimal for now: ASCII printable input
        // with Shift support only. Full XKB/group/modifier handling is out of scope until
        // keyboard/layout support is explicitly expanded and documented.
        let shift_pressed = state & keysyms::MOD_SHIFT != 0;
        let idx = if shift_pressed && mapping.keysyms.len() > 1 {
            1
        } else {
            0
        };

        Ok(mapping.keysyms[idx])
    }
}
