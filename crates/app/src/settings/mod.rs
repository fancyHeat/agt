//! Settings, read from `config.toml` and written back to it ([`agt_core::config::set`]); the
//! file stays the only place settings live. [`schema`] describes them once; they're shown
//! in the tray's Quick settings submenu ([`Settings`]) and the settings window ([`window`],
//! its own process: `agt settings`). In the tray, choices are clickable and free text (API
//! key, prompts, URLs, the models folder) is a line that says what it is.

pub mod schema;
pub mod window;

use agt_core::config::{Config, SettingPath, SettingValue};
use schema::{Choice, Item, Kind, Page, Tray};
use std::collections::HashMap;
use tray_icon::menu::{CheckMenuItem, Error, MenuId, MenuItem, PredefinedMenuItem, Submenu};

/// The submenu's title.
const TITLE: &str = "Quick settings";

/// What clicking a Settings item does.
#[derive(Clone)]
pub enum Action {
    /// Write these values to the file.
    Set(Vec<(SettingPath, SettingValue)>),
    /// Flip an on/off setting: the file's value at click time (`get` reads it), not the
    /// menu's, which may be up to a poll behind an edit made elsewhere.
    Toggle(SettingPath, fn(&Config) -> bool),
    /// Open `config.toml` in a text editor.
    EditFile,
    /// Open the saved dictations' folder (created if needed).
    OpenHistory,
    /// Restart now instead of waiting for dictation to be idle.
    ApplyNow,
}

pub struct Settings {
    pub menu: Submenu,
    actions: HashMap<MenuId, Action>,
}

impl Settings {
    pub fn new() -> Self {
        Self {
            menu: Submenu::new(TITLE, true),
            actions: HashMap::new(),
        }
    }

    pub fn action(&self, id: &MenuId) -> Option<&Action> {
        self.actions.get(id)
    }

    /// Rebuilds the submenu for `config` (the file's contents). `editable`: false while the
    /// file has an error (it's never rewritten then); the items show the running settings.
    /// `applying`: a change is waiting for a restart.
    pub fn show(&mut self, config: &Config, editable: bool, applying: bool) -> Result<(), Error> {
        while self.menu.remove_at(0).is_some() {}
        self.actions.clear();
        self.menu.set_text(if applying {
            format!("{TITLE} (applying...)")
        } else {
            TITLE.to_owned()
        });
        let mut b = Builder {
            actions: &mut self.actions,
            editable,
        };
        if applying {
            b.item(&self.menu, "Restart now to apply", Action::ApplyNow)?;
            self.menu.append(&PredefinedMenuItem::separator())?;
        }
        if !editable {
            b.info(
                &self.menu,
                "config.toml has an error: fix it to change settings",
            )?;
        }
        for page in schema::pages(config) {
            b.page(&self.menu, page)?;
        }
        Ok(())
    }
}

struct Builder<'a> {
    actions: &'a mut HashMap<MenuId, Action>,
    editable: bool,
}

impl Builder<'_> {
    fn page(&mut self, menu: &Submenu, page: Page) -> Result<(), Error> {
        match page.tray {
            Tray::Submenu(title) => {
                let sub = self.submenu(menu, title)?;
                let mut text = false;
                for (i, section) in page.sections.into_iter().enumerate() {
                    if i > 0 {
                        sub.append(&PredefinedMenuItem::separator())?;
                    }
                    for item in section.items {
                        text |= matches!(item.kind, Kind::Text(_));
                        self.add(&sub, item)?;
                    }
                }
                if text {
                    self.info(&sub, "Change these in Settings...")?;
                }
            }
            Tray::Sections => {
                for section in page.sections {
                    let sub;
                    let target = if section.title.is_empty() {
                        menu
                    } else {
                        sub = self.submenu(menu, section.title)?;
                        &sub
                    };
                    for item in section.items {
                        self.add(target, item)?;
                    }
                }
            }
        }
        Ok(())
    }

    fn add(&mut self, menu: &Submenu, item: Item) -> Result<(), Error> {
        match item.kind {
            Kind::Check { path, get, on } => self.check(menu, &item.label, path, get, on),
            Kind::Choice(choices) => self.choice(menu, &item.label, choices),
            // Shown, never the key itself.
            Kind::Text(t) if t.summary.is_empty() => Ok(()),
            Kind::Text(t) => self.info(menu, &t.summary),
            Kind::Info(text) => self.info(menu, &text),
            Kind::Button(action) => {
                menu.append(&PredefinedMenuItem::separator())?;
                let editable = std::mem::replace(&mut self.editable, true);
                let result = self.item(menu, &item.label, action);
                self.editable = editable;
                result
            }
        }
    }

    fn item(&mut self, menu: &Submenu, label: &str, action: Action) -> Result<(), Error> {
        let item = MenuItem::new(label, self.editable, None);
        self.actions.insert(item.id().clone(), action);
        menu.append(&item)
    }

    /// A line that shows a value.
    fn info(&mut self, menu: &Submenu, text: &str) -> Result<(), Error> {
        menu.append(&MenuItem::new(menu_text(text), false, None))
    }

    fn submenu(&mut self, menu: &Submenu, title: &str) -> Result<Submenu, Error> {
        let sub = Submenu::new(title, true);
        menu.append(&sub)?;
        Ok(sub)
    }

    fn check(
        &mut self,
        menu: &Submenu,
        label: &str,
        path: SettingPath,
        get: fn(&Config) -> bool,
        on: bool,
    ) -> Result<(), Error> {
        let item = CheckMenuItem::new(label, self.editable, on, None);
        self.actions
            .insert(item.id().clone(), Action::Toggle(path, get));
        menu.append(&item)
    }

    /// A submenu titled "title: current" with one checked option. A click writes the file,
    /// and the menu is rebuilt from it, so the checkmarks always show the file.
    fn choice(&mut self, menu: &Submenu, title: &str, choices: Vec<Choice>) -> Result<(), Error> {
        let current = choices.iter().find(|c| c.checked).map(|c| c.label.as_str());
        let sub = Submenu::new(
            menu_text(&format!("{title}: {}", current.unwrap_or("?"))),
            true,
        );
        for c in choices {
            let clickable = self.editable && !c.changes.is_empty();
            let item = CheckMenuItem::new(menu_text(&c.label), clickable, c.checked, None);
            if clickable {
                self.actions
                    .insert(item.id().clone(), Action::Set(c.changes));
            }
            sub.append(&item)?;
        }
        menu.append(&sub)
    }
}

/// Free text (a device name, a path) as a menu item's text: a lone `&` would be taken as a
/// mnemonic.
fn menu_text(text: &str) -> String {
    text.replace('&', "&&")
}
