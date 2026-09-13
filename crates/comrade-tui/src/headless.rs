use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use comrade_core::Autonomy;
use comrade_tool::{FormSpec, UserIo, UserPrompt, UserReply};

use crate::{Deps, new_session};

/// A no-UI [`UserIo`]: honours the autonomy policy and otherwise falls back to
/// prompting on stdin.
struct HeadlessIo {
    autonomy: Autonomy,
}

#[async_trait]
impl UserIo for HeadlessIo {
    async fn ask(&self, prompt: UserPrompt) -> Result<UserReply> {
        let reply = match prompt {
            UserPrompt::Confirm { title, .. } => match self.autonomy {
                Autonomy::Auto => UserReply::Answer("yes".into()),
                Autonomy::Deny => UserReply::Answer("no".into()),
                Autonomy::Ask => UserReply::Answer(confirm_on_stdin(&title).await?),
            },
            UserPrompt::Question {
                prompt,
                options,
                recommended,
            } => match self.autonomy {
                Autonomy::Auto => match auto_question_answer(&options, recommended.as_deref()) {
                    Some(a) => UserReply::Answer(a),
                    None => UserReply::Answer(question_on_stdin(&prompt, &options, None).await?),
                },
                _ => UserReply::Answer(
                    question_on_stdin(&prompt, &options, recommended.as_deref()).await?,
                ),
            },
            UserPrompt::Form(spec) => match self.autonomy {
                Autonomy::Auto => UserReply::Form(spec.initial_values()),
                _ => UserReply::Form(form_on_stdin(&spec).await?),
            },
        };
        Ok(reply)
    }
}

/// Ask each form field on stdin, keeping the seed value when the line is empty.
async fn form_on_stdin(spec: &FormSpec) -> Result<BTreeMap<String, String>> {
    let mut answers = spec.initial_values();
    for f in &spec.fields {
        let label = format!("[comrade] {} ({}) [{}]: ", f.label, f.id, answers[&f.id]);
        let line = read_line(&label).await?;
        if !line.trim().is_empty() {
            answers.insert(f.id.clone(), line.trim().to_string());
        }
    }
    Ok(answers)
}

/// The answer auto mode gives a question: its recommended value when set and
/// non-blank, else the first option when any. `None` means "no hint, ask".
fn auto_question_answer(options: &[String], recommended: Option<&str>) -> Option<String> {
    recommended
        .filter(|r| !r.trim().is_empty())
        .map(str::to_string)
        .or_else(|| options.first().cloned())
}

async fn confirm_on_stdin(title: &str) -> Result<String> {
    let answer = read_line(&format!("[comrade] {title} [y/N] ")).await?;
    Ok(answer)
}

async fn question_on_stdin(
    question: &str,
    options: &[String],
    recommended: Option<&str>,
) -> Result<String> {
    let recommended = recommended.filter(|r| !r.trim().is_empty());
    let mut msg = format!("[comrade] {question}");
    // With free-form input the recommended answer is shown as a hint; with
    // options it is expected to be one of them (the TUI flags it instead).
    if options.is_empty()
        && let Some(r) = recommended
    {
        msg.push_str(&format!("\n  (recommended: {r})"));
    }
    if !options.is_empty() {
        for (i, o) in options.iter().enumerate() {
            msg.push_str(&format!("\n  {}. {o}", i + 1));
        }
    }
    msg.push_str("\n> ");
    let answer = read_line(&msg).await?;
    if !options.is_empty()
        && let Ok(n) = answer.trim().parse::<usize>()
        && n >= 1
        && n <= options.len()
    {
        return Ok(options[n - 1].clone());
    }
    // An empty line accepts the recommended answer.
    if answer.trim().is_empty()
        && let Some(r) = recommended
    {
        return Ok(r.to_string());
    }
    Ok(answer)
}

async fn read_line(prompt: &str) -> Result<String> {
    let prompt = prompt.to_string();
    let line = tokio::task::spawn_blocking(move || {
        use std::io::Write;
        eprint!("{prompt}");
        std::io::stderr().flush().ok();
        let mut buf = String::new();
        std::io::stdin().read_line(&mut buf).ok();
        buf.trim().to_string()
    })
    .await?;
    Ok(line)
}

pub async fn run(deps: &Deps, prompt: &str) -> Result<()> {
    let prompt = if prompt.trim().is_empty() {
        anyhow::bail!("headless mode needs a prompt; pass one as an argument or use the TUI")
    } else {
        prompt.trim().to_string()
    };

    let io = Arc::new(HeadlessIo {
        autonomy: deps.cfg.security.autonomy,
    });
    let (bundle, _tx, _rx) = new_session(deps, io);
    let ctx = bundle.ctx_base.clone();

    println!(
        "comrade: project={} model={} autonomy={}",
        deps.root.display(),
        deps.cfg.llm.model,
        deps.cfg.security.autonomy.as_str()
    );

    let outcome =
        comrade_core::agent::run_headless(&deps.cfg, &deps.client, ctx, &deps.tools, prompt)
            .await?;
    println!("\n[comrade] done in {} iteration(s).", outcome.iterations);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::auto_question_answer;

    #[test]
    fn auto_prefers_recommended_over_first_option() {
        let options = vec!["single".to_string(), "double".to_string()];
        assert_eq!(
            auto_question_answer(&options, Some("double")).as_deref(),
            Some("double")
        );
    }

    #[test]
    fn auto_falls_back_to_first_option() {
        let options = vec!["single".to_string(), "double".to_string()];
        assert_eq!(
            auto_question_answer(&options, None).as_deref(),
            Some("single")
        );
        // A blank recommended value is ignored.
        assert_eq!(
            auto_question_answer(&options, Some("  ")).as_deref(),
            Some("single")
        );
    }

    #[test]
    fn auto_returns_none_without_any_hint() {
        assert_eq!(auto_question_answer(&[], None), None);
        assert_eq!(auto_question_answer(&[], Some("  ")), None);
        assert_eq!(
            auto_question_answer(&[], Some("free text")).as_deref(),
            Some("free text")
        );
    }
}
