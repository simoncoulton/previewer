// SPDX-License-Identifier: MIT

//! The list of images a window steps through: the siblings of the opened
//! file, a folder's contents, or the files given on the command line.

use std::{
    cmp::Ordering,
    fs,
    path::{Path, PathBuf},
};

/// Extensions offered when browsing a folder. Decoding sniffs the content, so
/// a mislabelled file still opens when it is chosen explicitly.
const EXTENSIONS: &[&str] = &[
    "png", "apng", "jpg", "jpeg", "jpe", "jfif", "gif", "webp", "bmp", "dib", "ico", "cur", "tif",
    "tiff", "svg", "svgz", "qoi", "tga", "pnm", "pbm", "pgm", "ppm", "pam", "hdr", "exr", "avif",
    "heic", "heif", "jxl",
];

pub fn is_image(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| EXTENSIONS.iter().any(|known| known.eq_ignore_ascii_case(e)))
}

#[derive(Default)]
pub struct Library {
    files: Vec<PathBuf>,
    index: usize,
}

impl Library {
    /// Builds the list for what the user opened.
    ///
    /// One file: its folder's images, positioned on that file. One folder: its
    /// images. Several paths: exactly those files, in the order given.
    pub fn open(paths: &[PathBuf]) -> Self {
        match paths {
            [] => Self::default(),
            [single] if single.is_dir() => Self {
                files: scan_dir(single),
                index: 0,
            },
            [single] => {
                let dir = single
                    .parent()
                    .filter(|p| !p.as_os_str().is_empty())
                    .unwrap_or_else(|| Path::new("."));
                let mut files = scan_dir(dir);
                let index = match files.iter().position(|f| same_file(f, single)) {
                    Some(index) => index,
                    None => {
                        // Opened explicitly but not recognised by extension.
                        files.push(single.clone());
                        files.sort_by(|a, b| natural_cmp(&file_name(a), &file_name(b)));
                        files.iter().position(|f| f == single).unwrap_or(0)
                    }
                };
                Self { files, index }
            }
            many => Self {
                files: many.iter().filter(|p| p.is_file()).cloned().collect(),
                index: 0,
            },
        }
    }

    pub fn files(&self) -> &[PathBuf] {
        &self.files
    }

    pub fn len(&self) -> usize {
        self.files.len()
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    pub fn index(&self) -> usize {
        self.index
    }

    pub fn current(&self) -> Option<&PathBuf> {
        self.files.get(self.index)
    }

    /// Moves to `index` if it exists; returns whether the position changed.
    pub fn select(&mut self, index: usize) -> bool {
        if index < self.files.len() && index != self.index {
            self.index = index;
            true
        } else {
            false
        }
    }

    pub fn next_index(&self) -> Option<usize> {
        (self.index + 1 < self.files.len()).then_some(self.index + 1)
    }

    pub fn previous_index(&self) -> Option<usize> {
        self.index.checked_sub(1)
    }

    pub fn last_index(&self) -> Option<usize> {
        self.files.len().checked_sub(1)
    }
}

fn same_file(a: &Path, b: &Path) -> bool {
    a == b
        || matches!(
            (fs::canonicalize(a), fs::canonicalize(b)),
            (Ok(a), Ok(b)) if a == b
        )
}

pub fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn scan_dir(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| is_image(path) && path.is_file())
        .filter(|path| {
            !path
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with('.'))
        })
        .collect();
    files.sort_by(|a, b| natural_cmp(&file_name(a), &file_name(b)));
    files
}

/// Case-insensitive ordering that reads digit runs as numbers, so `img2`
/// sorts before `img10`. Names that only differ in case or leading zeros
/// fall back to plain string order, which keeps the sort deterministic.
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    natural_cmp_loose(a, b).then_with(|| a.cmp(b))
}

fn natural_cmp_loose(a_text: &str, b_text: &str) -> Ordering {
    let (mut a, mut b) = (a_text.chars().peekable(), b_text.chars().peekable());
    loop {
        match (a.peek().copied(), b.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let take_number = |chars: &mut std::iter::Peekable<std::str::Chars>| {
                    let mut digits = String::new();
                    while let Some(&c) = chars.peek() {
                        if !c.is_ascii_digit() {
                            break;
                        }
                        digits.push(c);
                        chars.next();
                    }
                    digits
                };
                let (na, nb) = (take_number(&mut a), take_number(&mut b));
                let (ta, tb) = (na.trim_start_matches('0'), nb.trim_start_matches('0'));
                let order = ta.len().cmp(&tb.len()).then_with(|| ta.cmp(tb));
                if order != Ordering::Equal {
                    return order;
                }
            }
            (Some(x), Some(y)) => {
                let (lx, ly) = (x.to_lowercase().next(), y.to_lowercase().next());
                if lx != ly {
                    return lx.cmp(&ly);
                }
                a.next();
                b.next();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn natural_order_reads_numbers() {
        let mut names = vec!["img10.png", "img2.png", "Img1.png", "img02.png", "a.png"];
        names.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(
            names,
            ["a.png", "Img1.png", "img02.png", "img2.png", "img10.png"]
        );
    }

    #[test]
    fn natural_order_is_total_for_prefixes() {
        assert_eq!(natural_cmp("a", "ab"), Ordering::Less);
        assert_eq!(natural_cmp("ab", "a"), Ordering::Greater);
        assert_eq!(natural_cmp("a1", "a1"), Ordering::Equal);
    }

    #[test]
    fn recognises_extensions_case_insensitively() {
        assert!(is_image(Path::new("/x/photo.JPG")));
        assert!(is_image(Path::new("diagram.svg")));
        assert!(!is_image(Path::new("notes.txt")));
        assert!(!is_image(Path::new("noextension")));
    }

    fn make_dir(files: &[&str]) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering::Relaxed};
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "previewer-lib-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Relaxed)
        ));
        fs::create_dir_all(&dir).unwrap();
        for name in files {
            fs::write(dir.join(name), b"x").unwrap();
        }
        dir
    }

    #[test]
    fn opening_a_file_lists_its_siblings() {
        let dir = make_dir(&["b2.png", "b10.png", "b1.png", "notes.txt", ".hidden.png"]);
        let library = Library::open(&[dir.join("b2.png")]);
        let names: Vec<_> = library.files().iter().map(|p| file_name(p)).collect();
        assert_eq!(names, ["b1.png", "b2.png", "b10.png"]);
        assert_eq!(library.index(), 1);
        assert_eq!(library.current(), Some(&dir.join("b2.png")));
    }

    #[test]
    fn opening_an_unrecognised_file_still_shows_it() {
        let dir = make_dir(&["a.png", "mystery.dat"]);
        let library = Library::open(&[dir.join("mystery.dat")]);
        assert_eq!(library.len(), 2);
        assert_eq!(library.current(), Some(&dir.join("mystery.dat")));
    }

    #[test]
    fn opening_a_folder_starts_at_the_first_image() {
        let dir = make_dir(&["z.png", "a.jpg"]);
        let library = Library::open(std::slice::from_ref(&dir));
        assert_eq!(library.index(), 0);
        assert_eq!(library.current(), Some(&dir.join("a.jpg")));
    }

    #[test]
    fn several_files_keep_the_given_order() {
        let dir = make_dir(&["a.png", "b.png"]);
        let library = Library::open(&[dir.join("b.png"), dir.join("a.png"), dir.join("gone.png")]);
        let names: Vec<_> = library.files().iter().map(|p| file_name(p)).collect();
        assert_eq!(names, ["b.png", "a.png"]);
    }

    #[test]
    fn navigation_stays_in_bounds() {
        let dir = make_dir(&["a.png", "b.png", "c.png"]);
        let mut library = Library::open(&[dir.join("a.png")]);
        assert_eq!(library.previous_index(), None);
        assert_eq!(library.next_index(), Some(1));
        assert!(library.select(2));
        assert_eq!(library.next_index(), None);
        assert!(
            !library.select(2),
            "selecting the current item is not a change"
        );
        assert!(!library.select(9));
        assert_eq!(library.last_index(), Some(2));
    }

    #[test]
    fn empty_library_is_inert() {
        let library = Library::open(&[]);
        assert!(library.is_empty());
        assert_eq!(library.current(), None);
        assert_eq!(library.next_index(), None);
        assert_eq!(library.previous_index(), None);
        assert_eq!(library.last_index(), None);
    }
}
