//! `spotify how "<question>"`: which command does what, answered offline.
//!
//! Every command (its path, aliases, summary, help, arguments, allowed values and examples, plus
//! the catalog's capabilities and keywords), every section of the bundled guides, every term the
//! guides define (`- **SLT** (…)`) and every row of the error-code tables is a document. A
//! question is tokenized (lowercase words, `30s`-style amounts kept together), stemmed lightly,
//! widened with a table of synonyms and scored with BM25 over weighted fields. No network, no
//! model: the same question always gets the same answer.

use std::collections::{HashMap, HashSet};

use serde_json::{Value, json};
use silicon_spotify_client::{Error, Result};

use silicon_spotify_client::store::CONFIG_KEYS;

use crate::Ctx;
use crate::catalog::{self, ErrorRow};

// ---------------------------------------------------------------------------- words

/// Words that carry no meaning in a question or a help text.
const STOPWORDS: &[&str] = &[
    "a",
    "about",
    "after",
    "all",
    "also",
    "am",
    "an",
    "and",
    "any",
    "are",
    "as",
    "at",
    "be",
    "been",
    "but",
    "by",
    "can",
    "cli",
    "command",
    "commands",
    "could",
    "did",
    "do",
    "does",
    "e",
    "each",
    "else",
    "etc",
    "for",
    "from",
    "g",
    "get",
    "got",
    "has",
    "have",
    "here",
    "how",
    "i",
    "if",
    "in",
    "into",
    "is",
    "it",
    "its",
    "just",
    "let",
    "make",
    "may",
    "me",
    "might",
    "more",
    "most",
    "much",
    "must",
    "my",
    "myself",
    "need",
    "of",
    "on",
    "one",
    "only",
    "or",
    "our",
    "out",
    "please",
    "shall",
    "should",
    "so",
    "some",
    "spotify",
    "such",
    "than",
    "that",
    "the",
    "their",
    "them",
    "then",
    "there",
    "these",
    "they",
    "this",
    "those",
    "through",
    "to",
    "too",
    "us",
    "use",
    "used",
    "using",
    "very",
    "want",
    "was",
    "way",
    "we",
    "were",
    "what",
    "when",
    "where",
    "which",
    "while",
    "who",
    "whose",
    "will",
    "with",
    "would",
    "you",
    "your",
    "something",
    "anything",
    "everything",
    "thing",
    "things",
    "stuff",
    "possible",
    "able",
    "show",
    "see",
    "display",
    "view",
    "right",
    // Contractions lose their apostrophe: "what's in my queue".
    "whats",
    "thats",
    "theres",
    "heres",
    "wheres",
    "hows",
    "whos",
    "lets",
    "im",
    "ive",
    "youre",
];

/// Words that turn the next word around: "a song that isn't playing".
const NEGATIONS: &[&str] = &[
    "not", "no", "isnt", "arent", "dont", "doesnt", "wont", "cant", "cannot", "without", "never",
    "nothing", "wasnt", "didnt",
];

/// Negations that, right after "why", start an inverted question: "why doesn't the lyrics command
/// show anything", "why can't I see my playlists". What follows is what fails (the subject), not
/// what the question denies, so they turn nothing around there.
const INVERTED: &[&str] = &[
    "isnt", "arent", "dont", "doesnt", "wont", "cant", "cannot", "wasnt", "didnt",
];

/// Whether `words[at]` is a negation that turns the next word around (see [`INVERTED`]).
fn negates(words: &[String], at: usize) -> bool {
    let word = words[at].as_str();
    NEGATIONS.contains(&word) && !(at > 0 && words[at - 1] == "why" && INVERTED.contains(&word))
}

/// "like" is a stopword in questions ("songs like this") but also a command.
const KEEP: &[&str] = &["like", "liked", "likes"];

/// Words people use → words the help uses. Both sides are stemmed when a question is read.
const SYNONYMS: &[(&str, &[&str])] = &[
    ("notify", &["trigger", "ting", "notification"]),
    ("notification", &["trigger", "ting"]),
    ("remind", &["trigger", "note", "ting"]),
    ("reminder", &["trigger", "note", "ting"]),
    ("alert", &["trigger", "ting"]),
    ("ping", &["trigger", "ting"]),
    ("tell", &["trigger", "ting"]),
    ("wake", &["trigger"]),
    ("before", &["remaining", "left"]),
    ("left", &["remaining"]),
    ("ends", &["end", "remaining"]),
    ("finish", &["end"]),
    ("finishes", &["end"]),
    ("over", &["end"]),
    ("done", &["end"]),
    ("halfway", &["elapsed", "50%"]),
    ("half", &["elapsed", "50%"]),
    ("shuffled", &["shuffle", "random"]),
    ("random", &["shuffle"]),
    ("randomly", &["shuffle", "random"]),
    ("mix", &["shuffle"]),
    ("favorite", &["liked", "like"]),
    ("favourite", &["liked", "like"]),
    ("heart", &["like", "liked"]),
    ("hearted", &["liked"]),
    ("loved", &["liked"]),
    ("saved", &["liked", "library"]),
    ("words", &["lyrics"]),
    ("sing", &["lyrics"]),
    ("karaoke", &["lyrics"]),
    ("skip", &["next"]),
    ("forward", &["next"]),
    ("back", &["previous"]),
    ("rewind", &["previous", "seek"]),
    ("restart", &["previous"]),
    ("louder", &["volume"]),
    ("quieter", &["volume"]),
    ("loud", &["volume"]),
    ("quiet", &["volume"]),
    ("sound", &["volume"]),
    ("mute", &["volume"]),
    ("signin", &["login", "auth"]),
    ("sign", &["login", "auth"]),
    ("log", &["login"]),
    ("authenticate", &["login", "auth"]),
    ("token", &["slt"]),
    ("slt", &["login", "token"]),
    ("permission", &["automation", "allow"]),
    ("allowed", &["permission", "automation"]),
    ("denied", &["permission", "automation"]),
    ("blocked", &["permission", "automation"]),
    ("privacy", &["permission", "automation"]),
    ("current", &["status", "now"]),
    ("now", &["status", "current"]),
    ("playing", &["status"]),
    ("upcoming", &["queue"]),
    ("later", &["queue"]),
    ("enqueue", &["queue", "add"]),
    ("episode", &["podcast"]),
    ("song", &["track"]),
    ("tune", &["track"]),
    ("music", &["track", "play"]),
    ("record", &["album"]),
    ("band", &["artist"]),
    ("singer", &["artist"]),
    ("speaker", &["devices", "connect"]),
    ("phone", &["devices", "connect"]),
    ("cast", &["devices", "connect"]),
    ("install", &["setup"]),
    ("missing", &["setup", "doctor"]),
    ("broken", &["doctor"]),
    ("fix", &["doctor"]),
    ("diagnose", &["doctor"]),
    ("working", &["doctor"]),
    ("works", &["doctor"]),
    ("fail", &["error", "doctor"]),
    ("failing", &["error", "doctor"]),
    ("error", &["code"]),
    ("stop", &["pause"]),
    ("continue", &["resume"]),
    ("unpause", &["resume"]),
    ("find", &["search"]),
    ("look", &["search"]),
    ("lookup", &["search"]),
    ("details", &["track", "info"]),
    ("info", &["track"]),
    ("who", &["artist", "track"]),
    ("autocomplete", &["completions"]),
    ("tab", &["completions"]),
    ("completion", &["completions"]),
    ("setting", &["config"]),
    ("preference", &["config"]),
    ("upgrade", &["update"]),
    ("bug", &["report"]),
    ("crash", &["report"]),
    ("open", &["launch"]),
    ("loop", &["repeat"]),
    ("jump", &["seek"]),
    ("activate", &["frontmost"]),
    ("foreground", &["frontmost"]),
    ("background", &["daemon", "launch"]),
    ("collection", &["library"]),
    ("webhook", &["ting", "trigger"]),
    ("middle", &["50%", "halfway"]),
    ("undo", &["unlike", "remove"]),
    ("cancel", &["remove"]),
    ("timeout", &["timeout"]),
    ("save", &["like"]),
    ("stuck", &["doctor"]),
    ("hang", &["doctor"]),
    ("hangs", &["doctor"]),
    ("hung", &["doctor"]),
    ("frozen", &["doctor"]),
    ("freeze", &["doctor"]),
    ("unresponsive", &["doctor", "unavailable"]),
    ("respond", &["doctor", "unavailable"]),
    ("offline", &["network", "unavailable"]),
    ("unreachable", &["unavailable"]),
    ("problem", &["doctor"]),
    // "make a playlist from another playlist": a new one.
    ("make", &["create", "new"]),
];

/// Words that mean trouble when negated: "the daemon is not running", "spotify isn't
/// responding". Their synonyms count in full, not as a negated word.
const NEGATED_SYNONYMS: &[(&str, &[&str])] = &[
    ("run", &["unavailable", "doctor"]),
    ("respond", &["unavailable", "doctor"]),
    ("answer", &["unavailable", "doctor"]),
    ("work", &["doctor"]),
    ("start", &["unavailable", "doctor"]),
    ("connect", &["unavailable", "doctor"]),
];

/// Words that say a question is about a failure without saying which: they count for little
/// next to the words that do ("error: daemon not running" is about the daemon).
/// "why" asks about a failure too, and says no more than these ("why does play take so long" is
/// about `play`).
const META: &[&str] = &["error", "cod", "problem", "issu", "wrong", "why"];

/// Words that say a question is about what a command prints or returns: with one of them, "returns"
/// is its output ("spotify play returns exit code 1"), not `testing exit`'s "Return to production".
const OUTPUT_WORDS: &[&str] = &[
    "error", "errors", "code", "codes", "exitcode", "output", "json", "value",
];

/// A verb whose particle can come a few words later: "turn the volume up".
struct Separable {
    verb: &'static str,
    /// Words that may stand between the verb and its particle.
    between: &'static [&'static str],
    /// Particle → the one word the pair means.
    particles: &'static [(&'static str, &'static str)],
}

const LOUDNESS: &[&str] = &[
    "it", "this", "that", "the", "volume", "music", "sound", "spotify",
];

const SEPARABLE: &[Separable] = &[
    Separable {
        verb: "turn",
        between: LOUDNESS,
        particles: &[("up", "louder"), ("down", "quieter")],
    },
    Separable {
        verb: "crank",
        between: LOUDNESS,
        particles: &[("up", "louder")],
    },
    Separable {
        verb: "set",
        between: &["it", "this", "everything", "things", "all", "spotify", "me"],
        particles: &[("up", "setup")],
    },
];

/// How a verb says Spotify.app comes to the front (see [`focus_phrase`]).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    /// "jumps to the front", "stays in front", "stays on top": `front` (or `top`) only.
    Front,
    /// "comes to the front", "comes forward", "brings Spotify up": `front` or `forward`, and
    /// `up` next to Spotify ("bring it up", not "bring up the lyrics").
    Forward,
    /// "pops up", "pops to the front".
    Up,
    /// "steals focus", "takes the focus", "switches focus".
    Grab,
}

/// Words that may stand between a focus verb and what it says: "jumps right to the front".
const FOCUS_BETWEEN: &[&str] = &[
    "to", "the", "in", "into", "it", "itself", "spotify", "app", "its", "my", "window", "up",
    "over", "back", "on", "all", "right", "always", "suddenly", "a",
];

/// A phrase that says Spotify.app comes to the front or takes the focus ("keep Spotify from
/// jumping to the front", "Spotify pops up", "it steals focus", "brings Spotify.app to the
/// front"): what `keep_spotify_in_background` is about, read as the one word `frontmost` (the
/// daemon's "foreground" is another thing). How many words it takes, from its verb.
/// `jump` alone stays a seek ("jump to 1:30"), and "the front of the queue" is the queue's.
fn focus_phrase(words: &[String]) -> Option<usize> {
    let is =
        |at: usize, wanted: &[&str]| words.get(at).is_some_and(|w| wanted.contains(&w.as_str()));
    const SPOTIFY: &[&str] = &["spotify", "it", "app", "window"];
    // "Spotify activates itself", "it activates the app": the one word says it (not "activate
    // the trigger").
    if is(0, &["activate", "activates", "activated", "activating"]) {
        let spotify = is(1, &["itself"]) || is(1, SPOTIFY) || (is(1, &["the"]) && is(2, SPOTIFY));
        return spotify.then_some(1);
    }
    let kind = match words.first()?.as_str() {
        "jump" | "jumps" | "jumped" | "jumping" | "stay" | "stays" | "stayed" | "staying"
        | "remain" | "remains" => Focus::Front,
        "come" | "comes" | "came" | "coming" | "bring" | "brings" | "brought" | "bringing" => {
            Focus::Forward
        }
        "pop" | "pops" | "popped" | "popping" => Focus::Up,
        "steal" | "steals" | "stole" | "stealing" | "take" | "takes" | "took" | "taking"
        | "grab" | "grabs" | "grabbing" | "switch" | "switches" | "switching" => Focus::Grab,
        _ => return None,
    };
    let bring = words[0].starts_with("br");
    for (gap, word) in words[1..].iter().take(5).enumerate() {
        let at = gap + 1;
        // "the front of the queue" is the queue's front, not the screen's.
        let of_a_list = is(at + 1, &["of"])
            && words[at + 1..]
                .iter()
                .take(4)
                .any(|w| matches!(w.as_str(), "queue" | "list" | "line" | "playlist"));
        let hit = match word.as_str() {
            "front" | "foreground" => kind != Focus::Grab && !of_a_list,
            "top" => kind == Focus::Front && !of_a_list,
            "forward" => matches!(kind, Focus::Forward | Focus::Up),
            // "bring Spotify up", "brings up the app"; not "bring up the lyrics".
            "up" => {
                kind == Focus::Up
                    || (bring
                        && ((gap > 0 && is(at - 1, SPOTIFY))
                            || is(at + 1, SPOTIFY)
                            || (is(at + 1, &["the"]) && is(at + 2, SPOTIFY))))
            }
            "focus" => kind == Focus::Grab,
            _ => false,
        };
        if hit {
            return Some(at + 1);
        }
        if !FOCUS_BETWEEN.contains(&word.as_str()) {
            return None;
        }
    }
    None
}

/// `30 seconds`, `2 minutes`, `50 percent` as the help writes them: `30s`, `2m`, `50%`.
fn unit(word: &str) -> Option<&'static str> {
    Some(match word {
        "s" | "sec" | "secs" | "second" | "seconds" => "s",
        "m" | "min" | "mins" | "minute" | "minutes" => "m",
        "ms" | "millisecond" | "milliseconds" => "ms",
        "percent" | "pct" => "%",
        _ => return None,
    })
}

/// Lowercase words; `50%`, `1:30` and `30s` stay whole, apostrophes vanish (`isn't` → `isnt`).
fn raw_words(text: &str) -> Vec<String> {
    let lower = text.to_lowercase().replace(['\'', '’'], "");
    let chars: Vec<char> = lower.chars().collect();
    let mut out: Vec<String> = Vec::new();
    let mut word = String::new();
    for (at, &c) in chars.iter().enumerate() {
        let after_digit = word.chars().last().is_some_and(|l| l.is_ascii_digit());
        let keep = c.is_alphanumeric()
            || (c == '%' && after_digit)
            || (c == ':' && after_digit && chars.get(at + 1).is_some_and(char::is_ascii_digit));
        if keep {
            word.push(c);
        } else if !word.is_empty() {
            out.push(std::mem::take(&mut word));
        }
    }
    if !word.is_empty() {
        out.push(word);
    }
    // `timed out` → `timeout`, `log out` → `logout`, `sign in` → `login`, `half over` →
    // `halfway`.
    fn pair(word: &str, next: &str) -> Option<&'static str> {
        Some(match (word, next) {
            ("time" | "times" | "timed", "out") => "timeout",
            ("log" | "sign", "out") => "logout",
            ("log" | "sign", "in" | "on") => "login",
            ("half", "over" | "way" | "through" | "done") => "halfway",
            // An exit code is no `testing exit`.
            ("exit" | "exits" | "exited", "code" | "codes" | "status") => "exitcode",
            ("set", "up") => "setup",
            _ => return None,
        })
    }
    let mut joined: Vec<String> = Vec::with_capacity(out.len());
    let mut at = 0;
    while at < out.len() {
        let word = &out[at];
        let next = out.get(at + 1).map(String::as_str);
        // `30 seconds` → `30s`.
        if word.chars().all(|c| c.is_ascii_digit())
            && let Some(suffix) = next.and_then(unit)
        {
            joined.push(format!("{word}{suffix}"));
            at += 2;
            continue;
        }
        if let Some(one) = next.and_then(|next| pair(word, next)) {
            joined.push(one.into());
            at += 2;
            continue;
        }
        // "jump to the next song" skips and "jump to the previous one" goes back: no seek.
        if matches!(word.as_str(), "jump" | "jumps" | "jumped" | "jumping") {
            let target = out[at + 1..].iter().take(4).find(|w| {
                !matches!(
                    w.as_str(),
                    "to" | "the" | "a" | "ahead" | "forward" | "back" | "straight" | "right"
                )
            });
            match target.map(String::as_str) {
                Some("next") => {
                    joined.push("skip".into());
                    at += 1;
                    continue;
                }
                Some("previous" | "prev") => {
                    at += 1;
                    continue;
                }
                _ => {}
            }
        }
        // "jumps to the front", "pops up", "steals focus" → `frontmost`: Spotify.app coming
        // forward, which `keep_spotify_in_background` is about.
        if let Some(taken) = focus_phrase(&out[at..]) {
            joined.push("frontmost".into());
            // The words between the verb and what it says (none for a word alone: "activates").
            joined.extend(
                out[at + 1..at + taken.max(2) - 1]
                    .iter()
                    .filter(|w| !matches!(w.as_str(), "up" | "window" | "app"))
                    .cloned(),
            );
            at += taken;
            continue;
        }
        // `turn it up`, `turn the volume down`, `set everything up` → `louder`, `quieter`,
        // `setup` (`up` alone means little: `up next`).
        if let Some(verb) = SEPARABLE.iter().find(|v| v.verb == word.as_str()) {
            let rest = &out[at + 1..];
            if let Some((gap, one)) = rest.iter().take(3).enumerate().find_map(|(gap, w)| {
                verb.particles
                    .iter()
                    .find(|(particle, _)| particle == w)
                    .map(|(_, one)| (gap, *one))
            }) && rest[..gap]
                .iter()
                .all(|w| verb.between.contains(&w.as_str()))
            {
                joined.push(one.into());
                joined.extend(rest[..gap].iter().cloned());
                at += gap + 2;
                continue;
            }
        }
        joined.push(word.clone());
        at += 1;
    }
    joined
}

/// A light stemmer: enough that `songs`/`song`, `playing`/`play`, `shuffled`/`shuffle` meet.
fn stem(word: &str) -> String {
    let mut w = word.to_owned();
    if w.chars().any(|c| c.is_ascii_digit()) || !w.is_ascii() {
        return w;
    }
    if w.len() > 4 && w.ends_with("ies") {
        w.truncate(w.len() - 3);
        w.push('y');
    } else if w.len() > 4 && w.ends_with("sses") {
        w.truncate(w.len() - 2);
    } else if w.len() > 3
        && w.ends_with('s')
        && !w.ends_with("ss")
        && !w.ends_with("us")
        && !w.ends_with("is")
    {
        w.pop();
    }
    for suffix in ["ing", "ed"] {
        if w.len() > suffix.len() + 2 && w.ends_with(suffix) {
            w.truncate(w.len() - suffix.len());
            // `stopping` → `stop`.
            let bytes = w.as_bytes();
            let last = bytes.len() - 1;
            if bytes.len() > 2
                && bytes[last] == bytes[last - 1]
                && !b"aeiouls".contains(&bytes[last])
            {
                w.pop();
            }
            break;
        }
    }
    if w.len() > 3 && w.ends_with('e') {
        w.pop();
    }
    w
}

fn stopword(word: &str) -> bool {
    !KEEP.contains(&word) && STOPWORDS.contains(&word)
}

/// `text` without the quoted words a search is given (`spotify play --search 'arctic monkeys
/// 505'`): a song's or artist's name in an example says nothing about what the command does, and
/// would make a question naming that song pick the command whose example happens to name it.
fn without_search_queries(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find("search") {
        let after = &rest[at + "search".len()..];
        let gap = after.len() - after.trim_start_matches([' ', '=']).len();
        let quote = after[gap..]
            .chars()
            .next()
            .filter(|c| matches!(c, '\'' | '"'));
        let end = quote.and_then(|q| after[gap + 1..].find(q).map(|e| gap + 1 + e + 1));
        match end {
            Some(end) if gap > 0 => {
                out.push_str(&rest[..at + "search".len()]);
                out.push_str(" '…'");
                rest = &after[end..];
            }
            _ => {
                out.push_str(&rest[..at + "search".len()]);
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// `text` without the single-quoted values its command lines are given (`playlist create 'Road
/// trip'`, `--note 'wrap up the call'`, `devices connect --name 'Kitchen speaker'`): a name or a
/// note someone typed says nothing about what the command does, and would make "add hotel
/// california to my road trip playlist" pick `playlist create`. A value opens after a space, `=`,
/// `(` or a backtick and closes before one, on the same line; JSON (`'{"search_limit": 5}'`) and jq
/// filters (`'.lines[]'`) stay, and an apostrophe inside a word (`Spotify's`) opens nothing.
fn without_quoted_values(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    let mut at = 0;
    while let Some(offset) = text[at..].find('\'') {
        let open = at + offset;
        at = open + 1;
        let opens = text[..open]
            .chars()
            .next_back()
            .is_none_or(|c| c.is_whitespace() || matches!(c, '=' | '(' | '`'));
        let Some(close) = text[open + 1..]
            .find(['\'', '\n'])
            .map(|e| open + 1 + e)
            .filter(|&close| text[close..].starts_with('\''))
        else {
            continue;
        };
        let value = &text[open + 1..close];
        let ends = text[close + 1..]
            .chars()
            .next()
            .is_none_or(|c| c.is_whitespace() || matches!(c, ')' | ',' | '`' | ';' | '.' | ':'));
        if opens
            && ends
            && !value.trim().is_empty()
            && value.chars().count() <= 80
            && !value.starts_with(['{', '.', '['])
        {
            out.push_str(&text[copied..=open]);
            out.push('…');
            copied = close;
            at = close + 1;
        }
    }
    out.push_str(&text[copied..]);
    out
}

/// The stems of what a document says: its quoted values left out (see [`without_quoted_values`]).
fn doc_terms(text: &str) -> Vec<(String, bool)> {
    marked_terms(&without_quoted_values(text))
}

/// The stems of a text with whether each was negated ("nothing has to play": `play` is).
fn marked_terms(text: &str) -> Vec<(String, bool)> {
    let mut out = Vec::new();
    let mut negated = false;
    let words = raw_words(&without_search_queries(text));
    for (at, word) in words.iter().enumerate() {
        if NEGATIONS.contains(&word.as_str()) {
            negated = negates(&words, at);
            continue;
        }
        if stopword(word) {
            continue;
        }
        out.push((stem(word), negated));
        negated = false;
    }
    out
}

/// The stems of a text, stopwords out.
fn terms(text: &str) -> Vec<String> {
    marked_terms(text).into_iter().map(|(t, _)| t).collect()
}

/// A term of a question.
#[derive(Clone, Debug, PartialEq)]
struct Term {
    term: String,
    weight: f64,
    /// The question negates it: "isn't playing".
    negated: bool,
}

/// A question's terms: its own words (a negated one weighs little), then their synonyms at a
/// lower weight.
fn query_terms(question: &str) -> Vec<Term> {
    fn add(out: &mut Vec<Term>, term: String, weight: f64, negated: bool) {
        match out.iter_mut().find(|t| t.term == term) {
            Some(t) if weight > t.weight => {
                t.weight = weight;
                t.negated = negated;
            }
            Some(_) => {}
            None => out.push(Term {
                term,
                weight,
                negated,
            }),
        }
    }
    let mut out = Vec::new();
    for (term, negated) in marked_terms(question) {
        let weight = if negated { 0.3 } else { 1.0 };
        add(&mut out, term, weight, negated);
    }
    // Synonyms of every word, stopwords too: "who sings this".
    let mut negated = false;
    // After "why doesn't", "why isn't": which word fails is not known, so each one's trouble
    // synonyms count ("why isn't the daemon running").
    let mut inverted = false;
    let words = raw_words(question);
    for (at, word) in words.iter().enumerate() {
        if NEGATIONS.contains(&word.as_str()) {
            negated = negates(&words, at);
            inverted |= !negated;
            continue;
        }
        let term = stem(word);
        // A stopword between a negation and its word keeps the negation for that word.
        let this_negated = negated && !stopword(word);
        if !stopword(word) {
            negated = false;
        }
        let weight = if this_negated { 0.18 } else { 0.6 };
        for (from, to) in SYNONYMS {
            // Inflections meet (`notifications`, `notification`), but a key that is itself
            // inflected (`playing`) is only that word: `play` is not `playing`.
            let inflected = from.ends_with("ing") || from.ends_with("ed");
            if *from == word.as_str() || (!inflected && stem(from) == term) {
                for extra in to.iter().flat_map(|synonym| terms(synonym)) {
                    add(&mut out, extra, weight, this_negated);
                }
            }
        }
        if this_negated || (inverted && !stopword(word)) {
            for (from, to) in NEGATED_SYNONYMS {
                if stem(from) == term {
                    for extra in to.iter().flat_map(|synonym| terms(synonym)) {
                        add(&mut out, extra, 0.45, false);
                    }
                }
            }
        }
    }
    // "error: daemon not running" is about the daemon, not about errors in general.
    let specific = out
        .iter()
        .any(|t| t.weight >= 1.0 && !META.contains(&t.term.as_str()));
    if specific {
        for t in &mut out {
            if META.contains(&t.term.as_str()) {
                t.weight = t.weight.min(0.35);
            }
        }
    }
    if words.iter().any(|w| OUTPUT_WORDS.contains(&w.as_str())) {
        for t in out.iter_mut().filter(|t| t.term == "return") {
            t.weight = t.weight.min(0.35);
        }
    }
    // "play returns exit code 1": the 1 is the exit code, not a value some command takes.
    if words.iter().any(|w| w == "exitcode") {
        for t in out
            .iter_mut()
            .filter(|t| t.term.len() == 1 && t.term.chars().all(|c| c.is_ascii_digit()))
        {
            t.weight = t.weight.min(0.35);
        }
    }
    out
}

// ---------------------------------------------------------------------------- index

#[derive(Clone, Debug)]
enum Kind {
    Command {
        path: String,
        /// How close a subcommand must score for this command to give way to it (a share of
        /// this command's score); `None` when it never does.
        yields: Option<f64>,
    },
    Guide {
        topic: &'static str,
        heading: String,
        blocks: Vec<String>,
    },
    Error(usize),
    /// A setting of `spotify config keys` (its index in `CONFIG_KEYS`): answered by `config set`.
    Setting(usize),
}

#[derive(Debug)]
struct Doc {
    kind: Kind,
    tf: HashMap<String, f64>,
    len: f64,
    /// Keyword phrases (two or more terms): a question holding all of one scores extra.
    phrases: Vec<Vec<String>>,
    /// A command's name, word by word: a question saying every word of it scores extra, so
    /// "log in" is `login` before `auth login` and "remove a trigger" is `trigger remove` before
    /// `trigger clear`. Said as written, not stemmed: "is the daemon running" is not `daemon run`.
    name: Vec<String>,
}

impl Doc {
    fn new(kind: Kind) -> Self {
        Self {
            kind,
            tf: HashMap::new(),
            len: 0.0,
            phrases: Vec::new(),
            name: Vec::new(),
        }
    }

    fn add(&mut self, text: &str, weight: f64) {
        for (term, _) in doc_terms(text) {
            *self.tf.entry(term).or_default() += weight;
            self.len += weight;
        }
    }
}

struct Index {
    root: clap::Command,
    docs: Vec<Doc>,
    /// Everything the help and the guides say, as written: what tells a product's own words in
    /// capitals ("Liked Songs") from a title a question gives ("OK Computer").
    corpus: String,
    df: HashMap<String, usize>,
    avg: [f64; 4],
    errors: Vec<ErrorRow>,
}

const fn kind_slot(kind: &Kind) -> usize {
    match kind {
        Kind::Command { .. } => 0,
        Kind::Guide { .. } => 1,
        Kind::Error(_) => 2,
        Kind::Setting(_) => 3,
    }
}

/// A guide split into sections at headings of level 1-3 outside code blocks:
/// (level, heading, body).
pub(crate) fn sections(content: &str) -> Vec<(usize, String, String)> {
    let mut out: Vec<(usize, String, String)> = Vec::new();
    let mut fenced = false;
    for line in content.lines() {
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
        }
        let level = line.chars().take_while(|c| *c == '#').count();
        if !fenced && (1..=3).contains(&level) && line[level..].starts_with(' ') {
            out.push((level, line[level..].trim().to_owned(), String::new()));
        } else if let Some((.., body)) = out.last_mut() {
            body.push_str(line);
            body.push('\n');
        }
    }
    out
}

/// Paragraphs, list items and table rows of a section: what an excerpt can quote.
fn blocks(body: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut fenced = false;
    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") {
            fenced = !fenced;
            if !current.trim().is_empty() {
                out.push(std::mem::take(&mut current));
            }
            continue;
        }
        if fenced {
            // A code line is a block of its own, in backticks; `\` continues it.
            if !current.trim().is_empty() {
                out.push(std::mem::take(&mut current));
            }
            if trimmed.is_empty() {
                continue;
            }
            match out.last_mut() {
                Some(last) if last.ends_with("\\`") => {
                    last.truncate(last.len() - 2);
                    last.truncate(last.trim_end().len());
                    last.push(' ');
                    last.push_str(trimmed);
                    last.push('`');
                }
                _ => out.push(format!("`{trimmed}`")),
            }
            continue;
        }
        let starts_block = trimmed.is_empty()
            || trimmed.starts_with("- ")
            || trimmed.starts_with("| ")
            || (trimmed.chars().next().is_some_and(|c| c.is_ascii_digit())
                && trimmed.contains(". "));
        if starts_block && !current.trim().is_empty() {
            out.push(std::mem::take(&mut current));
        }
        if trimmed.starts_with("| ---") {
            // The row above was the table's header: column names are no excerpt.
            if out.last().is_some_and(|b| b.starts_with("| ")) {
                out.pop();
            }
            current.clear();
            continue;
        }
        if trimmed.is_empty() {
            current.clear();
            continue;
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(trimmed);
    }
    if !current.trim().is_empty() {
        out.push(current);
    }
    out
}

/// The term a block defines: `- **SLT** (short-lived token): …` defines `SLT`.
fn defined_term(block: &str) -> Option<&str> {
    let rest = block.strip_prefix("- **")?;
    let end = rest.find("**")?;
    Some(&rest[..end])
}

fn build() -> Index {
    let mut docs = Vec::new();
    let mut corpus = String::new();
    let mut root = crate::args::command();
    root.build();
    for (path, command) in catalog::walk(&root) {
        for text in [
            command.get_about(),
            command.get_long_about(),
            command.get_after_help(),
        ]
        .into_iter()
        .flatten()
        .map(ToString::to_string)
        .chain(
            command
                .get_arguments()
                .filter_map(|a| a.get_help().map(ToString::to_string)),
        ) {
            corpus.push_str(&text);
            corpus.push('\n');
        }
        if path.is_empty() {
            continue;
        }
        // A command that cannot run without a subcommand gives way to one that matches nearly as
        // well. One whose bare form is just a listing (`queue`, `devices`) gives way only to one
        // that matches about as well. One that does something of its own (`login <SLT>`) never.
        let own_args = command
            .get_arguments()
            .any(|a| !a.is_global_set() && !matches!(a.get_id().as_str(), "help" | "version"));
        let yields = match (
            command.has_subcommands(),
            command.is_subcommand_required_set(),
        ) {
            (true, true) => Some(0.7),
            (true, false) if !own_args => Some(0.9),
            _ => None,
        };
        let mut doc = Doc::new(Kind::Command {
            path: path.clone(),
            yields,
        });
        doc.add(&path, 3.0);
        doc.name = path.split(' ').map(str::to_owned).collect();
        for alias in command.get_visible_aliases() {
            doc.add(alias, 3.0);
        }
        if let Some(about) = command.get_about() {
            doc.add(&about.to_string(), 2.5);
        }
        if let Some(entry) = catalog::entry(&path) {
            for text in entry.capabilities.iter().chain(entry.keywords) {
                doc.add(text, 2.0);
            }
            doc.phrases = entry
                .keywords
                .iter()
                .map(|k| terms(k))
                .filter(|t| t.len() > 1)
                .collect();
        }
        for arg in command.get_arguments().filter(|a| !a.is_global_set()) {
            if let Some(long) = arg.get_long() {
                doc.add(long, 1.5);
            }
            if let Some(help) = arg.get_help() {
                doc.add(&help.to_string(), 1.0);
            }
            for value in arg.get_possible_values() {
                doc.add(value.get_name(), 1.0);
            }
        }
        // `how`'s own examples are questions about everything else.
        if path != "how" {
            for text in [command.get_long_about(), command.get_after_help()]
                .into_iter()
                .flatten()
            {
                // The `Next:` label of the suggestions says nothing about skipping to the next
                // track; the commands it suggests do say what this one is used with.
                doc.add(&text.to_string().replace("Next: ", ""), 0.7);
            }
        }
        docs.push(doc);
    }
    for (topic, title, content) in crate::docs::TOPICS {
        corpus.push_str(title);
        corpus.push('\n');
        corpus.push_str(content);
        for (level, heading, body) in sections(content) {
            if level == 1 && body.trim().is_empty() {
                continue;
            }
            let blocks = blocks(&body);
            // Terms the guide defines are documents of their own: "what is an SLT".
            for block in &blocks {
                if let Some(term) = defined_term(block) {
                    let mut doc = Doc::new(Kind::Guide {
                        topic,
                        heading: heading.clone(),
                        blocks: vec![block.clone()],
                    });
                    doc.add(term, 4.0);
                    doc.add(block, 1.0);
                    docs.push(doc);
                }
            }
            let mut doc = Doc::new(Kind::Guide {
                topic,
                heading: heading.clone(),
                blocks,
            });
            doc.add(title, 1.0);
            doc.add(&heading, 2.5);
            doc.add(&body, 0.6);
            docs.push(doc);
        }
    }
    // Every setting, so "change the default number of search results" finds `search_limit`.
    for (at, (key, kind, _, description)) in CONFIG_KEYS.iter().enumerate() {
        let mut doc = Doc::new(Kind::Setting(at));
        doc.add(&key.replace('_', " "), 3.0);
        doc.add(description, 1.5);
        doc.add(&kind.replace('|', " "), 1.0);
        let keywords = SETTING_KEYWORDS
            .iter()
            .find(|(k, _)| k == key)
            .map_or(&[][..], |(_, words)| *words);
        for keyword in keywords {
            doc.add(keyword, 2.0);
        }
        doc.phrases = keywords
            .iter()
            .map(|k| terms(k))
            .filter(|t| t.len() > 1)
            .collect();
        docs.push(doc);
    }
    let errors = catalog::error_rows();
    for (at, row) in errors.iter().enumerate() {
        let mut doc = Doc::new(Kind::Error(at));
        for code in &row.codes {
            doc.add(&code.replace('_', " "), 2.5);
        }
        doc.add(&row.meaning, 1.0);
        doc.add(&row.fix, 0.5);
        doc.add(&row.section, 0.5);
        docs.push(doc);
    }
    let mut df: HashMap<String, usize> = HashMap::new();
    let mut totals = [(0.0, 0usize); 4];
    for doc in &docs {
        for term in doc.tf.keys() {
            *df.entry(term.clone()).or_default() += 1;
        }
        let slot = &mut totals[kind_slot(&doc.kind)];
        slot.0 += doc.len;
        slot.1 += 1;
    }
    #[allow(clippy::cast_precision_loss)]
    let avg = totals.map(|(sum, n)| if n == 0 { 1.0 } else { sum / n as f64 });
    Index {
        root,
        docs,
        corpus,
        df,
        avg,
        errors,
    }
}

const K1: f64 = 1.2;
/// Length normalization per kind: commands, guide sections, error rows, settings. A command's help
/// is long because it says more, not because it matches by chance.
const B: [f64; 4] = [0.3, 0.75, 0.5, 0.5];

impl Index {
    #[allow(clippy::cast_precision_loss)]
    fn idf(&self, term: &str) -> f64 {
        let n = self.docs.len() as f64;
        let df = self.df.get(term).copied().unwrap_or(0) as f64;
        (1.0 + (n - df + 0.5) / (df + 0.5)).ln()
    }

    /// BM25 over the weighted fields, scaled by how much of the question the document covers,
    /// plus the keyword phrases the question holds.
    fn score(&self, doc: &Doc, query: &[Term], words: &HashSet<String>) -> f64 {
        let avg = self.avg[kind_slot(&doc.kind)];
        let b = B[kind_slot(&doc.kind)];
        let mut score = 0.0;
        let mut covered = 0.0;
        let mut total = 0.0;
        for q in query {
            // A word no document has (a song's name, a typo) says nothing about which fits best.
            if !self.df.contains_key(&q.term) {
                continue;
            }
            let idf = self.idf(&q.term);
            total += q.weight * idf;
            let Some(tf) = doc.tf.get(&q.term) else {
                continue;
            };
            covered += q.weight * idf;
            score += q.weight * idf * (tf * (K1 + 1.0)) / (tf + K1 * (1.0 - b + b * doc.len / avg));
        }
        if total <= 0.0 || score <= 0.0 {
            return 0.0;
        }
        let asked: HashSet<&str> = query
            .iter()
            .filter(|q| q.weight >= 0.5 && !q.negated)
            .map(|q| q.term.as_str())
            .collect();
        let phrases: f64 = doc
            .phrases
            .iter()
            .filter(|p| p.iter().all(|t| asked.contains(t.as_str())))
            .map(|p| p.iter().map(|t| self.idf(t)).sum::<f64>() * 0.5)
            .sum();
        // Only the question's own words name a command (not "phone" → `devices`).
        let said = |w: &String| words.contains(w) || words.contains(&format!("{w}s"));
        // A word no document is indexed by (a stopword: `commands`, `show`, `get`, `how`) would
        // weigh the most of all; in a question it is mostly just a word ("what commands change
        // the volume" is `volume`), so it adds nothing.
        let named = if !doc.name.is_empty() && doc.name.iter().all(said) {
            doc.name
                .iter()
                .filter(|w| !stopword(w))
                .map(|w| self.idf(&stem(w)))
                .sum::<f64>()
                * 0.5
        } else {
            0.0
        };
        score * (0.5 + covered / total) + phrases + named
    }

    /// How well a short text (an example, a paragraph) matches. A term the question negates
    /// counts against a text that asserts it, and for one that negates it too.
    fn text_score(&self, text: &str, query: &[Term]) -> f64 {
        let marked = doc_terms(text);
        let asserted: HashSet<&str> = marked
            .iter()
            .filter(|(_, n)| !n)
            .map(|(t, _)| t.as_str())
            .collect();
        let denied: HashSet<&str> = marked
            .iter()
            .filter(|(_, n)| *n)
            .map(|(t, _)| t.as_str())
            .collect();
        let mut score = 0.0;
        for q in query {
            let value = q.weight * self.idf(&q.term);
            let term = q.term.as_str();
            score += match (q.negated, asserted.contains(term), denied.contains(term)) {
                (true, _, true) | (false, true, _) => value,
                (true, true, false) => -value,
                (false, false, true) => value * 0.5,
                _ => 0.0,
            };
        }
        score
    }
}

// ---------------------------------------------------------------------------- answer

/// Words that say the question is about a failure.
const TROUBLE: &[&str] = &[
    "why",
    "error",
    "errors",
    "fail",
    "fails",
    "failed",
    "failing",
    "denied",
    "refused",
    "broken",
    "wrong",
    "code",
    "exit",
    "working",
    "timeout",
    "cannot",
    "cant",
    "doesnt",
    "wont",
    "stuck",
    "hang",
    "hangs",
    "hung",
    "frozen",
    "problem",
    "responding",
    "unresponsive",
];

/// Trouble words that alone only say what a question is about: "which errors can it return",
/// "what do the exit codes mean".
const GENERIC_TROUBLE: &[&str] = &["error", "errors", "code", "exit"];

/// Stems that say nothing about which error a question is about ("which errors can a command's
/// arguments return"): an error row matching only these is no answer.
const ERROR_GENERIC: &[&str] = &[
    "argument", "exit", "exitcod", "return", "command", "mean", "messag", "possibl", "kind", "typ",
    "list", "giv", "happen", "json", "output",
    // What trouble words stand for ("won't work" → doctor): most rows' fix names `spotify doctor`.
    "doctor",
];

/// Most commands `spotify how` shows.
const LIMIT_MAX: usize = 10;

/// A guide heading as a shell word: `'What spotify queue shows'`.
fn shell_quote(text: &str) -> String {
    if text
        .chars()
        .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
    {
        text.to_owned()
    } else {
        format!("'{}'", text.replace('\'', r"'\''"))
    }
}

/// Markdown made plain for an excerpt; a table row's cells are joined with ` · `.
fn plain(text: &str) -> String {
    let text = match text.strip_prefix("| ") {
        Some(row) => row.trim_end_matches(" |").replace(" | ", " · "),
        None => text.trim_start_matches("- ").to_owned(),
    };
    let text = text.replace("**", "").replace('`', "");
    silicon_spotify_client::model::truncate(&text, 240)
}

fn round(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

/// How well an example fits the question. What a pipe does after `spotify` (`| jq .a.b`) says
/// nothing about it; examples that run the command itself come before those that run another
/// one; plain examples come before `--json` ones unless the question is about JSON.
fn example_score(
    index: &Index,
    path: &str,
    example: &catalog::Example,
    query: &[Term],
    runs: Option<&str>,
) -> f64 {
    let command = example
        .command
        .split(" | ")
        .flat_map(|segment| segment.split(" && "))
        .find(|segment| segment.trim_start().starts_with("spotify "))
        .unwrap_or(&example.command)
        .trim();
    // `spotify play …` runs `play`; `spotify playlist …` does not.
    let own_path = format!("spotify {path}");
    let own = command == own_path || command.starts_with(&format!("{own_path} "));
    // Another command's name is about that command: `spotify track` under `like` matches
    // "save this track" but saves nothing.
    let words = match runs {
        Some(other) if !own => command
            .strip_prefix(&format!("spotify {other}"))
            .unwrap_or(command),
        _ => command,
    };
    let text = format!(
        "{words} {}",
        example.description.clone().unwrap_or_default()
    );
    let mut score = index.text_score(&text, query);
    if own {
        score += 1.0;
    }
    let scripted = query.iter().any(|q| {
        matches!(
            q.term.as_str(),
            "json" | "jq" | "script" | "agent" | "pars" | "machin" | "readabl"
        )
    });
    if !scripted && example.command.contains("--json") {
        score *= 0.6;
    }
    score
}

/// The command an example runs (`queue add` for `spotify queue add --search 'x'`): the longest
/// command path its first `spotify` command starts with.
fn example_path<'a>(tree: &'a [(String, &clap::Command)], example: &str) -> Option<&'a str> {
    let command = example
        .split(" | ")
        .flat_map(|segment| segment.split(" && "))
        .map(str::trim)
        .find(|segment| segment.starts_with("spotify "))?;
    let words: Vec<&str> = command.split_whitespace().skip(1).collect();
    tree.iter()
        .map(|(path, _)| path.as_str())
        .filter(|path| {
            let parts: Vec<&str> = path.split(' ').collect();
            !path.is_empty() && words.len() >= parts.len() && words[..parts.len()] == parts[..]
        })
        .max_by_key(|path| path.len())
}

fn command_answer(
    index: &Index,
    path: &str,
    score: f64,
    query: &[Term],
    first: bool,
    made: &[catalog::Example],
    reading: bool,
) -> Value {
    let tree = catalog::walk(&index.root);
    let command = tree.iter().find(|(p, _)| p == path).map(|(_, c)| *c);
    let about = command
        .and_then(clap::Command::get_about)
        .map(ToString::to_string);
    // A command with subcommands shows the example that fits best among theirs too: "tell me
    // when the song changes" is answered by a `trigger add --change` line.
    let mut pool = command.map(catalog::examples).unwrap_or_default();
    for sub in command
        .into_iter()
        .flat_map(clap::Command::get_subcommands)
        .filter(|s| s.get_name() != "help")
    {
        for example in catalog::examples(sub) {
            if !pool.iter().any(|e| e.command == example.command) {
                pool.push(example);
            }
        }
    }
    let mut examples: Vec<(f64, usize, catalog::Example)> = pool
        .into_iter()
        .enumerate()
        .map(|(at, e)| {
            let runs = example_path(&tree, &e.command);
            let mut score = example_score(index, path, &e, query, runs);
            // A question that only asks to see something gets the examples that only read.
            if reading && runs.is_some_and(catalog::mutates) {
                score -= 100.0;
            }
            (score, at, e)
        })
        .collect();
    examples.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
    let shown = if first { 3 } else { 2 };
    // Examples made for this question (its song's name, the setting it asks about) come first.
    let examples: Vec<catalog::Example> = made
        .iter()
        .cloned()
        .chain(
            examples
                .into_iter()
                .map(|(.., e)| e)
                .filter(|e| !made.iter().any(|m| m.command == e.command)),
        )
        .take(shown.max(made.len()))
        .collect();
    json!({
        "command": format!("spotify {path}"),
        "path": path,
        "summary": about,
        "examples": examples.into_iter().map(|e| json!({"command": e.command, "description": e.description})).collect::<Vec<_>>(),
        "help": format!("spotify {path} --help"),
        "score": round(score),
    })
}

fn guide_answer(index: &Index, doc: &Doc, score: f64, query: &[Term]) -> Option<Value> {
    let Kind::Guide {
        topic,
        heading,
        blocks,
    } = &doc.kind
    else {
        return None;
    };
    let excerpt = blocks
        .iter()
        .enumerate()
        .map(|(at, b)| (index.text_score(b, query), at, b))
        .max_by(|a, b| a.0.total_cmp(&b.0).then(b.1.cmp(&a.1)))
        .map(|(.., b)| plain(b));
    let heading = heading.replace('`', "");
    Some(json!({
        "topic": topic,
        "section": heading,
        "command": format!("spotify docs {topic} --section {}", shell_quote(&heading)),
        "excerpt": excerpt,
        "score": round(score),
    }))
}

/// Words that ask to see something: "see what's up next", "show my queue".
const READ_WORDS: &[&str] = &[
    "see", "show", "what", "whats", "list", "view", "display", "check", "which", "peek", "look",
];

/// Words that ask to do something (as stems). With one of them a question is not only a read:
/// "what command skips a song".
const ACTION_STEMS: &[&str] = &[
    "play", "skip", "add", "remov", "delet", "clear", "creat", "mak", "set", "chang", "turn",
    "start", "stop", "paus", "resum", "mov", "copy", "fork", "duplicat", "lik", "sav", "put",
    "notify", "remind", "tell", "alert", "jump", "seek", "shuffl", "repeat", "install", "login",
    "logout", "send", "report", "sync", "import", "renam", "connect", "transfer", "cancel",
    "reset", "enabl", "disabl", "updat", "upgrad", "launch", "open", "go",
];

/// Words that say the question is about a default or a setting.
const SETTING_WORDS: &[&str] = &[
    "default",
    "defaults",
    "setting",
    "settings",
    "config",
    "configure",
    "configuration",
    "preference",
    "preferences",
    "permanently",
    "always",
    "automatic",
    "automatically",
];

/// Words that say the question is about the guides: `docs` answers first.
const DOC_WORDS: &[&str] = &["docs", "documentation", "guide", "guides", "manual"];

/// What a mutating command's score is worth in a question that only asks to see something.
const READ_ONLY_PREFERENCE: f64 = 0.5;

/// What a setting's match is worth for `config set` when the question does not say it is about
/// a setting ("search results" alone is more likely about `search`).
const SETTING_WITHOUT_INTENT: f64 = 0.6;

/// The question asks to see something, not to change anything. A participle says what is, not
/// what to do: "what's playing", "show my liked songs".
fn wants_to_read(raw: &[String]) -> bool {
    raw.iter().any(|w| READ_WORDS.contains(&w.as_str()))
        && !raw.iter().any(|w| {
            !w.ends_with("ing") && !w.ends_with("ed") && ACTION_STEMS.contains(&stem(w).as_str())
        })
}

/// The question asks for the list of every command: "list all commands as json",
/// "machine-readable list of every command".
fn wants_the_manifest(raw: &[String]) -> bool {
    let plural = raw.iter().any(|w| w == "commands")
        && raw.iter().any(|w| {
            matches!(
                w.as_str(),
                "all"
                    | "every"
                    | "json"
                    | "machine"
                    | "readable"
                    | "manifest"
                    | "schema"
                    | "list"
                    | "read"
                    | "readonly"
                    | "safe"
                    | "mutate"
                    | "mutates"
            )
        });
    let every = raw.windows(2).any(|pair| {
        matches!(pair[0].as_str(), "every" | "each" | "all") && pair[1].starts_with("command")
    });
    // "which commands change my library", "commands that modify playlists", "which commands do
    // not change anything"; not "which commands change the volume" (that is `volume`).
    let changing = raw.iter().enumerate().any(|(at, w)| {
        w == "commands" && raw[at + 1..].iter().take(4).any(|next| changes_word(next))
    }) && (change_tag(raw).is_some()
        || raw.iter().any(|w| ANYTHING.contains(&w.as_str())));
    plural || every || changing
}

/// Words that name no one thing a command changes: "which commands change something".
const ANYTHING: &[&str] = &[
    "something",
    "anything",
    "nothing",
    "everything",
    "things",
    "stuff",
    "state",
];

/// An amount such as `30s`, `50%` or `1:30`: part of a trigger, not of a song's name.
fn amount(word: &str) -> bool {
    word.contains(':')
        || word.ends_with('%')
        || (word.chars().next().is_some_and(|c| c.is_ascii_digit())
            && word.chars().any(|c| c.is_ascii_alphabetic()))
}

/// The name a question gives (a song, an artist, an album).
#[derive(Debug)]
struct Name {
    text: String,
    /// Where the question says it (byte range), when it is words the guides know too ("OK
    /// Computer"): they are left out of the words that pick the command. Words no document knows
    /// count for nothing anyway.
    span: Option<std::ops::Range<usize>>,
}

/// Small words a title keeps between its capitalized words: "Stairway to Heaven".
const TITLE_JOINERS: &[&str] = &[
    "a", "an", "and", "by", "de", "for", "in", "of", "on", "or", "the", "to", "with", "&",
];

/// The whitespace-separated words of a text with their byte ranges, surrounding punctuation off.
fn spans(text: &str) -> Vec<(std::ops::Range<usize>, &str)> {
    let mut out = Vec::new();
    let mut start = None;
    for (at, c) in text
        .char_indices()
        .chain(std::iter::once((text.len(), ' ')))
    {
        match (c.is_whitespace(), start) {
            (true, Some(from)) => {
                let word: &str = &text[from..at];
                let trimmed = word.trim_matches(|c: char| !c.is_alphanumeric() && c != '&');
                if !trimmed.is_empty() {
                    let lead = word.find(trimmed).unwrap_or(0);
                    out.push((from + lead..from + lead + trimmed.len(), trimmed));
                }
                start = None;
            }
            (false, None) => start = Some(at),
            _ => {}
        }
    }
    out
}

/// Whether `phrase` stands in `text` as words of their own (not inside longer words).
fn says(text: &str, phrase: &str) -> bool {
    text.match_indices(phrase).any(|(at, _)| {
        let before = text[..at].chars().next_back();
        let after = text[at + phrase.len()..].chars().next();
        !before.is_some_and(char::is_alphanumeric) && !after.is_some_and(char::is_alphanumeric)
    })
}

/// A title in capitals after the question's first word ("play the album OK Computer", "add
/// Hotel California to my Road Trip playlist"): the first run of capitalized words (small words
/// such as "to" may join them) that the help and guides never say, so "Liked Songs", "Spotify.app"
/// or "SLT" are no name. A question in Title Case or capitals says nothing this way.
fn capitalized(index: &Index, question: &str) -> Option<(std::ops::Range<usize>, String)> {
    let words = spans(question);
    let upper = |w: &str| w.chars().next().is_some_and(char::is_uppercase);
    let title_case = words
        .iter()
        .filter(|(_, w)| !TITLE_JOINERS.contains(&w.to_lowercase().as_str()))
        .all(|(_, w)| upper(w));
    if words.len() < 2 || title_case {
        return None;
    }
    let mut at = 1;
    while at < words.len() {
        if !upper(words[at].1) || words[at].1 == "I" || words[at].1.starts_with("I'") {
            at += 1;
            continue;
        }
        // Extend over capitalized words, and small words (or "I") that more of them follow:
        // "Stop by the Spice Girls", "Now That I Found You".
        let joiner = |w: &str| w == "I" || TITLE_JOINERS.contains(&w.to_lowercase().as_str());
        let mut end = at;
        let mut next = at + 1;
        while next < words.len() {
            let word = words[next].1;
            if upper(word) && word != "I" {
                end = next;
                next += 1;
                continue;
            }
            let after = words[next..].iter().position(|(_, w)| !joiner(w));
            match after.map(|a| next + a) {
                Some(title) if title > next && upper(words[title].1) => next = title,
                _ => break,
            }
        }
        let range = words[at].0.start..words[end].0.end;
        let text = &question[range.clone()];
        // "Spotify Premium": each word is one the guides write in capitals themselves.
        let own_words = words[at..=end]
            .iter()
            .filter(|(_, w)| !TITLE_JOINERS.contains(&w.to_lowercase().as_str()))
            .all(|(_, w)| says(&index.corpus, w));
        if !says(&index.corpus, text) && !own_words {
            return Some((range, text.to_owned()));
        }
        at = end + 1;
    }
    None
}

/// The name a question gives (a song, an artist): words in double quotes, else a title in
/// capitals (see [`capitalized`]), else the words from the first to the last one no document
/// knows ("505 by arctic monkeys"), stopwords left out and up to a "my" ("hotel california to my
/// road trip playlist" names the song, not the playlist).
fn named(index: &Index, question: &str) -> Option<Name> {
    let mut quotes = question
        .match_indices(['"', '“', '”'])
        .map(|(at, q)| (at, q.len()));
    while let (Some((open, len)), Some((close, _))) = (quotes.next(), quotes.next()) {
        let text = question[open + len..close].trim();
        if !text.is_empty() {
            return Some(Name {
                text: text.to_owned(),
                span: Some(
                    open..close + question[close..].chars().next().map_or(1, char::len_utf8),
                ),
            });
        }
    }
    if let Some((range, text)) = capitalized(index, question) {
        return Some(Name {
            text,
            span: Some(range),
        });
    }
    let raw = raw_words(question);
    let unknown = |w: &String| {
        !stopword(w)
            && !NEGATIONS.contains(&w.as_str())
            && !amount(w)
            && !index.df.contains_key(&stem(w))
            // "which commands do not modify my queue": a question's word, not a song's name.
            && !changes_word(w)
    };
    let first = raw.iter().position(unknown)?;
    let possessive = raw[first..]
        .iter()
        .position(|w| matches!(w.as_str(), "my" | "your" | "our"))
        .map_or(raw.len(), |p| first + p);
    let last = raw[..possessive].iter().rposition(unknown)?;
    let words: Vec<&str> = raw[first..=last]
        .iter()
        .filter(|w| !stopword(w) && !NEGATIONS.contains(&w.as_str()))
        .map(String::as_str)
        .collect();
    // A number alone is a value ("turn the volume down to 30", "results to 5"), not a name; in
    // quotes it can be one ("lyrics of \"505\"").
    let named = words.iter().any(|w| !w.chars().all(|c| c.is_ascii_digit()));
    (named && !words.is_empty()).then(|| Name {
        text: words.join(" "),
        span: None,
    })
}

/// `spotify volume 30` for a question that names a level ("set the volume to 30", "volume 50%",
/// "mute"), `spotify volume +10` for one that names a step ("turn it up by 10"): the level as the
/// command takes it.
fn volume_example(raw: &[String]) -> Option<catalog::Example> {
    let found = raw.iter().enumerate().find_map(|(at, w)| {
        let n: u8 = w.strip_suffix('%').unwrap_or(w).parse().ok()?;
        (n <= 100).then_some((at, n))
    });
    let Some((at, n)) = found else {
        return raw
            .iter()
            .any(|w| w == "mute" || w == "silence")
            .then(|| example("spotify volume 0".into(), "Mute: the volume to 0"));
    };
    let has = |words: &[&str]| raw.iter().any(|w| words.contains(&w.as_str()));
    let up = has(&["louder", "up", "increase", "raise", "higher"]);
    let down = has(&["quieter", "down", "lower", "decrease", "reduce"]);
    // "down to 30" is a level; "down by 20" or "up 10" a step.
    let to = at
        .checked_sub(1)
        .is_some_and(|b| matches!(raw[b].as_str(), "to" | "at"));
    Some(if !to && (up != down) {
        let sign = if up { '+' } else { '-' };
        example(
            format!("spotify volume {sign}{n}"),
            "Change the volume by this much",
        )
    } else {
        example(
            format!("spotify volume {n}"),
            "Set the volume to this level (0-100)",
        )
    })
}

/// Words people use for a setting beyond its key and description: (key, phrases).
const SETTING_KEYWORDS: &[(&str, &[&str])] = &[
    ("api_url", &["backend url", "server url", "backend"]),
    (
        "telemetry",
        &["usage data", "analytics", "tracking", "diagnostics"],
    ),
    ("org", &["organization", "org handle"]),
    (
        "strategy",
        &[
            "applescript only",
            "spotify_player only",
            "web api only",
            "playback path",
            "never applescript",
        ],
    ),
    (
        "launch_spotify",
        &[
            "start spotify automatically",
            "open spotify automatically",
            "auto launch",
            "autostart spotify",
        ],
    ),
    (
        "verify_timeout_ms",
        &[
            "wait longer",
            "falling back",
            "fallback timeout",
            "verify timeout",
        ],
    ),
    (
        "keep_spotify_in_background",
        &[
            // Each of these reads as `frontmost` (see `focus_phrase`).
            "stealing focus",
            "steal focus",
            "bring spotify to the front",
            "bring spotify up",
            "spotify pops up",
            "jump to the front",
            "come to the front",
            "activate spotify",
            "foreground",
            "stay in the background",
            "in the background",
        ],
    ),
    (
        "spotify_player_binary",
        &["spotify_player path", "spotify_player executable"],
    ),
    (
        "spotify_player_config_dir",
        &["client id", "bring your own", "byo"],
    ),
    (
        "spotify_player_cache_dir",
        &["spotify tokens", "token cache"],
    ),
    (
        "search_limit",
        &[
            "number of search results",
            "how many results",
            "results per search",
            "search results",
        ],
    ),
    (
        "notify_isi",
        &["route notifications", "route trigger notifications", "isi"],
    ),
    (
        "auto_update",
        &[
            "automatic updates",
            "auto update",
            "update automatically",
            "auto updates",
        ],
    ),
    (
        "output",
        &[
            "json by default",
            "always json",
            "print json",
            "default output",
            "output format",
        ],
    ),
];

/// A value for a shell command line, single-quoted.
fn single_quoted(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

fn example(command: String, description: &str) -> catalog::Example {
    catalog::Example {
        command,
        description: Some(description.to_owned()),
    }
}

/// How a command is used with a song's name it cannot take itself: `lyrics` and `track` take a
/// URI, so the name is searched first. `kind` is what the question says the name is ("the album
/// OK Computer"); a song when it says nothing.
fn with_name(path: &str, name: &str, kind: Option<&str>) -> Vec<catalog::Example> {
    let words = single_quoted(name);
    let typed = kind.map(|k| format!(" --type {k}")).unwrap_or_default();
    let thing = match kind {
        Some("album") => "album",
        Some("playlist") => "playlist",
        Some("artist") => "artist",
        Some("show") => "show",
        Some("episode") => "episode",
        _ => "song",
    };
    let find_track = || {
        example(
            format!("spotify search {words} --type track"),
            "1. Find the song; copy its spotify:track: URI",
        )
    };
    match path {
        "lyrics" => vec![
            find_track(),
            example(
                "spotify lyrics <uri>".into(),
                "2. Its lyrics; nothing has to play",
            ),
        ],
        "track" => vec![
            example(
                format!("spotify search {words}{typed}"),
                "1. Find it; copy its URI",
            ),
            example("spotify track <uri>".into(), "2. Its details"),
        ],
        "playlist add" => vec![
            find_track(),
            example(
                "spotify playlist add <playlist-id> <uri>".into(),
                "2. Add it (playlist ids: spotify playlist list)",
            ),
        ],
        "devices connect" => vec![example(
            format!("spotify devices connect --name {words}"),
            "Move playback to the device by that name (names: spotify devices)",
        )],
        "playlist create" => vec![example(
            format!("spotify playlist create {words}"),
            "A new playlist by that name",
        )],
        "playlist play" | "playlist show" => vec![
            example(
                format!("spotify search {words} --type playlist"),
                "1. Find it (your own: spotify playlist list); copy its id",
            ),
            example(
                format!("spotify {path} <playlist-id>"),
                if path == "playlist play" {
                    "2. Play it"
                } else {
                    "2. Its tracks"
                },
            ),
        ],
        "play" => vec![example(
            format!("spotify play --search {words}{typed}"),
            &format!("Plays the first {thing} that matches"),
        )],
        "queue" | "queue add" => vec![example(
            format!("spotify queue add --search {words}"),
            "Queues the first song that matches",
        )],
        "search" => vec![example(
            format!("spotify search {words}{typed}"),
            if typed.is_empty() {
                "Songs, albums, artists, playlists and podcasts, with URIs"
            } else {
                "The matches, with URIs"
            },
        )],
        _ => Vec::new(),
    }
}

/// How to install tab completion for the shell a question names (zsh, macOS's default, when it
/// names none): every line, so it works as it is.
fn completion_lines(raw: &[String]) -> Vec<catalog::Example> {
    let shell = raw
        .iter()
        .find_map(|w| match w.as_str() {
            "zsh" | "bash" | "fish" | "powershell" => Some(w.as_str()),
            "pwsh" => Some("powershell"),
            _ => None,
        })
        .unwrap_or("zsh");
    match shell {
        "bash" => vec![
            example(
                "mkdir -p ~/.bash_completion.d && spotify completions bash > ~/.bash_completion.d/spotify".into(),
                "1. Save the script",
            ),
            example(
                "echo 'source ~/.bash_completion.d/spotify' >> ~/.bashrc".into(),
                "2. Load it in new shells (macOS Terminal: ~/.bash_profile)",
            ),
        ],
        "fish" => vec![
            example(
                "mkdir -p ~/.config/fish/completions".into(),
                "1. Where fish looks",
            ),
            example(
                "spotify completions fish > ~/.config/fish/completions/spotify.fish".into(),
                "2. Save the script; fish loads it",
            ),
        ],
        "powershell" => vec![example(
            "spotify completions powershell >> $PROFILE".into(),
            "Loaded by new PowerShell sessions",
        )],
        _ => vec![
            example(
                "mkdir -p ~/.zfunc && spotify completions zsh > ~/.zfunc/_spotify".into(),
                "1. Save the script",
            ),
            example(
                "echo 'fpath=(~/.zfunc $fpath); autoload -Uz compinit && compinit' >> ~/.zshrc".into(),
                "2. Load it in new shells",
            ),
        ],
    }
}

/// The value to show for a setting in `config set '{"<key>": <value>}'`: the one the question
/// names when it names one the setting takes, else one that differs from the default.
fn setting_value(at: usize, raw: &[String]) -> Value {
    let (key, kind, default, _) = CONFIG_KEYS[at];
    if kind == "boolean" {
        let said = |words: &[&str]| raw.iter().any(|w| words.contains(&w.as_str()));
        let on = said(&["on", "enable", "enabled", "true", "yes"]);
        // A negation turns a setting off ("don't start spotify automatically"), except the one
        // it asks for: "stop Spotify jumping to the front", "don't let it come forward" ask for
        // keep_spotify_in_background, which is on.
        let negative = said(&["no", "stop"]) || raw.iter().any(|w| NEGATIONS.contains(&w.as_str()));
        let off = said(&["off", "disable", "disabled", "false"])
            || (negative && key != "keep_spotify_in_background");
        return Value::Bool(if on && !off {
            true
        } else if off {
            false
        } else {
            // A question that names no value: for these, the behavior the setting turns on;
            // for the others, the value that differs from the default.
            ON_WHEN_ASKED.contains(&key) || default != "true"
        });
    }
    if let Some(range) = kind.strip_prefix("integer ") {
        let (min, max) = range
            .split_once('-')
            .and_then(|(a, b)| Some((a.trim().parse::<u64>().ok()?, b.trim().parse::<u64>().ok()?)))
            .unwrap_or((0, u64::MAX));
        let asked = raw
            .iter()
            .filter_map(|w| w.parse::<u64>().ok())
            .find(|n| (min..=max).contains(n));
        let more = raw.iter().any(|w| {
            matches!(
                w.as_str(),
                "longer" | "more" | "increase" | "raise" | "higher" | "bigger" | "slower"
            )
        });
        let fallback = default.parse::<u64>().ok().map_or(min, |d| {
            if more || d / 2 < min {
                (d * 2).min(max)
            } else {
                d / 2
            }
        });
        return json!(asked.unwrap_or(fallback));
    }
    if kind.contains('|') && !kind.contains('(') {
        let values: Vec<&str> = kind.split('|').map(str::trim).collect();
        // "never use applescript" names a value to stay away from.
        let said = |value: &str, negated: bool| {
            raw.iter().enumerate().any(|(at, w)| {
                (w == value || *w == value.replace('_', ""))
                    && raw[at.saturating_sub(2)..at]
                        .iter()
                        .any(|b| NEGATIONS.contains(&b.as_str()))
                        == negated
            })
        };
        let named = values.iter().find(|v| said(v, false));
        let other = values.iter().find(|v| **v != default && !said(v, true));
        return json!(named.or(other).copied().unwrap_or(default));
    }
    let placeholder = if kind.contains("path") {
        "<path>".to_owned()
    } else if kind.contains("origin") {
        "<https-origin>".to_owned()
    } else if kind.contains("org") {
        "<org>".to_owned()
    } else {
        format!("<{}>", key.rsplit('_').next().unwrap_or(key))
    };
    Value::String(placeholder)
}

/// Settings that a question naming no value asks to have on: "keep Spotify from jumping to the
/// front", "start spotify automatically", "update automatically". All three are on by default.
const ON_WHEN_ASKED: &[&str] = &[
    "keep_spotify_in_background",
    "launch_spotify",
    "auto_update",
];

/// A setting's default as a JSON value, where it is one (`true`, `2500`, `"auto"`).
fn setting_default(at: usize) -> Option<Value> {
    let (_, kind, default, _) = CONFIG_KEYS[at];
    if kind == "boolean" {
        return default.parse::<bool>().ok().map(Value::Bool);
    }
    if kind.starts_with("integer ") {
        return default.parse::<u64>().ok().map(|n| json!(n));
    }
    (kind.contains('|') && !kind.contains('(')).then(|| json!(default))
}

/// Whether the question asks for a setting as it is by default: "keep Spotify from jumping to
/// the front" is keep_spotify_in_background on, which it is unless someone turned it off.
fn asks_for_default(at: usize, raw: &[String]) -> bool {
    setting_default(at).is_some_and(|default| default == setting_value(at, raw))
}

/// `spotify config get <key>`: the setting's value now.
fn setting_get(at: usize) -> catalog::Example {
    let (key, _, default, _) = CONFIG_KEYS[at];
    example(
        format!("spotify config get {key}"),
        &format!("Its value now (default {default})"),
    )
}

/// For a setting the question wants as it is by default: check it, and the one line that changes
/// it, only if wanted.
fn setting_check(at: usize) -> Vec<catalog::Example> {
    let (key, _, default, _) = CONFIG_KEYS[at];
    let (check, change, other) = match setting_default(at) {
        Some(Value::Bool(true)) => (
            "Check it: true (on) by default".to_owned(),
            "Only to turn it off",
            Value::Bool(false),
        ),
        Some(Value::Bool(false)) => (
            "Check it: false (off) by default".to_owned(),
            "Only to turn it on",
            Value::Bool(true),
        ),
        _ => (
            format!("Check it: {default} by default"),
            "Only to change it",
            setting_value(at, &[]),
        ),
    };
    let object = format!("{{{}: {other}}}", Value::String(key.to_owned()));
    vec![
        example(format!("spotify config get {key}"), &check),
        example(
            format!("spotify config set {}", single_quoted(&object)),
            change,
        ),
    ]
}

/// What `spotify how` says under the `config get` answer for a setting that is already as asked.
fn setting_note(at: usize) -> String {
    let (key, _, default, description) = CONFIG_KEYS[at];
    let (already, change) = match default {
        "true" => (" (already on)", "Turn it off only if you do not want that."),
        "false" => (" (already off)", "Turn it on only if you want that."),
        _ => ("", "Change it only if you want another value."),
    };
    format!(
        "{key} is {default} by default{already}: {} {change}",
        description.trim_end_matches('.').to_owned() + "."
    )
}

/// What a command can change, as a question names it ("which commands change my library"): the
/// `changes` tag of `spotify commands --json`.
fn change_tag(raw: &[String]) -> Option<&'static str> {
    raw.iter().find_map(|w| match w.as_str() {
        "library" | "liked" | "likes" | "saved" | "favorites" | "favourites" => Some("library"),
        "playlist" | "playlists" => Some("playlists"),
        "playback" | "playing" | "music" => Some("playback"),
        "config" | "settings" | "setting" | "configuration" => Some("config"),
        "trigger" | "triggers" => Some("triggers"),
        "daemon" => Some("daemon"),
        "session" | "login" | "sessions" => Some("session"),
        _ => None,
    })
}

/// Examples of the manifest made for a question about the commands: those that change what it
/// names, or those that only read.
fn manifest_examples(raw: &[String]) -> Vec<catalog::Example> {
    let mut out = Vec::new();
    let read_only = changes_nothing(raw)
        || raw
            .iter()
            .any(|w| matches!(w.as_str(), "readonly" | "safe" | "harmless"))
        || (raw.iter().any(|w| w == "read") && raw.iter().any(|w| w == "only"));
    if read_only {
        out.push(example(
            "spotify commands --json | jq -r '.commands[] | select(.mutates == false or .read_only_when != null).command'".into(),
            "Commands that only read, and those with a form that only reads (read_only_when)",
        ));
    } else if let Some(tag) = change_tag(raw).filter(|_| raw.iter().any(|w| changes_word(w))) {
        let what = catalog::Change::ALL
            .iter()
            .find(|(_, id, _)| *id == tag)
            .map_or("", |(.., meaning)| *meaning);
        out.push(example(
            format!(
                "spotify commands --json | jq -r '.commands[] | select(.changes | index(\"{tag}\")).command'"
            ),
            &format!("The commands that change {tag}: {what}"),
        ));
    } else if raw.iter().any(|w| changes_word(w)) {
        out.push(example(
            "spotify commands --json | jq -r '.commands[] | select(.mutates).command'".into(),
            "The commands that can change something (`changes` says what)",
        ));
    }
    if !out.is_empty() {
        out.push(example(
            "spotify commands".into(),
            "By goal; ✎ marks what each command changes",
        ));
    }
    out
}

/// The question asks for what changes nothing: "which commands don't change anything",
/// "commands that change nothing", "never modify my playlists".
fn changes_nothing(raw: &[String]) -> bool {
    raw.iter().enumerate().any(|(at, w)| {
        changes_word(w)
            && (raw[at.saturating_sub(2)..at]
                .iter()
                .any(|b| NEGATIONS.contains(&b.as_str()))
                || raw.get(at + 1).is_some_and(|n| n == "nothing"))
    })
}

/// A word that asks what changes something: "which commands change my library".
fn changes_word(word: &str) -> bool {
    matches!(
        word,
        "change"
            | "changes"
            | "modify"
            | "modifies"
            | "mutate"
            | "mutates"
            | "write"
            | "writes"
            | "edit"
            | "edits"
            | "touch"
            | "touches"
            | "alter"
            | "alters"
            | "affect"
            | "affects"
    )
}

/// `spotify config set '{"search_limit": 5}'` for the setting a question asks about.
fn setting_example(at: usize, raw: &[String]) -> catalog::Example {
    let (key, kind, default, _) = CONFIG_KEYS[at];
    let object = format!(
        "{{{}: {}}}",
        Value::String(key.to_owned()),
        setting_value(at, raw)
    );
    example(
        format!("spotify config set {}", single_quoted(&object)),
        &format!("{key}: {kind}, default {default}"),
    )
}

/// The answer to a question, as `spotify how --json` prints it.
#[must_use]
pub fn answer(question: &str, limit: usize) -> Value {
    let index = build();
    let name = named(&index, question);
    // A title's words say nothing about the command ("OK Computer" is no `devices` question), so
    // the question is read without them, unless nothing else is left.
    let rest = name
        .as_ref()
        .and_then(|n| n.span.clone())
        .map(|span| format!("{} {}", &question[..span.start], &question[span.end..]))
        .filter(|rest| !query_terms(rest).is_empty());
    let asked = rest.as_deref().unwrap_or(question);
    let query = query_terms(asked);
    let raw = raw_words(asked);
    // The question's words as written (negated ones out): what names a command.
    let words: HashSet<String> = {
        let mut out = HashSet::new();
        let mut negated = false;
        for (at, word) in raw.iter().enumerate() {
            if NEGATIONS.contains(&word.as_str()) {
                negated = negates(&raw, at);
                continue;
            }
            if !negated {
                out.insert(word.clone());
            }
            if !stopword(word) {
                negated = false;
            }
        }
        out
    };
    let reading = wants_to_read(&raw);
    let manifest = wants_the_manifest(&raw);
    // Spotify.app coming to the front ("jumps to the front", "steals focus") is what one
    // setting is about.
    let about_settings = raw
        .iter()
        .any(|w| SETTING_WORDS.contains(&w.as_str()) || w == "frontmost");
    let about_docs = raw.iter().any(|w| DOC_WORDS.contains(&w.as_str()));
    let mut scored: Vec<(f64, &Doc)> = index
        .docs
        .iter()
        .map(|doc| {
            let mut score = index.score(doc, &query, &words);
            if let Kind::Command { path, .. } = &doc.kind {
                // "see what's up next" is `queue`, not `next`.
                if reading && catalog::mutates(path) && catalog::effects(path).1.is_none() {
                    score *= READ_ONLY_PREFERENCE;
                }
                if manifest && path == "commands" {
                    score = score.max(1.0) * 3.0;
                }
                if about_docs && path == "docs" {
                    score *= 2.0;
                }
            }
            (score, doc)
        })
        .filter(|(score, _)| *score > 0.0)
        .collect();
    scored.sort_by(|a, b| b.0.total_cmp(&a.0));

    // Commands: a group gives way to a subcommand that matches (nearly) as well.
    let mut command_scores: Vec<(f64, &str, Option<f64>)> = scored
        .iter()
        .filter_map(|(score, doc)| match &doc.kind {
            Kind::Command { path, yields } => Some((*score, path.as_str(), *yields)),
            _ => None,
        })
        .collect();
    // A question that names a command as it is typed ("spotify play returns exit code 1",
    // "`spotify queue add` fails") is about that one (the longest name it says), however common
    // its words are.
    let typed = index
        .docs
        .iter()
        .filter_map(|doc| match &doc.kind {
            Kind::Command { path, .. } => Some(path.as_str()),
            _ => None,
        })
        .filter(|path| {
            let name: Vec<&str> = std::iter::once("spotify").chain(path.split(' ')).collect();
            raw.windows(name.len())
                .any(|w| w.iter().zip(&name).all(|(a, b)| a == b))
        })
        .max_by_key(|path| path.split(' ').count());
    if let Some(path) = typed {
        let best = command_scores.iter().map(|c| c.0).fold(0.0, f64::max);
        match command_scores.iter_mut().find(|c| c.1 == path) {
            Some(entry) => entry.0 = entry.0.max(best * 1.05),
            None => command_scores.push((best * 1.05, path, None)),
        }
        command_scores.sort_by(|a, b| b.0.total_cmp(&a.0));
    }
    // A setting that matches is answered by `config set`, with that setting in the example; one
    // the question wants as it is by default by `config get`, to check it.
    let setting = scored.iter().find_map(|(score, doc)| match doc.kind {
        Kind::Setting(at) => Some((*score, at)),
        _ => None,
    });
    let as_default = setting.is_some_and(|(_, at)| asks_for_default(at, &raw));
    let setting_path = if as_default {
        "config get"
    } else {
        "config set"
    };
    if let Some((score, at)) = setting {
        let lifted = score
            * if about_settings {
                1.0
            } else {
                SETTING_WITHOUT_INTENT
            };
        // A question that says Spotify.app comes to the front is about that setting first,
        // whatever else it says ("... every time a song starts"): `play` says the same in passing.
        let lifted = if CONFIG_KEYS[at].0 == "keep_spotify_in_background"
            && query
                .iter()
                .any(|q| q.term == "frontmost" && q.weight >= 1.0)
        {
            let best = command_scores.iter().map(|c| c.0).fold(0.0, f64::max);
            lifted.max(best * 1.05)
        } else {
            lifted
        };
        // Already as asked: `config get` checks it and its card has the `config set` line, so
        // `config set`'s own card (its help's examples) would only repeat it.
        let lifted = if as_default {
            let own = command_scores
                .iter()
                .find(|c| c.1 == "config set")
                .map_or(0.0, |c| c.0);
            command_scores.retain(|c| c.1 != "config set");
            lifted.max(own)
        } else {
            lifted
        };
        match command_scores.iter_mut().find(|c| c.1 == setting_path) {
            Some(entry) => entry.0 = entry.0.max(lifted),
            None => command_scores.push((lifted, setting_path, None)),
        }
        command_scores.sort_by(|a, b| b.0.total_cmp(&a.0));
    }
    let name = name.map(|n| n.text);
    let kind = raw.iter().find_map(|w| match w.as_str() {
        "album" | "albums" | "record" => Some("album"),
        "playlist" | "playlists" => Some("playlist"),
        "artist" | "artists" | "band" | "singer" => Some("artist"),
        "podcast" | "podcasts" | "show" | "shows" => Some("show"),
        "episode" | "episodes" => Some("episode"),
        _ => None,
    });
    let made = |path: &str| -> Vec<catalog::Example> {
        let mut out = name
            .as_deref()
            .map(|name| with_name(path, name, kind))
            .unwrap_or_default();
        if path == setting_path
            && let Some((_, at)) = setting
        {
            if as_default {
                out = setting_check(at);
            } else {
                out.insert(0, setting_example(at, &raw));
                out.insert(1, setting_get(at));
            }
        }
        if path == "commands" && manifest {
            out.extend(manifest_examples(&raw));
        }
        if path == "completions" {
            out = completion_lines(&raw);
        }
        if path == "volume" {
            out.extend(volume_example(&raw));
        }
        // "search the docs for --random": the flag, as it was written.
        if path == "docs"
            && let Some(text) = question
                .split_whitespace()
                .find(|w| w.starts_with("--") && w.len() > 2)
                .map(|w| {
                    w.trim_matches(|c: char| !c.is_alphanumeric() && c != '-')
                        .to_owned()
                })
                .or_else(|| name.clone())
        {
            out.push(example(
                format!("spotify docs --search {}", single_quoted(&text)),
                "Every line of the guides that says it",
            ));
        }
        out
    };
    let best = command_scores.first().map_or(0.0, |c| c.0);
    // A command far behind the best guide or error row answers nothing: "what do the exit codes
    // mean" is a guide section, not `testing exit`.
    let overall = scored.first().map_or(0.0, |s| s.0).max(best);
    let floor = (best * 0.45).max(overall * 0.35);
    // "Which commands change my library": the manifest's filter, then the commands themselves
    // (not whatever else says "library").
    let changed = (manifest && raw.iter().any(|w| changes_word(w)) && !changes_nothing(&raw))
        .then(|| change_tag(&raw))
        .flatten();
    let read_only_list = manifest && changes_nothing(&raw);
    let mut commands = Vec::new();
    for (score, path, yields) in &command_scores {
        if changed.is_some() || commands.len() >= limit || *score < floor {
            break;
        }
        // "Which commands never change my playlists": not `playlist rename`.
        if read_only_list && !catalog::has_read_only_form(path) {
            continue;
        }
        // A group gives way only to a subcommand that is shown in its place.
        let prefix = format!("{path} ");
        if let Some(share) = yields
            && command_scores
                .iter()
                .any(|(s, p, _)| p.starts_with(&prefix) && *s >= score * share && *s >= floor)
        {
            continue;
        }
        commands.push(command_answer(
            &index,
            path,
            *score,
            &query,
            commands.is_empty(),
            &made(path),
            reading,
        ));
    }
    // "errors", "code" and "exit" alone say what the question is about, not that something is
    // wrong: "which errors can search return" gets error rows only when they match better than
    // any command.
    let troubled = raw
        .iter()
        .any(|w| TROUBLE.contains(&w.as_str()) && !GENERIC_TROUBLE.contains(&w.as_str()));
    // When something is wrong, doctor stays in the answer if it matches at all: it names the
    // failing piece and its fix. (Asking what exit codes or errors mean is not trouble.)
    let failing = raw.iter().any(|w| {
        TROUBLE.contains(&w.as_str())
            && !matches!(w.as_str(), "why" | "error" | "errors" | "code" | "exit")
    });
    if failing
        && changed.is_none()
        && commands.len() < limit
        && !commands.iter().any(|c| c["path"] == "doctor")
        && let Some((score, ..)) = command_scores.iter().find(|(_, p, _)| *p == "doctor")
    {
        commands.push(command_answer(
            &index,
            "doctor",
            *score,
            &query,
            commands.is_empty(),
            &[],
            reading,
        ));
    }

    if let Some(tag) = changed {
        for path in std::iter::once("commands").chain(catalog::changed_by(tag)) {
            if commands.len() >= limit {
                break;
            }
            if commands.iter().any(|c| c["path"] == path) {
                continue;
            }
            let score = command_scores
                .iter()
                .find(|(_, p, _)| *p == path)
                .map_or(0.0, |c| c.0);
            commands.push(command_answer(
                &index,
                path,
                score,
                &query,
                commands.is_empty(),
                &made(path),
                reading,
            ));
        }
    }

    // Guides: the best two, one per section, when they match about as well as the commands.
    let guide_best = scored
        .iter()
        .find(|(_, d)| matches!(d.kind, Kind::Guide { .. }))
        .map_or(0.0, |g| g.0);
    let mut guides: Vec<Value> = Vec::new();
    for (score, doc) in &scored {
        if guides.len() >= 2 || *score < guide_best * 0.6 || *score < best * 0.45 {
            continue;
        }
        if let Some(guide) = guide_answer(&index, doc, *score, &query)
            && !guides.iter().any(|g| g["command"] == guide["command"])
        {
            guides.push(guide);
        }
    }

    let error_best = scored
        .iter()
        .find(|(_, d)| matches!(d.kind, Kind::Error(_)))
        .map_or(0.0, |e| e.0);
    // An error row answers only when a word of the question that names something (not "error",
    // "arguments" or "return") is in it.
    let specific = |doc: &Doc| {
        query.iter().any(|q| {
            q.weight >= 0.5
                && !q.negated
                && !META.contains(&q.term.as_str())
                && !ERROR_GENERIC.contains(&q.term.as_str())
                && doc.tf.contains_key(&q.term)
        })
    };
    // A code the question names as written ("spotify says track_mismatch") answers first,
    // whatever else matches.
    let lower = question.to_lowercase();
    let named_code = |doc: &Doc| match doc.kind {
        Kind::Error(at) => index.errors.get(at).is_some_and(|row| {
            row.codes
                .iter()
                .any(|code| code.contains('_') && says(&lower, code))
        }),
        _ => false,
    };
    // A question a setting answers ("use applescript only") is about that setting, not about
    // the error rows that say the same word.
    let setting_first =
        setting.is_some() && commands.first().is_some_and(|c| c["path"] == setting_path);
    let matching: Vec<&(f64, &Doc)> = if troubled || (error_best >= best && !setting_first) {
        scored
            .iter()
            .filter(|(score, doc)| {
                matches!(doc.kind, Kind::Error(_)) && *score >= error_best * 0.7 && specific(doc)
            })
            .collect()
    } else {
        Vec::new()
    };
    let errors: Vec<Value> = scored
        .iter()
        .filter(|(_, doc)| named_code(doc))
        .chain(matching.into_iter().filter(|(_, doc)| !named_code(doc)))
        .take(2)
        .filter_map(|(score, doc)| match doc.kind {
            Kind::Error(at) => index.errors.get(at).map(|row| {
                json!({
                    "code": row.codes.first(),
                    "codes": row.codes,
                    // `—` in the guide: a warning, never an exit.
                    "exit": row.exit.parse::<i32>().ok(),
                    "meaning": row.meaning,
                    "fix": row.fix,
                    "command": "spotify docs errors",
                    "score": round(*score),
                })
            }),
            _ => None,
        })
        .collect();

    // A setting that is already as asked: the card says so, with the check and the change only.
    if as_default && let Some((_, at)) = setting {
        for card in commands.iter_mut().filter(|c| c["path"] == setting_path) {
            if let Some(examples) = card["examples"].as_array_mut() {
                examples.truncate(2);
            }
            card["note"] = json!(setting_note(at));
        }
    }

    // Something is wrong and no command matches its words ("something is wrong"): doctor
    // checks everything.
    if commands.is_empty() && (failing || (guides.is_empty() && errors.is_empty() && troubled)) {
        commands.push(command_answer(
            &index,
            "doctor",
            0.0,
            &query,
            true,
            &[],
            reading,
        ));
    }

    json!({
        "question": question,
        "terms": query.iter().map(|t| json!({"term": t.term, "weight": round(t.weight), "negated": t.negated})).collect::<Vec<_>>(),
        "name": name,
        "commands": commands,
        "guides": guides,
        "errors": errors,
        "more": ["spotify --help", "spotify docs --search '<words>'", "spotify commands --json"],
    })
}

/// The answer in words.
#[must_use]
pub fn render(v: &Value) -> String {
    let mut out = String::new();
    let commands = v["commands"].as_array().cloned().unwrap_or_default();
    let guides = v["guides"].as_array().cloned().unwrap_or_default();
    let errors = v["errors"].as_array().cloned().unwrap_or_default();
    if commands.is_empty() && guides.is_empty() && errors.is_empty() {
        return format!(
            "Nothing matches `{}`. Try other words, `spotify --help` (commands by goal) or `spotify docs --search '<words>'`.",
            v["question"].as_str().unwrap_or("")
        );
    }
    for command in &commands {
        out.push_str(&format!(
            "{} — {}\n",
            command["command"].as_str().unwrap_or(""),
            command["summary"].as_str().unwrap_or("")
        ));
        let examples = command["examples"].as_array().cloned().unwrap_or_default();
        let width = examples
            .iter()
            .filter(|e| e["description"].is_string())
            .map(|e| e["command"].as_str().unwrap_or("").chars().count())
            .max()
            .unwrap_or(0);
        for example in &examples {
            let line = example["command"].as_str().unwrap_or("");
            match example["description"].as_str() {
                Some(description) if width + 4 + description.chars().count() <= 98 => {
                    out.push_str(&format!("  {line:<width$}  {description}\n"));
                }
                // Not in the column (a longer line sets it), but on its own line it fits.
                Some(description)
                    if line.chars().count() + 4 + description.chars().count() <= 98 =>
                {
                    out.push_str(&format!("  {line}  {description}\n"));
                }
                _ => out.push_str(&format!("  {line}\n")),
            }
        }
        if let Some(note) = command["note"].as_str() {
            out.push_str(&format!("  Note: {note}\n"));
        }
        out.push('\n');
    }
    for guide in &guides {
        out.push_str(&format!(
            "Read: {}\n",
            guide["command"].as_str().unwrap_or("")
        ));
        if let Some(excerpt) = guide["excerpt"].as_str() {
            out.push_str(&format!("  {excerpt}\n"));
        }
    }
    for error in &errors {
        let exit = error["exit"]
            .as_i64()
            .map_or_else(|| "a warning".to_owned(), |e| format!("exit {e}"));
        let codes = error["codes"]
            .as_array()
            .map(|c| {
                c.iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(" / ")
            })
            .unwrap_or_default();
        out.push_str(&format!(
            "{}Error {codes} ({exit}): {}\n  Fix: {}\n",
            if guides.is_empty() { "" } else { "\n" },
            error["meaning"].as_str().unwrap_or(""),
            error["fix"].as_str().unwrap_or("")
        ));
    }
    out.trim_end().to_owned()
}

/// `spotify how`.
///
/// # Errors
/// `invalid_input` for a question without words or a bad `--limit`.
pub fn how(ctx: &Ctx, question: &[String], limit: Option<i64>) -> Result<()> {
    let question = question.join(" ");
    if query_terms(&question).is_empty() {
        return Err(Error::invalid(
            format!("`{question}` has no words to search for."),
            "Ask with a few plain words, e.g. spotify how \"play my liked songs shuffled\".",
        ));
    }
    let limit = crate::ops::check_limit(
        limit,
        Some(LIMIT_MAX),
        &format!("Pass --limit 1 to {LIMIT_MAX} (default 3)."),
    )?
    .unwrap_or(3);
    let value = answer(&question, limit);
    ctx.emit(&value, render);
    if let Some(help) = value["commands"][0]["help"].as_str() {
        ctx.next(&[help.to_owned()]);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn top(question: &str) -> Vec<String> {
        answer(question, 3)["commands"]
            .as_array()
            .map(|c| {
                c.iter()
                    .filter_map(|c| c["path"].as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn words_keep_amounts_and_drop_apostrophes() {
        assert_eq!(
            raw_words("Notify me 30 seconds before it's over, at 1:30 or 50%!"),
            [
                "notify", "me", "30s", "before", "its", "over", "at", "1:30", "or", "50%"
            ]
        );
        assert_eq!(raw_words("isn't"), ["isnt"]);
        assert_eq!(stem("songs"), "song");
        assert_eq!(stem("playing"), stem("play"));
        assert_eq!(stem("shuffled"), stem("shuffle"));
        assert_eq!(stem("stopping"), "stop");
        assert_eq!(stem("lyrics"), stem("lyric"));
        assert_eq!(stem("status"), "status");
        assert_eq!(stem("liked"), stem("like"));
    }

    #[test]
    fn negated_words_count_little() {
        let query = query_terms("lyrics of a song that isn't playing");
        let find = |t: &str| query.iter().find(|q| q.term == t).cloned();
        assert_eq!(
            find("lyric").map(|q| (q.weight, q.negated)),
            Some((1.0, false))
        );
        assert_eq!(
            find(&stem("playing")).map(|q| (q.weight, q.negated)),
            Some((0.3, true))
        );
        assert_eq!(
            marked_terms("nothing has to play"),
            [(stem("play"), true)],
            "a text's own negation"
        );
    }

    #[test]
    fn questions_find_the_right_command() {
        for (question, expected) in [
            ("lyrics of a song that isn't playing", "lyrics"),
            ("notify me 30 seconds before the song ends", "trigger add"),
            ("play my liked songs shuffled", "play"),
            ("skip this song", "next"),
            ("make it louder", "volume"),
            ("what is playing right now", "status"),
            ("add a song to a playlist", "playlist add"),
            ("put a song up next", "queue add"),
            ("tab completion for zsh", "completions"),
            ("log in with an SLT", "login"),
            ("log out", "logout"),
            ("is the daemon running", "daemon status"),
            ("turn off hints", "config"),
            ("copy a playlist", "playlist fork"),
            ("move playback to my phone", "devices connect"),
            ("turn off telemetry", "config set"),
            ("stop the music", "pause"),
            ("save this song", "like"),
            ("turn it up", "volume"),
            ("turn the volume down", "volume"),
            ("how long is left in this song", "status"),
            // A command that does something itself is not hidden behind its subcommands.
            ("how do I log in", "login"),
            ("what's in my queue", "queue"),
            // Trouble without the name of a command.
            ("spotify isn't responding", "doctor"),
            ("the daemon is not running", "doctor"),
            ("spotify is stuck", "doctor"),
            ("set everything up", "setup"),
            // Every word of a subcommand's name, as written.
            ("remove a trigger", "trigger remove"),
            ("which config keys exist", "config keys"),
            ("use applescript only", "config set"),
        ] {
            let found = top(question);
            assert_eq!(
                found.first().map(String::as_str),
                Some(expected),
                "{question}: {found:?}"
            );
        }
    }

    #[test]
    fn examples_that_fit_the_question_come_first() {
        let value = answer("notify me 30 seconds before the song ends", 3);
        let first = value["commands"][0]["examples"][0]["command"]
            .as_str()
            .unwrap_or("");
        assert!(first.contains("--remaining 30s"), "{value}");
        let value = answer("play my liked songs shuffled", 3);
        let first = value["commands"][0]["examples"][0]["command"]
            .as_str()
            .unwrap_or("");
        assert_eq!(first, "spotify play --liked --random", "{value}");
        // Not the song playing now: an example that names another song comes first.
        let value = answer("lyrics of a song that isn't playing", 3);
        let first = value["commands"][0]["examples"][0]["command"]
            .as_str()
            .unwrap_or("");
        assert_ne!(first, "spotify lyrics", "{value}");
    }

    #[test]
    fn phrases_become_the_words_the_help_uses() {
        assert_eq!(
            raw_words("what's half over"),
            ["whats", "halfway"],
            "`half over` is halfway"
        );
        assert_eq!(raw_words("turn it up"), ["louder", "it"]);
        assert_eq!(
            raw_words("turn the volume down"),
            ["quieter", "the", "volume"]
        );
        assert_eq!(raw_words("turn off shuffle"), ["turn", "off", "shuffle"]);
        assert_eq!(raw_words("set up the daemon"), ["setup", "the", "daemon"]);
        assert_eq!(raw_words("put it up next"), ["put", "it", "up", "next"]);
        // Spotify.app coming to the front is one word, whatever the verb.
        for (phrase, words) in [
            (
                "keep Spotify from jumping to the front",
                &["keep", "spotify", "from", "frontmost", "to", "the"][..],
            ),
            ("spotify pops up", &["spotify", "frontmost"]),
            ("it steals focus", &["it", "frontmost"]),
            (
                "brings Spotify.app to the front",
                &["frontmost", "spotify", "to", "the"],
            ),
            ("bring spotify up", &["frontmost", "spotify"]),
            (
                "spotify activates itself",
                &["spotify", "frontmost", "itself"],
            ),
            (
                "it stays in front of my editor",
                &["it", "frontmost", "in", "of", "my", "editor"],
            ),
        ] {
            assert_eq!(raw_words(phrase), words, "{phrase}");
        }
        // Not those: a seek, the queue's front, lyrics brought up, a re-activated registration.
        for phrase in [
            "jump to 1:30",
            "jump forward 30 seconds",
            "jump to the front of the queue",
            "bring up the lyrics",
            "re-activate this silicon",
            "activate the trigger",
            "come up with a playlist",
        ] {
            assert!(
                !raw_words(phrase).contains(&"frontmost".to_owned()),
                "{phrase}: {:?}",
                raw_words(phrase)
            );
        }
        // "error" says little next to the words that name the problem.
        let query = query_terms("error: daemon not running");
        let weight = |t: &str| query.iter().find(|q| q.term == t).map(|q| q.weight);
        assert_eq!(weight("daemon"), Some(1.0));
        assert!(weight("error").is_some_and(|w| w < 0.5), "{query:?}");
        assert!(weight("unavailabl").is_some_and(|w| w > 0.4), "{query:?}");
    }

    #[test]
    fn the_example_shown_fits_the_question() {
        for (question, expected) in [
            ("remind me when the song is half over", "--elapsed 50%"),
            ("tell me when the song changes", "--change"),
            ("turn it up", "+10"),
        ] {
            let value = answer(question, 3);
            let first = value["commands"][0]["examples"][0]["command"]
                .as_str()
                .unwrap_or("");
            assert!(first.contains(expected), "{question}: {value}");
        }
    }

    #[test]
    fn guide_answers_are_not_padded_with_unrelated_commands() {
        let value = answer("what do the exit codes mean", 3);
        assert_eq!(value["guides"][0]["section"], "Exit codes", "{value}");
        // The table's header row is no excerpt.
        assert_ne!(value["guides"][0]["excerpt"], "Exit · Meaning", "{value}");
        let found = top("what do the exit codes mean");
        assert!(!found.iter().any(|p| p == "testing exit"), "{found:?}");
        let value = answer("premium required", 3);
        assert_eq!(value["errors"][0]["code"], "premium_required", "{value}");
    }

    #[test]
    fn concept_questions_point_to_the_guide() {
        let value = answer("what is an SLT", 3);
        let excerpt = value["guides"][0]["excerpt"].as_str().unwrap_or("");
        assert!(excerpt.starts_with("SLT (short-lived token)"), "{value}");
        assert!(
            top("what is an SLT")
                .iter()
                .any(|p| p == "login" || p == "iam"),
            "{value}"
        );
    }

    #[test]
    fn failures_point_to_the_error_code() {
        let value = answer("why is automation denied", 3);
        assert_eq!(
            value["errors"][0]["code"], "automation_permission_denied",
            "{value}"
        );
        assert_eq!(value["errors"][0]["exit"], 4);
        let found = top("why is automation denied");
        assert_eq!(
            found.first().map(String::as_str),
            Some("doctor"),
            "{found:?}"
        );
        assert!(!found.iter().any(|p| p == "how"), "{found:?}");
    }

    #[test]
    fn guide_sections_skip_code_blocks() {
        let sections =
            sections("# Title\nintro\n## One\ntext\n```sh\n# not a heading\n```\n### Two\nmore\n");
        let headings: Vec<&str> = sections.iter().map(|(_, h, _)| h.as_str()).collect();
        assert_eq!(headings, ["Title", "One", "Two"]);
        assert!(sections[1].2.contains("# not a heading"));
        assert_eq!(
            defined_term("- **SLT** (short-lived token): …"),
            Some("SLT")
        );
        assert_eq!(defined_term("- plain"), None);
    }

    #[test]
    fn excerpts_keep_code_lines_whole() {
        let body = "Prose here.\n\n```sh\niam login \\\n  | jq -r .slt | spotify login -\n```\n| a | b |\n";
        let blocks = blocks(body);
        assert_eq!(
            blocks,
            [
                "Prose here.",
                "`iam login | jq -r .slt | spotify login -`",
                "| a | b |"
            ]
        );
        assert_eq!(
            plain(&blocks[1]),
            "iam login | jq -r .slt | spotify login -"
        );
        assert_eq!(plain(&blocks[2]), "a · b");
    }

    fn first_examples(value: &Value) -> Vec<String> {
        value["commands"][0]["examples"]
            .as_array()
            .map(|e| {
                e.iter()
                    .filter_map(|e| e["command"].as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The questions a newcomer asked that 0.1.5 answered with the wrong command: a song's name
    /// outweighed "lyrics", "what's next" found `next`, "make a playlist from another" found
    /// `playlist create`, settings found `search`, "every command" found `devices list`.
    #[test]
    fn newcomer_questions_find_the_right_command() {
        // (question, the commands that may come first, commands that must be in the top 3)
        let queue: &[&str] = &["queue", "queue list"];
        for (question, first, among) in [
            (
                "get the lyrics of 505 by arctic monkeys without playing it",
                &["lyrics"][..],
                &[][..],
            ),
            ("lyrics 505 arctic monkeys", &["lyrics"], &[]),
            ("show me the lyrics of bohemian rhapsody", &["lyrics"], &[]),
            ("see what's up next", queue, &[]),
            ("show me what's next", queue, &[]),
            ("what is next", queue, &[]),
            ("what's up next in my queue", queue, &[]),
            ("show my queue", queue, &[]),
            ("what's playing", &["status"], &[]),
            (
                "make a playlist from another playlist",
                &["playlist fork"],
                &["playlist import"],
            ),
            (
                "create a playlist from an existing playlist",
                &["playlist fork"],
                &[],
            ),
            ("duplicate a playlist", &["playlist fork"], &[]),
            ("make a copy of my playlist", &["playlist fork"], &[]),
            (
                "keep the copy of a playlist up to date",
                &["playlist sync"],
                &[],
            ),
            (
                "set the default number of search results",
                &["config set"],
                &[],
            ),
            (
                "change the default number of search results",
                &["config set"],
                &[],
            ),
            ("always print json", &["config set"], &[]),
            ("turn off automatic updates", &["config set"], &["update"]),
            (
                "wait longer before falling back to applescript",
                &["config set"],
                &[],
            ),
            (
                "route trigger notifications to my isi",
                &["config set", "trigger"],
                &["config set"],
            ),
            ("list all commands as json", &["commands"], &[]),
            ("machine-readable list of every command", &["commands"], &[]),
            ("which commands are read only", &["commands"], &[]),
            ("search the docs for --random", &["docs"], &[]),
            ("how do I get the iam cli", &["iam"], &[]),
        ] {
            let found = top(question);
            assert!(
                found.first().is_some_and(|f| first.contains(&f.as_str())),
                "{question}: {found:?}, expected first one of {first:?}"
            );
            for expected in among {
                assert!(
                    found.iter().any(|f| f == expected),
                    "{question}: {found:?}, expected {expected} among them"
                );
            }
        }
        // Nor an example that changes it.
        for question in ["what's in my queue", "show my queue"] {
            let value = answer(question, 3);
            for example in first_examples(&value) {
                assert!(!example.contains("queue add"), "{question}: {value}");
            }
        }
        // Read-only questions never put a command that changes the queue first.
        for question in ["see what's up next", "what is next", "show my queue"] {
            let found = top(question);
            let read = found.iter().position(|p| queue.contains(&p.as_str()));
            for write in [
                "next",
                "queue add",
                "queue clear",
                "queue move",
                "queue remove",
            ] {
                if let Some(at) = found.iter().position(|p| p == write) {
                    assert!(read.is_some_and(|r| r < at), "{question}: {found:?}");
                }
            }
        }
    }

    #[test]
    fn the_examples_are_made_for_the_question() {
        for question in [
            "get the lyrics of 505 by arctic monkeys without playing it",
            "lyrics 505 arctic monkeys",
        ] {
            let value = answer(question, 3);
            assert_eq!(value["name"], "505 arctic monkeys", "{value}");
            assert_eq!(
                first_examples(&value)[..2],
                [
                    "spotify search '505 arctic monkeys' --type track",
                    "spotify lyrics <uri>"
                ],
                "{value}"
            );
        }
        // Quoted words are the name as it is.
        let value = answer("what are the words to \"Don't Stop Me Now\"", 3);
        assert_eq!(
            first_examples(&value)[0],
            r"spotify search 'Don'\''t Stop Me Now' --type track",
            "{value}"
        );
        // Commands that search by themselves get the name there.
        let value = answer("play 505 by arctic monkeys", 3);
        assert_eq!(
            first_examples(&value)[0],
            "spotify play --search '505 arctic monkeys'",
            "{value}"
        );
        // No name: the help's own examples.
        let value = answer("lyrics of a song that isn't playing", 3);
        assert_eq!(value["name"], Value::Null, "{value}");
        // Settings: the key asked about, with the value asked for where the question gives one.
        for (question, example) in [
            (
                "set the default number of search results",
                r#"spotify config set '{"search_limit": 5}'"#,
            ),
            (
                "set search results to 3 by default",
                r#"spotify config set '{"search_limit": 3}'"#,
            ),
            (
                "always print json",
                r#"spotify config set '{"output": "json"}'"#,
            ),
            (
                "turn off automatic updates",
                r#"spotify config set '{"auto_update": false}'"#,
            ),
            (
                "wait longer before falling back to applescript",
                r#"spotify config set '{"verify_timeout_ms": 5000}'"#,
            ),
            (
                "don't start spotify automatically",
                r#"spotify config set '{"launch_spotify": false}'"#,
            ),
            (
                "use applescript only",
                r#"spotify config set '{"strategy": "applescript"}'"#,
            ),
            (
                "never use applescript",
                r#"spotify config set '{"strategy": "spotify_player"}'"#,
            ),
        ] {
            let value = answer(question, 3);
            assert_eq!(
                value["commands"][0]["path"], "config set",
                "{question}: {value}"
            );
            assert_eq!(first_examples(&value)[0], example, "{question}: {value}");
        }
        // The command list, machine-readable.
        for question in [
            "list all commands as json",
            "machine-readable list of every command",
        ] {
            let value = answer(question, 3);
            assert_eq!(
                first_examples(&value)[0],
                "spotify commands --json",
                "{question}: {value}"
            );
        }
        // Tab completion: every line, for the shell asked about (zsh when none is).
        for (question, lines) in [
            (
                "tab completion for zsh",
                [
                    "mkdir -p ~/.zfunc && spotify completions zsh > ~/.zfunc/_spotify",
                    "echo 'fpath=(~/.zfunc $fpath); autoload -Uz compinit && compinit' >> ~/.zshrc",
                ],
            ),
            (
                "set up tab completion for bash",
                [
                    "mkdir -p ~/.bash_completion.d && spotify completions bash > ~/.bash_completion.d/spotify",
                    "echo 'source ~/.bash_completion.d/spotify' >> ~/.bashrc",
                ],
            ),
            (
                "fish completions",
                [
                    "mkdir -p ~/.config/fish/completions",
                    "spotify completions fish > ~/.config/fish/completions/spotify.fish",
                ],
            ),
        ] {
            let value = answer(question, 3);
            assert_eq!(first_examples(&value)[..2], lines, "{question}: {value}");
        }
        let value = answer("search the docs for --random", 3);
        assert_eq!(
            first_examples(&value)[0],
            "spotify docs --search '--random'",
            "{value}"
        );
    }

    #[test]
    fn a_search_query_in_an_example_is_no_meaning() {
        assert_eq!(
            without_search_queries("spotify play --search 'arctic monkeys 505' --type track"),
            "spotify play --search '…' --type track"
        );
        assert_eq!(
            without_search_queries("spotify search \"daily news\" and search='x y'"),
            "spotify search '…' and search '…'"
        );
        assert_eq!(
            without_search_queries("Search every guide; --search 'a"),
            "Search every guide; --search 'a"
        );
        assert!(!terms("spotify search 'arctic monkeys 505'").contains(&"arctic".to_owned()));
    }

    #[test]
    fn a_value_typed_in_an_example_is_no_meaning() {
        assert_eq!(
            without_quoted_values(
                "spotify playlist create 'Road trip' --collab --json | jq -r .id"
            ),
            "spotify playlist create '…' --collab --json | jq -r .id"
        );
        assert_eq!(
            without_quoted_values("--note 'wrap up the call'; --name='Kitchen speaker'"),
            "--note '…'; --name='…'"
        );
        // JSON, jq filters and apostrophes inside words stay.
        for text in [
            r#"spotify config set '{"search_limit": 5}'"#,
            "spotify lyrics --json | jq -r '.lines[]'",
            "Spotify's own queue and Liked Songs' shuffle",
            "an 'open quote with no end",
        ] {
            assert_eq!(without_quoted_values(text), text);
        }
        assert!(
            !doc_terms("spotify playlist create 'Road trip'")
                .iter()
                .any(|(t, _)| t == "road")
        );
    }

    /// Everyday questions beyond the newcomer's: a playlist's or album's name in the question, a
    /// command whose examples run another one, a word another command also claims.
    #[test]
    fn everyday_questions_find_the_right_command() {
        for (question, expected, example) in [
            ("add this song to my liked songs", "like", "spotify like"),
            ("like the current song", "like", "spotify like"),
            ("save this track", "like", "spotify like"),
            (
                "add hotel california to my road trip playlist",
                "playlist add",
                "spotify search 'hotel california' --type track",
            ),
            (
                "add Stairway to Heaven to my Road Trip playlist",
                "playlist add",
                "spotify search 'Stairway to Heaven' --type track",
            ),
            (
                "play the album OK Computer",
                "play",
                "spotify play --search 'OK Computer' --type album",
            ),
            (
                "lyrics of Don't Stop Me Now",
                "lyrics",
                r"spotify search 'Don'\''t Stop Me Now' --type track",
            ),
            (
                "restart the daemon",
                "daemon restart",
                "spotify daemon restart",
            ),
            (
                "which devices are available",
                "devices list",
                "spotify devices list",
            ),
            (
                "switch to another device",
                "devices connect",
                "spotify devices connect",
            ),
            (
                "remove the third song from the queue",
                "queue remove",
                "spotify queue remove",
            ),
            (
                "queue up stairway to heaven",
                "queue add",
                "spotify queue add --search",
            ),
            (
                "play on the Living Room speaker",
                "devices connect",
                "spotify devices connect --name 'Living Room'",
            ),
            ("turn the volume down to 30", "volume", "spotify volume 30"),
            ("turn it up by 20", "volume", "spotify volume +20"),
            ("set the volume to 50%", "volume", "spotify volume 50"),
            ("mute", "volume", "spotify volume 0"),
        ] {
            let value = answer(question, 3);
            assert_eq!(
                value["commands"][0]["path"], expected,
                "{question}: {value}"
            );
            assert!(
                first_examples(&value)[0].starts_with(example),
                "{question}: {value}"
            );
        }
        // A number is a value, not a name; the product's own words in capitals are no name.
        for question in [
            "turn the volume down to 30",
            "set search results to 3 by default",
            "play my Liked Songs shuffled",
            "what is an SLT",
            "How Do I Skip A Song",
        ] {
            assert_eq!(answer(question, 3)["name"], Value::Null, "{question}");
        }
    }

    /// The questions the second newcomer asked: "jumping to the front" found `seek` and never
    /// named keep_spotify_in_background, "which commands change my library" found `library`
    /// (which only reads), and a song's album could not be seen without playing it.
    #[test]
    fn newcomer_2_questions_find_the_right_command() {
        // Spotify.app coming to the front is keep_spotify_in_background. It is on by default, so
        // the answer says so, checks it, and shows how to turn it off only if wanted.
        for question in [
            "keep Spotify from jumping to the front when playing",
            "spotify keeps stealing focus",
            "stop spotify popping up when a song starts",
            "spotify comes to the front every time a song starts",
            "spotify activates itself when I play something",
            "don't let spotify come to the front",
            "keep spotify in the background",
        ] {
            let value = answer(question, 3);
            assert_eq!(
                value["commands"][0]["path"], "config get",
                "{question}: {value}"
            );
            assert_eq!(
                first_examples(&value),
                [
                    "spotify config get keep_spotify_in_background",
                    r#"spotify config set '{"keep_spotify_in_background": false}'"#,
                ],
                "{question}: {value}"
            );
            let note = value["commands"][0]["note"].as_str().unwrap_or("");
            assert!(
                note.starts_with("keep_spotify_in_background is true by default (already on)"),
                "{question}: {note}"
            );
            assert!(!top(question).iter().any(|p| p == "seek"), "{question}");
            let text = render(&value);
            for said in [
                "spotify config get keep_spotify_in_background  ",
                "Check it: true (on) by default",
                "Only to turn it off",
                "Note: keep_spotify_in_background is true by default (already on)",
            ] {
                assert!(text.contains(said), "{question}: {text}");
            }
        }
        // Turning it off is asked for in so many words.
        let value = answer("turn off keep spotify in background", 3);
        assert_eq!(value["commands"][0]["path"], "config set", "{value}");
        assert_eq!(
            first_examples(&value)[0],
            r#"spotify config set '{"keep_spotify_in_background": false}'"#
        );
        // `jump` alone is still a seek, and the front of the queue the queue's.
        for (question, expected) in [
            ("jump to 1:30", "seek"),
            ("jump forward 30 seconds", "seek"),
            ("put a song at the front of the queue", "queue add"),
        ] {
            assert_eq!(
                top(question).first().map(String::as_str),
                Some(expected),
                "{question}"
            );
        }

        // What changes the library: the manifest's filter first, then the commands themselves.
        let value = answer("which commands change my library", 3);
        assert_eq!(
            top("which commands change my library"),
            ["commands", "like", "unlike"]
        );
        assert_eq!(
            first_examples(&value)[0],
            r#"spotify commands --json | jq -r '.commands[] | select(.changes | index("library")).command'"#,
            "{value}"
        );
        assert!(
            first_examples(&value)
                .iter()
                .any(|e| e == "spotify commands"),
            "{value}"
        );
        let value = answer("which commands modify playlists", 3);
        assert_eq!(value["commands"][0]["path"], "commands", "{value}");
        assert!(
            first_examples(&value)[0].contains(r#"index("playlists")"#),
            "{value}"
        );
        // Read-only: the documented filter, read_only_when included.
        let value = answer("which commands are read only", 3);
        assert_eq!(
            first_examples(&value)[0],
            "spotify commands --json | jq -r '.commands[] | select(.mutates == false or .read_only_when != null).command'",
            "{value}"
        );
        // "tell me when the song changes" is still a trigger, not a question about commands.
        let found = top("which commands tell me when the song changes");
        assert!(
            found.first().is_some_and(|p| p.starts_with("trigger")),
            "{found:?}"
        );

        // A song's album, without playing anything: `track` with a song's URI.
        let question = "see the album a track belongs to without playing it";
        let value = answer(question, 3);
        assert_eq!(value["commands"][0]["path"], "track", "{value}");
        assert_eq!(
            first_examples(&value)[0],
            "spotify track spotify:track:0BxE4FqsDD1Ot4YuBXwAPp",
            "{value}"
        );
        assert!(
            value["commands"][0]["examples"][0]["description"]
                .as_str()
                .is_some_and(|d| d.contains("album") && d.contains("nothing plays")),
            "{value}"
        );
        for example in first_examples(&value) {
            assert!(!example.starts_with("spotify play"), "{question}: {value}");
        }
    }

    /// "errors", "codes" or "arguments" in a question say what it is about, not that something
    /// failed: no error rows that only share such a word.
    #[test]
    fn words_like_errors_or_arguments_add_no_unrelated_error_cards() {
        for question in [
            "which errors can search return",
            "list every command with its arguments and errors",
            "what are the error codes",
            "what errors can arguments give",
            "how do I see the errors a command returns",
            "what arguments does play take",
            "keep Spotify from jumping to the front when playing",
        ] {
            let value = answer(question, 3);
            assert_eq!(value["errors"], json!([]), "{question}: {value}");
        }
        // Trouble still gets the code that fits.
        for (question, code) in [
            ("why is automation denied", "automation_permission_denied"),
            ("premium required", "premium_required"),
            ("error: daemon not running", "daemon_unavailable"),
            ("why does play fail", "applescript_failed"),
            // A code named as written comes first, trouble words or not.
            ("spotify says track_mismatch", "track_mismatch"),
            ("what does daemon_unavailable mean", "daemon_unavailable"),
        ] {
            let value = answer(question, 3);
            assert_eq!(value["errors"][0]["code"], code, "{question}: {value}");
        }
        // "why" alone names no error: applescript_failed's row says "without saying why", and
        // most rows' fixes say `spotify doctor`, which "won't work" stands for.
        for question in [
            "why is the volume so low",
            "why is shuffle off",
            "why won't my tab completion work in zsh -f",
            "why doesn't the lyrics command show anything",
            "why can't I see my playlists",
            // A setting answers it, not the row that says "applescript" too.
            "use applescript only",
        ] {
            let value = answer(question, 3);
            assert!(
                value["errors"]
                    .as_array()
                    .is_some_and(|e| e.iter().all(|e| e["code"] != "applescript_failed")),
                "{question}: {value}"
            );
        }
    }

    #[test]
    fn inverted_why_questions_are_about_their_subject() {
        // "why doesn't the lyrics command…": `lyrics` is what fails, not what is denied.
        for (question, expected) in [
            ("why doesn't the lyrics command show anything", "lyrics"),
            ("why can't I see my playlists", "playlist list"),
            ("why won't my tab completion work in zsh -f", "completions"),
            // The trouble words still count.
            ("why isn't the daemon running", "doctor"),
            ("why doesn't spotify respond", "doctor"),
            ("why doesn't it work", "doctor"),
            // Not after "why": still negated.
            ("lyrics of a song that isn't playing", "lyrics"),
        ] {
            let found = top(question);
            assert_eq!(
                found.first().map(String::as_str),
                Some(expected),
                "{question}: {found:?}"
            );
        }
        assert_eq!(
            marked_terms("why doesn't the lyrics command show"),
            [("why".to_owned(), false), (stem("lyrics"), false)]
        );
    }

    #[test]
    fn exit_codes_and_typed_commands() {
        // "exit code" is one word: not `testing exit`, and the number is the code.
        assert_eq!(raw_words("exit code 3"), ["exitcode", "3"]);
        for question in [
            "what does exit code 4 mean",
            "the exit code is 3",
            "exit code 5",
            "what do the exit codes mean",
        ] {
            let value = answer(question, 3);
            assert_eq!(value["commands"], json!([]), "{question}: {value}");
            assert_eq!(
                value["guides"][0]["section"], "Exit codes",
                "{question}: {value}"
            );
        }
        // A command named as it is typed is the one asked about.
        for (question, expected) in [
            ("spotify play returns exit code 1", "play"),
            ("spotify queue add fails", "queue add"),
        ] {
            let found = top(question);
            assert_eq!(
                found.first().map(String::as_str),
                Some(expected),
                "{question}: {found:?}"
            );
        }
        let value = answer("spotify play returns exit code 1", 3);
        assert!(
            value["guides"]
                .as_array()
                .is_some_and(|g| g.iter().any(|g| g["section"] == "Exit codes")),
            "{value}"
        );
        // "jump" is a seek, except to the next or the previous item.
        for (question, expected) in [
            ("jump to the next song", "next"),
            ("jump to the next track", "next"),
            ("jump to the previous song", "previous"),
            ("jump to 1:30", "seek"),
            ("jump to the middle of the song", "seek"),
        ] {
            let found = top(question);
            assert_eq!(
                found.first().map(String::as_str),
                Some(expected),
                "{question}: {found:?}"
            );
        }
    }

    #[test]
    fn hint_questions_get_the_hints_variable() {
        for (question, example) in [
            ("turn off next hints", "SPOTIFY_HINTS=0 spotify status"),
            ("hints are annoying", "SPOTIFY_HINTS=0 spotify status"),
            (
                "show hints when piped",
                "SPOTIFY_HINTS=always spotify status | cat",
            ),
        ] {
            let value = answer(question, 3);
            assert_eq!(
                value["commands"][0]["path"], "config",
                "{question}: {value}"
            );
            assert_eq!(
                value["commands"][0]["examples"][0]["command"], example,
                "{question}: {value}"
            );
        }
    }

    #[test]
    fn questions_about_what_commands_change() {
        // A thing that is no `changes` tag is its own command's.
        for (question, expected) in [
            ("what commands change the volume", "volume"),
            ("which commands change shuffle", "shuffle"),
        ] {
            let found = top(question);
            assert!(
                found.iter().take(2).any(|f| f == expected),
                "{question}: {found:?}"
            );
            assert_ne!(found.first().map(String::as_str), Some("commands"));
        }
        for (question, filter) in [
            (
                "which commands don't change anything",
                "select(.mutates == false or .read_only_when != null)",
            ),
            (
                "what commands do not change anything",
                "select(.mutates == false or .read_only_when != null)",
            ),
            (
                "which commands change nothing",
                "select(.mutates == false or .read_only_when != null)",
            ),
            (
                "which commands modify something",
                "select(.mutates).command",
            ),
        ] {
            let value = answer(question, 3);
            assert_eq!(
                value["commands"][0]["path"], "commands",
                "{question}: {value}"
            );
            assert!(
                value["commands"][0]["examples"][0]["command"]
                    .as_str()
                    .is_some_and(|e| e.contains(filter)),
                "{question}: {value}"
            );
        }
        // The filter, then what it lists: nothing else that says "playlist".
        assert_eq!(
            top("which commands modify my playlists"),
            ["commands", "playlist create", "playlist delete"]
        );
        assert_eq!(
            top("which commands can change my settings"),
            ["commands", "config set", "config reset"]
        );
        // "modify" is a question's word, not a song's name.
        let value = answer("which commands do not modify my queue", 3);
        assert_eq!(value["name"], Value::Null, "{value}");
    }

    #[test]
    fn every_setting_example_is_a_setting_config_set_takes() {
        for (key, ..) in SETTING_KEYWORDS {
            assert!(
                CONFIG_KEYS.iter().any(|(k, ..)| k == key),
                "SETTING_KEYWORDS names no setting `{key}`"
            );
        }
        let config = silicon_spotify_client::store::Config::default();
        for (at, (key, ..)) in CONFIG_KEYS.iter().enumerate() {
            let value = setting_value(at, &[]);
            if value.as_str().is_some_and(|v| v.starts_with('<')) {
                continue;
            }
            let object = json!({ *key: value }).to_string();
            config
                .apply_json(&object)
                .unwrap_or_else(|e| panic!("{object}: {}", e.message));
            // The change a check card offers is one config set takes too.
            let change = setting_check(at)[1].command.clone();
            let object = change
                .strip_prefix("spotify config set '")
                .and_then(|c| c.strip_suffix('\''))
                .unwrap_or_default();
            if !object.contains('<') {
                config
                    .apply_json(object)
                    .unwrap_or_else(|e| panic!("{change}: {}", e.message));
            }
        }
    }

    #[test]
    fn nothing_matching_says_where_to_look() {
        let value = answer("zzqx", 3);
        assert!(render(&value).starts_with("Nothing matches"), "{value}");
    }

    #[test]
    fn rendered_answers_show_examples_and_the_guide() {
        let text = render(&answer("notify me 30 seconds before the song ends", 3));
        assert!(text.starts_with("spotify trigger add — "), "{text}");
        assert!(
            text.contains("\n  spotify trigger add --remaining 30s"),
            "{text}"
        );
        assert!(
            text.contains("Read: spotify docs triggers --section "),
            "{text}"
        );
    }
}
