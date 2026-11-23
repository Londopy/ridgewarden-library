//! The offline map viewer: MapLibre GL JS drawing a held PMTiles map with the Protomaps
//! basemap style, and every file it needs (scripts, style, fonts, sprites) served from
//! here, so it works with no internet. The files are the published ones, unchanged and
//! built into the binary (about 2.6 MB); THIRD_PARTY.md lists where each came from, its
//! license and its hash.
//!
//! Built in rather than installed beside the binary: the viewer then can't be missing or
//! out of step with the app, packaging stays one file, and a few MB is small next to the
//! maps a site holds.

use std::fmt::Write as _;
use std::sync::{Arc, LazyLock};

use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use ridgewarden_client::util::esc;
use serde::Deserialize;

use crate::app::App;
use crate::content::Kind;
use crate::mirror;

pub struct Asset {
    pub path: &'static str,
    pub ctype: &'static str,
    pub body: &'static [u8],
}

macro_rules! asset {
    ($path:literal, $ctype:expr) => {
        Asset {
            path: $path,
            ctype: $ctype,
            body: include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/assets/viewer/",
                $path
            )),
        }
    };
}

const JS: &str = "text/javascript; charset=utf-8";
const CSS: &str = "text/css; charset=utf-8";
const JSON: &str = "application/json";
const PNG: &str = "image/png";
const PBF: &str = "application/x-protobuf";

pub const ASSETS: &[Asset] = &[
    asset!("maplibre-gl.mjs", JS),
    asset!("maplibre-gl-worker.mjs", JS),
    asset!("maplibre-gl.css", CSS),
    asset!("pmtiles.js", JS),
    asset!("basemaps.js", JS),
    asset!("sprites/light.json", JSON),
    asset!("sprites/light.png", PNG),
    asset!("sprites/light@2x.json", JSON),
    asset!("sprites/light@2x.png", PNG),
    asset!("fonts/Noto Sans Regular/0-255.pbf", PBF),
    asset!("fonts/Noto Sans Regular/256-511.pbf", PBF),
    asset!("fonts/Noto Sans Regular/8192-8447.pbf", PBF),
    asset!("fonts/Noto Sans Medium/0-255.pbf", PBF),
    asset!("fonts/Noto Sans Medium/256-511.pbf", PBF),
    asset!("fonts/Noto Sans Medium/8192-8447.pbf", PBF),
    asset!("fonts/Noto Sans Italic/0-255.pbf", PBF),
    asset!("fonts/Noto Sans Italic/256-511.pbf", PBF),
    asset!("fonts/Noto Sans Italic/8192-8447.pbf", PBF),
];

pub const THIRD_PARTY: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/THIRD_PARTY.md"));

/// Where the files are served: a hash of their contents, so browsers can keep them for
/// good, an update that changes them gets fresh copies, and one that doesn't costs no
/// download over RF.
pub static PREFIX: LazyLock<String> = LazyLock::new(|| {
    let mut all = String::new();
    for a in ASSETS {
        all.push_str(a.path);
        all.push_str(&crate::sha256::hex_of(a.body));
    }
    format!("/viewer/{}", &crate::sha256::hex_of(all.as_bytes())[..12])
});

/// The glyph files held here, as "font/range.pbf", so the page asks only for those.
pub fn glyph_files() -> String {
    let files: Vec<&str> = ASSETS
        .iter()
        .filter_map(|a| a.path.strip_prefix("fonts/"))
        .collect();
    serde_json::json!(files).to_string()
}

/// The file a viewer URL path names. A glyph request names a font stack ("Noto Sans
/// Regular,Noto Sans Devanagari Regular v1"); the first font of it held here serves it,
/// and ranges outside Latin are missing on purpose.
pub fn find(path: &str) -> Option<&'static Asset> {
    if let Some(rest) = path.strip_prefix("fonts/") {
        let (stack, range) = rest.rsplit_once('/')?;
        return stack.split(',').find_map(|font| {
            let want = format!("fonts/{}/{range}", font.trim());
            ASSETS.iter().find(|a| a.path == want)
        });
    }
    ASSETS.iter().find(|a| a.path == path)
}

pub async fn asset(Path((tag, path)): Path<(String, String)>) -> Response {
    let current = *PREFIX == format!("/viewer/{tag}");
    let Some(a) = current.then(|| find(&path)).flatten() else {
        // a glyph range not held is asked for on every visit unless the miss is cached too
        let keep = if current && path.starts_with("fonts/") {
            "public, max-age=86400"
        } else {
            "no-store"
        };
        return (
            StatusCode::NOT_FOUND,
            [(header::CACHE_CONTROL, keep)],
            "no such viewer file",
        )
            .into_response();
    };
    (
        [
            (header::CONTENT_TYPE, a.ctype),
            (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        a.body,
    )
        .into_response()
}

pub async fn third_party() -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        THIRD_PARTY,
    )
        .into_response()
}

#[derive(Deserialize)]
pub struct Which {
    map: Option<String>,
}

/// Opens the map at the file's own bounds, refuses files the style can't draw, and says
/// plainly what went wrong. Static text: everything it needs comes from data attributes
/// on the map element. An async function rather than top-level await, so an older browser
/// that can't run it at least shows the error.
pub const SCRIPT: &str = r#"
(async () => {
  const el = document.getElementById("map");
  const status = document.getElementById("map-status");
  status.textContent = "Loading the map...";
  try {
    const base = location.origin + el.dataset.assets;
    const ml = await import(base + "/maplibre-gl.mjs");
    const maplibregl = ml.default ?? ml;
    const file = new URL(el.dataset.file, location.href).href;
    const protocol = new pmtiles.Protocol();
    maplibregl.addProtocol("pmtiles", protocol.tile);
    // a glyph range not held here is never asked for: MapLibre draws it with the device's fonts
    const glyphs = new Set(JSON.parse(el.dataset.glyphs));
    maplibregl.addProtocol("glyphs", async (params, abort) => {
      const [stack, range] = decodeURIComponent(params.url.slice("glyphs://".length)).split("/");
      const font = stack.split(",").map((f) => f.trim()).find((f) => glyphs.has(f + "/" + range));
      if (!font) throw new Error("not held here, drawn with this device's fonts");
      const r = await fetch(base + "/fonts/" + encodeURIComponent(font) + "/" + range,
        { signal: abort.signal });
      if (!r.ok) throw new Error("glyphs answered " + r.status);
      return { data: await r.arrayBuffer() };
    });
    const archive = new pmtiles.PMTiles(file);
    protocol.add(archive);
    const h = await archive.getHeader();
    const meta = await archive.getMetadata();
    const layers = (meta && meta.vector_layers || []).map((l) => l.id);
    if (h.tileType !== 1 || !layers.includes("roads") || !layers.includes("places")) {
      status.textContent = "This file is not a Protomaps basemap, so this viewer's style can't draw it. " +
        "Open " + el.dataset.file + " with a PMTiles reader that has its own style.";
      return;
    }
    el.style.height = "70vh";
    el.style.minHeight = "300px";
    const view = h.minLon < h.maxLon && h.minLat < h.maxLat
      ? { bounds: [[h.minLon, h.minLat], [h.maxLon, h.maxLat]] }
      : { center: [h.centerLon, h.centerLat], zoom: h.centerZoom };
    const map = new maplibregl.Map(Object.assign({
      container: el,
      style: {
        version: 8,
        glyphs: "glyphs://{fontstack}/{range}.pbf",
        sprite: base + "/sprites/light",
        sources: {
          protomaps: {
            type: "vector",
            url: "pmtiles://" + file,
            attribution: "Map data &copy; OpenStreetMap contributors, style &copy; Protomaps",
          },
        },
        layers: basemaps.layers("protomaps", basemaps.namedFlavor("light"), { lang: "en" }),
      },
    }, view));
    map.addControl(new maplibregl.NavigationControl());
    let failed = false;
    map.on("error", (e) => {
      failed = true;
      status.textContent = "Part of the map could not be loaded (" +
        (e.error && e.error.message || e) + "). Reload the page to try again.";
    });
    map.on("load", () => { if (!failed) status.textContent = ""; });
  } catch (e) {
    el.style.height = "";
    el.style.minHeight = "";
    status.textContent = "The map could not be shown: " + e.message + " The map file is at " +
      el.dataset.file + " for any PMTiles reader.";
  }
})();
"#;

pub(crate) async fn page(
    State(app): State<Arc<App>>,
    Query(q): Query<Which>,
) -> Result<Response, crate::web::Fail> {
    let maps: Vec<mirror::Held> = mirror::held(&app.db())?
        .into_iter()
        .filter(|h| h.kind == Kind::Pmtiles)
        .collect();
    let chosen = match &q.map {
        Some(id) => maps.iter().find(|h| &h.item == id),
        None => maps.first(),
    };
    let mut b = String::from("<h1>Map viewer</h1>");
    let Some(h) = chosen else {
        if q.map.is_some() {
            b.push_str("<p>No such map at this site. <a href=\"/maps\">Maps held here</a></p>");
            let html = crate::web::page(&app, "/maps", "Map viewer", &b, false).await;
            return Ok((StatusCode::NOT_FOUND, html).into_response());
        }
        b.push_str(
            "<p class=mute>No map files have reached this site yet. The catalog shows what is \
             published and what is still being copied.</p>",
        );
        return Ok(crate::web::page(&app, "/maps", "Map viewer", &b, false)
            .await
            .into_response());
    };
    if maps.len() > 1 {
        b.push_str("<p class=small>Maps here: ");
        for (i, m) in maps.iter().enumerate() {
            let sep = if i > 0 { " &middot; " } else { "" };
            if m.item == h.item {
                let _ = write!(b, "{sep}<b>{}</b>", esc(&m.item));
            } else {
                let _ = write!(
                    b,
                    "{sep}<a href=\"/viewer?map={}\">{}</a>",
                    esc(&m.item),
                    esc(&m.item)
                );
            }
        }
        b.push_str("</p>");
    }
    let item = crate::catalog::published(&app.db(), &h.item, &h.root)?;
    let file = format!("/files/{}.{}", h.item, h.ext);
    let prefix = PREFIX.as_str();
    let _ = write!(
        b,
        "<noscript><p class=banner>The map viewer needs JavaScript (and WebGL 2). The map file \
         is at <code>{}</code> for any PMTiles reader; see <a href=\"/maps\">Maps</a>.</p></noscript>\
         <link rel=stylesheet href=\"{prefix}/maplibre-gl.css\">\
         <div id=map data-file=\"{}\" data-assets=\"{prefix}\" data-glyphs=\"{}\" \
         style=\"width:100%;border-radius:6px\"></div>\
         <p id=map-status class=\"mute small\" style=\"overflow-wrap:anywhere\"></p>\
         <p class=small>{} version {}. {} Basemap style &copy; Protomaps. Viewer: MapLibre GL \
         JS, pmtiles and Protomaps basemaps, served from this site \
         (<a href=\"/about/third-party\">licenses</a>).</p>\
         <script src=\"{prefix}/pmtiles.js\"></script>\
         <script src=\"{prefix}/basemaps.js\"></script>\
         <script type=module>{SCRIPT}</script>",
        esc(&file),
        esc(&file),
        esc(&glyph_files()),
        esc(item.as_ref().map_or(&h.item, |i| &i.title)),
        esc(&h.version),
        crate::web::credit_html(item.as_ref())
    );
    Ok(crate::web::page(&app, "/maps", "Map viewer", &b, false)
        .await
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_file_matches_the_hash_third_party_records() {
        for a in ASSETS {
            let sum = crate::sha256::hex_of(a.body);
            let line = THIRD_PARTY
                .lines()
                .find(|l| l.contains(&format!("`{}`", a.path)))
                .unwrap_or_else(|| panic!("{} is not listed in THIRD_PARTY.md", a.path));
            assert!(
                line.contains(&sum),
                "{} has SHA-256 {sum}, not what THIRD_PARTY.md says",
                a.path
            );
            assert!(
                line.contains(&format!("| {} |", a.body.len())),
                "{} size",
                a.path
            );
        }
    }

    #[test]
    fn content_types_fit_the_files() {
        for a in ASSETS {
            let want = match a.path.rsplit_once('.').map(|x| x.1) {
                Some("mjs" | "js") => "text/javascript; charset=utf-8",
                Some("css") => "text/css; charset=utf-8",
                Some("json") => "application/json",
                Some("png") => "image/png",
                Some("pbf") => "application/x-protobuf",
                other => panic!("unexpected file type {other:?}"),
            };
            assert_eq!(a.ctype, want, "{}", a.path);
        }
    }

    #[test]
    fn glyph_requests_use_the_first_font_held() {
        let a = find("fonts/Noto Sans Medium,Noto Sans Devanagari Regular v1/0-255.pbf").unwrap();
        assert_eq!(a.path, "fonts/Noto Sans Medium/0-255.pbf");
        let a = find("fonts/Some Other Font,Noto Sans Italic/256-511.pbf").unwrap();
        assert_eq!(a.path, "fonts/Noto Sans Italic/256-511.pbf");
        assert!(
            find("fonts/Noto Sans Regular/2304-2559.pbf").is_none(),
            "not a Latin range"
        );
        assert!(find("fonts/../../THIRD_PARTY.md").is_none());
        assert!(find("../Cargo.toml").is_none());
    }

    #[test]
    fn everything_the_style_asks_for_is_held() {
        // the script points MapLibre at these
        for part in [
            "\"glyphs://{fontstack}/{range}.pbf\"",
            "base + \"/fonts/\" + encodeURIComponent(font) + \"/\" + range",
            "\"/sprites/light\"",
            "\"/maplibre-gl.mjs\"",
            "namedFlavor(\"light\")",
            "bounds:",
        ] {
            assert!(SCRIPT.contains(part), "{part}");
        }
        for path in [
            "sprites/light.json",
            "sprites/light.png",
            "sprites/light@2x.json",
            "sprites/light@2x.png",
        ] {
            assert!(find(path).is_some(), "{path}");
        }
        // the page asks only for the glyph files it is told are here, so none is missing
        let listed: Vec<String> = serde_json::from_str(&glyph_files()).unwrap();
        assert_eq!(listed.len(), 9, "{listed:?}");
        for f in &listed {
            assert!(find(&format!("fonts/{f}")).is_some(), "{f}");
        }
        assert!(!listed.iter().any(|f| f.ends_with("/1024-1279.pbf")));
        // every font the pinned basemap style names is here, for its Latin ranges
        let style = std::str::from_utf8(find("basemaps.js").unwrap().body).unwrap();
        let mut fonts: Vec<&str> = style
            .match_indices("Noto Sans")
            .map(|(i, _)| {
                let rest = &style[i..];
                let end = rest.find(['"', '\'', '`', ',']).unwrap_or(rest.len());
                rest[..end].trim()
            })
            .collect();
        fonts.sort();
        fonts.dedup();
        assert!(fonts.len() >= 3, "{fonts:?}");
        for f in fonts.iter().filter(|f| !f.contains("Devanagari")) {
            for range in ["0-255", "256-511", "8192-8447"] {
                assert!(
                    find(&format!("fonts/{f}/{range}.pbf")).is_some(),
                    "{f} {range}"
                );
            }
        }
    }

    #[test]
    fn the_cache_path_is_a_short_content_hash() {
        assert!(PREFIX.starts_with("/viewer/") && PREFIX.len() == "/viewer/".len() + 12);
        assert!(PREFIX[8..].bytes().all(|b| b.is_ascii_hexdigit()));
    }
}
