//! Main application entry point for Ubuntu Miracast Server.
//!
//! Bootstraps logging, parses CLI flags (mirroring the Python argparse), and
//! dispatches to headless service mode or the GTK GUI.

use clap::Parser;
use std::io::Write;
use std::path::PathBuf;

/// Ubuntu Miracast Server — receive wireless display streams.
#[derive(Parser, Debug)]
#[command(
    name = "ubuntu-miracast-server",
    about = "Ubuntu Miracast Server — receive wireless display streams"
)]
struct Cli {
    /// Run in headless service mode (no GUI).
    #[arg(long)]
    service: bool,

    /// Start in fullscreen mode.
    #[arg(long)]
    fullscreen: bool,

    /// P2P device interface to use (e.g. p2p-dev-wlx...). Auto-detected if omitted.
    #[arg(long)]
    interface: Option<String>,

    /// Override the advertised device name.
    #[arg(long)]
    name: Option<String>,
}

fn log_file_path() -> PathBuf {
    let dir = dirs::home_dir()
        .unwrap_or_default()
        .join(".local")
        .join("share")
        .join("ubuntu-miracast-server")
        .join("logs");
    let _ = std::fs::create_dir_all(&dir);
    dir.join("miracast-server.log")
}

/// Configure logging to a file and stderr, using the Python format
/// `%(asctime)s - %(name)s - %(levelname)s - %(message)s` at INFO level.
fn init_logging() {
    use std::sync::Mutex;

    struct DualLogger {
        file: Mutex<Option<std::fs::File>>,
    }
    impl log::Log for DualLogger {
        fn enabled(&self, meta: &log::Metadata) -> bool {
            meta.level() <= log::Level::Info
        }
        fn log(&self, record: &log::Record) {
            if !self.enabled(record.metadata()) {
                return;
            }
            let ts = chrono::Local::now().format("%Y-%m-%d %H:%M:%S,%3f");
            let line = format!(
                "{} - {} - {} - {}",
                ts,
                record.target(),
                record.level(),
                record.args()
            );
            eprintln!("{line}");
            if let Ok(mut guard) = self.file.lock() {
                if let Some(f) = guard.as_mut() {
                    let _ = writeln!(f, "{line}");
                }
            }
        }
        fn flush(&self) {
            if let Ok(mut guard) = self.file.lock() {
                if let Some(f) = guard.as_mut() {
                    let _ = f.flush();
                }
            }
        }
    }

    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_file_path())
        .ok();
    let logger = DualLogger {
        file: Mutex::new(file),
    };
    let _ = log::set_boxed_logger(Box::new(logger));
    log::set_max_level(log::LevelFilter::Info);
}

fn main() {
    init_logging();
    let cli = Cli::parse();

    if cli.service {
        let code = ubuntu_miracast_server::service::run_as_service(cli.name, cli.interface);
        std::process::exit(code);
    }

    #[cfg(feature = "gui")]
    {
        let code = ubuntu_miracast_server::ui::run_gui(cli.name, cli.fullscreen, cli.interface);
        std::process::exit(code);
    }

    #[cfg(not(feature = "gui"))]
    {
        let _ = cli.fullscreen;
        eprintln!(
            "This build was compiled without the GUI feature. \
             Run with --service for headless mode, or rebuild with --features gui."
        );
        std::process::exit(2);
    }
}
