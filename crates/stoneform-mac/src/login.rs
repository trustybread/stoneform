//! `LoginFlow` adapter: Blizzard's web login in a WebKit window.
//!
//! Ported from Waypoint's `WebLogin.swift` (0BSD): the token is caught in three
//! navigation callbacks, the page gets a Safari-like user agent, the dark
//! backdrop Blizzard's launcher pages expect, and no passkey buttons.

use std::cell::{Cell, RefCell};

use block2::{DynBlock, RcBlock};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSApplication, NSAutoresizingMaskOptions, NSBackingStoreType, NSColor, NSTextField, NSView, NSWindow,
    NSWindowDelegate, NSWindowStyleMask,
};
use objc2_foundation::{
    NSError, NSNotification, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString, NSURL,
    NSURLErrorFailingURLErrorKey, NSURLRequest, NSUUID,
};
use objc2_web_kit::{
    WKNavigation, WKNavigationAction, WKNavigationActionPolicy, WKNavigationDelegate, WKUserScript,
    WKUserScriptInjectionTime, WKWebView, WKWebViewConfiguration, WKWebsiteDataStore,
};
use stoneform_app::ports::{LoginError, LoginFlow};
use stoneform_domain::{AccountId, LaunchToken, Region, WOW_FOREVER, login_url, token_from_callback};
use tokio::sync::oneshot;

use crate::main_thread::on_main;

/// The persistent WebKit data store (cookie jar) the sign-in uses. v1 has one
/// account, so one fixed store; signing out deletes it.
const WEB_SESSION: &str = "5F0A4E2C-8B31-4D7E-9A62-1CD437E8B590";

/// [`WEB_SESSION`] as an `NSUUID`. Built from the string: objc2-foundation
/// 0.3.2's `NSUUID::from_bytes` fails objc2's message-send check on macOS 27.
fn web_session_id() -> Retained<NSUUID> {
    NSUUID::initWithUUIDString(NSUUID::alloc(), &NSString::from_str(WEB_SESSION))
        .expect("WEB_SESSION is a valid UUID string")
}

/// Without a browser token in the user agent, login and CAPTCHA pages may
/// treat WebKit as unsupported.
const USER_AGENT_SUFFIX: &str = "Version/26.0 Safari/605.1.15";

const PASSKEY_NOTICE: &str =
    "Passkeys don't work here. If Battle.net asks for one, sign in with your password instead.";

/// `NSURLErrorCancelled`: a load we stopped ourselves.
const URL_ERROR_CANCELLED: isize = -999;

/// Blizzard's login pages are drawn for the launcher's dark window: a
/// transparent page with light text. On WebKit's white they vanish, so such a
/// page gets the launcher's dark backdrop. Pages with their own background or
/// dark text are left alone.
const BACKDROP_SCRIPT: &str = r#"
(function () {
  function clear(color) { return color === 'transparent' || /rgba\(\d+, \d+, \d+, 0\)/.test(color); }
  function fix() {
    var html = document.documentElement, body = document.body;
    if (!body || html.dataset.stoneformBackdrop) return;
    if (!clear(getComputedStyle(html).backgroundColor) || !clear(getComputedStyle(body).backgroundColor)) return;
    var rgb = (getComputedStyle(body).color.match(/\d+(\.\d+)?/g) || []).map(Number);
    if (rgb.length < 3 || (0.299 * rgb[0] + 0.587 * rgb[1] + 0.114 * rgb[2]) / 255 < 0.6) return;
    html.dataset.stoneformBackdrop = '1';
    html.style.backgroundColor = '#0d0f14';
  }
  document.addEventListener('DOMContentLoaded', fix);
  window.addEventListener('load', fix);
})();
"#;

/// WebKit lets only browsers and the site's own apps use passkeys, so the
/// buttons that start a passkey sign-in can only fail here. Only those are
/// hidden: a "can't use your passkey?" link may be the way back to the
/// password. The page swaps its steps in place, so this keeps watching.
const HIDE_PASSKEY_SCRIPT: &str = r#"
(function () {
  if (window.__stoneformNoPasskey) return;
  window.__stoneformNoPasskey = true;
  function hide() {
    document.querySelectorAll('button, a, [role="button"], input[type="submit"], input[type="button"]').forEach(function (e) {
      var text = (e.textContent || e.value || '').replace(/\s+/g, ' ').trim();
      if (/^(use|sign in with|log in with|continue with)( a| your)? passkeys?$/i.test(text))
        e.style.setProperty('display', 'none', 'important');
    });
  }
  hide();
  new MutationObserver(hide).observe(document.documentElement, { childList: true, subtree: true });
})();
"#;

type TokenReply = oneshot::Sender<Result<LaunchToken, LoginError>>;

/// The `LoginFlow` port's adapter. Callable from any thread; the window itself
/// lives on the main thread.
#[derive(Debug, Default)]
pub struct WebKitLoginFlow;

impl LoginFlow for WebKitLoginFlow {
    async fn acquire(&self, region: Region) -> Result<LaunchToken, LoginError> {
        let url = login_url(region, WOW_FOREVER.codename).to_string();
        let (reply, answer) = oneshot::channel();
        on_main(move |mtm| LoginWindow::present(mtm, &url, reply));
        answer.await.unwrap_or(Err(LoginError::Cancelled))
    }

    async fn forget(&self, _account: &AccountId) -> Result<(), LoginError> {
        let (reply, answer) = oneshot::channel();
        on_main(move |mtm| delete_web_session(mtm, reply));
        answer.await.unwrap_or_else(|_| Err(LoginError::Failed("deleting the web session was interrupted".into())))
    }
}

thread_local! {
    /// The open sign-in window. WebKit and AppKit hold their delegates weakly,
    /// so this is what keeps it alive.
    static CURRENT: RefCell<Option<Retained<LoginWindow>>> = const { RefCell::new(None) };
    static NEXT_GENERATION: Cell<u64> = const { Cell::new(0) };
}

#[derive(Debug)]
struct LoginState {
    reply: Option<TokenReply>,
    window: Option<Retained<NSWindow>>,
    web_view: Option<Retained<WKWebView>>,
}

#[derive(Debug)]
struct LoginIvars {
    /// Tells this window apart from a later one when its deferred clean-up runs.
    generation: u64,
    state: RefCell<LoginState>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "StoneFormLoginWindow"]
    #[ivars = LoginIvars]
    struct LoginWindow;

    unsafe impl NSObjectProtocol for LoginWindow {}

    unsafe impl WKNavigationDelegate for LoginWindow {
        #[unsafe(method(webView:decidePolicyForNavigationAction:decisionHandler:))]
        fn decide_policy_for_navigation_action(
            &self,
            _web_view: &WKWebView,
            action: &WKNavigationAction,
            decision_handler: &DynBlock<dyn Fn(WKNavigationActionPolicy)>,
        ) {
            let url = unsafe { action.request() }.URL();
            let caught = self.catch_token(url.as_deref());
            let policy = if caught || self.is_done() {
                WKNavigationActionPolicy::Cancel
            } else {
                WKNavigationActionPolicy::Allow
            };
            decision_handler.call((policy,));
        }

        #[unsafe(method(webView:didReceiveServerRedirectForProvisionalNavigation:))]
        fn did_receive_server_redirect(&self, web_view: &WKWebView, _navigation: Option<&WKNavigation>) {
            if self.catch_token(unsafe { web_view.URL() }.as_deref()) {
                unsafe { web_view.stopLoading() };
            }
        }

        #[unsafe(method(webView:didFinishNavigation:))]
        fn did_finish_navigation(&self, web_view: &WKWebView, _navigation: Option<&WKNavigation>) {
            // Host and path only: a query may carry secrets.
            let url = unsafe { web_view.URL() };
            let host = url.as_deref().and_then(|url| url.host()).map(|host| host.to_string()).unwrap_or_default();
            let path = url.as_deref().and_then(|url| url.path()).map(|path| path.to_string()).unwrap_or_default();
            tracing::debug!(%host, %path, "sign-in page loaded");
        }

        #[unsafe(method(webView:didFailProvisionalNavigation:withError:))]
        fn did_fail_provisional_navigation(
            &self,
            _web_view: &WKWebView,
            _navigation: Option<&WKNavigation>,
            error: &NSError,
        ) {
            // A redirect to localhost:0 can fail before it shows up as a
            // navigation action; the failing URL still carries the token.
            let failing_url = error
                .userInfo()
                .objectForKey(unsafe { NSURLErrorFailingURLErrorKey })
                .and_then(|url| url.downcast::<NSURL>().ok());
            if !self.catch_token(failing_url.as_deref()) {
                self.page_failed(error);
            }
        }

        #[unsafe(method(webView:didFailNavigation:withError:))]
        fn did_fail_navigation(&self, _web_view: &WKWebView, _navigation: Option<&WKNavigation>, error: &NSError) {
            self.page_failed(error);
        }
    }

    unsafe impl NSWindowDelegate for LoginWindow {
        #[unsafe(method(windowWillClose:))]
        fn window_will_close(&self, _notification: &NSNotification) {
            self.finish(Err(LoginError::Cancelled), false);
        }
    }
);

impl LoginWindow {
    /// Opens the sign-in window on `url`; `reply` gets the token, or
    /// `Cancelled` when the window is closed. A newer sign-in replaces an open one.
    fn present(mtm: MainThreadMarker, url: &str, reply: TokenReply) {
        tracing::debug!(url, "opening the sign-in window");
        if let Some(open) = CURRENT.with(|current| current.borrow().clone()) {
            open.finish(Err(LoginError::Cancelled), true);
        }
        let generation = NEXT_GENERATION.with(|next| {
            next.set(next.get() + 1);
            next.get()
        });
        let this = Self::alloc(mtm).set_ivars(LoginIvars {
            generation,
            state: RefCell::new(LoginState { reply: Some(reply), window: None, web_view: None }),
        });
        let this: Retained<Self> = unsafe { msg_send![super(this), init] };

        let web_view = make_web_view(mtm, &this);
        let window = make_window(mtm, &web_view);
        window.setDelegate(Some(ProtocolObject::from_ref(&*this)));
        {
            let mut state = this.ivars().state.borrow_mut();
            state.window = Some(window.clone());
            state.web_view = Some(web_view.clone());
        }
        CURRENT.with(|current| *current.borrow_mut() = Some(this));

        match NSURL::URLWithString(&NSString::from_str(url)) {
            Some(url) => {
                unsafe { web_view.loadRequest(&NSURLRequest::requestWithURL(&url)) };
            }
            None => tracing::error!(url, "the login URL is not a valid NSURL"),
        }
        NSApplication::sharedApplication(mtm).activate();
        window.makeKeyAndOrderFront(None);
    }

    fn is_done(&self) -> bool {
        self.ivars().state.borrow().reply.is_none()
    }

    /// Finishes the sign-in if `url` is the login page's final redirect.
    fn catch_token(&self, url: Option<&NSURL>) -> bool {
        if self.is_done() {
            return false;
        }
        let Some(text) = url.and_then(|url| url.absoluteString()) else { return false };
        let Some(token) = token_from_callback(&text.to_string()) else { return false };
        tracing::info!(account = %token.account_id(), "the sign-in page handed out a token");
        self.finish(Ok(token), true);
        true
    }

    /// Shows a load failure in the window; the user closes it to try again.
    fn page_failed(&self, error: &NSError) {
        if self.is_done() || error.code() == URL_ERROR_CANCELLED {
            return;
        }
        let description = error.localizedDescription().to_string();
        tracing::warn!(%description, "the sign-in page failed to load");
        let web_view = self.ivars().state.borrow().web_view.clone();
        if let Some(web_view) = web_view {
            let html = format!(
                "<html><body style=\"font: 14px -apple-system; color: #ddd; background: #0d0f14; padding: 24px\">\
                 <p>Battle.net's sign-in page did not load: {}</p>\
                 <p>Check your connection, then close this window to try again.</p></body></html>",
                escape_html(&description)
            );
            unsafe { web_view.loadHTMLString_baseURL(&NSString::from_str(&html), None) };
        }
    }

    /// Answers the waiting sign-in once. `close_window` is false when the
    /// window is already closing.
    fn finish(&self, result: Result<LaunchToken, LoginError>, close_window: bool) {
        let (reply, window) = {
            let mut state = self.ivars().state.borrow_mut();
            (state.reply.take(), state.window.clone())
        };
        let Some(reply) = reply else { return };
        let _ = reply.send(result);
        if let Some(window) = window {
            window.setDelegate(None);
            if close_window {
                window.close();
            }
        }
        // Release this object, its window and web view on a later turn of the
        // run loop, never inside one of its own callbacks.
        let generation = self.ivars().generation;
        on_main(move |_| {
            CURRENT.with(|current| {
                let mut current = current.borrow_mut();
                if current.as_ref().is_some_and(|open| open.ivars().generation == generation) {
                    current.take();
                }
            });
        });
    }
}

fn make_web_view(mtm: MainThreadMarker, delegate: &LoginWindow) -> Retained<WKWebView> {
    unsafe {
        let config = WKWebViewConfiguration::new(mtm);
        let store = WKWebsiteDataStore::dataStoreForIdentifier(&web_session_id(), mtm);
        config.setWebsiteDataStore(&store);
        config.setApplicationNameForUserAgent(Some(&NSString::from_str(USER_AGENT_SUFFIX)));
        let scripts = config.userContentController();
        for (source, time) in [
            (BACKDROP_SCRIPT, WKUserScriptInjectionTime::AtDocumentStart),
            (HIDE_PASSKEY_SCRIPT, WKUserScriptInjectionTime::AtDocumentEnd),
        ] {
            let script = WKUserScript::initWithSource_injectionTime_forMainFrameOnly(
                WKUserScript::alloc(mtm),
                &NSString::from_str(source),
                time,
                true,
            );
            scripts.addUserScript(&script);
        }
        let web_view = WKWebView::initWithFrame_configuration(WKWebView::alloc(mtm), NSRect::ZERO, &config);
        web_view.setNavigationDelegate(Some(ProtocolObject::from_ref(delegate)));
        web_view.setUnderPageBackgroundColor(Some(&backdrop_color()));
        web_view
    }
}

fn make_window(mtm: MainThreadMarker, web_view: &WKWebView) -> Retained<NSWindow> {
    let (width, height, notice_height) = (480.0, 640.0, 36.0);
    let frame = NSRect::new(NSPoint::ZERO, NSSize::new(width, height));
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            frame,
            NSWindowStyleMask::Titled
                | NSWindowStyleMask::Closable
                | NSWindowStyleMask::Miniaturizable
                | NSWindowStyleMask::Resizable,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    unsafe { window.setReleasedWhenClosed(false) };
    window.setTitle(&NSString::from_str("Sign in to Battle.net"));
    window.setContentMinSize(NSSize::new(400.0, 480.0));

    let container = NSView::initWithFrame(NSView::alloc(mtm), frame);
    let notice = NSTextField::wrappingLabelWithString(&NSString::from_str(PASSKEY_NOTICE), mtm);
    notice.setTextColor(Some(&NSColor::secondaryLabelColor()));
    notice.setFrame(NSRect::new(
        NSPoint::new(16.0, height - 12.0 - notice_height),
        NSSize::new(width - 32.0, notice_height),
    ));
    notice.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewMinYMargin);
    web_view.setFrame(NSRect::new(NSPoint::ZERO, NSSize::new(width, height - notice_height - 24.0)));
    web_view.setAutoresizingMask(
        NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable,
    );
    container.addSubview(&notice);
    container.addSubview(web_view);
    window.setContentView(Some(&container));
    window.center();
    window
}

/// The dark backdrop of Blizzard's launcher, behind and around the page.
fn backdrop_color() -> Retained<NSColor> {
    NSColor::colorWithSRGBRed_green_blue_alpha(13.0 / 255.0, 15.0 / 255.0, 20.0 / 255.0, 1.0)
}

/// Deletes the sign-in's cookie jar. If WebKit refuses (a web view still holds
/// the store), the store is emptied instead.
fn delete_web_session(mtm: MainThreadMarker, reply: oneshot::Sender<Result<(), LoginError>>) {
    // Getting a store starts WebKit; Waypoint saw store calls crash before that
    // on macOS 27.
    let _ = unsafe { WKWebsiteDataStore::defaultDataStore(mtm) };
    let reply = Cell::new(Some(reply));
    let removed = RcBlock::new(move |error: *mut NSError| {
        let Some(reply) = reply.take() else { return };
        // SAFETY: WebKit passes either null or a valid error.
        match unsafe { error.as_ref() } {
            None => {
                let _ = reply.send(Ok(()));
            }
            Some(error) => {
                tracing::warn!(error = %error.localizedDescription(), "could not delete the web session; emptying it");
                empty_web_session(reply);
            }
        }
    });
    unsafe {
        WKWebsiteDataStore::removeDataStoreForIdentifier_completionHandler(&web_session_id(), &removed, mtm);
    }
}

fn empty_web_session(reply: oneshot::Sender<Result<(), LoginError>>) {
    let Some(mtm) = MainThreadMarker::new() else {
        let _ = reply.send(Err(LoginError::Failed("WebKit answered off the main thread".into())));
        return;
    };
    let reply = Cell::new(Some(reply));
    let emptied = RcBlock::new(move || {
        if let Some(reply) = reply.take() {
            let _ = reply.send(Ok(()));
        }
    });
    unsafe {
        let store = WKWebsiteDataStore::dataStoreForIdentifier(&web_session_id(), mtm);
        store.removeDataOfTypes_modifiedSince_completionHandler(
            &WKWebsiteDataStore::allWebsiteDataTypes(mtm),
            &objc2_foundation::NSDate::distantPast(),
            &emptied,
        );
    }
}

fn escape_html(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}
