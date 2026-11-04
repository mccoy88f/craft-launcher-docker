//! Each installed app as a PWA of its own (Settings → Installable apps, off by default): its page
//! gets a web app manifest with the app's name, colour and icon, so a browser offers to install
//! it on its own, besides the launcher. Nothing of the app is changed on disk: the link to the
//! manifest is added to its `index.html` as it is served, and an app whose page already has a
//! manifest of its own keeps that one.
//!
//! The files live under [`DIR`] in the app's folder (`/apps/<app>/__artcraft-pwa/…`), a name no
//! app build uses. Every address is relative, so the server works under any path.

use artcraft_launcher_core::catalog::CraftApp;

/// The folder, inside an app's address, that the manifest and icon are served from.
pub const DIR: &str = "__artcraft-pwa";

/// The largest app page the link is added to; a bigger one is served as it is.
pub const PAGE_MAX: u64 = 1 << 20;

/// The app's 256 px icon, copied unmodified from its own repo (`assets/app-icons/256/`).
pub fn icon(app: &str) -> Option<&'static [u8]> {
    Some(match app {
        "photocraft" => include_bytes!("../../../assets/app-icons/256/photocraft.png"),
        "vectorcraft" => include_bytes!("../../../assets/app-icons/256/vectorcraft.png"),
        "filmcraft" => include_bytes!("../../../assets/app-icons/256/filmcraft.png"),
        "lightcraft" => include_bytes!("../../../assets/app-icons/256/lightcraft.png"),
        "pdfcraft" => include_bytes!("../../../assets/app-icons/256/pdfcraft.png"),
        "effectcraft" => include_bytes!("../../../assets/app-icons/256/effectcraft.png"),
        "designcraft" => include_bytes!("../../../assets/app-icons/256/designcraft.png"),
        "figcraft" => include_bytes!("../../../assets/app-icons/256/figcraft.png"),
        "wordcraft" => include_bytes!("../../../assets/app-icons/256/wordcraft.png"),
        "soundcraft" => include_bytes!("../../../assets/app-icons/256/soundcraft.png"),
        "deckcraft" => include_bytes!("../../../assets/app-icons/256/deckcraft.png"),
        "cadcraft" => include_bytes!("../../../assets/app-icons/256/cadcraft.png"),
        "gridcraft" => include_bytes!("../../../assets/app-icons/256/gridcraft.png"),
        "texturecraft" => include_bytes!("../../../assets/app-icons/256/texturecraft.png"),
        "materialcraft" => include_bytes!("../../../assets/app-icons/256/materialcraft.png"),
        "craft-anything" => include_bytes!("../../../assets/app-icons/256/craft-anything.png"),
        _ => return None,
    })
}

fn hex([r, g, b]: [u8; 3]) -> String {
    format!("#{r:02x}{g:02x}{b:02x}")
}

/// The app's web app manifest. Its addresses are relative to the manifest itself, so the app's
/// folder (one up) is its start page and its scope, and its id.
pub fn manifest(app: &CraftApp) -> String {
    let icons = if icon(app.id).is_some() {
        serde_json::json!([{ "src": "icon.png", "sizes": "256x256", "type": "image/png", "purpose": "any" }])
    } else {
        serde_json::json!([])
    };
    serde_json::json!({
        "name": app.name,
        "short_name": app.name,
        "description": app.tagline,
        "id": "../",
        "start_url": "../",
        "scope": "../",
        "display": "standalone",
        "background_color": "#121316",
        "theme_color": hex(app.color),
        "icons": icons,
    })
    .to_string()
}

/// `html` with the manifest link, the icon for iOS's home screen and the app's colour added at
/// the end of its `<head>`. `None` when the page has a manifest of its own, or no `</head>`.
pub fn add_manifest(html: &str, app: &CraftApp) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    if lower.contains("rel=\"manifest\"") || lower.contains("rel='manifest'") || lower.contains("rel=manifest") {
        return None;
    }
    let at = lower.find("</head>")?;
    let mut tags = format!("<link rel=\"manifest\" href=\"{DIR}/manifest.webmanifest\">");
    if icon(app.id).is_some() {
        tags.push_str(&format!("<link rel=\"apple-touch-icon\" href=\"{DIR}/icon.png\">"));
    }
    if !lower.contains("name=\"theme-color\"") {
        tags.push_str(&format!("<meta name=\"theme-color\" content=\"{}\">", hex(app.color)));
    }
    if !lower.contains("name=\"mobile-web-app-capable\"") {
        tags.push_str("<meta name=\"mobile-web-app-capable\" content=\"yes\">");
    }
    let mut out = String::with_capacity(html.len() + tags.len());
    out.push_str(html.get(..at)?);
    out.push_str(&tags);
    out.push_str(html.get(at..)?);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(id: &str) -> &'static CraftApp {
        CraftApp::by_id(id).unwrap()
    }

    #[test]
    fn every_official_app_has_an_icon() {
        for a in artcraft_launcher_core::catalog::SUITE.iter().filter(|a| crate::hosting::hostable(a)) {
            let png = icon(a.id).unwrap_or_else(|| panic!("{} has no PWA icon", a.id));
            assert!(png.starts_with(b"\x89PNG"), "{}", a.id);
        }
    }

    #[test]
    fn the_manifest_names_the_app_and_stays_in_its_folder() {
        let m: serde_json::Value = serde_json::from_str(&manifest(app("photocraft"))).unwrap();
        assert_eq!(m["name"], "PhotoCraft");
        assert_eq!(m["scope"], "../");
        assert_eq!(m["start_url"], "../");
        assert_eq!(m["display"], "standalone");
        assert_eq!(m["theme_color"], "#2f7bf5");
        assert_eq!(m["icons"][0]["src"], "icon.png");
    }

    #[test]
    fn the_link_goes_at_the_end_of_the_head_once() {
        let page = "<!DOCTYPE html><html><HEAD><title>x</title></HEAD><body></body></html>";
        let out = add_manifest(page, app("wordcraft")).unwrap();
        assert!(out.contains(r#"<link rel="manifest" href="__artcraft-pwa/manifest.webmanifest">"#));
        assert!(out.contains(r#"<link rel="apple-touch-icon" href="__artcraft-pwa/icon.png">"#));
        assert!(out.find("rel=\"manifest\"").unwrap() < out.find("</HEAD>").unwrap());
        assert!(out.contains("theme-color"));
        // A page with a manifest of its own (EffectCraft's) keeps it.
        assert!(add_manifest(&out, app("wordcraft")).is_none());
        assert!(add_manifest("<html><body>no head</body></html>", app("wordcraft")).is_none());
    }
}
