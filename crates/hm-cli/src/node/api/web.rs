//! The web interface: an HTML shell, one stylesheet and ES modules (in
//! `src/node/web/`), built into the binary and served as they are. They hold
//! no data, so they need no token: every value comes from the API behind it.
//! The page loads nothing from anywhere else, the map's coastline included,
//! so it works without internet and runs under a policy of `'self'` only.

use axum::http::header;
use axum::routing::get;
use axum::Router;

use super::AppState;

/// One file of the web interface.
struct Asset {
    path: &'static str,
    kind: &'static str,
    body: &'static str,
}

macro_rules! assets {
    ($($kind:ident $path:literal),* $(,)?) => {
        &[$(Asset { path: $path, kind: $kind, body: include_str!(concat!("../web", $path)) }),*]
    };
}

const HTML: &str = "text/html; charset=utf-8";
const CSS: &str = "text/css; charset=utf-8";
const JS: &str = "text/javascript; charset=utf-8";

const ASSETS: &[Asset] = assets![
    HTML "/index.html",
    CSS "/app.css",
    JS "/js/main.js",
    JS "/js/api.js",
    JS "/js/store.js",
    JS "/js/dom.js",
    JS "/js/format.js",
    JS "/js/geo.js",
    JS "/js/land.js",
    JS "/js/map.js",
    JS "/js/charts.js",
    JS "/js/explain.js",
    JS "/js/form.js",
    JS "/js/messages.js",
    JS "/js/views/chat.js",
    JS "/js/views/mail.js",
    JS "/js/views/bulletins.js",
    JS "/js/views/network.js",
    JS "/js/views/station.js",
    JS "/js/views/beliefs.js",
    JS "/js/views/settings.js",
];

/// Every file at its path, and the shell at `/` too.
pub(super) fn routes() -> Router<AppState> {
    let serve = |asset: &'static Asset| {
        get(move || async move { ([(header::CONTENT_TYPE, asset.kind)], asset.body) })
    };
    ASSETS
        .iter()
        .fold(Router::new().route("/", serve(&ASSETS[0])), |router, asset| {
            router.route(asset.path, serve(asset))
        })
}

#[cfg(test)]
mod tests {
    use super::ASSETS;

    /// Where `import … from "./x.js"` in the module at `from` points.
    fn resolve(from: &str, relative: &str) -> String {
        let mut parts: Vec<&str> = from.split('/').collect();
        parts.pop();
        for part in relative.split('/') {
            match part {
                "." => {}
                ".." => {
                    parts.pop();
                }
                part => parts.push(part),
            }
        }
        parts.join("/")
    }

    #[test]
    fn every_module_imported_is_served() {
        let served: Vec<&str> = ASSETS.iter().map(|a| a.path).collect();
        for asset in ASSETS.iter().filter(|a| a.path.ends_with(".js")) {
            for line in asset.body.lines().filter(|l| l.starts_with("import ")) {
                let target = line.split('"').nth(1).expect("an import names a module");
                let path = resolve(asset.path, target);
                assert!(
                    served.contains(&path.as_str()),
                    "{} imports {path}, not served",
                    asset.path
                );
            }
        }
    }

    #[test]
    fn the_page_loads_nothing_from_elsewhere() {
        for asset in ASSETS {
            assert!(
                !asset.body.contains("https://"),
                "{} refers to another site",
                asset.path
            );
        }
    }
}
