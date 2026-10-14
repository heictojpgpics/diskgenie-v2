//! `FileCategory` — the 9 buckets, colors and extension table of
//! `BuildPrompt` §4 (transcribed EXACTLY; do not "improve" it).
//!
//! Classification works off the lowercased ASCII extension of a UTF-16 name
//! WITHOUT allocating: a stack buffer holds the extension while compared
//! against the table (doc 04 §4 "stack variable-length for small names").

use serde::{Deserialize, Serialize};

/// The nine file categories. Bit values are the arena's 4-bit field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum FileCategory {
    /// Video files.
    Video = 0,
    /// Audio files.
    Audio = 1,
    /// Image files.
    Image = 2,
    /// Documents.
    Document = 3,
    /// Developer files.
    Developer = 4,
    /// Archives.
    Archive = 5,
    /// Applications.
    Apps = 6,
    /// System files.
    System = 7,
    /// Everything else.
    Other = 8,
}

impl FileCategory {
    /// Number of buckets (array lengths elsewhere depend on this).
    pub const COUNT: usize = 9;

    /// Decode from the 4-bit arena field; out-of-range decodes to `Other`.
    #[must_use]
    pub fn from_bits(bits: u8) -> Self {
        match bits {
            0 => Self::Video,
            1 => Self::Audio,
            2 => Self::Image,
            3 => Self::Document,
            4 => Self::Developer,
            5 => Self::Archive,
            6 => Self::Apps,
            7 => Self::System,
            _ => Self::Other,
        }
    }

    /// The 4-bit encoding stored in `Node::flags`.
    #[must_use]
    pub const fn as_bits(self) -> u8 {
        self as u8
    }

    /// Category color (hex, theme-independent pastels, spec §4/§14).
    #[must_use]
    pub const fn color(self) -> u32 {
        // Packed 0xRRGGBB (alpha added at cell-buffer packing time).
        match self {
            Self::Video => 0xFDA4AF,
            Self::Audio => 0xC4B5FD,
            Self::Image => 0x7DD3FC,
            Self::Document => 0xFCD34D,
            Self::Developer => 0x86EFAC,
            Self::Archive => 0xFDBA74,
            Self::Apps => 0x93C5FD,
            Self::System => 0x5EEAD4,
            Self::Other => 0xCBD5E1,
        }
    }

    /// Category color as `#RRGGBB` (CSS / legends).
    #[must_use]
    pub fn color_hex(self) -> String {
        format!("#{:06X}", self.color())
    }

    /// Icon tag for the UI (icon set resolves these names).
    #[must_use]
    pub const fn icon(self) -> &'static str {
        match self {
            Self::Video => "video",
            Self::Audio => "audio",
            Self::Image => "image",
            Self::Document => "document",
            Self::Developer => "code",
            Self::Archive => "archive",
            Self::Apps => "app",
            Self::System => "system",
            Self::Other => "file",
        }
    }

    /// Human label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Video => "Video",
            Self::Audio => "Audio",
            Self::Image => "Image",
            Self::Document => "Document",
            Self::Developer => "Developer",
            Self::Archive => "Archive",
            Self::Apps => "Apps",
            Self::System => "System",
            Self::Other => "Other",
        }
    }

    /// Classify from a UTF-16 name slice (no allocation; ASCII extensions).
    #[must_use]
    pub fn from_name(name: &[u16]) -> Self {
        // Find the last '.' outside of any trailing run; no dot → Other.
        let Some(dot) = name.iter().rposition(|&c| c == u16::from(b'.')) else {
            return Self::Other;
        };
        let ext = &name[dot + 1..];
        // Cap the comparison length (longest table entry is 11: "msixbundle").
        if ext.is_empty() || ext.len() > 16 {
            return Self::Other;
        }
        // Stack buffer with lowercased ASCII extension (doc 04 §4).
        let mut buf = [0u8; 16];
        for (i, &c) in ext.iter().enumerate() {
            let b = if c <= 0x7F {
                c as u8
            } else {
                // Non-ASCII "extension" can never match the table.
                return Self::Other;
            };
            buf[i] = b.to_ascii_lowercase();
        }
        Self::from_ext(&buf[..ext.len()])
    }

    /// Classify from a lowercased ASCII extension slice.
    ///
    /// The body is the spec §4 extension table verbatim — a long flat
    /// match is the honest shape for a lookup table (splitting it would
    /// only scatter the spec across files).
    #[allow(clippy::too_many_lines)]
    #[must_use]
    pub fn from_ext(ext: &[u8]) -> Self {
        // Category-per-extension table (spec §4). The macro compares the
        // stack buffer against literal extensions without allocating.
        macro_rules! any_of {
            ($cat:expr, $($e:literal),*) => { $( if ext == $e.as_bytes() { return $cat; } )* };
        }
        any_of!(
            Self::Video,
            "mp4",
            "mov",
            "m4v",
            "mkv",
            "avi",
            "wmv",
            "flv",
            "webm",
            "mpg",
            "mpeg",
            "3gp",
            "mts",
            "m2ts",
            "vob"
        );
        any_of!(
            Self::Audio,
            "mp3",
            "wav",
            "aac",
            "m4a",
            "flac",
            "ogg",
            "aiff",
            "aif",
            "alac",
            "wma",
            "opus",
            "mid",
            "midi",
            "caf",
            "m4b"
        );
        any_of!(
            Self::Image,
            "jpg",
            "jpeg",
            "png",
            "gif",
            "heic",
            "heif",
            "tiff",
            "tif",
            "bmp",
            "webp",
            "svg",
            "raw",
            "cr2",
            "cr3",
            "nef",
            "arw",
            "dng",
            "psd",
            "ai",
            "sketch",
            "fig",
            "ico",
            "icns",
            "exr"
        );
        any_of!(
            Self::Document,
            "pdf",
            "doc",
            "docx",
            "xls",
            "xlsx",
            "ppt",
            "pptx",
            "key",
            "pages",
            "numbers",
            "txt",
            "rtf",
            "md",
            "csv",
            "epub",
            "odt",
            "ods",
            "odp",
            "html",
            "htm",
            "tex",
            "pst",
            "ost"
        );
        any_of!(
            Self::Developer,
            "c",
            "cc",
            "cpp",
            "cxx",
            "h",
            "hpp",
            "cs",
            "csproj",
            "vcxproj",
            "sln",
            "rs",
            "go",
            "java",
            "kt",
            "class",
            "jar",
            "py",
            "pyc",
            "rb",
            "js",
            "mjs",
            "cjs",
            "ts",
            "tsx",
            "jsx",
            "json",
            "php",
            "vue",
            "svelte",
            "dart",
            "lua",
            "r",
            "ipynb",
            "swift",
            "m",
            "mm",
            "o",
            "obj",
            "lib",
            "a",
            "so",
            "dll",
            "pdb",
            "exp",
            "node",
            "wasm",
            "map",
            "lock",
            "yml",
            "yaml",
            "toml",
            "xml",
            "sh",
            "ps1",
            "bat",
            "cmd",
            "css",
            "scss",
            "less",
            "gradle",
            "sql",
            "tsbuildinfo",
            "d"
        );
        any_of!(
            Self::Archive,
            "zip",
            "gz",
            "tgz",
            "tar",
            "bz2",
            "xz",
            "7z",
            "rar",
            "dmg",
            "iso",
            "pkg",
            "xip",
            "zst",
            "lz4",
            "cab"
        );
        any_of!(
            Self::Apps,
            "exe",
            "msi",
            "msix",
            "msixbundle",
            "appx",
            "appxbundle"
        );
        any_of!(
            Self::System,
            "log",
            "db",
            "sqlite",
            "sqlite3",
            "sqlite-wal",
            "sqlite-shm",
            "db-wal",
            "db-shm",
            "wal",
            "shm",
            "cache",
            "dat",
            "ldb",
            "idx",
            "pack",
            "vhd",
            "vhdx",
            "vmdk",
            "img",
            "asar",
            "etl",
            "evtx",
            "dmp",
            "sys",
            "mui",
            "cat",
            "wim",
            "esd",
            "bak",
            "tmp"
        );
        Self::Other
    }

    /// Final category for a node considering the apps-root rule (spec §4):
    /// items inside an apps root are `Apps`, EXCEPT `.dll` which stays
    /// `Developer` ("the Apps extension list wins over the roots rule only
    /// for the classification of .dll" — i.e. the Developer extension
    /// classification wins for .dll and never the other way round).
    #[must_use]
    pub fn resolve(name: &[u16], in_apps_root: bool) -> Self {
        let by_ext = Self::from_name(name);
        if in_apps_root && by_ext != Self::Developer {
            Self::Apps
        } else {
            by_ext
        }
    }
}

/// The apps roots resolved once at scan start (spec §4):
/// True when `name` is a protected item at the given context (spec §4):
/// drive-root `Windows` / `Windows.old` / `pagefile.sys` / `hiberfil.sys` /
/// `swapfile.sys` / `System Volume Information` / `$Recycle.Bin`, and
/// `Program Files\WindowsApps` (any drive).
///
/// `parent_is_drive_root` marks depth-1 items; `parent_is_program_files`
/// marks items directly under a `Program Files*` folder.
#[must_use]
pub fn is_protected_name(
    name: &[u16],
    parent_is_drive_root: bool,
    parent_is_program_files: bool,
) -> bool {
    fn eq_ascii(utf16: &[u16], ascii: &str) -> bool {
        if utf16.len() != ascii.len() {
            return false;
        }
        utf16
            .iter()
            .zip(ascii.bytes())
            .all(|(&c, b)| lower_ascii_u16(c) == u16::from(b.to_ascii_lowercase()))
    }
    if parent_is_drive_root
        && (eq_ascii(name, "Windows")
            || eq_ascii(name, "Windows.old")
            || eq_ascii(name, "pagefile.sys")
            || eq_ascii(name, "hiberfil.sys")
            || eq_ascii(name, "swapfile.sys")
            || eq_ascii(name, "System Volume Information")
            || eq_ascii(name, "$Recycle.Bin"))
    {
        return true;
    }
    if parent_is_program_files && eq_ascii(name, "WindowsApps") {
        return true;
    }
    false
}

/// `A`–`Z` (ASCII) → `a`–`z`; everything else unchanged. `u16` has no
/// `to_ascii_lowercase` inherent method, so this is the manual twin.
#[inline]
fn lower_ascii_u16(c: u16) -> u16 {
    if (0x41..=0x5A).contains(&c) {
        c + 0x20
    } else {
        c
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u16s(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    #[test]
    fn table_samples() {
        // One per bucket + boundaries the spec calls out.
        assert_eq!(
            FileCategory::from_name(&u16s("movie.MP4")),
            FileCategory::Video
        );
        assert_eq!(
            FileCategory::from_name(&u16s("track.flac")),
            FileCategory::Audio
        );
        assert_eq!(
            FileCategory::from_name(&u16s("photo.jpeg")),
            FileCategory::Image
        );
        assert_eq!(
            FileCategory::from_name(&u16s("notes.txt")),
            FileCategory::Document
        );
        assert_eq!(
            FileCategory::from_name(&u16s("main.rs")),
            FileCategory::Developer
        );
        assert_eq!(
            FileCategory::from_name(&u16s("bundle.7z")),
            FileCategory::Archive
        );
        assert_eq!(
            FileCategory::from_name(&u16s("setup.EXE")),
            FileCategory::Apps
        );
        assert_eq!(
            FileCategory::from_name(&u16s("data.sqlite-wal")),
            FileCategory::System
        );
        assert_eq!(
            FileCategory::from_name(&u16s("data.dat")),
            FileCategory::System
        );
        assert_eq!(
            FileCategory::from_name(&u16s("weird.xyzzy")),
            FileCategory::Other
        );
        assert_eq!(FileCategory::from_name(&u16s("noext")), FileCategory::Other);
        assert_eq!(
            FileCategory::from_name(&u16s(".gitignore")),
            FileCategory::Other
        );
        assert_eq!(
            FileCategory::from_name(&u16s("ünïcode.mp4")),
            FileCategory::Video
        );
    }

    #[test]
    fn dll_rule_in_apps_roots() {
        // .dll stays Developer inside apps roots; .txt becomes Apps there.
        assert_eq!(
            FileCategory::resolve(&u16s("lib.dll"), true),
            FileCategory::Developer
        );
        assert_eq!(
            FileCategory::resolve(&u16s("lib.dll"), false),
            FileCategory::Developer
        );
        assert_eq!(
            FileCategory::resolve(&u16s("readme.txt"), true),
            FileCategory::Apps
        );
        assert_eq!(
            FileCategory::resolve(&u16s("app.exe"), true),
            FileCategory::Apps
        );
        assert_eq!(
            FileCategory::resolve(&u16s("app.exe"), false),
            FileCategory::Apps
        );
        assert_eq!(
            FileCategory::resolve(&u16s("readme.txt"), false),
            FileCategory::Document
        );
    }

    #[test]
    fn protected_names() {
        assert!(is_protected_name(&u16s("Windows"), true, false));
        assert!(is_protected_name(&u16s("windows.old"), true, false));
        assert!(is_protected_name(&u16s("PAGEFILE.SYS"), true, false));
        assert!(is_protected_name(&u16s("hiberfil.sys"), true, false));
        assert!(is_protected_name(&u16s("swapfile.sys"), true, false));
        // System volume shadow store + the bin itself: never stageable.
        assert!(is_protected_name(
            &u16s("System Volume Information"),
            true,
            false
        ));
        assert!(is_protected_name(&u16s("$Recycle.Bin"), true, false));
        // Not at drive root → a user folder named like these stays usable.
        assert!(!is_protected_name(&u16s("$Recycle.Bin"), false, false));
        assert!(!is_protected_name(&u16s("Windows"), false, false)); // not at drive root
        assert!(is_protected_name(&u16s("WindowsApps"), false, true)); // inside Program Files
        assert!(!is_protected_name(&u16s("WindowsApps"), false, false));
        assert!(!is_protected_name(&u16s("Users"), true, false));
    }

    #[test]
    fn colors_and_labels() {
        assert_eq!(FileCategory::Video.color_hex(), "#FDA4AF");
        assert_eq!(FileCategory::System.color_hex(), "#5EEAD4");
        assert_eq!(FileCategory::Other.color_hex(), "#CBD5E1");
        assert_eq!(FileCategory::from_bits(8), FileCategory::Other);
        assert_eq!(FileCategory::from_bits(255), FileCategory::Other);
        assert_eq!(FileCategory::COUNT, 9);
    }
}
