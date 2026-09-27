## Test-first (TDD)
Build every feature this way when `validate_tests` is available:

1. **Write the tests FIRST**, in the test block the file already uses (`#[cfg(test)] mod tests` with `use super::*;` for Rust; the `describe`/`it` block for JS/TS). Tests must be COMPREHENSIVE and exercise real behaviour — the happy path, the edge cases and the failure modes the feature must handle — never a vacuous assertion. Declare each test on the plan step it belongs to with `self_set_requirement_tests` (its `name`, `file` and `line`), so it shows under the step in the plan as a requirement test.
2. **Choose the right test type**, cheapest that proves the behaviour:
   - **unit** — one function/module in isolation: cheapest, least coverage; the backbone of the suite.
   - **integration** — a few parts together (a real dependency, a database, a module boundary): middle cost, middle coverage.
   - **functional** — the feature end to end as a user meets it: most expensive, most coverage; use sparingly.
   Follow the TEST PYRAMID: many unit tests, fewer integration tests, fewest functional tests. Never invert it — a suite that is mostly functional is slow and brittle.
3. Call `validate_tests` with `feature` (what it must do, in one or two sentences) and `tests` (the test code you wrote). Jev scores how well the tests cover the feature.
4. **NOT ENOUGH**: add the missing cases (prefer more unit tests at the base of the pyramid), then call `validate_tests` again. Do NOT delegate or write the implementation yet.
5. **ACCEPTED**: delegate the IMPLEMENTATION as a step, and make the tests the spec — tell the delegate to write the SMALLEST amount of code that makes the accepted tests pass, and that the tests must pass first and must NOT be weakened, skipped or deleted.
6. **Green**: refactor — delegate it or do it yourself — keeping the tests passing. Refactoring is ESSENTIAL, not optional: remove duplication, dead code and accidental complexity until what is left is the least code that passes the tests. Never refactor before the tests are green; the accepted tests are the safety net that makes it safe.
7. Never write the implementation before the tests are accepted. A delegate that returns having changed or deleted the tests has not finished: the original tests must pass unchanged.
