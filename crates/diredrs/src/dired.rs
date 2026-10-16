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
use crate::disk;
use crate::theme::Theme;

/// Rows occupied by the directory header line.
pub const HEADER_LINES: usize = 1;

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
    /// By size, largest first (`ls -lS`).
    Size,
}

impl SortKey {
    /// The next order in the `s` cycle (name -> date -> size -> name).
    pub fn next(self) -> SortKey {
        match self {
            SortKey::Name => SortKey::Date,
            SortKey::Date => SortKey::Size,
            SortKey::Size => SortKey::Name,
        }
    }

    /// A short label for the mode line and echo area.
    pub fn label(self) -> &'static str {
        match self {
            SortKey::Name => "name",
            SortKey::Date => "date",
            SortKey::Size => "size",
        }
    }
}

/// One entry in a directory listing.
#[derive(Debug, Clone)]
pub struct Entry {
    pub name: String,
    pub path: PathBuf,
    /// Permissions string, e.g. `drwxr-xr-x`.
    perm: String,
    /// Hard-link count.
    links: u64,
    owner: String,
    group: String,
    /// Size in bytes.
    size: u64,
    /// Preformatted modification time, e.g. `Oct 16 09:40`.
    date: String,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub is_exec: bool,
    /// Where the link points, for `name -> target` rendering.
    pub target: Option<String>,
    /// Modification time, used to sort by date.
    pub mtime: Option<SystemTime>,
    pub mark: Mark,
}

impl Entry {
    /// The `  perms links owner group size date ` text before the file name.
    /// The mark replaces its first character at render time.
    fn head(&self, human: bool, links_width: usize, size_width: usize) -> String {
        format!(
            "  {} {:>lw$} {} {} {:>sw$} {} ",
            self.perm,
            self.links,
            self.owner,
            self.group,
            format_size(self.size, human),
            self.date,
            lw = links_width,
            sw = size_width,
        )
    }
}

/// A directory listing.
#[derive(Debug, Clone)]
pub struct Dired {
    pub dir: PathBuf,
    pub entries: Vec<Entry>,
    /// Current ordering.
    pub sort: SortKey,
    /// Whether sizes are shown human-readably (`ls -h`).
    human_sizes: bool,
    links_width: usize,
    size_width: usize,
    /// Free space on the directory's filesystem, for the header line.
    free_space: Option<u64>,
}

impl Dired {
    /// Read `dir`, building a listing. `show_hidden` lists dotfiles and
    /// `human_sizes` formats sizes as K/M/G (`ls -h`).
    pub fn read(dir: &Path, show_hidden: bool, human_sizes: bool) -> io::Result<Dired> {
        let mut entries: Vec<Entry> = Vec::new();
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if !show_hidden && name.starts_with('.') {
                continue;
            }
            let path = entry.path();
            let meta = entry.metadata()?;
            let file_type = meta.file_type();
            let is_symlink = file_type.is_symlink();
            let is_dir = file_type.is_dir();
            let is_exec = exec_bit(&meta, &name);
            let target = if is_symlink {
                fs::read_link(&path).ok().map(|p| p.display().to_string())
            } else {
                None
            };
            let modified = meta.modified().ok();
            entries.push(Entry {
                name,
                path,
                perm: perm_string(&meta),
                links: nlink(&meta),
                owner: owner(&meta),
                group: group(&meta),
                size: meta.len(),
                date: format_mtime(modified),
                is_dir,
                is_symlink,
                is_exec,
                target,
                mtime: modified,
                mark: Mark::None,
            });
        }

        let mut dired = Dired {
            dir: dir.to_path_buf(),
            entries,
            sort: SortKey::default(),
            human_sizes,
            links_width: 2,
            size_width: 1,
            free_space: disk::available_space(dir),
        };
        dired.recompute_widths();
        dired.resort();
        Ok(dired)
    }

    /// Recompute the column widths from the entries and the current size mode.
    fn recompute_widths(&mut self) {
        self.links_width = self
            .entries
            .iter()
            .map(|e| e.links.to_string().len())
            .max()
            .unwrap_or(1)
            .max(2);
        self.size_width = self
            .entries
            .iter()
            .map(|e| format_size(e.size, self.human_sizes).chars().count())
            .max()
            .unwrap_or(1);
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
            SortKey::Size => self.entries.sort_by(|a, b| {
                b.size
                    .cmp(&a.size)
                    .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            }),
        }
    }

    /// Render the listing as styled lines (header, total, then one per entry).
    pub fn render(&self, theme: &Theme) -> Vec<Line> {
        let mut lines = Vec::with_capacity(self.entries.len() + HEADER_LINES);
        // Like Emacs' default (`dired-free-space` = `first`), the free space
        // rides on the directory line and there is no separate `total` line.
        let header = match self.free_space {
            Some(free) => format!("  {}: ({} available)", self.dir.display(), format_iec(free)),
            None => format!("  {}:", self.dir.display()),
        };
        lines.push(Line::styled(header, theme.header));
        for i in 0..self.entries.len() {
            lines.push(self.render_line(i, theme));
        }
        lines
    }

    /// Render a single entry's line, including its mark and name face.
    pub fn render_line(&self, i: usize, theme: &Theme) -> Line {
        let entry = &self.entries[i];
        let mut chars: Vec<char> = entry
            .head(self.human_sizes, self.links_width, self.size_width)
            .chars()
            .collect();
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
        let e = &self.entries[i];
        Some(
            e.head(self.human_sizes, self.links_width, self.size_width)
                .chars()
                .count(),
        )
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

/// Format a byte count for the size column. With `human`, use GNU `ls -h`'s
/// style: counts below 1K are exact, larger values use K/M/G/T with at most
/// three significant digits (`4.0K`, `234K`, `1.5M`).
fn format_size(bytes: u64, human: bool) -> String {
    if !human {
        return bytes.to_string();
    }
    const UNITS: [&str; 7] = ["", "K", "M", "G", "T", "P", "E"];
    if bytes < 1024 {
        return bytes.to_string();
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if value < 10.0 {
        format!("{value:.1}{}", UNITS[unit])
    } else {
        format!("{value:.0}{}", UNITS[unit])
    }
}

/// Format a byte count the way Emacs shows free disk space (an IEC unit with a
/// space and at most one decimal place): `512 B`, `1 KiB`, `1.5 GiB`,
/// `658 GiB`.
fn format_iec(bytes: u64) -> String {
    const UNITS: [&str; 7] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    let frac = value.fract();
    if value < 10.0 && (0.05..0.95).contains(&frac) {
        format!("{value:.1} {}", UNITS[unit])
    } else {
        format!("{value:.0} {}", UNITS[unit])
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

        let d = Dired::read(&dir, true, true).unwrap();
        assert_eq!(d.entries.len(), 2);
        // Sorted by name: alpha.txt, sub.
        assert_eq!(d.entries[0].name, "alpha.txt");
        assert_eq!(d.entries[1].name, "sub");
        assert!(d.entries[1].is_dir);

        let theme = Theme::for_background(crate::theme::Background::Dark);
        let lines = d.render(&theme);
        assert_eq!(lines.len(), HEADER_LINES + 2);
        // Header carries the directory; the file lines follow directly (no
        // `total` line).
        assert!(lines[0].text().contains(&dir.display().to_string()));
        assert!(lines[1].text().contains("alpha.txt"));
        assert!(lines[2].text().contains("sub"));
        // The name begins right after the head (permissions at column 2).
        assert_eq!(
            d.filename_col(HEADER_LINES).unwrap(),
            d.entries[0]
                .head(d.human_sizes, d.links_width, d.size_width)
                .len()
        );

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn hidden_files_can_be_filtered() {
        let dir = temp_dir("hidden");
        fs::write(dir.join(".secret"), b"x").unwrap();
        fs::write(dir.join("visible"), b"x").unwrap();

        let all = Dired::read(&dir, true, true).unwrap();
        assert_eq!(all.entries.len(), 2);
        let shown = Dired::read(&dir, false, true).unwrap();
        assert_eq!(shown.entries.len(), 1);
        assert_eq!(shown.entries[0].name, "visible");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn sorts_by_name_date_and_size() {
        let dir = temp_dir("sortmode");
        // Distinct sizes: b=2, a=3, c=4 bytes.
        fs::write(dir.join("b.txt"), b"bb").unwrap();
        fs::write(dir.join("a.txt"), b"aaa").unwrap();
        fs::write(dir.join("c.txt"), b"cccc").unwrap();

        let mut d = Dired::read(&dir, true, true).unwrap();
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

        assert_eq!(names(&d), ["a.txt", "b.txt", "c.txt"]); // by name
        d.set_sort(SortKey::Date);
        assert_eq!(names(&d), ["c.txt", "b.txt", "a.txt"]); // newest first
        d.set_sort(SortKey::Size);
        assert_eq!(names(&d), ["c.txt", "a.txt", "b.txt"]); // largest first
        d.set_sort(SortKey::Name);
        assert_eq!(names(&d), ["a.txt", "b.txt", "c.txt"]);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn sort_key_cycles_name_date_size() {
        assert_eq!(SortKey::Name.next(), SortKey::Date);
        assert_eq!(SortKey::Date.next(), SortKey::Size);
        assert_eq!(SortKey::Size.next(), SortKey::Name);
    }

    #[test]
    fn entry_index_and_header_rows() {
        let dir = temp_dir("rows");
        fs::write(dir.join("one"), b"x").unwrap();
        let d = Dired::read(&dir, true, true).unwrap();
        assert_eq!(d.entry_index_at(0), None);
        assert_eq!(d.entry_index_at(1), Some(0));
        assert_eq!(d.entry_index_at(2), None);
        assert_eq!(d.first_file_row(), Some(1));
    }

    #[test]
    fn header_shows_free_space() {
        let dir = temp_dir("freespace");
        fs::write(dir.join("f"), b"x").unwrap();
        let d = Dired::read(&dir, true, true).unwrap();
        let theme = Theme::for_background(crate::theme::Background::Dark);
        let lines = d.render(&theme);
        // Header plus one file line, no `total` line.
        assert_eq!(lines.len(), HEADER_LINES + 1);
        // Availability is environment-dependent; when known it is shown.
        if d.free_space.is_some() {
            let first = lines[0].text();
            assert!(first.contains(&format!("{}:", dir.display())));
            assert!(first.contains(" available)"), "{first}");
        }
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn format_iec_units() {
        assert_eq!(format_iec(512), "512 B");
        assert_eq!(format_iec(1024), "1 KiB");
        assert_eq!(format_iec(1587), "1.5 KiB");
        assert_eq!(format_iec(1024 * 1024 * 1024), "1 GiB");
        assert_eq!(format_iec(658 * 1024 * 1024 * 1024), "658 GiB");
    }

    #[test]
    fn format_size_human_readable() {
        assert_eq!(format_size(0, true), "0");
        assert_eq!(format_size(512, true), "512");
        assert_eq!(format_size(1024, true), "1.0K");
        assert_eq!(format_size(1536, true), "1.5K");
        assert_eq!(format_size(10 * 1024, true), "10K");
        assert_eq!(format_size(1024 * 1024, true), "1.0M");
        assert_eq!(format_size(2_500_000, true), "2.4M");
        // Exact bytes when human-readable is off.
        assert_eq!(format_size(1536, false), "1536");
    }

    #[test]
    fn human_sizes_in_listing() {
        let dir = temp_dir("humansize");
        fs::write(dir.join("big.bin"), vec![0u8; 2048]).unwrap();
        let theme = Theme::for_background(crate::theme::Background::Dark);

        let human = Dired::read(&dir, true, true).unwrap();
        assert!(human.human_sizes);
        assert!(human.render_line(0, &theme).text().contains("2.0K"));

        let exact = Dired::read(&dir, true, false).unwrap();
        assert!(!exact.human_sizes);
        assert!(exact.render_line(0, &theme).text().contains("2048"));

        fs::remove_dir_all(&dir).ok();
    }
}
