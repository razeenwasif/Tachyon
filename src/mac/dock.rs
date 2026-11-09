//! A progress bar on the Dock icon while a command shows a tqdm-style bar,
//! as Safari shows a download's. With several, the one updated last wins.
//! It goes away when the command ends.

use std::cell::{Cell, RefCell};
use std::time::Instant;

use objc2::rc::Retained;
use objc2::{define_class, msg_send, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{NSApplication, NSBezierPath, NSColor, NSResponder, NSView};
use objc2_foundation::{NSObject, NSPoint, NSRect, NSSize};
use rustc_hash::FxHashMap;

#[derive(Default)]
pub struct DockState {
    fraction: Cell<f64>,
}

define_class!(
    /// The Dock tile's content: the app icon with a bar along its bottom.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "TachyonDockProgress"]
    #[ivars = DockState]
    struct DockProgress;

    impl DockProgress {
        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            let b = self.bounds();
            if let Some(mtm) = MainThreadMarker::new() {
                if let Some(icon) = NSApplication::sharedApplication(mtm).applicationIconImage() {
                    icon.drawInRect(b);
                }
            }
            let (w, h) = (b.size.width, b.size.height);
            let track = NSRect::new(NSPoint::new(w * 0.14, h * 0.1), NSSize::new(w * 0.72, h * 0.09));
            let r = track.size.height / 2.0;
            NSColor::blackColor().colorWithAlphaComponent(0.55).setFill();
            NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(track, r, r).fill();
            let f = self.ivars().fraction.get().clamp(0.0, 1.0);
            if f > 0.0 {
                let inner = NSRect::new(
                    NSPoint::new(track.origin.x + 2.0, track.origin.y + 2.0),
                    NSSize::new(((track.size.width - 4.0) * f).max(track.size.height - 4.0), track.size.height - 4.0),
                );
                let ri = inner.size.height / 2.0;
                NSColor::controlAccentColor().setFill();
                NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(inner, ri, ri).fill();
            }
        }
    }
);

thread_local! {
    /// Progress by tab, with when it last changed.
    static PROGRESS: RefCell<FxHashMap<u64, (f32, Instant)>> = RefCell::new(FxHashMap::default());
    static VIEW: RefCell<Option<Retained<DockProgress>>> = const { RefCell::new(None) };
}

/// Tab `tab`'s command is `percent` done, or `None` once it has ended.
pub fn set_progress(tab: u64, percent: Option<f32>) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let shown = PROGRESS.with(|p| {
        let mut p = p.borrow_mut();
        match percent.filter(|v| *v < 100.0) {
            Some(v) => {
                p.insert(tab, (v, Instant::now()));
            }
            None => {
                p.remove(&tab);
            }
        }
        p.values().max_by_key(|(_, t)| *t).map(|(v, _)| *v)
    });
    let tile = NSApplication::sharedApplication(mtm).dockTile();
    match shown {
        Some(v) => {
            let view = VIEW.with(|cell| {
                cell.borrow_mut()
                    .get_or_insert_with(|| {
                        let this = DockProgress::alloc(mtm).set_ivars(DockState::default());
                        let size = tile.size();
                        unsafe { msg_send![super(this), initWithFrame: NSRect::new(NSPoint::new(0.0, 0.0), size)] }
                    })
                    .clone()
            });
            view.ivars().fraction.set(v as f64 / 100.0);
            tile.setContentView(Some(&view));
        }
        None => tile.setContentView(None),
    }
    tile.display();
}
