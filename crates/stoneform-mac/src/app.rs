//! The composition root's runtime side: use cases run on a tokio worker,
//! results come back to the window on the main thread.

use std::collections::VecDeque;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use objc2_foundation::NSLocale;
use stoneform_adapter_blizzard::{HttpContentDelivery, HttpVersionSource, Throttle};
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
    /// The download speed limit every download shares.
    pub throttle: Throttle,
    pub runtime: tokio::runtime::Runtime,
}

static APP: OnceLock<App> = OnceLock::new();

/// The running install, so Pause can stop it.
static INSTALL: Mutex<Option<JoinHandle<()>>> = Mutex::new(None);
/// Bumped when an install starts or pauses: progress from an older run that
/// is still queued for the window is dropped.
static INSTALL_RUN: AtomicU64 = AtomicU64::new(0);
/// The user held ⌥ at launch: stop before starting the game.
static HOLD: AtomicBool = AtomicBool::new(false);
/// The last progress shown, for the Paused state.
static LAST_DOWNLOAD: Mutex<Option<Download>> = Mutex::new(None);

/// Where StoneForm suggests installing when nothing was chosen yet.
const DEFAULT_INSTALL_PATH: &str = "/Applications/World of Warcraft";
/// How long quitting waits for Sparkle to finish checking and downloading
/// a StoneForm update; StoneForm itself is a few megabytes.
const SELF_UPDATE_GRACE: Duration = Duration::from_secs(20);
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
    /// Installed and current, at this version, and the user held ⌥ at
    /// launch: the window waits with a Play button, so its menus are usable.
    Ready(String),
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
    pub purpose: Purpose,
}

/// Why the installer runs; the same work, told differently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    Install,
    Update,
    /// Verify and Repair: every file checked, what's missing or wrong fetched.
    Repair,
}

impl Download {
    fn preparing(purpose: Purpose) -> Download {
        Download { stage: InstallStage::Preparing, done: 0, total: 0, seconds_left: None, purpose }
    }
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

/// Runs the autopilot from the top: sign in, install, update, launch.
/// `hold`: stop before launching (the user held ⌥ at launch).
pub fn start(hold: bool) {
    HOLD.store(hold, Ordering::SeqCst);
    show(Status::Checking);
    spawn(autopilot());
}

/// The download speed limit in bytes per second, if any.
pub fn download_limit() -> Option<u64> {
    app().throttle.limit()
}

/// Sets the download speed limit, at once for running downloads too, and
/// remembers it.
pub fn set_download_limit(limit: Option<u64>) {
    app().throttle.set_limit(limit);
    spawn(async move {
        let saved = app().settings.load().and_then(|mut settings| {
            settings.download_limit = limit;
            app().settings.save(&settings)
        });
        if let Err(error) = saved {
            tracing::warn!(%error, "could not save the download speed limit");
        }
    });
}

/// Play, from the window a held start leaves open.
pub fn play() {
    HOLD.store(false, Ordering::SeqCst);
    show(Status::Checking);
    spawn(autopilot());
}

/// Verify and Repair: checks every file of the install against the build's
/// manifests and fetches what is missing or wrong.
pub fn repair() {
    spawn(async {
        let settings = match app().settings.load() {
            Ok(settings) => settings,
            Err(error) => return show(Status::Failed(error.to_string())),
        };
        let installed = settings
            .install_path
            .as_deref()
            .and_then(|folder| app().records.installed(folder, &WOW_FOREVER).ok().flatten());
        match (settings.install_path, installed) {
            (Some(folder), Some(_)) => {
                let locale = settings.locale.unwrap_or_else(|| default_locale(&forever_locales()));
                begin_install(folder, locale, Some(Purpose::Repair));
            }
            // Nothing installed to repair: the autopilot offers to install.
            _ => autopilot().await,
        }
    });
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

/// Installs into `folder`, updates the game there, or continues either
/// after an interruption: one path, since an update is an install that
/// finds most of its files in place.
pub fn start_install(folder: PathBuf, locale: Locale) {
    begin_install(folder, locale, None);
}

/// `purpose`: told by the caller for a repair, else worked out from the folder.
fn begin_install(folder: PathBuf, locale: Locale, purpose: Option<Purpose>) {
    let mut running = INSTALL.lock().expect("never poisoned");
    if running.as_ref().is_some_and(|task| !task.is_finished()) {
        return;
    }
    let run = INSTALL_RUN.fetch_add(1, Ordering::SeqCst) + 1;
    let purpose = purpose.unwrap_or_else(|| match app().records.installed(&folder, &WOW_FOREVER).ok().flatten() {
        Some(_) => Purpose::Update,
        None => Purpose::Install,
    });
    show_download(run, Download::preparing(purpose));
    *running = Some(app().runtime.spawn(async move {
        match run_install(run, &folder, &locale, purpose).await {
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
    let last = LAST_DOWNLOAD.lock().expect("never poisoned").unwrap_or(Download::preparing(Purpose::Install));
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

async fn run_install(run: u64, folder: &Path, locale: &Locale, purpose: Purpose) -> Result<(), String> {
    let mut settings = app().settings.load().map_err(|error| error.to_string())?;
    settings.install_path = Some(folder.to_owned());
    settings.locale = Some(locale.clone());
    app().settings.save(&settings).map_err(|error| error.to_string())?;
    // From settings, not the token: reading the token can make macOS ask for
    // the Keychain password, and installing doesn't need it.
    let region = app().session.region().map_err(|error| error.to_string())?;
    let installed = app().records.installed(folder, &WOW_FOREVER).map_err(|error| error.to_string())?;
    tracing::info!(folder = %folder.display(), %locale, %region, ?purpose, "installing");

    let installer = &app().installer;
    let plan = installer
        .plan(&WOW_FOREVER, region, locale, installed.as_ref(), &|_| {})
        .await
        .map_err(|error| error.to_string())?;
    let meter = Mutex::new(Meter::new(purpose));
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
#[derive(Debug)]
struct Meter {
    last_shown: Option<(Instant, InstallStage)>,
    samples: VecDeque<(Instant, u64)>,
    purpose: Purpose,
}

impl Meter {
    fn new(purpose: Purpose) -> Self {
        Self { last_shown: None, samples: VecDeque::new(), purpose }
    }

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
        let (stage, done, total, purpose) = (progress.stage, progress.done, progress.total, self.purpose);
        Some(Download { stage, done, total, seconds_left, purpose })
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
        let update_available = match (&installed, signed_in) {
            (Some(installed), true) => update_available(installed).await,
            _ => false,
        };
        let facts = AutopilotFacts {
            signed_in,
            installed: installed.is_some(),
            game_running: running.game.is_some(),
            battle_net_running: running.battle_net,
            update_available,
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
            AutopilotStep::Update => {
                let installed = installed.as_ref().expect("an update needs an install");
                // The language the install was made in; else the one its tags name.
                let locale = settings.locale.clone().or_else(|| {
                    stoneform_tact::languages::text(&installed.tags).and_then(|text| Locale::parse(text).ok())
                });
                return start_install(root, locale.unwrap_or_else(|| default_locale(&forever_locales())));
            }
            AutopilotStep::Launch if HOLD.load(Ordering::SeqCst) => {
                let version = installed.map(|build| build.version).unwrap_or_default();
                return show(Status::Ready(version));
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

/// Whether Blizzard serves a newer build. Offline, or when the version
/// service fails, there is no update: the installed build is launched.
async fn update_available(installed: &stoneform_app::ports::InstalledBuild) -> bool {
    let region = match app().session.region() {
        Ok(region) => region,
        Err(error) => {
            tracing::warn!(%error, "cannot tell the region; not checking for updates");
            return false;
        }
    };
    match app().installer.check(&WOW_FOREVER, region, installed).await {
        Ok(Some(live)) => {
            tracing::info!(installed = %installed.version, live = %live.version, "an update is available");
            true
        }
        Ok(None) => false,
        Err(error) => {
            tracing::warn!(%error, "cannot check for updates; launching the installed build");
            false
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
    spawn(async {
        // A self-update found at launch gets a moment to download: Sparkle
        // installs it when StoneForm quits, so quitting first would lose it.
        let deadline = Instant::now() + SELF_UPDATE_GRACE;
        while !crate::updater::settled() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        on_main(|mtm| {
            let app = objc2_app_kit::NSApplication::sharedApplication(mtm);
            app.terminate(None);
        });
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
