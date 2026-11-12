//! StoneForm: a launcher for WoW: Forever on Apple Silicon Macs.
//!
//! This crate is the composition root: it picks the adapters, hands them to the
//! use cases and runs the window.

mod app;
mod login;
mod main_thread;
mod ui;
mod updater;
mod volume;

use std::io::IsTerminal;
use std::sync::Mutex;

use objc2::MainThreadMarker;
use objc2_foundation::NSLocale;
use stoneform_adapter_blizzard::{BlizzardHttp, HttpContentDelivery, HttpVersionSource};
use stoneform_adapter_casc::CascGameStorage;
use stoneform_adapter_disk::{DiskInstallRecords, DiskLooseFiles, TomlSettingsStore};
use stoneform_adapter_macos::{KeychainSecretStore, MacGameProcess, PrefsTokenHandoff};
use stoneform_app::ports::SettingsStore;
use stoneform_app::{Installer, Launcher, Session};
use stoneform_domain::Region;
use tracing_subscriber::EnvFilter;

use crate::app::App;
use crate::login::WebKitLoginFlow;

fn main() {
    init_logging();
    let mtm = MainThreadMarker::new().expect("StoneForm starts on the main thread");

    let settings = match TomlSettingsStore::new() {
        Ok(settings) => settings,
        Err(error) => {
            eprintln!("StoneForm cannot find its settings folder: {error}");
            std::process::exit(1);
        }
    };
    // Timers and I/O: the HTTP client and the retry pauses need both.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("stoneform-worker")
        .enable_all()
        .build()
        .expect("the async runtime starts");
    let region = default_region();
    tracing::info!(%region, "StoneForm starting");
    let session = Session::new(WebKitLoginFlow, KeychainSecretStore::new(), settings.clone(), region);
    let Some(cache) = directories::BaseDirs::new().map(|dirs| dirs.cache_dir().join("StoneForm")) else {
        eprintln!("StoneForm cannot find its cache folder");
        std::process::exit(1);
    };
    let http = BlizzardHttp::new().expect("the HTTP client starts");
    http.throttle().set_limit(settings.load().ok().and_then(|saved| saved.download_limit));
    let throttle = http.throttle().clone();
    let installer = Installer::new(HttpVersionSource::new(http.clone()), HttpContentDelivery::new(http, cache))
        .with_disk(CascGameStorage::with_free_space(volume::available_bytes), DiskLooseFiles, DiskInstallRecords);
    let launcher = Launcher::new(KeychainSecretStore::new(), PrefsTokenHandoff::new(), MacGameProcess);
    app::install(App { session, settings, installer, launcher, records: DiskInstallRecords, throttle, runtime });

    ui::run(mtm);
}

/// To the terminal when there is one; else, as an app opened from Finder,
/// to `~/Library/Logs/StoneForm/StoneForm.log`, so problems leave a trace.
fn init_logging() {
    let filter = || EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let log_file = (!std::io::stderr().is_terminal())
        .then(directories::BaseDirs::new)
        .flatten()
        .map(|dirs| dirs.home_dir().join("Library/Logs/StoneForm"))
        .and_then(|dir| {
            std::fs::create_dir_all(&dir).ok()?;
            std::fs::OpenOptions::new().create(true).append(true).open(dir.join("StoneForm.log")).ok()
        });
    match log_file {
        Some(file) => {
            tracing_subscriber::fmt().with_env_filter(filter()).with_ansi(false).with_writer(Mutex::new(file)).init();
        }
        None => tracing_subscriber::fmt().with_env_filter(filter()).init(),
    }
}

/// The first guess at the play region, from the Mac's country setting.
fn default_region() -> Region {
    let country = NSLocale::currentLocale().regionCode().map(|code| code.to_string()).unwrap_or_default();
    Region::for_country(&country)
}
