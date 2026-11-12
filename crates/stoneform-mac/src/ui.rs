//! The window: one status line and one button, plus the standard menus.

use std::cell::RefCell;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject, Sel};
use objc2::{MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSApplicationDelegate, NSBackingStoreType, NSButton, NSFont, NSMenu,
    NSMenuItem, NSTextAlignment, NSTextField, NSView, NSWindow, NSWindowStyleMask,
};
use objc2_foundation::{NSNotification, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString};

use crate::app::{self, Status};

#[derive(Debug)]
struct Ui {
    // Held so the window lives as long as the app.
    _window: Retained<NSWindow>,
    status: Retained<NSTextField>,
    button: Retained<NSButton>,
    shown: Status,
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
            let shown = UI.with(|ui| ui.borrow().as_ref().map(|ui| ui.shown.clone()));
            match shown {
                Some(Status::SignedIn(_)) => app::sign_out(),
                Some(Status::SignedOut | Status::Failed(_)) => app::sign_in(),
                _ => {}
            }
        }
    }
);

impl AppDelegate {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(());
        unsafe { msg_send![super(this), init] }
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
        let (text, button) = match &status {
            Status::Checking => ("Checking sign-in…".to_owned(), None),
            Status::SigningIn => ("Waiting for you to sign in to Battle.net…".to_owned(), None),
            Status::SignedIn(signed_in) => (
                format!("Signed in · account {} · {}", signed_in.account, signed_in.region.launch_option_value()),
                Some("Sign Out"),
            ),
            Status::SignedOut => ("Not signed in.".to_owned(), Some("Sign In")),
            Status::Failed(message) => (format!("Something went wrong: {message}"), Some("Try Again")),
        };
        ui.status.setStringValue(&NSString::from_str(&text));
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

fn build_window(mtm: MainThreadMarker, delegate: &AppDelegate) {
    let (width, height) = (420.0, 170.0);
    let frame = NSRect::new(NSPoint::ZERO, NSSize::new(width, height));
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            frame,
            NSWindowStyleMask::Titled | NSWindowStyleMask::Closable | NSWindowStyleMask::Miniaturizable,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    unsafe { window.setReleasedWhenClosed(false) };
    window.setTitle(&NSString::from_str("StoneForm"));

    let content = NSView::initWithFrame(NSView::alloc(mtm), frame);
    let heading = NSTextField::labelWithString(&NSString::from_str("WoW: Forever"), mtm);
    heading.setFont(Some(&NSFont::boldSystemFontOfSize(17.0)));
    heading.setAlignment(NSTextAlignment::Center);
    heading.setFrame(NSRect::new(NSPoint::new(20.0, 116.0), NSSize::new(width - 40.0, 24.0)));

    let status = NSTextField::wrappingLabelWithString(&NSString::new(), mtm);
    status.setAlignment(NSTextAlignment::Center);
    status.setFrame(NSRect::new(NSPoint::new(20.0, 66.0), NSSize::new(width - 40.0, 40.0)));

    let button = unsafe {
        NSButton::buttonWithTitle_target_action(
            &NSString::from_str("Sign In"),
            Some(delegate),
            Some(sel!(primaryAction:)),
            mtm,
        )
    };
    button.setFrame(NSRect::new(NSPoint::new((width - 140.0) / 2.0, 20.0), NSSize::new(140.0, 32.0)));
    button.setHidden(true);

    content.addSubview(&heading);
    content.addSubview(&status);
    content.addSubview(&button);
    window.setContentView(Some(&content));
    window.center();
    window.makeKeyAndOrderFront(None);
    NSApplication::sharedApplication(mtm).activate();

    UI.with(|ui| {
        *ui.borrow_mut() = Some(Ui { _window: window, status, button, shown: Status::Checking });
    });
    show(mtm, Status::Checking);
}

enum Entry {
    Item(&'static str, Sel, &'static str),
    Separator,
}

/// The app, Edit and Window menus. Edit matters: without it, Cmd-C/V/A do
/// nothing in the sign-in page's fields, so password managers can't paste.
fn main_menu(mtm: MainThreadMarker) -> Retained<NSMenu> {
    use Entry::{Item, Separator};
    let bar = NSMenu::new(mtm);
    let menus = [
        ("StoneForm", vec![Item("Quit StoneForm", sel!(terminate:), "q")]),
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
