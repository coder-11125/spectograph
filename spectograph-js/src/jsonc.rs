//! Lenient JSON ("JSON with comments") parsing.
//!
//! `tsconfig.json`/`jsconfig.json` (and `angular.json`, `.eslintrc.json`, ...) are routinely
//! written with `//` and `/* */` comments and trailing commas, which `serde_json` rejects.
//! Treating such a file as "absent" silently drops every `paths` alias it declares, which
//! turns every aliased import into an unresolved one and every imported file into a false
//! "guaranteed bloat" finding. This strips both before handing the text to `serde_json`.

use serde_json::Value;

/// Parse JSON that may contain comments and trailing commas.
pub fn parse(text: &str) -> Option<Value> {
    serde_json::from_str(&strip(text)).ok()
}

/// Remove `//` and `/* */` comments and trailing commas, leaving string contents intact.
pub fn strip(text: &str) -> String {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    remove_trailing_commas(&remove_comments(text))
}

fn remove_comments(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    let mut in_string = false;
    while i < chars.len() {
        let c = chars[i];
        if in_string {
            out.push(c);
            if c == '\\' {
                if let Some(&next) = chars.get(i + 1) {
                    out.push(next);
                    i += 1;
                }
            } else if c == '"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        match (c, chars.get(i + 1)) {
            ('"', _) => {
                in_string = true;
                out.push(c);
                i += 1;
            }
            ('/', Some('/')) => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            }
            ('/', Some('*')) => {
                i += 2;
                while i < chars.len() && !(chars[i] == '*' && chars.get(i + 1) == Some(&'/')) {
                    i += 1;
                }
                i += 2;
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

fn remove_trailing_commas(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut in_string = false;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if in_string {
            out.push(c);
            if c == '\\' {
                if let Some(&next) = chars.get(i + 1) {
                    out.push(next);
                    i += 1;
                }
            } else if c == '"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        if c == '"' {
            in_string = true;
        } else if c == ',' {
            let mut j = i + 1;
            while j < chars.len() && chars[j].is_whitespace() {
                j += 1;
            }
            if matches!(chars.get(j), Some('}') | Some(']')) {
                i += 1;
                continue;
            }
        }
        out.push(c);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_comments_and_trailing_commas() {
        let v = parse(
            r#"{
                // a line comment
                "compilerOptions": {
                    /* block */ "baseUrl": ".",
                    "paths": { "@/*": ["./src/*",], },
                },
            }"#,
        )
        .unwrap();
        assert_eq!(v["compilerOptions"]["baseUrl"], ".");
        assert_eq!(v["compilerOptions"]["paths"]["@/*"][0], "./src/*");
    }

    #[test]
    fn comment_markers_inside_strings_are_preserved() {
        let v = parse(r#"{"url": "http://example.com/*x*/", "a": "b,]"}"#).unwrap();
        assert_eq!(v["url"], "http://example.com/*x*/");
        assert_eq!(v["a"], "b,]");
    }

    #[test]
    fn tolerates_a_byte_order_mark() {
        assert!(parse("\u{feff}{\"a\": 1}").is_some());
    }

    #[test]
    fn garbage_is_none_not_a_panic() {
        assert!(parse("{invalid").is_none());
        assert!(parse("").is_none());
    }
}
