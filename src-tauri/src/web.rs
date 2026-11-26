//! Tier 3: live web. Model- or user-initiated; every fetch is cached in SQLite.
use crate::AppState;
use serde::Serialize;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0 Safari/537.36";
const MAX_TEXT: usize = 200_000;
const CACHE_TTL_SECS: i64 = 7 * 24 * 3600;

fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

pub fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
                match hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    Some(b) => { out.push(b); i += 3; }
                    None => { out.push(b'%'); i += 1; }
                }
            }
            b'+' => { out.push(b' '); i += 1; }
            b => { out.push(b); i += 1; }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// HTML → readable text: drop script/style, tags to whitespace, decode common
/// entities, normalize blank lines.
pub fn strip_html(input: &str) -> String {
    use regex::Regex;
    use std::sync::OnceLock;
    static RULES: OnceLock<Vec<(Regex, &'static str)>> = OnceLock::new();
    let rules = RULES.get_or_init(|| {
        let mk = |p: &str, rep: &'static str| (Regex::new(p).expect("valid regex"), rep);
        vec![
            mk(r"(?is)<(script|style|noscript|head|nav|footer|svg|iframe|form|button|select)[^>]*>.*?</[a-z]+>", " "),
            mk(r"(?is)<!--.*?-->", " "),
            mk(r"(?is)<br\s*/?>", "\n"),
            mk(r"(?is)</(p|div|li|h[1-6]|tr|section|article|pre|ul|ol|table)>", "\n"),
            mk(r"(?is)<[^>]+>", " "),
            mk("&nbsp;", " "), mk("&amp;", "&"), mk("&lt;", "<"), mk("&gt;", ">"),
            mk("&quot;", "\""), mk("&#39;", "'"), mk("&#x27;", "'"), mk("&rsquo;", "'"),
            mk("(?i)&#x?[0-9a-f]+;", " "),
        ]
    });
    let mut text = input.to_string();
    for (re, rep) in rules {
        text = re.replace_all(&text, *rep).into_owned();
    }
    let mut out = String::with_capacity(text.len());
    let mut last_space = true;
    let mut newlines = 0;
    for ch in text.chars() {
        if ch == '\n' {
            newlines += 1;
            if newlines <= 2 { out.push('\n'); }
        } else {
            newlines = 0;
            if ch.is_whitespace() {
                if !last_space { out.push(' '); }
                last_space = true;
            } else {
                out.push(ch);
                last_space = false;
            }
        }
    }
    out.trim().to_string()
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn client() -> reqwest::Client {
    reqwest::Client::builder().user_agent(UA).build().unwrap_or_default()
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FetchResult {
    pub url: String,
    pub title: String,
    pub text: String,
    pub cached: bool,
}

fn html_title(raw: &str) -> String {
    use regex::Regex;
    let re = Regex::new(r"(?is)<title[^>]*>(.*?)</title>").expect("regex");
    re.captures(raw)
        .map(|c| strip_html(&c[1]).trim().chars().take(120).collect())
        .unwrap_or_default()
}

fn truncate(s: String) -> String {
    if s.chars().count() <= MAX_TEXT {
        return s;
    }
    let cut: String = s.chars().take(MAX_TEXT).collect();
    format!("{cut}\n…[truncated]")
}

/// Fetch one URL and convert to readable text (no cache, no locks).
pub async fn fetch_net(url: &str) -> Result<(String, String), String> {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err("only http(s) URLs are allowed".into());
    }
    let resp = client()
        .get(url)
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }
    let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
    let raw = String::from_utf8_lossy(&bytes).into_owned();
    let title = html_title(&raw);
    let head = raw.chars().take(600).collect::<String>().to_lowercase();
    let text = if head.contains("<html") || head.contains("<!doctype") || raw.contains("</p>") {
        strip_html(&raw)
    } else {
        raw
    };
    Ok((title, truncate(text)))
}

pub fn cache_put(conn: &rusqlite::Connection, url: &str, title: &str, text: &str) {
    conn.execute(
        "INSERT INTO fetch_cache(url, title, text, fetched_at) VALUES(?1,?2,?3,?4)
         ON CONFLICT(url) DO UPDATE SET title=excluded.title, text=excluded.text, fetched_at=excluded.fetched_at",
        rusqlite::params![url, title, text, now()],
    )
    .ok();
}

pub fn cache_get(conn: &rusqlite::Connection, url: &str) -> Option<(String, String)> {
    let (title, text, at): (String, String, i64) = conn
        .query_row("SELECT title, text, fetched_at FROM fetch_cache WHERE url=?1", [url], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .ok()?;
    (now() - at < CACHE_TTL_SECS).then_some((title, text))
}

#[tauri::command(async)]
pub async fn fetch_url(url: String, state: tauri::State<'_, AppState>) -> Result<FetchResult, String> {
    let cached = {
        let db = state.db.lock().map_err(|e| e.to_string())?;
        cache_get(&db, &url)
    };
    if let Some((title, text)) = cached {
        return Ok(FetchResult { title, url, text, cached: true });
    }
    let (title, text) = fetch_net(&url).await?;
    {
        let db = state.db.lock().map_err(|e| e.to_string())?;
        cache_put(&db, &url, &title, &text);
    }
    Ok(FetchResult { title, url, text, cached: false })
}

#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Hit {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

fn clean(s: &str) -> String {
    strip_html(s).split_whitespace().collect::<Vec<_>>().join(" ")
}

async fn ddg(q: &str) -> Result<Vec<Hit>, String> {
    use regex::Regex;
    let url = format!("https://html.duckduckgo.com/html/?q={}", urlencode(q));
    let resp = client().get(&url).timeout(Duration::from_secs(15)).send().await.map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("ddg HTTP {}", resp.status()));
    }
    let html = String::from_utf8_lossy(&resp.bytes().await.map_err(|e| e.to_string())?).into_owned();
    if html.contains("challenge") || html.contains("anomaly") {
        return Err("ddg bot challenge".into());
    }
    let re = Regex::new(r#"(?s)<a[^>]*class="result__a"[^>]*href="([^"]+)"[^>]*>(.*?)</a>"#).expect("regex");
    let re2 = Regex::new(r#"(?s)<a[^>]*href="([^"]+)"[^>]*class="result__a"[^>]*>(.*?)</a>"#).expect("regex");
    let re_snip = Regex::new(r#"(?s)class="result__snippet"[^>]*>(.*?)</(a|span)>"#).expect("regex");
    let mut hits: Vec<Hit> = Vec::new();
    for cap in re.captures_iter(&html).chain(re2.captures_iter(&html)) {
        let mut u = cap[1].trim().to_string();
        if let Some(i) = u.find("uddg=") {
            let rest = &u[i + 5..];
            let end = rest.find('&').unwrap_or(rest.len());
            u = percent_decode(&rest[..end]);
        } else if u.starts_with("//") {
            u = format!("https:{u}");
        }
        hits.push(Hit { title: clean(&cap[2]), url: u, snippet: String::new() });
    }
    let snippets: Vec<String> = re_snip.captures_iter(&html).map(|c| clean(&c[1])).collect();
    for (i, h) in hits.iter_mut().enumerate() {
        if let Some(s) = snippets.get(i) {
            h.snippet = s.clone();
        }
    }
    Ok(hits)
}

async fn stackoverflow(q: &str) -> Result<Vec<Hit>, String> {
    let url = format!(
        "https://api.stackexchange.com/2.3/search/advanced?order=desc&sort=relevance&pagesize=8&site=stackoverflow&q={}",
        urlencode(q)
    );
    let v: serde_json::Value = client().get(&url).timeout(Duration::from_secs(15))
        .send().await.map_err(|e| e.to_string())?
        .error_for_status().map_err(|e| e.to_string())?
        .json().await.map_err(|e| e.to_string())?;
    let items = v["items"].as_array().ok_or("no items")?;
    Ok(items.iter().filter_map(|it| {
        Some(Hit {
            title: clean(it["title"].as_str()?),
            url: it["link"].as_str()?.to_string(),
            snippet: format!("score {}, answered: {}", it["score"].as_i64().unwrap_or(0), it["is_answered"].as_bool().unwrap_or(false)),
        })
    }).collect())
}

async fn wikipedia(q: &str) -> Result<Vec<Hit>, String> {
    let url = format!(
        "https://en.wikipedia.org/w/api.php?action=opensearch&limit=6&format=json&search={}",
        urlencode(q)
    );
    let v: serde_json::Value = client().get(&url).timeout(Duration::from_secs(15))
        .send().await.map_err(|e| e.to_string())?
        .error_for_status().map_err(|e| e.to_string())?
        .json().await.map_err(|e| e.to_string())?;
    let titles = v[1].as_array().ok_or("no titles")?;
    let descs = v[2].as_array().ok_or("no descs")?;
    let urls = v[3].as_array().ok_or("no urls")?;
    Ok(titles.iter().enumerate().filter_map(|(i, t)| {
        Some(Hit {
            title: t.as_str()?.to_string(),
            url: urls.get(i)?.as_str()?.to_string(),
            snippet: descs.get(i).and_then(|d| d.as_str()).unwrap_or("").to_string(),
        })
    }).collect())
}

/// Tier-3 search: DuckDuckGo first, then StackOverflow, then Wikipedia.
#[tauri::command(async)]
pub async fn web_search(query: String) -> Result<Vec<Hit>, String> {
    let q = query.trim().to_string();
    if q.is_empty() {
        return Ok(vec![]);
    }
    let mut errs: Vec<String> = Vec::new();
    macro_rules! attempt {
        ($name:literal, $f:path) => {
            match $f(&q).await {
                Ok(hits) if !hits.is_empty() => {
                    let mut seen = std::collections::HashSet::new();
                    let out: Vec<Hit> = hits.into_iter()
                        .filter(|h| !h.url.is_empty() && seen.insert(h.url.clone()))
                        .take(8)
                        .collect();
                    return Ok(out);
                }
                Ok(_) => errs.push(concat!($name, ": empty").to_string()),
                Err(e) => errs.push(format!("{}: {}", $name, e)),
            }
        };
    }
    attempt!("duckduckgo", ddg);
    attempt!("stackoverflow", stackoverflow);
    attempt!("wikipedia", wikipedia);
    Err(format!("all search sources failed: {}", errs.join("; ")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_decode_works() {
        assert_eq!(percent_decode("a%20b%2Fc+d"), "a b/c d");
        assert_eq!(percent_decode("100%25"), "100%");
        assert_eq!(percent_decode("no-escapes"), "no-escapes");
        assert_eq!(percent_decode("%zz"), "%zz");
    }

    #[test]
    fn strip_html_removes_noise() {
        let html = r#"<html><head><title>Hi &amp; Bye</title><style>b{color:red}</style></head>
            <body><script>alert(1)</script><h1>Header</h1><p>Some &quot;text&quot; here.</p>
            <svg><path d="M0 0"/><circle>icon junk</circle></svg>
            <form><select><option>dropdown garbage</option></select></form></body></html>"#;
        let out = strip_html(html);
        assert!(!out.contains("alert"), "{out}");
        assert!(!out.contains("color:red"), "{out}");
        assert!(!out.contains("M0 0"), "{out}");
        assert!(!out.contains("icon junk"), "{out}");
        assert!(!out.contains("dropdown garbage"), "{out}");
        assert!(out.contains("Header"), "{out}");
        assert!(out.contains("Some \"text\" here."), "{out}");
    }

    #[test]
    fn cache_roundtrip() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(crate::db::SCHEMA).unwrap();
        cache_put(&conn, "https://x", "Example Title", "hello");
        let (t, body) = cache_get(&conn, "https://x").unwrap();
        assert_eq!((t.as_str(), body.as_str()), ("Example Title", "hello"));
        assert_eq!(cache_get(&conn, "https://none"), None);
    }

    #[tokio::test]
    async fn live_search_returns_hits() {
        let hits = web_search("python context manager".into()).await.expect("search failed");
        assert!(!hits.is_empty());
        assert!(hits.iter().all(|h| h.url.starts_with("http")), "{:?}", hits);
    }

    #[tokio::test]
    async fn live_fetch_strips_html() {
        let (title, text) = fetch_net("https://example.com").await.expect("fetch failed");
        assert!(title.contains("Example Domain"), "title={title}");
        // the page body is a single short paragraph; assert we got readable,
        // tag-free text out of it rather than a specific marketing sentence
        assert!(text.contains("domain"), "text={:.200}", text);
        assert!(!text.contains("<p>"), "html not stripped: {:.200}", text);
    }
}
