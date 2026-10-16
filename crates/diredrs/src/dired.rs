//! Directory listings, in the shape Emacs' Dired produces.
//!
//! A listing is a header line (`  /dir:`), a `total` line, and one line per
//! file in the `ls -al` style. Like Dired, marks occupy the first column and
//! the classic `ls` fields start at column two.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use chrono::{DateTime, Local};

use crate::buffer::{Line, Span, Style};
use crate::theme::Theme;

/// Rows occupied by the directory header and the `total` line.
pub const HEADER_LINES: usize = 2;

/// How a file is marked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[allow(dead_code)] // `Marked`/`Flagged` are wired up with the mark commands.
pub enum Mark {
    #[default]
    None,
    /// `*`: marked for an operation.
    Marked,
    /// `D`: flagged for deletion.
    Flagged,
}

impl Mark {
    pub fn ch(self) -> char {
        match self {
            Mark::None => ' ',
            Mark::Marked => '*',
            Mark::Flagged => 'D',
        }
    }
}

/// How a listing is ordered. `s` toggles between the two, like
/// `dired-sort-toggle` flipping `ls`'s `-t` flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortKey {
    /// Alphabetically, case-insensitively (the `ls -l` default).
    #[default]
    Name,
    /// By modification time, newest first (`ls -lt`).
    Date,
}

impl SortKey {
    /// The other sort order.
    pub fn toggled(self) -> SortKey {
        match self {
            SortKey::Name => SortKey::Date,
            SortKey::Date => SortKey::Name,
        }
    }

    /// A short label for the echo area.
    pub fn label(self) -> &'static str {
        match self {
            SortKey::Name => "name",
            SortKey::Date => "date",
        }
    }
}

/// One entry in a directory listing.
#[derive(Debug, Clone)]
pub struct Entry {
    pub name: String,
    pub path: PathBuf,
    /// The `  perms links owner group size date ` text before the file name.
    /// The mark replaces its first character at render time.
    pub head: String,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub is_exec: bool,
    /// Where the link points, for `name -> target` rendering.
    pub target: Option<String>,
    /// 1K blocks, for the `total` line.
    pub blocks: u64,
    /// Modification time, used to sort by date.
    pub mtime: Option<SystemTime>,
    pub mark: Mark,
}

/// A directory listing.
#[derive(Debug, Clone)]
pub struct Dired {
    pub dir: PathBuf,
    pub entries: Vec<Entry>,
    /// Current ordering.
    pub sort: SortKey,
}

impl Dired {
    /// Read `dir`, building a listing. `show_hidden` lists dotfiles.
    pub fn read(dir: &Path, show_hidden: bool) -> io::Result<Dired> {
        let mut entries: Vec<Entry> = Vec::new();
        let mut read = Vec::new();
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if !show_hidden && name.starts_with('.') {
                continue;
            }
            let meta = entry.metadata()?;
            read.push((name, entry.path(), meta));
        }

        let links_width = 2;
        let size_width = read
            .iter()
            .map(|(_, _, m)| format!("{}", m.len()).len())
            .max()
            .unwrap_or(1);

        for (name, path, meta) in read {
            let file_type = meta.file_type();
            let is_symlink = file_type.is_symlink();
            let is_dir = file_type.is_dir();
            let is_exec = exec_bit(&meta, &name);
            let target = if is_symlink {
                fs::read_link(&path).ok().map(|p| p.display().to_string())
            } else {
                None
            };
            let head = format!(
                "  {} {:>lw$} {} {} {:>sw$} {} ",
                perm_string(&meta),
                nlink(&meta),
                owner(&meta),
                group(&meta),
                meta.len(),
                format_mtime(meta.modified().ok()),
                lw = links_width,
                sw = size_width,
            );
            entries.push(Entry {
                name,
                path,
                head,
                is_dir,
                is_symlink,
                is_exec,
                target,
                blocks: meta.len().div_ceil(1024),
                mtime: meta.modified().ok(),
                mark: Mark::None,
            });
        }

        let mut dired = Dired {
            dir: dir.to_path_buf(),
            entries,
            sort: SortKey::default(),
        };
        dired.resort();
        Ok(dired)
    }

    /// Change the ordering and re-sort in place (keeping marks and metadata).
    pub fn set_sort(&mut self, sort: SortKey) {
        self.sort = sort;
        self.resort();
    }

    /// Order `entries` according to `sort`. Name is case-insensitive
    /// ascending; date is newest first, ties broken by name.
    fn resort(&mut self) {
        match self.sort {
            SortKey::Name => self.entries.sort_by_key(|e| e.name.to_lowercase()),
            SortKey::Date => self.entries.sort_by(|a, b| {
                b.mtime
                    .cmp(&a.mtime)
                    .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            }),
        }
    }

    /// Render the listing as styled lines (header, total, then one per entry).
    pub fn render(&self, theme: &Theme) -> Vec<Line> {
        let mut lines = Vec::with_capacity(self.entries.len() + HEADER_LINES);
        lines.push(Line::styled(
            format!("  {}:", self.dir.display()),
            theme.header,
        ));
        let total: u64 = self.entries.iter().map(|e| e.blocks).sum();
        lines.push(Line::styled(format!("  total {total}"), theme.header));
        for i in 0..self.entries.len() {
            lines.push(self.render_line(i, theme));
        }
        lines
    }

    /// Render a single entry's line, including its mark and name face.
    pub fn render_line(&self, i: usize, theme: &Theme) -> Line {
        let entry = &self.entries[i];
        let mut chars: Vec<char> = entry.head.chars().collect();
        if let Some(first) = chars.first_mut() {
            *first = entry.mark.ch();
        }
        let head: String = chars.into_iter().collect();
        let name_style = self.name_style(entry, theme);
        let mut spans = vec![
            Span::new(head, Style::default()),
            Span::new(entry.name.clone(), name_style),
        ];
        if let Some(target) = &entry.target {
            spans.push(Span::new(format!(" -> {target}"), Style::default()));
        }
        Line::from_spans(spans)
    }

    /// The buffer row for entry `i`.
    pub fn row_of(&self, i: usize) -> usize {
        HEADER_LINES + i
    }

    fn name_style(&self, entry: &Entry, theme: &Theme) -> Style {
        let mut style = if entry.is_symlink {
            theme.symlink
        } else if entry.is_dir {
            theme.dir
        } else if entry.is_exec {
            theme.exec
        } else {
            theme.file
        };
        match entry.mark {
            Mark::None => {}
            Mark::Marked => style = theme.marked,
            Mark::Flagged => style = theme.flagged,
        }
        style
    }

    /// The entry index for a buffer `row`, if that row is a file line.
    pub fn entry_index_at(&self, row: usize) -> Option<usize> {
        if row < HEADER_LINES {
            return None;
        }
        let i = row - HEADER_LINES;
        (i < self.entries.len()).then_some(i)
    }

    /// The character column where a file line's name begins.
    pub fn filename_col(&self, row: usize) -> Option<usize> {
        let i = self.entry_index_at(row)?;
        Some(self.entries[i].head.chars().count())
    }

    /// The row of the first file line, if the listing is non-empty.
    pub fn first_file_row(&self) -> Option<usize> {
        (!self.entries.is_empty()).then_some(HEADER_LINES)
    }

    /// Total number of buffer rows this listing renders to.
    pub fn len(&self) -> usize {
        self.entries.len() + HEADER_LINES
    }
}

fn format_mtime(time: Option<SystemTime>) -> String {
    let Some(time) = time else {
        return "            ".to_string();
    };
    let dt: DateTime<Local> = time.into();
    let now = Local::now();
    let old = now.signed_duration_since(dt).num_days().abs() > 180;
    if old {
        dt.format("%b %e  %Y").to_string()
    } else {
        dt.format("%b %e %H:%M").to_string()
    }
}

#[cfg(unix)]
fn perm_string(meta: &fs::Metadata) -> String {
    use std::os::unix::fs::MetadataExt;
    let ft = meta.file_type();
    let kind = if ft.is_dir() {
        'd'
    } else if ft.is_symlink() {
        'l'
    } else if ft.is_fifo() {
        'p'
    } else if ft.is_socket() {
        's'
    } else {
        '-'
    };
    let mode = meta.mode();
    let rwx = |bits: u32| {
        let mut s = String::with_capacity(3);
        s.push(if bits & 4 != 0 { 'r' } else { '-' });
        s.push(if bits & 2 != 0 { 'w' } else { '-' });
        s.push(if bits & 1 != 0 { 'x' } else { '-' });
        s
    };
    let mut out = String::with_capacity(10);
    out.push(kind);
    out.push_str(&rwx((mode >> 6) & 7));
    out.push_str(&rwx((mode >> 3) & 7));
    out.push_str(&rwx(mode & 7));
    out
}

#[cfg(unix)]
fn exec_bit(meta: &fs::Metadata, _name: &str) -> bool {
    use std::os::unix::fs::MetadataExt;
    meta.file_type().is_file() && meta.mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn exec_bit(_meta: &fs::Metadata, name: &str) -> bool {
    let lower = name.to_lowercase();
    [".exe", ".bat", ".cmd", ".com", ".ps1"]
        .iter()
        .any(|ext| lower.ends_with(ext))
}

#[cfg(unix)]
fn nlink(meta: &fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    meta.nlink()
}

#[cfg(unix)]
fn owner(meta: &fs::Metadata) -> String {
    use std::os::unix::fs::MetadataExt;
    meta.uid().to_string()
}

#[cfg(unix)]
fn group(meta: &fs::Metadata) -> String {
    use std::os::unix::fs::MetadataExt;
    meta.gid().to_string()
}

#[cfg(not(unix))]
fn perm_string(meta: &fs::Metadata) -> String {
    let ft = meta.file_type();
    let kind = if ft.is_dir() { 'd' } else { '-' };
    let readonly = meta.permissions().readonly();
    let perms = if ft.is_dir() {
        "rwxr-xr-x"
    } else if readonly {
        "r--r--r--"
    } else {
        "rw-r--r--"
    };
    format!("{kind}{perms}")
}

#[cfg(not(unix))]
fn nlink(_meta: &fs::Metadata) -> u64 {
    1
}

#[cfg(not(unix))]
fn owner(_meta: &fs::Metadata) -> String {
    "-".to_string()
}

#[cfg(not(unix))]
fn group(_meta: &fs::Metadata) -> String {
    "-".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "diredrs-test-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn reads_and_renders_listing() {
        let dir = temp_dir("listing");
        fs::write(dir.join("alpha.txt"), b"hello").unwrap();
        fs::create_dir(dir.join("sub")).unwrap();

        let d = Dired::read(&dir, true).unwrap();
        assert_eq!(d.entries.len(), 2);
        // Sorted by name: alpha.txt, sub.
        assert_eq!(d.entries[0].name, "alpha.txt");
        assert_eq!(d.entries[1].name, "sub");
        assert!(d.entries[1].is_dir);

        let theme = Theme::for_background(crate::theme::Background::Dark);
        let lines = d.render(&theme);
        assert_eq!(lines.len(), HEADER_LINES + 2);
        assert!(lines[0].text().ends_with(':'));
        assert!(lines[1].text().starts_with("  total "));
        assert!(lines[2].text().contains("alpha.txt"));
        // The name begins right after the head (permissions at column 2).
        assert_eq!(
            d.filename_col(HEADER_LINES).unwrap(),
            d.entries[0].head.len()
        );

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn hidden_files_can_be_filtered() {
        let dir = temp_dir("hidden");
        fs::write(dir.join(".secret"), b"x").unwrap();
        fs::write(dir.join("visible"), b"x").unwrap();

        let all = Dired::read(&dir, true).unwrap();
        assert_eq!(all.entries.len(), 2);
        let shown = Dired::read(&dir, false).unwrap();
        assert_eq!(shown.entries.len(), 1);
        assert_eq!(shown.entries[0].name, "visible");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn sorts_by_name_and_date() {
        let dir = temp_dir("sortmode");
        fs::write(dir.join("b.txt"), b"x").unwrap();
        fs::write(dir.join("a.txt"), b"x").unwrap();
        fs::write(dir.join("c.txt"), b"x").unwrap();

        let mut d = Dired::read(&dir, true).unwrap();
        assert_eq!(d.sort, SortKey::Name);

        let t = |secs: u64| SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs);
        for e in &mut d.entries {
            e.mtime = Some(match e.name.as_str() {
                "a.txt" => t(1000),
                "b.txt" => t(2000),
                _ => t(3000),
            });
        }

        let names =
            |d: &Dired| -> Vec<String> { d.entries.iter().map(|e| e.name.clone()).collect() };

        // Date order is newest first.
        d.set_sort(SortKey::Date);
        assert_eq!(names(&d), ["c.txt", "b.txt", "a.txt"]);
        // Toggling back restores the alphabetical order.
        d.set_sort(SortKey::Name);
        assert_eq!(names(&d), ["a.txt", "b.txt", "c.txt"]);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn entry_index_and_header_rows() {
        let dir = temp_dir("rows");
        fs::write(dir.join("one"), b"x").unwrap();
        let d = Dired::read(&dir, true).unwrap();
        assert_eq!(d.entry_index_at(0), None);
        assert_eq!(d.entry_index_at(1), None);
        assert_eq!(d.entry_index_at(2), Some(0));
        assert_eq!(d.first_file_row(), Some(2));
    }
}
