//! Image attachments for one message from the human.
//!
//! A user prompt may carry images next to its text: a path the human typed,
//! dropped into the terminal or pasted, or a `data:`-encoded image off the
//! system clipboard. Both travel as ONE message — the text and its images are
//! never split across two turns — and reach the model as OpenAI-style content
//! parts (`{"type": "image_url", "image_url": {"url": "data:…"}}`).
//!
//! The image *type* is decided by magic bytes, never by the file extension, so
//! a mislabelled `chart.png` that is really a JPEG is sent as `image/jpeg`.

use std::path::{Path, PathBuf};

use base64::Engine as _;

/// Most images one message may carry. Beyond this the prompt stays readable and
/// the request stays small.
pub const MAX_IMAGES: usize = 5;
/// Largest image that is sent. Bigger images are refused with a clear error
/// rather than silently resized or dropped.
pub const MAX_IMAGE_BYTES: usize = 8 * 1024 * 1024;

/// The image formats Comrade can send, detected from the bytes themselves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageMime {
    Png,
    Jpeg,
    Gif,
    WebP,
}

impl ImageMime {
    /// The media type used in a `data:` URI.
    pub fn mime(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Gif => "image/gif",
            Self::WebP => "image/webp",
        }
    }

    /// The file extension the format usually has.
    pub fn ext(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpg",
            Self::Gif => "gif",
            Self::WebP => "webp",
        }
    }

    /// The format `bytes` are really in, from their magic bytes. `None` when
    /// they are not an image (or the header is truncated).
    pub fn sniff(bytes: &[u8]) -> Option<Self> {
        if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
            return Some(Self::Png);
        }
        if bytes.starts_with(b"\xff\xd8\xff") {
            return Some(Self::Jpeg);
        }
        if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
            return Some(Self::Gif);
        }
        if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
            return Some(Self::WebP);
        }
        None
    }
}

/// One image attached to a user message: what to call it, its real media type
/// and its base64 payload (no `data:` prefix — see [`ImagePart::data_uri`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImagePart {
    pub name: String,
    pub mime: ImageMime,
    pub base64: String,
}

impl ImagePart {
    /// Attach `bytes` under the display `name`, refusing a non-image or one
    /// over [`MAX_IMAGE_BYTES`].
    pub fn from_bytes(name: impl Into<String>, bytes: &[u8]) -> Result<Self, AttachError> {
        let name = name.into();
        if bytes.len() > MAX_IMAGE_BYTES {
            return Err(AttachError::TooLarge {
                name,
                size: bytes.len(),
                limit: MAX_IMAGE_BYTES,
            });
        }
        let mime = ImageMime::sniff(bytes).ok_or_else(|| AttachError::Unsupported(name.clone()))?;
        Ok(Self {
            name,
            mime,
            base64: base64::engine::general_purpose::STANDARD.encode(bytes),
        })
    }

    /// The `data:` URI a model request carries.
    pub fn data_uri(&self) -> String {
        format!("data:{};base64,{}", self.mime.mime(), self.base64)
    }

    /// Rebuild a part from the `data:` URI it travels as, when a serialized
    /// message is read back. The display name is deliberately NOT part of the
    /// wire form (no provider understands an unknown field there), so it is
    /// derived from the media type.
    pub fn from_data_uri(uri: &str) -> Option<Self> {
        let (meta, payload) = uri.strip_prefix("data:")?.split_once(";base64,")?;
        let mime = match meta {
            "image/png" => ImageMime::Png,
            "image/jpeg" => ImageMime::Jpeg,
            "image/gif" => ImageMime::Gif,
            "image/webp" => ImageMime::WebP,
            _ => return None,
        };
        Some(Self {
            name: format!("image.{}", mime.ext()),
            mime,
            base64: payload.to_string(),
        })
    }

    /// Size of the encoded image in bytes (the base64 payload decoded).
    pub fn bytes(&self) -> usize {
        let padding = self.base64.bytes().filter(|b| *b == b'=').count();
        self.base64.len() / 4 * 3 - padding
    }
}

/// Why an image could not be attached.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AttachError {
    #[error("{0} is not a PNG, JPEG, GIF or WebP image")]
    Unsupported(String),
    #[error("{name} is {size} bytes; the limit is {limit} bytes")]
    TooLarge {
        name: String,
        size: usize,
        limit: usize,
    },
    #[error("cannot read {path}: {err}")]
    Io { path: String, err: String },
}

/// Read an image file, checking the size cap from the metadata before the whole
/// file is read, then its real type from the magic bytes.
pub fn load_image_file(path: &Path) -> Result<ImagePart, AttachError> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    let meta = std::fs::metadata(path).map_err(|e| AttachError::Io {
        path: path.display().to_string(),
        err: e.to_string(),
    })?;
    if meta.len() > MAX_IMAGE_BYTES as u64 {
        return Err(AttachError::TooLarge {
            name,
            size: meta.len() as usize,
            limit: MAX_IMAGE_BYTES,
        });
    }
    let bytes = std::fs::read(path).map_err(|e| AttachError::Io {
        path: path.display().to_string(),
        err: e.to_string(),
    })?;
    ImagePart::from_bytes(name, &bytes)
}

/// Collect the images `text` refers to, in order, at most [`MAX_IMAGES`] of
/// them. A token is a candidate only when it ends in an image extension;
/// relative paths resolve against `root`. A candidate that does not exist is
/// just prose and is skipped silently; one that exists but is not an image is
/// reported. Returns the attachments and one error per rejected candidate.
pub fn images_in_text(text: &str, root: &Path) -> (Vec<ImagePart>, Vec<AttachError>) {
    let mut parts = Vec::new();
    let mut errors = Vec::new();
    let mut seen: Vec<PathBuf> = Vec::new();
    for token in tokens(text) {
        if parts.len() >= MAX_IMAGES {
            break;
        }
        let token = clean(&token);
        if !has_image_extension(token) {
            continue;
        }
        let path = resolve(token, root);
        if seen.contains(&path) {
            continue;
        }
        seen.push(path.clone());
        if !path.is_file() {
            continue;
        }
        match load_image_file(&path) {
            Ok(part) => parts.push(part),
            Err(e) => errors.push(e),
        }
    }
    (parts, errors)
}

/// One message from the human: the text plus the images attached to it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UserInput {
    pub text: String,
    pub images: Vec<ImagePart>,
}

impl UserInput {
    /// A plain text prompt, the way every caller without an image builds one.
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            images: Vec::new(),
        }
    }

    /// `true` when the message carries neither text nor an image.
    pub fn is_empty(&self) -> bool {
        self.text.trim().is_empty() && self.images.is_empty()
    }

    /// What the chat transcript shows: the text, then one `[image: name]` line
    /// per attachment, so the human sees what the model received.
    pub fn transcript(&self) -> String {
        let mut out = self.text.clone();
        for image in &self.images {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&format!("[image: {}]", image.name));
        }
        out
    }

    /// Move the attachments into the text as placeholders instead of sending
    /// them, for a model that cannot see. Returns whether anything was dropped.
    pub fn downgrade_images(&mut self, reason: &str) -> bool {
        if self.images.is_empty() {
            return false;
        }
        for image in std::mem::take(&mut self.images) {
            if !self.text.is_empty() {
                self.text.push('\n');
            }
            self.text
                .push_str(&format!("[image not sent: {reason} - {}]", image.name));
        }
        true
    }
}

impl From<String> for UserInput {
    fn from(text: String) -> Self {
        Self::text(text)
    }
}

impl From<&str> for UserInput {
    fn from(text: &str) -> Self {
        Self::text(text)
    }
}

/// Split `text` on whitespace, treating a single- or double-quoted run as one
/// token and a `\ `-escaped space as part of the token (a dropped path arrives
/// quoted or escaped, depending on the terminal).
fn tokens(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match quote {
            Some(q) if ch == q => quote = None,
            Some(_) => cur.push(ch),
            None if ch == '\\' => match chars.peek() {
                Some(next) if next.is_whitespace() || *next == '\\' || is_quote(*next) => {
                    cur.push(chars.next().unwrap());
                }
                _ => cur.push(ch),
            },
            None if is_quote(ch) => quote = Some(ch),
            None if ch.is_whitespace() => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            None => cur.push(ch),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn is_quote(ch: char) -> bool {
    ch == '"' || ch == '\''
}

/// Strip the decoration a terminal or a sentence adds around a path: a
/// `file://` scheme, wrapping brackets and trailing punctuation.
fn clean(token: &str) -> &str {
    let token = token.strip_prefix("file://").unwrap_or(token);
    let token = token.trim_start_matches(['(', '[', '{']);
    token.trim_end_matches([',', '.', ';', ':', ')', ']', '}', '"', '\''])
}

fn has_image_extension(token: &str) -> bool {
    Path::new(token)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .is_some_and(|e| matches!(e.as_str(), "png" | "jpg" | "jpeg" | "gif" | "webp"))
}

/// The absolute, `.`-normalised form of `token`, so the same file spelled two
/// ways (`dup.png`, `./dup.png`) is attached only once.
fn resolve(token: &str, root: &Path) -> PathBuf {
    let path = Path::new(token);
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    let mut out = PathBuf::new();
    for component in path.components() {
        if component != std::path::Component::CurDir {
            out.push(component.as_os_str());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n";
    const JPEG: &[u8] = b"\xff\xd8\xff\xe0";
    const GIF87: &[u8] = b"GIF87a";
    const GIF89: &[u8] = b"GIF89a";
    const WEBP: &[u8] = b"RIFF\x00\x00\x00\x00WEBP";

    /// A per-process scratch directory (comrade-tool has no tempfile dev-dep).
    fn sandbox() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("comrade-attach-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_file(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, bytes).unwrap();
        p
    }

    #[test]
    fn sniff_recognises_every_supported_format() {
        assert_eq!(ImageMime::sniff(PNG), Some(ImageMime::Png));
        assert_eq!(ImageMime::sniff(JPEG), Some(ImageMime::Jpeg));
        assert_eq!(ImageMime::sniff(GIF87), Some(ImageMime::Gif));
        assert_eq!(ImageMime::sniff(GIF89), Some(ImageMime::Gif));
        assert_eq!(ImageMime::sniff(WEBP), Some(ImageMime::WebP));
        assert_eq!(ImageMime::Png.mime(), "image/png");
        assert_eq!(ImageMime::Jpeg.mime(), "image/jpeg");
        assert_eq!(ImageMime::Gif.mime(), "image/gif");
        assert_eq!(ImageMime::WebP.mime(), "image/webp");
    }

    #[test]
    fn sniff_rejects_unknown_and_truncated_headers() {
        assert_eq!(ImageMime::sniff(b"hello world"), None);
        assert_eq!(ImageMime::sniff(b""), None);
        // A RIFF container that is not WebP (e.g. a wav file).
        assert_eq!(ImageMime::sniff(b"RIFF\x00\x00\x00\x00WAVE"), None);
        // A truncated PNG signature is not enough.
        assert_eq!(ImageMime::sniff(b"\x89PNG"), None);
        // Magic bytes must be at the very start, not merely present.
        assert_eq!(ImageMime::sniff(b"xx\x89PNG\r\n\x1a\n"), None);
    }

    #[test]
    fn from_bytes_rejects_a_non_image() {
        assert!(matches!(
            ImagePart::from_bytes("notes.txt", b"just text"),
            Err(AttachError::Unsupported(_))
        ));
    }

    #[test]
    fn from_bytes_builds_a_data_uri_and_reports_its_size() {
        let part = ImagePart::from_bytes("shot.png", PNG).unwrap();
        assert_eq!(part.name, "shot.png");
        assert_eq!(part.mime, ImageMime::Png);
        assert_eq!(part.base64, "iVBORw0KGgo=");
        assert_eq!(part.data_uri(), "data:image/png;base64,iVBORw0KGgo=");
        assert_eq!(part.bytes(), PNG.len());
    }

    #[test]
    fn from_bytes_encodes_a_payload_whose_length_needs_padding() {
        let part = ImagePart::from_bytes("a.png", b"\x89PNG\r\n\x1a\nabcd").unwrap();
        assert_eq!(part.bytes(), 12);
        // base64 of 12 bytes is 16 chars with no padding, decoded back exactly.
        assert_eq!(part.base64.len(), 16);
        assert_eq!(part.data_uri().len(), "data:image/png;base64,".len() + 16);
    }

    #[test]
    fn load_image_file_encodes_a_real_file() {
        let dir = sandbox();
        let path = write_file(&dir, "load_ok.png", PNG);
        let part = load_image_file(&path).unwrap();
        assert_eq!(part.name, "load_ok.png");
        assert_eq!(part.data_uri(), "data:image/png;base64,iVBORw0KGgo=");
    }

    #[test]
    fn load_image_file_trusts_magic_bytes_over_the_extension() {
        let dir = sandbox();
        // A JPEG that is named .png must be sent as image/jpeg.
        let path = write_file(&dir, "mislabelled.png", JPEG);
        assert_eq!(load_image_file(&path).unwrap().mime, ImageMime::Jpeg);
    }

    #[test]
    fn load_image_file_refuses_an_image_over_the_size_limit() {
        let dir = sandbox();
        let mut bytes = PNG.to_vec();
        bytes.resize(MAX_IMAGE_BYTES + 1, 0);
        let path = write_file(&dir, "huge.png", &bytes);
        match load_image_file(&path) {
            Err(AttachError::TooLarge { size, limit, .. }) => {
                assert_eq!(size, MAX_IMAGE_BYTES + 1);
                assert_eq!(limit, MAX_IMAGE_BYTES);
            }
            other => panic!("expected TooLarge, got {other:?}"),
        }
    }

    #[test]
    fn load_image_file_reports_a_missing_file() {
        let dir = sandbox();
        let missing = dir.join("definitely_absent.png");
        assert!(matches!(
            load_image_file(&missing),
            Err(AttachError::Io { .. })
        ));
    }

    #[test]
    fn images_in_text_finds_an_absolute_path() {
        let dir = sandbox();
        let path = write_file(&dir, "abs.png", PNG);
        let (parts, errors) =
            images_in_text(&format!("what is wrong here? {}", path.display()), &dir);
        assert!(errors.is_empty());
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].name, "abs.png");
    }

    #[test]
    fn images_in_text_handles_a_quoted_path_with_spaces_and_a_file_url() {
        let dir = sandbox();
        let path = write_file(&dir, "my shot.png", PNG);
        let (parts, errors) =
            images_in_text(&format!("look at \"{}\" please", path.display()), &dir);
        assert!(errors.is_empty());
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].name, "my shot.png");

        let (url_parts, _) = images_in_text(&format!("see \"file://{}\"", path.display()), &dir);
        assert_eq!(url_parts.len(), 1);

        // A terminal that escapes the space instead of quoting it.
        let (escaped, _) = images_in_text(
            &format!("look at {}", path.display().to_string().replace(' ', "\\ ")),
            &dir,
        );
        assert_eq!(escaped.len(), 1);
        assert_eq!(escaped[0].name, "my shot.png");
    }

    #[test]
    fn images_in_text_resolves_relative_paths_against_the_root() {
        let dir = sandbox();
        write_file(&dir, "rel.png", PNG);
        for text in ["rel.png", "./rel.png", "look at rel.png, then fix it"] {
            let (parts, errors) = images_in_text(text, &dir);
            assert!(errors.is_empty(), "{text}: {errors:?}");
            assert_eq!(parts.len(), 1, "{text}");
        }
        // A trailing sentence period must not defeat the detection.
        let (parts, _) = images_in_text("see rel.png.", &dir);
        assert_eq!(parts.len(), 1);
    }

    #[test]
    fn images_in_text_ignores_missing_files_and_duplicates() {
        let dir = sandbox();
        write_file(&dir, "dup.png", PNG);
        let (parts, errors) = images_in_text(
            "main.rs dup.png dup.png ./dup.png missing.png /nope/gone.jpg",
            &dir,
        );
        assert_eq!(parts.len(), 1, "a path is attached once");
        assert!(errors.is_empty(), "a missing file is just text: {errors:?}");
    }

    #[test]
    fn images_in_text_flags_a_file_that_only_looks_like_an_image() {
        let dir = sandbox();
        write_file(&dir, "fake.png", b"this is not a png");
        let (parts, errors) = images_in_text("check fake.png", &dir);
        assert!(parts.is_empty());
        assert_eq!(errors.len(), 1);
        assert!(matches!(errors[0], AttachError::Unsupported(_)));
    }

    #[test]
    fn images_in_text_stops_at_the_attachment_limit() {
        let dir = sandbox();
        let mut text = String::new();
        for i in 0..MAX_IMAGES + 2 {
            write_file(&dir, &format!("many{i}.png"), PNG);
            text.push_str(&format!("many{i}.png "));
        }
        let (parts, _) = images_in_text(&text, &dir);
        assert_eq!(parts.len(), MAX_IMAGES);
    }

    #[test]
    fn transcript_lists_the_attachment_names() {
        let shot = ImagePart::from_bytes("shot.png", PNG).unwrap();
        let chart = ImagePart::from_bytes("chart.jpg", JPEG).unwrap();

        let both = UserInput {
            text: "why is this wrong?".into(),
            images: vec![shot.clone(), chart.clone()],
        };
        assert_eq!(
            both.transcript(),
            "why is this wrong?\n[image: shot.png]\n[image: chart.jpg]"
        );
        assert!(!both.is_empty());

        let image_only = UserInput {
            text: String::new(),
            images: vec![shot.clone()],
        };
        assert_eq!(image_only.transcript(), "[image: shot.png]");
        assert!(!image_only.is_empty());

        assert_eq!(UserInput::text("plain").transcript(), "plain");
        assert!(UserInput::default().is_empty());
    }

    #[test]
    fn downgrade_images_moves_the_attachments_into_the_text() {
        let mut input = UserInput {
            text: "why is this wrong?".into(),
            images: vec![ImagePart::from_bytes("shot.png", PNG).unwrap()],
        };
        assert!(input.downgrade_images("the model has no vision support"));
        assert!(input.images.is_empty());
        assert_eq!(
            input.text,
            "why is this wrong?\n[image not sent: the model has no vision support - shot.png]"
        );
    }

    #[test]
    fn downgrade_images_is_a_no_op_without_images() {
        let mut input = UserInput::text("plain");
        assert!(!input.downgrade_images("no vision"));
        assert_eq!(input.text, "plain");
    }

    #[test]
    fn from_data_uri_rebuilds_a_part() {
        let uri = "data:image/png;base64,iVBORw0KGgo=";
        let part = ImagePart::from_data_uri(uri).unwrap();
        assert_eq!(part.mime, ImageMime::Png);
        assert_eq!(part.base64, "iVBORw0KGgo=");
        assert_eq!(part.data_uri(), uri, "it round-trips");
        assert_eq!(part.name, "image.png");

        assert!(ImagePart::from_data_uri("data:application/pdf;base64,AAA").is_none());
        assert!(ImagePart::from_data_uri("not a uri").is_none());
    }

    #[test]
    fn user_input_can_be_built_from_a_string() {
        let input: UserInput = "do the task".into();
        assert_eq!(input.text, "do the task");
        assert!(input.images.is_empty());
        assert_eq!(UserInput::from(String::from("x")).text, "x");
    }
}
