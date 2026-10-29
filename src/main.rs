#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod ink;
mod pdf;

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use eframe::egui::{self, Color32, Pos2, Rect, RichText, Sense, Stroke, StrokeKind, TextureHandle, load::SizedTexture, vec2};
use egui_phosphor::regular as ph;
use pdf::{Align, Content, Doc, Field, FieldKind, Line, ObjectId, Placed, Res};

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
    // PLICOPDF_CONFIG: cartella a parte, es. per le prove senza toccare firme e timbri veri.
    if let Some(dir) = std::env::var_os("PLICOPDF_CONFIG") {
        std::fs::create_dir_all(&dir).ok();
        return dir.into();
    }
    // Windows: %APPDATA%; Linux (anche dentro Flatpak): $XDG_CONFIG_HOME o ~/.config
    let var = |k| std::env::var_os(k).map(PathBuf::from);
    let base = var("APPDATA").or_else(|| var("XDG_CONFIG_HOME")).or_else(|| var("HOME").map(|h| h.join(".config")));
    let base = base.unwrap_or_else(|| ".".into());
    let (old, dir) = (base.join("pddf"), base.join("plicopdf"));
    // Il programma si chiamava pddf: firme e timbri si spostano nella cartella nuova, e i
    // percorsi delle immagini salvati in timbri.tsv con loro.
    if !dir.exists() && std::fs::rename(&old, &dir).is_ok()
        && let Ok(list) = std::fs::read_to_string(dir.join("timbri.tsv"))
    {
        std::fs::write(dir.join("timbri.tsv"), list.replace(&*old.to_string_lossy(), &dir.to_string_lossy())).ok();
    }
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
    Ok(Content::Text { lines: text.lines().map(Line::parse).collect(), color: s.color })
}

/// Riporta la formattazione delle righe dopo una modifica del testo: le righe prima e dopo il
/// punto modificato tengono la loro, quelle nuove prendono quella della riga in cui si scriveva.
fn remap(old: &str, new: &str, lines: &[Line]) -> Vec<Line> {
    let (o, n): (Vec<char>, Vec<char>) = (old.chars().collect(), new.chars().collect());
    let mut pre = o.iter().zip(&n).take_while(|(a, b)| a == b).count();
    let mut suf = o.iter().rev().zip(n.iter().rev()).take(o.len().min(n.len()) - pre).take_while(|(a, b)| a == b).count();
    // Solo inserimento o solo cancellazione: il punto è ambiguo (a capo in fondo a una riga o
    // all'inizio della seguente), si sceglie il più a sinistra, come fa un elaboratore di testi.
    let (short, long) = if o.len() < n.len() { (&o, &n) } else { (&n, &o) };
    if pre + suf == short.len() {
        let d = long.len() - short.len();
        while pre > 0 && d > 0 && long[pre - 1] == long[pre + d - 1] {
            pre -= 1;
        }
        suf = short.len() - pre;
    }
    let breaks = |c: &[char]| c.iter().filter(|&&c| c == '\n').count();
    let (head, tail) = (breaks(&o[..pre]), breaks(&o[o.len() - suf..]));
    let count = new.split('\n').count();
    new.split('\n')
        .enumerate()
        .map(|(k, text)| {
            let src = if k < head { k } else if k >= count - tail { lines.len().saturating_sub(count - k) } else { head };
            let mut l = lines.get(src).or(lines.last()).cloned().unwrap_or_else(|| Line::parse(""));
            l.text = text.into();
            l
        })
        .collect()
}

/// Impaginazione a video del testo del timbro, con la formattazione di ogni riga.
/// `bold_only`: solo le righe in grassetto, per ripassarle; le altre restano trasparenti.
fn stamp_job(fonts: &mut egui::epaint::text::FontsView, lines: &[Line], width: f32, ink: Color32, bold_only: bool) -> egui::text::LayoutJob {
    let mut job = egui::text::LayoutJob::default();
    for (k, l) in lines.iter().enumerate() {
        let font = egui::FontId::proportional(l.size);
        let w = fonts.layout_no_wrap(l.text.clone(), font.clone(), ink).size().x;
        let lead = match l.align {
            Align::Left => 0.0,
            Align::Center => (width - w) / 2.0,
            Align::Right => width - w,
        };
        let color = if bold_only && !l.bold { Color32::TRANSPARENT } else { ink };
        let underline = if l.underline && !bold_only { Stroke::new((l.size / 14.0).max(1.0), ink) } else { Stroke::NONE };
        let format = egui::TextFormat { font_id: font, color, italics: l.italic, underline, ..Default::default() };
        job.append(&l.text, lead.max(0.0), format.clone());
        if k + 1 < lines.len() {
            job.append("\n", 0.0, format);
        }
    }
    job
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

/// Finestra "Timbro": nuovo (di testo o da immagine) oppure in modifica (`index`).
struct StampEdit {
    index: Option<usize>,
    stamp: Stamp,
    from_image: bool,
    import: Option<Import>,
    /// Il testo del timbro nel campo unico; `lines` ne tiene la formattazione, una per riga.
    text: String,
    lines: Vec<Line>,
    /// Prima e ultima riga del cursore o della selezione: lì si applica la formattazione.
    cur: (usize, usize),
    /// Dopo un clic su un pulsante della barra il cursore torna nel campo, al fotogramma dopo:
    /// in quello del clic egui toglie il fuoco a chi non è stato cliccato.
    refocus: bool,
    /// Anteprima del timbro e il testo (con il colore) da cui è stata fatta.
    preview: Option<(String, TextureHandle)>,
}

impl StampEdit {
    fn new(index: Option<usize>, stamp: Stamp) -> Self {
        let lines: Vec<Line> = stamp.text.split('\n').map(Line::parse).collect();
        let text = lines.iter().map(|l| l.text.as_str()).collect::<Vec<_>>().join("\n");
        Self { index, stamp, from_image: false, import: None, text, lines, cur: (0, 0), refocus: false, preview: None }
    }
}

/// Oggetto posato sulla pagina e non ancora impresso, con la sua anteprima a video.
#[derive(Clone)]
struct Overlay {
    item: Placed,
    tex: TextureHandle,
    /// Altezza / larghezza del contenuto dritto.
    aspect: f32,
}

/// Strumento attivo, in attesa del clic sulla pagina: un timbro dell'elenco o (None) il testo.
struct Tool {
    stamp: Option<usize>,
    content: Rc<Content>,
    tex: TextureHandle,
    aspect: f32,
    width: f32,
}

/// Un passo da annullare: se ha toccato il documento (la cui storia sta in `Doc`)
/// e com'erano gli oggetti posati prima.
struct Step {
    doc: bool,
    items: Vec<Overlay>,
}

fn push(history: &mut Vec<Step>, step: Step) {
    if history.len() == 100 {
        history.remove(0);
    }
    history.push(step);
}

/// Anteprima a video di ciò che verrà impresso.
fn overlay_texture(ctx: &egui::Context, c: &Content) -> Res<TextureHandle> {
    let img = match c {
        Content::Image(img) => {
            let img = img.to_rgba8();
            egui::ColorImage::from_rgba_unmultiplied([img.width() as usize, img.height() as usize], &img)
        }
        _ => {
            let (w, h, px) = Doc::preview(c, 800)?;
            egui::ColorImage::from_rgba_premultiplied([w, h], &px)
        }
    };
    Ok(ctx.load_texture("oggetto", img, Default::default()))
}

fn aspect(c: &Content) -> f32 {
    let (w, h) = c.size();
    h / w
}

/// Larghezza iniziale in punti: le immagini 5 cm, i testi al loro corpo naturale.
fn natural_width(c: &Content) -> f32 {
    match c {
        Content::Image(_) => 50.0 * 72.0 / 25.4,
        _ => c.size().0,
    }
}

impl Tool {
    fn new(ctx: &egui::Context, stamp: Option<usize>, c: Content) -> Res<Self> {
        Ok(Self { stamp, tex: overlay_texture(ctx, &c)?, aspect: aspect(&c), width: natural_width(&c), content: Rc::new(c) })
    }
}

/// Pulsante a icona; la descrizione compare passandoci sopra, anche quando è disattivato.
fn icon(ui: &mut egui::Ui, glyph: &str, tip: &str, enabled: bool) -> bool {
    let b = egui::Button::new(RichText::new(glyph).size(18.0)).min_size(vec2(30.0, 28.0));
    ui.add_enabled(enabled, b).on_hover_text(tip).on_disabled_hover_text(tip).clicked()
}

/// Come `icon`, ma resta evidenziato finché è attivo.
fn toggle(ui: &mut egui::Ui, glyph: &str, tip: &str, on: bool, enabled: bool) -> bool {
    let b = egui::Button::selectable(on, RichText::new(glyph).size(18.0)).min_size(vec2(30.0, 28.0));
    ui.add_enabled(enabled, b).on_hover_text(tip).on_disabled_hover_text(tip).clicked()
}

/// Comando con icona e una parola sotto, per l'area Modifica.
fn labeled(ui: &mut egui::Ui, glyph: &str, word: &str, tip: &str, on: bool, enabled: bool) -> bool {
    let (rect, resp) = ui.allocate_exact_size(vec2(58.0, 48.0), if enabled { Sense::click() } else { Sense::hover() });
    resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::Button, enabled, on, word));
    let v = ui.visuals();
    if on {
        ui.painter().rect_filled(rect, 4.0, v.selection.bg_fill);
    } else if enabled && resp.hovered() {
        ui.painter().rect_filled(rect, 4.0, v.widgets.hovered.weak_bg_fill);
    }
    let color = if enabled { v.text_color() } else { v.text_color().gamma_multiply(v.disabled_alpha) };
    ui.painter().text(rect.center_top() + vec2(0.0, 17.0), egui::Align2::CENTER_CENTER, glyph, egui::FontId::proportional(20.0), color);
    ui.painter().text(rect.center_bottom() - vec2(0.0, 10.0), egui::Align2::CENTER_CENTER, word, egui::FontId::proportional(11.0), color);
    resp.on_hover_text(tip).clicked() && enabled
}

/// Area della barra, come i gruppi delle ribbon: i comandi in riga, il nome sotto.
fn group(ui: &mut egui::Ui, name: &str, add: impl FnOnce(&mut egui::Ui)) -> Rect {
    ui.vertical(|ui| {
        let row = ui.horizontal(|ui| {
            ui.set_min_height(48.0);
            add(ui)
        });
        let (r, _) = ui.allocate_exact_size(vec2(row.response.rect.width(), 13.0), Sense::hover());
        ui.painter().text(r.center(), egui::Align2::CENTER_CENTER, name, egui::FontId::proportional(11.0), ui.visuals().weak_text_color());
    })
    .response
    .rect
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
    /// Attiva o disattiva un timbro dell'elenco, o (None) il testo libero.
    Tool(Option<usize>),
    /// Posa l'oggetto dello strumento attivo sulla pagina, nel punto a video (u, v).
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
    /// Anteprime dell'elenco firme e timbri (None se il contenuto non si apre).
    tiles: HashMap<usize, Option<TextureHandle>>,
    /// Finestra di gestione di firme e timbri aperta.
    manage: bool,
    tool: Option<Tool>,
    /// Firme, timbri e testi posati e non ancora impressi: lo diventano al salvataggio.
    items: Vec<Overlay>,
    /// Oggetto selezionato sulla pagina.
    picked: Option<usize>,
    /// Testo libero in modifica: oggetto, testo, colore, corpo.
    note: Option<(usize, String, [u8; 3], f32)>,
    history: Vec<Step>,
    /// Larghezza dell'area Visualizza al fotogramma precedente, per centrarla.
    bar_mid: f32,
    bar_right: f32,
    /// Firma in corso di disegno: nome con cui salvarla e tratti.
    sig: Option<NewSig>,
    edit: Option<StampEdit>,
    msg: String,
    title: String,
    confirm: Option<Pending>,
}

/// Finestre di firme e timbri: ancorate in alto, così cambiando scheda non saltano.
fn window_top(ctx: &egui::Context) -> Pos2 {
    ctx.content_rect().center() - vec2(0.0, 220.0)
}

/// Salva un PNG nella cartella di configurazione: firme e timbri non dipendono dal file originale.
fn save_png(img: &image::RgbaImage) -> Result<PathBuf, String> {
    let dest = config_dir().join(format!("img-{}.png", chrono::Local::now().format("%Y%m%d-%H%M%S")));
    img.save(&dest).map(|_| dest).map_err(|e| e.to_string())
}

/// Salva l'immagine importata: il solo tratto estratto, o una copia del file se l'estrazione è spenta.
fn save_import(imp: &Import) -> Result<PathBuf, String> {
    match &imp.result {
        Some((img, _)) if imp.extract => save_png(img),
        _ => {
            let ext = imp.src.extension().and_then(|e| e.to_str()).unwrap_or("png");
            let dest = config_dir().join(format!("img-{}.{ext}", chrono::Local::now().format("%Y%m%d-%H%M%S")));
            std::fs::copy(&imp.src, &dest).map(|_| dest).map_err(|e| e.to_string())
        }
    }
}

/// Scheda "Da immagine", comune a firme e timbri: scelta del file, estrazione del tratto,
/// anteprima. Restituisce true quando c'è un'immagine pronta da salvare.
fn image_tab(ui: &mut egui::Ui, import: &mut Option<Import>, msg: &mut String, empty: &str) -> bool {
    let pick = ui.button(if import.is_some() { "Cambia immagine…" } else { "Scegli immagine…" });
    if pick.clicked()
        && let Some(src) = rfd::FileDialog::new().add_filter("Immagini", &["png", "jpg", "jpeg"]).pick_file()
    {
        match load_image(&src) {
            Ok(img) => {
                let scan = ink::Scan::new(&img);
                let (threshold, color) = (scan.auto, scan.ink_color(scan.auto));
                *import = Some(Import { src, img, scan, extract: true, threshold, color, result: None });
            }
            Err(e) => *msg = format!("Immagine non leggibile: {e}"),
        }
    }
    if let Some(imp) = import.as_mut() {
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
                for (text, c) in [("nero", [0, 0, 0]), ("blu", [15, 35, 130]), ("rosso", [190, 20, 20])] {
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
    let note = |text: &str| p.text(rect.center(), egui::Align2::CENTER_CENTER, text, egui::FontId::proportional(14.0), Color32::DARK_GRAY);
    match import {
        Some(Import { result: Some((_, t)), .. }) => {
            let size = t.size_vec2();
            let fit = Rect::from_center_size(rect.center(), size * ((rect.width() - 16.0) / size.x).min((rect.height() - 16.0) / size.y).min(1.0));
            p.image(t.id(), fit, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
        }
        Some(_) => _ = note("Nessun tratto: abbassa la pulizia"),
        None => _ = note(empty),
    }
    import.as_ref().is_some_and(|i| i.result.is_some())
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
            fit: Fit::Height,
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
            tiles: HashMap::new(),
            manage: false,
            tool: None,
            items: vec![],
            picked: None,
            note: None,
            history: vec![],
            bar_mid: 0.0,
            bar_right: 0.0,
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
        self.doc.as_ref().is_some_and(|d| d.dirty) || !self.items.is_empty()
    }

    fn stamps_changed(&mut self) {
        save_stamps(&self.stamps);
        self.tiles.clear();
    }

    fn placed(&self) -> Vec<Placed> {
        self.items.iter().map(|o| o.item.clone()).collect()
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
                (self.items, self.picked, self.note, self.history) = (vec![], None, None, vec![]);
                (self.fit, self.goto) = (Fit::Height, Some(0));
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
        // Gli oggetti posati vengono impressi ora; "Annulla" li riporta modificabili.
        let placed = self.items.iter().map(|o| o.item.clone()).collect::<Vec<_>>();
        if let Err(e) = doc.flatten(&placed) {
            self.msg = format!("Salvataggio fallito: {e}");
            return false;
        }
        let result = doc.save(&path);
        if !placed.is_empty() {
            push(&mut self.history, Step { doc: true, items: std::mem::take(&mut self.items) });
            (self.picked, self.note) = (None, None);
            self.refresh();
        }
        match result {
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
        // Scartato (o appena salvato): documento e oggetti posati, niente più modifiche in sospeso.
        (self.doc, self.items) = (None, vec![]);
        match pending {
            Some(Pending::Open(p)) => self.open(p),
            _ => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
        }
    }

    fn apply(&mut self, ctx: &egui::Context, act: Act) {
        let (file, cur, pages, placed) = (self.file_name(), self.cur, self.targets(), self.placed());
        let Some(d) = &mut self.doc else { return };
        let structural = matches!(act, Act::Rotate(_) | Act::Move(..) | Act::Delete | Act::Insert(_) | Act::Replace(_));
        let edits_doc = structural || matches!(act, Act::Field(..));
        if matches!(act, Act::Move(..) | Act::Delete | Act::Undo | Act::Insert(_) | Act::Replace(_)) {
            self.sel.clear(); // gli indici selezionati non indicano più le stesse pagine
        }
        let (before, is_undo) = (self.items.clone(), matches!(act, Act::Undo));
        let mut undone_doc = false;
        let mut done = String::new();
        let result: Res<()> = (|| {
            match act {
                Act::Rotate(q) => {
                    d.rotate(&pages, q)?;
                    // La vista va sulla pagina ruotata: l'adattamento segue la pagina corrente.
                    self.cur = pages[0];
                }
                Act::Move(from, to) => {
                    d.move_page(from, to)?;
                    self.cur = to;
                }
                Act::Delete => d.delete(&pages)?,
                Act::Undo => {
                    if let Some(step) = self.history.pop() {
                        if step.doc {
                            d.undo()?;
                            undone_doc = true;
                        }
                        (self.items, self.picked, self.note) = (step.items, None, None);
                    }
                }
                Act::Insert(p) => d.insert_pdf(&p, cur + 1)?,
                Act::Extract(p) => {
                    pdf::write(&p, &d.extract(&pages, &placed)?)?;
                    let what = if pages.len() == 1 { "1 pagina estratta".into() } else { format!("{} pagine estratte", pages.len()) };
                    done = format!("{what} in {}", p.display());
                }
                Act::Replace(p) => {
                    d.replace(&pages, &p)?;
                    self.cur = pages[0];
                }
                Act::Tool(stamp) => {
                    self.tool = match &self.tool {
                        Some(t) if t.stamp == stamp => None,
                        _ => {
                            let c = match stamp {
                                Some(i) => content(&self.stamps[i], &file, cur + 1, d.len())?,
                                None => Content::Note { text: "Testo".into(), color: [0, 0, 0], size: 12.0 },
                            };
                            Some(Tool::new(ctx, stamp, c)?)
                        }
                    };
                    self.picked = None;
                }
                Act::Field(id, value) => d.set_field(id, &value)?,
                Act::Place(page, u, v) => {
                    if let Some(t) = self.tool.take() {
                        let (x, y) = d.to_user(page, u, v).ok_or("pagina inesistente")?;
                        // I segnaposto ({pagina}, {data}...) valgono per la pagina e il momento in cui si posa.
                        let (content, tex) = match t.stamp {
                            Some(i) if !self.stamps[i].text.starts_with("img:") => {
                                let c = content(&self.stamps[i], &file, page + 1, d.len())?;
                                let tex = overlay_texture(ctx, &c)?;
                                (Rc::new(c), tex)
                            }
                            _ => (t.content, t.tex),
                        };
                        let item = Placed { page: d.page_key(page).ok_or("pagina inesistente")?, x, y, width: t.width, rot: d.page_rotation(page), content };
                        self.items.push(Overlay { item, tex, aspect: t.aspect });
                        self.picked = Some(self.items.len() - 1);
                        if let Content::Note { text, color, size } = &*self.items[self.items.len() - 1].item.content {
                            self.note = Some((self.items.len() - 1, text.clone(), *color, *size));
                        }
                    }
                }
            }
            Ok(())
        })();
        self.cur = self.cur.min(d.len().saturating_sub(1));
        if result.is_ok() && (edits_doc || (!is_undo && self.items.len() != before.len())) {
            push(&mut self.history, Step { doc: edits_doc, items: before });
        }
        self.msg = result.err().map_or(done, |e| e.to_string());
        if edits_doc || undone_doc {
            self.refresh();
        }
        if structural || undone_doc {
            self.goto = Some(self.cur); // la struttura è cambiata: resta sulla pagina corrente
        }
    }

    /// Barra a tre aree, come una ribbon: File a sinistra, Visualizza al centro, Modifica a destra.
    fn toolbar(&mut self, ui: &mut egui::Ui) -> Option<Act> {
        let mut act = None;
        let n = self.doc.as_ref().map_or(0, Doc::len);
        let has = n > 0;
        // Finestra stretta (es. 1024 px): anche Modifica passa alle sole icone, per starci tutta.
        let compact = ui.available_width() < 1100.0;
        let labeled = |ui: &mut egui::Ui, glyph: &str, word: &str, tip: &str, on: bool, enabled: bool| {
            if compact { toggle(ui, glyph, &format!("{word}: {tip}"), on, enabled) } else { labeled(ui, glyph, word, tip, on, enabled) }
        };
        ui.horizontal(|ui| {
            group(ui, "File", |ui| {
                if icon(ui, ph::FOLDER_OPEN, "Apri (Ctrl+O)", true)
                    && let Some(p) = pdf_dialog(&self.path).pick_file()
                {
                    self.open(p);
                }
                if icon(ui, ph::FLOPPY_DISK, "Salva (Ctrl+S): firme, timbri e testi posati vengono impressi nel PDF", has) {
                    self.save(false);
                }
                if icon(ui, ph::FLOPPY_DISK_BACK, "Salva con nome…", has) {
                    self.save(true);
                }
            });
            ui.separator();
            // Visualizza al centro dello spazio libero, Modifica in fondo a destra: si usano le
            // larghezze delle due aree al fotogramma precedente.
            let slack = 4.0 * ui.spacing().item_spacing.x + 8.0;
            let free = (ui.available_width() - self.bar_mid - self.bar_right - slack).max(0.0);
            ui.add_space(free / 2.0);
            let mid = group(ui, "Visualizza", |ui| {
                let mut percent = self.zoom * 100.0;
                if icon(ui, ph::MAGNIFYING_GLASS_MINUS, "Riduci (Ctrl −  oppure Ctrl + rotella)", has) {
                    percent /= 1.25;
                }
                let field = egui::DragValue::new(&mut percent).range(10.0..=640.0).fixed_decimals(0).suffix("%");
                ui.add_enabled(has, field).on_hover_text("Zoom: trascina, oppure clicca e scrivi il valore");
                if icon(ui, ph::MAGNIFYING_GLASS_PLUS, "Ingrandisci (Ctrl +  oppure Ctrl + rotella)", has) {
                    percent *= 1.25;
                }
                if percent != self.zoom * 100.0 {
                    (self.zoom, self.fit) = (percent / 100.0, Fit::Free);
                }
                for (fit, glyph, tip) in [(Fit::Width, ph::ARROWS_OUT_LINE_HORIZONTAL, "Adatta alla larghezza"), (Fit::Height, ph::ARROWS_OUT_LINE_VERTICAL, "Adatta all'altezza")] {
                    if toggle(ui, glyph, tip, self.fit == fit, has) {
                        self.fit = if self.fit == fit { Fit::Free } else { fit };
                        self.goto = Some(self.cur); // riallinea la pagina corrente in cima alla vista
                    }
                }
                let mut page = self.cur + 1;
                let field = egui::DragValue::new(&mut page).range(1..=n.max(1)).speed(0.05).update_while_editing(false).suffix(format!(" / {n}"));
                if ui.add_enabled(has, field).on_hover_text("Pagina: clicca, scrivi il numero e premi Invio").changed() {
                    (self.cur, self.goto, self.follow) = (page - 1, Some(page - 1), true);
                }
            });
            self.bar_mid = mid.width();
            ui.add_space(free / 2.0);
            ui.separator();
            let right = group(ui, "Modifica", |ui| {
                let text_on = self.tool.as_ref().is_some_and(|t| t.stamp.is_none());
                if labeled(ui, ph::TEXT_T, "Testo", "Aggiungi testo: poi clicca sulla pagina", text_on, has) {
                    act = Some(Act::Tool(None));
                }
                ui.separator();
                let k = self.targets().len();
                let which = "la pagina corrente, o quelle selezionate con Ctrl/Maiusc + clic sulle miniature (Ctrl+A tutte)";
                if labeled(ui, ph::ARROW_COUNTER_CLOCKWISE, "Antiorario", &format!("Ruota in senso antiorario {which}"), false, has) {
                    act = Some(Act::Rotate(-1));
                }
                if labeled(ui, ph::ARROW_CLOCKWISE, "Orario", &format!("Ruota in senso orario {which}"), false, has) {
                    act = Some(Act::Rotate(1));
                }
                if labeled(ui, ph::ARROW_UP, "Su", "Sposta su la pagina corrente", false, has && self.cur > 0) {
                    act = Some(Act::Move(self.cur, self.cur - 1));
                }
                if labeled(ui, ph::ARROW_DOWN, "Giù", "Sposta giù la pagina corrente", false, self.cur + 1 < n) {
                    act = Some(Act::Move(self.cur, self.cur + 1));
                }
                if labeled(ui, ph::TRASH, "Elimina", &format!("Elimina {which}"), false, has && k < n) {
                    act = Some(Act::Delete);
                }
                ui.separator();
                if labeled(ui, ph::FILE_PLUS, "Inserisci", "Inserisci un altro PDF dopo la pagina corrente", false, has)
                    && let Some(p) = pdf_dialog(&self.path).pick_file()
                {
                    act = Some(Act::Insert(p));
                }
                let stem = self.path.as_ref().and_then(|p| p.file_stem()).map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                if labeled(ui, ph::EXPORT, "Estrai", "Estrai in un nuovo PDF la pagina corrente o quelle selezionate", false, has)
                    && let Some(p) = pdf_dialog(&self.path).set_file_name(format!("{stem}-estratto.pdf")).save_file()
                {
                    act = Some(Act::Extract(p));
                }
                if labeled(ui, ph::SWAP, "Sostituisci", "Sostituisci la pagina corrente o quelle selezionate con un altro PDF", false, has)
                    && let Some(p) = pdf_dialog(&self.path).pick_file()
                {
                    act = Some(Act::Replace(p));
                }
                ui.separator();
                if labeled(ui, ph::ARROW_U_UP_LEFT, "Annulla", "Annulla l'ultima modifica (Ctrl+Z)", false, !self.history.is_empty()) {
                    act = Some(Act::Undo);
                }
            });
            self.bar_right = right.width();
        });
        act
    }

    /// Anteprima di una voce dell'elenco, creata la prima volta che serve.
    fn tile(&mut self, ctx: &egui::Context, i: usize) -> Option<TextureHandle> {
        if !self.tiles.contains_key(&i) {
            let n = self.doc.as_ref().map_or(1, Doc::len);
            let tex = content(&self.stamps[i], &self.file_name(), self.cur + 1, n).ok().and_then(|c| overlay_texture(ctx, &c).ok());
            self.tiles.insert(i, tex);
        }
        self.tiles[&i].clone()
    }

    /// Elenco sempre a vista: un clic sceglie la firma o il timbro da posare.
    /// Aggiunte, modifiche ed eliminazioni stanno nella finestra di gestione.
    fn stamps_panel(&mut self, ui: &mut egui::Ui) -> Option<Act> {
        let mut act = None;
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.strong("Firme e timbri");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if icon(ui, ph::GEAR, "Gestisci firme e timbri: aggiungi, modifica, elimina", true) {
                    self.manage = true;
                }
            });
        });
        ui.separator();
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            if self.stamps.is_empty() {
                ui.weak(format!("Nessuna firma o timbro: aggiungili con {} in alto.", ph::GEAR));
            }
            for i in 0..self.stamps.len() {
                let tex = self.tile(ui.ctx(), i);
                let active = self.tool.as_ref().is_some_and(|t| t.stamp == Some(i));
                let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 76.0), Sense::click());
                let p = ui.painter_at(rect);
                let v = ui.visuals();
                let bg = if active { ACCENT.gamma_multiply(0.5) } else if resp.hovered() { v.widgets.hovered.bg_fill } else { v.widgets.inactive.bg_fill };
                p.rect_filled(rect.shrink(2.0), 6.0, bg);
                // Cartoncino bianco: l'inchiostro scuro resta leggibile anche col tema scuro.
                let card = Rect::from_min_max(rect.min + vec2(8.0, 8.0), Pos2::new(rect.max.x - 8.0, rect.max.y - 24.0));
                p.rect_filled(card, 4.0, Color32::WHITE);
                if let Some(t) = tex {
                    let size = t.size_vec2();
                    let fit = size * ((card.width() - 8.0) / size.x).min((card.height() - 6.0) / size.y);
                    p.image(t.id(), Rect::from_center_size(card.center(), fit), Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
                }
                let name = &self.stamps[i].name;
                p.text(Pos2::new(rect.center().x, rect.max.y - 13.0), egui::Align2::CENTER_CENTER, name, egui::FontId::proportional(13.0), v.text_color());
                let tip = if self.doc.is_some() { format!("{name}: clicca, poi clicca sulla pagina per posarlo") } else { format!("{name}: apri un PDF per usarlo") };
                if resp.on_hover_text(tip).clicked() && self.doc.is_some() {
                    act = Some(Act::Tool(Some(i)));
                }
            }
        });
        act
    }

    fn manage_window(&mut self, ctx: &egui::Context) {
        if !self.manage {
            return;
        }
        let (mut open, mut remove) = (true, None);
        egui::Window::new("Firme e timbri").open(&mut open).collapsible(false).resizable(false).pivot(egui::Align2::CENTER_TOP).default_pos(window_top(ctx)).show(ctx, |ui| {
            ui.horizontal(|ui| {
                if ui.button(format!("{} Aggiungi firma…", ph::SIGNATURE)).on_hover_text("Disegnala col mouse o ricavala da una foto o scansione").clicked() {
                    self.sig = Some(NewSig { name: "Firma".into(), from_image: false, strokes: vec![], import: None });
                }
                if ui.button(format!("{} Nuovo timbro…", ph::SEAL)).on_hover_text("Timbro di testo, anche con data e ora, oppure da immagine").clicked() {
                    let stamp = Stamp { name: "Nuovo timbro".into(), color: [190, 20, 20], text: "TESTO\n{data}".into() };
                    self.edit = Some(StampEdit::new(None, stamp));
                }
            });
            ui.separator();
            egui::ScrollArea::vertical().max_height(420.0).show(ui, |ui| {
                for (i, s) in self.stamps.iter().enumerate() {
                    ui.horizontal(|ui| {
                        let glyph = if s.text.starts_with("img:") { ph::IMAGE } else { ph::TEXT_T };
                        ui.add_sized(vec2(300.0, 24.0), egui::Label::new(format!("{glyph}  {}", s.name)).truncate());
                        if icon(ui, ph::PENCIL_SIMPLE, "Modifica", true) {
                            self.edit = Some(StampEdit::new(Some(i), s.clone()));
                        }
                        if icon(ui, ph::TRASH, "Elimina", true) {
                            remove = Some(i);
                        }
                    });
                }
            });
        });
        if let Some(i) = remove {
            self.stamps.remove(i);
            self.tool = None;
            self.stamps_changed();
        }
        if !open {
            self.manage = false;
        }
    }

    /// Testo libero selezionato: testo, corpo e colore, applicati man mano.
    fn note_window(&mut self, ctx: &egui::Context) {
        let Some((k, text, color, size)) = &mut self.note else { return };
        let (mut open, mut changed, mut done) = (true, false, false);
        egui::Window::new("Testo").open(&mut open).collapsible(false).resizable(false).pivot(egui::Align2::CENTER_TOP).default_pos(window_top(ctx)).show(ctx, |ui| {
            let edit = ui.add(egui::TextEdit::multiline(text).desired_rows(3).desired_width(320.0));
            if !edit.has_focus() && ui.memory(|m| m.focused().is_none()) {
                edit.request_focus();
            }
            changed |= edit.changed();
            ui.horizontal(|ui| {
                changed |= ui.add(egui::DragValue::new(size).range(6.0..=72.0).suffix(" pt")).on_hover_text("Corpo del testo").changed();
                changed |= ui.color_edit_button_srgb(color).on_hover_text("Colore").changed();
                ui.weak("Solo alfabeto latino");
            });
            done = ui.button("Fatto").clicked();
        });
        if changed && let Some(o) = self.items.get_mut(*k) {
            let c = Content::Note { text: text.clone(), color: *color, size: *size };
            // La larghezza segue il testo, mantenendo l'ingrandimento scelto sulla pagina.
            let scale = o.item.width / o.item.content.size().0;
            if let Ok(tex) = overlay_texture(ctx, &c) {
                (o.item.width, o.aspect, o.tex) = (c.size().0 * scale, aspect(&c), tex);
                o.item.content = Rc::new(c);
            }
        }
        if done || !open {
            self.note = None;
        }
    }

    fn signature_window(&mut self, ctx: &egui::Context) {
        let Some(sig) = &mut self.sig else { return };
        let (mut open, mut done) = (true, false);
        // In primo piano: si apre anche dalla finestra di gestione e deve stare sopra.
        egui::Window::new("Aggiungi firma").order(egui::Order::Foreground).open(&mut open).collapsible(false).resizable(false).pivot(egui::Align2::CENTER_TOP).default_pos(window_top(ctx)).show(ctx, |ui| {
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
                image_tab(ui, &mut sig.import, &mut self.msg, "Scegli una foto o una scansione della firma (PNG o JPG)")
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
            let saved = match &sig.import {
                Some(imp) if sig.from_image => save_import(imp),
                _ => rasterize(&sig.strokes).ok_or_else(|| "firma vuota".to_string()).and_then(|img| save_png(&img)),
            };
            match saved {
                Ok(dest) => {
                    self.stamps.push(Stamp { name: sig.name.trim().to_string(), color: INK, text: format!("img:{}", dest.display()) });
                    self.stamps_changed();
                }
                Err(e) => self.msg = format!("Salvataggio firma fallito: {e}"),
            }
        }
        if done || !open {
            self.sig = None;
        }
    }

    fn editor_window(&mut self, ctx: &egui::Context) {
        let Some(ed) = &mut self.edit else { return };
        let (mut open, mut done) = (true, false);
        let title = match (&ed.index, ed.stamp.text.starts_with("img:")) {
            (None, _) => "Nuovo timbro",
            (Some(_), true) => "Modifica immagine",
            (Some(_), false) => "Modifica timbro",
        };
        egui::Window::new(title).id(egui::Id::new("timbro")).order(egui::Order::Foreground).open(&mut open).collapsible(false).resizable(false).pivot(egui::Align2::CENTER_TOP).default_pos(window_top(ctx)).show(ctx, |ui| {
            let s = &mut ed.stamp;
            ui.horizontal(|ui| {
                ui.label("Nome");
                ui.text_edit_singleline(&mut s.name);
            });
            // Un timbro nuovo può nascere da testo o da un'immagine; uno esistente resta com'è.
            if ed.index.is_none() {
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut ed.from_image, false, "Testo");
                    ui.selectable_value(&mut ed.from_image, true, "Da immagine").on_hover_text("Scansione di un timbro a inchiostro, oppure un logo");
                });
                ui.separator();
            }
            let ready = if ed.from_image {
                image_tab(ui, &mut ed.import, &mut self.msg, "Scegli l'immagine del timbro (PNG o JPG)")
            } else if s.text.starts_with("img:") {
                ui.label("Timbro grafico (immagine).");
                true
            } else {
                // Barra di formattazione: mostra quella della riga del cursore e la applica
                // a tutte le righe selezionate, come in un elaboratore di testi.
                let id = egui::Id::new("testo timbro");
                if std::mem::take(&mut ed.refocus) {
                    ui.memory_mut(|m| m.request_focus(id));
                }
                let (a, b) = (ed.cur.0.min(ed.lines.len() - 1), ed.cur.1.min(ed.lines.len() - 1));
                let was = ed.lines[a].clone();
                let mut f = was.clone();
                ui.horizontal(|ui| {
                    ui.add(egui::DragValue::new(&mut f.size).range(4.0..=96.0).fixed_decimals(0).suffix(" pt")).on_hover_text("Dimensione del carattere");
                    for (on, glyph, tip) in [(&mut f.bold, ph::TEXT_B, "Grassetto"), (&mut f.italic, ph::TEXT_ITALIC, "Corsivo"), (&mut f.underline, ph::TEXT_UNDERLINE, "Sottolineato")] {
                        if toggle(ui, glyph, tip, *on, true) {
                            (*on, ed.refocus) = (!*on, true);
                        }
                    }
                    ui.separator();
                    for (al, glyph, tip) in [(Align::Left, ph::TEXT_ALIGN_LEFT, "A sinistra"), (Align::Center, ph::TEXT_ALIGN_CENTER, "Al centro"), (Align::Right, ph::TEXT_ALIGN_RIGHT, "A destra")] {
                        if toggle(ui, glyph, tip, f.align == al, true) {
                            (f.align, ed.refocus) = (al, true);
                        }
                    }
                    ui.separator();
                    ui.label("Colore");
                    ui.color_edit_button_srgb(&mut s.color);
                });
                if f != was {
                    for l in &mut ed.lines[a..=b] {
                        (f.size != was.size).then(|| l.size = f.size);
                        (f.bold != was.bold).then(|| l.bold = f.bold);
                        (f.italic != was.italic).then(|| l.italic = f.italic);
                        (f.underline != was.underline).then(|| l.underline = f.underline);
                        (f.align != was.align).then(|| l.align = f.align);
                    }
                }
                // Il campo: foglio bianco, inchiostro del colore del timbro.
                let ink = Color32::from_rgb(s.color[0], s.color[1], s.color[2]);
                let (old, lines) = (ed.text.clone(), &ed.lines);
                let wide = std::cell::Cell::new(0.0); // larghezza usata dal campo, per ripassare il grassetto
                let mut layouter = |ui: &egui::Ui, buf: &dyn egui::TextBuffer, width: f32| {
                    wide.set(width);
                    ui.fonts_mut(|fonts| {
                        let job = stamp_job(fonts, &remap(&old, buf.as_str(), lines), width, ink, false);
                        fonts.layout_job(job)
                    })
                };
                let out = ui
                    .scope(|ui| {
                        ui.visuals_mut().text_cursor.stroke.color = Color32::BLACK;
                        egui::TextEdit::multiline(&mut ed.text).id(id).desired_width(440.0).desired_rows(4).background_color(Color32::WHITE).layouter(&mut layouter).show(ui)
                    })
                    .inner;
                if ed.text != old {
                    ed.text.retain(|c| c != '\r');
                    ed.lines = remap(&old, &ed.text, &ed.lines);
                }
                // Il grassetto: le righe in grassetto vengono ripassate spostate di poco.
                if ed.lines.iter().any(|l| l.bold) {
                    let bold = ui.fonts_mut(|fonts| {
                        let job = stamp_job(fonts, &ed.lines, wide.get(), ink, true);
                        fonts.layout_job(job)
                    });
                    for dx in [0.5, 1.0] {
                        ui.painter().galley(out.galley_pos + vec2(dx, 0.0), bold.clone(), ink);
                    }
                }
                // Righe del cursore solo mentre si scrive: un clic sulla barra non deve cambiarle.
                if let Some(r) = out.cursor_range.filter(|_| ui.memory(|m| m.has_focus(id))) {
                    let r = r.as_sorted_char_range();
                    let line = |i: usize| ed.text.chars().take(i).filter(|&c| c == '\n').count();
                    ed.cur = (line(r.start.0), line(r.end.0));
                }
                ui.weak("Campi dinamici: {data} {ora} {utente} {file} {pagina} {pagine}");
                s.text = ed.lines.iter().map(Line::to_string).collect::<Vec<_>>().join("\n");
                let key = format!("{:?}{}", s.color, s.text);
                if ed.preview.as_ref().is_none_or(|(k, _)| *k != key) {
                    ed.preview = content(s, "documento.pdf", 1, 1).and_then(|c| overlay_texture(ui.ctx(), &c)).ok().map(|t| (key, t));
                }
                if let Some((_, t)) = &ed.preview {
                    // Su cartoncino bianco, come nell'elenco a destra.
                    let size = t.size_vec2();
                    let fit = size * (380.0 / size.x).min(150.0 / size.y);
                    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width().max(fit.x + 16.0), fit.y + 16.0), Sense::hover());
                    ui.painter().rect_filled(rect, 4.0, Color32::WHITE);
                    ui.painter().image(t.id(), Rect::from_center_size(rect.center(), fit), Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
                }
                ed.lines.iter().any(|l| !l.text.trim().is_empty())
            };
            done = ui.add_enabled(ready && !s.name.trim().is_empty(), egui::Button::new("Salva")).clicked();
        });
        if done && let Some(imp) = ed.import.as_ref().filter(|_| ed.from_image) {
            match save_import(imp) {
                Ok(dest) => ed.stamp.text = format!("img:{}", dest.display()),
                Err(e) => {
                    self.msg = format!("Salvataggio timbro fallito: {e}");
                    done = false;
                }
            }
        }
        if done {
            match ed.index {
                Some(i) => self.stamps[i] = ed.stamp.clone(),
                None => self.stamps.push(ed.stamp.clone()),
            }
            self.tool = None;
            self.stamps_changed();
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

    fn remove_item(&mut self, k: usize) {
        push(&mut self.history, Step { doc: false, items: self.items.clone() });
        self.items.remove(k);
        (self.picked, self.note) = (None, None);
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

        let (placing, ppp) = (self.tool.is_some(), ui.pixels_per_point());
        let mut remove = None;
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
                for (k, f) in fields.iter().enumerate().filter(|_| !placing) {
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
                if resp.clicked() && !placing {
                    self.picked = None;
                }

                // Oggetti posati e non ancora impressi: sopra i campi, quindi ricevono loro i clic.
                let (key, page_rot) = (doc.page_key(i), doc.page_rotation(i));
                for k in 0..self.items.len() {
                    let o = &self.items[k];
                    if Some(o.item.page) != key {
                        continue;
                    }
                    let Some((u, v)) = doc.to_view(i, o.item.x, o.item.y) else { continue };
                    // La pagina può essere stata ruotata dopo la posa: l'oggetto ruota con lei.
                    let turn = (page_rot - o.item.rot).rem_euclid(360);
                    let upright = vec2(o.item.width, o.item.width * o.aspect) * zoom;
                    let center = rect.min + vec2(u, v) * zoom;
                    let bbox = Rect::from_center_size(center, if turn % 180 == 0 { upright } else { vec2(upright.y, upright.x) });
                    let tex = SizedTexture::new(o.tex.id(), upright);
                    egui::Image::new(tex).rotate((turn as f32).to_radians(), vec2(0.5, 0.5)).paint_at(ui, Rect::from_center_size(center, upright));
                    if placing {
                        continue;
                    }
                    let id = ui.id().with(("oggetto", k));
                    let grab = ui.interact(bbox, id, Sense::click_and_drag()).on_hover_cursor(egui::CursorIcon::Move);
                    if grab.clicked() || grab.drag_started() {
                        self.picked = Some(k);
                    }
                    if grab.drag_started() {
                        push(&mut self.history, Step { doc: false, items: self.items.clone() });
                    }
                    if grab.dragged() {
                        let d = grab.drag_delta() / zoom;
                        if let Some((x, y)) = doc.to_user(i, u + d.x, v + d.y) {
                            (self.items[k].item.x, self.items[k].item.y) = (x, y);
                        }
                    }
                    if grab.double_clicked()
                        && let Content::Note { text, color, size } = &*self.items[k].item.content
                    {
                        push(&mut self.history, Step { doc: false, items: self.items.clone() });
                        self.note = Some((k, text.clone(), *color, *size));
                    }
                    if self.picked != Some(k) {
                        continue;
                    }
                    painter.rect_stroke(bbox, 0.0, Stroke::new(1.5, ACCENT), StrokeKind::Outside);
                    // Maniglia nell'angolo: ridimensiona tenendo fermo il centro e le proporzioni.
                    let handle = Rect::from_center_size(bbox.max, vec2(12.0, 12.0));
                    painter.rect_filled(handle, 2.0, ACCENT);
                    let resize = ui.interact(handle, id.with("dimensione"), Sense::drag()).on_hover_cursor(egui::CursorIcon::ResizeNwSe);
                    if resize.drag_started() {
                        push(&mut self.history, Step { doc: false, items: self.items.clone() });
                    }
                    if resize.dragged() {
                        let d = resize.drag_delta();
                        let grow = ((bbox.width() + 2.0 * d.x) / bbox.width() + (bbox.height() + 2.0 * d.y) / bbox.height()) / 2.0;
                        let item = &mut self.items[k].item;
                        item.width = (item.width * grow).max(8.0);
                    }
                    let bin = Rect::from_center_size(Pos2::new(bbox.max.x, bbox.min.y), vec2(22.0, 22.0));
                    if ui.put(bin, egui::Button::new(RichText::new(ph::TRASH).size(13.0))).on_hover_text("Elimina (Canc)").clicked() {
                        remove = Some(k);
                    }
                }

                if let (Some(t), Some(pos)) = (&self.tool, resp.hover_pos()) {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::Crosshair);
                    let size = vec2(t.width, t.width * t.aspect) * zoom;
                    let ghost = Rect::from_center_size(pos, size);
                    egui::Image::new(SizedTexture::new(t.tex.id(), size)).tint(Color32::from_white_alpha(170)).paint_at(ui, ghost);
                    painter.rect_stroke(ghost, 2.0, Stroke::new(1.0, ACCENT), StrokeKind::Outside);
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
        if let Some(k) = remove {
            self.remove_item(k);
        }
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
                (self.tool, self.picked) = (None, None);
                self.sel.clear();
            }
            if (key(egui::Key::Delete) || key(egui::Key::Backspace))
                && let Some(k) = self.picked
            {
                self.remove_item(k);
            }
        }

        egui::Panel::top("barra").show(ui, |ui| {
            ui.add_space(4.0);
            act = self.toolbar(ui).or(act.take());
            ui.add_space(4.0);
        });
        egui::Panel::bottom("stato").show(ui, |ui| {
            let hint = match (&self.tool, self.picked, self.sel.len()) {
                (Some(_), _, _) => "Clicca sulla pagina per posare (Esc per annullare)".into(),
                (None, Some(_), _) => "Trascina per spostare, angolo blu per ridimensionare, Canc per eliminare, doppio clic sul testo per modificarlo".into(),
                (None, None, 0) => String::new(),
                (None, None, k) => format!("{k} pagine selezionate (Esc per deselezionare)"),
            };
            ui.label(if self.msg.is_empty() { &hint } else { &self.msg });
        });
        egui::Panel::left("pagine").default_size(180.0).show(ui, |ui| {
            act = self.thumbnails(ui).or(act.take());
        });
        egui::Panel::right("timbri").default_size(220.0).show(ui, |ui| {
            act = self.stamps_panel(ui).or(act.take());
        });
        egui::CentralPanel::default().show(ui, |ui| {
            act = self.page_view(ui).or(act.take());
        });
        self.manage_window(&ctx);
        self.note_window(&ctx);
        self.signature_window(&ctx);
        self.editor_window(&ctx);
        self.confirm_window(&ctx);

        if let Some(a) = act {
            self.apply(&ctx, a);
        }

        let dirty = if self.dirty() { "*" } else { "" };
        let title = format!("{}{dirty} — PlicoPDF", self.file_name());
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
        "PlicoPDF",
        options,
        Box::new(|cc| {
            // Ctrl +/− ingrandisce la pagina, non l'intera interfaccia.
            cc.egui_ctx.options_mut(|o| o.zoom_with_keyboard = false);
            let mut fonts = egui::FontDefinitions::default();
            egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);
            cc.egui_ctx.set_fonts(fonts);
            // Disattivato = molto sbiadito; col tema scuro testo più chiaro, pulsanti più in
            // rilievo e "attivo" nel blu pieno dell'accento.
            cc.egui_ctx.all_styles_mut(|s| s.visuals.disabled_alpha = 0.3);
            cc.egui_ctx.style_mut_of(egui::Theme::Dark, |s| {
                let w = &mut s.visuals.widgets;
                w.noninteractive.fg_stroke.color = Color32::from_gray(215);
                w.inactive.fg_stroke.color = Color32::from_gray(235);
                (w.inactive.weak_bg_fill, w.inactive.bg_fill) = (Color32::from_gray(72), Color32::from_gray(72));
                (w.hovered.weak_bg_fill, w.hovered.bg_fill) = (Color32::from_gray(95), Color32::from_gray(95));
                s.visuals.selection.bg_fill = ACCENT;
                s.visuals.selection.stroke.color = Color32::WHITE;
            });
            let mut app = App::new();
            if let Some(p) = arg {
                app.open(p);
            }
            Ok(Box::new(app))
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remap_keeps_line_formats() {
        let lines = vec![Line::parse("@bc14@TITOLO"), Line::parse("@il10@riga")];
        // A capo in fondo al titolo: la riga nuova ne prende il formato, "riga" tiene il suo.
        let after = remap("TITOLO\nriga", "TITOLO\nx\nriga", &lines);
        assert_eq!(after.iter().map(|l| (l.text.as_str(), l.bold, l.size)).collect::<Vec<_>>(), [("TITOLO", true, 14.0), ("x", true, 14.0), ("riga", false, 10.0)]);
        // Unire le due righe tiene il formato della prima.
        let joined = remap("TITOLO\nriga", "TITOLOriga", &lines);
        assert!(joined.len() == 1 && joined[0].bold && joined[0].text == "TITOLOriga");
        // Testo scritto dentro una riga: formati invariati.
        assert!(remap("TITOLO\nriga", "TITOLO\nrighe", &lines)[1] == Line { text: "righe".into(), ..lines[1].clone() });
    }
}
