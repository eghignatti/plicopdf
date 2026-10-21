//! Motore PDF: `lopdf` per modificare il documento, `hayro` per disegnarlo. Tutto Rust puro.

use std::path::Path;

use hayro::hayro_interpret::InterpreterSettings;
use hayro::hayro_syntax::Pdf;
use hayro::vello_cpu::color::palette::css::WHITE;
use hayro::{PixmapSettings, RenderCache, RenderSettings};
use image::DynamicImage;
pub use lopdf::ObjectId;
use lopdf::{Dictionary, Document, Object, Stream, decode_text_string, dictionary, text_string};

pub type Res<T> = Result<T, Box<dyn std::error::Error>>;

// Geometria del timbro di testo, in punti prima della messa in scala.
const SIZE: f32 = 14.0;
const LEAD: f32 = 18.0;
const PAD: f32 = 9.0;

/// Cosa viene impresso sulla pagina: una firma/immagine o un timbro di testo.
pub enum Content {
    Image(DynamicImage),
    Text { lines: Vec<String>, color: [u8; 3] },
}

impl Content {
    /// Ingombro (larghezza, altezza) in unità locali; conta solo il rapporto.
    pub fn size(&self) -> (f32, f32) {
        match self {
            Content::Image(img) => (100.0, 100.0 * img.height() as f32 / img.width().max(1) as f32),
            Content::Text { lines, .. } => {
                let widest = lines.iter().map(|l| text_width(l, &BOLD, SIZE)).fold(0.0, f32::max);
                (widest + 2.0 * PAD, lines.len() as f32 * LEAD + 2.0 * PAD)
            }
        }
    }
}

/// Larghezze Helvetica (millesimi di em) per i caratteri ASCII 32..=126.
#[rustfmt::skip]
const REGULAR: [u16; 95] = [
    278, 278, 355, 556, 556, 889, 667, 191, 333, 333, 389, 584, 278, 333, 278, 278,
    556, 556, 556, 556, 556, 556, 556, 556, 556, 556,
    278, 278, 584, 584, 584, 556, 1015,
    667, 667, 722, 722, 667, 611, 778, 722, 278, 500, 667, 556, 833, 722, 778, 667, 778, 722, 667, 611, 722, 667, 944, 667, 667, 611,
    278, 278, 278, 469, 556, 333,
    556, 556, 500, 556, 556, 278, 556, 556, 222, 222, 500, 222, 833, 556, 556, 556, 556, 333, 500, 278, 556, 500, 722, 500, 500, 500,
    334, 260, 334, 584,
];

/// Come sopra, per Helvetica-Bold.
#[rustfmt::skip]
const BOLD: [u16; 95] = [
    278, 333, 474, 556, 556, 889, 722, 238, 333, 333, 389, 584, 278, 333, 278, 278,
    556, 556, 556, 556, 556, 556, 556, 556, 556, 556,
    333, 333, 584, 584, 584, 611, 975,
    722, 722, 722, 722, 667, 611, 778, 722, 278, 556, 722, 611, 833, 722, 778, 667, 778, 722, 667, 611, 722, 667, 944, 667, 667, 611,
    333, 278, 333, 584, 556, 333,
    556, 611, 556, 611, 556, 333, 611, 611, 278, 278, 556, 278, 889, 611, 611, 611, 611, 389, 556, 333, 611, 556, 778, 556, 556, 500,
    389, 280, 389, 584,
];

/// Larghezza del testo al corpo dato. Le lettere accentate valgono quanto la lettera base;
/// per gli altri simboli è una stima, che incide solo su centratura e a capo.
fn text_width(s: &str, widths: &[u16; 95], size: f32) -> f32 {
    let units: u32 = s
        .chars()
        .map(|c| {
            let base = match c {
                'à'..='å' => 'a',
                'è'..='ë' => 'e',
                'ì'..='ï' => 'i',
                'ò'..='ö' => 'o',
                'ù'..='ü' => 'u',
                'À'..='Å' => 'A',
                'È'..='Ë' => 'E',
                'Ì'..='Ï' => 'I',
                'Ò'..='Ö' => 'O',
                'Ù'..='Ü' => 'U',
                _ => c,
            };
            (base as usize).checked_sub(32).and_then(|i| widths.get(i)).copied().unwrap_or(556) as u32
        })
        .sum();
    units as f32 * size / 1000.0
}

/// Spezza il testo in righe che stanno nella larghezza data, andando a capo tra le parole.
fn wrap(text: &str, max: f32, size: f32) -> Vec<String> {
    let mut lines = vec![];
    for paragraph in text.split('\n') {
        let mut line = String::new();
        for word in paragraph.split(' ') {
            let longer = if line.is_empty() { word.to_string() } else { format!("{line} {word}") };
            if !line.is_empty() && text_width(&longer, &REGULAR, size) > max {
                lines.push(std::mem::replace(&mut line, word.to_string()));
            } else {
                line = longer;
            }
        }
        lines.push(line);
    }
    lines
}

/// Testo come stringa esadecimale PDF in WinAnsi (Latin-1 più €); il resto diventa '?'.
fn hex_string(s: &str) -> String {
    let bytes = s.chars().map(|c| match c as u32 {
        0x20..=0x7e | 0xa0..=0xff => c as u8,
        0x20ac => 0x80,
        _ => b'?',
    });
    format!("<{}>", bytes.map(|b| format!("{b:02x}")).collect::<String>())
}

/// Quattro numeri di un rettangolo PDF, ordinati come [x min, y min, x max, y max].
fn rect(doc: &Document, o: &Object) -> Option<[f32; 4]> {
    let v = nums(doc, o)?;
    (v.len() == 4).then(|| [v[0].min(v[2]), v[1].min(v[3]), v[0].max(v[2]), v[1].max(v[3])])
}

fn nums(doc: &Document, o: &Object) -> Option<Vec<f32>> {
    let array = doc.dereference(o).ok()?.1.as_array().ok()?;
    array.iter().map(|n| doc.dereference(n).ok()?.1.as_float().ok()).collect()
}

fn kids_of(doc: &Document, root: ObjectId) -> Res<Vec<ObjectId>> {
    let kids = doc.get_dictionary(root)?.get(b"Kids")?.as_array()?;
    Ok(kids.iter().filter_map(|o| o.as_reference().ok()).collect())
}

/// Aggiunge alla pagina delle risorse (`kind`: "XObject" o "Font") e gli operatori che le usano.
fn draw_on_page(doc: &mut Document, id: ObjectId, kind: &str, items: Vec<(String, ObjectId)>, ops: &str) -> Res<()> {
    // Risorse e contenuti vengono copiati nella pagina prima di essere modificati:
    // possono essere condivisi con altre pagine.
    let page = doc.get_dictionary(id)?;
    let own = |d: &Dictionary, key: &[u8]| d.get_deref(key, doc).and_then(Object::as_dict).cloned().unwrap_or_default();
    let mut resources = own(page, b"Resources");
    let mut group = own(&resources, kind.as_bytes());
    for (name, resource) in items {
        group.set(name, resource);
    }
    resources.set(kind, group);
    let mut contents = match page.get(b"Contents") {
        Ok(Object::Array(a)) => a.clone(),
        Ok(Object::Reference(r)) => match doc.get_object(*r) {
            Ok(Object::Array(a)) => a.clone(),
            _ => vec![Object::Reference(*r)],
        },
        _ => vec![],
    };
    // q … Q isola il nostro disegno dallo stato grafico lasciato dal contenuto esistente.
    let open = doc.add_object(Stream::new(Dictionary::new(), b"q\n".to_vec()));
    let ours = doc.add_object(Stream::new(Dictionary::new(), format!("\nQ\n{ops}\n").into_bytes()));
    contents.insert(0, open.into());
    contents.push(ours.into());
    let page = doc.get_dictionary_mut(id)?;
    page.set("Contents", contents);
    page.set("Resources", resources);
    Ok(())
}

/// Aspetto visibile di un'annotazione: (stream, Rect, BBox, Matrix).
fn appearance(doc: &Document, annot: &Object) -> Option<(ObjectId, [f32; 4], [f32; 4], Vec<f32>)> {
    let d = doc.dereference(annot).ok()?.1.as_dict().ok()?;
    // Nascosta (bit 2) o da non mostrare a video (bit 6).
    if d.get_deref(b"F", doc).and_then(Object::as_i64).unwrap_or(0) & (2 | 32) != 0 {
        return None;
    }
    let normal = d.get_deref(b"AP", doc).ok()?.as_dict().ok()?.get(b"N").ok()?;
    let id = match doc.dereference(normal).ok()?.1 {
        Object::Stream(_) => normal.as_reference().ok()?,
        // Più stati (es. casella spuntata o no): vale quello indicato da AS.
        Object::Dictionary(states) => states.get(d.get(b"AS").ok()?.as_name().ok()?).ok()?.as_reference().ok()?,
        _ => return None,
    };
    let stream = doc.get_object(id).ok()?.as_stream().ok()?;
    let matrix = stream.dict.get(b"Matrix").ok().and_then(|m| nums(doc, m)).filter(|m| m.len() == 6);
    Some((id, rect(doc, d.get(b"Rect").ok()?)?, rect(doc, stream.dict.get(b"BBox").ok()?)?, matrix.unwrap_or(vec![1.0, 0.0, 0.0, 1.0, 0.0, 0.0])))
}

/// Toglie dall'albero le pagine agli indici dati e le azzera, così segnalibri o link che le
/// puntano non ne tengono in vita il contenuto.
// ponytail: segnalibri e campi del modulo delle pagine tolte restano, senza destinazione.
// Ripulire Outlines e AcroForm/Fields se in pratica danno fastidio.
fn drop_pages(doc: &mut Document, root: ObjectId, pages: &[usize]) -> Res<()> {
    let (gone, kept): (Vec<_>, Vec<_>) = kids_of(doc, root)?.into_iter().enumerate().partition(|(i, _)| pages.contains(i));
    for (_, id) in gone {
        doc.objects.insert(id, Object::Null);
    }
    set_kids(doc, root, kept.into_iter().map(|(_, id)| id).collect())
}

/// Scrive su file temporaneo e poi rinomina: un salvataggio fallito non rovina l'originale.
pub fn write(path: &Path, bytes: &[u8]) -> Res<()> {
    let tmp = path.with_extension("pdf.tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn set_kids(doc: &mut Document, root: ObjectId, kids: Vec<ObjectId>) -> Res<()> {
    let d = doc.get_dictionary_mut(root)?;
    d.set("Count", kids.len() as i64);
    d.set("Kids", kids.into_iter().map(Object::Reference).collect::<Vec<_>>());
    Ok(())
}

/// Carica il PDF e appiattisce l'albero delle pagine: ogni pagina diventa figlia diretta
/// della radice e riceve gli attributi che ereditava, così riordinare è spostare un elemento.
fn parse(bytes: &[u8]) -> Res<(Document, ObjectId)> {
    let mut doc = Document::load_mem(bytes)?;
    let root = doc.catalog()?.get(b"Pages")?.as_reference()?;
    let pages: Vec<ObjectId> = doc.get_pages().into_values().collect();
    let inheritable: [&[u8]; 4] = [b"Resources", b"MediaBox", b"CropBox", b"Rotate"];
    for &id in &pages {
        for key in inheritable {
            let (mut node, mut found) = (id, None);
            for _ in 0..64 {
                let d = doc.get_dictionary(node)?;
                if let Ok(v) = d.get(key) {
                    found = Some(v.clone());
                    break;
                }
                match d.get(b"Parent").and_then(Object::as_reference) {
                    Ok(parent) => node = parent,
                    Err(_) => break,
                }
            }
            if let Some(v) = found {
                doc.get_dictionary_mut(id)?.set(key, v);
            }
        }
        doc.get_dictionary_mut(id)?.set("Parent", root);
    }
    set_kids(&mut doc, root, pages)?;
    Ok((doc, root))
}

/// Scrive il documento eliminando gli oggetti non più raggiungibili (es. pagine cancellate).
fn serialize(doc: &mut Document) -> Res<Vec<u8>> {
    doc.prune_objects();
    let mut out = vec![];
    doc.save_to(&mut out)?;
    Ok(out)
}

/// Prepara il documento per il renderer. hayro non disegna le annotazioni (campi dei moduli,
/// commenti, timbri di altri programmi): qui, su una copia, i loro aspetti vengono stampati
/// nel contenuto della pagina. Il documento vero resta intatto.
fn load_view(doc: &Document, root: ObjectId) -> Res<Pdf> {
    let mut v = doc.clone();
    for page in kids_of(&v, root)? {
        let annots = match v.get_dictionary(page)?.get_deref(b"Annots", &v) {
            Ok(Object::Array(a)) => a.clone(),
            _ => continue,
        };
        let (mut items, mut ops) = (vec![], String::new());
        for annot in &annots {
            let Some((id, r, b, m)) = appearance(&v, annot) else { continue };
            // Il BBox, trasformato dalla Matrix dell'aspetto, va fatto combaciare col Rect.
            let corners = [(b[0], b[1]), (b[0], b[3]), (b[2], b[1]), (b[2], b[3])].map(|(x, y)| (m[0] * x + m[2] * y + m[4], m[1] * x + m[3] * y + m[5]));
            let (x0, y0) = corners.iter().fold((f32::MAX, f32::MAX), |a, c| (a.0.min(c.0), a.1.min(c.1)));
            let (x1, y1) = corners.iter().fold((f32::MIN, f32::MIN), |a, c| (a.0.max(c.0), a.1.max(c.1)));
            if x1 - x0 <= 0.0 || y1 - y0 <= 0.0 {
                continue;
            }
            let (sx, sy) = ((r[2] - r[0]) / (x1 - x0), (r[3] - r[1]) / (y1 - y0));
            let name = format!("PddfA{}", items.len());
            ops += &format!("q {sx:.5} 0 0 {sy:.5} {:.3} {:.3} cm /{name} Do Q\n", r[0] - x0 * sx, r[1] - y0 * sy);
            if let Ok(Object::Stream(stream)) = v.get_object_mut(id) {
                stream.dict.set("Subtype", "Form"); // alcuni file lo omettono
            }
            items.push((name, id));
        }
        if !items.is_empty() {
            draw_on_page(&mut v, page, "XObject", items, &ops)?;
        }
    }
    let mut out = vec![];
    v.save_to(&mut out)?;
    Pdf::new(out).map_err(|e| format!("PDF non visualizzabile: {e:?}").into())
}

pub enum FieldKind {
    Text { multiline: bool },
    /// Casella di spunta o pulsante di opzione; `on` è il nome dello stato attivo.
    Button { on: String, radio: bool },
    /// Elenco di scelte: (valore salvato, testo mostrato).
    Choice { options: Vec<(String, String)> },
}

/// Campo compilabile di un modulo, come appare su una pagina.
pub struct Field {
    pub id: ObjectId,
    /// Riquadro in punti dall'angolo in alto a sinistra della pagina a video: [x0, y0, x1, y1].
    pub rect: [f32; 4],
    pub kind: FieldKind,
    /// Testo, valore scelto, oppure stato del pulsante ("Off" o il nome dello stato attivo).
    pub value: String,
}

// ponytail: a ogni modifica il PDF viene riscritto in memoria e riletto dal renderer, e
// l'undo conserva le ultime 20 copie. Semplice e sempre coerente con ciò che verrà salvato,
// ma O(dimensione file) per operazione: con file molto grandi servirà un renderer che
// legga direttamente il documento modificato e un undo a operazioni inverse.
pub struct Doc {
    doc: Document,
    root: ObjectId,
    bytes: Vec<u8>,
    view: Pdf,
    undo: Vec<Vec<u8>>,
    pub dirty: bool,
}

impl Doc {
    pub fn open(path: &Path) -> Res<Self> {
        Self::from_bytes(&std::fs::read(path)?)
    }

    pub fn from_bytes(original: &[u8]) -> Res<Self> {
        let (mut doc, root) = parse(original)?;
        let bytes = serialize(&mut doc)?;
        let view = load_view(&doc, root)?;
        Ok(Self { doc, root, bytes, view, undo: vec![], dirty: false })
    }

    pub fn len(&self) -> usize {
        self.view.pages().len()
    }

    pub fn save(&mut self, path: &Path) -> Res<()> {
        write(path, &self.bytes)?;
        self.dirty = false;
        Ok(())
    }

    /// Applica una modifica in modo atomico: se fallisce, il documento torna com'era.
    fn edit(&mut self, f: impl FnOnce(&mut Self) -> Res<()>) -> Res<()> {
        let done = f(self).and_then(|()| {
            let bytes = serialize(&mut self.doc)?;
            let view = load_view(&self.doc, self.root)?;
            Ok((bytes, view))
        });
        match done {
            Ok((bytes, view)) => {
                if self.undo.len() == 20 {
                    self.undo.remove(0);
                }
                self.undo.push(std::mem::replace(&mut self.bytes, bytes));
                self.view = view;
                self.dirty = true;
                Ok(())
            }
            Err(e) => {
                (self.doc, self.root) = parse(&self.bytes)?;
                Err(e)
            }
        }
    }

    pub fn undo(&mut self) -> Res<bool> {
        let Some(bytes) = self.undo.pop() else { return Ok(false) };
        (self.doc, self.root) = parse(&bytes)?;
        self.view = load_view(&self.doc, self.root)?;
        self.bytes = bytes;
        Ok(true)
    }

    fn kids(&self) -> Res<Vec<ObjectId>> {
        kids_of(&self.doc, self.root)
    }

    fn page_id(&self, i: usize) -> Res<ObjectId> {
        self.kids()?.get(i).copied().ok_or_else(|| "pagina inesistente".into())
    }

    /// Rotazione della pagina in gradi orari: 0, 90, 180 o 270.
    fn rotation(&self, id: ObjectId) -> i64 {
        let rotate = self.doc.get_dictionary(id).and_then(|d| d.get_deref(b"Rotate", &self.doc));
        rotate.and_then(Object::as_i64).unwrap_or(0).rem_euclid(360)
    }

    /// Ruota le pagine di `quarters` quarti di giro in senso orario (negativo = antiorario).
    pub fn rotate(&mut self, pages: &[usize], quarters: i32) -> Res<()> {
        self.edit(|s| {
            for &i in pages {
                let id = s.page_id(i)?;
                let deg = (s.rotation(id) + 90 * quarters as i64).rem_euclid(360);
                s.doc.get_dictionary_mut(id)?.set("Rotate", deg);
            }
            Ok(())
        })
    }

    pub fn delete(&mut self, pages: &[usize]) -> Res<()> {
        if (0..self.len()).all(|i| pages.contains(&i)) {
            return Err("un PDF deve avere almeno una pagina".into());
        }
        self.edit(|s| drop_pages(&mut s.doc, s.root, pages))
    }

    /// Un nuovo PDF con le sole pagine indicate; il documento aperto non cambia.
    pub fn extract(&self, pages: &[usize]) -> Res<Vec<u8>> {
        let mut doc = self.doc.clone();
        let others: Vec<usize> = (0..self.len()).filter(|i| !pages.contains(i)).collect();
        drop_pages(&mut doc, self.root, &others)?;
        serialize(&mut doc)
    }

    /// Sostituisce le pagine indicate con tutte quelle di un altro PDF, messe al posto della prima.
    pub fn replace(&mut self, pages: &[usize], path: &Path) -> Res<()> {
        let (other, _) = parse(&std::fs::read(path)?)?;
        let at = pages.iter().copied().min().unwrap_or(0);
        self.edit(|s| {
            drop_pages(&mut s.doc, s.root, pages)?;
            s.add_pages(other, at)
        })
    }

    /// Sposta la pagina `from` in modo che finisca all'indice `to`.
    pub fn move_page(&mut self, from: usize, to: usize) -> Res<()> {
        if from == to || from >= self.len() || to >= self.len() {
            return Ok(());
        }
        self.edit(|s| {
            let mut kids = s.kids()?;
            let id = kids.remove(from);
            kids.insert(to, id);
            set_kids(&mut s.doc, s.root, kids)
        })
    }

    /// Inserisce tutte le pagine di un altro PDF a partire dall'indice `at`.
    pub fn insert_pdf(&mut self, path: &Path, at: usize) -> Res<()> {
        let (other, _) = parse(&std::fs::read(path)?)?;
        self.edit(|s| s.add_pages(other, at))
    }

    fn add_pages(&mut self, mut other: Document, at: usize) -> Res<()> {
        other.renumber_objects_with(self.doc.max_id + 1);
        let new_pages: Vec<ObjectId> = other.get_pages().into_values().collect();
        self.doc.max_id = other.max_id;
        // Il catalogo dell'altro documento resta orfano e viene scartato al salvataggio.
        self.doc.objects.extend(other.objects);
        for id in &new_pages {
            self.doc.get_dictionary_mut(*id)?.set("Parent", self.root);
        }
        let mut kids = self.kids()?;
        let at = at.min(kids.len());
        kids.splice(at..at, new_pages);
        set_kids(&mut self.doc, self.root, kids)
    }

    /// Dimensione della pagina in punti, come appare a video (rotazione inclusa).
    pub fn page_size(&self, i: usize) -> Res<(f32, f32)> {
        Ok(self.view.pages().get(i).ok_or("pagina inesistente")?.render_dimensions())
    }

    /// Renderizza la pagina alla larghezza data: (larghezza, altezza, RGBA).
    pub fn render(&self, i: usize, width_px: i32) -> Res<(usize, usize, Vec<u8>)> {
        let page = self.view.pages().get(i).ok_or("pagina inesistente")?;
        let scale = width_px.max(1) as f32 / page.render_dimensions().0;
        let settings = PixmapSettings { x_scale: scale, y_scale: scale, bg_color: WHITE };
        let pm = hayro::render(page, &RenderCache::new(), &InterpreterSettings::default(), &RenderSettings::default(), &settings);
        Ok((pm.width() as usize, pm.height() as usize, pm.data_as_u8_slice().to_vec()))
    }

    /// Imprime `c` sulla pagina `i`, largo `width` punti e centrato in (u, v):
    /// punti dall'angolo in alto a sinistra della pagina come appare a video.
    pub fn place(&mut self, i: usize, u: f32, v: f32, width: f32, c: &Content) -> Res<()> {
        let b = self.view.pages().get(i).ok_or("pagina inesistente")?.intersected_crop_box();
        let (x0, y0, x1, y1) = (b.x0 as f32, b.y0 as f32, b.x1 as f32, b.y1 as f32);
        self.edit(|s| {
            let id = s.page_id(i)?;
            // Da coordinate a video a spazio utente non ruotato; il contenuto va
            // contro-ruotato perché appaia dritto.
            let rot = s.rotation(id);
            let (cx, cy) = match rot {
                90 => (x0 + v, y0 + u),
                180 => (x1 - u, y0 + v),
                270 => (x1 - v, y1 - u),
                _ => (x0 + u, y1 - v),
            };
            let (w, h) = c.size();
            let scale = width / w;
            let (sin, cos) = (rot as f32).to_radians().sin_cos();
            let (ma, mb, mc, md) = (scale * cos, scale * sin, -scale * sin, scale * cos);
            let (me, mf) = (cx - (ma * w + mc * h) / 2.0, cy - (mb * w + md * h) / 2.0);

            let name = format!("Pddf{}", s.doc.max_id + 1);
            let (kind, resource, body) = match c {
                Content::Image(img) => {
                    let rgba = img.to_rgba8();
                    let base = dictionary! {
                        "Type" => "XObject", "Subtype" => "Image", "BitsPerComponent" => 8,
                        "Width" => rgba.width() as i64, "Height" => rgba.height() as i64,
                    };
                    let stream = |doc: &mut Document, mut dict: Dictionary, space: &str, data: Vec<u8>| {
                        dict.set("ColorSpace", space);
                        let mut stream = Stream::new(dict, data);
                        let _ = stream.compress(); // se fallisce resta non compresso, comunque valido
                        doc.add_object(stream)
                    };
                    let mask = stream(&mut s.doc, base.clone(), "DeviceGray", rgba.pixels().map(|p| p[3]).collect());
                    let mut dict = base;
                    dict.set("SMask", mask);
                    let rgb = rgba.pixels().flat_map(|p| [p[0], p[1], p[2]]).collect();
                    let image = stream(&mut s.doc, dict, "DeviceRGB", rgb);
                    ("XObject", image, format!("{w:.3} 0 0 {h:.3} 0 0 cm /{name} Do"))
                }
                Content::Text { lines, color } => {
                    let font = s.doc.add_object(dictionary! {
                        "Type" => "Font", "Subtype" => "Type1",
                        "BaseFont" => "Helvetica-Bold", "Encoding" => "WinAnsiEncoding",
                    });
                    let [r, g, b] = color.map(|c| c as f32 / 255.0);
                    let mut body = format!(
                        "{r:.3} {g:.3} {b:.3} RG {r:.3} {g:.3} {b:.3} rg 2 w 1.5 1.5 {:.3} {:.3} re S BT /{name} {SIZE} Tf",
                        w - 3.0,
                        h - 3.0
                    );
                    for (k, line) in lines.iter().enumerate() {
                        // +4.5: la linea di base sta sopra il fondo della riga (spazio per i discendenti)
                        let (x, y) = ((w - text_width(line, &BOLD, SIZE)) / 2.0, h - PAD - (k as f32 + 1.0) * LEAD + 4.5);
                        body += &format!(" 1 0 0 1 {x:.3} {y:.3} Tm {} Tj", hex_string(line));
                    }
                    ("Font", font, body + " ET")
                }
            };

            let ops = format!("q {ma:.5} {mb:.5} {mc:.5} {md:.5} {me:.3} {mf:.3} cm\n{body}\nQ");
            draw_on_page(&mut s.doc, id, kind, vec![(name, resource)], &ops)
        })
    }

    /// Cerca una voce nel campo o, se manca, nei campi da cui eredita.
    fn inherited(&self, mut id: ObjectId, key: &[u8]) -> Option<&Object> {
        for _ in 0..32 {
            let d = self.doc.get_dictionary(id).ok()?;
            if let Ok(v) = d.get_deref(key, &self.doc) {
                return Some(v);
            }
            id = d.get(b"Parent").and_then(Object::as_reference).ok()?;
        }
        None
    }

    /// Nome dello stato "attivo" di una casella o di un pulsante di opzione.
    fn on_state(&self, widget: ObjectId) -> Option<String> {
        let ap = self.doc.get_dictionary(widget).ok()?.get_deref(b"AP", &self.doc).ok()?.as_dict().ok()?;
        let states = ap.get_deref(b"N", &self.doc).ok()?.as_dict().ok()?;
        states.iter().map(|(k, _)| String::from_utf8_lossy(k).into_owned()).find(|k| k != "Off")
    }

    fn options(&self, id: ObjectId) -> Vec<(String, String)> {
        let text = |o: &Object| self.doc.dereference(o).ok().and_then(|(_, o)| decode_text_string(o).ok());
        let Some(Ok(opt)) = self.inherited(id, b"Opt").map(Object::as_array) else { return vec![] };
        let pair = |o: &Object| match self.doc.dereference(o).ok()?.1 {
            Object::Array(a) => Some((text(a.first()?)?, text(a.get(1)?)?)),
            _ => text(o).map(|t| (t.clone(), t)),
        };
        opt.iter().filter_map(pair).collect()
    }

    /// Campi compilabili della pagina `i` (esclusi quelli in sola lettura, nascosti o di firma).
    pub fn fields(&self, i: usize) -> Vec<Field> {
        let (Ok(page), Some(view)) = (self.page_id(i), self.view.pages().get(i)) else { return vec![] };
        let b = view.intersected_crop_box();
        let (x0, y0, x1, y1) = (b.x0 as f32, b.y0 as f32, b.x1 as f32, b.y1 as f32);
        let rot = self.rotation(page);
        // Dallo spazio utente della pagina alle coordinate a video (l'inverso di `place`).
        let to_screen = |x: f32, y: f32| match rot {
            90 => (y - y0, x - x0),
            180 => (x1 - x, y - y0),
            270 => (y1 - y, x1 - x),
            _ => (x - x0, y1 - y),
        };
        let annots = match self.doc.get_dictionary(page).and_then(|d| d.get_deref(b"Annots", &self.doc)) {
            Ok(Object::Array(a)) => a,
            _ => return vec![],
        };
        let int = |o: Option<&Object>| o.and_then(|o| o.as_i64().ok()).unwrap_or(0);
        let text = |o: Option<&Object>| o.and_then(|o| decode_text_string(o).ok()).unwrap_or_default();
        let field = |annot: &Object| {
            let id = annot.as_reference().ok()?;
            let d = self.doc.get_dictionary(id).ok()?;
            let flags = int(self.inherited(id, b"Ff"));
            if d.get(b"Subtype").and_then(Object::as_name).ok()? != b"Widget" || flags & 1 != 0 || int(d.get(b"F").ok()) & 2 != 0 {
                return None;
            }
            let r = rect(&self.doc, d.get(b"Rect").ok()?)?;
            let ((ua, va), (ub, vb)) = (to_screen(r[0], r[1]), to_screen(r[2], r[3]));
            let (kind, value) = match self.inherited(id, b"FT")?.as_name().ok()? {
                // 0x2000: password, non gestita
                b"Tx" if flags & 0x2000 == 0 => (FieldKind::Text { multiline: flags & 0x1000 != 0 }, text(self.inherited(id, b"V"))),
                // 0x10000: pulsante d'azione, non è un dato
                b"Btn" if flags & 0x10000 == 0 => {
                    let state = d.get(b"AS").and_then(Object::as_name).map_or("Off".into(), |n| String::from_utf8_lossy(n).into_owned());
                    (FieldKind::Button { on: self.on_state(id)?, radio: flags & 0x8000 != 0 }, state)
                }
                b"Ch" => (FieldKind::Choice { options: self.options(id) }, text(self.inherited(id, b"V"))),
                _ => return None,
            };
            Some(Field { id, rect: [ua.min(ub), va.min(vb), ua.max(ub), va.max(vb)], kind, value })
        };
        annots.iter().filter_map(field).collect()
    }

    /// Disegna il valore di un campo di testo o di scelta: è ciò che si vede nel riquadro.
    // ponytail: sempre Helvetica in WinAnsi (alfabeto latino occidentale), senza ruotare il
    // testo sulle pagine ruotate né spaziare i campi "a pettine". Per altri alfabeti o per
    // rispettare il font del modulo serve incorporare un font vero.
    fn text_appearance(&self, widget: ObjectId, text: &str) -> Res<Stream> {
        let d = self.doc.get_dictionary(widget)?;
        let r = d.get(b"Rect").ok().and_then(|r| rect(&self.doc, r)).ok_or("campo senza riquadro")?;
        let (w, h) = (r[2] - r[0], r[3] - r[1]);
        let int = |key: &[u8]| self.inherited(widget, key).and_then(|o| o.as_i64().ok()).unwrap_or(0);
        let multiline = int(b"Ff") & 0x1000 != 0;

        // Aspetto predefinito del modulo, es. "/Helv 10 Tf 0 g": se ne tengono corpo e colore.
        let form = self.doc.catalog().ok().and_then(|c| c.get_deref(b"AcroForm", &self.doc).ok()?.as_dict().ok());
        let da = self.inherited(widget, b"DA").or_else(|| form?.get_deref(b"DA", &self.doc).ok());
        let da = String::from_utf8_lossy(da.and_then(|o| o.as_str().ok()).unwrap_or_default()).into_owned();
        let tokens: Vec<&str> = da.split_whitespace().collect();
        let tf = tokens.iter().position(|t| *t == "Tf").unwrap_or(usize::MAX);
        let is_color = |(i, t): &(usize, &&str)| (*i > tf || *i + 2 < tf) && (t.parse::<f32>().is_ok() || ["g", "rg", "k"].contains(*t));
        let color: Vec<&str> = tokens.iter().enumerate().filter(is_color).map(|(_, t)| *t).collect();
        let color = if color.is_empty() { "0 g".to_string() } else { color.join(" ") };
        let mut size = tf.checked_sub(1).and_then(|i| tokens.get(i)?.parse::<f32>().ok()).unwrap_or(0.0);
        if size <= 0.0 {
            size = if multiline { 11.0 } else { (h - 4.0).clamp(4.0, 12.0) }; // 0 = automatico
        }

        let mut ops = String::new();
        // Sfondo e bordo dichiarati dal campo.
        let mk = d.get_deref(b"MK", &self.doc).and_then(Object::as_dict).ok();
        let paint = |key: &[u8], gray: &str, rgb: &str| {
            let c = nums(&self.doc, mk?.get(key).ok()?)?;
            match c[..] {
                [g] => Some(format!("{g:.3} {gray}")),
                [r, g, b] => Some(format!("{r:.3} {g:.3} {b:.3} {rgb}")),
                _ => None,
            }
        };
        if let Some(c) = paint(b"BG", "g", "rg") {
            ops += &format!("{c} 0 0 {w:.3} {h:.3} re f ");
        }
        if let Some(c) = paint(b"BC", "G", "RG") {
            ops += &format!("{c} 1 w 0.5 0.5 {:.3} {:.3} re S ", w - 1.0, h - 1.0);
        }

        let lines = if multiline { wrap(text, w - 4.0, size) } else { vec![text.to_string()] };
        let first = if multiline { h - 2.0 - size } else { (h - size) / 2.0 + size * 0.22 };
        ops += &format!("/Tx BMC q 1 1 {:.3} {:.3} re W n BT /Helv {size:.2} Tf {color}", w - 2.0, h - 2.0);
        for (k, line) in lines.iter().enumerate() {
            let free = w - 4.0 - text_width(line, &REGULAR, size);
            let x = 2.0 + [0.0, free / 2.0, free][int(b"Q").clamp(0, 2) as usize]; // sinistra, centro, destra
            ops += &format!(" 1 0 0 1 {x:.3} {:.3} Tm {} Tj", first - k as f32 * size * 1.15, hex_string(line));
        }
        ops += " ET Q EMC";
        let font = dictionary! { "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica", "Encoding" => "WinAnsiEncoding" };
        let dict = dictionary! {
            "Type" => "XObject", "Subtype" => "Form",
            "BBox" => vec![0.0.into(), 0.0.into(), w.into(), h.into()],
            "Resources" => dictionary! { "Font" => dictionary! { "Helv" => font } },
        };
        Ok(Stream::new(dict, ops.into_bytes()))
    }

    /// Imposta il valore di un campo a partire da uno dei suoi riquadri sulla pagina.
    /// Per caselle e opzioni `value` è il nome dello stato ("Off" per togliere la spunta).
    pub fn set_field(&mut self, widget: ObjectId, value: &str) -> Res<()> {
        self.edit(|s| {
            // Un campo può avere più riquadri (figli senza nome proprio) o coincidere col suo unico riquadro.
            let d = s.doc.get_dictionary(widget)?;
            let field = if d.has(b"T") { widget } else { d.get(b"Parent").and_then(Object::as_reference).unwrap_or(widget) };
            let kids = s.doc.get_dictionary(field)?.get_deref(b"Kids", &s.doc).and_then(Object::as_array).cloned().unwrap_or_default();
            let mut widgets: Vec<ObjectId> = kids.iter().filter_map(|k| k.as_reference().ok()).collect();
            widgets.retain(|k| s.doc.get_dictionary(*k).is_ok_and(|k| !k.has(b"T")));
            if widgets.is_empty() {
                widgets.push(field);
            }

            let kind = s.inherited(widget, b"FT").and_then(|o| o.as_name().ok()).unwrap_or_default().to_vec();
            if kind == b"Btn" {
                s.doc.get_dictionary_mut(field)?.set("V", Object::Name(value.into()));
                for w in widgets {
                    let state = if s.on_state(w).as_deref() == Some(value) { value } else { "Off" };
                    s.doc.get_dictionary_mut(w)?.set("AS", Object::Name(state.into()));
                }
            } else {
                let shown = s.options(widget).into_iter().find(|(saved, _)| saved == value).map_or(value.to_string(), |(_, shown)| shown);
                s.doc.get_dictionary_mut(field)?.set("V", text_string(value));
                for w in widgets {
                    let ap = s.text_appearance(w, &shown)?;
                    let ap = s.doc.add_object(ap);
                    s.doc.get_dictionary_mut(w)?.set("AP", dictionary! { "N" => ap });
                }
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(v: [i64; 4]) -> Vec<Object> {
        v.map(Object::Integer).to_vec()
    }

    /// Tre pagine larghe 100, 200, 300: la seconda con origine spostata, la terza in un
    /// nodo intermedio da cui eredita il MediaBox.
    fn sample() -> Vec<u8> {
        let mut d = Document::with_version("1.5");
        let (root, mid) = (d.new_object_id(), d.new_object_id());
        let p1 = d.add_object(dictionary! { "Type" => "Page", "Parent" => root, "MediaBox" => rect([0, 0, 100, 400]) });
        let p2 = d.add_object(dictionary! { "Type" => "Page", "Parent" => root, "MediaBox" => rect([50, 50, 250, 450]) });
        let p3 = d.add_object(dictionary! { "Type" => "Page", "Parent" => mid });
        let kids = |v: &[ObjectId]| v.iter().map(|id| Object::Reference(*id)).collect::<Vec<_>>();
        d.objects.insert(
            mid,
            dictionary! { "Type" => "Pages", "Parent" => root, "Kids" => kids(&[p3]), "Count" => 1, "MediaBox" => rect([0, 0, 300, 400]) }.into(),
        );
        d.objects.insert(root, dictionary! { "Type" => "Pages", "Kids" => kids(&[p1, p2, mid]), "Count" => 3 }.into());
        let catalog = d.add_object(dictionary! { "Type" => "Catalog", "Pages" => root });
        d.trailer.set("Root", catalog);
        let mut out = vec![];
        d.save_to(&mut out).unwrap();
        out
    }

    /// Larghezze diverse per riconoscere le pagine dopo gli spostamenti.
    fn widths(d: &Doc) -> Vec<i32> {
        (0..d.len()).map(|i| d.page_size(i).unwrap().0.round() as i32).collect()
    }

    /// True se nel riquadro (in frazioni della pagina) c'è almeno un pixel non bianco.
    fn inked(d: &Doc, i: usize, x0: f32, y0: f32, x1: f32, y1: f32) -> bool {
        let (w, h, px) = d.render(i, 400).unwrap();
        let (xa, xb) = ((x0 * w as f32) as usize, (x1 * w as f32) as usize);
        let (ya, yb) = ((y0 * h as f32) as usize, (y1 * h as f32) as usize);
        (ya..yb).any(|y| (xa..xb).any(|x| px[(y * w + x) * 4..][..3] != [255, 255, 255]))
    }

    #[test]
    fn pages_and_stamps() {
        let mut d = Doc::from_bytes(&sample()).unwrap();
        assert_eq!(widths(&d), [100, 200, 300]);

        d.move_page(0, 2).unwrap();
        assert_eq!(widths(&d), [200, 300, 100]);
        d.move_page(2, 0).unwrap();
        assert_eq!(widths(&d), [100, 200, 300]);
        d.move_page(1, 0).unwrap();
        assert_eq!(widths(&d), [200, 100, 300]);
        assert!(d.undo().unwrap());
        assert_eq!(widths(&d), [100, 200, 300]);

        d.rotate(&[2], 1).unwrap();
        assert_eq!(d.page_size(2).unwrap(), (400.0, 300.0));
        d.rotate(&[2], -1).unwrap();
        assert_eq!(d.page_size(2).unwrap(), (300.0, 400.0));

        d.delete(&[0]).unwrap();
        assert_eq!(widths(&d), [200, 300]);

        // Timbro di testo in alto a sinistra di una pagina ruotata: deve apparire lì, e solo lì.
        let stamp = Content::Text { lines: vec!["APPROVATO".into(), "13/10/2026 è €".into()], color: [200, 0, 0] };
        let (sw, sh) = stamp.size();
        assert!(sh < sw);
        for page in 0..2 {
            for quarters in 0..4 {
                d.rotate(&[page], 1).unwrap();
                let (pw, ph) = d.page_size(page).unwrap();
                assert!(!inked(&d, page, 0.0, 0.0, 1.0, 1.0), "pagina non vuota prima del timbro");
                d.place(page, pw * 0.25, ph * 0.25, pw * 0.3, &stamp).unwrap();
                assert!(inked(&d, page, 0.05, 0.05, 0.45, 0.45), "timbro assente: pagina {page}, rotazione {quarters}");
                assert!(
                    !inked(&d, page, 0.5, 0.0, 1.0, 1.0) && !inked(&d, page, 0.0, 0.5, 1.0, 1.0),
                    "timbro fuori posto: pagina {page}, rotazione {quarters}"
                );
                assert!(d.undo().unwrap());
            }
        }

        // Firma (immagine) sulla pagina con origine spostata, poi salvataggio e riapertura.
        let sig = Content::Image(DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(40, 20, image::Rgba([0, 0, 120, 255]))));
        d.place(0, 100.0, 100.0, 80.0, &sig).unwrap();
        d.place(0, 100.0, 300.0, 80.0, &stamp).unwrap(); // un secondo oggetto sulla stessa pagina
        let out = std::env::temp_dir().join("pddf-test.pdf");
        d.save(&out).unwrap();
        let mut again = Doc::open(&out).unwrap();
        assert_eq!(widths(&again), [200, 300]);
        assert!(inked(&again, 0, 0.32, 0.21, 0.68, 0.29) && inked(&again, 0, 0.3, 0.7, 0.7, 0.8));
        assert!(!inked(&again, 0, 0.0, 0.0, 0.28, 0.5) && !inked(&again, 0, 0.72, 0.0, 1.0, 0.5), "firma fuori posto");

        again.insert_pdf(&out, 1).unwrap();
        assert_eq!(widths(&again), [200, 200, 300, 300]);
        assert!(inked(&again, 1, 0.32, 0.21, 0.68, 0.29) && !inked(&again, 3, 0.0, 0.0, 1.0, 1.0));
        std::fs::remove_file(out).ok();
    }

    #[test]
    fn extract_and_replace() {
        let mut d = Doc::from_bytes(&sample()).unwrap();
        let out = std::env::temp_dir().join("pddf-test-estratte.pdf");
        write(&out, &d.extract(&[0, 2]).unwrap()).unwrap();
        assert_eq!(widths(&Doc::open(&out).unwrap()), [100, 300]);
        assert_eq!(widths(&d), [100, 200, 300]);

        d.replace(&[1, 2], &out).unwrap();
        assert_eq!(widths(&d), [100, 100, 300]);
        d.rotate(&[0, 2], 1).unwrap();
        assert_eq!(widths(&d), [400, 100, 400]);
        assert!(d.delete(&[0, 1, 2]).is_err());
        d.delete(&[0, 2]).unwrap();
        assert_eq!(widths(&d), [100]);
        std::fs::remove_file(out).ok();
    }

    /// Una pagina 400x800 con un campo di testo, una casella di spunta e un elenco a discesa.
    fn form() -> Vec<u8> {
        let mut d = Document::with_version("1.5");
        let (pages, page) = (d.new_object_id(), d.new_object_id());
        let widget = |ft: &str, name: &str, r: [i64; 4]| dictionary! { "Type" => "Annot", "Subtype" => "Widget", "FT" => ft, "T" => Object::string_literal(name), "Rect" => rect(r), "P" => page };
        let text = d.add_object(widget("Tx", "nome", [50, 700, 250, 720]));
        let state = |d: &mut Document, ops: &str| d.add_object(Stream::new(dictionary! { "Type" => "XObject", "Subtype" => "Form", "BBox" => rect([0, 0, 20, 20]) }, ops.into()));
        let (on, off) = (state(&mut d, "0 g 2 2 16 16 re f"), state(&mut d, ""));
        let mut check = widget("Btn", "ok", [50, 600, 70, 620]);
        check.set("AS", "Off");
        check.set("AP", dictionary! { "N" => dictionary! { "Yes" => on, "Off" => off } });
        let check = d.add_object(check);
        let mut choice = widget("Ch", "scelta", [50, 500, 250, 520]);
        choice.set("Ff", 0x20000);
        choice.set("Opt", vec![Object::string_literal("a"), vec![Object::string_literal("b"), Object::string_literal("Bravo")].into()]);
        let choice = d.add_object(choice);
        let fields: Vec<Object> = [text, check, choice].map(Object::Reference).to_vec();
        d.objects.insert(page, dictionary! { "Type" => "Page", "Parent" => pages, "MediaBox" => rect([0, 0, 400, 800]), "Annots" => fields.clone() }.into());
        d.objects.insert(pages, dictionary! { "Type" => "Pages", "Kids" => vec![Object::Reference(page)], "Count" => 1 }.into());
        let catalog = d.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages, "AcroForm" => dictionary! { "Fields" => fields } });
        d.trailer.set("Root", catalog);
        let mut out = vec![];
        d.save_to(&mut out).unwrap();
        out
    }

    #[test]
    fn form_fields() {
        // Riquadri dei tre campi, in frazioni della pagina a video.
        const TEXT: [f32; 4] = [0.125, 0.1, 0.625, 0.125];
        const CHECK: [f32; 4] = [0.125, 0.225, 0.175, 0.25];
        const CHOICE: [f32; 4] = [0.125, 0.35, 0.625, 0.375];
        let ink = |d: &Doc, r: [f32; 4]| inked(d, 0, r[0], r[1], r[2], r[3]);
        let values = |d: &Doc| d.fields(0).into_iter().map(|f| f.value).collect::<Vec<_>>();

        let mut d = Doc::from_bytes(&form()).unwrap();
        let f = d.fields(0);
        let (text, check, choice) = (f[0].id, f[1].id, f[2].id);
        assert_eq!(f.len(), 3);
        assert_eq!(f[0].rect, [50.0, 80.0, 250.0, 100.0]);
        assert!(matches!(f[0].kind, FieldKind::Text { multiline: false }));
        assert!(matches!(&f[1].kind, FieldKind::Button { on, radio: false } if on == "Yes"));
        assert!(matches!(&f[2].kind, FieldKind::Choice { options } if options[1] == ("b".to_string(), "Bravo".to_string())));
        assert_eq!(values(&d), ["", "Off", ""]);
        assert!(!inked(&d, 0, 0.0, 0.0, 1.0, 1.0));

        d.set_field(text, "Mario Rossi è qui").unwrap();
        assert!(ink(&d, TEXT) && !ink(&d, CHECK) && !ink(&d, CHOICE));
        d.set_field(check, "Yes").unwrap();
        assert!(ink(&d, CHECK));
        d.set_field(check, "Off").unwrap();
        assert!(!ink(&d, CHECK));
        d.set_field(check, "Yes").unwrap();
        d.set_field(choice, "b").unwrap();
        assert!(ink(&d, CHOICE));
        assert!(d.undo().unwrap());
        assert!(!ink(&d, CHOICE));
        d.set_field(choice, "b").unwrap();
        assert_eq!(values(&d), ["Mario Rossi è qui", "Yes", "b"]);

        // I valori devono sopravvivere a salvataggio e riapertura, e fuori dai campi non c'è nulla.
        let out = std::env::temp_dir().join("pddf-test-modulo.pdf");
        d.save(&out).unwrap();
        let again = Doc::open(&out).unwrap();
        assert_eq!(values(&again), ["Mario Rossi è qui", "Yes", "b"]);
        assert!(ink(&again, TEXT) && ink(&again, CHECK) && ink(&again, CHOICE));
        assert!(!inked(&again, 0, 0.0, 0.4, 1.0, 1.0) && !inked(&again, 0, 0.7, 0.0, 1.0, 1.0));
        std::fs::remove_file(out).ok();
    }
}
