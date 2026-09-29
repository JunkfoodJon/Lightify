//! Beatport Top-100 chart scraping + Spotify match scoring.
//!
//! Ported verbatim (behavior-for-behavior) from the shipped host's
//! `src-tauri/src/beatport.rs` + the `beatport_match_track` scorer in `main.rs`,
//! so the native shell resolves the same chart entries and Spotify matches.
//! Fetches a Beatport genre page and extracts track data from embedded JSON
//! (`__NEXT_DATA__` or inline arrays); tracks are simple structs for display and
//! Spotify search matching.

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use wreq_util::Emulation;

use crate::config;
use crate::model::Track;

/// Optional previously-saved Beatport session cookies (a local file in the data
/// directory), reused so the shell never needs its own embedded browser to get past
/// Beatport's bot check (Tier 2 has no WebView). Without the file, chart requests are
/// plain HTTP and may be challenged; the app then points the user at the chart in
/// their browser instead.
#[derive(Debug, Clone, Deserialize)]
struct BeatportCookie {
    name: String,
    value: String,
}

/// Read every reused cookie, if the helper script has ever been run and its
/// output hasn't been deleted. Silently `None` when absent — the caller falls
/// back to the plain (possibly challenged) request, exactly like before this
/// existed. Accepts both the old single-cookie JSON object (from before this
/// supported multiple cookies) and the current JSON array, so an already-written
/// file from an older script version still works until it's re-run.
fn load_cookie_header() -> Option<String> {
    let path = config::data_dir().join("lightify_beatport_cookie.json");
    let raw = std::fs::read_to_string(path).ok()?;
    let cookies: Vec<BeatportCookie> = serde_json::from_str(&raw)
        .map(|c: BeatportCookie| vec![c])
        .or_else(|_| serde_json::from_str::<Vec<BeatportCookie>>(&raw))
        .ok()?;
    // Skip any cookie that can't go on the wire as-is. One stray CR/LF, `;` or
    // other non-visible byte (a hand-edited file, a script decoding glitch) would
    // make the whole `Cookie` header invalid, and `send()` would then fail for
    // *every* chart fetch — worse than the documented no-cookie fallback. Dropping
    // just the bad entry keeps the rest (usually `cf_clearance` itself) usable.
    let header = cookies
        .iter()
        .filter(|c| is_cookie_name(&c.name) && is_cookie_value(&c.value))
        .map(|c| format!("{}={}", c.name, c.value))
        .collect::<Vec<_>>()
        .join("; ");
    if header.is_empty() {
        return None;
    }
    Some(header)
}

/// An RFC 6265 cookie name: a non-empty HTTP token (visible ASCII, none of the
/// separators — notably `=`, `;` and `,`, which would split the header apart).
fn is_cookie_name(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(|b| b.is_ascii_graphic() && !b"()<>@,;:\\\"/[]?={}".contains(&b))
}

/// A cookie value safe to splice into a `Cookie` header: non-empty visible ASCII
/// with no `;` (which would start a bogus next cookie). Deliberately a little
/// looser than RFC 6265's cookie-octet — Cloudflare's own values stay well inside
/// it, and the point is only to reject what would break the request.
fn is_cookie_value(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|b| b.is_ascii_graphic() && b != b';')
}

/// An HTTP client that impersonates Edge's real TLS/HTTP2 fingerprint (via
/// `wreq`, a `reqwest` fork built on BoringSSL), not just its headers.
///
/// A matching `User-Agent` and `Sec-CH-UA*` Client Hints alone weren't enough
/// to make Beatport/Cloudflare honor a freshly reused, definitely-not-expired
/// `cf_clearance` cookie — every request still came back as a full Cloudflare
/// **managed challenge** (`cType: 'managed'`, the interactive "Just a moment..."
/// page), identical to a first-time visitor with no cookie at all. `reqwest`'s
/// default TLS stack (schannel on Windows) produces a JA3/JA4 fingerprint
/// nothing like real Chromium/BoringSSL, and Cloudflare's managed challenge
/// re-triggers per-connection when that fingerprint doesn't match whatever
/// solved the challenge — no combination of plain headers can fix that, since
/// it's decided before the request's headers are even parsed. `Edge148` is the
/// closest available profile to the installed WebView2 runtime (currently in
/// the 150s) that solves Beatport's challenge for real; TLS/HTTP2 fingerprints
/// stay stable across nearby point releases of the same engine generation, so
/// this doesn't need to track the exact installed version the way the old
/// header-only approach did.
///
/// wreq has no timeout by default, and a chart fetch is awaited by the app's
/// worker loop — so one stalled connection (a Cloudflare tarpit, a captive
/// portal, a half-open socket) would freeze everything behind it indefinitely.
/// The total timeout covers connect through the end of the body read.
fn beatport_client() -> &'static wreq::Client {
    use std::sync::OnceLock;
    use std::time::Duration;
    static CLIENT: OnceLock<wreq::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        wreq::Client::builder()
            .emulation(Emulation::Edge148)
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(20))
            .build()
            .expect("failed to build Beatport TLS-impersonating HTTP client")
    })
}

/// A single Beatport chart entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BeatportTrack {
    pub rank: u32,
    pub name: String,
    pub artists: String,
    #[serde(default)]
    pub label: String,
}

/// All supported Beatport genre slugs (name → slug). The empty slug is "Overall".
pub fn genre_list() -> Vec<(&'static str, &'static str)> {
    vec![
        ("Overall", ""),
        ("140 / Deep Dubstep / Grime", "140-deep-dubstep-grime/95"),
        ("African", "african/102"),
        ("Afro House", "afro-house/89"),
        ("Amapiano", "amapiano/98"),
        ("Ambient / Experimental", "ambient-experimental/100"),
        ("Bass / Club", "bass-club/85"),
        ("Bass House", "bass-house/91"),
        ("Brazilian Funk", "brazilian-funk/101"),
        ("Breaks / Breakbeat / UK Bass", "breaks-breakbeat-uk-bass/9"),
        ("Caribbean", "caribbean/103"),
        ("Country", "country/104"),
        ("Dance / Pop", "dance-pop/39"),
        ("Deep House", "deep-house/12"),
        ("DJ Tools", "dj-tools/16"),
        ("Downtempo", "downtempo/63"),
        ("Drum & Bass", "drum-bass/1"),
        ("Dubstep", "dubstep/18"),
        ("Electro (Classic / Detroit / Modern)", "electro-classic-detroit-modern/94"),
        ("Electronica", "electronica/3"),
        ("Funky House", "funky-house/81"),
        ("Hard Dance / Hardcore / Neo Rave", "hard-dance-hardcore-neo-rave/8"),
        ("Hard Techno", "hard-techno/2"),
        ("Hip-Hop", "hip-hop/105"),
        ("House", "house/5"),
        ("Indie Dance", "indie-dance/37"),
        ("Jackin House", "jackin-house/97"),
        ("Latin", "latin/106"),
        ("Latin Electronic", "latin-electronic/111"),
        ("Mainstage", "mainstage/96"),
        ("Melodic House & Techno", "melodic-house-techno/90"),
        ("Minimal / Deep Tech", "minimal-deep-tech/14"),
        ("Nu Disco / Disco", "nu-disco-disco/50"),
        ("Organic House", "organic-house/93"),
        ("Pop", "pop/107"),
        ("Progressive House", "progressive-house/15"),
        ("Psy-Trance", "psy-trance/13"),
        ("R&B", "rb/108"),
        ("Rock", "rock/109"),
        ("Tech House", "tech-house/11"),
        ("Techno (Peak Time / Driving)", "techno-peak-time-driving/6"),
        ("Techno (Raw / Deep / Hypnotic)", "techno-raw-deep-hypnotic/92"),
        ("Trance (Main Floor)", "trance-main-floor/7"),
        ("Trance (Raw / Deep / Hypnotic)", "trance-raw-deep-hypnotic/99"),
        ("Trap / Future Bass", "trap-future-bass/38"),
        ("UK Garage / Bassline", "uk-garage-bassline/86"),
    ]
}

pub fn chart_url(genre_slug: &str, chart_kind: &str) -> Result<String, String> {
    let route = match chart_kind {
        "" | "tracks" => "top-100",
        "hype" => "hype-100",
        "releases" => "top-100-releases",
        _ => return Err("Unknown Beatport chart".to_string()),
    };
    if genre_slug.is_empty() {
        Ok(format!("https://www.beatport.com/{route}"))
    } else {
        Ok(format!("https://www.beatport.com/genre/{genre_slug}/{route}"))
    }
}

/// Fetch and parse a Beatport chart for a genre slug + chart kind (tracks/hype/releases).
pub async fn fetch_chart(
    genre_slug: &str,
    chart_kind: &str,
) -> Result<Vec<BeatportTrack>, String> {
    let url = chart_url(genre_slug, chart_kind)?;
    let reused_cookie = load_cookie_header();
    let mut req = beatport_client().get(&url);
    if let Some(cookie) = reused_cookie.as_deref() {
        req = req.header("Cookie", cookie);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| format!("Beatport fetch error: {e}"))?;

    let debug = std::env::var("LIGHTIFY_DEBUG_BEATPORT").is_ok();
    let status = resp.status();
    let is_challenge = resp
        .headers()
        .get("cf-mitigated")
        .and_then(|value| value.to_str().ok())
        .map(|value| value.eq_ignore_ascii_case("challenge"))
        .unwrap_or(false);
    if debug {
        eprintln!("[beatport debug] status={status}");
        for (k, v) in resp.headers() {
            eprintln!("[beatport debug] header: {k}: {}", v.to_str().unwrap_or("<binary>"));
        }
    }

    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        if debug {
            eprintln!("[beatport debug] body:\n{body}");
        }
        if status == wreq::StatusCode::FORBIDDEN && is_challenge {
            return Err(if reused_cookie.is_some() {
                "Beatport is still blocking chart requests \u{2014} the saved session has likely expired. Open the chart in your browser (OPEN \u{2197}).".into()
            } else {
                "Beatport is blocking automated chart requests right now. Open the chart in your browser (OPEN \u{2197}).".into()
            });
        }
        return Err(format!("Beatport returned HTTP {status}"));
    }

    let html = resp.text().await.map_err(|e| format!("Beatport read error: {e}"))?;
    parse_beatport_html(&html)
}

/// Parse Beatport HTML for track data using multiple strategies.
fn parse_beatport_html(html: &str) -> Result<Vec<BeatportTrack>, String> {
    // Strategy 1: __NEXT_DATA__ JSON blob.
    let re_next = Regex::new(r#"(?s)<script id="__NEXT_DATA__"[^>]*>(.*?)</script>"#).unwrap();
    if let Some(cap) = re_next.captures(html) {
        if let Ok(data) = serde_json::from_str::<Value>(&cap[1]) {
            let tracks = extract_from_next_data(&data);
            if !tracks.is_empty() {
                return Ok(tracks);
            }
        }
    }

    // Strategy 2: JSON arrays with track-like data in script tags.
    //
    // The regex only locates where an array *starts*; serde_json then reads exactly
    // one complete value from there. Capturing the array with a lazy regex instead
    // (`\[[\s\S]*?\]`) stopped at the first nested `]` — any track with an
    // `artists: [...]` list — so the capture was never valid JSON.
    for pattern in &[r#""tracks"\s*:\s*\["#, r#""results"\s*:\s*\["#] {
        let re = Regex::new(pattern).unwrap();
        for m in re.find_iter(html) {
            // `m.end() - 1` is the opening `[` itself.
            let rest = &html[m.end() - 1..];
            let parsed = serde_json::Deserializer::from_str(rest).into_iter::<Vec<Value>>().next();
            if let Some(Ok(arr)) = parsed {
                let tracks = extract_track_array(&arr);
                if !tracks.is_empty() {
                    return Ok(tracks);
                }
            }
        }
    }

    // Strategy 3: ld+json structured data. `(?s)` so `.` spans newlines — these
    // blocks are routinely pretty-printed across many lines.
    let re_ld = Regex::new(r#"(?s)<script type="application/ld\+json">(.*?)</script>"#).unwrap();
    for cap in re_ld.captures_iter(html) {
        if let Ok(ld) = serde_json::from_str::<Value>(&cap[1]) {
            let items = if ld.is_array() {
                ld.as_array().cloned().unwrap_or_default()
            } else {
                ld["itemListElement"].as_array().cloned().unwrap_or_default()
            };
            let mut tracks = Vec::new();
            for (i, it) in items.iter().enumerate() {
                let obj = it.get("item").unwrap_or(it);
                let t = obj.get("track").unwrap_or(obj);
                if let Some(name) = t["name"].as_str() {
                    let artist = if let Some(by) = t.get("byArtist") {
                        by["name"].as_str().unwrap_or("").to_string()
                    } else {
                        String::new()
                    };
                    tracks.push(BeatportTrack {
                        rank: (i + 1) as u32,
                        name: name.to_string(),
                        artists: artist,
                        label: String::new(),
                    });
                }
            }
            if tracks.len() >= 5 {
                return Ok(truncate(tracks));
            }
        }
    }

    Err("No tracks found - Beatport may have changed their page structure.".into())
}

/// Extract tracks from `__NEXT_DATA__` JSON.
fn extract_from_next_data(data: &Value) -> Vec<BeatportTrack> {
    let pp = &data["props"]["pageProps"];

    // Primary: dehydratedState -> queries -> state -> data -> results.
    if let Some(queries) = pp["dehydratedState"]["queries"].as_array() {
        for q in queries {
            if let Some(arr) = q["state"]["data"]["results"].as_array() {
                let tracks = extract_track_array(arr);
                if !tracks.is_empty() {
                    return tracks;
                }
            }
        }
    }

    for key in &["tracks", "results", "chart", "topTracks"] {
        if let Some(arr) = pp[key].as_array() {
            let tracks = extract_track_array(arr);
            if !tracks.is_empty() {
                return tracks;
            }
        }
    }

    deep_search(data, 0)
}

/// Recursively search JSON for an array of 10+ track-like objects.
fn deep_search(obj: &Value, depth: usize) -> Vec<BeatportTrack> {
    if depth > 8 {
        return vec![];
    }
    if let Some(arr) = obj.as_array() {
        if arr.len() >= 10 {
            let tracks = extract_track_array(arr);
            if !tracks.is_empty() {
                return tracks;
            }
        }
    }
    if let Some(map) = obj.as_object() {
        for v in map.values() {
            let r = deep_search(v, depth + 1);
            if !r.is_empty() {
                return r;
            }
        }
    }
    vec![]
}

/// Convert a JSON array of track-like objects into `BeatportTrack` structs.
pub fn extract_track_array(arr: &[Value]) -> Vec<BeatportTrack> {
    let mut tracks = Vec::new();
    for (i, item) in arr.iter().enumerate() {
        let obj = match item.as_object() {
            Some(o) => o,
            None => continue,
        };
        let name = obj
            .get("name")
            .or_else(|| obj.get("title"))
            .or_else(|| obj.get("track_name"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if name.is_empty() {
            continue;
        }
        let artists = match obj.get("artists").or_else(|| obj.get("artist")) {
            Some(Value::Array(arr)) => arr
                .iter()
                .filter_map(|a| {
                    if let Some(obj) = a.as_object() {
                        obj.get("name").and_then(|n| n.as_str()).map(|s| s.to_string())
                    } else {
                        a.as_str().map(|s| s.to_string())
                    }
                })
                .collect::<Vec<_>>()
                .join(", "),
            Some(Value::Object(o)) => o.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string(),
            Some(Value::String(s)) => s.clone(),
            _ => String::new(),
        };
        let label = obj
            .get("release")
            .and_then(|r| r.get("label"))
            .or_else(|| obj.get("label"))
            .and_then(|l| {
                if let Some(o) = l.as_object() {
                    o.get("name").and_then(|n| n.as_str()).map(|s| s.to_string())
                } else {
                    l.as_str().map(|s| s.to_string())
                }
            })
            .unwrap_or_default();
        tracks.push(BeatportTrack { rank: (i + 1) as u32, name, artists, label });
    }
    if tracks.len() >= 5 {
        truncate(tracks)
    } else {
        vec![]
    }
}

/// Strip Beatport mix suffixes that hurt Spotify search matching.
pub fn clean_bp_name(name: &str) -> String {
    let re = Regex::new(
        r"(?i)\s*\((Extended|Original|Club|Radio|Dub|Instrumental|VIP|Short)(\s+(Mix|Edit|Remix|Version|Dub))?\)",
    )
    .unwrap();
    let cleaned = re.replace_all(name, "").trim().to_string();
    if cleaned.is_empty() {
        name.to_string()
    } else {
        cleaned
    }
}

fn truncate(mut tracks: Vec<BeatportTrack>) -> Vec<BeatportTrack> {
    tracks.truncate(100);
    tracks
}

// ── Spotify match scoring (ported from the host's beatport_match_track) ───────

/// Lowercase, alphanumeric-only, space-collapsed form for fuzzy comparison.
///
/// Unicode-aware on purpose: the host's ASCII-only version dropped every
/// non-ASCII letter, so a Japanese or Cyrillic title normalized to `""` (never
/// matched at all) and an accented one split mid-word ("Café Del Mar" →
/// "caf del mar"). Common Latin accents are also folded to their base letter, so
/// Beatport's "Tiësto" still matches a Spotify "Tiesto" and vice versa.
pub fn normalize_match_text(value: &str) -> String {
    let mut normalized = String::with_capacity(value.len());
    let mut last_was_space = true;
    for lower in value.chars().flat_map(char::to_lowercase) {
        // Combining diacritics (decomposed "e\u{301}", or the dot `to_lowercase`
        // leaves after Turkish 'İ') belong to the previous letter — drop them
        // rather than letting them split the word like punctuation would.
        if ('\u{0300}'..='\u{036F}').contains(&lower) {
            continue;
        }
        if lower.is_alphanumeric() {
            match fold_latin_accent(lower) {
                Some(base) => normalized.push_str(base),
                None => normalized.push(lower),
            }
            last_was_space = false;
        } else if !last_was_space {
            normalized.push(' ');
            last_was_space = true;
        }
    }
    normalized.trim().to_string()
}

/// ASCII base form of a common accented lowercase Latin letter (Western, Central
/// European, Nordic and Turkish sets). A small table rather than full Unicode
/// decomposition, to avoid pulling in a normalization crate for match scoring.
fn fold_latin_accent(ch: char) -> Option<&'static str> {
    Some(match ch {
        'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ā' | 'ă' | 'ą' => "a",
        'æ' => "ae",
        'ç' | 'ć' | 'č' => "c",
        'ď' | 'đ' => "d",
        'è' | 'é' | 'ê' | 'ë' | 'ē' | 'ė' | 'ę' | 'ě' => "e",
        'ğ' => "g",
        'ì' | 'í' | 'î' | 'ï' | 'ī' | 'į' | 'ı' => "i",
        'ł' | 'ľ' | 'ĺ' => "l",
        'ñ' | 'ń' | 'ň' => "n",
        'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'ō' | 'ő' => "o",
        'œ' => "oe",
        'ŕ' | 'ř' => "r",
        'ś' | 'š' | 'ş' | 'ș' => "s",
        'ß' => "ss",
        'ť' | 'ţ' | 'ț' => "t",
        'ù' | 'ú' | 'û' | 'ü' | 'ū' | 'ů' | 'ű' | 'ų' => "u",
        'ý' | 'ÿ' => "y",
        'ź' | 'ż' | 'ž' => "z",
        'þ' => "th",
        'ð' => "d",
        _ => return None,
    })
}

pub fn normalize_artist_list(artists: &str) -> Vec<String> {
    artists
        .split(',')
        .map(normalize_match_text)
        .filter(|artist| !artist.is_empty())
        .collect()
}

fn token_overlap_score(left: &str, right: &str) -> i32 {
    let left_tokens: std::collections::HashSet<&str> = left.split_whitespace().collect();
    let right_tokens: std::collections::HashSet<&str> = right.split_whitespace().collect();
    if left_tokens.is_empty() || right_tokens.is_empty() {
        return 0;
    }
    let overlap = left_tokens.intersection(&right_tokens).count() as i32;
    let scale = left_tokens.len().max(right_tokens.len()) as i32;
    (overlap * 100) / scale.max(1)
}

/// Score a Spotify candidate against a normalized Beatport (name, artists). The
/// host accepts the best candidate only when this is ≥ 160 (see `MATCH_THRESHOLD`).
pub fn score_beatport_track_match(target_name: &str, target_artists: &[String], track: &Track) -> i32 {
    let candidate_name = normalize_match_text(&track.name);
    if candidate_name.is_empty() || target_name.is_empty() {
        return 0;
    }
    let mut score = 0;
    if candidate_name == target_name {
        score += 500;
    } else if candidate_name.contains(target_name) || target_name.contains(&candidate_name) {
        score += 280;
    }
    score += token_overlap_score(target_name, &candidate_name) * 3;

    let candidate_artists = normalize_artist_list(&track.artists);
    if let Some(primary_artist) = target_artists.first() {
        if candidate_artists.iter().any(|artist| artist == primary_artist) {
            score += 180;
        } else if candidate_artists
            .iter()
            .any(|artist| artist.contains(primary_artist) || primary_artist.contains(artist))
        {
            score += 90;
        }
    }

    let mut exact_artist_hits = 0;
    for artist in target_artists {
        if candidate_artists.iter().any(|candidate| candidate == artist) {
            exact_artist_hits += 1;
            score += 70;
        } else if candidate_artists
            .iter()
            .any(|candidate| candidate.contains(artist) || artist.contains(candidate))
        {
            score += 30;
        }
    }
    if exact_artist_hits == 0 && !target_artists.is_empty() {
        score -= 120;
    }
    score
}

/// Minimum score at which a Spotify candidate is accepted as the match.
pub const MATCH_THRESHOLD: i32 = 160;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Track;

    fn track(name: &str, artists: &str) -> Track {
        Track {
            id: "x".into(),
            name: name.into(),
            artists: artists.into(),
            artist_ids: vec![],
            album: String::new(),
            album_id: String::new(),
            duration_ms: 0,
            uri: "spotify:track:x".into(),
            album_art: String::new(),
            added_at: None,
            is_playable: true,
        }
    }

    #[test]
    fn clean_strips_mix_suffix() {
        assert_eq!(clean_bp_name("Strobe (Extended Mix)"), "Strobe");
        assert_eq!(clean_bp_name("Around the World (Radio Edit)"), "Around the World");
        assert_eq!(clean_bp_name("Plain Title"), "Plain Title");
    }

    #[test]
    fn exact_match_passes_threshold() {
        let name = normalize_match_text("Strobe");
        let artists = normalize_artist_list("deadmau5");
        let s = score_beatport_track_match(&name, &artists, &track("Strobe", "deadmau5"));
        assert!(s >= MATCH_THRESHOLD, "exact match should pass, got {s}");
    }

    #[test]
    fn unrelated_track_fails_threshold() {
        let name = normalize_match_text("Strobe");
        let artists = normalize_artist_list("deadmau5");
        let s = score_beatport_track_match(&name, &artists, &track("A Totally Different Song", "Nobody"));
        assert!(s < MATCH_THRESHOLD, "unrelated track should fail, got {s}");
    }

    #[test]
    fn normalize_keeps_non_ascii_and_folds_accents() {
        assert_eq!(normalize_match_text("Café Del Mar (Remix)"), "cafe del mar remix");
        assert_eq!(normalize_match_text("Tiësto"), normalize_match_text("Tiesto"));
        assert_eq!(normalize_match_text("Cafe\u{301}"), "cafe");
        assert_eq!(normalize_match_text("夜に駆ける"), "夜に駆ける");
        assert_eq!(normalize_match_text("Кино — Группа крови"), "кино группа крови");

        let name = normalize_match_text("Группа крови");
        let artists = normalize_artist_list("Кино");
        let s = score_beatport_track_match(&name, &artists, &track("Группа крови", "Кино"));
        assert!(s >= MATCH_THRESHOLD, "non-ASCII exact match should pass, got {s}");
    }

    #[test]
    fn array_strategy_survives_nested_arrays() {
        let rows: Vec<String> = (1..=6)
            .map(|i| format!(r#"{{"name":"T{i}","artists":[{{"name":"A{i}"}}]}}"#))
            .collect();
        let html = format!(r#"<script>var x = {{"tracks": [{}], "other": 1}};</script>"#, rows.join(","));
        let tracks = parse_beatport_html(&html).expect("should parse");
        assert_eq!(tracks.len(), 6);
        assert_eq!(tracks[0].artists, "A1");
    }

    #[test]
    fn invalid_reused_cookies_are_rejected() {
        assert!(is_cookie_name("cf_clearance") && is_cookie_value("abc.DEF-123_x=="));
        assert!(!is_cookie_value("a\r\nInjected: 1"));
        assert!(!is_cookie_value("a;b"));
        assert!(!is_cookie_value("a b"));
        assert!(!is_cookie_name("bad name") && !is_cookie_name("a=b") && !is_cookie_name(""));
    }
}
