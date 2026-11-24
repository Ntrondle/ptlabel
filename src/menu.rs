//! Native macOS File menu (New / Open / Save / Save As / Print), inserted into the
//! menu bar next to the app menu. Clicks and their key equivalents arrive as
//! `MenuCmd`s on a channel and wake the UI.
//! Also File › Open Recent (filled by `set_recent`) and an Edit menu with Undo / Redo.

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MenuCmd {
    New = 1,
    Open,
    Save,
    SaveAs,
    ExportPng,
    Print,
    Undo,
    Redo,
    /// File › Open Recent entry; its path comes from `take_recent`.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    OpenRecent,
    ClearRecent,
}

#[cfg(target_os = "macos")]
mod imp {
    use super::MenuCmd;
    use crossbeam_channel::{Receiver, Sender, unbounded};
    use objc2::rc::Retained;
    use objc2::runtime::NSObject;
    use objc2::{MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
    use objc2_app_kit::{NSApplication, NSMenu, NSMenuItem};
    use objc2_foundation::NSString;
    use std::cell::RefCell;
    use std::sync::{Mutex, OnceLock};

    static SINK: OnceLock<(Sender<MenuCmd>, egui::Context)> = OnceLock::new();

    define_class!(
        // SAFETY: NSObject has no subclassing requirements and Target has no Drop impl.
        #[unsafe(super(NSObject))]
        #[thread_kind = MainThreadOnly]
        #[name = "PTLabelMenuTarget"]
        struct Target;

        impl Target {
            #[unsafe(method(ptlabelMenu:))]
            fn menu(&self, sender: &NSMenuItem) {
                if let (Some((tx, ctx)), Some(cmd)) = (SINK.get(), cmd_from_tag(sender.tag())) {
                    let _ = tx.send(cmd);
                    ctx.request_repaint();
                }
            }
        }
    );

    pub fn install(ctx: &egui::Context) -> Option<Receiver<MenuCmd>> {
        let mtm = MainThreadMarker::new()?;
        let main = NSApplication::sharedApplication(mtm).mainMenu()?;
        let (tx, rx) = unbounded();
        SINK.set((tx, ctx.clone())).ok()?;

        let target: Retained<Target> = {
            let this = Target::alloc(mtm).set_ivars(());
            // SAFETY: plain NSObject init.
            unsafe { msg_send![super(this), init] }
        };
        let file = NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str("File"));
        // Key equivalents use ⌘; a capital letter adds ⇧.
        let entries = [
            Some(("New", "n", MenuCmd::New)),
            Some(("Open…", "o", MenuCmd::Open)),
            None,
            Some(("Save", "s", MenuCmd::Save)),
            Some(("Save As…", "S", MenuCmd::SaveAs)),
            Some(("Export PNG…", "E", MenuCmd::ExportPng)),
            None,
            Some(("Print", "p", MenuCmd::Print)),
        ];
        for e in entries {
            let Some((title, key, cmd)) = e else {
                file.addItem(&NSMenuItem::separatorItem(mtm));
                continue;
            };
            // SAFETY: the selector is implemented by Target, which outlives the menu.
            let item = unsafe {
                NSMenuItem::initWithTitle_action_keyEquivalent(
                    NSMenuItem::alloc(mtm),
                    &NSString::from_str(title),
                    Some(sel!(ptlabelMenu:)),
                    &NSString::from_str(key),
                )
            };
            unsafe { item.setTarget(Some(&target)) };
            item.setTag(cmd as isize);
            file.addItem(&item);
        }
        // File › Open Recent below Open…, filled by `set_recent`.
        let recent = NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str("Open Recent"));
        recent.setAutoenablesItems(false);
        let recent_item = NSMenuItem::new(mtm);
        recent_item.setTitle(&NSString::from_str("Open Recent"));
        recent_item.setSubmenu(Some(&recent));
        file.insertItem_atIndex(&recent_item, file.indexOfItemWithTag(MenuCmd::Open as isize) + 1);
        RECENT.set(Some((recent, target.clone(), vec![])));
        let file_item = NSMenuItem::new(mtm);
        file_item.setSubmenu(Some(&file));
        main.insertItem_atIndex(&file_item, main.numberOfItems().min(1));
        // Edit › Undo / Redo only: egui does cut/copy/paste from key events, which menu key
        // equivalents would swallow. The app hands ⌘Z back to a focused text field.
        let edit = NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str("Edit"));
        edit.addItem(&item(mtm, &target, "Undo", "z", MenuCmd::Undo as isize));
        edit.addItem(&item(mtm, &target, "Redo", "Z", MenuCmd::Redo as isize));
        let edit_item = NSMenuItem::new(mtm);
        edit_item.setSubmenu(Some(&edit));
        main.insertItem_atIndex(&edit_item, main.indexOfItem(&file_item) + 1);
        // Menu items reference their target weakly; keep it for the app's lifetime.
        std::mem::forget(target);
        Some(rx)
    }

    /// Tag of File › Open Recent entry `i` is `RECENT_TAG + i`.
    const RECENT_TAG: isize = 1000;

    thread_local! {
        /// Open Recent submenu, the target of its items and the paths it lists.
        static RECENT: RefCell<Option<(Retained<NSMenu>, Retained<Target>, Vec<String>)>> = const { RefCell::new(None) };
    }
    /// Path of the Open Recent entry clicked last.
    static RECENT_PICK: Mutex<Option<String>> = Mutex::new(None);

    fn cmd_from_tag(t: isize) -> Option<MenuCmd> {
        use MenuCmd::*;
        if t >= RECENT_TAG {
            let path = RECENT.with_borrow(|r| r.as_ref()?.2.get((t - RECENT_TAG) as usize).cloned())?;
            *RECENT_PICK.lock().unwrap() = Some(path);
            return Some(OpenRecent);
        }
        [New, Open, Save, SaveAs, ExportPng, Print, Undo, Redo, ClearRecent]
            .into_iter()
            .find(|c| *c as isize == t)
    }

    fn item(mtm: MainThreadMarker, target: &Target, title: &str, key: &str, tag: isize) -> Retained<NSMenuItem> {
        // SAFETY: the selector is implemented by Target, which outlives the menu.
        let item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str(title),
                Some(sel!(ptlabelMenu:)),
                &NSString::from_str(key),
            )
        };
        unsafe { item.setTarget(Some(target)) };
        item.setTag(tag);
        item
    }

    /// List `paths` (most recent first) in File › Open Recent, plus Clear Menu.
    pub fn set_recent(paths: &[String]) {
        let Some(mtm) = MainThreadMarker::new() else { return };
        RECENT.with_borrow_mut(|r| {
            let Some((menu, target, shown)) = r else { return };
            menu.removeAllItems();
            for (i, p) in paths.iter().enumerate() {
                let name = std::path::Path::new(p).file_name().map_or(p.clone(), |n| n.to_string_lossy().into_owned());
                let it = item(mtm, target, &name, "", RECENT_TAG + i as isize);
                it.setToolTip(Some(&NSString::from_str(p)));
                menu.addItem(&it);
            }
            if !paths.is_empty() {
                menu.addItem(&NSMenuItem::separatorItem(mtm));
            }
            let clear = item(mtm, target, "Clear Menu", "", MenuCmd::ClearRecent as isize);
            clear.setEnabled(!paths.is_empty());
            menu.addItem(&clear);
            *shown = paths.to_vec();
        });
    }

    /// Path of the Open Recent entry behind the last `MenuCmd::OpenRecent`.
    pub fn take_recent() -> Option<String> {
        RECENT_PICK.lock().unwrap().take()
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn tags_round_trip() {
            use MenuCmd::*;
            for c in [New, Open, Save, SaveAs, ExportPng, Print, Undo, Redo, ClearRecent] {
                assert!((c as isize) < RECENT_TAG);
                assert_eq!(cmd_from_tag(c as isize), Some(c));
            }
            assert_eq!(cmd_from_tag(0), None); // NSMenuItem's default tag
            assert_eq!(cmd_from_tag(OpenRecent as isize), None); // recent items use RECENT_TAG + i
            assert_eq!(cmd_from_tag(RECENT_TAG), None);
        }
    }
}

#[cfg(target_os = "macos")]
pub use imp::install;
#[cfg(target_os = "macos")]
pub use imp::{set_recent, take_recent};

#[cfg(not(target_os = "macos"))]
pub fn install(_: &egui::Context) -> Option<crossbeam_channel::Receiver<MenuCmd>> {
    None
}

#[cfg(not(target_os = "macos"))]
pub fn set_recent(_: &[String]) {}

#[cfg(not(target_os = "macos"))]
pub fn take_recent() -> Option<String> {
    None
}
