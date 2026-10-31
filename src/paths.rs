//! Path rules that differ by OS, written as plain functions so every OS can test them.
//!
//! Windows reads far more into a name than Unix does: drive letters, UNC and device paths, alternate
//! data streams, reserved device names and trailing dots all reach somewhere other than the folder a
//! person served. `resolve` applies [`windows_violation`] on Windows only; on Unix those names are
//! ordinary and stay allowed.

use std::path::Path;

/// `FILE_ATTRIBUTE_REPARSE_POINT`: junctions, symlinks and mount points all carry it.
#[cfg_attr(
    not(windows),
    allow(dead_code, reason = "used by the Windows build; tested everywhere")
)]
const REPARSE_POINT: u32 = 0x400;

/// Whether file attributes mark a reparse point, which a scan must not walk through.
#[cfg_attr(
    not(windows),
    allow(dead_code, reason = "used by the Windows build; tested everywhere")
)]
pub(crate) const fn reparse_flag(attributes: u32) -> bool {
    attributes & REPARSE_POINT != 0
}

const RESERVED: [&str; 6] = ["con", "prn", "aux", "nul", "conin$", "conout$"];

fn reserved_device(segment: &str) -> bool {
    // `CON.txt` and `con .txt` are the device too: only the part before the first dot counts.
    let stem = segment
        .split('.')
        .next()
        .unwrap_or_default()
        .trim_end_matches(' ');
    let stem = stem.to_lowercase();
    if RESERVED.contains(&stem.as_str()) {
        return true;
    }
    // COM0..COM9, LPT0..LPT9 and their superscript forms.
    let letters: Vec<char> = stem.chars().collect();
    match letters.as_slice() {
        [a, b, c, last] => {
            matches!((a, b, c), ('c', 'o', 'm') | ('l', 'p', 't'))
                && (last.is_ascii_digit() || matches!(last, '¹' | '²' | '³'))
        }
        _ => false,
    }
}

/// Why a relative path is unsafe on Windows, or `None`. Backslash and slash both separate.
pub(crate) fn windows_violation(relative: &str) -> Option<&'static str> {
    if relative.starts_with(['/', '\\']) {
        return Some("rooted, UNC or device path");
    }
    if relative.contains(':') {
        return Some("drive letter or alternate data stream");
    }
    if relative
        .chars()
        .any(|c| c.is_control() || "<>\"|?*".contains(c))
    {
        return Some("character Windows does not allow in a name");
    }
    for segment in relative.split(['/', '\\']) {
        match segment {
            "." | "" => {}
            ".." => return Some("parent folder"),
            _ if segment.ends_with(['.', ' ']) => return Some("trailing dot or space"),
            _ if reserved_device(segment) => return Some("reserved device name"),
            _ => {}
        }
    }
    None
}

/// `path` is `root` or inside it. With `fold`, letter case is ignored, as on Windows volumes.
pub(crate) fn contained(root: &Path, path: &Path, fold: bool) -> bool {
    if !fold {
        return path.starts_with(root);
    }
    let parts = |p: &Path| -> Vec<String> {
        p.components()
            .map(|c| c.as_os_str().to_string_lossy().to_lowercase())
            .collect()
    };
    parts(path).starts_with(&parts(root))
}

/// A stable 64-bit FNV-1a digest of the normalized folder path, as 16 hex digits. Windows has no
/// device and inode to cheaply read without `unsafe`, so a replaced folder is not detected there.
#[cfg_attr(
    not(windows),
    allow(dead_code, reason = "used by the Windows build; tested everywhere")
)]
pub(crate) fn path_digest(folder: &Path) -> String {
    let normal = folder.to_string_lossy().replace('\\', "/").to_lowercase();
    let normal = normal.trim_end_matches('/');
    let hash = normal
        .bytes()
        .fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
        });
    format!("w-{hash:016x}")
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{contained, path_digest, reparse_flag, windows_violation};

    #[test]
    fn windows_rejects_every_way_out_of_the_folder() {
        for bad in [
            "C:x",
            "C:\\x",
            "c:/x",
            "\\\\server\\share\\x",
            "//server/share",
            "\\\\?\\C:\\x",
            "\\\\.\\pipe\\x",
            "\\x",
            "/x",
            "..\\x",
            "a\\..\\..\\x",
            "a/..\\x",
            "a\\b/../../x",
            "..",
            "a.txt:stream",
            "a.txt:stream:$DATA",
            "CON",
            "con",
            "NUL.txt",
            "d/Aux.tar.gz",
            "COM1",
            "lpt9.log",
            "COM1 .txt",
            "com0.txt",
            "d\\PRN",
            "CONIN$",
            "a.",
            "a ",
            "d/b. /c",
            "x/a..",
            "a<b",
            "a|b",
            "a?b",
            "a*b",
            "a\"b",
            "a\tb",
            "COM¹",
        ] {
            assert!(
                windows_violation(bad).is_some(),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn windows_allows_ordinary_names() {
        for good in [
            "a.txt",
            "d/a b.dxf",
            "d\\e\\f.txt",
            "./a",
            "console.txt",
            "com10",
            "comx",
            "nul2",
            "auxiliary",
            ".hidden",
            "Đồ án/bản vẽ.dxf",
            "a/b//c",
        ] {
            assert_eq!(windows_violation(good), None, "{good:?}");
        }
    }

    #[test]
    fn confinement_can_ignore_case() {
        let (root, inside) = (Path::new("/Work/Proj"), Path::new("/work/proj/Sub/a.txt"));
        assert!(contained(root, inside, true));
        assert!(!contained(root, inside, false));
        assert!(contained(root, root, false));
        assert!(!contained(root, Path::new("/work/project/a"), true));
        assert!(!contained(root, Path::new("/work"), true));
    }

    #[test]
    fn a_folder_digest_is_stable_and_fits_the_server_limits() {
        let id = path_digest(Path::new("C:\\Users\\Me\\Work\\"));
        assert_eq!(id, path_digest(Path::new("c:/users/me/work")));
        assert_ne!(id, path_digest(Path::new("c:/users/me/other")));
        assert!((8..=64).contains(&id.len()), "{id}");
    }

    #[test]
    fn the_reparse_attribute_is_recognized() {
        assert!(reparse_flag(0x410));
        assert!(!reparse_flag(0x10));
    }
}
