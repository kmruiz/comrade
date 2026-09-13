//! Secret redaction: scrub credential-looking values out of tool output before
//! it is fed back to the model or shown in the transcript.
//!
//! Two layers:
//! 1. exact values harvested from the environment (any env var whose NAME looks
//!    like a credential), plus anything the caller adds;
//! 2. a prefix scan for well-known token shapes (`sk-…`, `ghp_…`, `AKIA…`) that
//!    appear inline in command output.
//!
//! Redaction is best-effort: it lowers the chance of leaking a key into a cloud
//! model, not a guarantee. Off when `[security] redact_secrets = false`.

/// What a redacted value is replaced with.
const REDACTION: &str = "«redacted»";

/// Minimum length a value must have to be treated as a secret (avoids scrubbing
/// innocuous short strings that happen to live in a `*_KEY` env var).
const MIN_SECRET_LEN: usize = 8;

/// Token prefixes that are always scrubbed, with the minimum tail length that
/// makes the match look like a real credential.
const PREFIXES: &[&str] = &[
    "sk-",
    "ghp_",
    "gho_",
    "ghu_",
    "ghs_",
    "github_pat_",
    "xoxb-",
    "xoxp-",
    "AKIA",
    "AIza",
];
const MIN_PREFIX_TAIL: usize = 16;

/// Holds the values to scrub.
#[derive(Clone, Debug, Default)]
pub struct Redactor {
    secrets: Vec<String>,
}

impl Redactor {
    /// A no-op redactor (redaction disabled).
    pub fn none() -> Self {
        Self::default()
    }

    /// Build from an explicit list of secret values.
    pub fn with_secrets(secrets: Vec<String>) -> Self {
        Self {
            secrets: secrets
                .into_iter()
                .filter(|s| s.chars().count() >= MIN_SECRET_LEN)
                .collect(),
        }
    }

    /// Harvest secret-looking values from the process environment: any variable
    /// whose name contains a credential keyword and whose value is long enough.
    pub fn from_env() -> Self {
        let mut secrets = Vec::new();
        for (name, value) in std::env::vars() {
            let upper = name.to_ascii_uppercase();
            let looks_secret = upper.contains("TOKEN")
                || upper.contains("SECRET")
                || upper.contains("PASSWORD")
                || upper.contains("PASSWD")
                || upper.contains("CREDENTIAL")
                || upper.contains("API_KEY")
                || upper.ends_with("_KEY")
                || upper.ends_with("_AUTH");
            if looks_secret && value.chars().count() >= MIN_SECRET_LEN {
                secrets.push(value);
            }
        }
        // Longest first so a short secret that is a substring of a longer one
        // cannot leave the tail of the longer one behind.
        secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
        Self { secrets }
    }

    /// True when there is nothing to scrub (so callers can skip the work).
    pub fn is_empty(&self) -> bool {
        self.secrets.is_empty()
    }

    /// Replace every known secret value and every prefixed token in `text`.
    pub fn redact(&self, text: &str) -> String {
        if self.secrets.is_empty() && !PREFIXES.iter().any(|p| text.contains(p)) {
            return text.to_string();
        }
        // 1. exact values
        let mut out = text.to_string();
        for secret in &self.secrets {
            if out.contains(secret.as_str()) {
                out = out.replace(secret.as_str(), REDACTION);
            }
        }
        // 2. prefixed token shapes
        scan_prefixed(&out)
    }
}

/// Scan for `PREFIX…` tokens with a long enough tail and redact them.
fn scan_prefixed(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut last = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        let matched = PREFIXES
            .iter()
            .find(|p| bytes[i..].starts_with(p.as_bytes()));
        let Some(prefix) = matched else {
            i += 1;
            continue;
        };
        let mut j = i + prefix.len();
        while j < bytes.len()
            && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_' || bytes[j] == b'-')
        {
            j += 1;
        }
        if j - i >= prefix.len() + MIN_PREFIX_TAIL {
            out.push_str(&text[last..i]);
            out.push_str(REDACTION);
            last = j;
            i = j;
        } else {
            i += 1;
        }
    }
    out.push_str(&text[last..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_secret_values_are_replaced() {
        let r = Redactor::with_secrets(vec!["super-secret-token".into()]);
        let out = r.redact("Authorization: Bearer super-secret-token (leaked)");
        assert!(out.contains(REDACTION), "{out}");
        assert!(!out.contains("super-secret-token"), "{out}");
    }

    #[test]
    fn short_values_are_left_alone() {
        let r = Redactor::with_secrets(vec!["abc".into()]);
        assert!(r.is_empty());
        assert_eq!(r.redact("abc stays"), "abc stays");
    }

    #[test]
    fn prefixed_tokens_are_scrubbed() {
        let r = Redactor::none();
        let out = r.redact("key=sk-abcDEF1234567890abcDEF and ghp_ABCDEFGHIJKLMNOPQRSTUVWX");
        assert!(!out.contains("sk-abcDEF1234567890abcDEF"), "{out}");
        assert!(!out.contains("ghp_ABCDEFGHIJKLMNOPQRSTUVWX"), "{out}");

        // A short lookalike is not treated as a credential.
        assert_eq!(r.redact("sk-short"), "sk-short");
    }
}
