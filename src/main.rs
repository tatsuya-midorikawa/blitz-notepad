use std::env;
use std::path::PathBuf;
use std::process::ExitCode;

use blitz_notepad::gui::{run_window, save_startup_screenshot, GuiOptions};
use blitz_notepad::settings::EditorSettings;
use blitz_notepad::{BlitzApp, Result};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("blitzpad: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    let mut args = env::args_os().skip(1).collect::<Vec<_>>();
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        print_help();
        return Ok(());
    }
    if args.iter().any(|arg| arg == "--version" || arg == "-V") {
        println!("blitzpad {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    let dump_ui = take_flag(&mut args, "--dump-ui");
    let smoke_window = take_flag(&mut args, "--smoke-window");
    let screenshot_path = take_option(&mut args, "--screenshot");
    let settings = EditorSettings::load().unwrap_or_default();
    let app = if let Some(path) = args.first() {
        BlitzApp::open(PathBuf::from(path), settings)?
    } else {
        BlitzApp::new(settings)
    };

    let state = app.ui_state()?;
    if dump_ui {
        for line in state.shell_snapshot() {
            println!("{line}");
        }
        return Ok(());
    }

    if let Some(path) = screenshot_path {
        save_startup_screenshot(&app, PathBuf::from(path))?;
        return Ok(());
    }

    let options = if smoke_window {
        GuiOptions::smoke_test(2)
    } else {
        GuiOptions::interactive()
    };
    run_window(app, options)
}

fn take_flag(args: &mut Vec<std::ffi::OsString>, flag: &str) -> bool {
    let Some(index) = args.iter().position(|arg| arg == flag) else {
        return false;
    };
    args.remove(index);
    true
}

fn take_option(args: &mut Vec<std::ffi::OsString>, flag: &str) -> Option<std::ffi::OsString> {
    let index = args.iter().position(|arg| arg == flag)?;
    args.remove(index);
    if index < args.len() {
        Some(args.remove(index))
    } else {
        None
    }
}

fn print_help() {
    println!("blitzpad - Notepad-compatible text editor core");
    println!();
    println!("Usage: blitzpad [OPTIONS] [FILE]");
    println!();
    println!("Options:");
    println!("  --dump-ui     Print the current Notepad UI model and exit");
    println!("  --smoke-window Render a window briefly and exit");
    println!("  --screenshot PATH Render startup UI to a PNG file and exit");
    println!("  -V, --version Print version information");
    println!("  -h, --help    Print this help");
}
