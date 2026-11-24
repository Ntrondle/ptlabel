#![cfg_attr(all(target_os = "windows", not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod cli;
mod data;
mod history;
mod icons;
mod job;
mod label;
mod menu;
mod printer;
mod proto;
mod spell;
mod transport;
mod update;

#[cfg(target_os = "windows")]
fn attach_parent_console() {
    // SAFETY: attaches to the parent terminal when launched from cmd/PowerShell; harmlessly fails
    // when there is no console or handles are redirected.
    unsafe {
        windows_sys::Win32::System::Console::AttachConsole(
            windows_sys::Win32::System::Console::ATTACH_PARENT_PROCESS,
        );
    }
}

fn main() -> anyhow::Result<()> {
    update::cleanup_old();
    let args: Vec<String> = std::env::args().skip(1).filter(|a| !a.starts_with("-psn")).collect();
    if args.is_empty() {
        return app::run();
    }
    #[cfg(target_os = "windows")]
    attach_parent_console();
    cli::run(args)
}
