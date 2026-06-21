//! 录制模式:抓某会话当前 pane,提取候选特征行,给出可加进 profile 的正则建议。
//!
//! 只打印建议,不自动改 config——把判断权留给人。

/// 一条特征建议。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suggestion {
    /// 原始行(去首尾空白)。
    pub line: String,
    /// 建议的正则(已转义)。
    pub regex: String,
}

/// 从 pane 文本提取候选特征行(末尾若干非空行,去重,跳过纯分隔线)。
pub fn extract_candidates(pane: &str, max: usize) -> Vec<Suggestion> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for line in pane.lines().rev() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        // 跳过纯分隔线(全是 ─ — = - 之类)。
        if t.chars().all(|c| is_rule_char(c)) {
            continue;
        }
        if !seen.insert(t.to_string()) {
            continue;
        }
        out.push(Suggestion {
            line: t.to_string(),
            regex: suggest_regex(t),
        });
        if out.len() >= max {
            break;
        }
    }
    out
}

fn is_rule_char(c: char) -> bool {
    matches!(c, '─' | '—' | '=' | '-' | '_' | '·' | ' ' | '∙' | '•')
}

/// 给一行文本生成一个"够稳定"的正则建议:
/// 取该行里最有辨识度的一段(优先含字母的连续 token 串),做正则转义。
pub fn suggest_regex(line: &str) -> String {
    // 优先挑一段含字母、长度适中的子串(避免把动态数字/路径写死)。
    let snippet = pick_stable_snippet(line);
    regex_escape(&snippet)
}

/// 从行里挑一段稳定子串:取前若干个"非纯数字"单词拼起来,限制长度。
fn pick_stable_snippet(line: &str) -> String {
    let words: Vec<&str> = line.split_whitespace().collect();
    let mut picked = Vec::new();
    for w in words {
        // 跳过看着像动态值的 token(纯数字、含数字的计时/路径)。
        let dynamic = w.chars().any(|c| c.is_ascii_digit()) || w.starts_with('~') || w.contains('/');
        if dynamic {
            // 已经攒到了就停;否则继续找。
            if !picked.is_empty() {
                break;
            }
            continue;
        }
        picked.push(w);
        if picked.join(" ").len() >= 24 {
            break;
        }
    }
    if picked.is_empty() {
        // 退而求其次:用整行(截断)。
        let s: String = line.chars().take(32).collect();
        s
    } else {
        picked.join(" ")
    }
}

/// 正则元字符转义。
fn regex_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if "\\.+*?()|[]{}^$".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skips_blank_and_rule_lines() {
        let pane = "real line\n\n────────────\n  another  ";
        let cands = extract_candidates(pane, 10);
        let lines: Vec<&str> = cands.iter().map(|c| c.line.as_str()).collect();
        assert!(lines.contains(&"another"));
        assert!(lines.contains(&"real line"));
        assert!(!lines.iter().any(|l| l.contains('─')));
    }

    #[test]
    fn dedups_repeated_lines() {
        let pane = "same\nsame\nsame";
        let cands = extract_candidates(pane, 10);
        assert_eq!(cands.len(), 1);
    }

    #[test]
    fn respects_max() {
        let pane = "a\nb\nc\nd\ne";
        let cands = extract_candidates(pane, 2);
        assert_eq!(cands.len(), 2);
    }

    #[test]
    fn snippet_avoids_dynamic_tokens() {
        // 含计时的 Working 行应挑出 "Working" 而不是 "(3s"。
        let s = suggest_regex("• Working (3s • esc to interrupt)");
        assert!(s.contains("Working"), "got: {}", s);
        assert!(!s.contains("3s"), "got: {}", s);
    }

    #[test]
    fn escapes_regex_metachars() {
        assert_eq!(regex_escape("a.b(c)"), "a\\.b\\(c\\)");
    }
}
