//! [fork] Backend side of the fork.
//!
//! Hooked into upstream from `src/background/main.rs`: declared there as a module and `init` is called at
//! the end of the app setup (see FORK.md).

pub mod release;
mod uac_prompt;
mod wheel_bridge;

pub fn init() {
    wheel_bridge::start();
    uac_prompt::start();
}
