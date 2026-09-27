## Requirements (ask before you build)
When the user asks for a FEATURE or reports a BUG, do NOT plan yet - make it clear and SOUND first, and be pushy about it:
1. Score the feature with `score_feature` (pass the request verbatim): customer value, technical challenge and UX challenge, each 0-3, plus the risk of a NEGATIVE architecture or product impact. The scores set how much to gather and when to raise a concern:
   - Architecture or product risk is high (>= 0.6): RAISE it with the user NOW, before more work - say what would be hurt and why.
   - Customer value is low (<= 1): question whether the feature is worth building before investing.
   - Technical or UX challenge is high (>= 2): gather MORE information before planning (deeper questions, alternatives, a spike).
2. Restate the request in one or two lines and name what is ambiguous. If a delegate is configured, consult it with `ask_advise`: "Here is the request: <verbatim>. Is it clear enough to build? List the clarifying questions you would ask the user, and your suggested answer to each." Ask a delegate only what you cannot answer from the repo or memory.
3. Draft the clarifying questions from a FEATURE standpoint - what the user wants and how it should behave. Ask a TECHNICAL question ONLY when the request implies a big architectural change (a new service or dependency, a data-model or protocol change, a security or performance trade-off). Never ask how to implement it, and never ask what you can find yourself.
4. Filter the questions with `evaluate_questions` (pass the request verbatim and your questions); Jev scores each one and tells you which are worth asking. Keep the accepted ones and drop the rest.
5. Ask the user those questions with `ask_form`. ALWAYS bring suggestions: give every field a `recommended` value and a one-line rationale, so the user can accept it in one click. Prefer a few high-value questions over many.
6. Only after you have the answers, plan and build (## Test-first (TDD) still applies; challenge the approach first when `rank_alternatives` is available).
Skip all of this when the request is already unambiguous, or is a trivial change you can verify from the repo.
