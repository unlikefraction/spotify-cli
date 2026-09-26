//! Spotify identifiers: bare ids, `spotify:` URIs and `open.spotify.com` links.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::{Error, Result};

/// The kind of Spotify item a URI names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// A song.
    Track,
    /// An album.
    Album,
    /// An artist.
    Artist,
    /// A playlist.
    Playlist,
    /// A podcast show.
    Show,
    /// A podcast episode.
    Episode,
}

impl Kind {
    /// Lowercase name used in URIs.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Track => "track",
            Self::Album => "album",
            Self::Artist => "artist",
            Self::Playlist => "playlist",
            Self::Show => "show",
            Self::Episode => "episode",
        }
    }

    /// Whether playing this item starts a context (a list of items) rather than one item.
    #[must_use]
    pub fn is_context(self) -> bool {
        matches!(
            self,
            Self::Album | Self::Artist | Self::Playlist | Self::Show
        )
    }

    /// All kinds, for help text.
    pub const ALL: [Kind; 6] = [
        Self::Track,
        Self::Album,
        Self::Artist,
        Self::Playlist,
        Self::Show,
        Self::Episode,
    ];
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Kind {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "track" | "tracks" | "song" | "songs" => Ok(Self::Track),
            "album" | "albums" => Ok(Self::Album),
            "artist" | "artists" => Ok(Self::Artist),
            "playlist" | "playlists" => Ok(Self::Playlist),
            "show" | "shows" | "podcast" | "podcasts" => Ok(Self::Show),
            "episode" | "episodes" => Ok(Self::Episode),
            other => Err(Error::invalid(
                format!("`{other}` is not a Spotify item kind."),
                "Use one of: track, album, artist, playlist, show, episode.",
            )),
        }
    }
}

/// A parsed Spotify item reference.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SpotifyUri {
    /// What the id names.
    pub kind: Kind,
    /// The base62 id (22 characters).
    pub id: String,
}

impl SpotifyUri {
    /// Builds a URI after validating the id.
    ///
    /// # Errors
    /// Returns `invalid_input` when the id is not 22 base62 characters (letters and digits).
    pub fn new(kind: Kind, id: &str) -> Result<Self> {
        validate_id(id)?;
        Ok(Self {
            kind,
            id: id.to_owned(),
        })
    }

    /// `spotify:<kind>:<id>`.
    #[must_use]
    pub fn uri(&self) -> String {
        format!("spotify:{}:{}", self.kind, self.id)
    }

    /// `https://open.spotify.com/<kind>/<id>`.
    #[must_use]
    pub fn url(&self) -> String {
        format!("https://open.spotify.com/{}/{}", self.kind, self.id)
    }

    /// Parses a URI, an open.spotify.com URL, or a bare id when `default_kind` is given.
    ///
    /// Accepted: `spotify:track:ID`, `spotify:user:x:playlist:ID` (legacy), `https://open.spotify.com/track/ID?si=…`,
    /// `https://open.spotify.com/intl-de/album/ID`, `ID` (with a default kind).
    ///
    /// # Errors
    /// Returns `invalid_input` explaining the accepted forms.
    pub fn parse(input: &str, default_kind: Option<Kind>) -> Result<Self> {
        let value = input.trim();
        if value.is_empty() {
            return Err(Error::invalid(
                "An empty Spotify reference was given.",
                "Pass a Spotify URI (spotify:track:<id>), an open.spotify.com link, or a bare id.",
            ));
        }
        if let Some(rest) = value.strip_prefix("spotify:") {
            let parts: Vec<&str> = rest.split(':').collect();
            // Legacy playlist form: spotify:user:<user>:playlist:<id>
            let (kind, id) = match parts.as_slice() {
                [kind, id] => (*kind, *id),
                ["user", _, kind, id] => (*kind, *id),
                _ => return Err(malformed(value)),
            };
            return Self::new(kind.parse()?, id);
        }
        if value.starts_with("http://") || value.starts_with("https://") {
            let url = url::Url::parse(value).map_err(|_| malformed(value))?;
            let host = url.host_str().unwrap_or_default();
            if host != "open.spotify.com" && host != "play.spotify.com" {
                return Err(Error::invalid(
                    format!("`{host}` is not a Spotify link host."),
                    "Use an https://open.spotify.com/<kind>/<id> link, a spotify:<kind>:<id> URI, or a bare id.",
                ));
            }
            let segments: Vec<&str> = url
                .path_segments()
                .map(|segments| segments.filter(|s| !s.is_empty()).collect())
                .unwrap_or_default();
            let segments: Vec<&str> = segments
                .into_iter()
                .skip_while(|segment| segment.starts_with("intl-") || *segment == "embed")
                .collect();
            return match segments.as_slice() {
                [kind, id, ..] if kind.parse::<Kind>().is_ok() => Self::new(kind.parse()?, id),
                ["user", _, "playlist", id, ..] => Self::new(Kind::Playlist, id),
                _ => Err(malformed(value)),
            };
        }
        match default_kind {
            Some(kind) => Self::new(kind, value),
            None => {
                validate_id(value)?;
                Err(Error::invalid(
                    format!("`{value}` is a bare id, but its kind is unknown."),
                    "Pass a full URI such as spotify:track:<id> or spotify:playlist:<id>, or an open.spotify.com link.",
                ))
            }
        }
    }
}

impl fmt::Display for SpotifyUri {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.uri())
    }
}

/// The forms [`SpotifyUri::parse`] accepts, for hints.
pub const ACCEPTED_FORMS: &str = "Accepted forms: spotify:<kind>:<id>, https://open.spotify.com/<kind>/<id>, or a bare 22-character id (with --type where the kind is not implied).";

/// Length of every Spotify id (base62).
pub const ID_LEN: usize = 22;

fn malformed(value: &str) -> Error {
    Error::invalid(
        format!("`{value}` is not a Spotify reference this CLI understands."),
        ACCEPTED_FORMS,
    )
}

/// Checks the id's shape locally, so a typo fails as `invalid_input` instead of reaching Spotify
/// (which answers 400 or 404).
fn validate_id(id: &str) -> Result<()> {
    if id.len() != ID_LEN || !id.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return Err(Error::invalid(
            format!(
                "`{id}` is not a Spotify id: ids are exactly {ID_LEN} letters and digits (base62)."
            ),
            format!(
                "{ACCEPTED_FORMS} Find ids with `spotify search '<query>'` or copy a share link."
            ),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_supported_form() {
        let id = "0BxE4FqsDD1Ot4YuBXwAPp";
        for input in [
            format!("spotify:track:{id}"),
            format!("https://open.spotify.com/track/{id}?si=abc"),
            format!("https://open.spotify.com/intl-de/track/{id}"),
            format!("https://open.spotify.com/embed/track/{id}"),
        ] {
            let parsed = SpotifyUri::parse(&input, None).expect("parses");
            assert_eq!(parsed.kind, Kind::Track);
            assert_eq!(parsed.id, id);
        }
        let bare = SpotifyUri::parse(id, Some(Kind::Album)).expect("bare");
        assert_eq!(bare.uri(), format!("spotify:album:{id}"));
        let legacy =
            SpotifyUri::parse("spotify:user:someone:playlist:37i9dQZF1DXcBWIGoYBM5M", None)
                .expect("legacy");
        assert_eq!(legacy.kind, Kind::Playlist);
    }

    #[test]
    fn rejects_garbage_with_guidance() {
        let error = SpotifyUri::parse("hello world", None).expect_err("not an id");
        assert_eq!(error.code, "invalid_input");
        assert!(
            error.message.contains("not a Spotify id"),
            "{}",
            error.message
        );
        let error =
            SpotifyUri::parse("0BxE4FqsDD1Ot4YuBXwAPp", None).expect_err("bare without kind");
        assert!(
            error.message.contains("kind is unknown"),
            "{}",
            error.message
        );
        assert!(SpotifyUri::parse("https://example.com/track/abc", None).is_err());
        assert!(SpotifyUri::parse("spotify:podcast:4rOoJ6Egrf8K2IrywzwOMk", None).is_ok());
        assert!(SpotifyUri::parse("spotify:track:ab-c", None).is_err());
    }

    #[test]
    fn rejects_malformed_ids_before_they_reach_spotify() {
        for (input, kind) in [
            ("garbage", Some(Kind::Track)),
            ("notatrack", Some(Kind::Track)),
            ("spotify:track:abc", None),
            ("https://open.spotify.com/album/78bpIziExqiI9qztvNFlQ", None),
            ("0BxE4FqsDD1Ot4YuBXwAPpX", Some(Kind::Track)),
            ("0BxE4FqsDD1Ot4YuBXwAP_", Some(Kind::Track)),
        ] {
            let error = SpotifyUri::parse(input, kind).expect_err(input);
            assert_eq!(error.code, "invalid_input", "{input}");
            assert!(error.hint.contains("spotify:<kind>:<id>"), "{input}");
        }
        // A well-formed id passes even when Spotify has no such item (that is `not_found`).
        assert!(SpotifyUri::parse("0000000000000000000000", Some(Kind::Playlist)).is_ok());
    }
}
