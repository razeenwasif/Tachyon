//! System notifications for long commands that finish while you're looking
//! elsewhere. Clicking one brings its window and tab to the front.
//!
//! Notifications need a bundle identity, so they're off when the binary runs
//! outside `Tachyon.app` (as with `cargo run`); the caller falls back to a
//! Dock bounce.

use block2::RcBlock;
use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::{Bool, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{define_class, msg_send, AllocAnyThread, MainThreadMarker};
use objc2_foundation::{NSBundle, NSError, NSString};
use objc2_user_notifications::{
    UNAuthorizationOptions, UNMutableNotificationContent, UNNotification, UNNotificationPresentationOptions,
    UNNotificationRequest, UNNotificationResponse, UNNotificationSound, UNUserNotificationCenter,
    UNUserNotificationCenterDelegate,
};

/// Request identifiers carry the tab they came from: `tachyon.tab.<tab>.<cmd>`.
fn identifier(tab: u64, cmd: u32) -> String {
    format!("tachyon.tab.{tab}.{cmd}")
}

fn tab_of(identifier: &str) -> Option<u64> {
    identifier.strip_prefix("tachyon.tab.")?.split('.').next()?.parse().ok()
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "TachyonNotificationDelegate"]
    struct NotificationDelegate;

    unsafe impl NSObjectProtocol for NotificationDelegate {}

    unsafe impl UNUserNotificationCenterDelegate for NotificationDelegate {
        /// Shown even while Tachyon is the active app, since the command's
        /// window may be behind another one.
        #[unsafe(method(userNotificationCenter:willPresentNotification:withCompletionHandler:))]
        fn will_present(
            &self,
            _center: &UNUserNotificationCenter,
            _notification: &UNNotification,
            handler: &block2::DynBlock<dyn Fn(UNNotificationPresentationOptions)>,
        ) {
            handler.call((UNNotificationPresentationOptions::Banner | UNNotificationPresentationOptions::List,));
        }

        /// A click: bring the command's tab forward.
        #[unsafe(method(userNotificationCenter:didReceiveNotificationResponse:withCompletionHandler:))]
        fn did_receive(
            &self,
            _center: &UNUserNotificationCenter,
            response: &UNNotificationResponse,
            handler: &block2::DynBlock<dyn Fn()>,
        ) {
            let id = response.notification().request().identifier().to_string();
            if let Some(tab) = tab_of(&id) {
                DispatchQueue::main().exec_async(move || super::show_tab(tab));
            }
            handler.call(());
        }
    }
);

impl NotificationDelegate {
    fn new() -> Retained<Self> {
        let this = Self::alloc().set_ivars(());
        unsafe { msg_send![super(this), init] }
    }
}

pub struct Notifier {
    center: Retained<UNUserNotificationCenter>,
    _delegate: Retained<NotificationDelegate>,
}

impl Notifier {
    /// `None` outside an app bundle, where the notification center is not
    /// available.
    pub fn new(_mtm: MainThreadMarker) -> Option<Notifier> {
        NSBundle::mainBundle().bundleIdentifier()?;
        let center = UNUserNotificationCenter::currentNotificationCenter();
        let delegate = NotificationDelegate::new();
        center.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
        Some(Notifier { center, _delegate: delegate })
    }

    /// Ask macOS for permission to notify, if it hasn't been asked: the
    /// session host, which can't ask, posts under the same permission.
    pub fn ask_permission(&self) {
        static ASKED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if ASKED.swap(true, std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        let done = RcBlock::new(|granted: Bool, _error: *mut NSError| {
            crate::log!("notifications: {}", if granted.as_bool() { "allowed" } else { "not allowed" });
        });
        self.center
            .requestAuthorizationWithOptions_completionHandler(UNAuthorizationOptions::Alert | UNAuthorizationOptions::Sound, &done);
    }

    /// Post one notification. The first time, macOS asks whether Tachyon may
    /// notify; the notification follows if you allow it.
    pub fn post(&self, tab: u64, cmd: u32, title: &str, subtitle: &str, body: &str) {
        let content = UNMutableNotificationContent::new();
        content.setTitle(&NSString::from_str(title));
        if !subtitle.is_empty() {
            content.setSubtitle(&NSString::from_str(subtitle));
        }
        content.setBody(&NSString::from_str(body));
        content.setSound(Some(&UNNotificationSound::defaultSound()));
        // Group a tab's notifications together in Notification Centre.
        content.setThreadIdentifier(&NSString::from_str(&format!("tachyon.tab.{tab}")));
        let request = UNNotificationRequest::requestWithIdentifier_content_trigger(
            &NSString::from_str(&identifier(tab, cmd)),
            &content,
            None,
        );

        let center = self.center.clone();
        let done = RcBlock::new(move |granted: Bool, _error: *mut NSError| {
            if granted.as_bool() {
                center.addNotificationRequest_withCompletionHandler(&request, None);
            } else {
                crate::log!("notifications: not allowed");
            }
        });
        self.center
            .requestAuthorizationWithOptions_completionHandler(UNAuthorizationOptions::Alert | UNAuthorizationOptions::Sound, &done);
    }
}

/// Post a notification from the session host, which has no app around it:
/// a cluster job that ended while Tachyon was closed. Clicking it opens
/// Tachyon at the job's shell (`session`, which is its tab's pane id).
pub fn post_detached(session: u64, key: &str, title: &str, subtitle: &str, body: &str) {
    if NSBundle::mainBundle().bundleIdentifier().is_none() {
        crate::log!("notifications: no bundle, so none for {title}");
        return;
    }
    let center = UNUserNotificationCenter::currentNotificationCenter();
    let content = UNMutableNotificationContent::new();
    content.setTitle(&NSString::from_str(title));
    if !subtitle.is_empty() {
        content.setSubtitle(&NSString::from_str(subtitle));
    }
    content.setBody(&NSString::from_str(body));
    content.setSound(Some(&UNNotificationSound::defaultSound()));
    content.setThreadIdentifier(&NSString::from_str(&format!("tachyon.tab.{session}")));
    let request = UNNotificationRequest::requestWithIdentifier_content_trigger(
        &NSString::from_str(&format!("tachyon.tab.{session}.job.{key}")),
        &content,
        None,
    );
    let title = title.to_string();
    let posting = center.clone();
    let done = RcBlock::new(move |granted: Bool, error: *mut NSError| {
        if granted.as_bool() {
            posting.addNotificationRequest_withCompletionHandler(&request, None);
            crate::log!("host: told: {title}");
        } else {
            // SAFETY: the error, if any, lives for the handler's call.
            let why = unsafe { error.as_ref() }.map(|e| e.localizedDescription().to_string()).unwrap_or_default();
            crate::log!("notifications: not allowed ({why})");
        }
    });
    center.requestAuthorizationWithOptions_completionHandler(UNAuthorizationOptions::Alert | UNAuthorizationOptions::Sound, &done);
}

#[cfg(test)]
mod tests {
    use super::{identifier, tab_of};

    #[test]
    fn identifiers_carry_the_tab() {
        assert_eq!(tab_of(&identifier(42, 7)), Some(42));
        assert_eq!(tab_of("something.else"), None);
    }
}
