//! Shell words of the command lines written in help texts and guides, for the tests that check
//! them against the grammar. Test-only: shared by the unit tests (`catalog.rs`) and
//! `tests/docs.rs`.

/// The commands of a line, each as its words: quotes and backslashes are shell-like; `|`, `;`,
/// `&`, `(`, `)`, `$(`, a backtick and a `>` redirect end a command; `<placeholder>` is one word.
/// Leading `VAR=value` assignments and `sudo`/`time`/`env` are dropped.
#[must_use]
pub fn commands(line: &str) -> Vec<Vec<String>> {
    let mut out: Vec<Vec<String>> = Vec::new();
    let mut words: Vec<String> = Vec::new();
    let mut word = String::new();
    let mut started = false;
    let mut chars = line.chars().peekable();
    let end_word = |word: &mut String, started: &mut bool, words: &mut Vec<String>| {
        if *started {
            words.push(std::mem::take(word));
            *started = false;
        }
    };
    let end_command = |words: &mut Vec<String>, out: &mut Vec<Vec<String>>| {
        if !words.is_empty() {
            out.push(std::mem::take(words));
        }
    };
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                started = true;
                for q in chars.by_ref() {
                    if q == '\'' {
                        break;
                    }
                    word.push(q);
                }
            }
            '"' => {
                started = true;
                while let Some(q) = chars.next() {
                    match q {
                        '"' => break,
                        '\\' => {
                            if let Some(escaped) = chars.next() {
                                word.push(escaped);
                            }
                        }
                        _ => word.push(q),
                    }
                }
            }
            '\\' => {
                if let Some(escaped) = chars.next()
                    && escaped != '\n'
                {
                    word.push(escaped);
                    started = true;
                }
            }
            '<' => {
                // A placeholder such as `<id>` or `<playlist-id>`.
                started = true;
                word.push('<');
                for q in chars.by_ref() {
                    word.push(q);
                    if q == '>' {
                        break;
                    }
                }
            }
            ' ' | '\t' | '\n' => end_word(&mut word, &mut started, &mut words),
            // A comment, to the end of the line.
            '#' if !started => break,
            '$' if chars.peek() == Some(&'(') => {
                chars.next();
                end_word(&mut word, &mut started, &mut words);
                end_command(&mut words, &mut out);
            }
            '|' | ';' | '&' | '(' | ')' | '`' | '>' => {
                end_word(&mut word, &mut started, &mut words);
                end_command(&mut words, &mut out);
            }
            _ => {
                word.push(c);
                started = true;
            }
        }
    }
    end_word(&mut word, &mut started, &mut words);
    end_command(&mut words, &mut out);
    out.into_iter()
        .map(|mut words| {
            let skip = words
                .iter()
                .take_while(|w| {
                    matches!(
                        w.as_str(),
                        "sudo" | "time" | "env" | "exec" | "then" | "do" | "!"
                    ) || w.split_once('=').is_some_and(|(name, _)| {
                        !name.is_empty()
                            && name
                                .chars()
                                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
                    })
                })
                .count();
            words.drain(..skip);
            words
        })
        .filter(|words| !words.is_empty())
        .collect()
}

/// The `spotify …` commands of a line.
#[must_use]
pub fn spotify_commands(line: &str) -> Vec<Vec<String>> {
    commands(line)
        .into_iter()
        .filter(|words| words.first().is_some_and(|w| w == "spotify"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_split_into_commands() {
        assert_eq!(
            spotify_commands(
                "printf %s \"$X\" | spotify testing use --app-secret-file - && spotify login 'oac_…'"
            ),
            [
                vec!["spotify", "testing", "use", "--app-secret-file", "-"],
                vec!["spotify", "login", "oac_…"],
            ]
        );
        assert_eq!(
            spotify_commands("spotify completions zsh > ~/.zfunc/_spotify"),
            [vec!["spotify", "completions", "zsh"]]
        );
        assert_eq!(
            spotify_commands("spotify config set '{\"telemetry\": null}'"),
            [vec!["spotify", "config", "set", "{\"telemetry\": null}"]]
        );
        assert_eq!(
            spotify_commands("SPOTIFY_HINTS=0 spotify trigger show <trigger-id>"),
            [vec!["spotify", "trigger", "show", "<trigger-id>"]]
        );
        assert_eq!(
            spotify_commands("iam silicon-login --app-id spotify --grant-org <org>"),
            Vec::<Vec<String>>::new()
        );
        assert_eq!(
            spotify_commands("spotify queue      # lists it; spotify frobnicate"),
            [vec!["spotify", "queue"]]
        );
        assert_eq!(
            spotify_commands("spotify report 'bug #3'"),
            [vec!["spotify", "report", "bug #3"]]
        );
        assert_eq!(
            spotify_commands("hint: Run `spotify auth login`: it opens"),
            [vec!["spotify", "auth", "login"]]
        );
    }
}
