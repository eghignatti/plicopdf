//! Estrazione del tratto da una firma fotografata o scansionata: la carta diventa
//! trasparente e resta solo l'inchiostro, come in Acrobat.

use image::{DynamicImage, RgbaImage, imageops::FilterType};

/// Lato lungo massimo su cui si lavora: oltre, una firma non guadagna dettaglio, solo peso.
const MAX_SIDE: u32 = 1600;
/// Lato dei blocchi su cui si stima la luminosità della carta.
const BLOCK: usize = 32;

/// Immagine analizzata: quanto ogni pixel è più scuro della carta che ha attorno.
pub struct Scan {
    w: usize,
    h: usize,
    rgb: Vec<[u8; 3]>,
    /// 0 = carta, 1 = nero pieno. Già corretto per illuminazione irregolare e ombre morbide.
    dark: Vec<f32>,
    /// Soglia proposta.
    pub auto: f32,
}

impl Scan {
    pub fn new(img: &DynamicImage) -> Self {
        let img = if img.width().max(img.height()) > MAX_SIDE { img.resize(MAX_SIDE, MAX_SIDE, FilterType::Triangle) } else { img.clone() };
        let img = img.to_rgba8();
        let (w, h) = (img.width() as usize, img.height() as usize);
        // Le zone trasparenti valgono come carta bianca.
        let rgb: Vec<[u8; 3]> = img.pixels().map(|p| p.0).map(|[r, g, b, a]| [r, g, b].map(|c| 255 - ((255 - c as u32) * a as u32 / 255) as u8)).collect();
        let lum: Vec<f32> = rgb.iter().map(|[r, g, b]| 0.299 * *r as f32 + 0.587 * *g as f32 + 0.114 * *b as f32).collect();

        // Carta: il pixel più chiaro di ogni blocco, poi il massimo coi vicini (un blocco
        // coperto d'inchiostro prende la carta accanto), poi interpolazione bilineare.
        let (bw, bh) = (w.div_ceil(BLOCK), h.div_ceil(BLOCK));
        let mut blocks = vec![0.0f32; bw * bh];
        for y in 0..h {
            for x in 0..w {
                let b = &mut blocks[y / BLOCK * bw + x / BLOCK];
                *b = b.max(lum[y * w + x]);
            }
        }
        let grown: Vec<f32> = (0..bw * bh)
            .map(|i| {
                let (bx, by) = ((i % bw) as isize, (i / bw) as isize);
                let near = (-1..=1).flat_map(|dy| (-1..=1).map(move |dx| (bx + dx, by + dy)));
                near.filter(|&(x, y)| x >= 0 && y >= 0 && (x as usize) < bw && (y as usize) < bh).map(|(x, y)| blocks[y as usize * bw + x as usize]).fold(0.0, f32::max)
            })
            .collect();
        let paper = |x: usize, y: usize| {
            let fx = ((x as f32 + 0.5) / BLOCK as f32 - 0.5).clamp(0.0, (bw - 1) as f32);
            let fy = ((y as f32 + 0.5) / BLOCK as f32 - 0.5).clamp(0.0, (bh - 1) as f32);
            let (x0, y0) = (fx as usize, fy as usize);
            let (x1, y1) = ((x0 + 1).min(bw - 1), (y0 + 1).min(bh - 1));
            let (tx, ty) = (fx - x0 as f32, fy - y0 as f32);
            let at = |x, y| grown[y * bw + x];
            let top = at(x0, y0) + (at(x1, y0) - at(x0, y0)) * tx;
            let bottom = at(x0, y1) + (at(x1, y1) - at(x0, y1)) * tx;
            (top + (bottom - top) * ty).max(1.0)
        };
        let dark: Vec<f32> = (0..w * h).map(|i| (1.0 - lum[i] / paper(i % w, i / w)).clamp(0.0, 1.0)).collect();
        Self { auto: otsu(&dark).clamp(0.1, 0.6), w, h, rgb, dark }
    }

    /// Colore medio dell'inchiostro pieno alla soglia data.
    pub fn ink_color(&self, threshold: f32) -> [u8; 3] {
        let (mut sum, mut n) = ([0u64; 3], 0u64);
        for (c, d) in self.rgb.iter().zip(&self.dark) {
            if *d > threshold + 0.15 {
                (0..3).for_each(|k| sum[k] += c[k] as u64);
                n += 1;
            }
        }
        if n == 0 { [0, 0, 0] } else { sum.map(|s| (s / n) as u8) }
    }

    /// Tratto alla soglia data, tinto con `color` e ritagliato: None se non resta nulla.
    pub fn extract(&self, threshold: f32, color: [u8; 3]) -> Option<RgbaImage> {
        const SOFT: f32 = 0.06; // bordo sfumato attorno alla soglia, contro le scalettature
        let (w, h) = (self.w, self.h);
        let mut alpha: Vec<u8> = self.dark.iter().map(|d| (((d - threshold + SOFT) / (2.0 * SOFT)).clamp(0.0, 1.0) * 255.0) as u8).collect();

        // Puntini isolati (polvere, grana della carta, rumore del sensore) via.
        let min = (w * h / 20000).clamp(3, 40);
        let mut seen = vec![false; w * h];
        let mut stack = vec![];
        for start in 0..w * h {
            if alpha[start] == 0 || seen[start] {
                continue;
            }
            let mut blob = vec![];
            seen[start] = true;
            stack.push(start);
            while let Some(i) = stack.pop() {
                blob.push(i);
                let (x, y) = (i % w, i / w);
                for (ok, j) in [(x > 0, i.wrapping_sub(1)), (x + 1 < w, i + 1), (y > 0, i.wrapping_sub(w)), (y + 1 < h, i + w)] {
                    if ok && alpha[j] > 0 && !seen[j] {
                        seen[j] = true;
                        stack.push(j);
                    }
                }
            }
            if blob.len() < min {
                blob.into_iter().for_each(|i| alpha[i] = 0);
            }
        }

        let (mut x0, mut y0, mut x1, mut y1) = (w, h, 0, 0);
        for (i, _) in alpha.iter().enumerate().filter(|(_, a)| **a > 0) {
            (x0, y0, x1, y1) = (x0.min(i % w), y0.min(i / w), x1.max(i % w), y1.max(i / w));
        }
        if x0 > x1 {
            return None;
        }
        const M: usize = 4;
        let (x0, y0, x1, y1) = (x0.saturating_sub(M), y0.saturating_sub(M), (x1 + M).min(w - 1), (y1 + M).min(h - 1));
        let [r, g, b] = color;
        Some(RgbaImage::from_fn((x1 - x0 + 1) as u32, (y1 - y0 + 1) as u32, |x, y| image::Rgba([r, g, b, alpha[(y0 + y as usize) * w + x0 + x as usize]])))
    }
}

/// Soglia che separa al meglio le due popolazioni (carta e inchiostro) dell'istogramma
/// (metodo di Otsu), messa a metà tra le loro medie: con popolazioni ben distinte qualunque
/// taglio tra le due è "ottimo", e il primo sarebbe a ridosso della grana della carta.
fn otsu(values: &[f32]) -> f32 {
    let mut hist = [0u64; 256];
    values.iter().for_each(|v| hist[(v * 255.0) as usize] += 1);
    let total = values.len() as f64;
    let sum: f64 = hist.iter().enumerate().map(|(i, n)| i as f64 * *n as f64).sum();
    let (mut best, mut at, mut w0, mut sum0) = (0.0, 0.0, 0.0, 0.0);
    for (i, n) in hist.iter().enumerate() {
        w0 += *n as f64;
        sum0 += i as f64 * *n as f64;
        let w1 = total - w0;
        if w0 == 0.0 || w1 == 0.0 {
            continue;
        }
        let between = w0 * w1 * (sum0 / w0 - (sum - sum0) / w1).powi(2);
        if between > best {
            (best, at) = (between, (sum0 / w0 + (sum - sum0) / w1) / 2.0);
        }
    }
    at as f32 / 255.0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Foto simulata: carta che passa da bianco a grigio (luce irregolare), un tratto blu
    /// scuro, un granello di polvere. Deve restare il solo tratto, sul trasparente.
    #[test]
    fn extracts_stroke() {
        let img = RgbaImage::from_fn(400, 200, |x, y| {
            let paper = 250 - (x * 90 / 400) as u8; // 250 a sinistra, 160 a destra
            if (90..110).contains(&y) && (50..350).contains(&x) {
                image::Rgba([20, 30, 110, 255])
            } else if (20..22).contains(&x) && y == 20 {
                image::Rgba([60, 60, 60, 255])
            } else {
                image::Rgba([paper, paper, paper.saturating_sub(5), 255])
            }
        });
        let scan = Scan::new(&DynamicImage::ImageRgba8(img));
        let color = scan.ink_color(scan.auto);
        assert!(color[2] > color[0] + 50, "inchiostro blu: {color:?}");
        let out = scan.extract(scan.auto, color).unwrap();
        // Ritagliato sul tratto (300x20) più il margine: polvere e carta grigia esclusi.
        assert_eq!((out.width(), out.height()), (308, 28));
        assert_eq!(out.get_pixel(150, 14)[3], 255);
        assert_eq!(out.get_pixel(150, 1)[3], 0);
        assert!(scan.extract(1.0, color).is_none());
    }
}
