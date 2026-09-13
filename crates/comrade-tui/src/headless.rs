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

async fn confirm_on_stdin(title: &str) -> Result<String> {
    let answer = read_line(&format!("[comrade] {title} [y/N] ")).await?;
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
