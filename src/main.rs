// mado-clipboard — pixel plugin for Mado sidebar

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};

// ── Palette (Slate) ───────────────────────────────────────────────────────────

const BG:        [u8; 4] = [15,  23,  42,  255]; // slate-900
const BG_ITEM:   [u8; 4] = [30,  41,  59,  255]; // slate-800
const BG_HOV:    [u8; 4] = [51,  65,  85,  255]; // slate-700
const BG_SEL:    [u8; 4] = [71,  85,  105, 255]; // slate-600
const TEXT:      [u8; 4] = [248, 250, 252, 255]; // slate-50
const DIM:       [u8; 4] = [148, 163, 184, 255]; // slate-400
const ACCENT:    [u8; 4] = [99,  102, 241, 255]; // indigo-500
const SEARCH_BG: [u8; 4] = [30,  41,  59,  255]; // slate-800

// ── Font ──────────────────────────────────────────────────────────────────────

fn load_font(candidates: &[&str]) -> Option<fontdue::Font> {
    for path in candidates {
        if let Ok(data) = std::fs::read(path) {
            if let Ok(font) = fontdue::Font::from_bytes(
                data.as_slice(), fontdue::FontSettings::default()) {
                return Some(font);
            }
        }
    }
    None
}

fn load_system_font() -> Option<fontdue::Font> {
    load_font(&[
        "/System/Library/Fonts/Helvetica.ttc",
        "/Library/Fonts/Arial.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        "/usr/share/fonts/TTF/DejaVuSans.ttf",
        "/usr/share/fonts/dejavu-sans-fonts/DejaVuSans.ttf",
        "C:\\Windows\\Fonts\\arial.ttf",
        "C:\\Windows\\Fonts\\segoeui.ttf",
    ])
}

// ── Glyph width cache ─────────────────────────────────────────────────────────
// fontdue re-rasterizes on every call; caching advance widths eliminates the
// dominant CPU cost (repeated full bitmap render just to get the advance width).

struct GlyphCache {
    font: fontdue::Font,
    widths: HashMap<(u32, u32), usize>, // (char as u32, size_bits) → advance px
}

impl GlyphCache {
    fn new(font: fontdue::Font) -> Self {
        GlyphCache { font, widths: HashMap::new() }
    }

    fn advance(&mut self, ch: char, size: f32) -> usize {
        let key = (ch as u32, size.to_bits());
        if let Some(&w) = self.widths.get(&key) {
            return w;
        }
        let (m, _) = self.font.rasterize(ch, size);
        let w = m.advance_width.round() as usize;
        self.widths.insert(key, w);
        w
    }

    fn measure(&mut self, text: &str, size: f32) -> usize {
        text.chars().map(|ch| self.advance(ch, size)).sum()
    }

    /// Render text into a pixel buffer, returning the x position after the last char.
    fn draw_text(&mut self, buf: &mut [u8], stride: usize, h: usize,
                 text: &str, size: f32, mut cx: usize, y: usize, color: [u8; 4]) -> usize {
        for ch in text.chars() {
            let (m, bmp) = self.font.rasterize(ch, size);
            // cache the width while we have the metrics
            self.widths.insert((ch as u32, size.to_bits()), m.advance_width.round() as usize);
            let gx = cx as isize + m.xmin as isize;
            let gy = y as isize - m.height as isize - m.ymin as isize;
            for (k, &cov) in bmp.iter().enumerate() {
                if cov == 0 { continue; }
                let px = gx + (k % m.width) as isize;
                let py = gy + (k / m.width) as isize;
                if px < 0 || py < 0 || px as usize >= stride || py as usize >= h { continue; }
                let i = (py as usize * stride + px as usize) * 4;
                let a = cov as f32 / 255.0;
                let ia = 1.0 - a;
                for c in 0..3 {
                    buf[i + c] = (buf[i + c] as f32 * ia + color[c] as f32 * a) as u8;
                }
                buf[i + 3] = 255;
            }
            cx += m.advance_width.round() as usize;
        }
        cx
    }
}

// ── Canvas ────────────────────────────────────────────────────────────────────

struct Canvas { pixels: Vec<u8>, w: usize, h: usize }

impl Canvas {
    fn new(w: usize, h: usize) -> Self {
        let mut pixels = vec![0u8; w * h * 4];
        for px in pixels.chunks_exact_mut(4) { px.copy_from_slice(&BG); }
        Canvas { pixels, w, h }
    }

    fn fill_rect(&mut self, x: usize, y: usize, w: usize, h: usize, color: [u8; 4]) {
        for row in y..(y + h).min(self.h) {
            for col in x..(x + w).min(self.w) {
                let i = (row * self.w + col) * 4;
                self.pixels[i..i + 4].copy_from_slice(&color);
            }
        }
    }

    fn write_frame(&self, out: &mut impl Write) -> std::io::Result<()> {
        out.write_all(b"MADO")?;
        out.write_all(&(self.w as u32).to_le_bytes())?;
        out.write_all(&(self.h as u32).to_le_bytes())?;
        out.write_all(&self.pixels)?;
        out.flush()
    }
}

// ── MACT action protocol ──────────────────────────────────────────────────────

fn send_action(out: &mut impl Write, action: &str) {
    let json = format!("{{\"action\":\"{action}\"}}");
    out.write_all(b"MACT").unwrap();
    out.write_all(&(json.len() as u32).to_le_bytes()).unwrap();
    out.write_all(json.as_bytes()).unwrap();
    out.flush().unwrap();
}

// ── Protocol events ───────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct Event {
    #[serde(rename = "type")]
    kind:   String,
    width:  Option<u32>,
    height: Option<u32>,
    x:      Option<f32>,
    y:      Option<f32>,
    text:   Option<String>,
    delta:  Option<f32>,
}

// ── Clipboard polling ─────────────────────────────────────────────────────────

fn read_clipboard() -> Option<String> {
    let out = std::process::Command::new("pbpaste").output().ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

fn write_clipboard(text: &str) {
    use std::process::{Command, Stdio};
    if let Ok(mut child) = Command::new("pbcopy").stdin(Stdio::piped()).spawn() {
        if let Some(stdin) = child.stdin.as_mut() {
            let _ = stdin.write_all(text.as_bytes());
        }
        let _ = child.wait();
    }
}

// ── History persistence ───────────────────────────────────────────────────────

#[derive(Serialize, Deserialize)]
struct HistoryFile { items: Vec<String> }

fn history_path() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    std::path::Path::new(&home)
        .join(".config/mado/clipboard-history.json")
}

fn load_history() -> Vec<String> {
    let path = history_path();
    let content = std::fs::read_to_string(path).unwrap_or_default();
    serde_json::from_str::<HistoryFile>(&content)
        .map(|f| f.items)
        .unwrap_or_default()
}

fn save_history(items: &[String]) {
    let path = history_path();
    if let Some(dir) = path.parent() { let _ = std::fs::create_dir_all(dir); }
    if let Ok(json) = serde_json::to_string(&HistoryFile { items: items.to_vec() }) {
        let _ = std::fs::write(path, json);
    }
}

// ── State ─────────────────────────────────────────────────────────────────────

struct State {
    history:    Vec<String>,
    filter:     String,
    scroll_off: usize,
    copied_idx: Option<usize>,
    last_clip:  Option<String>,
    focused:    bool,
    dirty:      bool, // set whenever visible state changes; cleared after render
}

impl State {
    fn new() -> Self {
        let history = load_history();
        let last_clip = history.first().cloned();
        State { history, filter: String::new(), scroll_off: 0,
                copied_idx: None, last_clip, focused: false, dirty: true }
    }

    fn push(&mut self, s: String) {
        self.history.retain(|h| h != &s);
        self.history.insert(0, s.clone());
        if self.history.len() > 100 { self.history.truncate(100); }
        self.last_clip = Some(s);
        save_history(&self.history);
        self.dirty = true;
    }

    fn filtered(&self) -> Vec<(usize, &str)> {
        let q = self.filter.to_lowercase();
        self.history.iter().enumerate()
            .filter(|(_, h)| q.is_empty() || h.to_lowercase().contains(&q))
            .map(|(i, h)| (i, h.as_str()))
            .collect()
    }
}

// ── Rendering ─────────────────────────────────────────────────────────────────

const SEARCH_H: usize = 36;
const ITEM_H:   usize = 44;
const PAD:      usize = 12;

fn render(state: &State, cache: &mut GlyphCache, w: usize, h: usize, out: &mut impl Write) -> bool {
    let mut canvas = Canvas::new(w, h);
    let text_size:  f32 = (w as f32 * 0.075).clamp(11.0, 15.0);
    let small_size: f32 = (w as f32 * 0.06).clamp(9.0, 12.0);

    // Search bar
    canvas.fill_rect(PAD, 8, w - PAD * 2, SEARCH_H, SEARCH_BG);
    let border_color = if state.focused { ACCENT } else { BG_SEL };
    canvas.fill_rect(PAD, 8, w - PAD * 2, 1, border_color);
    canvas.fill_rect(PAD, 8 + SEARCH_H - 1, w - PAD * 2, 1, border_color);
    canvas.fill_rect(PAD, 8, 1, SEARCH_H, border_color);
    canvas.fill_rect(PAD + w - PAD * 2 - 1, 8, 1, SEARCH_H, border_color);
    let placeholder = if state.filter.is_empty() { "Search..." } else { &state.filter };
    let color = if state.filter.is_empty() { DIM } else { TEXT };
    cache.draw_text(&mut canvas.pixels, canvas.w, canvas.h,
                    placeholder, text_size, PAD + 10, 8 + SEARCH_H - 10, color);

    // Items
    let list_top = 8 + SEARCH_H + 8;
    let visible_count = (h.saturating_sub(list_top)) / ITEM_H;
    let filtered = state.filtered();
    let items: Vec<(usize, &str)> = filtered.iter()
        .skip(state.scroll_off).take(visible_count + 1).copied().collect();

    for (slot, (orig_idx, text)) in items.iter().enumerate() {
        let iy = list_top + slot * ITEM_H;
        if iy + ITEM_H > h { break; }
        let is_copied = state.copied_idx == Some(*orig_idx);
        let bg = if is_copied { ACCENT } else { BG_ITEM };
        canvas.fill_rect(PAD, iy + 2, w - PAD * 2, ITEM_H - 4, bg);

        let max_w = w - PAD * 2 - 16;
        let mut display: String = text.replace('\n', " ↵ ").replace('\t', "  ");
        // Truncate to fit: estimate chars that fit, then trim precisely
        let approx_char_w = cache.advance('m', text_size).max(1);
        let approx_chars = max_w / approx_char_w;
        if display.len() > approx_chars + 4 {
            let n = (0..=(approx_chars + 4))
                .rev()
                .find(|&i| display.is_char_boundary(i))
                .unwrap_or(0);
            display.truncate(n);
        }
        // Fine-tune with ellipsis
        while cache.measure(&format!("{display}…"), text_size) > max_w && !display.is_empty() {
            display.pop();
        }
        if cache.measure(text, text_size) > max_w { display.push('…'); }

        cache.draw_text(&mut canvas.pixels, canvas.w, canvas.h,
                        &display, text_size, PAD + 8, iy + ITEM_H - 14, TEXT);

        let hint = format!("{} chars", text.len());
        let hint_w = cache.measure(&hint, small_size);
        let hint_x = w.saturating_sub(PAD + hint_w + 4);
        cache.draw_text(&mut canvas.pixels, canvas.w, canvas.h,
                        &hint, small_size, hint_x, iy + ITEM_H - 14, DIM);
    }

    if filtered.is_empty() {
        let msg = if state.history.is_empty() { "Nothing copied yet" } else { "No matches" };
        let mw = cache.measure(msg, text_size);
        cache.draw_text(&mut canvas.pixels, canvas.w, canvas.h,
                        msg, text_size, w.saturating_sub(mw) / 2, h / 2, DIM);
    }

    // Scrollbar
    let total = filtered.len();
    if total > visible_count && visible_count > 0 {
        let track_h = h - list_top;
        let thumb_h = (track_h * visible_count / total).max(20);
        let thumb_y = list_top + track_h * state.scroll_off / total;
        canvas.fill_rect(w - 4, thumb_y, 3, thumb_h, BG_SEL);
    }

    canvas.write_frame(out).is_ok()
}

// ── Main ──────────────────────────────────────────────────────────────────────

fn main() {
    let font = load_system_font().unwrap_or_else(|| {
        eprintln!("mado-clipboard: no system font found");
        std::process::exit(1);
    });

    let dims:  Arc<Mutex<(u32, u32)>> = Arc::new(Mutex::new((300, 400)));
    let state: Arc<Mutex<State>>      = Arc::new(Mutex::new(State::new()));

    // Clipboard polling thread — checks every 500ms
    {
        let state = Arc::clone(&state);
        std::thread::spawn(move || loop {
            if let Some(clip) = read_clipboard() {
                let mut st = state.lock().unwrap();
                if st.last_clip.as_deref() != Some(&clip) {
                    st.push(clip);
                }
            }
            std::thread::sleep(Duration::from_millis(500));
        });
    }

    let stdout_shared: Arc<Mutex<std::io::BufWriter<std::io::Stdout>>> =
        Arc::new(Mutex::new(std::io::BufWriter::new(std::io::stdout())));

    {
        let dims   = Arc::clone(&dims);
        let state  = Arc::clone(&state);
        let stdout = Arc::clone(&stdout_shared);
        std::thread::spawn(move || {
            let stdin = std::io::stdin();
            for line in BufReader::new(stdin.lock()).lines().flatten() {
                if let Ok(ev) = serde_json::from_str::<Event>(&line) {
                    match ev.kind.as_str() {
                        "resize" => {
                            if let (Some(w), Some(h)) = (ev.width, ev.height) {
                                if w > 0 && h > 0 {
                                    *dims.lock().unwrap() = (w, h);
                                    state.lock().unwrap().dirty = true;
                                }
                            }
                        }
                        "click" => {
                            if let (Some(_x), Some(y)) = (ev.x, ev.y) {
                                let list_top = (8 + SEARCH_H + 8) as f32;
                                if y >= list_top {
                                    let slot = ((y - list_top) / ITEM_H as f32) as usize;
                                    let mut st = state.lock().unwrap();
                                    let idx = st.scroll_off + slot;
                                    let item = st.filtered().get(idx)
                                        .map(|&(_, t)| t.to_string());
                                    if let Some(owned) = item {
                                        write_clipboard(&owned);
                                        let orig = st.history.iter().position(|h| h == &owned);
                                        st.copied_idx = orig;
                                        st.last_clip = Some(owned.clone());
                                        st.history.retain(|h| h != &owned);
                                        st.history.insert(0, owned);
                                        save_history(&st.history);
                                        st.dirty = true;
                                        drop(st);
                                        if let Ok(mut out) = stdout.lock() {
                                            send_action(&mut *out, "paste");
                                        }
                                    }
                                }
                            }
                        }
                        "key" => {
                            if let Some(text) = ev.text {
                                let mut st = state.lock().unwrap();
                                match text.as_str() {
                                    "\u{0008}" | "\u{007F}" => { st.filter.pop(); st.dirty = true; }
                                    "\u{001B}" => { st.filter.clear(); st.dirty = true; }
                                    t if t.len() == 1 && !t.starts_with('\u{00}') => {
                                        st.filter.push_str(t);
                                        st.scroll_off = 0;
                                        st.dirty = true;
                                    }
                                    _ => {}
                                }
                            }
                        }
                        "focus" => { let mut st = state.lock().unwrap(); st.focused = true;  st.dirty = true; }
                        "blur"  => { let mut st = state.lock().unwrap(); st.focused = false; st.dirty = true; }
                        "scroll" => {
                            if let Some(delta) = ev.delta {
                                let mut st = state.lock().unwrap();
                                let len = st.filtered().len();
                                if delta > 0.0 {
                                    if st.scroll_off + 1 < len { st.scroll_off += 1; st.dirty = true; }
                                } else if st.scroll_off > 0 {
                                    st.scroll_off -= 1; st.dirty = true;
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        });
    }

    let mut cache = GlyphCache::new(font);
    let mut tick: u32 = 0;

    loop {
        let (w, h) = *dims.lock().unwrap();
        let (w, h) = (w as usize, h as usize);

        let should_render = {
            let mut st = state.lock().unwrap();
            // Always tick the copied_idx flash (2 ticks ≈ 1s) even if not otherwise dirty
            if st.copied_idx.is_some() {
                tick += 1;
                if tick >= 2 { st.copied_idx = None; tick = 0; }
                st.dirty = true;
            }
            let d = st.dirty;
            st.dirty = false;
            d
        };

        if should_render {
            let st = state.lock().unwrap();
            if let Ok(mut out) = stdout_shared.lock() {
                if !render(&st, &mut cache, w, h, &mut *out) {
                    // Write failed (broken pipe) — exit cleanly
                    break;
                }
            }
        }

        std::thread::sleep(Duration::from_millis(500));
    }
}
