//! The kinds of content the library carries, and the names it gives their files.

use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// a Kiwix archive (Wikipedia and friends), served through kiwix-serve
    Zim,
    /// a Protomaps tile archive, served here over HTTP with Range requests
    Pmtiles,
    /// a document served agencies asked for: forms, plans, phone lists
    Doc,
}

/// File types a document may be. No HTML: a page served from here could run script.
pub const DOC_EXTS: &[&str] = &[
    "pdf", "txt", "csv", "md", "odt", "ods", "docx", "xlsx", "png", "jpg", "jpeg",
];

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Zim => "zim",
            Kind::Pmtiles => "pmtiles",
            Kind::Doc => "doc",
        }
    }

    pub fn parse(s: &str) -> Option<Kind> {
        match s {
            "zim" => Some(Kind::Zim),
            "pmtiles" => Some(Kind::Pmtiles),
            "doc" => Some(Kind::Doc),
            _ => None,
        }
    }

    pub fn word(self) -> &'static str {
        match self {
            Kind::Zim => "Reference (ZIM)",
            Kind::Pmtiles => "Map (PMTiles)",
            Kind::Doc => "Document",
        }
    }

    /// The extension a file of this kind is stored under. A document keeps its own, if it
    /// is one of the allowed types.
    pub fn ext_for(self, name: &str) -> Result<String> {
        match self {
            Kind::Zim => Ok("zim".into()),
            Kind::Pmtiles => Ok("pmtiles".into()),
            Kind::Doc => {
                let ext = name
                    .rsplit_once('.')
                    .map(|(_, e)| e.to_ascii_lowercase())
                    .unwrap_or_default();
                if !DOC_EXTS.contains(&ext.as_str()) {
                    bail!(
                        "{name} is not a document type this library serves ({})",
                        DOC_EXTS.join(", ")
                    );
                }
                Ok(ext)
            }
        }
    }

    /// Check the file starts the way this kind of file must, so an HTML error page or the
    /// wrong download is caught before it is published to every site.
    pub fn check(self, path: &Path, ext: &str) -> Result<()> {
        let mut head = vec![0u8; 8192];
        let n = std::fs::File::open(path)
            .with_context(|| format!("opening {}", path.display()))?
            .read(&mut head)?;
        let head = &head[..n];
        let ok = match (self, ext) {
            // ZIM magic number 72173914, little-endian
            (Kind::Zim, _) => head.starts_with(&[0x5a, 0x49, 0x4d, 0x04]),
            // PMTiles version 3 header
            (Kind::Pmtiles, _) => head.starts_with(b"PMTiles\x03"),
            (Kind::Doc, "pdf") => head.starts_with(b"%PDF-"),
            (Kind::Doc, "png") => head.starts_with(b"\x89PNG"),
            (Kind::Doc, "jpg" | "jpeg") => head.starts_with(&[0xff, 0xd8, 0xff]),
            (Kind::Doc, "odt" | "ods" | "docx" | "xlsx") => head.starts_with(b"PK\x03\x04"),
            // text: no NUL bytes and nothing that looks like a web page
            (Kind::Doc, _) => {
                n > 0 && !head.contains(&0) && !head.trim_ascii_start().starts_with(b"<")
            }
        };
        if !ok {
            bail!("{} is not a {ext} file", path.display());
        }
        Ok(())
    }
}

/// The Content-Type a file is served with.
pub fn content_type(ext: &str) -> &'static str {
    match ext {
        "pdf" => "application/pdf",
        "txt" | "md" => "text/plain; charset=utf-8",
        "csv" => "text/csv; charset=utf-8",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "odt" => "application/vnd.oasis.opendocument.text",
        "ods" => "application/vnd.oasis.opendocument.spreadsheet",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        _ => "application/octet-stream",
    }
}

/// Item ids end up in file names, blob names and URLs.
pub fn valid_item(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 40
        && s.as_bytes()[0].is_ascii_alphanumeric()
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Versions are dates like "2026-08" or "2026-11-18", sometimes with a letter.
pub fn valid_version(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 20
        && s.as_bytes()[0].is_ascii_digit()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
}

/// The name a copy is stored and served under. Built only from checked parts, never from a
/// file name another station sent; the root prefix keeps two builds of one day apart.
pub fn file_name(item: &str, version: &str, root: &str, ext: &str) -> String {
    format!("{item}_{version}_{}.{ext}", &root[..8])
}

/// Check a finished file: the right kind, and the expected SHA-256 if there is one.
/// Returns its SHA-256. Blocking, and minutes for a big file.
pub fn check_file(path: &Path, kind: Kind, ext: &str, sha256: Option<&str>) -> Result<String> {
    kind.check(path, ext)?;
    let got = crate::sha256::file_hex(path)?;
    if let Some(want) = sha256
        && want != got
    {
        bail!(
            "SHA-256 of {} is {got}, but the publisher says {want}",
            path.display()
        );
    }
    Ok(got)
}

/// The blob name other sites see in the core's blob list.
pub fn blob_name(item: &str, version: &str) -> String {
    format!("library/{item}/{version}")
}

/// The version in a Kiwix-style file name, "wikipedia_en_all_mini_2026-08.zim" -> "2026-08".
pub fn version_in_name(name: &str) -> Option<String> {
    let stem = name.rsplit_once('.').map_or(name, |(s, _)| s);
    let last = stem.rsplit('_').next()?;
    let b = last.as_bytes();
    let dated = b.len() >= 7
        && b[..4].iter().all(u8::is_ascii_digit)
        && b[4] == b'-'
        && b[5..7].iter().all(u8::is_ascii_digit);
    (dated && valid_version(last)).then(|| last.to_string())
}

/// "Wed, 18 Nov 2026 08:55:27 GMT" -> "2026-11-18".
pub fn http_date_day(s: &str) -> Option<String> {
    let mut w = s.split_whitespace().skip(1);
    let (day, mon, year) = (w.next()?, w.next()?, w.next()?);
    let m = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ]
    .iter()
    .position(|x| *x == mon)?
        + 1;
    let d: u32 = day.parse().ok()?;
    let y: u32 = year.parse().ok()?;
    (1..=31)
        .contains(&d)
        .then(|| format!("{y:04}-{m:02}-{d:02}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_from_kiwix_names() {
        assert_eq!(
            version_in_name("wikipedia_en_all_mini_2026-08.zim").as_deref(),
            Some("2026-08")
        );
        assert_eq!(
            version_in_name("wikipedia_en_100_2026-07a.zim").as_deref(),
            Some("2026-07a")
        );
        assert_eq!(version_in_name("wikipedia_en_all_mini.zim"), None);
        assert_eq!(version_in_name("region.pmtiles"), None);
    }

    #[test]
    fn http_dates() {
        assert_eq!(
            http_date_day("Wed, 18 Nov 2026 08:55:27 GMT").as_deref(),
            Some("2026-11-18")
        );
        assert_eq!(http_date_day("yesterday"), None);
    }

    #[test]
    fn names_stay_plain() {
        assert!(valid_item("wikipedia-en-mini"));
        for bad in ["", "../x", "A", "x/y", "-x", &"a".repeat(41)] {
            assert!(!valid_item(bad), "{bad}");
        }
        assert!(valid_version("2026-11-18") && !valid_version("../2026") && !valid_version(""));
        assert_eq!(
            file_name("maps", "2026-11-18", &"ab12".repeat(16), "pmtiles"),
            "maps_2026-11-18_ab12ab12.pmtiles"
        );
    }

    #[test]
    fn magic_numbers() {
        let d = tempfile::tempdir().unwrap();
        let zim = d.path().join("a.zim");
        std::fs::write(&zim, [0x5a, 0x49, 0x4d, 0x04, 6, 0, 1, 0]).unwrap();
        Kind::Zim.check(&zim, "zim").unwrap();
        assert!(Kind::Pmtiles.check(&zim, "pmtiles").is_err());
        let html = d.path().join("b.zim");
        std::fs::write(&html, b"<!doctype html>").unwrap();
        assert!(Kind::Zim.check(&html, "zim").is_err());
        let pm = d.path().join("c.pmtiles");
        std::fs::write(&pm, b"PMTiles\x03rest").unwrap();
        Kind::Pmtiles.check(&pm, "pmtiles").unwrap();
        let sum = crate::sha256::hex_of(b"PMTiles\x03rest");
        assert_eq!(
            check_file(&pm, Kind::Pmtiles, "pmtiles", Some(&sum)).unwrap(),
            sum
        );
        let err = check_file(&pm, Kind::Pmtiles, "pmtiles", Some(&"0".repeat(64))).unwrap_err();
        assert!(err.to_string().contains("publisher says"), "{err}");
    }

    #[test]
    fn documents_keep_their_type_and_html_is_refused() {
        assert_eq!(Kind::Doc.ext_for("Phone List.CSV").unwrap(), "csv");
        assert!(Kind::Doc.ext_for("plan.html").is_err());
        assert!(Kind::Doc.ext_for("noext").is_err());
        assert_eq!(Kind::Zim.ext_for("whatever.bin").unwrap(), "zim");
        let d = tempfile::tempdir().unwrap();
        let csv = d.path().join("a.csv");
        std::fs::write(&csv, "name,call,phone\nShelter desk,N0CALL,555-0100\n").unwrap();
        Kind::Doc.check(&csv, "csv").unwrap();
        std::fs::write(&csv, "<script>alert(1)</script>").unwrap();
        assert!(
            Kind::Doc.check(&csv, "csv").is_err(),
            "markup posing as text"
        );
        let pdf = d.path().join("a.pdf");
        std::fs::write(&pdf, "%PDF-1.7 rest").unwrap();
        Kind::Doc.check(&pdf, "pdf").unwrap();
        assert!(Kind::Doc.check(&pdf, "docx").is_err());
    }
}
