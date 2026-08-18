use super::{PreparedImage, ToolExecError, context::ToolContext, resolve_path};
use choreo_keystore::ServiceCredential;
use headless_chrome::protocol::cdp::Page::CaptureScreenshotFormatOption;
use headless_chrome::{Browser, LaunchOptions};
use image::GenericImageView;
use schemars::JsonSchema;
use serde::Deserialize;
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;
use url::Url;

/// What `retrieve_webpage` should produce from the rendered page.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WebpageAction {
    /// Fully rendered HTML (outerHTML); restricted to `selector` when given.
    Content,
    /// Human-readable plain text of the page (innerText); defaults to `<body>`.
    Text,
    /// A PNG screenshot, saved to `output_path` if given and shown inline.
    Screenshot,
    /// A PDF of the page; requires `output_path` (binary output can't be a string).
    Pdf,
}

impl WebpageAction {
    fn as_str(&self) -> &'static str {
        match self {
            WebpageAction::Content => "content",
            WebpageAction::Text => "text",
            WebpageAction::Screenshot => "screenshot",
            WebpageAction::Pdf => "pdf",
        }
    }
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct RetrieveWebpageArgs {
    /// URL to render (http or https only).
    url: String,
    /// What to retrieve. Defaults to "content".
    action: Option<WebpageAction>,
    /// Milliseconds to wait after load before capturing (lets JS settle). 0 = none.
    wait_ms: Option<u64>,
    /// Navigation / element-wait timeout in milliseconds. Default 30_000.
    timeout_ms: Option<u64>,
    /// Viewport width. Default 1280.
    width: Option<u32>,
    /// Viewport height. Default 800.
    height: Option<u32>,
    /// Screenshot: capture the full scrollable page (surface). Default true.
    full_page: Option<bool>,
    /// Restrict content/text/screenshot capture to this CSS selector.
    selector: Option<String>,
    /// Where to write the result for `screenshot` / `pdf`. Resolved against the
    /// session working directory. PDFs require this.
    output_path: Option<String>,
}

/// Default viewport / nav timeout used when the caller omits them.
const DEFAULT_TIMEOUT_MS: u64 = 30_000;

/// Binary names/known paths to search for, in *preference* order: chromium
/// first, then the various chrome bundle names, so a Chromium install wins
/// over Chrome when both are present.
const CANDIDATE_NAMES: &[&str] = &[
    "chromium",
    "chromium-browser",
    "google-chrome",
    "google-chrome-stable",
    "google-chrome-beta",
    "google-chrome-unstable",
    "chrome",
    "microsoft-edge",
    "brave-browser",
];

/// Resolve a locally-installed Chromium/Chrome binary, preferring chromium.
///
/// Lookup order:
/// 1. `CHROMIUM_BIN`, then `CHROME_BIN` env overrides (must exist).
/// 2. Names on `PATH` (chromium first).
/// 3. Known absolute install paths for the current OS.
///
/// Returns `None` when nothing usable is found — the caller reports that as a
/// clear error telling the operator to install Chromium (this tool deliberately
/// does NOT auto-download a browser).
fn resolve_browser_binary() -> Option<std::path::PathBuf> {
    for var in ["CHROMIUM_BIN", "CHROME_BIN"] {
        if let Some(p) = std::env::var_os(var) {
            let candidate = std::path::PathBuf::from(p);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }

    // Search each PATH entry for a candidate executable name.
    if let Some(path_var) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path_var) {
            for name in CANDIDATE_NAMES {
                let candidate = dir.join(name);
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }

    // Known absolute locations (best-effort per OS), chromium first.
    #[cfg(target_os = "macos")]
    {
        let mac_paths = [
            "/Applications/Chromium.app/Contents/MacOS/Chromium",
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
            "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
        ];
        for p in mac_paths {
            let candidate = std::path::PathBuf::from(p);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    #[cfg(target_os = "windows")]
    {
        for base in [
            "C:/Program Files/Chromium/Application/chrome.exe",
            "C:/Program Files/Google/Chrome/Application/chrome.exe",
            "C:/Program Files (x86)/Google/Chrome/Application/chrome.exe",
            "C:/Program Files (x86)/Microsoft/Edge/Application/msedge.exe",
        ] {
            let candidate = std::path::PathBuf::from(base);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }

    None
}

/// True when `url` has an http or https scheme (the only schemes a headless
/// browser should be asked to render; guards file:// and other local schemes).
fn validate_url(url: &str) -> Result<(), ToolExecError> {
    let parsed = Url::parse(url).map_err(|e| ToolExecError(format!("invalid URL '{url}': {e}")))?;
    match parsed.scheme() {
        "http" | "https" => Ok(()),
        other => Err(ToolExecError(format!(
            "unsupported URL scheme '{other}'; only http/https are allowed"
        ))),
    }
}

/// Build the JS expression that extracts text (innerText) from a bound node.
/// `selector` is JSON-embedded so it can't break out of the string literal.
fn text_expression(selector: Option<&str>) -> String {
    match selector {
        Some(sel) => {
            let sel = serde_json::to_string(sel).unwrap_or_else(|_| "\"body\"".to_string());
            format!(
                "(() => {{ const e = document.querySelector({sel}); return e ? e.innerText : ''; }})()"
            )
        }
        None => "(() => { const e = document.body; return e ? e.innerText : ''; })()".to_string(),
    }
}

/// Build the JS expression that extracts HTML (outerHTML) from a bound node.
fn html_expression(selector: &str) -> String {
    let sel = serde_json::to_string(selector).unwrap_or_else(|_| "\"html\"".to_string());
    format!("(() => {{ const e = document.querySelector({sel}); return e ? e.outerHTML : ''; }})()")
}

/// Decode PNG/JPEG dimensions from raw screenshot bytes so the client can
/// render the image with a known aspect ratio.
fn decode_image_dimensions(bytes: &[u8]) -> (u32, u32) {
    image::load_from_memory(bytes)
        .map(|img| img.dimensions())
        .unwrap_or((0, 0))
}

/// A tool that renders a URL in a local headless Chromium/Chrome and returns
/// page content (HTML), plain text, a screenshot, or a PDF.
pub struct RetrieveWebpage {
    /// Holds the most recent screenshot so `extract_image` can hand it to the
    /// client as an inline image (same pattern as `DisplayImage`).
    last_image: Mutex<Option<PreparedImage>>,
}

impl RetrieveWebpage {
    pub fn new() -> Self {
        RetrieveWebpage {
            last_image: Mutex::new(None),
        }
    }
}

impl Default for RetrieveWebpage {
    fn default() -> Self {
        Self::new()
    }
}

impl super::Tool for RetrieveWebpage {
    type Args = RetrieveWebpageArgs;
    type Return = String;
    type Error = ToolExecError;

    fn name(&self) -> &'static str {
        "retrieve_webpage"
    }
    fn description(&self) -> &'static str {
        "Render a URL in a local headless Chromium/Chrome and return page content (HTML), plain text, a screenshot (PNG), or a PDF. Runs locally and offline; requires a chromium/chrome binary already installed (prefers chromium; override with CHROMIUM_BIN). Screenshots are returned inline or saved to output_path; PDFs require output_path. One-shot per call — no persistent session."
    }
    fn describe_invocation(&self, args: &Self::Args) -> String {
        let action = args
            .action
            .as_ref()
            .map(|a| a.as_str())
            .unwrap_or(WebpageAction::Content.as_str());
        let mut parts = vec![format!(
            "Retrieving web page ({action}). URL: {}.",
            args.url
        )];
        if let Some(sel) = args.selector.as_deref() {
            parts.push(format!(" Selector: {sel}."));
        }
        if let Some(out) = args.output_path.as_deref() {
            parts.push(format!(" Output: {out}."));
        }
        parts.concat()
    }

    fn return_string(ret: &Self::Return) -> String {
        ret.clone()
    }

    fn execute(
        &self,
        args: Self::Args,
        _x_credentials: Option<&ServiceCredential>,
        working_dir: Option<&Path>,
        _ctx: Option<&ToolContext>,
    ) -> Result<Self::Return, Self::Error> {
        let action = args.action.clone().unwrap_or(WebpageAction::Content);
        let url = args.url.trim();
        validate_url(url)?;

        let binary = resolve_browser_binary().ok_or_else(|| {
            ToolExecError(
                "no chromium or chrome binary found on PATH (or in standard locations). \
                 Install Chromium/Chrome, or set CHROMIUM_BIN / CHROME_BIN to its path"
                    .to_string(),
            )
        })?;

        // Launch a private, headless, one-shot browser instance with an
        // explicit path (so it uses the resolved chromium, never auto-detect)
        // and our viewport.
        let mut builder = LaunchOptions::default_builder();
        builder.headless(true);
        builder.path(Some(binary));
        builder.window_size(Some((
            args.width.unwrap_or(1280),
            args.height.unwrap_or(800),
        )));
        // Keep the DevTools socket alive well past an individual page wait so a
        // slow page can't get torn down mid-navigation.
        builder.idle_browser_timeout(Duration::from_secs(60));
        let options = builder.build().map_err(|e| ToolExecError(e.to_string()))?;

        let browser = Browser::new(options)
            .map_err(|e| ToolExecError(format!("failed to launch headless browser: {e:#}")))?;

        // Run the whole capture in a closure so the `Browser` (and its Chromium
        // child process) is released on every path, success or error, when it
        // drops. Explicit `close()` is best-effort on top of that.
        let outcome = (|| -> Result<String, ToolExecError> {
            let tab = browser
                .new_tab()
                .map_err(|e| ToolExecError(format!("failed to open a tab: {e:#}")))?;

            let timeout = Duration::from_millis(args.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS));
            tab.set_default_timeout(timeout);
            tab.navigate_to(url)
                .map_err(|e| ToolExecError(format!("navigation to {url} failed: {e:#}")))?;
            tab.wait_until_navigated()
                .map_err(|e| ToolExecError(format!("page never finished loading: {e:#}")))?;

            // Optional settle delay so client-side JS can run before capture.
            // Only reached at runtime (never in unit tests, per repo rules).
            if let Some(ms) = args.wait_ms
                && ms > 0
            {
                std::thread::sleep(Duration::from_millis(ms));
            }

            match action {
                WebpageAction::Content => match args.selector.as_deref() {
                    Some(sel) => {
                        let obj = tab
                            .evaluate(&html_expression(sel), false)
                            .map_err(|e| ToolExecError(format!("failed to extract HTML: {e:#}")))?;
                        Ok(remote_text(&obj))
                    }
                    None => tab
                        .get_content()
                        .map_err(|e| ToolExecError(format!("failed to get page HTML: {e:#}"))),
                },

                WebpageAction::Text => {
                    let expr = text_expression(args.selector.as_deref());
                    let obj = tab
                        .evaluate(&expr, false)
                        .map_err(|e| ToolExecError(format!("failed to extract text: {e:#}")))?;
                    Ok(remote_text(&obj))
                }

                WebpageAction::Screenshot => {
                    let bytes = if let Some(sel) = args.selector.as_deref() {
                        let element = tab.find_element(sel).map_err(|e| {
                            ToolExecError(format!("selector '{sel}' not found: {e:#}"))
                        })?;
                        element
                            .capture_screenshot(CaptureScreenshotFormatOption::Png)
                            .map_err(|e| {
                                ToolExecError(format!("failed to screenshot element: {e:#}"))
                            })?
                    } else {
                        tab.capture_screenshot(
                            CaptureScreenshotFormatOption::Png,
                            None,
                            None,
                            args.full_page.unwrap_or(true),
                        )
                        .map_err(|e| {
                            ToolExecError(format!("failed to capture screenshot: {e:#}"))
                        })?
                    };

                    // Always offer the screenshot inline to the client.
                    let (width, height) = decode_image_dimensions(&bytes);
                    let alt = Some(format!("Screenshot of {url}"));
                    *self.last_image.lock().unwrap_or_else(|e| e.into_inner()) =
                        Some(PreparedImage {
                            mime_type: "image/png".to_string(),
                            data: bytes.clone(),
                            width,
                            height,
                            alt,
                        });

                    if let Some(out) = args.output_path.as_deref() {
                        let path = resolve_path(out, working_dir);
                        write_bytes_with_dirs(&path, &bytes)?;
                        Ok(format!(
                            "captured screenshot ({width}x{height}, PNG, {size}); saved to {path}",
                            size = super::human_size(bytes.len() as u64),
                            path = path.display(),
                        ))
                    } else {
                        Ok(format!(
                            "captured screenshot ({width}x{height}, PNG, {size})",
                            size = super::human_size(bytes.len() as u64),
                        ))
                    }
                }

                WebpageAction::Pdf => {
                    let out = args.output_path.as_deref().ok_or_else(|| {
                        ToolExecError(
                            "pdf action requires output_path so the binary can be saved"
                                .to_string(),
                        )
                    })?;
                    let bytes = tab
                        .print_to_pdf(None)
                        .map_err(|e| ToolExecError(format!("failed to render PDF: {e:#}")))?;
                    let path = resolve_path(out, working_dir);
                    write_bytes_with_dirs(&path, &bytes)?;
                    Ok(format!(
                        "saved PDF ({size}) to {path}",
                        size = super::human_size(bytes.len() as u64),
                        path = path.display(),
                    ))
                }
            }
        })();

        // The `Browser` is dropped here (and its Chromium child process is
        // terminated by the transport's `Drop`), releasing the instance whether
        // the capture succeeded or failed — see the closure above.
        outcome
    }

    fn extract_image(&self, _ret: &Self::Return) -> Option<PreparedImage> {
        self.last_image
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }
}

/// Pull the returned string out of a CDP `RemoteObject`: the protocol encodes
/// a primitive string result as a JSON value (`value.as_str()`).
fn extract_text(value: &serde_json::Value) -> String {
    value.as_str().map(str::to_owned).unwrap_or_default()
}

/// Convenience: unwrap a `RemoteObject`'s optional `value` and extract text.
fn remote_text(object: &headless_chrome::protocol::cdp::Runtime::RemoteObject) -> String {
    object.value.as_ref().map(extract_text).unwrap_or_default()
}

/// Write `bytes` to `path`, creating parent directories as needed.
fn write_bytes_with_dirs(path: &Path, bytes: &[u8]) -> Result<(), ToolExecError> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .map_err(|e| ToolExecError(format!("failed to create output dir: {e}")))?;
    }
    std::fs::write(path, bytes)
        .map_err(|e| ToolExecError(format!("failed to write '{}': {e}", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::Tool;

    #[test]
    fn validate_url_accepts_http_and_https() {
        for u in ["https://example.com", "http://example.com/path?q=1"] {
            assert!(validate_url(u).is_ok(), "{u} should be accepted");
        }
    }

    #[test]
    fn validate_url_rejects_non_http_schemes() {
        for u in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "ftp://x",
            "not a url",
        ] {
            let err = validate_url(u).unwrap_err();
            assert!(!err.to_string().is_empty());
        }
    }

    #[test]
    fn text_expression_embeds_selector_safely() {
        // A selector containing a quote must be JSON-escaped, not interpolated,
        // so it cannot break out of the string literal.
        let expr = text_expression(Some("div[data-x=\"y\"]"));
        assert!(expr.contains("\\\"y\\\""));
        assert!(expr.contains("innerText"));
    }

    #[test]
    fn text_expression_defaults_to_body() {
        let expr = text_expression(None);
        assert!(expr.contains("document.body"));
        assert!(expr.contains("innerText"));
    }

    #[test]
    fn html_expression_uses_outer_html() {
        let expr = html_expression("#main");
        assert!(expr.contains("#main"));
        assert!(expr.contains("outerHTML"));
    }

    #[test]
    fn describe_invocation_defaults_to_content() {
        let tool = RetrieveWebpage::new();
        let args = RetrieveWebpageArgs {
            url: "https://example.com".to_string(),
            ..RetrieveWebpageArgs::default()
        };
        let desc = tool.describe_invocation(&args);
        assert!(desc.contains("content"));
        assert!(desc.contains("https://example.com"));
    }

    #[test]
    fn describe_invocation_includes_selector_and_output() {
        let tool = RetrieveWebpage::new();
        let args = RetrieveWebpageArgs {
            url: "https://example.com".to_string(),
            action: Some(WebpageAction::Screenshot),
            selector: Some("#main".to_string()),
            output_path: Some("shot.png".to_string()),
            ..RetrieveWebpageArgs::default()
        };
        let desc = tool.describe_invocation(&args);
        assert!(desc.contains("screenshot"));
        assert!(desc.contains("#main"));
        assert!(desc.contains("shot.png"));
    }

    #[test]
    fn extract_text_handles_string_and_absent_value() {
        assert_eq!(extract_text(&serde_json::json!("hello")), "hello");
        // Non-string / null values must degrade to an empty string.
        assert_eq!(extract_text(&serde_json::Value::Null), "");
        assert_eq!(extract_text(&serde_json::json!(42)), "");
    }
}
