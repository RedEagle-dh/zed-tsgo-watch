//! Parser for `tsc --pretty false --listFiles` output.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    Error,
    Warning,
    Suggestion,
    Message,
}

impl Category {
    fn from_word(word: &str) -> Option<Self> {
        match word {
            "error" => Some(Self::Error),
            "warning" => Some(Self::Warning),
            "suggestion" => Some(Self::Suggestion),
            "message" => Some(Self::Message),
            _ => None,
        }
    }
}

/// One diagnostic as printed by tsc. `file` is `None` for global diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawDiagnostic {
    pub file: Option<String>,
    /// 1-based line.
    pub line: u32,
    /// 1-based column in UTF-16 code units.
    pub column: u32,
    pub category: Category,
    pub code: u32,
    pub message: String,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Output {
    pub diagnostics: Vec<RawDiagnostic>,
    /// Absolute paths from `--listFiles`, as printed.
    pub files: Vec<String>,
}

pub fn parse(stdout: &str) -> Output {
    let mut out = Output::default();
    for raw_line in stdout.lines() {
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("  ") {
            if let Some(last) = out.diagnostics.last_mut() {
                last.message.push('\n');
                last.message.push_str(rest);
            }
            continue;
        }
        if let Some(diagnostic) = parse_global(line).or_else(|| parse_located(line)) {
            out.diagnostics.push(diagnostic);
        } else if is_absolute(line) {
            out.files.push(line.to_string());
        }
    }
    out
}

fn is_absolute(line: &str) -> bool {
    let bytes = line.as_bytes();
    line.starts_with('/')
        || line.starts_with("\\\\")
        || (bytes.len() > 2
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && matches!(bytes[2], b'/' | b'\\'))
}

/// `<category> TS<code>: <message>`
fn parse_head(text: &str) -> Option<(Category, u32, &str)> {
    let (word, rest) = text.split_once(' ')?;
    let category = Category::from_word(word)?;
    let rest = rest.strip_prefix("TS")?;
    let (code, message) = rest.split_once(": ")?;
    let code = code.parse().ok()?;
    Some((category, code, message))
}

fn parse_global(line: &str) -> Option<RawDiagnostic> {
    let (category, code, message) = parse_head(line)?;
    Some(RawDiagnostic {
        file: None,
        line: 0,
        column: 0,
        category,
        code,
        message: message.to_string(),
    })
}

/// `<file>(<line>,<column>): <category> TS<code>: <message>`
fn parse_located(line: &str) -> Option<RawDiagnostic> {
    let mut search = 0;
    while let Some(found) = line[search..].find("): ") {
        let close = search + found;
        search = close + 3;
        let Some((category, code, message)) = parse_head(&line[close + 3..]) else {
            continue;
        };
        let head = &line[..close];
        let Some(open) = head.rfind('(') else {
            continue;
        };
        let Some((l, c)) = head[open + 1..].split_once(',') else {
            continue;
        };
        let (Ok(l), Ok(c)) = (l.parse(), c.parse()) else {
            continue;
        };
        if open == 0 {
            continue;
        }
        return Some(RawDiagnostic {
            file: Some(head[..open].to_string()),
            line: l,
            column: c,
            category,
            code,
            message: message.to_string(),
        });
    }
    None
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

/// Given a line and a 0-based start column in UTF-16 units, returns the 0-based end column of the
/// word that starts there, or one character further when there is no word.
pub fn word_end(line: &str, start: u32) -> u32 {
    let mut position = 0u32;
    let mut chars = line.chars();
    for c in chars.by_ref() {
        if position >= start {
            if !is_word_char(c) {
                return start + c.len_utf16() as u32;
            }
            position += c.len_utf16() as u32;
            break;
        }
        position += c.len_utf16() as u32;
    }
    if position <= start {
        return start;
    }
    for c in chars {
        if !is_word_char(c) {
            break;
        }
        position += c.len_utf16() as u32;
    }
    position
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_diagnostics_continuations_and_files() {
        let stdout = "\
src/main.ts(7,7): error TS2322: Type 'B' is not assignable to type 'A'.
  Types have separate declarations of a private property 'p'.
    Deeper detail.
../../packages/ui/src/index.ts(3,14): error TS2322: Type 'string' is not assignable to type 'number'.
/w/node_modules/typescript/lib/lib.es5.d.ts
/w/src/main.ts
";
        let out = parse(stdout);
        assert_eq!(out.diagnostics.len(), 2);
        let first = &out.diagnostics[0];
        assert_eq!(first.file.as_deref(), Some("src/main.ts"));
        assert_eq!((first.line, first.column, first.code), (7, 7, 2322));
        assert_eq!(first.category, Category::Error);
        assert_eq!(
            first.message,
            "Type 'B' is not assignable to type 'A'.\nTypes have separate declarations of a private property 'p'.\n  Deeper detail."
        );
        assert_eq!(
            out.diagnostics[1].file.as_deref(),
            Some("../../packages/ui/src/index.ts")
        );
        assert_eq!(
            out.files,
            vec![
                "/w/node_modules/typescript/lib/lib.es5.d.ts",
                "/w/src/main.ts"
            ]
        );
    }

    #[test]
    fn parses_global_and_config_errors() {
        let stdout = "\
error TS5083: Cannot read file '/w/missing.json'.
tsconfig.json(1,34): error TS5024: Compiler option 'strict' requires a value of type boolean.
";
        let out = parse(stdout);
        assert_eq!(out.diagnostics[0].file, None);
        assert_eq!(out.diagnostics[0].code, 5083);
        assert_eq!(
            out.diagnostics[0].message,
            "Cannot read file '/w/missing.json'."
        );
        assert_eq!(out.diagnostics[1].file.as_deref(), Some("tsconfig.json"));
        assert_eq!(
            (out.diagnostics[1].line, out.diagnostics[1].column),
            (1, 34)
        );
        assert!(out.files.is_empty());
    }

    #[test]
    fn parses_other_categories_odd_paths_and_crlf() {
        let stdout = "a (1).ts(2,3): warning TS6133: 'x' is declared but its value is never read.\r\n\
C:/w/b.ts(1,1): message TS1: m\r\n\
C:/w/b.ts\r\n";
        let out = parse(stdout);
        assert_eq!(out.diagnostics[0].file.as_deref(), Some("a (1).ts"));
        assert_eq!(out.diagnostics[0].category, Category::Warning);
        assert_eq!(out.diagnostics[1].file.as_deref(), Some("C:/w/b.ts"));
        assert_eq!(out.diagnostics[1].category, Category::Message);
        assert_eq!(out.files, vec!["C:/w/b.ts"]);
    }

    #[test]
    fn ignores_noise() {
        let out = parse("Found 2 errors.\nsomething else\n");
        assert_eq!(out, Output::default());
    }

    #[test]
    fn word_end_counts_utf16() {
        // const s = "äöü 😀 日本"; const n: number = "x";
        let line = "const s = \"äöü 😀 日本\"; const n: number = \"x\";";
        // tsc reports column 30 (1-based) for `n`.
        assert_eq!(word_end(line, 29), 30);
        assert_eq!(word_end("\tconst t: number = 1;", 7), 8);
        assert_eq!(word_end("const value = 1;", 6), 11);
        // Non-word character: one character.
        assert_eq!(word_end("f({ x: 1 });", 2), 3);
        assert_eq!(word_end("a😀b", 1), 3);
        // Past the end of the line.
        assert_eq!(word_end("ab", 5), 5);
        assert_eq!(word_end("", 0), 0);
        // Identifiers with non-ASCII letters.
        assert_eq!(word_end("const 日本 = 1", 6), 8);
    }
}
