# Intro

This is a vibe-coded clone of dired in Rust.

# Usage

You run it in a terminal.

On boot it opens a `*scratch*` buffer and a Dired buffer for your home
directory. Moving around uses Emacs motions; visiting a file opens it
in a read-only buffer. I haven't fully decided what is out-of-scope. I
intend to keep adding until I stop noticing I'm not in emacs.

A few files get syntax highlighting. I'll probably add more.

I've diverged from dired in a few places where I thought it made sense.

It's a work-in-progress.

# Notes

On Windows Terminal, `Ctrl-V` is bound by the *terminal* to paste, so it
never reaches diredrs and the `C-v` page-down binding is unavailable.

# Model Used

I'm using Deepseek v4.1 flash.
