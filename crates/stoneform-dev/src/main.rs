//! `stoneform-dev`: run StoneForm's use cases from a terminal.
//!
//! ```text
//! stoneform-dev plan                [options]
//! stoneform-dev install FOLDER      [options] [--skip-storage]
//! stoneform-dev check FOLDER        [options]
//! stoneform-dev launch FOLDER
//!
//! options: --region eu|us|kr  --locale enUS  --cache DIR
//!          --build-config HEX --cdn-config HEX --version NAME   (pin an older build)
//! ```
//!
//! `FOLDER` is a `World of Warcraft` folder. `install` installs into it, or
//! updates what is there; interrupt it any time and run it again to continue.
//! `launch` uses StoneForm's saved sign-in, like the app.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Mutex;
use std::time::Instant;

use stoneform_adapter_blizzard::{BlizzardHttp, HttpContentDelivery, HttpVersionSource};
use stoneform_adapter_casc::CascGameStorage;
use stoneform_adapter_disk::{DiskInstallRecords, DiskLooseFiles, TomlSettingsStore};
use stoneform_adapter_macos::{KeychainSecretStore, MacGameProcess, PrefsTokenHandoff};
use stoneform_app::install::plan::Source;
use stoneform_app::ports::{InstallRecords, SettingsStore, VersionSource};
use stoneform_app::{InstallPlan, InstallProgress, InstallTarget, Installer, LaunchTarget, Launcher, PlanStep};
use stoneform_domain::{Locale, Region, WOW_FOREVER};
use stoneform_tact::keys::ContentKey;
use tracing_subscriber::EnvFilter;

const USAGE: &str = "usage: stoneform-dev (plan | install FOLDER | check FOLDER | launch FOLDER) \
     [--region eu|us|kr] [--locale enUS] [--cache DIR] [--skip-storage] \
     [--build-config HEX --cdn-config HEX --version NAME]";

/// What the command line asked for.
#[derive(Debug)]
struct Options {
    region: Region,
    locale: Locale,
    cache: PathBuf,
    skip_storage: bool,
    /// A build to use instead of the live one: build config, CDN config, version.
    pinned: Option<(String, String, String)>,
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();
    let mut args = std::env::args().skip(1).peekable();
    let command = args.next();
    let folder = args.next_if(|arg| !arg.starts_with("--")).map(PathBuf::from);
    let options = match parse_options(args) {
        Ok(options) => options,
        Err(message) => {
            eprintln!("{message}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    match (command.as_deref(), folder) {
        (Some("plan"), _) => plan(&options).await,
        (Some("install"), Some(folder)) => install(&options, &folder).await,
        (Some("check"), Some(folder)) => check(&options, &folder).await,
        (Some("launch"), Some(folder)) => launch(&folder),
        _ => {
            eprintln!("{USAGE}");
            ExitCode::from(2)
        }
    }
}

fn parse_options(mut args: impl Iterator<Item = String>) -> Result<Options, String> {
    let mut options = Options {
        region: Region::Eu,
        locale: Locale::parse("enUS").expect("enUS is a locale"),
        cache: directories::BaseDirs::new()
            .map(|dirs| dirs.cache_dir().join("StoneForm"))
            .ok_or("no cache folder; pass --cache")?,
        skip_storage: false,
        pinned: None,
    };
    let (mut build_config, mut cdn_config, mut version) = (None, None, None);
    while let Some(flag) = args.next() {
        if flag == "--skip-storage" {
            // Dev only: everything but local storage, to test the rest quickly.
            options.skip_storage = true;
            continue;
        }
        let value = args.next().ok_or_else(|| format!("{flag} needs a value"))?;
        match flag.as_str() {
            "--region" => options.region = value.parse().map_err(|e| format!("{e}"))?,
            "--locale" => options.locale = Locale::parse(&value).map_err(|e| e.to_string())?,
            "--cache" => options.cache = PathBuf::from(value),
            "--build-config" => build_config = Some(value),
            "--cdn-config" => cdn_config = Some(value),
            "--version" => version = Some(value),
            _ => return Err(format!("unknown option {flag}")),
        }
    }
    options.pinned = match (build_config, cdn_config, version) {
        (Some(build), Some(cdn), Some(version)) => Some((build, cdn, version)),
        (None, None, None) => None,
        _ => return Err("--build-config, --cdn-config and --version go together".to_owned()),
    };
    Ok(options)
}

type DevInstaller =
    Installer<HttpVersionSource, HttpContentDelivery, CascGameStorage, DiskLooseFiles, DiskInstallRecords>;

fn installer(options: &Options) -> Result<DevInstaller, String> {
    let http = BlizzardHttp::new().map_err(|error| format!("cannot start HTTP: {error}"))?;
    Ok(Installer::new(HttpVersionSource::new(http.clone()), HttpContentDelivery::new(http, &options.cache)).with_disk(
        CascGameStorage::default(),
        DiskLooseFiles,
        DiskInstallRecords,
    ))
}

/// The live build's plan, or the pinned build's.
async fn make_plan(
    installer: &DevInstaller,
    options: &Options,
    installed: Option<&stoneform_app::ports::InstalledBuild>,
) -> Result<InstallPlan, String> {
    let progress = |step: PlanStep| {
        if let PlanStep::Indexes { done, total } = step
            && done == total
        {
            eprintln!("  located files in {total} archives");
        }
    };
    let (region, locale) = (options.region, &options.locale);
    let planned = match &options.pinned {
        None => installer.plan(&WOW_FOREVER, region, locale, installed, &progress).await,
        Some((build_config, cdn_config, version)) => {
            let http = BlizzardHttp::new().map_err(|error| error.to_string())?;
            let mut build =
                HttpVersionSource::new(http).live(WOW_FOREVER.code, region).await.map_err(|e| e.to_string())?;
            build.build_config = ContentKey::from_hex(build_config).map_err(|e| e.to_string())?;
            build.cdn_config = ContentKey::from_hex(cdn_config).map_err(|e| e.to_string())?;
            build.version = version.clone();
            installer.plan_build(&WOW_FOREVER, region, locale, build, installed, &progress).await
        }
    };
    planned.map_err(|error| format!("planning failed: {error}"))
}

async fn plan(options: &Options) -> ExitCode {
    let started = Instant::now();
    let result = async {
        let installer = installer(options)?;
        make_plan(&installer, options, None).await
    };
    let plan = match result.await {
        Ok(plan) => plan,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };
    let gib = |bytes: u64| bytes as f64 / (1u64 << 30) as f64;
    println!("{} {} ({}, {})", WOW_FOREVER.display_name, plan.build.version, options.region, options.locale);
    println!("tags:          {}", plan.tags);
    println!("loose files:   {} ({:.2} GiB)", plan.loose.len(), gib(plan.loose_bytes()));
    println!("storage files: {} ({:.2} GiB)", plan.storage.len(), gib(plan.storage_bytes()));
    println!("unarchived:    {}", plan.unarchived());
    let on_their_own: Vec<_> = plan.loose.iter().filter(|file| matches!(file.source, Source::File(_))).collect();
    println!("loose files fetched on their own: {}", on_their_own.len());
    for file in on_their_own {
        println!("    {} {} ({} bytes encoded)", file.encoding_key, file.path, file.encoded_size);
    }
    println!("archives:      {}", plan.archives.len());
    println!("total:         {:.2} GiB, planned in {:.1?}", gib(plan.total_bytes()), started.elapsed());
    ExitCode::SUCCESS
}

async fn install(options: &Options, folder: &Path) -> ExitCode {
    let started = Instant::now();
    let installed = match DiskInstallRecords.installed(folder, &WOW_FOREVER) {
        Ok(installed) => installed,
        Err(error) => {
            eprintln!("cannot read {}: {error}", folder.display());
            return ExitCode::FAILURE;
        }
    };
    let installer = match installer(options) {
        Ok(installer) => installer,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };
    let mut plan = match make_plan(&installer, options, installed.as_ref()).await {
        Ok(plan) => plan,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };
    if options.skip_storage {
        plan.storage.clear();
    }
    let from = installed.as_ref().map(|build| build.version.as_str()).unwrap_or("nothing");
    eprintln!(
        "{} {} → {} in {} ({} obsolete files)",
        WOW_FOREVER.display_name,
        from,
        plan.build.version,
        folder.display(),
        plan.obsolete.len()
    );
    let printer = Mutex::new(Printer::default());
    let report = |progress: InstallProgress| printer.lock().expect("never poisoned").report(progress);
    let target = InstallTarget { root: folder, product: &WOW_FOREVER, region: options.region, locale: &options.locale };
    match installer.apply(&plan, target, &report).await {
        Ok(()) => {
            println!("installed {} in {:.1?}", plan.build.version, started.elapsed());
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("install failed: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Prints install progress once per percent and stage. Speed counts from the
/// first download report: what is already in place arrives at once.
#[derive(Debug, Default)]
struct Printer {
    shown: Option<(stoneform_app::InstallStage, u64)>,
    downloads_from: Option<(Instant, u64)>,
}

impl Printer {
    fn report(&mut self, progress: InstallProgress) {
        let percent = progress.done.saturating_mul(100) / progress.total.max(1);
        if progress.stage == stoneform_app::InstallStage::Downloading && self.downloads_from.is_none() {
            self.downloads_from = Some((Instant::now(), progress.done));
        }
        if self.shown == Some((progress.stage, percent)) {
            return;
        }
        self.shown = Some((progress.stage, percent));
        let speed = self.downloads_from.map_or(0.0, |(since, from)| {
            progress.done.saturating_sub(from) as f64 / since.elapsed().as_secs_f64().max(0.001) / 1e6
        });
        eprintln!(
            "  {:?} {percent}% ({:.2} of {:.2} GB, {speed:.1} MB/s this run)",
            progress.stage,
            progress.done as f64 / 1e9,
            progress.total as f64 / 1e9
        );
    }
}

async fn check(options: &Options, folder: &Path) -> ExitCode {
    let installed = match DiskInstallRecords.installed(folder, &WOW_FOREVER) {
        Ok(Some(installed)) => installed,
        Ok(None) => {
            println!("not installed in {}", folder.display());
            return ExitCode::FAILURE;
        }
        Err(error) => {
            eprintln!("cannot read {}: {error}", folder.display());
            return ExitCode::FAILURE;
        }
    };
    let installer = match installer(options) {
        Ok(installer) => installer,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };
    match installer.check(&WOW_FOREVER, options.region, &installed).await {
        Ok(Some(live)) => println!("installed {}, live {}: an update is available", installed.version, live.version),
        Ok(None) => println!("installed {}: up to date", installed.version),
        Err(error) => {
            eprintln!("cannot check: {error}");
            return ExitCode::FAILURE;
        }
    }
    ExitCode::SUCCESS
}

/// Starts WoW from `folder` with StoneForm's saved sign-in.
fn launch(folder: &Path) -> ExitCode {
    let settings = TomlSettingsStore::new().and_then(|store| store.load());
    let settings = match settings {
        Ok(settings) => settings,
        Err(error) => {
            eprintln!("cannot read StoneForm's settings: {error}");
            return ExitCode::FAILURE;
        }
    };
    let Some(account) = settings.account else {
        eprintln!("not signed in; sign in with the app first");
        return ExitCode::FAILURE;
    };
    let launcher = Launcher::new(KeychainSecretStore::new(), PrefsTokenHandoff::new(), MacGameProcess);
    let target = LaunchTarget {
        root: folder,
        product: &WOW_FOREVER,
        account: &account,
        region: settings.region.unwrap_or(Region::Eu),
        locale: settings.locale.as_ref(),
    };
    match launcher.launch(target) {
        Ok(pid) => {
            println!("started WoW (pid {pid}) from {}", folder.display());
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("cannot launch: {error}");
            ExitCode::FAILURE
        }
    }
}
