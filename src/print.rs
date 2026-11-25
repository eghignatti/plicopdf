//! Stampa col dialogo del sistema: GDI su Windows, portale di stampa (D-Bus) su Linux.
//! Restituisce il messaggio per la barra di stato; vuoto se l'utente annulla.

#[cfg(windows)]
pub fn print(bytes: Vec<u8>, title: &str) -> Result<String, String> {
    use windows_sys::Win32::Foundation::GlobalFree;
    use windows_sys::Win32::Graphics::Gdi::DeleteDC;
    use windows_sys::Win32::UI::Controls::Dialogs::*;

    let doc = crate::pdf::Doc::from_bytes(&bytes).map_err(|e| e.to_string())?;
    let n = doc.len() as u16;
    let mut pd = PRINTDLGW {
        lStructSize: size_of::<PRINTDLGW>() as u32,
        Flags: PD_RETURNDC | PD_NOSELECTION | PD_USEDEVMODECOPIESANDCOLLATE,
        nFromPage: 1,
        nToPage: n,
        nMinPage: 1,
        nMaxPage: n,
        nCopies: 1,
        ..Default::default()
    };
    // ponytail: dialogo senza finestra proprietaria, aperto dal thread di stampa.
    if unsafe { PrintDlgW(&mut pd) } == 0 || pd.hDC.is_null() {
        return Ok(String::new());
    }
    unsafe {
        GlobalFree(pd.hDevMode);
        GlobalFree(pd.hDevNames);
    }
    let (from, to) = if pd.Flags & PD_PAGENUMS != 0 { (pd.nFromPage.max(1), pd.nToPage.min(n)) } else { (1, n) };
    let done = unsafe { job(pd.hDC, &doc, from as usize - 1..to as usize, title) };
    unsafe { DeleteDC(pd.hDC) };
    done
}

#[cfg(windows)]
unsafe fn job(hdc: windows_sys::Win32::Graphics::Gdi::HDC, doc: &crate::pdf::Doc, pages: std::ops::Range<usize>, title: &str) -> Result<String, String> {
    use windows_sys::Win32::Graphics::Gdi::*;
    use windows_sys::Win32::Storage::Xps::*;

    let name: Vec<u16> = title.encode_utf16().chain([0]).collect();
    let info = DOCINFOW { cbSize: size_of::<DOCINFOW>() as i32, lpszDocName: name.as_ptr(), ..unsafe { std::mem::zeroed() } };
    if unsafe { StartDocW(hdc, &info) } <= 0 {
        return Err("la stampante non accetta il lavoro".into());
    }
    let caps = |i: u32| unsafe { GetDeviceCaps(hdc, i as i32) } as f32;
    let (dpi, area) = ((caps(LOGPIXELSX), caps(LOGPIXELSY)), (caps(HORZRES), caps(VERTRES)));
    unsafe { SetStretchBltMode(hdc, HALFTONE) };
    let count = pages.len();
    for i in pages {
        let Ok((mut w, mut h)) = doc.page_size(i) else { continue };
        // Pagina orizzontale su foglio verticale (o viceversa): si gira, come fanno i lettori PDF.
        let turn = (w > h) != (area.0 > area.1);
        if turn {
            (w, h) = (h, w);
        }
        // Dimensione reale, ridotta solo se non entra nell'area stampabile; centrata.
        let scale = (area.0 / (w / 72.0 * dpi.0)).min(area.1 / (h / 72.0 * dpi.1)).min(1.0);
        let (dw, dh) = (w / 72.0 * dpi.0 * scale, h / 72.0 * dpi.1 * scale);
        // Oltre 300 dpi non si vede differenza e la memoria cresce col quadrato.
        let px = ((if turn { h } else { w }) / 72.0 * dpi.0.min(300.0) * scale).round() as i32;
        let Ok((pw, ph, rgba)) = doc.render(i, px) else { continue };
        let (bw, bh, mut bits) = if turn { rotate(pw, ph, &rgba) } else { (pw, ph, rgba) };
        bits.chunks_exact_mut(4).for_each(|p| p.swap(0, 2)); // GDI vuole BGRA
        let mut bmi: BITMAPINFO = unsafe { std::mem::zeroed() };
        bmi.bmiHeader = BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: bw as i32,
            biHeight: -(bh as i32), // dall'alto in basso
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB,
            ..unsafe { std::mem::zeroed() }
        };
        unsafe {
            StartPage(hdc);
            let (x, y) = (((area.0 - dw) / 2.0) as i32, ((area.1 - dh) / 2.0) as i32);
            StretchDIBits(hdc, x, y, dw as i32, dh as i32, 0, 0, bw as i32, bh as i32, bits.as_ptr().cast(), &bmi, DIB_RGB_COLORS, SRCCOPY);
            EndPage(hdc);
        }
    }
    unsafe { EndDoc(hdc) };
    Ok(if count == 1 { "1 pagina inviata alla stampante".into() } else { format!("{count} pagine inviate alla stampante") })
}

/// Ruota di 90° in senso orario un'immagine RGBA larga `w` e alta `h`.
#[cfg(any(windows, test))]
fn rotate(w: usize, h: usize, px: &[u8]) -> (usize, usize, Vec<u8>) {
    let mut out = vec![0; px.len()];
    for y in 0..h {
        for x in 0..w {
            let (nx, ny) = (h - 1 - y, x);
            out[(ny * h + nx) * 4..][..4].copy_from_slice(&px[(y * w + x) * 4..][..4]);
        }
    }
    (h, w, out)
}

/// Il portale mostra il dialogo di stampa del desktop e stampa il PDF: funziona anche
/// dentro il Flatpak, senza permessi in più.
#[cfg(target_os = "linux")]
pub fn print(bytes: Vec<u8>, title: &str) -> Result<String, String> {
    use std::collections::HashMap;
    use zbus::zvariant::{Fd, OwnedValue, Value};
    let e = |e: &dyn std::fmt::Display| e.to_string();

    // Al portale si passa un descrittore aperto: il file può sparire subito.
    let path = std::env::temp_dir().join(format!("plicopdf-stampa-{}.pdf", std::process::id()));
    std::fs::write(&path, &bytes).map_err(|x| e(&x))?;
    let file = std::fs::File::open(&path).map_err(|x| e(&x));
    std::fs::remove_file(&path).ok();
    let file = file?;

    let conn = zbus::blocking::Connection::session().map_err(|x| e(&x))?;
    // La risposta arriva come segnale su un percorso che dipende dal token: ci si iscrive prima
    // della chiamata, così non la si perde.
    let token = format!("plicopdf{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis()));
    let sender = conn.unique_name().map(|n| n.trim_start_matches(':').replace('.', "_")).unwrap_or_default();
    let handle = format!("/org/freedesktop/portal/desktop/request/{sender}/{token}");
    let request = zbus::blocking::Proxy::new(&conn, "org.freedesktop.portal.Desktop", handle.as_str(), "org.freedesktop.portal.Request").map_err(|x| e(&x))?;
    let mut responses = request.receive_signal("Response").map_err(|x| e(&x))?;

    let options = HashMap::from([("handle_token", Value::from(token.as_str()))]);
    let args = ("", title, Fd::from(&file), options);
    conn.call_method(Some("org.freedesktop.portal.Desktop"), "/org/freedesktop/portal/desktop", Some("org.freedesktop.portal.Print"), "Print", &args)
        .map_err(|x| e(&x))?;
    let reply = responses.next().ok_or("nessuna risposta dal portale di stampa")?;
    let (code, _): (u32, HashMap<String, OwnedValue>) = reply.body().deserialize().map_err(|x| e(&x))?;
    Ok(if code == 0 { "Inviato alla stampante".into() } else { String::new() })
}

#[cfg(test)]
mod tests {
    #[test]
    fn rotate_clockwise() {
        // 2×1: rosso a sinistra, blu a destra -> 1×2: rosso in alto, blu in basso.
        let px = [255, 0, 0, 255, 0, 0, 255, 255];
        assert_eq!(super::rotate(2, 1, &px), (1, 2, px.to_vec()));
        // 2×2: l'angolo in alto a sinistra va in alto a destra.
        let px: Vec<u8> = (0..4).flat_map(|i| [i, 0, 0, 255]).collect();
        let (_, _, out) = super::rotate(2, 2, &px);
        assert_eq!(out.chunks(4).map(|p| p[0]).collect::<Vec<_>>(), [2, 0, 3, 1]);
    }
}
