//! The composition root's runtime side: use cases run on a tokio worker,
//! results come back to the window on the main thread.

use std::collections::VecDeque;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use objc2_foundation::NSLocale;
use stoneform_adapter_blizzard::{HttpContentDelivery, HttpVersionSource};
use stoneform_adapter_casc::CascGameStorage;
use stoneform_adapter_disk::{DiskInstallRecords, DiskLooseFiles, TomlSettingsStore};
use stoneform_adapter_macos::{KeychainSecretStore, MacGameProcess, PrefsTokenHandoff};
use stoneform_app::ports::{InstallRecords, SettingsStore};
use stoneform_app::{
    InstallProgress, InstallStage, InstallTarget, Installer, LaunchError, LaunchTarget, Launcher, Session, SignedIn,
};
use stoneform_domain::{AutopilotFacts, AutopilotStep, Locale, WOW_FOREVER, next_step};
use tokio::task::JoinHandle;

use crate::login::WebKitLoginFlow;
use crate::main_thread::on_main;
use crate::ui;

pub type AppSession = Session<WebKitLoginFlow, KeychainSecretStore, TomlSettingsStore>;
pub type AppLauncher = Launcher<KeychainSecretStore, PrefsTokenHandoff, MacGameProcess>;
pub type AppInstaller =
    Installer<HttpVersionSource, HttpContentDelivery, CascGameStorage, DiskLooseFiles, DiskInstallRecords>;

#[derive(Debug)]
pub struct App {
    pub session: AppSession,
    pub settings: TomlSettingsStore,
    pub installer: AppInstaller,
    pub launcher: AppLauncher,
    pub records: DiskInstallRecords,
    pub runtime: tokio::runtime::Runtime,
}

static APP: OnceLock<App> = OnceLock::new();

/// The running install, so Pause can stop it.
static INSTALL: Mutex<Option<JoinHandle<()>>> = Mutex::new(None);
/// Bumped when an install starts or pauses: progress from an older run that
/// is still queued for the window is dropped.
static INSTALL_RUN: AtomicU64 = AtomicU64::new(0);
/// The last progress shown, for the Paused state.
static LAST_DOWNLOAD: Mutex<Option<Download>> = Mutex::new(None);

/// Where StoneForm suggests installing when nothing was chosen yet.
const DEFAULT_INSTALL_PATH: &str = "/Applications/World of Warcraft";
/// How often StoneForm looks again whether Battle.net has quit.
const BATTLE_NET_POLL: Duration = Duration::from_secs(2);
/// How often progress reaches the window.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(250);
/// The window the download speed is averaged over.
const SPEED_WINDOW: Duration = Duration::from_secs(20);

/// What the window shows.
#[derive(Debug, Clone)]
pub enum Status {
    Checking,
    SigningIn,
    SignedOut,
    /// Waiting for the user to confirm folder and language.
    ChooseInstall(InstallChoice),
    Installing(Download),
    Paused(Download),
    /// Battle.net is running and StoneForm needs to write game files.
    WaitingForBattleNet,
    /// Handing the token over and starting the game; StoneForm quits after.
    Launching,
    Failed(String),
}

#[derive(Debug, Clone)]
pub struct InstallChoice {
    pub folder: PathBuf,
    pub locale: Locale,
    pub locales: Vec<Locale>,
    /// Why the last attempt failed, if it did.
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Download {
    pub stage: InstallStage,
    pub done: u64,
    /// 0 while the size is not known yet.
    pub total: u64,
    pub seconds_left: Option<u64>,
}

impl Download {
    const PREPARING: Download = Download { stage: InstallStage::Preparing, done: 0, total: 0, seconds_left: None };
}

pub fn install(app: App) {
    if APP.set(app).is_err() {
        panic!("the app is installed once, at start-up");
    }
}

fn app() -> &'static App {
    APP.get().expect("the app is installed at start-up")
}

fn spawn(work: impl Future<Output = ()> + Send + 'static) {
    app().runtime.spawn(work);
}

fn show(status: Status) {
    on_main(move |mtm| ui::show(mtm, status));
}

/// Runs the autopilot from the top: sign in, install, then ready.
pub fn start() {
    show(Status::Checking);
    spawn(autopilot());
}

pub fn sign_in() {
    spawn(async {
        if run_sign_in().await.is_some() {
            autopilot().await;
        }
    });
}

pub fn sign_out() {
    show(Status::Checking);
    spawn(async {
        let status = match app().session.sign_out().await {
            Ok(()) => Status::SignedOut,
            Err(error) => Status::Failed(error.to_string()),
        };
        show(status);
    });
}

/// Installs into `folder`, or continues an interrupted install there.
pub fn start_install(folder: PathBuf, locale: Locale) {
    let mut running = INSTALL.lock().expect("never poisoned");
    if running.as_ref().is_some_and(|task| !task.is_finished()) {
        return;
    }
    let run = INSTALL_RUN.fetch_add(1, Ordering::SeqCst) + 1;
    show_download(run, Download::PREPARING);
    *running = Some(app().runtime.spawn(async move {
        match run_install(run, &folder, &locale).await {
            Ok(()) => autopilot().await,
            Err(error) => {
                tracing::error!(%error, "install failed");
                let locales = forever_locales();
                show(Status::ChooseInstall(InstallChoice { folder, locale, locales, error: Some(error) }));
            }
        }
    }));
}

/// Stops the download; what is stored stays, and Resume continues from there.
pub fn pause_install() {
    INSTALL_RUN.fetch_add(1, Ordering::SeqCst);
    if let Some(task) = INSTALL.lock().expect("never poisoned").take() {
        task.abort();
    }
    let last = LAST_DOWNLOAD.lock().expect("never poisoned").unwrap_or(Download::PREPARING);
    show(Status::Paused(Download { seconds_left: None, ..last }));
}

pub fn resume_install() {
    spawn(async {
        match app().settings.load() {
            Ok(settings) => match (settings.install_path, settings.locale) {
                (Some(folder), Some(locale)) => start_install(folder, locale),
                _ => autopilot().await,
            },
            Err(error) => show(Status::Failed(error.to_string())),
        }
    });
}

async fn run_install(run: u64, folder: &Path, locale: &Locale) -> Result<(), String> {
    let mut settings = app().settings.load().map_err(|error| error.to_string())?;
    settings.install_path = Some(folder.to_owned());
    settings.locale = Some(locale.clone());
    app().settings.save(&settings).map_err(|error| error.to_string())?;
    // From settings, not the token: reading the token can make macOS ask for
    // the Keychain password, and installing doesn't need it.
    let region = app().session.region().map_err(|error| error.to_string())?;
    tracing::info!(folder = %folder.display(), %locale, %region, "installing");

    let installer = &app().installer;
    let plan = installer.plan(&WOW_FOREVER, region, locale, |_| false, &|_| {}).await.map_err(|e| e.to_string())?;
    let meter = Mutex::new(Meter::default());
    let target = InstallTarget { root: folder, product: &WOW_FOREVER, region, locale };
    installer
        .apply(&plan, target, &|progress| {
            if let Some(download) = meter.lock().expect("never poisoned").update(progress) {
                show_download(run, download);
            }
        })
        .await
        .map_err(|error| error.to_string())
}

/// Shows install progress, unless a newer run or a pause made it stale.
fn show_download(run: u64, download: Download) {
    *LAST_DOWNLOAD.lock().expect("never poisoned") = Some(download);
    on_main(move |mtm| {
        if INSTALL_RUN.load(Ordering::SeqCst) == run {
            ui::show(mtm, Status::Installing(download));
        }
    });
}

/// Turns the installer's many progress calls into a few window updates with
/// a speed and time left.
#[derive(Debug, Default)]
struct Meter {
    last_shown: Option<(Instant, InstallStage)>,
    samples: VecDeque<(Instant, u64)>,
}

impl Meter {
    fn update(&mut self, progress: InstallProgress) -> Option<Download> {
        let now = Instant::now();
        self.samples.push_back((now, progress.done));
        while self.samples.front().is_some_and(|(at, _)| now.duration_since(*at) > SPEED_WINDOW) {
            self.samples.pop_front();
        }
        let due = match self.last_shown {
            Some((at, stage)) => stage != progress.stage || now.duration_since(at) >= PROGRESS_INTERVAL,
            None => true,
        };
        if !due {
            return None;
        }
        self.last_shown = Some((now, progress.stage));
        let seconds_left = self.samples.front().and_then(|&(since, from)| {
            let seconds = now.duration_since(since).as_secs_f64();
            let speed = progress.done.saturating_sub(from) as f64 / seconds;
            (seconds >= 3.0 && speed > 0.0)
                .then(|| (progress.total.saturating_sub(progress.done) as f64 / speed) as u64)
        });
        Some(Download { stage: progress.stage, done: progress.done, total: progress.total, seconds_left })
    }
}

async fn autopilot() {
    loop {
        let signed_in = match app().session.current() {
            Ok(signed_in) => signed_in.is_some(),
            Err(error) => return show(Status::Failed(error.to_string())),
        };
        let settings = match app().settings.load() {
            Ok(settings) => settings,
            Err(error) => return show(Status::Failed(error.to_string())),
        };
        let installed = match settings.install_path.as_deref() {
            Some(folder) => match app().records.installed(folder, &WOW_FOREVER) {
                Ok(installed) => installed,
                Err(error) => return show(Status::Failed(error.to_string())),
            },
            None => None,
        };
        let root = settings.install_path.clone().unwrap_or_else(|| PathBuf::from(DEFAULT_INSTALL_PATH));
        let running = app().launcher.running(&root);
        let facts = AutopilotFacts {
            signed_in,
            installed: installed.is_some(),
            game_running: running.game.is_some(),
            battle_net_running: running.battle_net,
        };
        match next_step(&facts) {
            AutopilotStep::BringGameForward => {
                if let Some(pid) = running.game {
                    tracing::info!(pid, "the game is already running; bringing it forward");
                    app().launcher.bring_forward(pid);
                }
                return quit();
            }
            AutopilotStep::SignIn => {
                if run_sign_in().await.is_none() {
                    return;
                }
            }
            AutopilotStep::WaitForBattleNet => {
                show(Status::WaitingForBattleNet);
                tokio::time::sleep(BATTLE_NET_POLL).await;
            }
            AutopilotStep::Install => {
                // A folder that already holds a started install: carry on
                // without asking again.
                if let (Some(folder), Some(locale)) = (&settings.install_path, &settings.locale)
                    && folder.join("Data").is_dir()
                {
                    return start_install(folder.clone(), locale.clone());
                }
                let locales = forever_locales();
                let locale = settings.locale.unwrap_or_else(|| default_locale(&locales));
                return show(Status::ChooseInstall(InstallChoice { folder: root, locale, locales, error: None }));
            }
            AutopilotStep::Launch => {
                show(Status::Launching);
                match launch(&root, settings.locale.as_ref()) {
                    Ok(()) => return quit(),
                    // The saved token is gone or unreadable: sign in, then try again.
                    Err(LaunchError::SignInRequired) => {
                        if run_sign_in().await.is_none() {
                            return;
                        }
                    }
                    Err(error) => return show(Status::Failed(error.to_string())),
                }
            }
        }
    }
}

/// Hands the token to the game and starts it. Reading the token is the one
/// Keychain read StoneForm makes on a normal start.
fn launch(root: &Path, locale: Option<&Locale>) -> Result<(), LaunchError> {
    let signed_in = match app().session.current() {
        Ok(Some(signed_in)) => signed_in,
        Ok(None) => return Err(LaunchError::SignInRequired),
        Err(error) => return Err(LaunchError::System(stoneform_app::ports::SystemError::new(error))),
    };
    let target =
        LaunchTarget { root, product: &WOW_FOREVER, account: &signed_in.account, region: signed_in.region, locale };
    app().launcher.launch(target).map(drop)
}

/// The game runs on its own; StoneForm's work is done.
fn quit() {
    on_main(|mtm| {
        let app = objc2_app_kit::NSApplication::sharedApplication(mtm);
        app.terminate(None);
    });
}

/// Shows the sign-in window; `None` when it was cancelled or failed.
async fn run_sign_in() -> Option<SignedIn> {
    show(Status::SigningIn);
    match app().session.sign_in().await {
        Ok(signed_in) => Some(signed_in),
        Err(error) if error.is_cancelled() => {
            show(Status::SignedOut);
            None
        }
        Err(error) => {
            show(Status::Failed(error.to_string()));
            None
        }
    }
}

fn forever_locales() -> Vec<Locale> {
    WOW_FOREVER.locales.iter().filter_map(|code| Locale::parse(code).ok()).collect()
}

/// The Mac's preferred language, among the game's.
fn default_locale(locales: &[Locale]) -> Locale {
    let preferred: Vec<String> = NSLocale::preferredLanguages().iter().map(|tag| tag.to_string()).collect();
    let preferred: Vec<&str> = preferred.iter().map(String::as_str).collect();
    Locale::best_match(&preferred, locales).unwrap_or_else(|| Locale::parse("enUS").expect("enUS is a locale"))
}
