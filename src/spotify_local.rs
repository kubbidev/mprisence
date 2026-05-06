use crate::config::ConfigManager;
use crate::error::TemplateError;
use lofty::prelude::*;
use log::{debug, error, info, trace, warn};
use once_cell::sync::Lazy;
use regex::Regex;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use url::Url;
use walkdir::WalkDir;

static LOCAL_URL_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^https?://open\.spotify\.com/local/([^/]*)/([^/]*)/([^/]+)/(\d+)/?$").unwrap()
});

const SUPPORTED_EXTS: &[&str] = &[
    "mp3", "flac", "ogg", "oga", "opus", "m4a", "mp4", "wav", "aiff", "wma",
];

#[derive(Debug, Clone)]
pub struct Track {
    pub artist: String,
    pub album_artist: String,
    pub album: String,
    pub title: String,
    pub duration: u32, // seconds
    pub path: PathBuf,
}

#[derive(Debug, Default)]
pub struct Library {
    pub tracks: Vec<Track>,
}

static PUNCT_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"[^\w\s]").unwrap());
static WS_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\s+").unwrap());

fn normalize(s: &str) -> String {
    let lower = s.to_lowercase();
    let no_punct = PUNCT_RE.replace_all(&lower, "");
    let collapsed = WS_RE.replace_all(&no_punct, " ");
    collapsed.trim().to_string()
}

#[derive(Debug)]
pub struct SpotifyLocalMeta {
    pub artist: String,
    pub album: String,
    pub title: String,
    pub duration: u32,
}

pub fn parse_local_url(url: &str) -> Option<SpotifyLocalMeta> {
    let caps = LOCAL_URL_RE.captures(url)?;

    let decode_part = |s: &str| {
        urlencoding::decode(&s.replace('+', " "))
            .unwrap_or_else(|_| s.into())
            .to_string()
    };

    let meta = SpotifyLocalMeta {
        artist: decode_part(&caps[1]),
        album: decode_part(&caps[2]),
        title: decode_part(&caps[3]),
        duration: caps[4].parse().ok()?,
    };

    debug!(
        "Parsed Spotify local URL: artist='{}' album='{}' title='{}' duration={}s",
        meta.artist, meta.album, meta.title, meta.duration
    );

    Some(meta)
}

impl Library {
    pub fn find_match(
        &self,
        artist: &str,
        album: &str,
        title: &str,
        duration: u32,
    ) -> Option<&Track> {
        const DURATION_TOLERANCE: i32 = 2;

        let a = normalize(artist);
        let b = normalize(album);
        let t = normalize(title);

        let duration_known = duration > 0;

        debug!(
            "Searching library for: artist='{}' album='{}' title='{}' duration={}",
            artist,
            album,
            title,
            if duration_known {
                format!("{}s", duration)
            } else {
                "unknown".to_string()
            }
        );

        let mut best: Option<(i32, &Track)> = None;

        for tr in &self.tracks {
            if duration_known
                && (tr.duration as i32 - duration as i32).abs() > DURATION_TOLERANCE
            {
                continue;
            }

            let mut score = 0;

            if normalize(&tr.title) == t {
                score += 3;
            }
            if normalize(&tr.album) == b {
                score += 2;
            }
            if normalize(&tr.artist) == a || normalize(&tr.album_artist) == a {
                score += 2;
            }

            // Require strong enough match
            if score < 5 {
                continue;
            }

            trace!(
                "Candidate (score={}): '{}' by '{}' [{}] at {}",
                score,
                tr.title,
                tr.artist,
                tr.album,
                tr.path.display()
            );

            if best.is_none() || score > best.unwrap().0 {
                best = Some((score, tr));
            }
        }

        if let Some((score, tr)) = best {
            info!(
                "Matched Spotify local track (score={}): '{}' by '{}' -> {}",
                score,
                tr.title,
                tr.artist,
                tr.path.display()
            );
            Some(tr)
        } else {
            warn!(
                "No match found in library for: artist='{}' album='{}' title='{}' duration={}s",
                artist, album, title, duration
            );
            None
        }
    }

    pub fn new(config: &Arc<ConfigManager>) -> Result<Self, TemplateError> {
        let dirs = config.local_library_dirs();

        if dirs.is_empty() {
            debug!("No local library dirs configured, skipping indexing");
            return Ok(Self { tracks: Vec::new() });
        }

        info!("Initializing local library ({} dir(s))", dirs.len());
        let mut tracks = Vec::new();

        for dir in &dirs {
            Self::collect_from_dir(Path::new(dir), &mut tracks);
        }

        info!("Local library ready: {} tracks indexed", tracks.len());
        Ok(Self { tracks })
    }

    fn collect_from_dir(root: &Path, out: &mut Vec<Track>) {
        if !root.exists() {
            warn!("Music dir does not exist: {}", root.display());
            return;
        }

        info!("Scanning {} ...", root.display());
        let before = out.len();

        for entry in WalkDir::new(root).into_iter().filter_map(Result::ok) {
            let path = entry.path();

            if !path.is_file() {
                continue;
            }

            let Some(ext) = path
                .extension()
                .and_then(|e| e.to_str())
                .map(|s| s.to_lowercase())
            else {
                continue;
            };

            if !SUPPORTED_EXTS.contains(&ext.as_str()) {
                continue;
            }

            trace!("Reading tags: {}", path.display());

            match lofty::read_from_path(path) {
                Ok(tagged_file) => {
                    let properties = tagged_file.properties();
                    let duration = properties.duration().as_secs() as u32;

                    let primary_tag = tagged_file.primary_tag();

                    let artist = primary_tag
                        .and_then(|tag| tag.get_string(ItemKey::TrackArtist))
                        .unwrap_or("")
                        .to_string();

                    let album_artists = primary_tag
                        .and_then(|tag| tag.get_string(ItemKey::AlbumArtist))
                        .unwrap_or("")
                        .to_string();

                    let album = primary_tag
                        .and_then(|t| t.album())
                        .unwrap_or("".into())
                        .to_string();
                    let title = primary_tag
                        .and_then(|t| t.title())
                        .unwrap_or_else(|| {
                            path.file_stem()
                                .and_then(|s| s.to_str())
                                .unwrap_or("")
                                .into()
                        })
                        .to_string();

                    if artist.is_empty() || title.is_empty() {
                        debug!(
                            "Incomplete tags (artist='{}' title='{}'): {}",
                            artist,
                            title,
                            path.display()
                        );
                    }

                    out.push(Track {
                        artist: artist.clone(),
                        album_artist: if album_artists.is_empty() {
                            artist.clone()
                        } else {
                            album_artists
                        },
                        album,
                        title,
                        duration,
                        path: path.to_path_buf(),
                    });
                }
                Err(e) => {
                    error!("Failed to read tags for {}: {}", path.display(), e);
                }
            }
        }

        debug!("Scanned {}: {} tracks added", root.display(), out.len() - before);
    }
}

pub fn resolve_local_spotify_url(url: &str, library: &Library) -> Option<PathBuf> {
    let parsed = parse_local_url(url)?;
    let track = library.find_match(
        &parsed.artist,
        &parsed.album,
        &parsed.title,
        parsed.duration,
    )?;
    Some(track.path.clone())
}

pub fn resolve_to_file_url(url: &str, library: &Library) -> Option<String> {
    if !url.contains("open.spotify.com/local") {
        return None;
    }
    debug!("Resolving Spotify local URL: {}", url);
    let result = resolve_local_spotify_url(url, library)
        .and_then(|p| Url::from_file_path(p).ok())
        .map(|u| u.to_string());
    if result.is_none() {
        debug!("Could not resolve Spotify local URL to a file: {}", url);
    }
    result
}
