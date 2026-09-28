//! Image cards in the chat: the collapsed line, the layout arithmetic, and the
//! renderer that paints an opened image.
//!
//! An image is COLLAPSED by default — one line naming it — and opened inline by
//! the same expand/collapse key that opens a tool card. Opening it reserves rows
//! in the chat layout (so scrolling, selection and the row cache keep working)
//! and paints the picture into them with `ratatui-image`: real pixels through
//! the kitty/iTerm2/sixel protocol where the terminal supports one, half-blocks
//! everywhere else.
//!
//! The terminal's graphics capability can only be discovered by ASKING it, which
//! writes to stdout and reads the reply from stdin — up to two seconds of retry
//! when the terminal does not answer. That query is therefore deferred until the
//! first image is actually opened ([`ImageRenderer::needs_query`]), with the
//! event loop paused so the harness owns stdin, instead of taxing every launch.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::widgets::StatefulWidget as _;
use ratatui_image::StatefulImage;
use ratatui_image::picker::{Picker, ProtocolType};
use ratatui_image::protocol::StatefulProtocol;
use serde::{Deserialize, Serialize};

/// Below this many chat columns a preview is worthless (a picture squeezed into
/// a few cells shows nothing), so only the collapsed line is shown.
pub(crate) const MIN_PREVIEW_COLS: u16 = 40;

/// The tallest an opened image may be, however tall its aspect ratio is.
pub(crate) const MAX_PREVIEW_ROWS: u16 = 24;

/// One image in the chat: what to call it, where its pixels are, and whether it
/// is currently opened.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ImageCard {
    pub name: String,
    /// The file the pixels are read from, when there is one. Persisted, so a
    /// saved session shows its images again.
    pub path: Option<PathBuf>,
    /// Pixels held in memory — a clipboard paste has no file behind it. NOT
    /// persisted: embedding megabytes of pixels would bloat a session file, so a
    /// restored card shows its line with no preview.
    #[serde(skip)]
    pub pixels: Option<Vec<u8>>,
    /// Pixel size, read from the header when the card is created. `None` when it
    /// could not be read (the card then shows the collapsed line only).
    pub dims: Option<(u32, u32)>,
    /// Encoded size in bytes, for the collapsed line.
    pub bytes: u64,
    /// Opened inline. Collapsed by default, like every other card body.
    pub open: bool,
}

impl ImageCard {
    /// A card backed by a file in the worktree, re-read whenever it is drawn.
    pub fn file(name: impl Into<String>, path: PathBuf) -> Self {
        Self {
            name: name.into(),
            path: Some(path),
            pixels: None,
            dims: None,
            bytes: 0,
            open: false,
        }
    }

    /// A card backed by bytes held in memory (a clipboard paste).
    pub fn pixels(name: impl Into<String>, pixels: Vec<u8>) -> Self {
        Self {
            name: name.into(),
            path: None,
            pixels: Some(pixels),
            dims: None,
            bytes: 0,
            open: false,
        }
    }

    /// Fill in the pixel size and encoded size for the collapsed line.
    pub fn probed(mut self) -> Self {
        self.dims = probe_dims(&self);
        self.bytes = self.encoded_len();
        self
    }

    fn encoded_len(&self) -> u64 {
        match (&self.path, &self.pixels) {
            (Some(path), _) => std::fs::metadata(path).map(|m| m.len()).unwrap_or(0),
            (None, Some(bytes)) => bytes.len() as u64,
            (None, None) => 0,
        }
    }

    /// The collapsed line: what it is, how big, and how to open it. Tab is the
    /// key that opens the selected block (the same one that opens a tool card),
    /// so a mouse click on the line and `M-x toggle-tool-card` work too.
    pub fn line(&self) -> String {
        format!("🖼 {} ({}) — tab opens", self.name, human_bytes(self.bytes))
    }
}

fn human_bytes(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

/// The pixel size of an image, read from its HEADER only — a full decode of a
/// multi-megabyte photo costs tens of milliseconds and is not needed to lay the
/// card out.
pub(crate) fn probe_dims(card: &ImageCard) -> Option<(u32, u32)> {
    match (&card.path, &card.pixels) {
        (Some(path), _) => image::ImageReader::open(path)
            .ok()?
            .with_guessed_format()
            .ok()?
            .into_dimensions()
            .ok(),
        (None, Some(bytes)) => cursor(bytes.clone()).into_dimensions().ok(),
        (None, None) => None,
    }
}

/// A reader over `bytes` with the format guessed from the magic bytes.
fn cursor(bytes: Vec<u8>) -> image::ImageReader<std::io::Cursor<Vec<u8>>> {
    image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .expect("a Cursor reader never fails")
}

/// How many character cells an image should occupy: fit inside `max_cols` ×
/// `max_rows` keeping its aspect ratio, never ENLARGING it past its own pixel
/// size (a 1×1 spy-pixel must not become a 48-cell blob), and never returning a
/// zero dimension.
pub(crate) fn image_cells(
    px: (u32, u32),
    font: (u16, u16),
    max_cols: u16,
    max_rows: u16,
) -> (u16, u16) {
    let (cell_w, cell_h) = (font.0.max(1) as f64, font.1.max(1) as f64);
    let (max_cols, max_rows) = (max_cols.max(1) as f64, max_rows.max(1) as f64);
    // The cells the image would need at its own size…
    let cols_natural = (px.0.max(1) as f64 / cell_w).ceil();
    let rows_natural = (px.1.max(1) as f64 / cell_h).ceil();
    // …then shrink (only) to fit the room available, keeping the ratio.
    let scale_limit = (max_cols / cols_natural).min(max_rows / rows_natural);
    let scale = if scale_limit < 1.0 { scale_limit } else { 1.0 };
    let cols = (cols_natural * scale).round().clamp(1.0, max_cols);
    let rows = (rows_natural * scale).round().clamp(1.0, max_rows);
    (cols as u16, rows as u16)
}

/// The rows an opened image reserves in the chat, or 0 when the chat is too
/// narrow for a preview to mean anything (the collapsed line is shown instead).
pub(crate) fn preview_rows(
    px: Option<(u32, u32)>,
    font: (u16, u16),
    chat_cols: u16,
    max_rows: u16,
) -> u16 {
    if chat_cols < MIN_PREVIEW_COLS {
        return 0;
    }
    let Some(px) = px else {
        return 0;
    };
    image_cells(px, font, chat_cols, max_rows).1
}

/// Find the images a message names: the assistant's own words and the output of
/// the tools it ran. `seen` carries the paths already shown anywhere in this
/// session, so the same chart is not re-rendered in every following message, and
/// at most `max` new cards come out of one message.
pub(crate) fn model_images(
    text: &str,
    root: &std::path::Path,
    seen: &mut HashSet<PathBuf>,
    max: usize,
) -> Vec<ImageCard> {
    let (parts, _) = comrade_tool::images_in_text(text, root);
    let mut out = Vec::new();
    for part in parts {
        if out.len() >= max {
            break;
        }
        // Only an image with a file behind it can be re-read for a preview; a
        // name with no path cannot have come from this scan.
        let Some(path) = part.path else {
            continue;
        };
        if !seen.insert(path.clone()) {
            continue;
        }
        out.push(ImageCard::file(part.name, path).probed());
    }
    out
}

/// The terminal's character cell size in pixels, discovered once when the
/// terminal is asked what it can render. Terminal-global (like the terminal
/// itself) and fixed for the run: the chat layout needs it to know how many rows
/// an opened image occupies. Before the query it is the 10×20 default that
/// `Picker::halfblocks` uses, which is all half-blocks need.
static CELL_PIXELS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

pub(crate) fn set_cell_pixels(font: (u16, u16)) {
    let packed = ((font.0 as u32) << 16) | font.1 as u32;
    CELL_PIXELS.store(packed, std::sync::atomic::Ordering::Relaxed);
}

/// The cell size the layout must assume, in pixels.
pub(crate) fn cell_pixels() -> (u16, u16) {
    let packed = CELL_PIXELS.load(std::sync::atomic::Ordering::Relaxed);
    if packed == 0 {
        (10, 20)
    } else {
        ((packed >> 16) as u16, (packed & 0xffff) as u16)
    }
}

/// A protocol plus the shape and source revision it was built for, so a resize
/// or a regenerated file re-encodes while everything else is reused.
struct Cached {
    protocol: StatefulProtocol,
    cols: u16,
    rows: u16,
    /// Identifies the pixels: the file's size and modification time, or the held
    /// bytes' length. A chart re-plotted during a run has a new stamp and is
    /// therefore drawn fresh instead of showing the previous picture.
    stamp: u64,
}

/// Paints opened image cards and holds their protocols.
pub(crate) struct ImageRenderer {
    picker: Picker,
    /// True while `picker` is a placeholder and the terminal has not been asked
    /// what it can do yet.
    needs_query: bool,
    /// One entry per card, keyed by name: resizing and encoding is the expensive
    /// part, so it is done once per size and per revision of the pixels.
    protocols: HashMap<String, Cached>,
}

impl ImageRenderer {
    /// A renderer that has not asked the terminal anything yet: it starts on
    /// half-blocks (which need no capabilities at all) and upgrades the moment
    /// the first image is opened.
    pub fn deferred() -> Self {
        Self {
            picker: Picker::halfblocks(),
            needs_query: true,
            protocols: HashMap::new(),
        }
    }

    /// Whether the terminal still has to be asked what it can render.
    pub fn needs_query(&self) -> bool {
        self.needs_query
    }

    /// The cell size in pixels the layout must use.
    pub fn font_size(&self) -> (u16, u16) {
        let f = self.picker.font_size();
        (f.0, f.1)
    }

    /// Paint `card` into `area`, building (and caching) its protocol on first
    /// use — and rebuilding it when the area's shape or the pixels change. A card
    /// whose pixels cannot be read is left blank: the collapsed line above it
    /// still names the file.
    pub fn draw(&mut self, frame: &mut Frame, area: Rect, card: &ImageCard) -> anyhow::Result<()> {
        let Some(dims) = card.dims else {
            return Ok(());
        };
        let (cols, rows) = image_cells(dims, self.font_size(), area.width, area.height);
        let stamp = stamp_of(card);
        let stale = !matches!(self.protocols.get(&card.name),
            Some(cached) if cached.cols == cols && cached.rows == rows && cached.stamp == stamp);
        if stale {
            let Some(img) = load(card) else {
                return Ok(());
            };
            let protocol = self.picker.new_resize_protocol(img);
            self.protocols.insert(
                card.name.clone(),
                Cached {
                    protocol,
                    cols,
                    rows,
                    stamp,
                },
            );
        }
        let Some(cached) = self.protocols.get_mut(&card.name) else {
            return Ok(());
        };
        let area = Rect {
            width: area.width.min(cached.cols),
            height: area.height.min(cached.rows),
            ..area
        };
        StatefulImage::new().render(area, frame.buffer_mut(), &mut cached.protocol);
        Ok(())
    }

    /// Forget every cached protocol, so the next draw re-encodes with the current
    /// picker (used after the terminal query upgrades it).
    pub fn invalidate(&mut self) {
        self.protocols.clear();
    }

    /// Ask the terminal what it can render — the kitty, iTerm2 or sixel graphics
    /// protocol, and its cell size — falling back to half-blocks when it does not
    /// answer. Returns the protocol's name for the chat.
    ///
    /// This WRITES a query and READS the reply from stdin and can therefore spend
    /// up to two seconds waiting on a terminal that does not answer. It must be
    /// called with the event reader paused, from the main loop, and only once:
    /// the caller checks [`Self::needs_query`] first.
    pub fn query(&mut self) -> &'static str {
        if let Ok(picker) = Picker::from_query_stdio() {
            self.picker = picker;
        }
        self.needs_query = false;
        set_cell_pixels(self.font_size());
        self.invalidate();
        match self.picker.protocol_type() {
            ProtocolType::Kitty => "kitty",
            ProtocolType::Iterm2 => "iterm2",
            ProtocolType::Sixel => "sixel",
            ProtocolType::Halfblocks => "half-blocks (no graphics protocol found)",
        }
    }
}

/// Identifies one revision of a card's pixels cheaply: a file's size and
/// modification time, or the length of the bytes held in memory.
fn stamp_of(card: &ImageCard) -> u64 {
    use std::hash::{Hash as _, Hasher as _};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    match (&card.path, &card.pixels) {
        (Some(path), _) => {
            if let Ok(meta) = std::fs::metadata(path) {
                meta.len().hash(&mut hasher);
                if let Ok(modified) = meta.modified()
                    && let Ok(since) = modified.duration_since(std::time::UNIX_EPOCH)
                {
                    since.as_nanos().hash(&mut hasher);
                }
            }
        }
        (None, Some(bytes)) => bytes.len().hash(&mut hasher),
        (None, None) => {}
    }
    hasher.finish()
}

/// Decode a card's pixels.
fn load(card: &ImageCard) -> Option<image::DynamicImage> {
    match (&card.path, &card.pixels) {
        (Some(path), _) => image::ImageReader::open(path)
            .ok()?
            .with_guessed_format()
            .ok()?
            .decode()
            .ok(),
        (None, Some(bytes)) => cursor(bytes.clone()).decode().ok(),
        (None, None) => None,
    }
}

/// A tiny PNG of a known size, for the tests of this module and of the TUI.
#[cfg(test)]
pub(crate) fn test_png(w: u32, h: u32) -> Vec<u8> {
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, w, h);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let rgba: Vec<u8> = (0..w * h).flat_map(|_| [200u8, 40, 40, 255]).collect();
    let mut writer = encoder.write_header().unwrap();
    writer.write_image_data(&rgba).unwrap();
    writer.finish().unwrap();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn png_bytes(w: u32, h: u32) -> Vec<u8> {
        test_png(w, h)
    }

    fn card(w: u32, h: u32) -> ImageCard {
        let mut c = ImageCard::pixels("chart.png", png_bytes(w, h)).probed();
        c.bytes = 2048;
        c
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("comrade-img-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_wide_image_fills_the_width_and_keeps_its_aspect() {
        // 1000x500 at a 10x20 cell is 2:1 — 80 cells wide, 20 rows tall.
        assert_eq!(image_cells((1000, 500), (10, 20), 80, 24), (80, 20));
    }

    #[test]
    fn a_tall_image_is_capped_by_height_and_narrowed() {
        // 1000x1000 at the full width would need 40 rows; capped at 24, so half
        // the width (the arithmetic rounds the odd cell away).
        let (cols, rows) = image_cells((1000, 1000), (10, 20), 80, 24);
        assert_eq!(rows, 24);
        assert_eq!(cols, 48);
    }

    #[test]
    fn an_extreme_aspect_ratio_still_yields_a_drawable_size() {
        let (cols, rows) = image_cells((10, 100_000), (10, 20), 80, 24);
        assert_eq!(rows, 24);
        assert!(cols >= 1, "never zero columns: {cols}");
        let (cols, rows) = image_cells((100_000, 10), (10, 20), 80, 24);
        assert_eq!(cols, 80);
        assert!(rows >= 1, "never zero rows: {rows}");
        // A one-pixel image is still one cell.
        assert_eq!(image_cells((1, 1), (10, 20), 80, 24), (1, 1));
    }

    #[test]
    fn a_narrow_chat_reserves_no_rows() {
        assert_eq!(preview_rows(Some((1000, 500)), (10, 20), 39, 24), 0);
        assert!(preview_rows(Some((1000, 500)), (10, 20), 40, 24) > 0);
        // No known size, no preview.
        assert_eq!(preview_rows(None, (10, 20), 80, 24), 0);
    }

    #[test]
    fn the_collapsed_line_names_the_image_and_its_size() {
        let c = card(4, 4);
        assert_eq!(c.line(), "🖼 chart.png (2.0 KiB) — tab opens");
        let mut small = card(4, 4);
        small.bytes = 900;
        assert_eq!(small.line(), "🖼 chart.png (900 B) — tab opens");
        let mut big = card(4, 4);
        big.bytes = 3 * 1024 * 1024;
        assert_eq!(big.line(), "🖼 chart.png (3.0 MiB) — tab opens");
    }

    #[test]
    fn a_card_starts_collapsed() {
        assert!(!card(4, 4).open, "images are collapsed by default");
    }

    #[test]
    fn probing_reads_the_header_of_a_file_and_of_held_bytes() {
        let dir = scratch("probe");
        let path = dir.join("shot.png");
        std::fs::write(&path, png_bytes(7, 3)).unwrap();
        assert_eq!(
            probe_dims(&ImageCard::file("shot.png", path.clone())),
            Some((7, 3))
        );
        assert_eq!(
            probe_dims(&ImageCard::pixels("held.png", png_bytes(2, 5))),
            Some((2, 5))
        );
        assert_eq!(
            probe_dims(&ImageCard::file("x.png", dir.join("absent.png"))),
            None
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A saved session must round-trip its images: a file-backed card comes back
    /// with its path, and a clipboard card comes back without its pixels rather
    /// than failing the save outright.
    #[test]
    fn cards_survive_a_session_round_trip() {
        let dir = scratch("serde");
        let path = dir.join("chart.png");
        std::fs::write(&path, png_bytes(3, 3)).unwrap();
        let file_card = ImageCard::file("chart.png", path.clone()).probed();
        let json = serde_json::to_string(&file_card).unwrap();
        let back: ImageCard = serde_json::from_str(&json).unwrap();
        assert_eq!(back.path, Some(path));
        assert_eq!(back.dims, Some((3, 3)));
        assert_eq!(back.bytes, file_card.bytes);
        assert!(!back.open);

        let pasted = ImageCard::pixels("clipboard.png", png_bytes(2, 2)).probed();
        let json = serde_json::to_string(&pasted).unwrap();
        assert!(!json.contains("pixels"), "pixels are not persisted: {json}");
        let back: ImageCard = serde_json::from_str(&json).unwrap();
        assert_eq!(back.name, "clipboard.png");
        assert!(back.pixels.is_none());
        assert_eq!(back.dims, Some((2, 2)), "the size is still known");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn model_images_finds_a_path_and_forgets_one_already_shown() {
        let dir = scratch("found");
        std::fs::write(dir.join("chart.png"), png_bytes(3, 3)).unwrap();
        let mut seen = HashSet::new();

        let found = model_images(
            "I plotted it in chart.png, take a look.",
            &dir,
            &mut seen,
            5,
        );
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "chart.png");
        assert_eq!(found[0].dims, Some((3, 3)));
        assert_eq!(found[0].path, Some(dir.join("chart.png")));
        assert!(!found[0].open, "discovered images start collapsed");

        // The same file in a later message is not shown again.
        let mut seen2 = seen.clone();
        let again = model_images("still chart.png", &dir, &mut seen2, 5);
        assert!(again.is_empty(), "a shown image is not repeated");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn model_images_caps_what_one_message_can_add() {
        let dir = scratch("cap");
        let mut text = String::new();
        for i in 0..4 {
            std::fs::write(dir.join(format!("c{i}.png")), png_bytes(2, 2)).unwrap();
            text.push_str(&format!("c{i}.png "));
        }
        let mut seen = HashSet::new();
        assert_eq!(model_images(&text, &dir, &mut seen, 2).len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn model_images_ignores_prose_and_missing_files() {
        let dir = scratch("none");
        let mut seen = HashSet::new();
        assert!(model_images("just a sentence, no picture", &dir, &mut seen, 5).is_empty());
        assert!(model_images("look at absent.png", &dir, &mut seen, 5).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_deferred_renderer_asks_the_terminal_only_when_an_image_is_opened() {
        let renderer = ImageRenderer::deferred();
        assert!(
            renderer.needs_query(),
            "the terminal is not asked at startup"
        );
        let renderer = ImageRenderer {
            needs_query: false,
            ..ImageRenderer::deferred()
        };
        assert!(!renderer.needs_query());
    }

    /// A card whose file changes (a chart re-plotted during a run) is drawn
    /// again rather than reusing the previous picture, and a resize re-encodes.
    #[test]
    fn a_changed_picture_or_area_re_encodes() {
        let dir = scratch("stamp");
        let path = dir.join("chart.png");
        std::fs::write(&path, png_bytes(40, 20)).unwrap();
        let card = ImageCard::file("chart.png", path.clone()).probed();

        let mut renderer = ImageRenderer::deferred();
        let mut terminal = Terminal::new(TestBackend::new(80, 30)).unwrap();
        let area = Rect::new(0, 0, 60, 10);
        terminal
            .draw(|f| renderer.draw(f, area, &card).unwrap())
            .unwrap();
        assert_eq!(renderer.protocols.len(), 1);
        let first = renderer.protocols.get("chart.png").unwrap().stamp;

        // Same picture, same area: reused, not re-encoded.
        terminal
            .draw(|f| renderer.draw(f, area, &card).unwrap())
            .unwrap();
        assert_eq!(
            renderer.protocols.get("chart.png").unwrap().stamp,
            first,
            "an unchanged card is not re-encoded"
        );

        // A regenerated file (different size, later mtime) invalidates it.
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&path, png_bytes(60, 30)).unwrap();
        let mut regenerated = card.clone();
        regenerated.dims = Some((60, 30));
        terminal
            .draw(|f| renderer.draw(f, area, &regenerated).unwrap())
            .unwrap();
        assert_ne!(
            renderer.protocols.get("chart.png").unwrap().stamp,
            first,
            "a re-plotted chart must be drawn fresh"
        );

        // A different area reshapes the picture.
        terminal
            .draw(|f| {
                renderer
                    .draw(f, Rect::new(0, 0, 30, 5), &regenerated)
                    .unwrap()
            })
            .unwrap();
        let cached = renderer.protocols.get("chart.png").unwrap();
        assert_eq!(
            (cached.cols, cached.rows),
            image_cells((60, 30), renderer.font_size(), 30, 5)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The point of the whole feature: an opened image actually paints cells.
    #[test]
    fn an_opened_image_paints_the_area_it_was_given() {
        let mut renderer = ImageRenderer::deferred();
        let mut terminal = Terminal::new(TestBackend::new(20, 8)).unwrap();
        let c = card(4, 2);

        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 10, 5);
                renderer.draw(f, area, &c).unwrap();
            })
            .unwrap();

        let painted = |buf: &ratatui::buffer::Buffer, x: u16, y: u16| {
            buf.cell((x, y)).is_some_and(|cell| cell.symbol() != " ")
        };
        let buf = terminal.backend().buffer();
        let inside = (0..5)
            .flat_map(|y| (0..10).map(move |x| (x, y)))
            .filter(|(x, y)| painted(buf, *x, *y))
            .count();
        assert!(inside > 0, "the reserved area must be painted");
        assert!(
            !painted(terminal.backend().buffer(), 15, 2),
            "the image must stay inside its rect"
        );
    }
}
