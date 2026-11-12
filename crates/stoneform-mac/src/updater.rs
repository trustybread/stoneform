//! StoneForm keeps itself up to date with Sparkle, quietly: an update is
//! found and downloaded in the background and installed when StoneForm quits.
//! Sparkle's own windows never appear. Ported from Waypoint's `AppUpdater`
//! (0BSD).
//!
//! Sparkle is loaded at run time from the app bundle, and only when the
//! bundle's `Info.plist` names a feed: release builds do (see
//! `scripts/bundle.sh`), `cargo run` and local bundles don't, so a
//! development copy never replaces itself.

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};

use block2::DynBlock;
use objc2::rc::{Allocated, Retained};
use objc2::runtime::{AnyClass, AnyObject};
use objc2::{MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_foundation::{NSBundle, NSError, NSObject, NSObjectProtocol, NSString};

/// False while Sparkle is checking or downloading: quitting then would cut
/// a self-update short. True when there is no updater at all.
static SETTLED: AtomicBool = AtomicBool::new(true);

thread_local! {
    /// Sparkle's controller and our delegate (which Sparkle holds weakly),
    /// alive as long as the app.
    static UPDATER: RefCell<Option<(Retained<AnyObject>, Retained<UpdaterDelegate>)>> = const { RefCell::new(None) };
}

/// Starts Sparkle and a background check, if this build has a feed.
pub fn start(mtm: MainThreadMarker) {
    let main = NSBundle::mainBundle();
    if main.objectForInfoDictionaryKey(&NSString::from_str("SUFeedURL")).is_none() {
        tracing::debug!("no update feed in this build; self-update is off");
        return;
    }
    let Some(frameworks) = main.privateFrameworksPath() else { return };
    let path = format!("{frameworks}/Sparkle.framework");
    // SAFETY: loading runs Sparkle's initialisers; it is the framework the
    // bundle script put there and signed with the app.
    let loaded = NSBundle::bundleWithPath(&NSString::from_str(&path)).is_some_and(|sparkle| unsafe { sparkle.load() });
    let Some(class) = loaded.then(|| AnyClass::get(c"SPUStandardUpdaterController")).flatten() else {
        tracing::warn!(path, "cannot load Sparkle; self-update is off");
        return;
    };

    SETTLED.store(false, Ordering::SeqCst);
    let delegate = UpdaterDelegate::new(mtm);
    // SAFETY: the selectors and argument types are those of Sparkle 2's
    // public headers (SPUStandardUpdaterController.h, SPUUpdater.h).
    let controller = unsafe {
        let allocated: Allocated<AnyObject> = msg_send![class, alloc];
        let controller: Option<Retained<AnyObject>> = msg_send![
            allocated,
            initWithStartingUpdater: true,
            updaterDelegate: &*delegate,
            userDriverDelegate: None::<&AnyObject>
        ];
        controller
    };
    let Some(controller) = controller else {
        SETTLED.store(true, Ordering::SeqCst);
        tracing::warn!("Sparkle did not start; self-update is off");
        return;
    };
    unsafe {
        let updater: Retained<AnyObject> = msg_send![&controller, updater];
        let _: () = msg_send![&updater, setAutomaticallyDownloadsUpdates: true];
        let _: () = msg_send![&updater, checkForUpdatesInBackground];
    }
    tracing::info!("checking for StoneForm updates");
    UPDATER.with(|updater| *updater.borrow_mut() = Some((controller, delegate)));
}

/// Whether StoneForm can quit without cutting a self-update short.
pub fn settled() -> bool {
    SETTLED.load(Ordering::SeqCst)
}

/// The version an appcast item stands for, as shown to people.
fn version_of(item: &AnyObject) -> String {
    // SAFETY: SUAppcastItem has `displayVersionString` (SUAppcastItem.h).
    let version: Option<Retained<NSString>> = unsafe { msg_send![item, displayVersionString] };
    version.map(|version| version.to_string()).unwrap_or_default()
}

define_class!(
    // Sparkle asks its delegate with `respondsToSelector:`, so implementing
    // the methods is all the conformance it needs. It calls them on the main thread.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "StoneFormUpdaterDelegate"]
    struct UpdaterDelegate;

    unsafe impl NSObjectProtocol for UpdaterDelegate {}

    impl UpdaterDelegate {
        #[unsafe(method(updater:didFindValidUpdate:))]
        fn did_find_valid_update(&self, _updater: &AnyObject, item: &AnyObject) {
            tracing::info!(version = version_of(item), "downloading a StoneForm update");
        }

        #[unsafe(method(updaterDidNotFindUpdate:))]
        fn did_not_find_update(&self, _updater: &AnyObject) {
            tracing::debug!("StoneForm is up to date");
        }

        #[unsafe(method(updater:failedToDownloadUpdate:error:))]
        fn failed_to_download(&self, _updater: &AnyObject, item: &AnyObject, error: &NSError) {
            let error = error.localizedDescription().to_string();
            tracing::warn!(version = version_of(item), error, "the StoneForm update did not download");
        }

        /// The update is downloaded and installs when StoneForm quits.
        /// Returning true tells Sparkle we handle the UI, so it shows none.
        #[unsafe(method(updater:willInstallUpdateOnQuit:immediateInstallationBlock:))]
        fn will_install_on_quit(
            &self,
            _updater: &AnyObject,
            item: &AnyObject,
            _install_now: &DynBlock<dyn Fn()>,
        ) -> bool {
            tracing::info!(version = version_of(item), "StoneForm update ready; it installs when StoneForm quits");
            true
        }

        #[unsafe(method(updater:didAbortWithError:))]
        fn did_abort(&self, _updater: &AnyObject, error: &NSError) {
            tracing::warn!(error = %error.localizedDescription(), "the StoneForm update check stopped");
        }

        #[unsafe(method(updater:didFinishUpdateCycleForUpdateCheck:error:))]
        fn did_finish_cycle(&self, _updater: &AnyObject, _check: isize, _error: Option<&NSError>) {
            SETTLED.store(true, Ordering::SeqCst);
        }
    }
);

impl UpdaterDelegate {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(());
        // SAFETY: NSObject's plain initializer.
        unsafe { msg_send![super(this), init] }
    }
}
