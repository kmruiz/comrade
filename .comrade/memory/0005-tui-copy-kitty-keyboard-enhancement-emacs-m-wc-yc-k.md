# 0005 - TUI copy: kitty keyboard-enhancement + Emacs M-w/C-y/C-k
status: accepted
date: 2026-09-09
tags: tui, keybindings, clipboard
summary: Copy relies on the kitty keyboard-enhancement protocol so Ctrl+Shift+C is distinguishable from Ctrl+C, and Emacs-style M-w/C-y/C-k cover terminals (GNOME Terminal/VTE) that swallow Ctrl+Shift+C.

## Context
Users reported that Ctrl+Shift+C does not copy the selected chat message to the clipboard. Two independent causes existed: (1) In legacy terminal mode Ctrl+Shift+C is byte-identical to Ctrl+C (0x03), so crossterm cannot report the SHIFT modifier and the app's handler at handle_event never fired — on terminals that forwarded the chord the app would even quit; (2) in GNOME Terminal and other VTE-based terminals Ctrl+Shift+C is reserved by the terminal itself for "copy terminal selection" and never reaches the app at all. Reported environment was GNOME Terminal (VTE 8.4) on X11.

## Decision
On Unix, push crossterm's PushKeyboardEnhancementFlags (kitty keyboard protocol: DISAMBIGUATE_ESCAPE_CODES | REPORT_EVENT_TYPES | REPORT_ALTERNATE_KEYS | REPORT_ALL_KEYS_AS_ESCAPE_CODES) at TUI start and pop it at teardown, and ignore KeyEventKind::Release in handle_event. In addition, bind Emacs-style copy/cut/paste on the prompt editor: M-w copies the prompt selection, else the chat block under the cursor; C-k cuts the prompt selection to the clipboard; C-y pastes the system clipboard at the cursor (new Editor::insert_str and Editor::cut_selection). Chat blocks remain read-only: M-w copies them, cut/paste apply only to the prompt. M-x copy shows "C-S-c / M-w".

## Rationale
The keyboard protocol is the only way a terminal application can distinguish Ctrl+Shift+C from Ctrl+C; terminals that do not support it ignore the request and keep legacy behaviour (so no regression). Because GNOME Terminal/VTE reserve Ctrl+Shift+C at the terminal level, no app-side change can ever receive that chord there — hence the M-w fallback which every terminal forwards. M-w/C-y/C-k match the editor's existing Emacs-style keybindings (M-x, M-<left/right>, C-p/C-n).

## Alternatives considered
(a) Change the copy binding away from Ctrl+Shift+C entirely — rejected: Ctrl+Shift+C is the muscle-memory binding for copy, and the protocol fix makes it work in supporting terminals. (b) Use arboard only without protocol — rejected: does not address the modifier-folded byte stream. (c) Full in-app kill ring with M-y rotation — rejected as overkill; the system clipboard via arboard is the single shared store.

## Scope
comrade-tui input handling and prompt Editor only; no config surface added. Deliberately not covered: paste/cut inside the Ctrl-S search bar or approval-dialog text fields, and kill-ring history.

## Impact
Terminals supporting the kitty keyboard protocol (kitty, WezTerm, foot, ghostty, recent VTE with env) now deliver Ctrl+Shift+C as a distinct key. Terminals that do not (older VTE, some tmux passthroughs) ignore the push and keep the old behaviour; there M-w is the reliable copy. REPORT_EVENT_TYPES introduces key-release events, hence the Release guard in handle_event. C-k with no prompt selection is a no-op (chat is read-only); M-w with no prompt selection still copies the chat block.

