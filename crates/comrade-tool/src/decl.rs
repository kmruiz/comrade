//! A cheap, language-tolerant scan of the declarations a source file defines.
//!
//! The only consumer is a safety check: a sub-agent that rewrites a whole file
//! with `fs_write_file` must not silently delete code that was already there, so
//! both the filesystem tool (a warning in its result) and the delegate loop (a
//! permission gate, `delegate::refuse_destructive`) need to know WHAT a rewrite
//! would drop. This is deliberately not a parser: it looks for the `fn NAME` /
//! `function NAME` keywords several languages share, which is enough to name a
//! deletion in a message a small model can act on.

/// The `fn` / `function` declarations `src` defines, in order of appearance.
///
/// Byte-oriented and panic-free on arbitrary input (a multi-byte character can
/// never continue an ASCII identifier, so every slice lands on a char boundary).
pub fn declarations(src: &str) -> Vec<String> {
    /// Identifier introducers shared by the languages we care about, each with
    /// the trailing space the source must have for a name to follow.
    const KEYWORDS: [&str; 2] = ["fn ", "function "];
    let bytes = src.as_bytes();
    let mut out = Vec::new();
    for kw in KEYWORDS {
        let mut i = 0usize;
        while let Some(pos) = src[i..].find(kw) {
            let start = i + pos + kw.len();
            let mut end = start;
            while end < bytes.len()
                && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_' || bytes[end] == b'$')
            {
                end += 1;
            }
            let name = &src[start..end];
            if !name.is_empty() {
                out.push(name.to_string());
            }
            // Always advance past the keyword, so the scan terminates.
            i = end.max(start);
            if i >= src.len() {
                break;
            }
        }
    }
    out
}

/// Declarations `before` had that `after` no longer defines: what a whole-file
/// rewrite is about to delete. Empty when nothing is lost (the common case, and
/// the reason the gate below costs nothing until a real deletion appears).
pub fn removed_declarations(before: &str, after: &str) -> Vec<String> {
    let kept = declarations(after);
    let mut out: Vec<String> = Vec::new();
    for name in declarations(before) {
        if !kept.contains(&name) && !out.contains(&name) {
            out.push(name);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declarations_lists_rust_and_js_names() {
        let src = "pub fn greet(name: &str) -> String {\n    format!(\"h\")\n}\n\n\
                   #[cfg(test)]\nmod tests {\n    #[test]\n    fn greet_works() {}\n\n    fn helper() {}\n}\n";
        assert_eq!(declarations(src), ["greet", "greet_works", "helper"]);
    }

    #[test]
    fn declarations_finds_js_functions() {
        let src = "function shout(s) { return s; }\nconst f = function inner() {};\n";
        assert_eq!(declarations(src), ["shout", "inner"]);
    }

    #[test]
    fn declarations_is_empty_without_keywords_and_never_panics() {
        assert!(declarations("let x = 1;\n").is_empty());
        assert!(declarations("").is_empty());
        // Multi-byte text around/inside the keyword must not panic; a non-ASCII
        // identifier is simply not captured (the scan is ASCII, by design).
        assert!(declarations("héllo fn😀 word").is_empty());
        assert!(declarations("fn é() {}").is_empty());
    }

    #[test]
    fn removed_declarations_names_what_a_rewrite_drops() {
        let before = "pub fn greet() {}\n\n#[cfg(test)]\nmod tests {\n    fn greet_works() {}\n}\n";
        let after = "pub fn shout() {}\n";
        assert_eq!(
            removed_declarations(before, after),
            ["greet", "greet_works"]
        );
        // An addition, or an unchanged file, loses nothing.
        assert!(removed_declarations(before, before).is_empty());
        assert!(
            removed_declarations("fn a() {}", "fn a() {}\nfn b() {}").is_empty(),
            "adding a function is not a deletion"
        );
    }
}
