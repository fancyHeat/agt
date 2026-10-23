//! macOS: a preferences-style window. A toolbar of pages (`NSWindowToolbarStylePreference`),
//! each page an `NSGridView` of right-aligned labels and their controls, the window sized to
//! the page. Its own process (an accessory app, like AGT: no Dock icon), so it has a main
//! menu just for the key equivalents: Cmd+C/V/X/A/Z in text fields, Cmd+W, Cmd+Q.
//!
//! Another `run` (the tray's "Settings..." again) raises this window through a distributed
//! notification ([`activate`]): macOS only lets the active app activate another, and
//! neither process is active then.
//!
//! Every control's `tag` is its row's id. Changes are sent to the model as they happen; the
//! page is then rebuilt from the model on the next run-loop turn (never inside the control's
//! own action or a text field's end-of-editing), with the focused control focused again.
//!
//! A hotkey row listens for a press through the key recorder (`agt_platform::capture_keys`),
//! which keeps the keys from every app (this window too) while it runs. A timer picks up what
//! it reports and shows the keys held; anything else done in the window cancels listening.

use crate::{Control, Error, Event, Model, Page, Stat};
use agt_platform::{Capture, CaptureHandle, capture_keys};
use objc2::{
    ClassType, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send,
    rc::Retained,
    runtime::{AnyObject, NSObject, NSObjectProtocol, ProtocolObject, Sel},
    sel,
};
use objc2_app_kit::{
    NSAlert, NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSBorderType,
    NSButton, NSColor, NSControl, NSControlSize, NSControlStateValueOff, NSControlStateValueOn,
    NSControlTextEditingDelegate, NSEventModifierFlags, NSFont, NSGridCellPlacement,
    NSGridRowAlignment, NSGridView, NSImage, NSLayoutAttribute, NSLayoutConstraint,
    NSLineBreakMode, NSMenu, NSMenuItem, NSPopUpButton, NSResponder, NSRunningApplication,
    NSScreen, NSScrollView, NSScroller, NSScrollerStyle, NSSecureTextField, NSStackView,
    NSTextDelegate, NSTextField, NSTextFieldDelegate, NSTextView, NSTextViewDelegate, NSToolbar,
    NSToolbarDelegate, NSToolbarItem, NSView, NSWindow, NSWindowDelegate, NSWindowStyleMask,
    NSWindowToolbarStyle,
};
use objc2_foundation::{
    NSArray, NSDistributedNotificationCenter, NSNotification, NSNotificationSuspensionBehavior,
    NSPoint, NSRange, NSRect, NSRunLoop, NSRunLoopCommonModes, NSSize, NSString, NSTimer,
};
use std::{
    cell::RefCell,
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Instant,
};

/// Width of the label and control columns, in points: the same on every page, so the
/// window keeps its width.
const LABEL_WIDTH: f64 = 170.0;
const CONTROL_WIDTH: f64 = 320.0;
const LONG_TEXT_HEIGHT: f64 = 84.0;
const MARGIN: f64 = 20.0;
/// A full-width row (a table, figures): both columns and the gap between them.
const FULL_WIDTH: f64 = LABEL_WIDTH + 8.0 + CONTROL_WIDTH;
/// A table's columns after the first.
const TABLE_COLUMN_WIDTH: f64 = 70.0;
/// Posted by [`activate`]: the window comes to the front.
const RAISE: &str = "io.notics.agt.settings.raise";
/// How often [`Model::changed_elsewhere`] is asked.
const POLL_SECONDS: f64 = 2.0;
/// How often a listening hotkey row shows what the key recorder reported, and a recorder done
/// listening is checked for its keys being up.
const CAPTURE_SECONDS: f64 = 0.05;
/// A hotkey row's key button: the control column but for the minus button beside it, the
/// same with or without one.
const HOTKEY_WIDTH: f64 = CONTROL_WIDTH - 40.0;

struct State {
    model: Box<dyn Model>,
    pages: Vec<Page>,
    current: usize,
    window: Option<Retained<NSWindow>>,
    /// Text rows as shown: an edit is only reported when it differs.
    shown_text: HashMap<usize, String>,
    /// Multi-line fields (an `NSTextView` has no settable tag).
    text_views: Vec<(Retained<NSTextView>, usize)>,
    /// A rebuild is scheduled.
    rebuild_pending: bool,
    /// The file changed while a text field was being edited: rebuild once editing ends.
    stale: bool,
    /// Hotkey rows' key buttons, by row id.
    hotkey_buttons: HashMap<usize, Retained<NSButton>>,
    listening: Option<Listening>,
    /// Recorders done listening, each kept (since when) until the keys it kept from apps are
    /// up: dropped while one is held (the Esc that cancelled), its auto-repeat and release
    /// would reach this window and AGT.
    releasing: Vec<(CaptureHandle, Instant)>,
    /// Runs `capture:` while a row listens or a recorder waits for its keys.
    capture_timer: Option<Retained<NSTimer>>,
}

/// The hotkey row listening for a press.
struct Listening {
    id: usize,
    capture: CaptureHandle,
    /// What the recorder reported last: set on its thread (which must never wait for this
    /// one), taken by `capture:`.
    latest: Arc<Mutex<Option<Capture>>>,
    /// The row's text: [`crate::LISTENING`], or the keys held.
    title: String,
}

struct Ivars {
    state: RefCell<Option<State>>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; `Controller` doesn't implement Drop.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "AGTSettingsController"]
    #[ivars = Ivars]
    struct Controller;

    impl Controller {
        #[unsafe(method(toggled:))]
        fn toggled(&self, sender: &NSButton) {
            self.end_editing();
            self.send(Event::Toggle(tag(sender), sender.state() == NSControlStateValueOn));
        }

        #[unsafe(method(chose:))]
        fn chose(&self, sender: &NSPopUpButton) {
            self.end_editing();
            let index = sender.indexOfSelectedItem();
            if index >= 0 {
                self.send(Event::Choose(tag(sender), index as usize));
            }
        }

        #[unsafe(method(clicked:))]
        fn clicked(&self, sender: &NSButton) {
            self.end_editing();
            self.send(Event::Click(tag(sender)));
        }

        /// A hotkey row's key button: listens for a press (already listening: goes on).
        #[unsafe(method(recordHotkey:))]
        fn record_hotkey(&self, sender: &NSButton) {
            self.end_editing();
            let id = tag(sender);
            if self.with(|s| s.listening.as_ref().is_some_and(|l| l.id == id)) == Some(false) {
                // Another row listening is cancelled first.
                self.end_listening(None);
                self.listen(id);
            }
        }

        /// Lets go of recorders done listening whose keys are up, and shows what the listening
        /// one reported.
        #[unsafe(method(capture:))]
        fn capture_tick(&self, _timer: &NSTimer) {
            self.with(|s| {
                s.releasing
                    .retain(|(capture, since)| {
                        !capture.keys_up() && since.elapsed() < crate::RELEASE_WAIT
                    })
            });
            let latest = self
                .with(|s| s.listening.as_ref()?.latest.lock().ok()?.take())
                .flatten();
            match latest {
                Some(Capture::Held(codes)) => self.show_held(&codes),
                Some(Capture::Done(codes)) => self.end_listening(Some(codes)),
                Some(Capture::Cancelled) => self.end_listening(None),
                None => {}
            }
            self.run_capture_timer(false);
        }

        #[unsafe(method(selectPage:))]
        fn select_page(&self, sender: &NSToolbarItem) {
            self.end_editing();
            self.end_listening(None);
            let id = sender.itemIdentifier().to_string();
            let index = self.with(|s| s.pages.iter().position(|p| p.title == id));
            if let Some(index) = index.flatten() {
                self.with(|s| s.current = index);
                self.rebuild(true);
            }
        }

        #[unsafe(method(poll:))]
        fn poll(&self, _timer: &NSTimer) {
            let changed = self.with(|s| s.model.changed_elsewhere()).unwrap_or(false);
            if changed || self.with(|s| s.stale).unwrap_or(false) {
                if self.editing() {
                    self.with(|s| s.stale = true);
                } else {
                    self.schedule_rebuild();
                }
            }
        }

        #[unsafe(method(raise:))]
        fn raise(&self, notification: &NSNotification) {
            // The page to show, if [`activate`] named one.
            let page = notification
                .object()
                .and_then(|o| o.downcast::<NSString>().ok())
                .map(|t| t.to_string());
            let index = self
                .with(|s| crate::page_index(&s.pages, page.as_deref()).filter(|&i| i != s.current))
                .flatten();
            if let Some(index) = index {
                self.end_editing();
                self.end_listening(None);
                self.with(|s| s.current = index);
                self.rebuild(true);
            }
            if let Some(window) = self.with(|s| s.window.clone()).flatten() {
                bring_forward(&window);
            }
        }

        #[unsafe(method(rebuild))]
        fn rebuild_now(&self) {
            self.with(|s| s.rebuild_pending = false);
            self.rebuild(false);
        }
    }

    unsafe impl NSObjectProtocol for Controller {}

    unsafe impl NSControlTextEditingDelegate for Controller {
        #[unsafe(method(controlTextDidEndEditing:))]
        fn control_text_did_end_editing(&self, notification: &NSNotification) {
            let Some(object) = notification.object() else { return };
            let Ok(field) = object.downcast::<NSTextField>() else { return };
            self.text_ended(tag(&field), field.stringValue().to_string());
        }
    }

    unsafe impl NSTextFieldDelegate for Controller {}

    unsafe impl NSTextDelegate for Controller {
        #[unsafe(method(textDidEndEditing:))]
        fn text_did_end_editing(&self, notification: &NSNotification) {
            let Some(object) = notification.object() else { return };
            let Ok(view) = object.downcast::<NSTextView>() else { return };
            let id = self.with(|s| {
                s.text_views
                    .iter()
                    .find(|(v, _)| *v == view)
                    .map(|(_, id)| *id)
            });
            if let Some(id) = id.flatten() {
                self.text_ended(id, view.string().to_string());
            }
        }
    }

    unsafe impl NSTextViewDelegate for Controller {}

    unsafe impl NSToolbarDelegate for Controller {
        #[unsafe(method_id(toolbar:itemForItemIdentifier:willBeInsertedIntoToolbar:))]
        fn toolbar_item(
            &self,
            _toolbar: &NSToolbar,
            identifier: &NSString,
            _will_insert: bool,
        ) -> Option<Retained<NSToolbarItem>> {
            self.toolbar_item_for(identifier)
        }

        #[unsafe(method_id(toolbarDefaultItemIdentifiers:))]
        fn default_items(&self, _toolbar: &NSToolbar) -> Retained<NSArray<NSString>> {
            self.page_identifiers()
        }

        #[unsafe(method_id(toolbarAllowedItemIdentifiers:))]
        fn allowed_items(&self, _toolbar: &NSToolbar) -> Retained<NSArray<NSString>> {
            self.page_identifiers()
        }

        #[unsafe(method_id(toolbarSelectableItemIdentifiers:))]
        fn selectable_items(&self, _toolbar: &NSToolbar) -> Retained<NSArray<NSString>> {
            self.page_identifiers()
        }
    }

    unsafe impl NSWindowDelegate for Controller {
        #[unsafe(method(windowWillClose:))]
        fn window_will_close(&self, _notification: &NSNotification) {
            // Ends editing first: a field's end-of-editing reports its last change.
            if let Some(window) = self.with(|s| s.window.clone()).flatten() {
                window.makeFirstResponder(None);
            }
            self.end_listening(None);
            NSApplication::sharedApplication(self.mtm()).terminate(None);
        }
    }
);

impl Controller {
    fn new(mtm: MainThreadMarker, state: State) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(Ivars {
            state: RefCell::new(Some(state)),
        });
        unsafe { msg_send![super(this), init] }
    }

    /// Runs `f` on the state, unless it's borrowed already (an AppKit callback from inside
    /// one of ours, e.g. end-of-editing while a rebuild removes the field): `None` then.
    fn with<R>(&self, f: impl FnOnce(&mut State) -> R) -> Option<R> {
        let mut state = self.ivars().state.try_borrow_mut().ok()?;
        state.as_mut().map(f)
    }

    /// The toolbar item of the page titled `identifier`.
    fn toolbar_item_for(&self, identifier: &NSString) -> Option<Retained<NSToolbarItem>> {
        let title = identifier.to_string();
        let symbol = self
            .with(|s| s.pages.iter().find(|p| p.title == title).map(|p| p.symbol))
            .flatten()?;
        let item =
            NSToolbarItem::initWithItemIdentifier(NSToolbarItem::alloc(self.mtm()), identifier);
        item.setLabel(identifier);
        item.setImage(
            NSImage::imageWithSystemSymbolName_accessibilityDescription(
                &NSString::from_str(symbol),
                Some(identifier),
            )
            .as_deref(),
        );
        unsafe {
            item.setTarget(Some(self));
            item.setAction(Some(sel!(selectPage:)));
        }
        Some(item)
    }

    fn page_identifiers(&self) -> Retained<NSArray<NSString>> {
        let titles = self
            .with(|s| {
                s.pages
                    .iter()
                    .map(|p| NSString::from_str(&p.title))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        NSArray::from_retained_slice(&titles)
    }

    fn text_ended(&self, id: usize, text: String) {
        let changed = self
            .with(|s| s.shown_text.get(&id).is_some_and(|shown| *shown != text))
            .unwrap_or(false);
        if changed {
            self.with(|s| {
                s.shown_text.insert(id, text.clone());
            });
            self.send(Event::Text(id, text));
        } else if self.with(|s| s.stale).unwrap_or(false) {
            self.schedule_rebuild();
        }
    }

    /// Commits a text field being edited (clicking a checkbox, pop-up, button or page doesn't
    /// take the focus from it), before what the click does.
    fn end_editing(&self) {
        if self.editing()
            && let Some(window) = self.with(|s| s.window.clone()).flatten()
        {
            window.makeFirstResponder(None);
        }
    }

    fn send(&self, event: Event) {
        // Any other change cancels listening first.
        if !matches!(event, Event::Hotkey(..)) {
            self.end_listening(None);
        }
        let Some(result) = self.with(|s| {
            crate::note(&mut s.pages, &event);
            s.model.handle(event)
        }) else {
            return;
        };
        if let Err(message) = result {
            self.alert(&message);
        }
        // Shows the file's state: the change, a failed change undone, rows it adds or removes.
        self.schedule_rebuild();
    }

    fn alert(&self, message: &str) {
        let alert = NSAlert::new(self.mtm());
        alert.setMessageText(&NSString::from_str("The setting wasn't changed"));
        alert.setInformativeText(&NSString::from_str(message));
        alert.runModal();
    }

    /// Listens for a press for hotkey row `id`. False if the key recorder can't start (macOS:
    /// no Accessibility), after saying why: the row stays as it is.
    fn listen(&self, id: usize) -> bool {
        let latest = Arc::new(Mutex::new(None));
        let on = latest.clone();
        let capture = match capture_keys(move |c| {
            if let Ok(mut latest) = on.lock() {
                *latest = Some(c);
            }
        }) {
            Ok(capture) => capture,
            Err(e) => {
                self.alert(&e.to_string());
                return false;
            }
        };
        self.with(|s| {
            if let Some(button) = s.hotkey_buttons.get(&id) {
                button.setTitle(&NSString::from_str(crate::LISTENING));
            }
            s.listening = Some(Listening {
                id,
                capture,
                latest,
                title: crate::LISTENING.into(),
            });
        });
        self.run_capture_timer(false);
        true
    }

    /// Shows the keys held so far on the listening row.
    fn show_held(&self, codes: &[u16]) {
        let title = crate::held_label(codes);
        self.with(|s| {
            let Some(listening) = s.listening.as_mut() else {
                return;
            };
            if let Some(button) = s.hotkey_buttons.get(&listening.id) {
                button.setTitle(&NSString::from_str(&title));
            }
            listening.title = title;
        });
    }

    /// Ends listening, if a row listens: shows its hotkey again, lets the recorder go once the
    /// keys it kept are up, and reports `keys` (`None`: cancelled). The page shows what the
    /// model makes of them once it's rebuilt.
    fn end_listening(&self, keys: Option<Vec<u16>>) {
        let listening = self
            .with(|s| {
                let listening = s.listening.take()?;
                let label = s
                    .pages
                    .get(s.current)
                    .and_then(|p| crate::hotkey_label(p, listening.id));
                if let (Some(button), Some(label)) = (s.hotkey_buttons.get(&listening.id), label) {
                    button.setTitle(&NSString::from_str(label));
                }
                Some(listening)
            })
            .flatten();
        let Some(Listening { id, capture, .. }) = listening else {
            return;
        };
        // Before the event, which may bring up an alert: keys must reach it.
        if capture.keys_up() {
            drop(capture);
        } else {
            self.with(|s| s.releasing.push((capture, Instant::now())));
            // A fresh timer: AppKit doesn't fire one again while its own action runs (this may
            // be `capture:`), and the alert's modal loop mustn't hold up the wait for the keys.
            self.run_capture_timer(true);
        }
        self.send(Event::Hotkey(id, keys));
    }

    /// Runs the capture timer while a row listens or a recorder waits for its keys, and stops
    /// it after. `fresh`: replaces a running one.
    fn run_capture_timer(&self, fresh: bool) {
        self.with(|s| {
            let needed = s.listening.is_some() || !s.releasing.is_empty();
            if (fresh || !needed)
                && let Some(timer) = s.capture_timer.take()
            {
                timer.invalidate();
            }
            if needed && s.capture_timer.is_none() {
                let timer = unsafe {
                    NSTimer::timerWithTimeInterval_target_selector_userInfo_repeats(
                        CAPTURE_SECONDS,
                        self,
                        sel!(capture:),
                        None,
                        true,
                    )
                };
                // Also while a pop-up's menu is open or an alert is up.
                unsafe {
                    NSRunLoop::currentRunLoop().addTimer_forMode(&timer, NSRunLoopCommonModes)
                };
                s.capture_timer = Some(timer);
            }
        });
    }

    /// After the page is shown afresh: listening goes on while the page has its row, and a row
    /// that asks to listen starts.
    fn listen_as_shown(&self) {
        let Some((gone, wanted)) = self.with(|s| {
            crate::listening_as_shown(s.pages.get(s.current), s.listening.as_ref().map(|l| l.id))
        }) else {
            return;
        };
        if gone {
            self.end_listening(None);
        }
        if let Some(id) = wanted {
            self.end_listening(None);
            if !self.listen(id) {
                // The model has the row listening: that's over.
                self.send(Event::Hotkey(id, None));
            }
        }
    }

    fn schedule_rebuild(&self) {
        if self.with(|s| std::mem::replace(&mut s.rebuild_pending, true)) == Some(false) {
            unsafe {
                let _: () = msg_send![
                    self,
                    performSelector: sel!(rebuild),
                    withObject: None::<&AnyObject>,
                    afterDelay: 0.0f64
                ];
            }
        }
    }

    /// Whether a text field or text view has keyboard focus.
    fn editing(&self) -> bool {
        let Some(window) = self.with(|s| s.window.clone()).flatten() else {
            return false;
        };
        window
            .firstResponder()
            .is_some_and(|r: Retained<NSResponder>| r.isKindOfClass(NSTextView::class()))
    }

    /// Shows the current page afresh from the model, sizing the window to it (its top-left
    /// corner stays put). Unless `force`d (another page), only if the pages changed.
    fn rebuild(&self, force: bool) {
        let mtm = self.mtm();
        let Some(window) = self.with(|s| s.window.clone()).flatten() else {
            return;
        };
        let focused = focused_id(&window, self);
        // The same page shown afresh (e.g. new figures) keeps its scroll position: how far
        // its visible part is below the top.
        let below_top = window
            .contentView()
            .filter(|_| !force)
            .and_then(|v| v.downcast::<NSScrollView>().ok())
            .and_then(|scroll| {
                let doc = scroll.documentView()?;
                let visible = scroll.documentVisibleRect();
                Some(doc.frame().size.height - visible.origin.y - visible.size.height)
            });
        let Some(view) = self.with(|s| {
            s.stale = false;
            let pages = s.model.pages();
            if !force && pages == s.pages {
                return None;
            }
            s.pages = pages;
            s.current = s.current.min(s.pages.len().saturating_sub(1));
            s.shown_text.clear();
            s.text_views.clear();
            s.hotkey_buttons.clear();
            let page = s.pages.get(s.current).cloned()?;
            Some((page_view(mtm, self, &page, s), page.title))
        }) else {
            return;
        };
        let Some((view, title)) = view else { return };
        window.setTitle(&NSString::from_str(&title));
        if let Some(toolbar) = window.toolbar() {
            toolbar.setSelectedItemIdentifier(Some(&NSString::from_str(&title)));
        }

        let mut size = view.fittingSize();
        // Taller than the screen has room for: scrolls.
        let chrome = window.frame().size.height - window.contentLayoutRect().size.height;
        let room = window
            .screen()
            .or_else(|| NSScreen::mainScreen(mtm))
            .map_or(f64::MAX, |s| s.visibleFrame().size.height - chrome);
        let scrolls = size.height > room;
        let content_view = if scrolls {
            let scroll = NSScrollView::initWithFrame(NSScrollView::alloc(mtm), NSRect::ZERO);
            scroll.setHasVerticalScroller(true);
            scroll.setDrawsBackground(false);
            view.setFrame(NSRect::new(NSPoint::new(0.0, 0.0), size));
            scroll.setDocumentView(Some(&view));
            size = NSSize::new(
                size.width
                    + NSScroller::scrollerWidthForControlSize_scrollerStyle(
                        NSControlSize::Regular,
                        NSScrollerStyle::Overlay,
                        mtm,
                    ),
                room,
            );
            Retained::into_super(scroll)
        } else {
            view.clone()
        };
        let old = window.frame();
        let content = NSRect::new(NSPoint::new(0.0, 0.0), size);
        let mut frame = window.frameRectForContentRect(content);
        frame.origin.x = old.origin.x;
        frame.origin.y = old.origin.y + old.size.height - frame.size.height;
        window.setContentView(Some(&content_view));
        window.setFrame_display_animate(frame, true, window.isVisible());
        if scrolls {
            // To the top (not flipped: the top is at the largest y; the clip view clamps),
            // or where it was.
            let height = view.frame().size.height;
            let y = below_top.map_or(height, |below| (height - size.height - below).max(0.0));
            view.scrollPoint(NSPoint::new(0.0, y));
        }

        if let Some(id) = focused {
            refocus(&window, &view, id, self);
        }
        self.listen_as_shown();
    }
}

/// A control's row id. Tags are ids + 1: 0 is every view's default.
fn tag(control: &NSView) -> usize {
    (control.tag() - 1).max(0) as usize
}

/// The row id of the control with keyboard focus (a text field's field editor counts as
/// the field).
fn focused_id(window: &NSWindow, controller: &Controller) -> Option<usize> {
    let responder = window.firstResponder()?;
    if let Ok(view) = responder.clone().downcast::<NSTextView>() {
        // A text field's field editor: its delegate is the field.
        if view.isFieldEditor() {
            let delegate: Option<Retained<AnyObject>> = unsafe { msg_send![&view, delegate] };
            let field = delegate?.downcast::<NSTextField>().ok()?;
            return Some(tag(&field));
        }
        return controller
            .with(|s| {
                s.text_views
                    .iter()
                    .find(|(v, _)| *v == view)
                    .map(|(_, id)| *id)
            })
            .flatten();
    }
    let view = responder.downcast::<NSView>().ok()?;
    (view.tag() > 0).then(|| tag(&view))
}

fn refocus(window: &NSWindow, page: &NSView, id: usize, controller: &Controller) {
    let text_view = controller
        .with(|s| {
            s.text_views
                .iter()
                .find(|(_, i)| *i == id)
                .map(|(v, _)| v.clone())
        })
        .flatten();
    let target: Option<Retained<NSView>> = match text_view {
        Some(v) => Some(Retained::into_super(Retained::into_super(v))),
        None => page.viewWithTag(id as isize + 1),
    };
    if let Some(view) = target {
        window.makeFirstResponder(Some(&view));
    }
}

/// The page's grid inside a view with margins.
fn page_view(
    mtm: MainThreadMarker,
    controller: &Controller,
    page: &Page,
    state: &mut State,
) -> Retained<NSView> {
    let grid = NSGridView::initWithFrame(NSGridView::alloc(mtm), NSRect::ZERO);
    grid.setRowSpacing(8.0);
    grid.setColumnSpacing(8.0);
    grid.setRowAlignment(NSGridRowAlignment::FirstBaseline);
    let empty = || unsafe { NSGridCell_emptyContentView(mtm) };

    let mut first_section = true;
    for section in &page.sections {
        let mut section_top = !first_section;
        first_section = false;
        if !section.title.is_empty() {
            let header = NSTextField::labelWithString(&NSString::from_str(&section.title), mtm);
            header.setFont(Some(
                &NSFont::boldSystemFontOfSize(NSFont::systemFontSize()),
            ));
            let row = grid.addRowWithViews(&NSArray::from_retained_slice(&[
                Retained::into_super(Retained::into_super(header)),
                empty(),
            ]));
            row.mergeCellsInRange(NSRange::new(0, 2));
            row.cellAtIndex(0)
                .setXPlacement(NSGridCellPlacement::Leading);
            if section_top {
                row.setTopPadding(14.0);
                section_top = false;
            }
        }
        for r in &section.rows {
            if let Some(wide) = full_width_view(mtm, &r.control) {
                let row = grid.addRowWithViews(&NSArray::from_retained_slice(&[wide, empty()]));
                row.mergeCellsInRange(NSRange::new(0, 2));
                row.cellAtIndex(0)
                    .setXPlacement(NSGridCellPlacement::Leading);
                if section_top {
                    row.setTopPadding(14.0);
                    section_top = false;
                }
                continue;
            }
            let label: Retained<NSView> = if r.label.is_empty() {
                empty()
            } else {
                let text = format!("{}:", r.label);
                let label = NSTextField::labelWithString(&NSString::from_str(&text), mtm);
                if !r.enabled {
                    label.setTextColor(Some(&NSColor::disabledControlTextColor()));
                }
                Retained::into_super(Retained::into_super(label))
            };
            let control = control_view(mtm, controller, r.id, &r.control, r.enabled, state);
            let row = grid.addRowWithViews(&NSArray::from_retained_slice(&[label, control]));
            if section_top {
                row.setTopPadding(14.0);
                section_top = false;
            }
            if !r.note.is_empty() {
                let note = NSTextField::wrappingLabelWithString(&NSString::from_str(&r.note), mtm);
                note.setFont(Some(&NSFont::systemFontOfSize(
                    NSFont::smallSystemFontSize(),
                )));
                note.setTextColor(Some(&NSColor::secondaryLabelColor()));
                note.setPreferredMaxLayoutWidth(CONTROL_WIDTH);
                note.setSelectable(false);
                let row = grid.addRowWithViews(&NSArray::from_retained_slice(&[
                    empty(),
                    Retained::into_super(Retained::into_super(note)),
                ]));
                row.setTopPadding(-4.0);
            }
        }
    }
    if grid.numberOfColumns() >= 2 {
        let labels = grid.columnAtIndex(0);
        labels.setXPlacement(NSGridCellPlacement::Trailing);
        labels.setWidth(LABEL_WIDTH);
        let controls = grid.columnAtIndex(1);
        controls.setXPlacement(NSGridCellPlacement::Leading);
        controls.setWidth(CONTROL_WIDTH);
    }

    let view = NSView::initWithFrame(NSView::alloc(mtm), NSRect::ZERO);
    grid.setTranslatesAutoresizingMaskIntoConstraints(false);
    view.addSubview(&grid);
    NSLayoutConstraint::activateConstraints(&NSArray::from_retained_slice(&[
        grid.leadingAnchor()
            .constraintEqualToAnchor_constant(&view.leadingAnchor(), MARGIN),
        view.trailingAnchor()
            .constraintEqualToAnchor_constant(&grid.trailingAnchor(), MARGIN),
        grid.topAnchor()
            .constraintEqualToAnchor_constant(&view.topAnchor(), MARGIN),
        view.bottomAnchor()
            .constraintEqualToAnchor_constant(&grid.bottomAnchor(), MARGIN),
    ]));
    view
}

/// The system font at `size` and `weight` (`NSFontWeight`: 0 regular, 0.3 semibold), with
/// evenly spaced digits if `digits`. Through `msg_send!`: objc2-app-kit puts `NSFontWeight`
/// behind a feature of its own.
fn font(size: f64, weight: f64, digits: bool) -> Retained<NSFont> {
    unsafe {
        if digits {
            msg_send![NSFont::class(), monospacedDigitSystemFontOfSize: size, weight: weight]
        } else {
            msg_send![NSFont::class(), systemFontOfSize: size, weight: weight]
        }
    }
}

/// The view of a control that spans both columns ([`Control::Stats`], [`Control::Table`]).
fn full_width_view(mtm: MainThreadMarker, control: &Control) -> Option<Retained<NSView>> {
    let label = |text: &str, font: &NSFont, secondary: bool| {
        let l = NSTextField::labelWithString(&NSString::from_str(text), mtm);
        l.setFont(Some(font));
        if secondary {
            l.setTextColor(Some(&NSColor::secondaryLabelColor()));
        }
        l.setSelectable(true);
        Retained::into_super(Retained::into_super(l))
    };
    let grid = match control {
        Control::Stats(stats) => stats_grid(mtm, stats, &label),
        Control::Table { columns, rows } => {
            let header = font(NSFont::smallSystemFontSize(), 0.3, false);
            let body = font(NSFont::systemFontSize(), 0.0, true);
            let grid = NSGridView::initWithFrame(NSGridView::alloc(mtm), NSRect::ZERO);
            grid.setRowSpacing(4.0);
            grid.setColumnSpacing(8.0);
            let cells: Vec<_> = columns.iter().map(|c| label(c, &header, true)).collect();
            grid.addRowWithViews(&NSArray::from_retained_slice(&cells));
            for row in rows {
                let cells: Vec<_> = (0..columns.len())
                    .map(|i| label(row.get(i).map_or("", String::as_str), &body, false))
                    .collect();
                grid.addRowWithViews(&NSArray::from_retained_slice(&cells));
            }
            let rest = columns.len().saturating_sub(1) as f64;
            for i in 0..grid.numberOfColumns() {
                let column = grid.columnAtIndex(i);
                if i == 0 {
                    column.setXPlacement(NSGridCellPlacement::Leading);
                    column.setWidth(FULL_WIDTH - rest * (TABLE_COLUMN_WIDTH + 8.0));
                } else {
                    column.setXPlacement(NSGridCellPlacement::Trailing);
                    column.setWidth(TABLE_COLUMN_WIDTH);
                }
            }
            if grid.numberOfRows() > 1 {
                grid.rowAtIndex(1).setTopPadding(2.0);
            }
            grid
        }
        _ => return None,
    };
    Some(Retained::into_super(grid))
}

/// Large figures over their captions, in equal columns.
fn stats_grid(
    mtm: MainThreadMarker,
    stats: &[Stat],
    label: &dyn Fn(&str, &NSFont, bool) -> Retained<NSView>,
) -> Retained<NSGridView> {
    let big = font(22.0, 0.3, true);
    let small = font(NSFont::smallSystemFontSize(), 0.0, false);
    let grid = NSGridView::initWithFrame(NSGridView::alloc(mtm), NSRect::ZERO);
    grid.setRowSpacing(0.0);
    grid.setColumnSpacing(8.0);
    let values: Vec<_> = stats.iter().map(|s| label(&s.value, &big, false)).collect();
    let captions: Vec<_> = stats
        .iter()
        .map(|s| label(&s.caption, &small, true))
        .collect();
    grid.addRowWithViews(&NSArray::from_retained_slice(&values));
    grid.addRowWithViews(&NSArray::from_retained_slice(&captions));
    let n = stats.len().max(1) as f64;
    for i in 0..grid.numberOfColumns() {
        let column = grid.columnAtIndex(i);
        column.setXPlacement(NSGridCellPlacement::Center);
        column.setWidth((FULL_WIDTH - (n - 1.0) * 8.0) / n);
    }
    grid
}

#[allow(non_snake_case)]
unsafe fn NSGridCell_emptyContentView(mtm: MainThreadMarker) -> Retained<NSView> {
    // `+[NSGridCell emptyContentView]`: a placeholder that takes no space. objc2-app-kit
    // declares it on NSGridCell, which needs a feature of its own; this is the same call.
    let _ = mtm;
    unsafe { msg_send![objc2::class!(NSGridCell), emptyContentView] }
}

fn control_view(
    mtm: MainThreadMarker,
    controller: &Controller,
    id: usize,
    control: &Control,
    enabled: bool,
    state: &mut State,
) -> Retained<NSView> {
    let target: Option<&AnyObject> = Some(controller);
    let view: Retained<NSControl> = match control {
        Control::Toggle { text, on } => {
            let b = unsafe {
                NSButton::checkboxWithTitle_target_action(
                    &NSString::from_str(text),
                    target,
                    Some(sel!(toggled:)),
                    mtm,
                )
            };
            b.setState(if *on {
                NSControlStateValueOn
            } else {
                NSControlStateValueOff
            });
            Retained::into_super(b)
        }
        Control::Choice { options, selected } => {
            let p = NSPopUpButton::initWithFrame_pullsDown(
                NSPopUpButton::alloc(mtm),
                NSRect::ZERO,
                false,
            );
            // Through the menu: `addItemWithTitle:` drops an earlier item of the same title.
            if let Some(menu) = p.menu() {
                for o in options {
                    unsafe {
                        menu.addItemWithTitle_action_keyEquivalent(
                            &NSString::from_str(o),
                            None,
                            &NSString::new(),
                        )
                    };
                }
            }
            p.selectItemAtIndex(*selected as isize);
            unsafe {
                p.setTarget(target);
                p.setAction(Some(sel!(chose:)));
            }
            Retained::into_super(Retained::into_super(p))
        }
        Control::Text {
            value,
            placeholder,
            secret,
        } => {
            let f: Retained<NSTextField> = if *secret {
                Retained::into_super(NSSecureTextField::initWithFrame(
                    NSSecureTextField::alloc(mtm),
                    NSRect::ZERO,
                ))
            } else {
                NSTextField::initWithFrame(NSTextField::alloc(mtm), NSRect::ZERO)
            };
            f.setStringValue(&NSString::from_str(value));
            f.setPlaceholderString(Some(&NSString::from_str(placeholder)));
            unsafe { f.setDelegate(Some(ProtocolObject::from_ref(controller))) };
            // Return ends editing (and so reports the change) instead of inserting nothing.
            if let Some(cell) = f.cell() {
                cell.setScrollable(true);
                cell.setWraps(false);
            }
            f.setTranslatesAutoresizingMaskIntoConstraints(false);
            f.widthAnchor()
                .constraintEqualToConstant(CONTROL_WIDTH)
                .setActive(true);
            state.shown_text.insert(id, value.clone());
            Retained::into_super(f)
        }
        Control::LongText { value, .. } => {
            let scroll = NSTextView::scrollableTextView(mtm);
            scroll.setBorderType(NSBorderType::BezelBorder);
            scroll.setHasVerticalScroller(true);
            if let Some(doc) = scroll.documentView()
                && let Ok(text) = doc.downcast::<NSTextView>()
            {
                text.setString(&NSString::from_str(value));
                text.setRichText(false);
                text.setFont(Some(&NSFont::systemFontOfSize(NSFont::systemFontSize())));
                // Prompts are sent as written: no smart quotes, dashes or replacements.
                text.setAutomaticQuoteSubstitutionEnabled(false);
                text.setAutomaticDashSubstitutionEnabled(false);
                text.setAutomaticTextReplacementEnabled(false);
                text.setAutomaticSpellingCorrectionEnabled(false);
                text.setEditable(enabled);
                text.setDelegate(Some(ProtocolObject::from_ref(controller)));
                state.text_views.push((text, id));
                state.shown_text.insert(id, value.clone());
            }
            scroll.setTranslatesAutoresizingMaskIntoConstraints(false);
            scroll
                .widthAnchor()
                .constraintEqualToConstant(CONTROL_WIDTH)
                .setActive(true);
            scroll
                .heightAnchor()
                .constraintEqualToConstant(LONG_TEXT_HEIGHT)
                .setActive(true);
            return Retained::into_super(scroll);
        }
        Control::Value(text) => {
            let l = NSTextField::wrappingLabelWithString(&NSString::from_str(text), mtm);
            l.setPreferredMaxLayoutWidth(CONTROL_WIDTH);
            l.setSelectable(true);
            Retained::into_super(l)
        }
        Control::Button(title) => {
            let b = unsafe {
                NSButton::buttonWithTitle_target_action(
                    &NSString::from_str(title),
                    target,
                    Some(sel!(clicked:)),
                    mtm,
                )
            };
            Retained::into_super(b)
        }
        Control::Hotkey {
            label, removable, ..
        } => {
            // Rebuilt while listening: still shows the keys held.
            let title = match &state.listening {
                Some(l) if l.id == id => l.title.as_str(),
                _ => label.as_str(),
            };
            let key = unsafe {
                NSButton::buttonWithTitle_target_action(
                    &NSString::from_str(title),
                    target,
                    Some(sel!(recordHotkey:)),
                    mtm,
                )
            };
            key.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
            key.setTranslatesAutoresizingMaskIntoConstraints(false);
            key.widthAnchor()
                .constraintEqualToConstant(HOTKEY_WIDTH)
                .setActive(true);
            let mut buttons = vec![key.clone()];
            if *removable {
                buttons.push(unsafe {
                    NSButton::buttonWithTitle_target_action(
                        // A minus sign.
                        &NSString::from_str("\u{2212}"),
                        target,
                        Some(sel!(clicked:)),
                        mtm,
                    )
                });
            }
            state.hotkey_buttons.insert(id, key);
            let views: Vec<Retained<NSView>> = buttons
                .into_iter()
                .map(|b| {
                    b.setTag(id as isize + 1);
                    b.setEnabled(enabled);
                    Retained::into_super(Retained::into_super(b))
                })
                .collect();
            let stack = NSStackView::stackViewWithViews(&NSArray::from_retained_slice(&views), mtm);
            stack.setAlignment(NSLayoutAttribute::FirstBaseline);
            return Retained::into_super(stack);
        }
        // Shown by `full_width_view`.
        Control::Stats(_) | Control::Table { .. } => {
            return unsafe { NSGridCell_emptyContentView(mtm) };
        }
    };
    view.setTag(id as isize + 1);
    view.setEnabled(enabled);
    Retained::into_super(view)
}

/// The main menu: invisible for an accessory app, but key equivalents still go through it.
fn main_menu(mtm: MainThreadMarker) -> Retained<NSMenu> {
    let item = |title: &str, action: Sel, key: &str| unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str(title),
            Some(action),
            &NSString::from_str(key),
        )
    };
    let submenu = |title: &str, items: Vec<Retained<NSMenuItem>>| {
        let menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str(title));
        for i in &items {
            menu.addItem(i);
        }
        let parent = item(title, sel!(submenuAction:), "");
        unsafe { parent.setAction(None) };
        parent.setSubmenu(Some(&menu));
        parent
    };
    let redo = item("Redo", sel!(redo:), "z");
    redo.setKeyEquivalentModifierMask(NSEventModifierFlags::Command | NSEventModifierFlags::Shift);
    let bar = NSMenu::new(mtm);
    bar.addItem(&submenu(
        "AGT",
        // Closes the window, which quits (after saving an edit in progress).
        vec![item("Quit AGT Settings", sel!(performClose:), "q")],
    ));
    bar.addItem(&submenu(
        "Edit",
        vec![
            item("Undo", sel!(undo:), "z"),
            redo,
            item("Cut", sel!(cut:), "x"),
            item("Copy", sel!(copy:), "c"),
            item("Paste", sel!(paste:), "v"),
            item("Select All", sel!(selectAll:), "a"),
        ],
    ));
    bar.addItem(&submenu(
        "Window",
        vec![item("Close", sel!(performClose:), "w")],
    ));
    bar
}

pub fn run(
    title: &str,
    mut model: Box<dyn Model>,
    page: Option<&str>,
    _icon: Option<crate::Icon>,
) -> Result<(), Error> {
    let mtm = MainThreadMarker::new().ok_or("the settings window must run on the main thread")?;
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    app.setMainMenu(Some(&main_menu(mtm)));

    let pages = model.pages();
    if pages.is_empty() {
        return Err("no settings to show".into());
    }
    let current = crate::page_index(&pages, page).unwrap_or(0);
    let controller = Controller::new(
        mtm,
        State {
            model,
            pages,
            current,
            window: None,
            shown_text: HashMap::new(),
            text_views: Vec::new(),
            rebuild_pending: false,
            stale: false,
            hotkey_buttons: HashMap::new(),
            listening: None,
            releasing: Vec::new(),
            capture_timer: None,
        },
    );

    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(500.0, 300.0)),
            NSWindowStyleMask::Titled
                | NSWindowStyleMask::Closable
                | NSWindowStyleMask::Miniaturizable,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    unsafe { window.setReleasedWhenClosed(false) };
    window.setDelegate(Some(ProtocolObject::from_ref(&*controller)));
    let toolbar = NSToolbar::initWithIdentifier(
        NSToolbar::alloc(mtm),
        &NSString::from_str("AGTSettingsToolbar"),
    );
    toolbar.setDelegate(Some(ProtocolObject::from_ref(&*controller)));
    toolbar.setAllowsUserCustomization(false);
    window.setToolbar(Some(&toolbar));
    window.setToolbarStyle(NSWindowToolbarStyle::Preference);
    window.setTitle(&NSString::from_str(title));
    controller.with(|s| s.window = Some(window.clone()));

    controller.rebuild(true);
    window.center();
    bring_forward(&window);
    unsafe {
        NSDistributedNotificationCenter::defaultCenter()
            .addObserver_selector_name_object_suspensionBehavior(
                &controller,
                sel!(raise:),
                Some(&NSString::from_str(RAISE)),
                None,
                NSNotificationSuspensionBehavior::DeliverImmediately,
            );
    }

    let _timer = unsafe {
        NSTimer::scheduledTimerWithTimeInterval_target_selector_userInfo_repeats(
            POLL_SECONDS,
            &controller,
            sel!(poll:),
            None,
            true,
        )
    };
    app.run();
    Ok(())
}

/// Shows `window` above other apps' windows, and asks to make the app active (macOS may
/// say no; the window is in front either way and a click focuses it).
fn bring_forward(window: &NSWindow) {
    let app = NSApplication::sharedApplication(window.mtm());
    if window.isMiniaturized() {
        window.deminiaturize(None);
    }
    window.makeKeyAndOrderFront(None);
    window.orderFrontRegardless();
    #[allow(deprecated)]
    app.activateIgnoringOtherApps(true);
}

pub fn activate(pid: u32, page: Option<&str>) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    // Still running (not a stale pid file).
    if NSRunningApplication::runningApplicationWithProcessIdentifier(pid).is_none() {
        return false;
    }
    unsafe {
        NSDistributedNotificationCenter::defaultCenter()
            .postNotificationName_object_userInfo_deliverImmediately(
                &NSString::from_str(RAISE),
                page.map(NSString::from_str).as_deref(),
                None,
                true,
            );
    }
    true
}
