//! The composition root's runtime side: use cases run on a tokio worker,
//! results come back to the window on the main thread.

use std::future::Future;
use std::sync::OnceLock;

use stoneform_adapter_disk::TomlSettingsStore;
use stoneform_adapter_macos::KeychainSecretStore;
use stoneform_app::{Session, SignedIn};

use crate::login::WebKitLoginFlow;
use crate::main_thread::on_main;
use crate::ui;

pub type AppSession = Session<WebKitLoginFlow, KeychainSecretStore, TomlSettingsStore>;

#[derive(Debug)]
pub struct App {
    pub session: AppSession,
    pub runtime: tokio::runtime::Runtime,
}

static APP: OnceLock<App> = OnceLock::new();

/// What the window shows.
#[derive(Debug, Clone)]
pub enum Status {
    Checking,
    SigningIn,
    SignedIn(SignedIn),
    SignedOut,
    Failed(String),
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

/// The start of the autopilot: signed in already, or sign in now.
pub fn start() {
    spawn(async {
        match app().session.current() {
            Ok(Some(signed_in)) => show(Status::SignedIn(signed_in)),
            Ok(None) => run_sign_in().await,
            Err(error) => show(Status::Failed(error.to_string())),
        }
    });
}

pub fn sign_in() {
    spawn(run_sign_in());
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

async fn run_sign_in() {
    show(Status::SigningIn);
    let status = match app().session.sign_in().await {
        Ok(signed_in) => Status::SignedIn(signed_in),
        Err(error) if error.is_cancelled() => Status::SignedOut,
        Err(error) => Status::Failed(error.to_string()),
    };
    show(status);
}
