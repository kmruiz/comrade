/// The delegate's answer to a context-readiness check for a plan step.
pub(crate) enum Readiness {
    Ready,
    NeedsMore(String),
}

/// Extract the verdict from an advisor's reply. The readiness prompt asks the
/// delegate to close with exactly one final `VERDICT:` line; the last one wins.
/// A reply without any `VERDICT:` line is treated as "needs more" (a step is
/// never marked ready without an explicit READY verdict).
pub(crate) fn readiness_verdict(reply: &str) -> Readiness {
    let mut verdict: Option<Readiness> = None;
    for line in reply.lines() {
        let line = line.trim();
        let Some(rest) = line
            .strip_prefix("VERDICT:")
            .or_else(|| line.strip_prefix("verdict:"))
        else {
            continue;
        };
        let rest = rest.trim();
        let value = rest
            .split_once(':')
            .map(|(k, v)| (k.trim(), Some(v.trim())))
            .or(Some((rest, None)));
        let Some((kind, extra)) = value else {
            continue;
        };
        verdict = Some(if kind.eq_ignore_ascii_case("READY") {
            Readiness::Ready
        } else if kind.eq_ignore_ascii_case("NEEDS_MORE") {
            Readiness::NeedsMore(extra.filter(|e| !e.is_empty()).unwrap_or(rest).to_string())
        } else {
            // Unknown verdict keyword: be conservative.
            Readiness::NeedsMore(line.to_string())
        });
    }
    verdict.unwrap_or_else(|| {
        Readiness::NeedsMore(
            reply
                .lines()
                .last()
                .unwrap_or("(the delegate gave no verdict)")
                .trim()
                .to_string(),
        )
    })
}
