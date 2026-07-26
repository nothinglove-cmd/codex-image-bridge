#![cfg_attr(windows, windows_subsystem = "windows")]

use std::{ffi::OsString, path::PathBuf};

use anyhow::Result;
use clap::{error::ErrorKind, Parser, Subcommand};

use codex_image_fix::{
    diagnostics, guardian, gui, image,
    install::{self, InstallationHealth},
    proxy, session,
};

const EXIT_SUCCESS: i32 = 0;
const EXIT_RUNTIME_ERROR: i32 = 1;
const EXIT_NOT_INSTALLED: i32 = 10;
const EXIT_INSTALLATION_BROKEN: i32 = 11;
const EXIT_CONFIG_CONFLICT: i32 = 20;
const EXIT_NETWORK_FAILURE: i32 = 30;

#[derive(Parser, Debug)]
#[command(name = "codex-image-fix", version)]
#[command(about = "Display Codex image generation results in the Desktop chat")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    Diagnose {
        #[arg(long)]
        session: PathBuf,
    },
    Restore {
        #[arg(long)]
        session: PathBuf,
        #[arg(long)]
        output_dir: Option<PathBuf>,
    },
    Install {
        #[arg(long)]
        real_cli: Option<PathBuf>,
        #[arg(long)]
        silent: bool,
    },
    Repair {
        #[arg(long)]
        silent: bool,
    },
    Uninstall {
        #[arg(long)]
        silent: bool,
    },
    Status {
        #[arg(long)]
        json: bool,
    },
    SupportBundle {
        #[arg(long)]
        output: Option<PathBuf>,
    },
    VerifyChat {
        #[arg(long)]
        thread: String,
    },
    #[command(hide = true)]
    Guardian,
}

fn main() {
    match run() {
        Ok(exit_code) if exit_code == EXIT_SUCCESS => {}
        Ok(exit_code) => std::process::exit(exit_code),
        Err(error) => {
            write_stderr(&format!("codex-image-fix: {error:#}\n"));
            std::process::exit(exit_code_for_error(&error));
        }
    }
}

fn run() -> Result<i32> {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    if args.is_empty() {
        gui::run()?;
        return Ok(EXIT_SUCCESS);
    }
    if !is_utility_command(&args) {
        proxy::run(&args)?;
        return Ok(EXIT_SUCCESS);
    }
    if args.first().and_then(|argument| argument.to_str()) == Some("guardian") {
        guardian::run()?;
        return Ok(EXIT_SUCCESS);
    }
    attach_parent_console();

    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => {
            let help = matches!(
                error.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            );
            if help {
                write_stdout(&error.to_string());
                return Ok(EXIT_SUCCESS);
            }
            write_stderr(&error.to_string());
            return Ok(2);
        }
    };
    match cli.command {
        Command::Diagnose { session: path } => {
            let snapshot = session::read_session(&path)?;
            write_stdout(&format!(
                "thread: {}\nimages: {}\n",
                snapshot.thread_id.as_deref().unwrap_or("unknown"),
                snapshot.images.len()
            ));
            for item in snapshot.images {
                write_stdout(&format!(
                    "turn={} image={} status={} encoded_bytes={}\n",
                    item.turn_id.as_deref().unwrap_or("unknown"),
                    item.id,
                    item.status,
                    item.result.as_ref().map_or(0, String::len)
                ));
            }
        }
        Command::Restore {
            session: path,
            output_dir,
        } => {
            let snapshot = session::read_session(&path)?;
            let thread_id = snapshot.thread_id.as_deref().unwrap_or("unknown-thread");
            let output_root = output_dir.unwrap_or_else(image::default_output_dir);
            for item in snapshot.ready_images() {
                let saved = image::decode_and_save(&output_root, thread_id, item)?;
                write_stdout(&format!("{}\n", saved.path.display()));
            }
        }
        Command::Install { real_cli, silent } => {
            let _ = silent;
            install::install(real_cli.as_deref())?;
        }
        Command::Repair { silent } => {
            let _ = silent;
            install::repair()?;
        }
        Command::Uninstall { silent } => {
            let _ = silent;
            let _ = install::uninstall()?;
        }
        Command::Status { json } => {
            let report = install::status_report()?;
            if json {
                write_stdout(&format!("{}\n", serde_json::to_string(&report)?));
            } else {
                write_stdout(&install::format_status_report(&report));
            }
            return Ok(match report.health {
                InstallationHealth::Healthy => EXIT_SUCCESS,
                InstallationHealth::NotInstalled => EXIT_NOT_INSTALLED,
                InstallationHealth::Broken => EXIT_INSTALLATION_BROKEN,
            });
        }
        Command::SupportBundle { output } => {
            let path = diagnostics::create_support_bundle(output.as_deref())?;
            diagnostics::require_bundle_is_redacted(&path)?;
            write_stdout(&format!("{}\n", path.display()));
        }
        Command::VerifyChat { thread } => install::verify_chat(&thread)?,
        Command::Guardian => guardian::run()?,
    }
    Ok(EXIT_SUCCESS)
}

fn is_utility_command(args: &[OsString]) -> bool {
    let Some(first) = args.first().and_then(|value| value.to_str()) else {
        return false;
    };
    matches!(
        first,
        "diagnose"
            | "restore"
            | "install"
            | "repair"
            | "uninstall"
            | "status"
            | "support-bundle"
            | "verify-chat"
            | "guardian"
    ) || (matches!(first, "-h" | "--help" | "-V" | "--version") && args.len() == 1)
}

fn exit_code_for_error(error: &anyhow::Error) -> i32 {
    let message = format!("{error:#}").to_ascii_lowercase();
    if message.contains("not installed") {
        EXIT_NOT_INSTALLED
    } else if message.contains("changed after")
        || message.contains("reload before")
        || message.contains("refusing to overwrite")
    {
        EXIT_CONFIG_CONFLICT
    } else if [
        "winhttp",
        "dns",
        "tls",
        "network",
        "http status",
        "server returned",
    ]
    .iter()
    .any(|needle| message.contains(needle))
    {
        EXIT_NETWORK_FAILURE
    } else {
        EXIT_RUNTIME_ERROR
    }
}

#[cfg(windows)]
fn write_stdout(value: &str) {
    write_windows_handle(
        windows_sys::Win32::System::Console::STD_OUTPUT_HANDLE,
        value,
    );
}

#[cfg(windows)]
fn write_stderr(value: &str) {
    write_windows_handle(windows_sys::Win32::System::Console::STD_ERROR_HANDLE, value);
}

#[cfg(windows)]
fn write_windows_handle(kind: u32, value: &str) {
    use windows_sys::Win32::{
        Foundation::INVALID_HANDLE_VALUE, Storage::FileSystem::WriteFile,
        System::Console::GetStdHandle,
    };

    unsafe {
        let handle = GetStdHandle(kind);
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            return;
        }
        for bytes in value.as_bytes().chunks(u32::MAX as usize) {
            let mut written = 0u32;
            if WriteFile(
                handle,
                bytes.as_ptr().cast(),
                bytes.len() as u32,
                &mut written,
                std::ptr::null_mut(),
            ) == 0
            {
                return;
            }
        }
    }
}

#[cfg(not(windows))]
fn write_stdout(value: &str) {
    print!("{value}");
}

#[cfg(not(windows))]
fn write_stderr(value: &str) {
    eprint!("{value}");
}

#[cfg(windows)]
fn attach_parent_console() {
    use windows_sys::Win32::{
        Foundation::{GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE},
        Storage::FileSystem::{CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING},
        System::Console::{
            AttachConsole, GetStdHandle, SetConsoleCP, SetConsoleOutputCP, SetStdHandle,
            ATTACH_PARENT_PROCESS, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
        },
    };

    unsafe {
        AttachConsole(ATTACH_PARENT_PROCESS);
        ensure_console_handle(STD_OUTPUT_HANDLE, "CONOUT$", GENERIC_READ | GENERIC_WRITE);
        ensure_console_handle(STD_ERROR_HANDLE, "CONOUT$", GENERIC_READ | GENERIC_WRITE);
        ensure_console_handle(STD_INPUT_HANDLE, "CONIN$", GENERIC_READ | GENERIC_WRITE);
        SetConsoleCP(65001);
        SetConsoleOutputCP(65001);
    }

    unsafe fn ensure_console_handle(kind: u32, device: &str, access: u32) {
        let current = GetStdHandle(kind);
        if !current.is_null() && current != INVALID_HANDLE_VALUE {
            return;
        }
        let device = device.encode_utf16().chain(Some(0)).collect::<Vec<_>>();
        let handle = CreateFileW(
            device.as_ptr(),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        );
        if handle != INVALID_HANDLE_VALUE {
            SetStdHandle(kind, handle);
        }
    }
}

#[cfg(not(windows))]
fn attach_parent_console() {}
