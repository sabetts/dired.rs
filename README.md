# Intro

In the spirit of clones that are fun to write, I present a vibe-coded
clone of dired in Rust. It mirrors the stack used by `cricr/`: a
ratatui + crossterm TUI with a shallow set of Emacs keybindings.

# Usage

You run it in a terminal.

On boot it opens a `*scratch*` buffer and a Dired buffer for your home
directory. Moving around uses Emacs motions; visiting a file opens it in
a read-only buffer (text files as text, binary files in a hex viewer
like `hexl-mode`).

It clones a shallow subset of Emacs' dired within that scope. I've
diverged in a few places where it made sense.

It's a work-in-progress.

# Notes

On Windows Terminal, `Ctrl-V` is bound by the *terminal* to paste, so it
never reaches diredrs and the `C-v` page-down binding is unavailable
there. Use the `PageDown` key instead, or free the chord by unbinding
`Ctrl-V` in Windows Terminal (Settings → Actions → delete the Paste
binding for `ctrl+v`; `Ctrl-Shift-V` still pastes):

    { "command": "unbound", "keys": "ctrl+v" }

Pasting into a read-only buffer (Dired, file views, the hex viewer) is
refused with "Text is read-only"; pasting into `*scratch*` or a prompt
inserts the text. A deliberate single-character paste on Windows is
indistinguishable from a keystroke.

# Model Used

I'm using Deepseek v4.1 flash.
