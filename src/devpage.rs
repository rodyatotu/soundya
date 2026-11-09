//! The developer build only (`--features dev`; the normal build has none of
//! this): puts the feedback mode (dev/feedback.js) into a page. Shared by the
//! speaker's panel (web.rs) and the installer's wizard (installer/src/main.rs).
//!
//! In <head>: the settings (`window.SOUNDYA_DEV`) and dev/errors.js, which has
//! to run before the page's own scripts to catch their errors; before </body>:
//! the feedback mode itself. html2canvas is loaded by it on the first picture.

pub const FEEDBACK_JS: &str = include_str!("../dev/feedback.js");
pub const HTML2CANVAS_JS: &str = include_str!("../dev/html2canvas.min.js");
/// The alarm that cannot be slept through, on the panel's home tab (src/alarm.rs).
pub const ALARM_JS: &str = include_str!("../dev/alarm.js");
const ERRORS_JS: &str = include_str!("../dev/errors.js");

/// The page with the feedback mode in it. `config` becomes `window.SOUNDYA_DEV`
/// (app, title, version, log, save, store, h2c — see dev/feedback.js).
pub fn inject(page: &str, config: &serde_json::Value) -> String {
    // «</script>» inside a string would end the script early.
    let config = config.to_string().replace("</", "<\\/");
    let head = format!("<head>\n<script data-sd>window.SOUNDYA_DEV={config};\n{ERRORS_JS}</script>");
    let body = "<script data-sd src=\"/dev/feedback.js\"></script>\n<script data-sd src=\"/dev/alarm.js\"></script>\n</body>";
    let page = page.replacen("<head>", &head, 1);
    match page.rfind("</body>") {
        Some(i) => format!("{}{body}{}", &page[..i], &page[i + "</body>".len()..]),
        None => page + body,
    }
}

/// The two scripts by path, for the page's server.
pub fn script(path: &str) -> Option<&'static str> {
    match path {
        "/dev/feedback.js" => Some(FEEDBACK_JS),
        "/dev/html2canvas.js" => Some(HTML2CANVAS_JS),
        "/dev/alarm.js" => Some(ALARM_JS),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn puts_the_feedback_mode_into_the_page() {
        let page = "<!doctype html><html><head><title>t</title></head><body><p>x</p></body></html>";
        let out = inject(page, &serde_json::json!({ "app": "panel", "title": "a</script>b" }));
        let head = out.find("window.SOUNDYA_DEV").unwrap();
        assert!(head < out.find("<title>").unwrap(), "before the page's own scripts");
        assert!(out.contains("a<\\/script>b"), "the config cannot close the script");
        assert!(out.contains("__sdErrors"));
        let tag = out.find("src=\"/dev/feedback.js\"").unwrap();
        assert!(tag > out.find("<p>x</p>").unwrap() && tag < out.find("</body>").unwrap());
        assert_eq!(out.matches("</body>").count(), 1);
        assert!(script("/dev/feedback.js").unwrap().contains("Собрать промпт"));
        assert!(script("/dev/html2canvas.js").unwrap().contains("html2canvas 1.4.1"));
        assert!(script("/dev/alarm.js").unwrap().contains("не выключают"));
        assert!(out.contains("src=\"/dev/alarm.js\""));
        assert!(script("/dev/other.js").is_none());
    }
}
