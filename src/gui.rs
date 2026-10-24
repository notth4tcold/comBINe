//! Graphical interface (egui). Mirrors CDmage's layout: track/folder tree on the
//! left, file list or sector view in the centre, error log at the bottom.

use crate::cd::{self, Form, RebuildFields, Sector};
use crate::image::Image;
use crate::iso9660::{Entry, Filesystem};
use crate::ops::{self, Progress, SaveMode, SaveOptions, ScanReport, TrackFormat};
use anyhow::Result;
use eframe::egui::{self, Color32, RichText, text::LayoutJob};
use egui_extras::{Column, TableBuilder};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, channel};

pub fn run(initial: Option<PathBuf>) -> Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("comBINe")
            .with_app_id("combine")
            .with_inner_size([1100.0, 720.0])
            .with_min_inner_size([720.0, 480.0])
            .with_drag_and_drop(true),
        ..Default::default()
    };
    eframe::run_native(
        "comBINe",
        options,
        Box::new(move |_cc| {
            let mut app = App::default();
            app.recent = load_recent();
            if let Some(p) = initial {
                app.open(&p);
            }
            Ok(Box::new(app))
        }),
    )
    .map_err(|e| anyhow::anyhow!("falha ao iniciar a interface: {e}"))
}

#[derive(PartialEq, Clone, Copy)]
enum View {
    Files,
    Sectors,
    Info,
}

#[derive(Clone, Copy, PartialEq)]
enum Level {
    Info,
    Warn,
    Error,
}

struct LogLine {
    level: Level,
    text: String,
    lba: Option<u32>,
}

struct JobDone {
    message: String,
    warnings: Vec<String>,
    scan: Option<ScanReport>,
    reload: bool,
}

struct Job {
    title: String,
    progress: Arc<Progress>,
    rx: Receiver<Result<JobDone>>,
}

struct SaveDialog {
    mode: SaveMode,
    out_dir: String,
    name: String,
    pattern: String,
    regenerate: bool,
}

struct RebuildDialog {
    fields: RebuildFields,
    only_errors: bool,
}

struct ImportDialog {
    entry_path: String,
    file: PathBuf,
    size: u64,
}

enum Action {
    Open(PathBuf),
    OpenDialog,
    Close,
    EnterDir(String),
    ViewSector(u32),
    ExtractEntries(Vec<String>),
    ExtractTrack(usize, TrackFormat),
    ImportOver(String),
    Scan,
}

#[derive(Default)]
struct App {
    img: Option<Arc<Image>>,
    fs: Option<Arc<Filesystem>>,
    fs_error: Option<String>,
    view: Option<View>,
    cur_dir: String,
    selected: BTreeSet<String>,
    sector_lba: u32,
    sector: Option<(u32, Result<Sector, String>)>,
    log: Vec<LogLine>,
    show_log: bool,
    scan: Option<ScanReport>,
    job: Option<Job>,
    recent: Vec<PathBuf>,
    save_dialog: Option<SaveDialog>,
    rebuild_dialog: Option<RebuildDialog>,
    import_dialog: Option<ImportDialog>,
    about: bool,
    status: String,
}

fn config_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config"))).unwrap_or_else(|| PathBuf::from(".")).join("combine")
}

fn load_recent() -> Vec<PathBuf> {
    std::fs::read_to_string(config_dir().join("recent")).map(|s| s.lines().filter(|l| !l.is_empty()).map(PathBuf::from).collect()).unwrap_or_default()
}

fn save_recent(list: &[PathBuf]) {
    let dir = config_dir();
    let _ = std::fs::create_dir_all(&dir);
    let text: Vec<String> = list.iter().map(|p| p.to_string_lossy().to_string()).collect();
    let _ = std::fs::write(dir.join("recent"), text.join("\n"));
}

fn human(bytes: u64) -> String {
    match bytes {
        b if b >= 1 << 30 => format!("{:.2} GiB", b as f64 / (1u64 << 30) as f64),
        b if b >= 1 << 20 => format!("{:.1} MiB", b as f64 / (1u64 << 20) as f64),
        b if b >= 1 << 10 => format!("{:.1} KiB", b as f64 / 1024.0),
        b => format!("{b} B"),
    }
}

fn image_filter(d: rfd::FileDialog) -> rfd::FileDialog {
    d.add_filter("Imagens de CD (cue, ccd, bin, iso, img, mm2)", &["cue", "CUE", "ccd", "CCD", "bin", "BIN", "iso", "ISO", "img", "IMG", "mm2", "MM2"]).add_filter("Todos os arquivos", &["*"])
}

/// Field colours taken from CDmage's defaults ([Colors] in CDmage.ini).
fn field_color(form: Option<Form>, off: usize, dark: bool) -> (Color32, &'static str) {
    let sync = Color32::from_rgb(128, 128, 128);
    let header = if dark { Color32::from_rgb(110, 140, 255) } else { Color32::from_rgb(0, 0, 255) };
    let sub = Color32::from_rgb(128, 128, 255);
    let edc = Color32::from_rgb(255, 128, 192);
    let eccp = Color32::from_rgb(255, 0, 255);
    let eccq = if dark { Color32::from_rgb(255, 60, 150) } else { Color32::from_rgb(255, 0, 128) };
    let data = if dark { Color32::from_gray(220) } else { Color32::from_gray(20) };
    let Some(form) = form else { return (data, "Áudio") };
    match (form, off) {
        (_, 0..12) => (sync, "Sync"),
        (_, 12..16) => (header, "Cabeçalho"),
        (Form::Mode1, 16..2064) => (data, "Dados"),
        (Form::Mode1, 2064..2068) => (edc, "EDC"),
        (Form::Mode1, 2068..2076) => (sync, "Intermediário"),
        (Form::Mode2Form1 | Form::Mode2Form2, 16..24) => (sub, "Subheader"),
        (Form::Mode2Form1, 24..2072) => (data, "Dados"),
        (Form::Mode2Form1, 2072..2076) => (edc, "EDC"),
        (Form::Mode2Form2, 24..2348) => (data, "Dados"),
        (Form::Mode2Form2, 2348..2352) => (edc, "EDC"),
        (Form::Mode1 | Form::Mode2Form1, 2076..2248) => (eccp, "ECC P"),
        (Form::Mode1 | Form::Mode2Form1, 2248..2352) => (eccq, "ECC Q"),
        _ => (data, "Dados"),
    }
}

impl App {
    fn log(&mut self, level: Level, text: impl Into<String>, lba: Option<u32>) {
        let text = text.into();
        if level != Level::Info {
            self.show_log = true;
        }
        self.status = text.clone();
        self.log.push(LogLine { level, text, lba });
    }

    fn open(&mut self, path: &Path) {
        match Image::open(path) {
            Ok(img) => {
                let path = img.path.clone();
                let (fs, fs_error) = match Filesystem::read(&img) {
                    Ok(fs) => (Some(Arc::new(fs)), None),
                    Err(e) => (None, Some(format!("{e:#}"))),
                };
                self.log(Level::Info, format!("Aberto: {} ({} trilha(s), {} setores)", path.display(), img.tracks.len(), img.total_sectors()), None);
                if let Some(e) = &fs_error {
                    self.log(Level::Warn, format!("Sistema de arquivos: {e}"), None);
                }
                self.img = Some(Arc::new(img));
                self.view = Some(if fs.is_some() { View::Files } else { View::Info });
                self.fs = fs;
                self.fs_error = fs_error;
                self.cur_dir.clear();
                self.selected.clear();
                self.scan = None;
                self.sector = None;
                self.sector_lba = 0;
                self.recent.retain(|p| p != &path);
                self.recent.insert(0, path);
                self.recent.truncate(10);
                save_recent(&self.recent);
            }
            Err(e) => self.log(Level::Error, format!("Não foi possível abrir {}: {e:#}", path.display()), None),
        }
    }

    fn reopen(&mut self) {
        if let Some(img) = &self.img {
            let p = img.path.clone();
            let (dir, view, lba) = (self.cur_dir.clone(), self.view, self.sector_lba);
            self.open(&p);
            self.cur_dir = dir;
            self.view = view;
            self.sector_lba = lba;
        }
    }

    fn spawn(&mut self, title: &str, f: impl FnOnce(&Image, Option<&Filesystem>, &Progress) -> Result<JobDone> + Send + 'static) {
        let Some(img) = self.img.clone() else { return };
        let fs = self.fs.clone();
        let progress = Arc::new(Progress::default());
        let (tx, rx) = channel();
        let p = progress.clone();
        std::thread::spawn(move || {
            let _ = tx.send(f(&img, fs.as_deref(), &p));
        });
        self.job = Some(Job { title: title.into(), progress, rx });
    }

    fn poll_job(&mut self) {
        let Some(job) = &self.job else { return };
        let Ok(res) = job.rx.try_recv() else { return };
        let title = job.title.clone();
        self.job = None;
        match res {
            Ok(done) => {
                for w in done.warnings {
                    self.log(Level::Warn, w, None);
                }
                if let Some(rep) = done.scan {
                    for t in &rep.skipped_tracks {
                        self.log(Level::Info, format!("Trilha {t:02}: setores de 2048/2336 bytes não têm EDC/ECC para verificar"), None);
                    }
                    for e in rep.errors.iter().take(5000) {
                        self.log(Level::Error, format!("Setor {} ({}) trilha {:02}, {}: {}", e.lba, cd::msf_string(e.lba + cd::LBA_OFFSET), e.track, e.form.name(), e.problems.describe()), Some(e.lba));
                    }
                    if rep.errors.len() > 5000 {
                        self.log(Level::Error, format!("... e mais {} setores com erro", rep.errors.len() - 5000), None);
                    }
                    self.scan = Some(rep);
                }
                if done.reload {
                    self.reopen();
                }
                self.log(Level::Info, done.message, None);
            }
            Err(e) => self.log(Level::Error, format!("{title}: {e:#}"), None),
        }
    }

    fn entries_in_dir(&self) -> Vec<Entry> {
        let Some(fs) = &self.fs else { return vec![] };
        fs.root.find(&self.cur_dir).map(|d| d.children.clone()).unwrap_or_default()
    }

    fn child_path(&self, name: &str) -> String {
        if self.cur_dir.is_empty() { name.to_string() } else { format!("{}/{name}", self.cur_dir) }
    }

    fn apply(&mut self, a: Action) {
        match a {
            Action::Open(p) => self.open(&p),
            Action::OpenDialog => {
                let mut d = image_filter(rfd::FileDialog::new().set_title("Abrir imagem de CD"));
                if let Some(dir) = self.recent.first().and_then(|p| p.parent()) {
                    d = d.set_directory(dir);
                }
                if let Some(p) = d.pick_file() {
                    self.open(&p);
                }
            }
            Action::Close => {
                *self = App { recent: std::mem::take(&mut self.recent), log: std::mem::take(&mut self.log), ..Default::default() };
            }
            Action::EnterDir(d) => {
                self.cur_dir = d;
                self.selected.clear();
                self.view = Some(View::Files);
            }
            Action::ViewSector(lba) => {
                self.sector_lba = lba;
                self.view = Some(View::Sectors);
            }
            Action::Scan => self.spawn("Verificação", |img, _, p| {
                let rep = ops::scan(img, p)?;
                let message = if rep.errors.is_empty() {
                    format!("Verificação concluída: {} setores, nenhum erro encontrado", rep.scanned)
                } else {
                    format!("Verificação concluída: {} setores, {} com erro", rep.scanned, rep.errors.len())
                };
                Ok(JobDone { message, warnings: vec![], scan: Some(rep), reload: false })
            }),
            Action::ExtractEntries(paths) => {
                let Some(dir) = rfd::FileDialog::new().set_title("Extrair para a pasta").pick_folder() else { return };
                self.spawn("Extração", move |img, fs, p| {
                    let fs = fs.ok_or_else(|| anyhow::anyhow!("sem sistema de arquivos"))?;
                    let entries: Vec<&Entry> = paths.iter().filter_map(|x| fs.root.find(x)).collect();
                    let warnings = ops::extract_entries(img, fs, &entries, &dir, false, p)?;
                    Ok(JobDone { message: format!("Extraído em {}", dir.display()), warnings, scan: None, reload: false })
                });
            }
            Action::ExtractTrack(ti, fmt) => {
                let Some(img) = &self.img else { return };
                let ext = match fmt {
                    TrackFormat::Raw => "bin",
                    TrackFormat::Iso => "iso",
                    TrackFormat::Wav => "wav",
                };
                let name = format!("{} (Track {:02}).{ext}", img.name(), img.tracks[ti].number);
                let Some(out) = rfd::FileDialog::new().set_title("Salvar trilha").set_file_name(&name).save_file() else { return };
                self.spawn("Extração de trilha", move |img, _, p| {
                    ops::extract_track(img, ti, fmt, &out, p)?;
                    Ok(JobDone { message: format!("Trilha salva em {}", out.display()), warnings: vec![], scan: None, reload: false })
                });
            }
            Action::ImportOver(entry_path) => {
                if let Some(file) = rfd::FileDialog::new().set_title(format!("Arquivo para importar sobre {entry_path}")).pick_file() {
                    let size = std::fs::metadata(&file).map(|m| m.len()).unwrap_or(0);
                    self.import_dialog = Some(ImportDialog { entry_path, file, size });
                }
            }
        }
    }

    fn toolbar(&mut self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        let has = self.img.is_some();
        let busy = self.job.is_some();
        ui.horizontal(|ui| {
            ui.spacing_mut().button_padding = egui::vec2(10.0, 6.0);
            if ui.button("📂 Abrir").on_hover_text("Abrir .cue, .ccd, .bin, .iso, .img").clicked() {
                actions.push(Action::OpenDialog);
            }
            ui.separator();
            ui.add_enabled_ui(has && !busy, |ui| {
                if ui.button("💾 Juntar em um BIN").on_hover_text("Salva todas as trilhas em um único .bin + .cue (PSIO, emuladores)").clicked() {
                    self.open_save_dialog(SaveMode::Merge);
                }
                if ui.button("✂ Separar trilhas").on_hover_text("Um .bin por trilha + .cue").clicked() {
                    self.open_save_dialog(SaveMode::Split);
                }
                ui.separator();
                if ui.button("🔍 Verificar").on_hover_text("Procurar setores corrompidos (EDC/ECC)").clicked() {
                    actions.push(Action::Scan);
                }
                if ui.button("🛠 Reconstruir").on_hover_text("Regenerar campos dos setores (modifica a imagem)").clicked() {
                    self.rebuild_dialog = Some(RebuildDialog { fields: RebuildFields::default(), only_errors: self.scan.as_ref().is_some_and(|s| !s.errors.is_empty()) });
                }
                ui.separator();
                if ui.add_enabled(self.fs.is_some(), egui::Button::new("📤 Extrair tudo")).clicked() {
                    actions.push(Action::ExtractEntries(vec![String::new()]));
                }
            });
        });
    }

    fn open_save_dialog(&mut self, mode: SaveMode) {
        let Some(img) = &self.img else { return };
        let dir = img.path.parent().map(|p| p.join(if mode == SaveMode::Merge { "comBINe" } else { "faixas" })).unwrap_or_default();
        self.save_dialog = Some(SaveDialog { mode, out_dir: dir.to_string_lossy().to_string(), name: img.name(), pattern: "?i (Track ?n).?e".into(), regenerate: false });
    }

    fn menu(&mut self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        egui::MenuBar::new().ui(ui, |ui| {
            ui.menu_button("Arquivo", |ui| {
                if ui.button("📂 Abrir...").clicked() {
                    actions.push(Action::OpenDialog);
                }
                ui.menu_button("Recentes", |ui| {
                    if self.recent.is_empty() {
                        ui.label("(nenhum)");
                    }
                    for p in self.recent.clone() {
                        if ui.button(p.file_name().unwrap_or_default().to_string_lossy()).on_hover_text(p.to_string_lossy()).clicked() {
                            actions.push(Action::Open(p));
                        }
                    }
                });
                if ui.add_enabled(self.img.is_some(), egui::Button::new("Fechar imagem")).clicked() {
                    actions.push(Action::Close);
                }
                ui.separator();
                if ui.button("Sair").clicked() {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
            ui.add_enabled_ui(self.img.is_some() && self.job.is_none(), |ui| {
                ui.menu_button("Imagem", |ui| {
                    if ui.button("💾 Salvar como BIN único...").clicked() {
                        self.open_save_dialog(SaveMode::Merge);
                    }
                    if ui.button("✂ Salvar com trilhas separadas...").clicked() {
                        self.open_save_dialog(SaveMode::Split);
                    }
                    ui.separator();
                    if ui.button("🔍 Verificar corrupção").clicked() {
                        actions.push(Action::Scan);
                    }
                    if ui.button("🛠 Reconstruir campos dos setores...").clicked() {
                        self.rebuild_dialog = Some(RebuildDialog { fields: RebuildFields::default(), only_errors: self.scan.as_ref().is_some_and(|s| !s.errors.is_empty()) });
                    }
                    ui.separator();
                    if ui.add_enabled(self.fs.is_some(), egui::Button::new("📤 Extrair todos os arquivos...")).clicked() {
                        actions.push(Action::ExtractEntries(vec![String::new()]));
                    }
                    if ui.button("Salvar imagem de boot (16 setores)...").clicked() {
                        if let Some(img) = &self.img {
                            if let Some(out) = rfd::FileDialog::new().set_file_name(format!("{}_boot.img", img.name())).save_file() {
                                match ops::save_boot(img, &out) {
                                    Ok(()) => self.log(Level::Info, format!("Boot salvo em {}", out.display()), None),
                                    Err(e) => self.log(Level::Error, format!("{e:#}"), None),
                                }
                            }
                        }
                    }
                });
            });
            ui.menu_button("Exibir", |ui| {
                for (v, label) in [(View::Files, "Arquivos"), (View::Sectors, "Setores"), (View::Info, "Informações")] {
                    if ui.add_enabled(self.img.is_some(), egui::Button::selectable(self.view == Some(v), label)).clicked() {
                        self.view = Some(v);
                    }
                }
                ui.checkbox(&mut self.show_log, "Log de erros");
            });
            ui.menu_button("Ajuda", |ui| {
                if ui.button("Sobre o comBINe").clicked() {
                    self.about = true;
                }
            });
        });
    }

    fn tree(&mut self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        let Some(img) = self.img.clone() else { return };
        ui.heading(img.name());
        ui.label(RichText::new(format!("{} trilha(s) · {}", img.tracks.len(), cd::msf_string(img.total_sectors()))).weak());
        ui.separator();
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            egui::CollapsingHeader::new("💿 Trilhas").default_open(true).show(ui, |ui| {
                for (ti, t) in img.tracks.iter().enumerate() {
                    let icon = if t.is_data() { "🗄" } else { "🎵" };
                    let r = ui.selectable_label(false, format!("{icon} Trilha {:02} · {}", t.number, t.mode_text));
                    if r.clicked() {
                        actions.push(Action::ViewSector(t.start_lba()));
                    }
                    r.on_hover_text(format!("Início {} · {} setores · clique para ver os setores", cd::msf_string(t.start_lba()), t.region_sectors)).context_menu(|ui| {
                        if ui.button("Extrair como BIN (2352)...").clicked() {
                            actions.push(Action::ExtractTrack(ti, TrackFormat::Raw));
                            ui.close();
                        }
                        if t.is_data() && ui.button("Extrair como ISO (2048)...").clicked() {
                            actions.push(Action::ExtractTrack(ti, TrackFormat::Iso));
                            ui.close();
                        }
                        if !t.is_data() && ui.button("Extrair como WAV...").clicked() {
                            actions.push(Action::ExtractTrack(ti, TrackFormat::Wav));
                            ui.close();
                        }
                    });
                }
            });
            if let Some(fs) = self.fs.clone() {
                let label = if fs.info.volume_id.is_empty() { "📁 Sistema de arquivos".to_string() } else { format!("📁 {}", fs.info.volume_id) };
                let root = egui::CollapsingHeader::new(label).id_salt("fsroot").default_open(true).show(ui, |ui| self.dir_tree(ui, &fs.root, "", actions));
                if root.header_response.clicked() {
                    actions.push(Action::EnterDir(String::new()));
                }
            } else if let Some(e) = &self.fs_error {
                ui.label(RichText::new(format!("Sem ISO9660: {e}")).weak());
            }
        });
    }

    fn dir_tree(&self, ui: &mut egui::Ui, dir: &Entry, prefix: &str, actions: &mut Vec<Action>) {
        for c in dir.children.iter().filter(|c| c.is_dir) {
            let path = if prefix.is_empty() { c.name.clone() } else { format!("{prefix}/{}", c.name) };
            let selected = self.cur_dir == path;
            if c.children.iter().any(|g| g.is_dir) {
                let r = egui::CollapsingHeader::new(RichText::new(format!("📁 {}", c.name)).strong().color(if selected { ui.visuals().selection.stroke.color } else { ui.visuals().text_color() }))
                    .id_salt(&path)
                    .show(ui, |ui| self.dir_tree(ui, c, &path, actions));
                if r.header_response.clicked() {
                    actions.push(Action::EnterDir(path));
                }
            } else if ui.selectable_label(selected, format!("📁 {}", c.name)).clicked() {
                actions.push(Action::EnterDir(path));
            }
        }
    }

    fn files_view(&mut self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        if self.fs.is_none() {
            ui.label(self.fs_error.clone().unwrap_or_else(|| "Sem sistema de arquivos.".into()));
            return;
        }
        let entries = self.entries_in_dir();
        ui.horizontal(|ui| {
            if ui.add_enabled(!self.cur_dir.is_empty(), egui::Button::new("⬆ Acima")).clicked() {
                let up = self.cur_dir.rsplit_once('/').map(|x| x.0.to_string()).unwrap_or_default();
                actions.push(Action::EnterDir(up));
            }
            ui.label(RichText::new(format!("/{}", self.cur_dir)).monospace());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let sel: Vec<String> = self.selected.iter().cloned().collect();
                let one = (sel.len() == 1).then(|| sel[0].clone());
                let import_ok = one.as_ref().and_then(|p| self.fs.as_ref()?.root.find(p).cloned()).is_some_and(|e| !e.is_dir && !e.is_stream());
                if ui.add_enabled(import_ok, egui::Button::new("📥 Importar sobre...")).on_hover_text("Substitui o conteúdo do arquivo selecionado (precisa caber no espaço original)").clicked() {
                    actions.push(Action::ImportOver(one.clone().unwrap()));
                }
                if ui.add_enabled(!sel.is_empty(), egui::Button::new(format!("📤 Extrair selecionados ({})", sel.len()))).clicked() {
                    actions.push(Action::ExtractEntries(sel));
                }
            });
        });
        ui.separator();
        let text_h = ui.text_style_height(&egui::TextStyle::Body) + 4.0;
        TableBuilder::new(ui)
            .striped(true)
            .sense(egui::Sense::click())
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .column(Column::remainder().at_least(180.0).clip(true))
            .column(Column::auto().at_least(80.0))
            .column(Column::auto().at_least(60.0))
            .column(Column::auto().at_least(140.0))
            .column(Column::auto().at_least(120.0))
            .header(text_h, |mut h| {
                for t in ["Nome", "Tamanho", "LBA", "Data", "Tipo"] {
                    h.col(|ui| {
                        ui.strong(t);
                    });
                }
            })
            .body(|body| {
                body.rows(text_h, entries.len(), |mut row| {
                    let e = &entries[row.index()];
                    let path = self.child_path(&e.name);
                    row.set_selected(self.selected.contains(&path));
                    row.col(|ui| {
                        ui.label(format!("{} {}", if e.is_dir { "📁" } else { "📄" }, e.name));
                    });
                    row.col(|ui| {
                        ui.label(if e.is_dir { String::new() } else { human(e.size as u64) });
                    });
                    row.col(|ui| {
                        ui.label(e.lba.to_string());
                    });
                    row.col(|ui| {
                        ui.label(e.date_string());
                    });
                    row.col(|ui| {
                        ui.label(e.kind_string());
                    });
                    let r = row.response();
                    if r.double_clicked() {
                        if e.is_dir {
                            actions.push(Action::EnterDir(path.clone()));
                        } else {
                            actions.push(Action::ViewSector(e.lba));
                        }
                    } else if r.clicked() {
                        let ctrl = r.ctx.input(|i| i.modifiers.command || i.modifiers.shift);
                        if !ctrl {
                            self.selected.clear();
                        }
                        if !self.selected.remove(&path) || !ctrl {
                            self.selected.insert(path.clone());
                        }
                    }
                    r.context_menu(|ui| {
                        if ui.button("📤 Extrair...").clicked() {
                            actions.push(Action::ExtractEntries(vec![path.clone()]));
                            ui.close();
                        }
                        if ui.button("🔢 Ver setores").clicked() {
                            actions.push(Action::ViewSector(e.lba));
                            ui.close();
                        }
                        if !e.is_dir && !e.is_stream() && ui.button("📥 Importar sobre...").clicked() {
                            actions.push(Action::ImportOver(path.clone()));
                            ui.close();
                        }
                    });
                });
            });
    }

    fn sectors_view(&mut self, ui: &mut egui::Ui) {
        let Some(img) = self.img.clone() else { return };
        let total = img.total_sectors().max(1);
        ui.horizontal(|ui| {
            if ui.button("⏮").clicked() {
                self.sector_lba = 0;
            }
            if ui.button("◀").clicked() {
                self.sector_lba = self.sector_lba.saturating_sub(1);
            }
            ui.label("Setor (LBA):");
            ui.add(egui::DragValue::new(&mut self.sector_lba).range(0..=total - 1));
            if ui.button("▶").clicked() {
                self.sector_lba = (self.sector_lba + 1).min(total - 1);
            }
            if ui.button("⏭").clicked() {
                self.sector_lba = total - 1;
            }
            ui.label(RichText::new(format!("MSF {}  ·  de {}", cd::msf_string(self.sector_lba + cd::LBA_OFFSET), total)).weak());
            if let Some(scan) = &self.scan {
                if !scan.errors.is_empty() {
                    ui.separator();
                    if ui.button("Próximo erro ⏩").clicked() {
                        if let Some(e) = scan.errors.iter().find(|e| e.lba > self.sector_lba).or(scan.errors.first()) {
                            self.sector_lba = e.lba;
                        }
                    }
                }
            }
        });
        if ui.input(|i| i.key_pressed(egui::Key::PageDown)) {
            self.sector_lba = (self.sector_lba + 1).min(total - 1);
        }
        if ui.input(|i| i.key_pressed(egui::Key::PageUp)) {
            self.sector_lba = self.sector_lba.saturating_sub(1);
        }
        if self.sector.as_ref().map(|s| s.0) != Some(self.sector_lba) {
            self.sector = Some((self.sector_lba, img.read_lba(self.sector_lba).map_err(|e| format!("{e:#}"))));
        }
        let Some((lba, res)) = &self.sector else { return };
        let s = match res {
            Ok(s) => *s,
            Err(e) => {
                ui.colored_label(Color32::RED, e);
                return;
            }
        };
        let ti = img.track_at(*lba);
        let tr = ti.map(|i| &img.tracks[i]);
        let is_data = tr.is_some_and(|t| t.is_data());
        let form = is_data.then(|| cd::detect_form(&s));
        ui.horizontal_wrapped(|ui| {
            if let Some(t) = tr {
                ui.label(format!("Trilha {:02} ({})", t.number, t.mode_text));
                if t.index(1).is_some() && *lba < t.start_lba() {
                    ui.label(RichText::new("pregap (INDEX 00)").weak());
                }
            }
            if let Some(f) = form {
                ui.separator();
                ui.label(f.name());
                if s[15] == 2 {
                    let sh = &s[16..20];
                    ui.label(RichText::new(format!("Subheader: arq {} canal {} submodo {:02X} cod {:02X}", sh[0], sh[1], sh[2], sh[3])).monospace());
                }
                ui.separator();
                if tr.is_some_and(|t| t.sector_size < cd::RAW) {
                    ui.label(RichText::new("setor sintetizado a partir de dados de 2048/2336 bytes").weak());
                } else {
                    let (_, p) = cd::check(&s, Some(*lba), tr.and_then(|t| t.mode_byte()));
                    if p.any() {
                        ui.colored_label(Color32::from_rgb(230, 70, 70), format!("✖ Erros: {}", p.describe()));
                    } else {
                        ui.colored_label(Color32::from_rgb(60, 170, 80), "✔ Setor íntegro");
                    }
                }
            }
        });
        let dark = ui.visuals().dark_mode;
        ui.horizontal_wrapped(|ui| {
            let mut seen = Vec::new();
            for off in 0..cd::RAW {
                let (c, name) = field_color(form, off, dark);
                if !seen.contains(&name) {
                    seen.push(name);
                    ui.label(RichText::new(format!("■ {name}")).color(c).small());
                }
            }
        });
        ui.separator();
        let font = egui::FontId::monospace(13.0);
        let addr = ui.visuals().weak_text_color();
        let row_h = ui.fonts_mut(|f| f.row_height(&font));
        egui::ScrollArea::vertical().auto_shrink(false).show_rows(ui, row_h, cd::RAW / 16, |ui, rows| {
            for r in rows {
                let mut job = LayoutJob::default();
                let fmt = |c: Color32| egui::TextFormat { font_id: font.clone(), color: c, ..Default::default() };
                job.append(&format!("{:04X}  ", r * 16), 0.0, fmt(addr));
                for i in 0..16 {
                    let off = r * 16 + i;
                    job.append(&format!("{:02X} ", s[off]), 0.0, fmt(field_color(form, off, dark).0));
                    if i == 7 {
                        job.append(" ", 0.0, fmt(addr));
                    }
                }
                job.append(" ", 0.0, fmt(addr));
                for i in 0..16 {
                    let off = r * 16 + i;
                    let b = s[off];
                    let ch = if (0x20..0x7F).contains(&b) { b as char } else { '.' };
                    job.append(&ch.to_string(), 0.0, fmt(field_color(form, off, dark).0));
                }
                ui.label(job);
            }
        });
    }

    fn info_view(&mut self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        let Some(img) = self.img.clone() else { return };
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            ui.heading("Imagem");
            egui::Grid::new("imginfo").num_columns(2).striped(true).show(ui, |ui| {
                ui.label("Arquivo");
                ui.label(img.path.to_string_lossy());
                ui.end_row();
                ui.label("Formato");
                ui.label(match img.format {
                    crate::image::ImageFormat::Cue => "CUE sheet",
                    crate::image::ImageFormat::CloneCd => "CloneCD (.ccd/.img)",
                    crate::image::ImageFormat::Raw => "Imagem bruta",
                });
                ui.end_row();
                ui.label("Duração");
                ui.label(format!("{} ({} setores)", cd::msf_string(img.total_sectors()), img.total_sectors()));
                ui.end_row();
                ui.label("Arquivos de dados");
                ui.label(img.files.len().to_string());
                ui.end_row();
            });
            if let Some(fs) = &self.fs {
                ui.add_space(8.0);
                ui.heading("Volume ISO9660");
                let i = &fs.info;
                egui::Grid::new("volinfo").num_columns(2).striped(true).show(ui, |ui| {
                    for (k, v) in [
                        ("ID do jogo (boot)", i.boot.clone().unwrap_or_default()),
                        ("Licença", i.license.clone().unwrap_or_default()),
                        ("Volume", i.volume_id.clone()),
                        ("Sistema", i.system_id.clone()),
                        ("Conjunto de volumes", i.volume_set_id.clone()),
                        ("Editora", i.publisher_id.clone()),
                        ("Preparado por", i.preparer_id.clone()),
                        ("Aplicação", i.application_id.clone()),
                        ("Criado em", i.creation_date.clone()),
                        ("Setores do volume", i.volume_sectors.to_string()),
                    ] {
                        if !v.is_empty() {
                            ui.label(k);
                            ui.label(v);
                            ui.end_row();
                        }
                    }
                });
            }
            ui.add_space(8.0);
            ui.heading("Trilhas");
            egui::Grid::new("tracks").num_columns(7).striped(true).spacing([16.0, 4.0]).show(ui, |ui| {
                for h in ["Nº", "Modo", "Início", "Pregap", "Setores", "Arquivo", ""] {
                    ui.strong(h);
                }
                ui.end_row();
                for (ti, t) in img.tracks.iter().enumerate() {
                    ui.label(format!("{:02}", t.number));
                    ui.label(&t.mode_text);
                    ui.label(cd::msf_string(t.start_lba()));
                    let gap = t.start_lba() - t.region_lba + t.pregap;
                    ui.label(if gap > 0 { cd::msf_string(gap) } else { "-".into() });
                    ui.label(t.region_sectors.to_string());
                    ui.label(img.files[t.file].path.file_name().unwrap_or_default().to_string_lossy());
                    ui.horizontal(|ui| {
                        if ui.small_button("BIN").on_hover_text("Extrair trilha bruta (2352)").clicked() {
                            actions.push(Action::ExtractTrack(ti, TrackFormat::Raw));
                        }
                        if t.is_data() {
                            if ui.small_button("ISO").on_hover_text("Extrair dados (2048)").clicked() {
                                actions.push(Action::ExtractTrack(ti, TrackFormat::Iso));
                            }
                        } else if ui.small_button("WAV").clicked() {
                            actions.push(Action::ExtractTrack(ti, TrackFormat::Wav));
                        }
                    });
                    ui.end_row();
                }
            });
        });
    }

    fn dialogs(&mut self, ctx: &egui::Context) {
        if let Some(d) = &mut self.save_dialog {
            let mut open = true;
            let mut go = false;
            let title = if d.mode == SaveMode::Merge { "Salvar como BIN único" } else { "Salvar com trilhas separadas" };
            egui::Window::new(title).open(&mut open).collapsible(false).resizable(false).anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0]).show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.radio_value(&mut d.mode, SaveMode::Merge, "Um único .bin");
                    ui.radio_value(&mut d.mode, SaveMode::Split, "Um .bin por trilha");
                });
                egui::Grid::new("savegrid").num_columns(2).show(ui, |ui| {
                    ui.label("Pasta de destino");
                    ui.horizontal(|ui| {
                        ui.add(egui::TextEdit::singleline(&mut d.out_dir).desired_width(320.0));
                        if ui.button("Escolher...").clicked() {
                            if let Some(p) = rfd::FileDialog::new().set_directory(&d.out_dir).pick_folder() {
                                d.out_dir = p.to_string_lossy().to_string();
                            }
                        }
                    });
                    ui.end_row();
                    ui.label("Nome");
                    ui.text_edit_singleline(&mut d.name);
                    ui.end_row();
                    if d.mode == SaveMode::Split {
                        ui.label("Padrão de nome");
                        ui.text_edit_singleline(&mut d.pattern).on_hover_text("?i = nome da imagem, ?n = número da trilha, ?e = extensão");
                        ui.end_row();
                    }
                });
                ui.checkbox(&mut d.regenerate, "Regenerar EDC/ECC dos setores de dados na cópia (corrige setores corrompidos)");
                let o = SaveOptions { mode: d.mode, out_dir: PathBuf::from(&d.out_dir), name: d.name.clone(), pattern: d.pattern.clone(), regenerate: d.regenerate, pad: self.img.as_ref().is_some_and(|i| i.tracks.len() >= 10) };
                if let Some(img) = &self.img {
                    ui.add_space(4.0);
                    ui.label(RichText::new("Será criado:").weak());
                    ui.label(RichText::new(format!("  {}.cue", o.name)).monospace());
                    if d.mode == SaveMode::Merge {
                        ui.label(RichText::new(format!("  {}", o.track_file_name(0, true))).monospace());
                    } else {
                        for t in img.tracks.iter().take(6) {
                            ui.label(RichText::new(format!("  {}", o.track_file_name(t.number, false))).monospace());
                        }
                        if img.tracks.len() > 6 {
                            ui.label(RichText::new(format!("  ... (+{})", img.tracks.len() - 6)).monospace());
                        }
                    }
                    let cooked = img.tracks.iter().any(|t| t.sector_size != cd::RAW);
                    if cooked {
                        ui.label(RichText::new("Trilhas de 2048/2336/2448 bytes serão convertidas para 2352 bytes (raw).").weak());
                    }
                }
                ui.add_space(6.0);
                ui.add_enabled_ui(!d.name.trim().is_empty() && !d.out_dir.trim().is_empty(), |ui| {
                    if ui.button("💾 Salvar").clicked() {
                        go = true;
                    }
                });
            });
            if go {
                let d = self.save_dialog.take().unwrap();
                let o = SaveOptions { mode: d.mode, out_dir: PathBuf::from(d.out_dir), name: d.name.trim().to_string(), pattern: d.pattern, regenerate: d.regenerate, pad: self.img.as_ref().is_some_and(|i| i.tracks.len() >= 10) };
                self.spawn("Salvar", move |img, _, p| {
                    let cue = ops::save(img, &o, p)?;
                    Ok(JobDone { message: format!("Imagem salva: {}", cue.display()), warnings: vec![], scan: None, reload: false })
                });
            } else if !open {
                self.save_dialog = None;
            }
        }

        if let Some(d) = &mut self.rebuild_dialog {
            let mut open = true;
            let mut go = false;
            let n_err = self.scan.as_ref().map(|s| s.errors.len());
            egui::Window::new("Reconstruir campos dos setores").open(&mut open).collapsible(false).resizable(false).anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0]).show(ctx, |ui| {
                ui.label("Campos a regenerar:");
                ui.checkbox(&mut d.fields.sync, "Sync");
                ui.checkbox(&mut d.fields.address, "Endereço do cabeçalho (MSF)");
                ui.checkbox(&mut d.fields.subheader, "Subheader (Mode 2: iguala as duas cópias)");
                ui.checkbox(&mut d.fields.edc, "EDC");
                ui.checkbox(&mut d.fields.ecc, "ECC P e Q");
                ui.separator();
                ui.add_enabled_ui(n_err.is_some_and(|n| n > 0), |ui| {
                    ui.radio_value(&mut d.only_errors, true, format!("Somente setores com erro ({})", n_err.unwrap_or(0)));
                });
                ui.radio_value(&mut d.only_errors, false, "Todos os setores de dados");
                if n_err.is_none() {
                    ui.label(RichText::new("Dica: rode a Verificação antes para reconstruir só os setores com erro.").weak());
                }
                ui.add_space(6.0);
                ui.colored_label(Color32::from_rgb(220, 150, 40), "⚠ Isto modifica a imagem original. Faça um backup antes,\nou use \"Juntar em um BIN\" com a opção de regenerar EDC/ECC para gerar uma cópia.");
                ui.add_space(6.0);
                if ui.button("🛠 Reconstruir").clicked() {
                    go = true;
                }
            });
            if go {
                let d = self.rebuild_dialog.take().unwrap();
                let lbas: Option<Vec<u32>> = d.only_errors.then(|| self.scan.as_ref().map(|s| s.errors.iter().map(|e| e.lba).collect()).unwrap_or_default());
                let fields = d.fields;
                self.spawn("Reconstrução", move |img, _, p| {
                    let lbas = lbas.unwrap_or_else(|| ops::all_data_lbas(img));
                    let n = ops::rebuild_in_place(img, &lbas, fields, p)?;
                    Ok(JobDone { message: format!("{n} setor(es) reconstruído(s). Rode a verificação novamente para confirmar."), warnings: vec![], scan: None, reload: true })
                });
            } else if !open {
                self.rebuild_dialog = None;
            }
        }

        if let Some(d) = &self.import_dialog {
            let mut open = true;
            let mut go = false;
            let entry = self.fs.as_ref().and_then(|fs| fs.root.find(&d.entry_path).cloned());
            egui::Window::new("Importar arquivo").open(&mut open).collapsible(false).resizable(false).anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0]).show(ctx, |ui| {
                let Some(e) = &entry else {
                    ui.label("Arquivo não encontrado no disco.");
                    return;
                };
                let cap = e.sectors() as u64 * 2048;
                ui.label(format!("Substituir {} ({}) por:", d.entry_path, human(e.size as u64)));
                ui.label(RichText::new(format!("{} ({})", d.file.display(), human(d.size))).monospace());
                ui.label(format!("Espaço disponível: {} ({} setores)", human(cap), e.sectors()));
                if d.size > cap {
                    ui.colored_label(Color32::from_rgb(230, 70, 70), "O arquivo novo não cabe no espaço do original.");
                } else {
                    ui.colored_label(Color32::from_rgb(220, 150, 40), "⚠ Isto modifica a imagem original. Faça um backup antes.");
                    if ui.button("📥 Importar").clicked() {
                        go = true;
                    }
                }
            });
            if go {
                let d = self.import_dialog.take().unwrap();
                self.spawn("Importação", move |img, fs, _| {
                    let fs = fs.ok_or_else(|| anyhow::anyhow!("sem sistema de arquivos"))?;
                    let e = fs.root.find(&d.entry_path).ok_or_else(|| anyhow::anyhow!("arquivo não encontrado"))?;
                    fs.import(img, e, &std::fs::read(&d.file)?)?;
                    Ok(JobDone { message: format!("{} importado sobre {}", d.file.display(), d.entry_path), warnings: vec![], scan: None, reload: true })
                });
            } else if !open {
                self.import_dialog = None;
            }
        }

        if let Some(job) = &self.job {
            egui::Window::new(&job.title).collapsible(false).resizable(false).title_bar(true).anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0]).show(ctx, |ui| {
                ui.label(job.progress.stage.lock().unwrap().clone());
                ui.add(egui::ProgressBar::new(job.progress.fraction()).show_percentage().desired_width(360.0));
                if ui.button("Cancelar").clicked() {
                    job.progress.cancel.store(true, Ordering::Relaxed);
                }
            });
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }

        if self.about {
            egui::Window::new("Sobre o comBINe").open(&mut self.about).collapsible(false).resizable(false).anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0]).show(ctx, |ui| {
                ui.heading(format!("comBINe {}", env!("CARGO_PKG_VERSION")));
                ui.label("Ferramenta de imagens de CD para Linux, inspirada no CDmage 1.02.1.");
                ui.label("Abre CUE/BIN, CloneCD e ISO; junta e separa trilhas; verifica e reconstrói EDC/ECC;\nnavega e extrai arquivos ISO9660/XA; importa arquivos.");
            });
        }
    }

    fn welcome(&mut self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        ui.vertical_centered(|ui| {
            ui.add_space(ui.available_height() * 0.25);
            ui.heading(RichText::new("comBINe").size(36.0));
            ui.label("Arraste uma imagem (.cue, .ccd, .bin, .iso) para cá ou clique em Abrir.");
            ui.add_space(12.0);
            if ui.add(egui::Button::new(RichText::new("📂 Abrir imagem").size(18.0)).min_size(egui::vec2(200.0, 40.0))).clicked() {
                actions.push(Action::OpenDialog);
            }
            if !self.recent.is_empty() {
                ui.add_space(16.0);
                ui.label(RichText::new("Recentes").strong());
                for p in self.recent.clone() {
                    if ui.link(p.file_name().unwrap_or_default().to_string_lossy()).on_hover_text(p.to_string_lossy()).clicked() {
                        actions.push(Action::Open(p));
                    }
                }
            }
        });
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_job();
        let mut actions = Vec::new();
        if let Some(p) = ctx.input(|i| i.raw.dropped_files.iter().find_map(|f| f.path.clone())) {
            actions.push(Action::Open(p));
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::O)) {
            actions.push(Action::OpenDialog);
        }

        egui::TopBottomPanel::top("menu").show(ctx, |ui| {
            self.menu(ui, &mut actions);
            self.toolbar(ui, &mut actions);
            ui.add_space(2.0);
        });

        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(&self.status).small());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let n_err = self.log.iter().filter(|l| l.level == Level::Error).count();
                    let label = if n_err > 0 { format!("Log ({n_err} erros)") } else { "Log".into() };
                    ui.toggle_value(&mut self.show_log, label);
                    if let Some(s) = &self.scan {
                        if s.errors.is_empty() {
                            ui.colored_label(Color32::from_rgb(60, 170, 80), "✔ sem erros");
                        } else {
                            ui.colored_label(Color32::from_rgb(230, 70, 70), format!("✖ {} setores com erro", s.errors.len()));
                        }
                    }
                });
            });
        });

        if self.show_log {
            egui::TopBottomPanel::bottom("log").resizable(true).default_height(150.0).show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.strong("Log");
                    if ui.small_button("Limpar").clicked() {
                        self.log.clear();
                    }
                    if ui.small_button("Copiar").clicked() {
                        let text: Vec<&str> = self.log.iter().map(|l| l.text.as_str()).collect();
                        ui.ctx().copy_text(text.join("\n"));
                    }
                });
                let row_h = ui.text_style_height(&egui::TextStyle::Body);
                egui::ScrollArea::vertical().auto_shrink(false).stick_to_bottom(true).show_rows(ui, row_h, self.log.len(), |ui, range| {
                    for l in &self.log[range] {
                        let c = match l.level {
                            Level::Info => ui.visuals().text_color(),
                            Level::Warn => Color32::from_rgb(220, 150, 40),
                            Level::Error => Color32::from_rgb(230, 70, 70),
                        };
                        let r = ui.add(egui::Label::new(RichText::new(&l.text).color(c)).sense(egui::Sense::click()));
                        if let Some(lba) = l.lba {
                            if r.on_hover_text("Clique para ver o setor").clicked() {
                                actions.push(Action::ViewSector(lba));
                            }
                        }
                    }
                });
            });
        }

        if self.img.is_some() {
            egui::SidePanel::left("tree").resizable(true).default_width(270.0).show(ctx, |ui| self.tree(ui, &mut actions));
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            if self.img.is_none() {
                self.welcome(ui, &mut actions);
                return;
            }
            ui.horizontal(|ui| {
                for (v, label) in [(View::Files, "📁 Arquivos"), (View::Sectors, "🔢 Setores"), (View::Info, "ℹ Informações")] {
                    ui.selectable_value(&mut self.view, Some(v), label);
                }
            });
            ui.separator();
            match self.view {
                Some(View::Files) => self.files_view(ui, &mut actions),
                Some(View::Sectors) => self.sectors_view(ui),
                _ => self.info_view(ui, &mut actions),
            }
        });

        self.dialogs(ctx);

        if self.job.is_none() {
            for a in actions {
                self.apply(a);
            }
        } else {
            // Ignore actions that would race the running job, but still allow navigation.
            for a in actions {
                if matches!(a, Action::EnterDir(_) | Action::ViewSector(_)) {
                    self.apply(a);
                }
            }
        }
    }
}
