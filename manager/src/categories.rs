//! IDM's default categories and the rule that files a download under one by extension.

use std::path::Path;

use crate::model::Category;

pub const GENERAL: &str = "General";

/// The default categories, each in a subfolder of the downloads folder.
pub fn defaults(downloads: &Path) -> Vec<Category> {
    let cat = |name: &str, exts: &str| Category {
        name: name.to_owned(),
        extensions: exts.split_whitespace().map(str::to_owned).collect(),
        dir: downloads.join(name),
    };
    vec![
        cat("Compressed", "zip rar r0* r1* arj gz sit sitx sea ace bz2 7z"),
        cat("Documents", "doc pdf ppt pps docx pptx"),
        cat("Music", "mp3 wav wma mpa ram ra aac aif m4a tsa"),
        cat("Programs", "exe msi"),
        cat("Video", "avi mpg mpe mpeg asf wmv mov qt rm mp4 flv m4v webm ogv ogg mkv ts tsv"),
        Category { name: GENERAL.to_owned(), extensions: Vec::new(), dir: downloads.to_owned() },
    ]
}

/// The first category whose extensions match the file name, else General.
pub fn for_file<'a>(categories: &'a [Category], file_name: &str) -> Option<&'a Category> {
    let ext = Path::new(file_name).extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase);
    categories
        .iter()
        .find(|c| ext.as_ref().is_some_and(|ext| c.extensions.iter().any(|pattern| wildcard(pattern, ext))))
        .or_else(|| categories.iter().find(|c| c.name == GENERAL))
}

/// `*` matches any run of characters, `?` one character.
fn wildcard(pattern: &str, text: &str) -> bool {
    match pattern.chars().next() {
        None => text.is_empty(),
        Some('*') => (0..=text.len()).filter(|&i| text.is_char_boundary(i)).any(|i| wildcard(&pattern[1..], &text[i..])),
        Some(p) => {
            let mut t = text.chars();
            match t.next() {
                Some(c) if p == '?' || p == c => wildcard(&pattern[p.len_utf8()..], t.as_str()),
                _ => false,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_go_to_their_category() {
        let cats = defaults(Path::new("D"));
        let name = |f: &str| for_file(&cats, f).map(|c| c.name.as_str());
        assert_eq!(name("setup.EXE"), Some("Programs"));
        assert_eq!(name("movie.part1.mkv"), Some("Video"));
        assert_eq!(name("archive.r01"), Some("Compressed"));
        assert_eq!(name("notes.txt"), Some("General"));
        assert_eq!(name("README"), Some("General"));
        assert_eq!(for_file(&cats, "a.zip").unwrap().dir, Path::new("D").join("Compressed"));
    }

    #[test]
    fn wildcards() {
        assert!(wildcard("r0*", "r01"));
        assert!(wildcard("r0*", "r0"));
        assert!(!wildcard("r0*", "rar"));
        assert!(wildcard("m?v", "m4v"));
        assert!(!wildcard("zip", "zipx"));
    }
}
