# 0009 - TUI emacs-style mode line: git + state + auto-mode
status: accepted
tags: comrade-tui, tui, status-bar, git, auto-approve, ratatui
summary: Bottom bar in comrade-tui now shows branch, +L/-L, +f/-f, IDLE/RUNNING and auto/ask; whole bar turns orange when auto-mode is on.

## Context
User asked for an emacs-like status bar in the comrade TUI. The old "footer" (right-aligned status text + RUNNING/IDLE + key legend) in draw() at crates/comrade-tui/src/tui.rs was replaced by a full-width mode line. Auto-mode = App.auto_accept (ctrl-space toggle, tui.rs handle_event) OR ctx_base.auto_approve (config --auto), decided in App::auto_mode_on().

## Decision
Mode line semantics (user-confirmed): +line/-line are total inserted/removed lines vs HEAD from `git diff --numstat` + `git diff --cached --numstat`; +files = newly added (staged A) or untracked (??) files, -files = deleted files (D in index or worktree) from `git status --porcelain`. Git data lives in GitBarInfo (branch, ins, del, added_files, deleted_files, repo) refreshed in the background: App::refresh_git() spawns fetch_git_bar(root) at most every 2 s (10 s when not a repo) via an mpsc channel; a select arm in run() calls App::on_git() on each snapshot. Colors: bar bg Color::Blue when asking, AUTO_BAR_BG = Rgb(203,106,15) orange when auto on; fg flips to black; per-count fg is muted to black on orange via the on_auto closure. Key legend is a const LEGEND right-aligned and truncated on narrow terminals.

## Consequences
Parsers status_file_counts() and numstat_totals() are pure and unit-tested in mod mode_bar_tests at the bottom of tui.rs. The mode line only refreshes git while the UI is drawing (event-driven), so an idle terminal can show stale git stats until the next keypress/agent event. Both crates/comrade-core/src/delegate.rs and crates/comrade-tool-session/src/lib.rs had unrelated uncommitted edits (tool-prompt wording) before this task; do not fold them into unrelated commits without checking.

