//! The window: a heading, one status line, the install form or a progress
//! bar, and one button whose meaning follows the status. Plus the menus.

use std::cell::RefCell;
use std::path::{Path, PathBuf};

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject, Sel};
use objc2::{MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSApplicationDelegate, NSBackingStoreType, NSButton, NSFont,
    NSLineBreakMode, NSMenu, NSMenuItem, NSModalResponseOK, NSOpenPanel, NSPopUpButton, NSProgressIndicator,
    NSProgressIndicatorStyle, NSTextAlignment, NSTextField, NSView, NSWindow, NSWindowStyleMask,
};
use objc2_foundation::{NSNotification, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString, NSURL};
use stoneform_domain::Locale;

use stoneform_app::InstallStage;

use crate::app::{self, Download, InstallChoice, Status};
use crate::volume::{available_bytes, format_bytes};

const WIDTH: f64 = 480.0;
const HEIGHT: f64 = 240.0;
const MARGIN: f64 = 24.0;

#[derive(Debug)]
struct Ui {
    // Held so the window lives as long as the app.
    _window: Retained<NSWindow>,
    status: Retained<NSTextField>,
    form: Retained<NSView>,
    folder_label: Retained<NSTextField>,
    language: Retained<NSPopUpButton>,
    progress: Retained<NSProgressIndicator>,
    button: Retained<NSButton>,
    shown: Status,
    /// The install form's choice, which the user may change before Install.
    folder: PathBuf,
    locales: Vec<Locale>,
}

thread_local! {
    static UI: RefCell<Option<Ui>> = const { RefCell::new(None) };
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "StoneFormAppDelegate"]
    struct AppDelegate;

    unsafe impl NSObjectProtocol for AppDelegate {}

    unsafe impl NSApplicationDelegate for AppDelegate {
        #[unsafe(method(applicationDidFinishLaunching:))]
        fn did_finish_launching(&self, _notification: &NSNotification) {
            tracing::debug!("launched; opening the window");
            build_window(self.mtm(), self);
            app::start();
        }

        #[unsafe(method(applicationShouldTerminateAfterLastWindowClosed:))]
        fn should_terminate_after_last_window_closed(&self, _sender: &NSApplication) -> bool {
            true
        }
    }

    impl AppDelegate {
        #[unsafe(method(primaryAction:))]
        fn primary_action(&self, _sender: Option<&AnyObject>) {
            let Some((shown, choice)) = UI.with(|ui| ui.borrow().as_ref().map(|ui| (ui.shown.clone(), ui.choice())))
            else {
                return;
            };
            match shown {
                Status::SignedOut => app::sign_in(),
                Status::Failed(_) => app::start(),
                Status::ChooseInstall(_) => {
                    if let Some((folder, locale)) = choice {
                        app::start_install(folder, locale);
                    }
                }
                Status::Installing(_) => app::pause_install(),
                Status::Paused(_) => app::resume_install(),
                Status::Checking | Status::SigningIn | Status::WaitingForBattleNet | Status::Launching => {}
            }
        }

        #[unsafe(method(chooseFolder:))]
        fn choose_folder(&self, _sender: Option<&AnyObject>) {
            let current = UI.with(|ui| ui.borrow().as_ref().map(|ui| ui.folder.clone()));
            let Some(chosen) = ask_for_folder(self.mtm(), current.as_deref()) else { return };
            let folder = install_root(&chosen);
            UI.with(|ui| {
                if let Some(ui) = ui.borrow_mut().as_mut() {
                    ui.set_folder(folder);
                }
            });
        }

        /// For when the game rejects the saved sign-in, which StoneForm
        /// cannot see from outside: a fresh token for the next launch.
        #[unsafe(method(signInAgain:))]
        fn sign_in_again(&self, _sender: Option<&AnyObject>) {
            app::sign_in();
        }

        #[unsafe(method(signOut:))]
        fn sign_out(&self, _sender: Option<&AnyObject>) {
            app::sign_out();
        }
    }
);

impl AppDelegate {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(());
        unsafe { msg_send![super(this), init] }
    }
}

impl Ui {
    /// The folder and language selected in the install form.
    fn choice(&self) -> Option<(PathBuf, Locale)> {
        let index = usize::try_from(self.language.indexOfSelectedItem()).ok()?;
        Some((self.folder.clone(), self.locales.get(index)?.clone()))
    }

    fn set_folder(&mut self, folder: PathBuf) {
        self.folder_label.setStringValue(&NSString::from_str(&folder.display().to_string()));
        self.status.setStringValue(&NSString::from_str(&install_hint(&folder)));
        self.folder = folder;
    }

    fn set_choice(&mut self, choice: &InstallChoice) {
        self.language.removeAllItems();
        for locale in &choice.locales {
            self.language.addItemWithTitle(&NSString::from_str(locale.display_name()));
        }
        let selected = choice.locales.iter().position(|locale| *locale == choice.locale).unwrap_or(0);
        self.language.selectItemAtIndex(selected as isize);
        self.locales = choice.locales.clone();
        self.set_folder(choice.folder.clone());
    }

    fn set_progress(&self, progress: Option<Download>) {
        match progress {
            Some(progress) => {
                let unknown = progress.total == 0;
                self.progress.setIndeterminate(unknown);
                if unknown {
                    unsafe { self.progress.startAnimation(None) };
                } else {
                    unsafe { self.progress.stopAnimation(None) };
                    self.progress.setMaxValue(progress.total as f64);
                    self.progress.setDoubleValue(progress.done as f64);
                }
                self.progress.setHidden(false);
            }
            None => self.progress.setHidden(true),
        }
    }
}

/// Runs the app until it quits.
pub fn run(mtm: MainThreadMarker) {
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    let delegate = AppDelegate::new(mtm);
    app.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
    app.setMainMenu(Some(&main_menu(mtm)));
    app.run();
}

/// Shows `status` in the window.
pub fn show(_mtm: MainThreadMarker, status: Status) {
    UI.with(|ui| {
        let mut ui = ui.borrow_mut();
        let Some(ui) = ui.as_mut() else { return };
        let mut form = false;
        let mut progress = None;
        let (text, button) = match &status {
            Status::Checking => ("Checking…".to_owned(), None),
            Status::SigningIn => ("Waiting for you to sign in to Battle.net…".to_owned(), None),
            Status::SignedOut => ("Not signed in.".to_owned(), Some("Sign In")),
            Status::ChooseInstall(choice) => {
                ui.set_choice(choice);
                form = true;
                let text = match &choice.error {
                    Some(error) => format!("Could not install: {error}"),
                    None => install_hint(&choice.folder),
                };
                (text, Some("Install"))
            }
            Status::Installing(download) => {
                progress = Some(*download);
                (download_text(download), Some("Pause"))
            }
            Status::Paused(download) => {
                progress = Some(*download);
                (
                    format!("Paused · {} of {}", format_bytes(download.done), format_bytes(download.total)),
                    Some("Resume"),
                )
            }
            Status::WaitingForBattleNet => {
                ("Quit Battle.net to continue. StoneForm carries on when it has closed.".to_owned(), None)
            }
            Status::Launching => ("Starting WoW: Forever…".to_owned(), None),
            Status::Failed(message) => (format!("Something went wrong: {message}"), Some("Try Again")),
        };
        ui.status.setStringValue(&NSString::from_str(&text));
        ui.form.setHidden(!form);
        ui.set_progress(progress);
        match button {
            Some(title) => {
                ui.button.setTitle(&NSString::from_str(title));
                ui.button.setHidden(false);
            }
            None => ui.button.setHidden(true),
        }
        ui.shown = status;
    });
}

fn download_text(download: &Download) -> String {
    let verb = if download.update { "Updating" } else { "Downloading" };
    match download.stage {
        InstallStage::Preparing if download.update => "Preparing the update…".to_owned(),
        InstallStage::Preparing => "Preparing the download…".to_owned(),
        InstallStage::Finishing => "Finishing…".to_owned(),
        InstallStage::Downloading => {
            let mut text = format!("{verb} · {} of {}", format_bytes(download.done), format_bytes(download.total));
            if let Some(left) = download.seconds_left {
                text.push_str(" · ");
                text.push_str(&time_left(left));
            }
            text
        }
    }
}

fn time_left(seconds: u64) -> String {
    match seconds / 60 {
        0 => "less than a minute left".to_owned(),
        minutes @ 1..60 => format!("{minutes} min left"),
        minutes => format!("{} h {} min left", minutes / 60, minutes % 60),
    }
}

fn install_hint(folder: &Path) -> String {
    match available_bytes(folder) {
        Some(free) => format!("Choose where to install WoW: Forever. {} free on that disk.", format_bytes(free)),
        None => "Choose where to install WoW: Forever.".to_owned(),
    }
}

/// The `World of Warcraft` folder for what the user picked: the folder itself
/// if it is one, else a new one inside it.
fn install_root(chosen: &Path) -> PathBuf {
    if chosen.file_name().is_some_and(|name| name == "World of Warcraft") {
        chosen.to_owned()
    } else {
        chosen.join("World of Warcraft")
    }
}

fn ask_for_folder(mtm: MainThreadMarker, current: Option<&Path>) -> Option<PathBuf> {
    let panel = NSOpenPanel::openPanel(mtm);
    panel.setCanChooseDirectories(true);
    panel.setCanChooseFiles(false);
    panel.setCanCreateDirectories(true);
    panel.setPrompt(Some(&NSString::from_str("Choose")));
    panel.setMessage(Some(&NSString::from_str(
        "Choose where to install WoW: Forever. StoneForm puts it in a \"World of Warcraft\" folder there.",
    )));
    if let Some(parent) = current.and_then(Path::parent).and_then(Path::to_str) {
        panel.setDirectoryURL(Some(&NSURL::fileURLWithPath(&NSString::from_str(parent))));
    }
    if panel.runModal() != NSModalResponseOK {
        return None;
    }
    panel.URL()?.path().map(|path| PathBuf::from(path.to_string()))
}

fn frame(x: f64, y: f64, width: f64, height: f64) -> NSRect {
    NSRect::new(NSPoint::new(x, y), NSSize::new(width, height))
}

fn label(mtm: MainThreadMarker, text: &str, frame: NSRect) -> Retained<NSTextField> {
    let label = NSTextField::labelWithString(&NSString::from_str(text), mtm);
    label.setFrame(frame);
    label
}

fn build_window(mtm: MainThreadMarker, delegate: &AppDelegate) {
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            frame(0.0, 0.0, WIDTH, HEIGHT),
            NSWindowStyleMask::Titled | NSWindowStyleMask::Closable | NSWindowStyleMask::Miniaturizable,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    unsafe { window.setReleasedWhenClosed(false) };
    window.setTitle(&NSString::from_str("StoneForm"));
    let content = NSView::initWithFrame(NSView::alloc(mtm), frame(0.0, 0.0, WIDTH, HEIGHT));
    let inner = WIDTH - 2.0 * MARGIN;

    let heading = label(mtm, "WoW: Forever", frame(MARGIN, 196.0, inner, 24.0));
    heading.setFont(Some(&NSFont::boldSystemFontOfSize(17.0)));
    heading.setAlignment(NSTextAlignment::Center);

    let status = NSTextField::wrappingLabelWithString(&NSString::new(), mtm);
    status.setAlignment(NSTextAlignment::Center);
    status.setFrame(frame(MARGIN, 146.0, inner, 40.0));

    // The install form: folder and language.
    let form = NSView::initWithFrame(NSView::alloc(mtm), frame(MARGIN, 66.0, inner, 72.0));
    let folder_label = label(mtm, "", frame(96.0, 44.0, 236.0, 20.0));
    folder_label.setLineBreakMode(NSLineBreakMode::ByTruncatingMiddle);
    let change = unsafe {
        NSButton::buttonWithTitle_target_action(
            &NSString::from_str("Change…"),
            Some(delegate),
            Some(sel!(chooseFolder:)),
            mtm,
        )
    };
    change.setFrame(frame(inner - 96.0, 38.0, 96.0, 30.0));
    let language =
        NSPopUpButton::initWithFrame_pullsDown(NSPopUpButton::alloc(mtm), frame(92.0, 2.0, 200.0, 28.0), false);
    form.addSubview(&label(mtm, "Install in", frame(0.0, 44.0, 90.0, 20.0)));
    form.addSubview(&folder_label);
    form.addSubview(&change);
    form.addSubview(&label(mtm, "Language", frame(0.0, 8.0, 90.0, 20.0)));
    form.addSubview(&language);
    form.setHidden(true);

    let progress =
        NSProgressIndicator::initWithFrame(NSProgressIndicator::alloc(mtm), frame(MARGIN, 110.0, inner, 20.0));
    progress.setStyle(NSProgressIndicatorStyle::Bar);
    progress.setHidden(true);

    let button = unsafe {
        NSButton::buttonWithTitle_target_action(
            &NSString::from_str("Sign In"),
            Some(delegate),
            Some(sel!(primaryAction:)),
            mtm,
        )
    };
    button.setFrame(frame((WIDTH - 160.0) / 2.0, 20.0, 160.0, 32.0));
    button.setHidden(true);

    for view in [&*heading, &*status, &*form, &*progress, &*button] {
        content.addSubview(view);
    }
    window.setContentView(Some(&content));
    window.center();
    window.makeKeyAndOrderFront(None);
    NSApplication::sharedApplication(mtm).activate();

    UI.with(|ui| {
        *ui.borrow_mut() = Some(Ui {
            _window: window,
            status,
            form,
            folder_label,
            language,
            progress,
            button,
            shown: Status::Checking,
            folder: PathBuf::new(),
            locales: Vec::new(),
        });
    });
    show(mtm, Status::Checking);
}

enum Entry {
    Item(&'static str, Sel, &'static str),
    Separator,
}

/// The app, Edit and Window menus. Edit matters: without it, Cmd-C/V/A do
/// nothing in the sign-in page's fields, so password managers can't paste.
/// Items without a target go up the responder chain, which ends at the app
/// delegate.
fn main_menu(mtm: MainThreadMarker) -> Retained<NSMenu> {
    use Entry::{Item, Separator};
    let bar = NSMenu::new(mtm);
    let menus = [
        (
            "StoneForm",
            vec![Item("Sign Out", sel!(signOut:), ""), Separator, Item("Quit StoneForm", sel!(terminate:), "q")],
        ),
        (
            "Edit",
            vec![
                Item("Undo", sel!(undo:), "z"),
                // An upper-case key equivalent means Shift.
                Item("Redo", sel!(redo:), "Z"),
                Separator,
                Item("Cut", sel!(cut:), "x"),
                Item("Copy", sel!(copy:), "c"),
                Item("Paste", sel!(paste:), "v"),
                Item("Select All", sel!(selectAll:), "a"),
            ],
        ),
        ("Window", vec![Item("Minimize", sel!(performMiniaturize:), "m"), Item("Close", sel!(performClose:), "w")]),
    ];
    for (title, entries) in menus {
        let menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str(title));
        for entry in entries {
            let item = match entry {
                Item(title, action, key) => unsafe {
                    NSMenuItem::initWithTitle_action_keyEquivalent(
                        NSMenuItem::alloc(mtm),
                        &NSString::from_str(title),
                        Some(action),
                        &NSString::from_str(key),
                    )
                },
                Separator => NSMenuItem::separatorItem(mtm),
            };
            menu.addItem(&item);
        }
        let holder = NSMenuItem::new(mtm);
        holder.setSubmenu(Some(&menu));
        bar.addItem(&holder);
    }
    bar
}
