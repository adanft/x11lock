use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use nix::sys::signal::{sigaction, SaFlags, SigAction, SigHandler, SigSet, Signal};

static TERMINATION_REQUESTED: AtomicBool = AtomicBool::new(false);

extern "C" fn handle_signal(_: std::ffi::c_int) {
    TERMINATION_REQUESTED.store(true, Ordering::Relaxed);
}

pub(crate) fn register_signal_handlers() -> Result<()> {
    let action = SigAction::new(
        SigHandler::Handler(handle_signal),
        SaFlags::SA_RESTART,
        SigSet::empty(),
    );

    unsafe {
        sigaction(Signal::SIGTERM, &action)?;
        sigaction(Signal::SIGINT, &action)?;
    }

    Ok(())
}

pub(crate) fn termination_requested() -> bool {
    TERMINATION_REQUESTED.load(Ordering::Relaxed)
}

pub(crate) fn block_sigusr1() -> Result<()> {
    let mut mask = SigSet::empty();
    mask.add(Signal::SIGUSR1);
    nix::sys::signal::pthread_sigmask(nix::sys::signal::SigmaskHow::SIG_BLOCK, Some(&mask), None)?;
    Ok(())
}
