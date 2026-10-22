//! JSONC pre-pass (SPEC §12A.1): strips `//` and `/* */` comments and trailing commas.
//!
//! Stripped bytes become spaces and newlines stay, so byte offsets and line
//! numbers in later parse errors still point into the original file.

use crate::model::MAX_DEPTH;

/// Why a JSONC document was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum JsoncError {
    #[error("line {line}: unterminated block comment")]
    UnterminatedComment { line: usize },
    #[error("nesting deeper than {limit}")]
    TooDeep { limit: usize },
    #[error("json: {0}")]
    Json(String),
}

/// Returns `input` with comments and trailing commas blanked out.
pub fn strip(input: &[u8]) -> Result<Vec<u8>, JsoncError> {
    let mut out = input.to_vec();
    let mut i = 0;
    let mut line = 1;
    let mut depth = 0usize;
    let mut pending_comma: Option<usize> = None;
    if out.starts_with(&[0xEF, 0xBB, 0xBF]) {
        out[..3].fill(b' ');
        i = 3;
    }
    while i < out.len() {
        let b = out[i];
        match b {
            b'\n' => {
                line += 1;
                i += 1;
            }
            b' ' | b'\t' | b'\r' => i += 1,
            b'/' if out.get(i + 1) == Some(&b'/') => {
                while i < out.len() && out[i] != b'\n' {
                    out[i] = b' ';
                    i += 1;
                }
            }
            b'/' if out.get(i + 1) == Some(&b'*') => {
                let start_line = line;
                out[i] = b' ';
                out[i + 1] = b' ';
                i += 2;
                loop {
                    match out.get(i) {
                        None => return Err(JsoncError::UnterminatedComment { line: start_line }),
                        Some(b'*') if out.get(i + 1) == Some(&b'/') => {
                            out[i] = b' ';
                            out[i + 1] = b' ';
                            i += 2;
                            break;
                        }
                        Some(b'\n') => line += 1,
                        Some(_) => out[i] = b' ',
                    }
                    i += 1;
                }
            }
            b',' => {
                pending_comma = Some(i);
                i += 1;
            }
            b'"' => {
                pending_comma = None;
                i += 1;
                while i < out.len() {
                    match out[i] {
                        b'\\' => i += 1,
                        b'"' => break,
                        b'\n' => line += 1,
                        _ => {}
                    }
                    i += 1;
                }
                i += 1;
            }
            _ => {
                if let (b'}' | b']', Some(c)) = (b, pending_comma) {
                    out[c] = b' ';
                }
                pending_comma = None;
                match b {
                    b'{' | b'[' => {
                        depth += 1;
                        if depth > MAX_DEPTH {
                            return Err(JsoncError::TooDeep { limit: MAX_DEPTH });
                        }
                    }
                    b'}' | b']' => depth = depth.saturating_sub(1),
                    _ => {}
                }
                i += 1;
            }
        }
    }
    Ok(out)
}

/// Strips `input` and parses it as JSON.
pub fn parse(input: &[u8]) -> Result<serde_json::Value, JsoncError> {
    serde_json::from_slice(&strip(input)?).map_err(|e| JsoncError::Json(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ASC's `turbo.json`, reduced: line and block comments, inline arrays.
    const TURBO: &str = r#"{
  "$schema": "https://v2-10-11.turborepo.dev/schema.json",
  "ui": "tui",
  "tasks": {
    "build": {
      "dependsOn": ["^build"],
      "inputs": ["$TURBO_DEFAULT$", ".env*"],
      "outputs": ["dist/**", "js/dist/**"],
      "env": ["*"]
    },
    // soma-workspace is an orchestrator: its build is `bun run --filter '*' build`,
    // which delegates to the packages of a NESTED workspace.
    "soma-workspace#build": {
      "outputs": []
    },
    /* block
       comment */
    "dev": { "cache": false, "persistent": true }
  }
}"#;

    /// A `bun.lock` excerpt: trailing commas everywhere, `packages` as arrays.
    const BUN_LOCK: &str = r#"{
  "lockfileVersion": 1,
  "workspaces": {
    "": {
      "name": "@asc/private",
      "dependencies": {
        "zod": "^4.4.1",
      },
      "devDependencies": {
        "turbo": "^2.10.11",
      },
    },
  },
  "packages": {
    "zod": ["zod@4.4.1", "", {}, "sha512-abc//def=="],
    "turbo": ["turbo@2.10.11", "", { "optionalDependencies": { "turbo-linux-64": "2.10.11", } }, "sha512-xyz"],
  }
}
"#;

    #[test]
    fn turbo_json_parses() {
        let v = parse(TURBO.as_bytes()).unwrap();
        assert_eq!(v["tasks"]["build"]["dependsOn"][0], "^build");
        assert_eq!(
            v["tasks"]["soma-workspace#build"]["outputs"],
            serde_json::json!([])
        );
        assert_eq!(v["tasks"]["dev"]["persistent"], true);
    }

    #[test]
    fn bun_lock_parses_with_trailing_commas() {
        let v = parse(BUN_LOCK.as_bytes()).unwrap();
        assert_eq!(v["workspaces"][""]["dependencies"]["zod"], "^4.4.1");
        assert_eq!(v["packages"]["zod"][3], "sha512-abc//def==");
        assert_eq!(
            v["packages"]["turbo"][2]["optionalDependencies"]["turbo-linux-64"],
            "2.10.11"
        );
    }

    #[test]
    fn strip_preserves_offsets() {
        let s = strip(TURBO.as_bytes()).unwrap();
        assert_eq!(s.len(), TURBO.len());
        let nl = |b: &[u8]| b.iter().filter(|&&c| c == b'\n').count();
        assert_eq!(nl(&s), nl(TURBO.as_bytes()));
    }

    #[test]
    fn comment_markers_inside_strings_survive() {
        let v = parse(br#"{"a": "x // y /* z */", "b": "q\"//", }"#).unwrap();
        assert_eq!(v["a"], "x // y /* z */");
        assert_eq!(v["b"], "q\"//");
    }

    #[test]
    fn comma_before_comment_before_close() {
        let v = parse(b"[1, 2, // done\n /* x */ ]").unwrap();
        assert_eq!(v, serde_json::json!([1, 2]));
    }

    #[test]
    fn bom_is_skipped() {
        assert!(parse(b"\xEF\xBB\xBF{\"a\":1}").is_ok());
    }

    #[test]
    fn unterminated_block_comment_names_line() {
        assert_eq!(
            strip(b"{\n/* open\n}"),
            Err(JsoncError::UnterminatedComment { line: 2 })
        );
    }

    #[test]
    fn depth_limit() {
        let ok = format!("{}{}", "[".repeat(MAX_DEPTH), "]".repeat(MAX_DEPTH));
        assert!(strip(ok.as_bytes()).is_ok());
        let deep = format!("{}{}", "[".repeat(MAX_DEPTH + 1), "]".repeat(MAX_DEPTH + 1));
        assert_eq!(
            strip(deep.as_bytes()),
            Err(JsoncError::TooDeep { limit: MAX_DEPTH })
        );
    }
}
