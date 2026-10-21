#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod ink;
mod pdf;

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use eframe::egui::{self, Color32, Pos2, Rect, Sense, Stroke, StrokeKind, TextureHandle, vec2};
use pdf::{Content, Doc, Field, FieldKind, ObjectId, Res};

const INK: [u8; 3] = [15, 25, 90];
const ACCENT: Color32 = Color32::from_rgb(40, 110, 220);
const ROW_H: f32 = 190.0;

/// Firma o timbro salvato. `text` è il modello del timbro, oppure `img:<percorso>`.
#[derive(Clone)]
struct Stamp {
    name: String,
    color: [u8; 3],
    text: String,
}

fn config_dir() -> PathBuf {
    // Windows: %APPDATA%; Linux (anche dentro Flatpak): $XDG_CONFIG_HOME o ~/.config
    let var = |k| std::env::var_os(k).map(PathBuf::from);
    let base = var("APPDATA").or_else(|| var("XDG_CONFIG_HOME")).or_else(|| var("HOME").map(|h| h.join(".config")));
    let dir = base.unwrap_or_else(|| ".".into()).join("pddf");
    std::fs::create_dir_all(&dir).ok();
    dir
}

fn load_stamps() -> Vec<Stamp> {
    let Ok(s) = std::fs::read_to_string(config_dir().join("timbri.tsv")) else {
        let t = |name: &str, color, text: &str| Stamp { name: name.into(), color, text: text.into() };
        return vec![
            t("Approvato", [0, 130, 60], "APPROVATO\n{data} {ora}\n{utente}"),
            t("Ricevuto", [30, 60, 170], "RICEVUTO\n{data}"),
            t("Copia conforme", [190, 20, 20], "COPIA CONFORME"),
        ];
    };
    s.lines()
        .filter_map(|l| {
            let mut f = l.splitn(3, '\t');
            let (name, hex, text) = (f.next()?, f.next()?, f.next()?);
            let c = u32::from_str_radix(hex, 16).ok()?.to_be_bytes();
            Some(Stamp { name: name.into(), color: [c[1], c[2], c[3]], text: text.replace("\\n", "\n") })
        })
        .collect()
}

fn save_stamps(stamps: &[Stamp]) {
    let out: String = stamps
        .iter()
        .map(|s| {
            let [r, g, b] = s.color;
            format!("{}\t{r:02x}{g:02x}{b:02x}\t{}\n", s.name.replace('\t', " "), s.text.replace('\n', "\\n"))
        })
        .collect();
    std::fs::write(config_dir().join("timbri.tsv"), out).ok();
}

/// Apre un'immagine raddrizzandola secondo l'EXIF: le foto da telefono sono spesso ruotate.
fn load_image(path: &Path) -> Res<image::DynamicImage> {
    use image::ImageDecoder;
    let mut decoder = image::ImageReader::open(path)?.with_guessed_format()?.into_decoder()?;
    let orientation = decoder.orientation()?;
    let mut img = image::DynamicImage::from_decoder(decoder)?;
    img.apply_orientation(orientation);
    Ok(img)
}

/// Risolve i segnaposto dinamici e prepara il contenuto da imprimere.
fn content(s: &Stamp, file: &str, page: usize, pages: usize) -> Res<Content> {
    if let Some(path) = s.text.strip_prefix("img:") {
        return Ok(Content::Image(load_image(Path::new(path))?));
    }
    let now = chrono::Local::now();
    let text = s
        .text
        .replace("{data}", &now.format("%d/%m/%Y").to_string())
        .replace("{ora}", &now.format("%H:%M").to_string())
        .replace("{utente}", &std::env::var("USERNAME").or_else(|_| std::env::var("USER")).unwrap_or_default())
        .replace("{file}", file)
        .replace("{pagina}", &page.to_string())
        .replace("{pagine}", &pages.to_string());
    Ok(Content::Text { lines: text.lines().map(String::from).collect(), color: s.color })
}

/// Trasforma i tratti disegnati in un PNG trasparente ritagliato sulla firma.
fn rasterize(strokes: &[Vec<Pos2>]) -> Option<image::RgbaImage> {
    const S: f32 = 3.0; // sovracampionamento rispetto al riquadro a video
    const R: f32 = 3.0; // raggio del tratto in pixel
    let pts = || strokes.iter().flatten();
    let min = pts().fold(Pos2::new(f32::MAX, f32::MAX), |a, p| a.min(*p));
    let max = pts().fold(Pos2::new(f32::MIN, f32::MIN), |a, p| a.max(*p));
    if min.x > max.x {
        return None;
    }
    let m = R + 2.0;
    let (w, h) = (((max.x - min.x) * S + 2.0 * m) as u32, ((max.y - min.y) * S + 2.0 * m) as u32);
    let mut img = image::RgbaImage::new(w, h);
    let mut dot = |c: Pos2| {
        for y in (c.y - m).max(0.0) as u32..((c.y + m) as u32).min(h) {
            for x in (c.x - m).max(0.0) as u32..((c.x + m) as u32).min(w) {
                let d = c.distance(Pos2::new(x as f32 + 0.5, y as f32 + 0.5));
                let a = ((R + 0.5 - d).clamp(0.0, 1.0) * 255.0) as u8;
                let px = img.get_pixel_mut(x, y);
                if a > px[3] {
                    *px = image::Rgba([INK[0], INK[1], INK[2], a]);
                }
            }
        }
    };
    let map = |p: Pos2| Pos2::new((p.x - min.x) * S + m, (p.y - min.y) * S + m);
    for s in strokes {
        for i in 0..s.len() {
            let (a, b) = (map(s[i]), map(s[(i + 1).min(s.len() - 1)]));
            let steps = a.distance(b).ceil().max(1.0);
            for k in 0..=steps as u32 {
                dot(a.lerp(b, k as f32 / steps));
            }
        }
    }
    Some(img)
}

/// Adattamento dello zoom alla finestra; resta attivo anche quando la si ridimensiona.
#[derive(Clone, Copy, PartialEq)]
enum Fit {
    Free,
    Width,
    Height,
}

/// Azione sospesa in attesa che l'utente decida cosa fare delle modifiche non salvate.
enum Pending {
    Close,
    Open(PathBuf),
}

/// Finestra "Aggiungi firma": tracciata col mouse o ricavata da una foto/scansione.
struct NewSig {
    name: String,
    from_image: bool,
    strokes: Vec<Vec<Pos2>>,
    import: Option<Import>,
}

/// Immagine scelta nella scheda "Da immagine", con l'anteprima di ciò che verrà salvato.
struct Import {
    src: PathBuf,
    img: image::DynamicImage,
    scan: ink::Scan,
    /// Toglie la carta e tiene solo il tratto; spento per loghi e timbri a colori.
    extract: bool,
    threshold: f32,
    color: [u8; 3],
    /// Risultato per le impostazioni correnti; None = da ricalcolare.
    result: Option<(image::RgbaImage, TextureHandle)>,
}

enum Act {
    /// Le azioni sulle pagine valgono per la selezione o, se vuota, per la pagina corrente.
    Rotate(i32),
    Move(usize, usize),
    Delete,
    Undo,
    Insert(PathBuf),
    Extract(PathBuf),
    Replace(PathBuf),
    Tool(usize),
    Place(usize, f32, f32),
    /// Nuovo valore per un campo del modulo.
    Field(ObjectId, String),
}

struct App {
    doc: Option<Doc>,
    path: Option<PathBuf>,
    cur: usize,
    /// Pagine selezionate con Ctrl/Maiusc + clic sulle miniature.
    sel: BTreeSet<usize>,
    zoom: f32,
    fit: Fit,
    /// Zoom, scorrimento e riquadro della vista al fotogramma precedente.
    last_zoom: f32,
    zoomed_at: f64,
    offset: egui::Vec2,
    viewport: Rect,
    /// Pagina da portare in cima alla vista al prossimo fotogramma.
    goto: Option<usize>,
    /// La pagina corrente è cambiata scorrendo: le miniature devono seguirla.
    follow: bool,
    /// Campi compilabili delle pagine visibili.
    forms: HashMap<usize, Vec<Field>>,
    /// Campo di testo in corso di modifica, col testo non ancora confermato.
    editing: Option<(ObjectId, String)>,
    /// Il campo in modifica deve ancora ricevere il cursore.
    focus: bool,
    thumbs: HashMap<usize, TextureHandle>,
    /// Pagine renderizzate: indice -> (larghezza in pixel, texture). Larghezza -1 = da rifare.
    view: HashMap<usize, (i32, TextureHandle)>,
    stamps: Vec<Stamp>,
    /// Timbro attivo (indice, rapporto altezza/larghezza) in attesa del clic sulla pagina.
    tool: Option<(usize, f32)>,
    width_mm: f32,
    /// Firma in corso di disegno: nome con cui salvarla e tratti.
    sig: Option<NewSig>,
    edit: Option<(Option<usize>, Stamp)>,
    msg: String,
    title: String,
    confirm: Option<Pending>,
}

/// Dialogo per i PDF, aperto nella cartella del documento corrente.
fn pdf_dialog(current: &Option<PathBuf>) -> rfd::FileDialog {
    let dialog = rfd::FileDialog::new().add_filter("PDF", &["pdf"]);
    match current.as_ref().and_then(|p| p.parent()) {
        Some(dir) => dialog.set_directory(dir),
        None => dialog,
    }
}

fn texture(ctx: &egui::Context, doc: &Doc, i: usize, px: i32) -> Option<TextureHandle> {
    let (w, h, rgba) = doc.render(i, px).ok()?;
    let img = egui::ColorImage::from_rgba_unmultiplied([w, h], &rgba);
    Some(ctx.load_texture(format!("p{i}"), img, Default::default()))
}

impl App {
    fn new() -> Self {
        Self {
            doc: None,
            path: None,
            cur: 0,
            sel: BTreeSet::new(),
            zoom: 1.0,
            fit: Fit::Width,
            last_zoom: 0.0,
            zoomed_at: 0.0,
            offset: egui::Vec2::ZERO,
            viewport: Rect::ZERO,
            goto: None,
            follow: false,
            forms: HashMap::new(),
            editing: None,
            focus: false,
            thumbs: HashMap::new(),
            view: HashMap::new(),
            stamps: load_stamps(),
            tool: None,
            width_mm: 50.0,
            sig: None,
            edit: None,
            msg: String::new(),
            title: String::new(),
            confirm: None,
        }
    }

    fn file_name(&self) -> String {
        self.path.as_ref().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
    }

    fn targets(&self) -> Vec<usize> {
        if self.sel.is_empty() { vec![self.cur] } else { self.sel.iter().copied().collect() }
    }

    fn refresh(&mut self) {
        self.thumbs.clear();
        self.forms.clear();
        // Le pagine restano a video con la vecchia immagine finché non vengono ridisegnate.
        self.view.values_mut().for_each(|v| v.0 = -1);
    }

    fn dirty(&self) -> bool {
        self.doc.as_ref().is_some_and(|d| d.dirty)
    }

    fn open(&mut self, path: PathBuf) {
        if self.dirty() {
            self.confirm = Some(Pending::Open(path));
            return;
        }
        match Doc::open(&path) {
            Ok(d) => {
                (self.doc, self.path, self.cur, self.tool) = (Some(d), Some(path), 0, None);
                self.sel.clear();
                (self.fit, self.goto) = (Fit::Width, Some(0));
                self.msg.clear();
                self.editing = None;
                self.refresh();
                self.view.clear();
            }
            Err(e) => self.msg = format!("Impossibile aprire {}: {e}", path.display()),
        }
    }

    /// Salva; restituisce false se l'utente annulla o il salvataggio fallisce.
    fn save(&mut self, save_as: bool) -> bool {
        let Some(doc) = &mut self.doc else { return false };
        let path = match &self.path {
            Some(p) if !save_as => p.clone(),
            _ => match pdf_dialog(&self.path).save_file() {
                Some(p) => p,
                None => return false,
            },
        };
        match doc.save(&path) {
            Ok(()) => {
                self.msg = format!("Salvato {}", path.display());
                self.path = Some(path);
                true
            }
            Err(e) => {
                self.msg = format!("Salvataggio fallito: {e}");
                false
            }
        }
    }

    /// Chiede cosa fare delle modifiche non salvate, poi esegue l'azione sospesa.
    // Finestra interna e non di sistema: su Linux quelle native richiedono zenity/GTK.
    fn confirm_window(&mut self, ctx: &egui::Context) {
        if self.confirm.is_none() {
            return;
        }
        let mut choice = None;
        egui::Modal::new(egui::Id::new("conferma")).show(ctx, |ui| {
            ui.label(format!("Salvare le modifiche a {}?", self.file_name()));
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                for (i, text) in ["Salva", "Non salvare", "Annulla"].into_iter().enumerate() {
                    if ui.button(text).clicked() {
                        choice = Some(i);
                    }
                }
            });
        });
        let proceed = match choice {
            Some(0) => self.save(false),
            Some(1) => true,
            Some(_) => false,
            None => return,
        };
        let pending = self.confirm.take();
        if !proceed {
            return;
        }
        self.doc = None; // scartato (o appena salvato): niente più modifiche in sospeso
        match pending {
            Some(Pending::Open(p)) => self.open(p),
            _ => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
        }
    }

    fn apply(&mut self, act: Act) {
        let (file, cur, pages) = (self.file_name(), self.cur, self.targets());
        let Some(d) = &mut self.doc else { return };
        let wpt = self.width_mm * 72.0 / 25.4;
        let (is_tool, is_place) = (matches!(act, Act::Tool(_) | Act::Extract(_)), matches!(act, Act::Place(..) | Act::Field(..)));
        if matches!(act, Act::Move(..) | Act::Delete | Act::Undo | Act::Insert(_) | Act::Replace(_)) {
            self.sel.clear(); // gli indici selezionati non indicano più le stesse pagine
        }
        let mut done = String::new();
        let result: Res<()> = (|| {
            match act {
                Act::Rotate(q) => {
                    d.rotate(&pages, q)?;
                    // La vista va sulla pagina ruotata: "Larghezza" si adatta alla pagina corrente.
                    self.cur = pages[0];
                }
                Act::Move(from, to) => {
                    d.move_page(from, to)?;
                    self.cur = to;
                }
                Act::Delete => d.delete(&pages)?,
                Act::Undo => {
                    d.undo()?;
                }
                Act::Insert(p) => d.insert_pdf(&p, cur + 1)?,
                Act::Extract(p) => {
                    pdf::write(&p, &d.extract(&pages)?)?;
                    done = format!("{} pagine estratte in {}", pages.len(), p.display());
                }
                Act::Replace(p) => {
                    d.replace(&pages, &p)?;
                    self.cur = pages[0];
                }
                Act::Tool(i) => {
                    self.tool = match self.tool {
                        Some((j, _)) if j == i => None,
                        _ => {
                            let (w, h) = content(&self.stamps[i], &file, cur + 1, d.len())?.size();
                            Some((i, h / w))
                        }
                    }
                }
                Act::Field(id, value) => d.set_field(id, &value)?,
                Act::Place(page, u, v) => {
                    if let Some((i, _)) = self.tool.take() {
                        d.place(page, u, v, wpt, &content(&self.stamps[i], &file, page + 1, d.len())?)?;
                    }
                }
            }
            Ok(())
        })();
        self.cur = self.cur.min(d.len().saturating_sub(1));
        self.msg = result.err().map_or(done, |e| e.to_string());
        if !is_tool {
            self.refresh();
        }
        if !is_tool && !is_place {
            self.goto = Some(self.cur); // la struttura è cambiata: resta sulla pagina corrente
        }
    }

    fn toolbar(&mut self, ui: &mut egui::Ui) -> Option<Act> {
        let mut act = None;
        ui.horizontal_wrapped(|ui| {
            if ui.button("Apri…").clicked()
                && let Some(p) = pdf_dialog(&self.path).pick_file()
            {
                self.open(p);
            }
            let Some(n) = self.doc.as_ref().map(Doc::len) else { return };
            if ui.button("Salva").clicked() {
                self.save(false);
            }
            if ui.button("Salva con nome…").clicked() {
                self.save(true);
            }
            if ui.button("Inserisci PDF…").on_hover_text("Inserisce un altro PDF dopo la pagina corrente").clicked()
                && let Some(p) = pdf_dialog(&self.path).pick_file()
            {
                act = Some(Act::Insert(p));
            }
            ui.separator();
            let k = self.targets().len();
            let hint = "Vale per la pagina corrente o per quelle selezionate (Ctrl/Maiusc + clic sulle miniature, Ctrl+A tutte)";
            if ui.button("Ruota ↺").on_hover_text(hint).clicked() {
                act = Some(Act::Rotate(-1));
            }
            if ui.button("Ruota ↻").on_hover_text(hint).clicked() {
                act = Some(Act::Rotate(1));
            }
            if ui.add_enabled(self.cur > 0, egui::Button::new("Sposta su")).clicked() {
                act = Some(Act::Move(self.cur, self.cur - 1));
            }
            if ui.add_enabled(self.cur + 1 < n, egui::Button::new("Sposta giù")).clicked() {
                act = Some(Act::Move(self.cur, self.cur + 1));
            }
            if ui.add_enabled(k < n, egui::Button::new("Elimina")).on_hover_text(hint).clicked() {
                act = Some(Act::Delete);
            }
            let stem = self.path.as_ref().and_then(|p| p.file_stem()).map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            if ui.button("Estrai…").on_hover_text("Salva in un nuovo PDF la pagina corrente o quelle selezionate").clicked()
                && let Some(p) = pdf_dialog(&self.path).set_file_name(format!("{stem}-estratto.pdf")).save_file()
            {
                act = Some(Act::Extract(p));
            }
            if ui.button("Sostituisci…").on_hover_text("Sostituisce la pagina corrente o quelle selezionate con un altro PDF").clicked()
                && let Some(p) = pdf_dialog(&self.path).pick_file()
            {
                act = Some(Act::Replace(p));
            }
            if ui.button("Annulla").on_hover_text("Ctrl+Z").clicked() {
                act = Some(Act::Undo);
            }
            ui.separator();
            let mut percent = self.zoom * 100.0;
            if ui.button("−").on_hover_text("Ctrl −  oppure Ctrl + rotella").clicked() {
                percent /= 1.25;
            }
            ui.add(egui::DragValue::new(&mut percent).range(10.0..=640.0).fixed_decimals(0).suffix("%"));
            if ui.button("+").on_hover_text("Ctrl +  oppure Ctrl + rotella").clicked() {
                percent *= 1.25;
            }
            if ui.button("100%").clicked() {
                percent = 100.0;
            }
            if percent != self.zoom * 100.0 {
                (self.zoom, self.fit) = (percent / 100.0, Fit::Free);
            }
            for (fit, text) in [(Fit::Width, "Larghezza"), (Fit::Height, "Altezza")] {
                if ui.selectable_label(self.fit == fit, text).on_hover_text("Adatta la pagina alla finestra").clicked() {
                    self.fit = if self.fit == fit { Fit::Free } else { fit };
                    self.goto = Some(self.cur); // riallinea la pagina corrente in cima alla vista
                }
            }
            ui.separator();
            ui.horizontal(|ui| {
                ui.label("Pagina");
                let mut page = self.cur + 1;
                let field = egui::DragValue::new(&mut page).range(1..=n).speed(0.05).update_while_editing(false).suffix(format!(" / {n}"));
                if ui.add(field).on_hover_text("Clicca, scrivi il numero e premi Invio").changed() {
                    (self.cur, self.goto, self.follow) = (page - 1, Some(page - 1), true);
                }
            });

        });
        act
    }

    fn stamps_panel(&mut self, ui: &mut egui::Ui) -> Option<Act> {
        let mut act = None;
        let mut remove = None;
        ui.add_space(4.0);
        ui.strong("Firme e timbri");
        ui.add_space(4.0);
        // Pulsanti in alto: con molte firme salvate l'elenco non li spinge fuori dalla finestra.
        let full = ui.available_width();
        let wide = |text| egui::Button::new(text).min_size(vec2(full, 0.0));
        if ui.add(wide("Aggiungi firma…")).on_hover_text("Disegnala col mouse o ricavala da una foto o scansione").clicked() {
            self.sig = Some(NewSig { name: "Firma".into(), from_image: false, strokes: vec![], import: None });
        }
        if ui.add(wide("Nuovo timbro…")).clicked() {
            self.edit = Some((None, Stamp { name: "Nuovo timbro".into(), color: [190, 20, 20], text: "TESTO\n{data}".into() }));
        }
        ui.add(egui::Slider::new(&mut self.width_mm, 10.0..=180.0).suffix(" mm").text("larghezza"));
        ui.separator();
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            for (i, s) in self.stamps.iter().enumerate() {
                ui.horizontal(|ui| {
                    let active = self.tool.is_some_and(|(j, _)| j == i);
                    let label = ui.add_enabled(self.doc.is_some(), egui::Button::selectable(active, &s.name));
                    if label.on_hover_text("Seleziona, poi clicca sulla pagina").clicked() {
                        act = Some(Act::Tool(i));
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("elimina").clicked() {
                            remove = Some(i);
                        }
                        if ui.small_button("modifica").clicked() {
                            self.edit = Some((Some(i), s.clone()));
                        }
                    });
                });
            }
        });
        if let Some(i) = remove {
            self.stamps.remove(i);
            self.tool = None;
            save_stamps(&self.stamps);
        }
        act
    }

    fn signature_window(&mut self, ctx: &egui::Context) {
        let Some(sig) = &mut self.sig else { return };
        let (mut open, mut done) = (true, false);
        // Ancorata in alto: cambiando scheda l'altezza cambia e la finestra non deve saltare.
        let top = ctx.content_rect().center() - vec2(0.0, 220.0);
        egui::Window::new("Aggiungi firma").open(&mut open).collapsible(false).resizable(false).pivot(egui::Align2::CENTER_TOP).default_pos(top).show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label("Nome");
                ui.text_edit_singleline(&mut sig.name);
            });
            ui.horizontal(|ui| {
                ui.selectable_value(&mut sig.from_image, false, "Disegna");
                ui.selectable_value(&mut sig.from_image, true, "Da immagine").on_hover_text("Foto o scansione di una firma su carta: resta solo il tratto");
            });
            ui.separator();
            let ready = if sig.from_image {
                let pick = ui.button(if sig.import.is_some() { "Cambia immagine…" } else { "Scegli immagine…" });
                if pick.clicked()
                    && let Some(src) = rfd::FileDialog::new().add_filter("Immagini", &["png", "jpg", "jpeg"]).pick_file()
                {
                    match load_image(&src) {
                        Ok(img) => {
                            let scan = ink::Scan::new(&img);
                            let (threshold, color) = (scan.auto, scan.ink_color(scan.auto));
                            sig.import = Some(Import { src, img, scan, extract: true, threshold, color, result: None });
                        }
                        Err(e) => self.msg = format!("Immagine non leggibile: {e}"),
                    }
                }
                if let Some(imp) = &mut sig.import {
                    let mut changed = ui.checkbox(&mut imp.extract, "Estrai il tratto (toglie lo sfondo della carta)").changed();
                    ui.add_enabled_ui(imp.extract, |ui| {
                        ui.horizontal(|ui| {
                            let slider = egui::Slider::new(&mut imp.threshold, 0.05..=0.8).show_value(false).text("pulizia");
                            changed |= ui.add(slider).on_hover_text("Più a destra: sfondo più pulito, tratto più sottile").changed();
                            if ui.small_button("auto").clicked() {
                                imp.threshold = imp.scan.auto;
                                changed = true;
                            }
                            ui.separator();
                            ui.label("Colore");
                            changed |= ui.color_edit_button_srgb(&mut imp.color).changed();
                            for (text, c) in [("nero", [0, 0, 0]), ("blu", [15, 35, 130])] {
                                if ui.small_button(text).clicked() {
                                    (imp.color, changed) = (c, true);
                                }
                            }
                        });
                    });
                    if changed {
                        imp.result = None;
                    }
                    if imp.result.is_none() {
                        let img = if imp.extract { imp.scan.extract(imp.threshold, imp.color) } else { Some(imp.img.to_rgba8()) };
                        imp.result = img.map(|img| {
                            let shown = egui::ColorImage::from_rgba_unmultiplied([img.width() as usize, img.height() as usize], &img);
                            let tex = ui.ctx().load_texture("importa", shown, Default::default());
                            (img, tex)
                        });
                    }
                }
                // Fondo grigio chiaro: si vede subito se la carta è stata tolta.
                let (rect, _) = ui.allocate_exact_size(vec2(520.0, 200.0), Sense::hover());
                let p = ui.painter_at(rect);
                p.rect_filled(rect, 4.0, Color32::from_gray(225));
                let note = |text| p.text(rect.center(), egui::Align2::CENTER_CENTER, text, egui::FontId::proportional(14.0), Color32::DARK_GRAY);
                match &sig.import {
                    Some(Import { result: Some((_, t)), .. }) => {
                        let size = t.size_vec2();
                        let fit = Rect::from_center_size(rect.center(), size * ((rect.width() - 16.0) / size.x).min((rect.height() - 16.0) / size.y).min(1.0));
                        p.image(t.id(), fit, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
                    }
                    Some(_) => _ = note("Nessun tratto: abbassa la pulizia"),
                    None => _ = note("Scegli una foto o una scansione della firma (PNG o JPG)"),
                }
                sig.import.as_ref().is_some_and(|i| i.result.is_some())
            } else {
                let strokes = &mut sig.strokes;
                let (rect, resp) = ui.allocate_exact_size(vec2(520.0, 200.0), Sense::drag());
                if resp.drag_started() {
                    strokes.push(vec![]);
                }
                if let (Some(pos), Some(s)) = (resp.interact_pointer_pos(), strokes.last_mut())
                    && resp.dragged()
                {
                    s.push((pos - rect.min).to_pos2());
                }
                let p = ui.painter_at(rect);
                p.rect_filled(rect, 4.0, Color32::WHITE);
                for s in strokes.iter() {
                    let pts = s.iter().map(|q| rect.min + q.to_vec2()).collect();
                    p.add(egui::Shape::line(pts, Stroke::new(2.0, Color32::from_rgb(INK[0], INK[1], INK[2]))));
                }
                strokes.iter().any(|s| !s.is_empty()) // un clic senza trascinare lascia un tratto vuoto
            };
            ui.horizontal(|ui| {
                done = ui.add_enabled(ready && !sig.name.trim().is_empty(), egui::Button::new("Salva firma")).clicked();
                if !sig.from_image && ui.button("Cancella").clicked() {
                    sig.strokes.clear();
                }
            });
        });
        if done {
            // Copia nella cartella di configurazione: la firma non dipende più dal file scelto.
            let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
            let png = |img: &image::RgbaImage| {
                let dest = config_dir().join(format!("firma-{stamp}.png"));
                img.save(&dest).map(|_| dest).map_err(|e| e.to_string())
            };
            let saved = match &sig.import {
                Some(imp) if sig.from_image && !imp.extract => {
                    let ext = imp.src.extension().and_then(|e| e.to_str()).unwrap_or("png");
                    let dest = config_dir().join(format!("img-{stamp}.{ext}"));
                    std::fs::copy(&imp.src, &dest).map(|_| dest).map_err(|e| e.to_string())
                }
                Some(Import { result: Some((img, _)), .. }) if sig.from_image => png(img),
                _ => rasterize(&sig.strokes).ok_or_else(|| "firma vuota".to_string()).and_then(|img| png(&img)),
            };
            match saved {
                Ok(dest) => {
                    self.stamps.push(Stamp { name: sig.name.trim().to_string(), color: INK, text: format!("img:{}", dest.display()) });
                    save_stamps(&self.stamps);
                }
                Err(e) => self.msg = format!("Salvataggio firma fallito: {e}"),
            }
        }
        if done || !open {
            self.sig = None;
        }
    }

    fn editor_window(&mut self, ctx: &egui::Context) {
        let Some((index, s)) = &mut self.edit else { return };
        let (mut open, mut done) = (true, false);
        egui::Window::new("Timbro").open(&mut open).collapsible(false).resizable(false).pivot(egui::Align2::CENTER_CENTER).default_pos(ctx.content_rect().center()).show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label("Nome");
                ui.text_edit_singleline(&mut s.name);
            });
            if s.text.starts_with("img:") {
                ui.label("Timbro grafico (immagine).");
            } else {
                ui.text_edit_multiline(&mut s.text);
                ui.horizontal(|ui| {
                    ui.label("Colore");
                    ui.color_edit_button_srgb(&mut s.color);
                });
                ui.weak("Campi dinamici: {data} {ora} {utente} {file} {pagina} {pagine}");
            }
            done = ui.add_enabled(!s.text.trim().is_empty() && !s.name.trim().is_empty(), egui::Button::new("Salva")).clicked();
        });
        if done {
            match *index {
                Some(i) => self.stamps[i] = s.clone(),
                None => self.stamps.push(s.clone()),
            }
            self.tool = None;
            save_stamps(&self.stamps);
        }
        if done || !open {
            self.edit = None;
        }
    }

    fn thumbnails(&mut self, ui: &mut egui::Ui) -> Option<Act> {
        let doc = self.doc.as_ref()?;
        let mut act = None;
        let ppp = ui.pixels_per_point();
        let mut area = egui::ScrollArea::vertical().auto_shrink(false);
        if std::mem::take(&mut self.follow) {
            let row = ROW_H + ui.spacing().item_spacing.y;
            area = area.vertical_scroll_offset((self.cur as f32 * row - (ui.available_height() - ROW_H) / 2.0).max(0.0));
        }
        area.show_rows(ui, ROW_H, doc.len(), |ui, rows| {
            for i in rows {
                let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), Sense::click_and_drag());
                let p = ui.painter_at(rect);
                if if self.sel.is_empty() { i == self.cur } else { self.sel.contains(&i) } {
                    p.rect_filled(rect, 4.0, ACCENT.gamma_multiply(0.18));
                }
                if !self.thumbs.contains_key(&i)
                    && let Some(t) = texture(ui.ctx(), doc, i, (150.0 * ppp) as i32)
                {
                    self.thumbs.insert(i, t);
                }
                let area = Rect::from_min_max(rect.min + vec2(8.0, 8.0), rect.max - vec2(8.0, 24.0));
                if let Some(t) = self.thumbs.get(&i) {
                    let size = t.size_vec2();
                    let fitted = Rect::from_center_size(area.center(), size * (area.width() / size.x).min(area.height() / size.y));
                    p.image(t.id(), fitted, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
                    p.rect_stroke(fitted, 0.0, Stroke::new(1.0, Color32::GRAY), StrokeKind::Outside);
                }
                let label_pos = Pos2::new(rect.center().x, rect.max.y - 12.0);
                p.text(label_pos, egui::Align2::CENTER_CENTER, (i + 1).to_string(), egui::FontId::proportional(12.0), ui.visuals().text_color());

                // Clic = va alla pagina; Ctrl + clic aggiunge o toglie dalla selezione, Maiusc + clic
                // seleziona dalla pagina corrente a questa; trascinamento = sposta la pagina.
                if resp.clicked() {
                    let m = ui.input(|i| i.modifiers);
                    if m.shift {
                        self.sel = (self.cur.min(i)..=self.cur.max(i)).collect();
                    } else {
                        if !m.command {
                            self.sel.clear();
                        } else if self.sel.is_empty() {
                            self.sel.insert(self.cur);
                        }
                        if m.command && !self.sel.remove(&i) {
                            self.sel.insert(i);
                        }
                        (self.cur, self.goto) = (i, Some(i));
                    }
                }
                if resp.drag_started() {
                    egui::DragAndDrop::set_payload(ui.ctx(), i);
                }
                if let Some(from) = resp.dnd_hover_payload::<usize>()
                    && *from != i
                {
                    let y = if *from > i { rect.min.y } else { rect.max.y };
                    p.hline(rect.x_range(), y, Stroke::new(3.0, ACCENT));
                }
                if let Some(from) = resp.dnd_release_payload::<usize>() {
                    act = Some(Act::Move(*from, i));
                }
            }
        });
        act
    }

    /// Vista continua: tutte le pagine in colonna, si disegnano solo quelle visibili.
    fn page_view(&mut self, ui: &mut egui::Ui) -> Option<Act> {
        const GAP: f32 = 12.0;
        let Some(doc) = &self.doc else {
            ui.centered_and_justified(|ui| ui.weak("Apri un PDF (Ctrl+O) o trascinalo qui"));
            return None;
        };
        let sizes: Vec<egui::Vec2> = (0..doc.len()).map(|i| doc.page_size(i).map_or(vec2(595.0, 842.0), |(w, h)| vec2(w, h))).collect();
        let largest = sizes.iter().fold(vec2(1.0, 1.0), |a, s| a.max(*s));
        let current = sizes.get(self.cur).copied().unwrap_or(largest);
        let avail = ui.available_size();
        let pointer = ui.input(|i| i.pointer.hover_pos()).filter(|p| ui.available_rect_before_wrap().contains(*p));

        // Ctrl + rotella (o pizzico sul touchpad) ingrandisce attorno al puntatore.
        let wheel = if pointer.is_some() { ui.input(|i| i.zoom_delta()) } else { 1.0 };
        if wheel != 1.0 {
            (self.zoom, self.fit) = (self.zoom * wheel, Fit::Free);
        }
        match self.fit {
            Fit::Width => self.zoom = (avail.x - 2.0 * GAP) / current.x,
            Fit::Height => self.zoom = (avail.y - 2.0 * GAP) / current.y,
            Fit::Free => {}
        }
        self.zoom = self.zoom.clamp(0.1, 6.4);
        let zoom = self.zoom;

        let content_w = |z: f32| (largest.x * z + 2.0 * GAP).max(avail.x);
        let mut total_h = GAP;
        let mut tops = Vec::with_capacity(sizes.len());
        for s in &sizes {
            tops.push(total_h);
            total_h += s.y * zoom + GAP;
        }

        // Lo scorrimento viene imposto solo per andare a una pagina o per tenere fermo,
        // quando lo zoom cambia, il punto sotto il puntatore (o il centro della vista).
        let mut area = egui::ScrollArea::both().auto_shrink(false);
        let mut forced = true;
        if let Some(i) = self.goto.take().filter(|i| *i < tops.len()) {
            area = area.vertical_scroll_offset(tops[i] - GAP / 2.0);
        } else if zoom != self.last_zoom && self.last_zoom > 0.0 {
            let old = self.last_zoom;
            let anchor = pointer.filter(|_| wheel != 1.0).map_or(self.viewport.size() / 2.0, |p| p - self.viewport.min);
            let x = (self.offset.x + anchor.x - content_w(old) / 2.0) / old * zoom + content_w(zoom) / 2.0 - anchor.x;
            let y = (self.offset.y + anchor.y) / old * zoom - anchor.y;
            area = area.scroll_offset(vec2(x.max(0.0), y.max(0.0)));
        } else {
            forced = false;
        }
        let now = ui.input(|i| i.time);
        if zoom != self.last_zoom {
            (self.last_zoom, self.zoomed_at) = (zoom, now);
        }
        // Mentre lo zoom sta cambiando si riusa l'immagine esistente, stirata: si ridisegna a fine gesto.
        let settled = now - self.zoomed_at > 0.2;

        let (tool, wpt, ppp) = (self.tool, self.width_mm * 72.0 / 25.4, ui.pixels_per_point());
        let mut act = None;
        let mut budget = 1; // al massimo un rendering per fotogramma, per non bloccare lo scorrimento
        let mut most_visible = (0.0, self.cur);
        let mut seen = (usize::MAX, 0);
        let out = area.show_viewport(ui, |ui, _| {
            let (outer, _) = ui.allocate_exact_size(vec2(content_w(zoom), total_h), Sense::hover());
            let painter = ui.painter().clone();
            for (i, size) in sizes.iter().map(|s| *s * zoom).enumerate() {
                let rect = Rect::from_min_size(Pos2::new(outer.center().x - size.x / 2.0, outer.min.y + tops[i]), size);
                let shown = rect.intersect(ui.clip_rect());
                if !shown.is_positive() {
                    continue;
                }
                if shown.height() > most_visible.0 {
                    most_visible = (shown.height(), i);
                }
                seen = (seen.0.min(i), i);

                // ponytail: la pagina è un'unica texture, limitata a 6000 px sul lato lungo:
                // oltre, a zoom molto alti, sgrana. Per andare oltre serve il rendering a riquadri.
                let px = (size.x * ppp * (6000.0 / (size.max_elem() * ppp)).min(1.0)).round() as i32;
                let fresh = self.view.get(&i).is_some_and(|(w, _)| *w == px);
                if !fresh && budget > 0 && (settled || !self.view.contains_key(&i)) {
                    budget -= 1;
                    if let Some(t) = texture(ui.ctx(), doc, i, px) {
                        self.view.insert(i, (px, t));
                    }
                }
                painter.rect_filled(rect, 0.0, Color32::WHITE);
                if let Some((_, t)) = self.view.get(&i) {
                    painter.image(t.id(), rect, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
                }
                if !self.view.get(&i).is_some_and(|(w, _)| *w == px) {
                    ui.ctx().request_repaint_after(std::time::Duration::from_millis(50));
                }
                painter.rect_stroke(rect, 0.0, Stroke::new(1.0, Color32::GRAY), StrokeKind::Outside);

                let resp = ui.interact(rect, ui.id().with(("pagina", i)), Sense::click());
                // Moduli: i campi si compilano direttamente sulla pagina (non mentre si posiziona un timbro).
                let fields = self.forms.entry(i).or_insert_with(|| doc.fields(i));
                for (k, f) in fields.iter().enumerate().filter(|_| tool.is_none()) {
                    let at = |x: f32, y: f32| rect.min + vec2(x, y) * zoom;
                    let area = Rect::from_min_max(at(f.rect[0], f.rect[1]), at(f.rect[2], f.rect[3]));
                    let id = ui.id().with(("campo", i, k));
                    if !ui.is_rect_visible(area) {
                        continue;
                    }
                    if self.editing.as_ref().is_some_and(|(id, _)| *id == f.id) {
                        let multiline = matches!(f.kind, FieldKind::Text { multiline: true });
                        let text = &mut self.editing.as_mut().unwrap().1;
                        let edit = if multiline { egui::TextEdit::multiline(text) } else { egui::TextEdit::singleline(text) };
                        let size = if multiline { 11.0 * zoom } else { (area.height() * 0.7).min(12.0 * zoom) };
                        let edit = edit.id(id).font(egui::FontId::proportional(size.max(8.0))).margin(2.0);
                        let edit = ui.put(area, edit.background_color(Color32::WHITE).text_color(Color32::BLACK));
                        if std::mem::take(&mut self.focus) {
                            edit.request_focus();
                        }
                        if edit.lost_focus() {
                            // Invio o clic altrove confermano, Esc annulla, Tab passa al campo di testo successivo.
                            let (cancel, tab) = ui.input(|i| (i.key_pressed(egui::Key::Escape), i.key_pressed(egui::Key::Tab)));
                            let (field, text) = self.editing.take().unwrap();
                            if !cancel && text != f.value {
                                act = Some(Act::Field(field, text));
                            }
                            if let Some(next) = fields[k + 1..].iter().find(|n| matches!(n.kind, FieldKind::Text { .. })).filter(|_| tab) {
                                (self.editing, self.focus) = (Some((next.id, next.value.clone())), true);
                            }
                        }
                        continue;
                    }
                    let cursor = if matches!(f.kind, FieldKind::Text { .. }) { egui::CursorIcon::Text } else { egui::CursorIcon::PointingHand };
                    let field = ui.interact(area, id, Sense::click()).on_hover_cursor(cursor);
                    painter.rect_filled(area, 1.0, ACCENT.gamma_multiply(if field.hovered() { 0.25 } else { 0.10 }));
                    match &f.kind {
                        FieldKind::Text { .. } if field.clicked() => (self.editing, self.focus) = (Some((f.id, f.value.clone())), true),
                        FieldKind::Button { on, radio } if field.clicked() => {
                            // Una casella si spunta e si toglie; un'opzione si può solo scegliere.
                            let checked = f.value == *on;
                            if !(checked && *radio) {
                                act = Some(Act::Field(f.id, if checked { "Off".into() } else { on.clone() }));
                            }
                        }
                        FieldKind::Choice { options } => {
                            egui::Popup::menu(&field).show(|ui| {
                                for (saved, shown) in options {
                                    if ui.selectable_label(*saved == f.value, shown).clicked() {
                                        act = Some(Act::Field(f.id, saved.clone()));
                                    }
                                }
                            });
                        }
                        _ => {}
                    }
                }
                if let (Some((_, aspect)), Some(pos)) = (tool, resp.hover_pos()) {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::Crosshair);
                    let ghost = Rect::from_center_size(pos, vec2(wpt, wpt * aspect) * zoom);
                    painter.rect_filled(ghost, 2.0, ACCENT.gamma_multiply(0.12));
                    painter.rect_stroke(ghost, 2.0, Stroke::new(1.5, ACCENT), StrokeKind::Inside);
                    if resp.clicked() {
                        let at = (pos - rect.min) / zoom;
                        act = Some(Act::Place(i, at.x, at.y));
                    }
                }
            }
            if !ui.ctx().egui_wants_keyboard_input() {
                let key = |k| ui.input(|i| i.key_pressed(k)) as i32 as f32;
                let dy = key(egui::Key::ArrowUp) - key(egui::Key::ArrowDown);
                if dy != 0.0 {
                    ui.scroll_with_delta(vec2(0.0, dy * 80.0));
                }
            }
        });
        self.view.retain(|i, _| (seen.0.saturating_sub(2)..=seen.1.saturating_add(2)).contains(i));
        self.forms.retain(|i, _| (seen.0..=seen.1).contains(i));

        // Scorrendo a mano, la pagina corrente diventa quella che occupa più spazio a video.
        if !forced && out.state.offset.y != self.offset.y && most_visible.1 != self.cur {
            (self.cur, self.follow) = (most_visible.1, true);
        }
        (self.offset, self.viewport) = (out.state.offset, out.inner_rect);
        act
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

        if ctx.input(|i| i.viewport().close_requested()) && self.dirty() {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.confirm = Some(Pending::Close);
        }
        if let Some(p) = ctx.input(|i| i.raw.dropped_files.first().map(|f| f.path().to_path_buf())) {
            self.open(p);
        }

        let mut act = None;
        let ctrl = |key| ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, key));
        if ctrl(egui::Key::O)
            && let Some(p) = pdf_dialog(&self.path).pick_file()
        {
            self.open(p);
        }
        if ctrl(egui::Key::S) {
            self.save(false);
        }
        let step = (ctrl(egui::Key::Plus) || ctrl(egui::Key::Equals)) as i32 - ctrl(egui::Key::Minus) as i32;
        if step != 0 {
            (self.zoom, self.fit) = (self.zoom * 1.25f32.powi(step), Fit::Free);
        }
        if !ctx.egui_wants_keyboard_input() {
            if ctrl(egui::Key::Z) {
                act = Some(Act::Undo);
            }
            let n = self.doc.as_ref().map_or(0, Doc::len);
            if ctrl(egui::Key::A) {
                self.sel = (0..n).collect();
            }
            let key = |k| ctx.input(|i| i.key_pressed(k));
            let target = [
                (egui::Key::PageDown, self.cur + 1),
                (egui::Key::PageUp, self.cur.saturating_sub(1)),
                (egui::Key::Home, 0),
                (egui::Key::End, n.saturating_sub(1)),
            ];
            if let Some((_, page)) = target.into_iter().find(|(k, page)| key(*k) && *page < n) {
                (self.cur, self.goto, self.follow) = (page, Some(page), true);
            }
            if key(egui::Key::Escape) {
                self.tool = None;
                self.sel.clear();
            }
        }

        egui::Panel::top("barra").show(ui, |ui| {
            ui.add_space(4.0);
            act = self.toolbar(ui).or(act.take());
            ui.add_space(4.0);
        });
        egui::Panel::bottom("stato").show(ui, |ui| {
            let hint = match (self.tool, self.sel.len()) {
                (Some(_), _) => "Clicca sulla pagina per posizionare (Esc per annullare)".into(),
                (None, 0) => String::new(),
                (None, k) => format!("{k} pagine selezionate (Esc per deselezionare)"),
            };
            ui.label(if self.msg.is_empty() { &hint } else { &self.msg });
        });
        egui::Panel::left("pagine").default_size(180.0).show(ui, |ui| {
            act = self.thumbnails(ui).or(act.take());
        });
        egui::Panel::right("timbri").default_size(250.0).show(ui, |ui| {
            act = self.stamps_panel(ui).or(act.take());
        });
        egui::CentralPanel::default().show(ui, |ui| {
            act = self.page_view(ui).or(act.take());
        });
        self.signature_window(&ctx);
        self.editor_window(&ctx);
        self.confirm_window(&ctx);

        if let Some(a) = act {
            self.apply(a);
        }

        let dirty = if self.dirty() { "*" } else { "" };
        let title = format!("{}{dirty} — pddf", self.file_name());
        if title != self.title {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
            self.title = title;
        }
    }
}

fn main() -> eframe::Result {
    let arg = std::env::args().nth(1).map(PathBuf::from);
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1280.0, 860.0]),
        ..Default::default()
    };
    eframe::run_native(
        "pddf",
        options,
        Box::new(|cc| {
            // Ctrl +/− ingrandisce la pagina, non l'intera interfaccia.
            cc.egui_ctx.options_mut(|o| o.zoom_with_keyboard = false);
            let mut app = App::new();
            if let Some(p) = arg {
                app.open(p);
            }
            Ok(Box::new(app))
        }),
    )
}
