//! Native macOS File menu (New / Open / Save / Save As / Print), inserted into the
//! menu bar next to the app menu. Clicks and their key equivalents arrive as
//! `MenuCmd`s on a channel and wake the UI.

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MenuCmd {
    New = 1,
    Open,
    Save,
    SaveAs,
    Print,
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
    use std::sync::OnceLock;

    static SINK: OnceLock<(Sender<MenuCmd>, egui::Context)> = OnceLock::new();

    fn from_tag(t: isize) -> Option<MenuCmd> {
        use MenuCmd::*;
        [New, Open, Save, SaveAs, Print].into_iter().find(|c| *c as isize == t)
    }

    define_class!(
        // SAFETY: NSObject has no subclassing requirements and Target has no Drop impl.
        #[unsafe(super(NSObject))]
        #[thread_kind = MainThreadOnly]
        #[name = "PTLabelMenuTarget"]
        struct Target;

        impl Target {
            #[unsafe(method(ptlabelMenu:))]
            fn menu(&self, sender: &NSMenuItem) {
                if let (Some((tx, ctx)), Some(cmd)) = (SINK.get(), from_tag(sender.tag())) {
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
        let file_item = NSMenuItem::new(mtm);
        file_item.setSubmenu(Some(&file));
        main.insertItem_atIndex(&file_item, main.numberOfItems().min(1));
        // Menu items reference their target weakly; keep it for the app's lifetime.
        std::mem::forget(target);
        Some(rx)
    }
}

#[cfg(target_os = "macos")]
pub use imp::install;

#[cfg(not(target_os = "macos"))]
pub fn install(_: &egui::Context) -> Option<crossbeam_channel::Receiver<MenuCmd>> {
    None
}
