// mado-clipboard — pixel plugin for Mado sidebar

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

    fn blend(&mut self, x: usize, y: usize, color: [u8; 4], alpha: f32) {
        if x >= self.w || y >= self.h { return; }
        let i = (y * self.w + x) * 4;
        let ia = 1.0 - alpha;
        for c in 0..3 {
            self.pixels[i + c] =
                (self.pixels[i + c] as f32 * ia + color[c] as f32 * alpha).round() as u8;
        }
        self.pixels[i + 3] = 255;
    }

    fn text(&mut self, font: &fontdue::Font, text: &str,
            size: f32, x: usize, y: usize, color: [u8; 4]) -> usize {
        let mut cx = x;
        for ch in text.chars() {
            let (m, bmp) = font.rasterize(ch, size);
            let gx = cx as isize + m.xmin as isize;
            let gy = y as isize - m.height as isize - m.ymin as isize;
            for (k, &cov) in bmp.iter().enumerate() {
                if cov == 0 { continue; }
                let px = gx + (k % m.width) as isize;
                let py = gy + (k / m.width) as isize;
                if px >= 0 && py >= 0 {
                    self.blend(px as usize, py as usize, color, cov as f32 / 255.0);
                }
            }
            cx += m.advance_width.round() as usize;
        }
        cx
    }

    fn measure(font: &fontdue::Font, text: &str, size: f32) -> usize {
        text.chars().map(|ch| {
            let (m, _) = font.rasterize(ch, size);
            m.advance_width.round() as usize
        }).sum()
    }

    fn write_frame(&self, out: &mut impl Write) {
        out.write_all(b"MADO").unwrap();
        out.write_all(&(self.w as u32).to_le_bytes()).unwrap();
        out.write_all(&(self.h as u32).to_le_bytes()).unwrap();
        out.write_all(&self.pixels).unwrap();
        out.flush().unwrap();
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
}

impl State {
    fn new() -> Self {
        let history = load_history();
        let last_clip = history.first().cloned();
        State { history, filter: String::new(), scroll_off: 0,
                copied_idx: None, last_clip, focused: false }
    }

    fn push(&mut self, s: String) {
        self.history.retain(|h| h != &s);
        self.history.insert(0, s.clone());
        if self.history.len() > 100 { self.history.truncate(100); }
        self.last_clip = Some(s);
        save_history(&self.history);
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

fn render(state: &State, font: &fontdue::Font, w: usize, h: usize, out: &mut impl Write) {
    let mut canvas = Canvas::new(w, h);
    let text_size:  f32 = (w as f32 * 0.075).clamp(11.0, 15.0);
    let small_size: f32 = (w as f32 * 0.06).clamp(9.0, 12.0);

    // Search bar
    canvas.fill_rect(PAD, 8, w - PAD * 2, SEARCH_H, SEARCH_BG);
    // Border: accent when focused, dim outline when not
    let border_color = if state.focused { ACCENT } else { BG_SEL };
    canvas.fill_rect(PAD, 8, w - PAD * 2, 1, border_color);                   // top
    canvas.fill_rect(PAD, 8 + SEARCH_H - 1, w - PAD * 2, 1, border_color);   // bottom
    canvas.fill_rect(PAD, 8, 1, SEARCH_H, border_color);                      // left
    canvas.fill_rect(PAD + w - PAD * 2 - 1, 8, 1, SEARCH_H, border_color);   // right
    let placeholder = if state.filter.is_empty() { "Search..." } else { &state.filter };
    let color = if state.filter.is_empty() { DIM } else { TEXT };
    canvas.text(font, placeholder, text_size, PAD + 10, 8 + SEARCH_H - 10, color);

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
        let mut display = text.replace('\n', " ↵ ").replace('\t', "  ");
        if Canvas::measure(font, &display, text_size) > max_w {
            while Canvas::measure(font, &format!("{display}…"), text_size) > max_w
                && !display.is_empty() { display.pop(); }
            display.push('…');
        }
        canvas.text(font, &display, text_size, PAD + 8, iy + ITEM_H - 14, TEXT);

        let hint = format!("{} chars", text.len());
        let hint_x = w.saturating_sub(PAD + Canvas::measure(font, &hint, small_size) + 4);
        canvas.text(font, &hint, small_size, hint_x, iy + ITEM_H - 14, DIM);
    }

    if filtered.is_empty() {
        let msg = if state.history.is_empty() { "Nothing copied yet" } else { "No matches" };
        let mw = Canvas::measure(font, msg, text_size);
        canvas.text(font, msg, text_size, w.saturating_sub(mw) / 2, h / 2, DIM);
    }

    // Scrollbar
    let total = filtered.len();
    if total > visible_count && visible_count > 0 {
        let track_h = h - list_top;
        let thumb_h = (track_h * visible_count / total).max(20);
        let thumb_y = list_top + track_h * state.scroll_off / total;
        canvas.fill_rect(w - 4, thumb_y, 3, thumb_h, BG_SEL);
    }

    canvas.write_frame(out);
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

    // Stdin event listener
    // Note: stdout is shared with the render loop. We use a Mutex so the
    // event thread can write MACT actions without racing with MADO frames.
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
                                if w > 0 && h > 0 { *dims.lock().unwrap() = (w, h); }
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
                                        drop(st);
                                        // Tell Mado to paste into the focused terminal.
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
                                    "\u{0008}" | "\u{007F}" => { st.filter.pop(); }
                                    "\u{001B}" => { st.filter.clear(); }
                                    t if t.len() == 1 && !t.starts_with('\u{00}') => {
                                        st.filter.push_str(t);
                                        st.scroll_off = 0;
                                    }
                                    _ => {}
                                }
                            }
                        }
                        "focus" => { state.lock().unwrap().focused = true; }
                        "blur"  => { state.lock().unwrap().focused = false; }
                        "scroll" => {
                            if let Some(delta) = ev.delta {
                                let mut st = state.lock().unwrap();
                                let len = st.filtered().len();
                                if delta > 0.0 {
                                    if st.scroll_off + 1 < len { st.scroll_off += 1; }
                                } else if st.scroll_off > 0 {
                                    st.scroll_off -= 1;
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        });
    }

    let mut tick: u32 = 0;
    loop {
        let (w, h) = *dims.lock().unwrap();
        let (w, h) = (w as usize, h as usize);
        {
            let mut st = state.lock().unwrap();
            if st.copied_idx.is_some() {
                tick += 1;
                if tick >= 2 { st.copied_idx = None; tick = 0; }
            }
            if let Ok(mut out) = stdout_shared.lock() {
                render(&st, &font, w, h, &mut *out);
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}
