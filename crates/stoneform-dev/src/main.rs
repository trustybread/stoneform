//! `stoneform-dev`: run StoneForm's use cases from a terminal.
//!
//! ```text
//! stoneform-dev plan [--region eu] [--locale enUS] [--cache DIR]
//! stoneform-dev install FOLDER [--region eu] [--locale enUS] [--cache DIR]
//! ```
//!
//! `install` puts WoW: Forever into `FOLDER` (the `World of Warcraft` folder).
//! Interrupt it any time; running it again continues where it stopped.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Mutex;
use std::time::Instant;

use stoneform_adapter_blizzard::{BlizzardHttp, HttpContentDelivery, HttpVersionSource};
use stoneform_adapter_casc::CascGameStorage;
use stoneform_adapter_disk::{DiskInstallRecords, DiskLooseFiles};
use stoneform_app::{InstallProgress, InstallTarget, Installer, PlanStep};
use stoneform_domain::{Locale, Region, WOW_FOREVER};
use tracing_subscriber::EnvFilter;

const USAGE: &str = "usage: stoneform-dev (plan | install FOLDER) [--region eu|us|kr] [--locale enUS] [--cache DIR]";

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();
    let mut args = std::env::args().skip(1);
    let command = args.next();
    let mut args = args.peekable();
    let folder = args.next_if(|arg| !arg.starts_with("--")).map(PathBuf::from);
    let mut region = Region::Eu;
    let mut locale = Locale::parse("enUS").expect("enUS is a locale");
    let mut cache = directories::BaseDirs::new().map(|dirs| dirs.cache_dir().join("StoneForm"));
    let mut skip_storage = false;
    while let Some(flag) = args.next() {
        if flag == "--skip-storage" {
            // Dev only: everything but local storage, to test the rest quickly.
            skip_storage = true;
            continue;
        }
        let value = args.next();
        let parsed = match (flag.as_str(), value) {
            ("--region", Some(value)) => value.parse().map(|value| region = value).map_err(|e| e.to_string()),
            ("--locale", Some(value)) => Locale::parse(&value).map(|value| locale = value).map_err(|e| e.to_string()),
            ("--cache", Some(value)) => {
                cache = Some(PathBuf::from(value));
                Ok(())
            }
            _ => Err(USAGE.to_owned()),
        };
        if let Err(message) = parsed {
            eprintln!("{message}");
            return ExitCode::from(2);
        }
    }
    let Some(cache) = cache else {
        eprintln!("no cache folder; pass --cache");
        return ExitCode::from(2);
    };
    match command.as_deref() {
        Some("plan") => plan(region, &locale, cache).await,
        Some("install") => match folder {
            Some(folder) => install(region, &locale, cache, &folder, skip_storage).await,
            None => {
                eprintln!("{USAGE}");
                ExitCode::from(2)
            }
        },
        _ => {
            eprintln!("{USAGE}");
            ExitCode::from(2)
        }
    }
}

async fn plan(region: Region, locale: &Locale, cache: PathBuf) -> ExitCode {
    let started = Instant::now();
    let http = match BlizzardHttp::new() {
        Ok(http) => http,
        Err(error) => {
            eprintln!("cannot start HTTP: {error}");
            return ExitCode::FAILURE;
        }
    };
    let installer = Installer::new(HttpVersionSource::new(http.clone()), HttpContentDelivery::new(http, &cache));
    let progress = |step: PlanStep| match step {
        PlanStep::Indexes { done, total } if done % 200 == 0 || done == total => {
            eprintln!("  archive indexes: {done}/{total}");
        }
        PlanStep::Indexes { .. } => {}
        other => eprintln!("  {other:?}"),
    };
    let plan = match installer.plan(&WOW_FOREVER, region, locale, |_| false, &progress).await {
        Ok(plan) => plan,
        Err(error) => {
            eprintln!("planning failed: {error}");
            return ExitCode::FAILURE;
        }
    };
    let gib = |bytes: u64| bytes as f64 / (1u64 << 30) as f64;
    println!("{} {} ({region}, {locale})", WOW_FOREVER.display_name, plan.build.version);
    println!("tags:          {}", plan.tags);
    println!("loose files:   {} ({:.2} GiB)", plan.loose.len(), gib(plan.loose_bytes()));
    println!("storage files: {} ({:.2} GiB)", plan.storage.len(), gib(plan.storage_bytes()));
    println!("unarchived:    {}", plan.unarchived());
    let unlocated_loose =
        plan.loose.iter().filter(|file| matches!(file.source, stoneform_app::install::plan::Source::File(_))).count();
    println!("loose files fetched on their own: {unlocated_loose}");
    for file in plan.loose.iter().filter(|file| matches!(file.source, stoneform_app::install::plan::Source::File(_))) {
        println!("    {} {} ({} bytes encoded)", file.encoding_key, file.path, file.encoded_size);
    }
    println!("archives:      {}", plan.archives.len());
    println!("total:         {:.2} GiB, planned in {:.1?}", gib(plan.total_bytes()), started.elapsed());
    ExitCode::SUCCESS
}

async fn install(region: Region, locale: &Locale, cache: PathBuf, folder: &Path, skip_storage: bool) -> ExitCode {
    let started = Instant::now();
    let http = match BlizzardHttp::new() {
        Ok(http) => http,
        Err(error) => {
            eprintln!("cannot start HTTP: {error}");
            return ExitCode::FAILURE;
        }
    };
    let installer = Installer::new(HttpVersionSource::new(http.clone()), HttpContentDelivery::new(http, &cache))
        .with_disk(CascGameStorage::default(), DiskLooseFiles, DiskInstallRecords);
    let planning = |step: PlanStep| {
        if let PlanStep::Indexes { done, total } = step
            && done == total
        {
            eprintln!("  located files in {total} archives");
        }
    };
    let mut plan = match installer.plan(&WOW_FOREVER, region, locale, |_| false, &planning).await {
        Ok(plan) => plan,
        Err(error) => {
            eprintln!("planning failed: {error}");
            return ExitCode::FAILURE;
        }
    };
    if skip_storage {
        plan.storage.clear();
    }
    eprintln!("installing {} {} into {}", WOW_FOREVER.display_name, plan.build.version, folder.display());
    let last = Mutex::new((None, u64::MAX, Instant::now(), None::<u64>));
    let report = |progress: InstallProgress| {
        let percent = progress.done.saturating_mul(100) / progress.total.max(1);
        let mut last = last.lock().expect("never poisoned");
        // Speed counts this run's downloads, not what an earlier run stored.
        let first = *last.3.get_or_insert(progress.done);
        if last.0 != Some(progress.stage) || last.1 != percent {
            let elapsed = last.2.elapsed().as_secs_f64().max(0.001);
            let speed = progress.done.saturating_sub(first) as f64 / elapsed / 1e6;
            eprintln!(
                "  {:?} {percent}% ({:.2} of {:.2} GB, {speed:.1} MB/s this run)",
                progress.stage,
                progress.done as f64 / 1e9,
                progress.total as f64 / 1e9
            );
            *last = (Some(progress.stage), percent, last.2, last.3);
        }
    };
    let target = InstallTarget { root: folder, product: &WOW_FOREVER, region, locale };
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
