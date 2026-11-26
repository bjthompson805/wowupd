//! A window over wowupd: one row per game with its versions, a Play / Update / Install button, and a
//! progress bar with speed and time left while it works.

use gtk4 as gtk;
use gtk4::prelude::*;
use gtk4::{gio, glib};
use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use wowupd::install::{self, PRODUCTS, Product, ProductStatus, State};
use wowupd::launch::{self, Config};
use wowupd::report::{Event, Phase};

const APP_ID: &str = "io.github.bjthompson805.Wowupd";

const CSS: &str = "
.game-name { font-weight: bold; font-size: 1.15em; }
.dim { opacity: 0.7; }
.error { color: #e66; }
row.game { padding: 12px 14px; }
";

fn main() -> glib::ExitCode {
    let app = gtk::Application::builder().application_id(APP_ID).build();
    // Callbacks hold the window weakly; this keeps it.
    let shown: Rc<RefCell<Option<Rc<Window>>>> = Rc::default();
    app.connect_activate(move |app| {
        if let Some(w) = shown.borrow().as_ref() {
            w.window.present();
            return;
        }
        let provider = gtk::CssProvider::new();
        provider.load_from_string(CSS);
        if let Some(display) = gtk::gdk::Display::default() {
            gtk::style_context_add_provider_for_display(&display, &provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
        }
        *shown.borrow_mut() = Some(Window::build(app));
    });
    app.run_with_args::<&str>(&[])
}

struct Row {
    product: &'static Product,
    versions: gtk::Label,
    status: gtk::Label,
    button: gtk::Button,
    progress: gtk::ProgressBar,
    detail: gtk::Label,
    state: RefCell<Option<State>>,
}

struct Window {
    window: gtk::ApplicationWindow,
    folder: gtk::Label,
    rows: Vec<Rc<Row>>,
    config: RefCell<Config>,
    /// One update at a time: they share the store.
    busy: Cell<bool>,
}

impl Window {
    fn build(app: &gtk::Application) -> Rc<Window> {
        let config = launch::load().unwrap_or_default();
        let window = gtk::ApplicationWindow::builder().application(app).title("wowupd").default_width(620).default_height(560).build();
        let header = gtk::HeaderBar::new();
        let choose = gtk::Button::from_icon_name("folder-open-symbolic");
        choose.set_tooltip_text(Some("Choose the World of Warcraft folder"));
        header.pack_start(&choose);
        let refresh = gtk::Button::from_icon_name("view-refresh-symbolic");
        refresh.set_tooltip_text(Some("Check for updates"));
        header.pack_end(&refresh);
        window.set_titlebar(Some(&header));

        let column = gtk::Box::new(gtk::Orientation::Vertical, 8);
        column.set_margin_top(12);
        column.set_margin_bottom(12);
        column.set_margin_start(12);
        column.set_margin_end(12);
        let folder = gtk::Label::new(None);
        folder.set_xalign(0.0);
        folder.set_wrap(true);
        folder.add_css_class("dim");
        column.append(&folder);

        let list = gtk::ListBox::new();
        list.set_selection_mode(gtk::SelectionMode::None);
        list.add_css_class("boxed-list");
        let mut rows = Vec::new();
        for p in PRODUCTS {
            let (widget, row) = game_row(p);
            list.append(&widget);
            rows.push(row);
        }
        let scroll = gtk::ScrolledWindow::builder().child(&list).vexpand(true).hscrollbar_policy(gtk::PolicyType::Never).build();
        column.append(&scroll);
        window.set_child(Some(&column));

        let w = Rc::new(Window { window, folder, rows, config: RefCell::new(config), busy: Cell::new(false) });
        for row in &w.rows {
            let (w2, row2) = (Rc::downgrade(&w), row.clone());
            row.button.connect_clicked(move |_| {
                if let Some(w) = w2.upgrade() {
                    w.clicked(&row2);
                }
            });
        }
        let w2 = Rc::downgrade(&w);
        choose.connect_clicked(move |_| {
            if let Some(w) = w2.upgrade() {
                w.choose_folder();
            }
        });
        let w2 = Rc::downgrade(&w);
        refresh.connect_clicked(move |_| {
            if let Some(w) = w2.upgrade() {
                w.refresh();
            }
        });
        w.window.present();
        w.refresh();
        w
    }

    fn root(&self) -> Option<PathBuf> {
        self.config.borrow().dir.clone()
    }

    fn refresh(self: &Rc<Self>) {
        let Some(root) = self.root() else {
            self.folder.set_text("No World of Warcraft folder yet: choose one with the folder button.");
            for row in &self.rows {
                row.button.set_sensitive(false);
                row.status.set_text("");
            }
            return;
        };
        self.folder.set_text(&root.display().to_string());
        let (region, arch) = { let c = self.config.borrow(); (c.region(), c.arch()) };
        for row in &self.rows {
            row.status.set_text("checking…");
            row.status.remove_css_class("error");
            let (row, root, region, arch) = (row.clone(), root.clone(), region.clone(), arch.clone());
            let w = Rc::downgrade(self);
            glib::spawn_future_local(async move {
                let p = row.product;
                let s = gio::spawn_blocking(move || install::product_status(&root, p, &region, &arch)).await;
                if let (Ok(s), Some(w)) = (s, w.upgrade()) {
                    w.show_status(&row, s);
                }
            });
        }
    }

    fn show_status(&self, row: &Row, s: ProductStatus) {
        let live = s.live.as_deref().unwrap_or("?");
        row.versions.set_text(&match &s.installed {
            Some(v) => format!("Installed {v} · Live {live}"),
            None => format!("Not installed · Live {live}"),
        });
        let (status, button) = match &s.state {
            State::UpToDate => ("Up to date".to_owned(), "Play"),
            State::UpdateAvailable => ("Update available".to_owned(), "Update"),
            State::NotInstalled => ("".to_owned(), "Install"),
            State::Unknown(e) => (format!("Can't reach Blizzard: {e}"), "Play"),
        };
        row.status.set_text(&status);
        row.status.set_css_classes(if matches!(s.state, State::Unknown(_)) { &["error"] } else { &["dim"] });
        row.button.set_label(button);
        row.button.set_css_classes(if button == "Play" { &["suggested-action"] } else { &[] });
        row.button.set_sensitive(!self.busy.get() && (button != "Play" || s.installed.is_some()));
        *row.state.borrow_mut() = Some(s.state);
    }

    fn clicked(self: &Rc<Self>, row: &Rc<Row>) {
        let Some(root) = self.root() else { return };
        match row.state.borrow().clone() {
            Some(State::UpToDate) | Some(State::Unknown(_)) => self.play(&root, row),
            _ => self.update(root, row.clone(), false),
        }
    }

    fn play(self: &Rc<Self>, root: &Path, row: &Rc<Row>) {
        let cfg = self.config.borrow();
        let mut child = match launch::spawn(root, row.product, &cfg.arch(), &cfg) {
            Ok(c) => c,
            Err(e) => return show_detail(row, &format!("{e:#}"), true),
        };
        let name = row.product.name;
        show_detail(row, &format!("{name} is running"), false);
        // Reap the game on a thread (else it stays a zombie) and report its exit back here.
        let (tx, rx) = async_channel::bounded(1);
        std::thread::spawn(move || {
            let _ = tx.send_blocking(child.wait());
        });
        let row = row.clone();
        glib::spawn_future_local(async move {
            match rx.recv().await {
                Ok(Ok(st)) if st.success() => show_detail(&row, &format!("{name} exited"), false),
                Ok(Ok(st)) => show_detail(&row, &format!("{name} exited ({st})"), true),
                Ok(Err(e)) => show_detail(&row, &format!("lost track of {name}: {e}"), true),
                Err(_) => {}
            }
        });
    }

    /// Runs an update on a worker thread; its events come back over a channel. `then_play` starts the game after.
    fn update(self: &Rc<Self>, root: PathBuf, row: Rc<Row>, then_play: bool) {
        if self.busy.replace(true) {
            return;
        }
        for r in &self.rows {
            r.button.set_sensitive(false);
        }
        row.progress.set_visible(true);
        row.progress.set_fraction(0.0);
        show_detail(&row, "Checking…", false);
        let (tx, rx) = async_channel::unbounded::<Event>();
        let opts = {
            let c = self.config.borrow();
            install::Options {
                region: c.region(),
                locale: c.locale(),
                arch: c.arch(),
                jobs: 8,
                check_only: false,
                verify: false,
                report: Arc::new(move |e| {
                    let _ = tx.send_blocking(e);
                }),
            }
        };
        let p = row.product;
        let job = gio::spawn_blocking(move || install::update(&root, &[p], &opts).map_err(|e| format!("{e:#}")));
        let row2 = row.clone();
        glib::spawn_future_local(async move {
            while let Ok(e) = rx.recv().await {
                match e {
                    Event::Note(s) => show_detail(&row2, &s, false),
                    Event::Progress(pr) => {
                        row2.progress.set_fraction(pr.fraction());
                        let verb = match pr.phase {
                            Phase::Downloading => "Downloading",
                            Phase::Copying => "Copying the Battle.net data",
                        };
                        show_detail(&row2, &format!("{verb}: {}", pr.describe()), false);
                    }
                }
            }
        });
        let w = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let result = job.await.unwrap_or_else(|_| Err("the update stopped unexpectedly".to_owned()));
            let Some(w) = w.upgrade() else { return };
            w.busy.set(false);
            row.progress.set_visible(false);
            match result {
                Ok(_) => {
                    show_detail(&row, &format!("{} is ready", row.product.name), false);
                    if then_play {
                        if let Some(root) = w.root() {
                            w.play(&root, &row);
                        }
                    }
                }
                Err(e) => show_detail(&row, &e, true),
            }
            w.refresh();
        });
    }

    fn choose_folder(self: &Rc<Self>) {
        let dialog = gtk::FileDialog::builder().title("World of Warcraft folder").modal(true).build();
        if let Some(root) = self.root() {
            dialog.set_initial_folder(Some(&gio::File::for_path(root)));
        }
        let w = Rc::downgrade(self);
        dialog.select_folder(Some(&self.window), gio::Cancellable::NONE, move |res| {
            let (Some(w), Ok(file)) = (w.upgrade(), res) else { return };
            let Some(path) = file.path() else { return };
            if let Err(e) = launch::save_dir(&path) {
                w.folder.set_text(&format!("Couldn't save the folder: {e:#}"));
                return;
            }
            w.config.borrow_mut().dir = Some(path);
            w.refresh();
        });
    }
}

fn show_detail(row: &Row, text: &str, error: bool) {
    row.detail.set_visible(true);
    row.detail.set_text(text);
    row.detail.set_css_classes(if error { &["error"] } else { &["dim"] });
}

fn game_row(p: &'static Product) -> (gtk::ListBoxRow, Rc<Row>) {
    let outer = gtk::Box::new(gtk::Orientation::Vertical, 6);
    let line = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    let text = gtk::Box::new(gtk::Orientation::Vertical, 2);
    text.set_hexpand(true);
    let name = gtk::Label::new(Some(p.name));
    name.set_xalign(0.0);
    name.add_css_class("game-name");
    let versions = gtk::Label::new(Some("…"));
    versions.set_xalign(0.0);
    let status = gtk::Label::new(None);
    status.set_xalign(0.0);
    status.add_css_class("dim");
    text.append(&name);
    text.append(&versions);
    text.append(&status);
    let button = gtk::Button::with_label("…");
    button.set_valign(gtk::Align::Center);
    button.set_sensitive(false);
    line.append(&text);
    line.append(&button);
    outer.append(&line);
    let progress = gtk::ProgressBar::new();
    progress.set_visible(false);
    outer.append(&progress);
    let detail = gtk::Label::new(None);
    detail.set_xalign(0.0);
    detail.set_wrap(true);
    detail.set_visible(false);
    outer.append(&detail);
    let widget = gtk::ListBoxRow::new();
    widget.add_css_class("game");
    widget.set_activatable(false);
    widget.set_child(Some(&outer));
    let row = Rc::new(Row { product: p, versions, status, button, progress, detail, state: RefCell::new(None) });
    (widget, row)
}
