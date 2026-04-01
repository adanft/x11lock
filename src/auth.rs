use anyhow::{Context, Result};
use pam_client2::{Context as PamContext, ErrorCode, Flag};
use std::cell::RefCell;
use std::ffi::{CStr, CString};
use std::rc::Rc;

pub(crate) enum AuthResult {
    Success,
    Failure(String),
}

pub(crate) fn authenticate(password: &str) -> Result<AuthResult> {
    let username = get_current_username().context("Failed to get current user")?;
    let messages = Rc::new(RefCell::new(Vec::new()));

    let conv = PasswordConv {
        username: username.clone(),
        password: password.to_owned(),
        messages: Rc::clone(&messages),
    };

    let mut ctx = PamContext::new("system-auth", Some(&username), conv)
        .context("Failed to init PAM context")?;

    match ctx.authenticate(Flag::NONE) {
        Ok(()) => Ok(AuthResult::Success),
        Err(e) => {
            let pam_message = e.message().unwrap_or_default().to_string();

            // Prefer conv message, then PAM message, then fallback to error string
            let conv_msg = messages.borrow().join(" ");
            let message = if !conv_msg.trim().is_empty() {
                conv_msg
            } else if !pam_message.trim().is_empty() {
                pam_message
            } else {
                e.to_string()
            };

            Ok(AuthResult::Failure(message))
        }
    }
}

fn get_current_username() -> Result<String> {
    // Try environment variables first, then fallback to /etc/passwd
    let user = std::env::var("USER").ok().filter(|u| !u.is_empty());
    let logname = std::env::var("LOGNAME").ok().filter(|l| !l.is_empty());

    if let Some(username) = user.or(logname) {
        return Ok(username);
    }

    let uid = nix::unistd::getuid();
    let passwd = nix::unistd::User::from_uid(uid)
        .context("Failed to read /etc/passwd")?
        .context("User not found in /etc/passwd")?;

    Ok(passwd.name)
}

struct PasswordConv {
    username: String,
    password: String,
    messages: Rc<RefCell<Vec<String>>>,
}

impl pam_client2::ConversationHandler for PasswordConv {
    fn prompt_echo_on(&mut self, _prompt: &CStr) -> std::result::Result<CString, ErrorCode> {
        CString::new(self.username.as_str()).map_err(|_| ErrorCode::CONV_ERR)
    }

    fn prompt_echo_off(&mut self, _prompt: &CStr) -> std::result::Result<CString, ErrorCode> {
        CString::new(self.password.as_str()).map_err(|_| ErrorCode::CONV_ERR)
    }

    fn text_info(&mut self, msg: &CStr) {
        self.push_message(msg);
    }

    fn error_msg(&mut self, msg: &CStr) {
        self.push_message(msg);
    }
}

impl PasswordConv {
    fn push_message(&mut self, msg: &CStr) {
        if let Ok(text) = msg.to_str() {
            self.messages.borrow_mut().push(text.to_owned());
        }
    }
}
