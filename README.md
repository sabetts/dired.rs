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

# Model Used

I'm using Deepseek v4.1 flash.
