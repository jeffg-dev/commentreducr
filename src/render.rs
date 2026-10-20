//! The judge `state` string and the questions that apply, ported line by line from
//! `tools/bakeoff/render.py`. Output must stay byte-identical to the Python: thresholds tuned on
//! the Python state only transfer if the Rust state is the same bytes.

use serde::de::{Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde::ser::{Serialize, SerializeMap, SerializeSeq, Serializer};
use std::fmt;

pub const MAX_STATE_TOKENS: usize = 3000;
pub const SCHEMA_STRICT: &str = include_str!("schema/strict-v1.json");
pub const SCHEMA_VERIFY: &str = include_str!("schema/verify-v1.json");
pub const SCHEMA_STALE: &str = include_str!("schema/stale-v2.json");

const QUESTION_KEYS: [&str; 3] = ["type", "instructions", "criteria"];

/// Input row for the renderer: the fields render.py reads, as Options so missing keys print like
/// Python's None.
#[derive(Debug, Default)]
pub struct StateRow<'a> {
    pub language: Option<&'a str>,
    pub comment_kind: Option<&'a str>,
    pub placement: Option<&'a str>,
    pub comment: &'a str,
    pub annotated_code: &'a str,
    pub enclosing_scope: &'a str,
    pub symbol_name: Option<&'a str>,
    pub public: Option<bool>,
    pub is_test: Option<bool>,
    pub authored_by_agent: Option<bool>,
    pub agent: Option<&'a str>,
    pub comment_age_days: Option<i64>,
    pub code_newer_than_comment: Option<bool>,
    pub comment_new: Option<bool>,
    pub comment_modified: Option<bool>,
    pub scope_changed: Option<bool>,
    pub annotated_code_before: Option<&'a str>,
}

impl<'a> StateRow<'a> {
    /// Read the fields `render_state` needs from a dataset v2 row.
    pub fn from_v2_row(v: &'a serde_json::Value) -> StateRow<'a> {
        use serde_json::Value;
        let s = |k: &str| v.get(k).and_then(Value::as_str);
        let sym = |k: &str| v.get("symbol").and_then(|x| x.get(k));
        let ch = |k: &str| v.get("change").and_then(|x| x.get(k));
        StateRow {
            language: s("language"),
            comment_kind: s("comment_kind"),
            placement: s("placement"),
            comment: s("comment").unwrap_or(""),
            annotated_code: s("annotated_code").unwrap_or(""),
            enclosing_scope: s("enclosing_scope").unwrap_or(""),
            symbol_name: sym("name").and_then(Value::as_str),
            public: sym("public").and_then(Value::as_bool),
            is_test: sym("is_test").and_then(Value::as_bool),
            authored_by_agent: ch("authored_by_agent").and_then(Value::as_bool),
            agent: ch("agent").and_then(Value::as_str),
            comment_age_days: ch("comment_age_days").and_then(Value::as_i64),
            code_newer_than_comment: ch("code_newer_than_comment").and_then(Value::as_bool),
            comment_new: ch("comment_new").and_then(Value::as_bool),
            comment_modified: ch("comment_modified").and_then(Value::as_bool),
            scope_changed: ch("scope_changed").and_then(Value::as_bool),
            annotated_code_before: ch("annotated_code_before").and_then(Value::as_str),
        }
    }
}

pub(crate) fn py_str_bool(b: Option<bool>) -> &'static str {
    match b {
        Some(true) => "True",
        Some(false) => "False",
        None => "None",
    }
}

/// Python `len(s) // 4`: counts Unicode scalar values, not bytes.
pub(crate) fn estimate_tokens(s: &str) -> usize {
    s.chars().count() / 4
}

/// Python `str.splitlines()`: same boundaries, `\r\n` is one break, no trailing empty line.
pub(crate) fn py_splitlines(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut chars = s.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        let end = match c {
            '\r' if matches!(chars.peek(), Some((_, '\n'))) => {
                chars.next();
                i + 2
            }
            '\n' | '\r' | '\x0b' | '\x0c' | '\x1c' | '\x1d' | '\x1e' | '\u{85}' | '\u{2028}'
            | '\u{2029}' => i + c.len_utf8(),
            _ => continue,
        };
        out.push(&s[start..i]);
        start = end;
    }
    if start < s.len() {
        out.push(&s[start..]);
    }
    out
}

/// Python `str.strip()`: Rust's whitespace plus `\x1c`-`\x1f`, which Python also treats as space.
fn py_strip(s: &str) -> &str {
    s.trim_matches(|c: char| c.is_whitespace() || ('\x1c'..='\x1f').contains(&c))
}

/// `_scope_window`: keep `half` lines either side of the centre of `annotated` in `scope`.
fn scope_window(scope: &str, annotated: &str, half: usize) -> String {
    let lines = py_splitlines(scope);
    let ann = py_splitlines(annotated);
    let first = ann
        .iter()
        .map(|l| py_strip(l))
        .find(|l| !l.is_empty())
        .unwrap_or("");
    let start = lines
        .iter()
        .position(|ln| !first.is_empty() && ln.contains(first))
        .unwrap_or(lines.len() / 2);
    let centre = start + ann.len() / 2;
    let (lo, hi) = (
        centre.saturating_sub(half),
        lines.len().min(centre + half + 1),
    );
    let mut out: Vec<&str> = Vec::new();
    if lo > 0 {
        out.push("...");
    }
    // Python slicing clamps: lines[lo:hi] is empty when lo >= hi.
    if lo < hi {
        out.extend_from_slice(&lines[lo..hi]);
    }
    if hi < lines.len() {
        out.push("...");
    }
    out.join("\n")
}

pub fn render_state(row: &StateRow, max_tokens: usize) -> String {
    let opt = |v: Option<&str>| v.unwrap_or("None").to_string();
    let mut head = vec![
        format!(
            "language: {}  comment_kind: {}  placement: {}",
            opt(row.language),
            opt(row.comment_kind),
            opt(row.placement)
        ),
        format!("comment:\n{}", row.comment),
        format!("annotated_code:\n{}", row.annotated_code),
        format!(
            "symbol: {}  public: {}  is_test: {}",
            opt(row.symbol_name),
            py_str_bool(row.public),
            py_str_bool(row.is_test)
        ),
    ];
    let evidence: Vec<String> = [
        row.authored_by_agent
            .map(|b| format!("authored_by_agent: {}", py_str_bool(Some(b)))),
        row.agent.map(|a| format!("agent: {a}")),
        row.comment_age_days
            .map(|d| format!("comment_age_days: {d}")),
        row.code_newer_than_comment
            .map(|b| format!("code_newer_than_comment: {}", py_str_bool(Some(b)))),
        row.comment_new
            .map(|b| format!("comment_new: {}", py_str_bool(Some(b)))),
        row.comment_modified
            .map(|b| format!("comment_modified: {}", py_str_bool(Some(b)))),
        row.scope_changed
            .map(|b| format!("scope_changed: {}", py_str_bool(Some(b)))),
    ]
    .into_iter()
    .flatten()
    .collect();
    if !evidence.is_empty() {
        head.push(format!("evidence:\n{}", evidence.join("\n")));
    }
    if let Some(before) = row.annotated_code_before {
        head.push(format!("annotated_code_before:\n{before}"));
    }
    fit(&head, row.enclosing_scope, row.annotated_code, max_tokens)
}

/// `render_verify`: the state for the rewrite verification questions.
pub fn render_verify(
    original: &str,
    candidate: &str,
    annotated_code: &str,
    enclosing_scope: &str,
    max_tokens: usize,
) -> String {
    let head = [
        format!("original:\n{original}"),
        format!("candidate:\n{candidate}"),
        format!("annotated_code:\n{annotated_code}"),
    ];
    fit(&head, enclosing_scope, annotated_code, max_tokens)
}

/// `_fit`: `head` then `enclosing_scope`, the scope trimmed around the code to fit `max_tokens`.
fn fit(head: &[String], scope: &str, annotated: &str, max_tokens: usize) -> String {
    let build = |scope: &str| format!("{}\n\nenclosing_scope:\n{scope}", head.join("\n\n"));
    let text = build(scope);
    if estimate_tokens(&text) <= max_tokens {
        return text;
    }
    // Binary search for the widest centred scope window that fits; "..." if none does.
    let (mut lo, mut hi) = (0i64, py_splitlines(scope).len() as i64);
    let mut best = build("...");
    while lo <= hi {
        let half = (lo + hi) / 2;
        let candidate = build(&scope_window(scope, annotated, half as usize));
        if estimate_tokens(&candidate) <= max_tokens {
            best = candidate;
            lo = half + 1;
        } else {
            hi = half - 1;
        }
    }
    best
}

/// A JSON value that keeps object keys in file order. This crate's `serde_json` is built without
/// `preserve_order`, so `serde_json::Value` would sort question ids and `choice` criteria, and the
/// System One request would differ from the one the bake-off thresholds were tuned on.
#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Number(serde_json::Number),
    String(String),
    Array(Vec<Json>),
    Object(Vec<(String, Json)>),
}

impl Json {
    fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Object(kv) => kv.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }
}

impl<'de> Deserialize<'de> for Json {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Json;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("any JSON value")
            }
            fn visit_unit<E>(self) -> Result<Json, E> {
                Ok(Json::Null)
            }
            fn visit_bool<E>(self, b: bool) -> Result<Json, E> {
                Ok(Json::Bool(b))
            }
            fn visit_i64<E>(self, n: i64) -> Result<Json, E> {
                Ok(Json::Number(n.into()))
            }
            fn visit_u64<E>(self, n: u64) -> Result<Json, E> {
                Ok(Json::Number(n.into()))
            }
            fn visit_f64<E: serde::de::Error>(self, n: f64) -> Result<Json, E> {
                serde_json::Number::from_f64(n)
                    .map(Json::Number)
                    .ok_or_else(|| E::custom("non-finite number"))
            }
            fn visit_str<E>(self, s: &str) -> Result<Json, E> {
                Ok(Json::String(s.to_string()))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut a: A) -> Result<Json, A::Error> {
                let mut v = Vec::new();
                while let Some(x) = a.next_element()? {
                    v.push(x);
                }
                Ok(Json::Array(v))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut a: A) -> Result<Json, A::Error> {
                let mut v = Vec::new();
                while let Some(kv) = a.next_entry()? {
                    v.push(kv);
                }
                Ok(Json::Object(v))
            }
        }
        d.deserialize_any(V)
    }
}

impl Serialize for Json {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Json::Null => s.serialize_unit(),
            Json::Bool(b) => s.serialize_bool(*b),
            Json::Number(n) => n.serialize(s),
            Json::String(x) => s.serialize_str(x),
            Json::Array(v) => {
                let mut seq = s.serialize_seq(Some(v.len()))?;
                for x in v {
                    seq.serialize_element(x)?;
                }
                seq.end()
            }
            Json::Object(kv) => {
                let mut map = s.serialize_map(Some(kv.len()))?;
                for (k, v) in kv {
                    map.serialize_entry(k, v)?;
                }
                map.end()
            }
        }
    }
}

/// Question id -> question, in schema file order. Serialise with `Json::Object(questions)`.
pub type Questions = Vec<(String, Json)>;

/// `questions_for`: keep `type`, `instructions`, `criteria`; drop a question whose `applies_to`
/// kind is not `comment_kind`. A schema's top-level `instructions` prefix each question's, since
/// the request carries only the questions. Panics on a malformed schema; the shipped one is embedded.
pub fn questions_for(schema: &str, comment_kind: Option<&str>) -> Questions {
    let schema: Json = serde_json::from_str(schema).expect("question schema is valid JSON");
    let applies = schema.get("applies_to");
    let preamble = match schema.get("instructions") {
        Some(Json::String(t)) => Some(t.as_str()),
        _ => None,
    };
    let Some(Json::Object(questions)) = schema.get("questions") else {
        panic!("question schema has no `questions` object");
    };
    questions
        .iter()
        .filter(|(qid, _)| match applies.and_then(|a| a.get(qid)) {
            None => true,
            Some(Json::String(kind)) => Some(kind.as_str()) == comment_kind,
            Some(_) => false,
        })
        .map(|(qid, q)| {
            let Json::Object(kv) = q else {
                panic!("question {qid} is not an object");
            };
            let kept = kv
                .iter()
                .filter(|(k, _)| QUESTION_KEYS.contains(&k.as_str()))
                .map(|(k, v)| match (k.as_str(), v, preamble) {
                    ("instructions", Json::String(t), Some(pre)) => {
                        (k.clone(), Json::String(format!("{pre} {t}")))
                    }
                    _ => (k.clone(), v.clone()),
                });
            (qid.clone(), Json::Object(kept.collect()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    const DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/render");

    /// Expected files are written by tools/bakeoff/test_render.py::test_write_rust_fixtures.
    #[test]
    fn parity() {
        let cases: Vec<Value> =
            serde_json::from_str(&std::fs::read_to_string(format!("{DIR}/cases.json")).unwrap())
                .unwrap();
        assert!(cases.len() >= 8);
        for case in &cases {
            let name = case["name"].as_str().unwrap();
            let r = StateRow::from_v2_row(&case["row"]);
            let max = case["max_tokens"].as_u64().unwrap() as usize;
            let want = std::fs::read_to_string(format!("{DIR}/{name}.state.txt")).unwrap();
            assert_eq!(render_state(&r, max), want, "state for {name}");
            let want: Json = serde_json::from_str(
                &std::fs::read_to_string(format!("{DIR}/{name}.questions.json")).unwrap(),
            )
            .unwrap();
            let got = Json::Object(questions_for(SCHEMA_STRICT, r.comment_kind));
            assert_eq!(got, want, "questions for {name}");
        }
    }

    #[test]
    fn verify_parity() {
        let cases: Vec<Value> =
            serde_json::from_str(&std::fs::read_to_string(format!("{DIR}/verify.json")).unwrap())
                .unwrap();
        assert_eq!(cases.len(), 2);
        for c in &cases {
            let s = |k: &str| c[k].as_str().unwrap();
            let want = std::fs::read_to_string(format!("{DIR}/{}.verify.txt", s("name"))).unwrap();
            let got = render_verify(
                s("original"),
                s("candidate"),
                s("annotated_code"),
                s("enclosing_scope"),
                c["max_tokens"].as_u64().unwrap() as usize,
            );
            assert_eq!(got, want, "verify state for {}", s("name"));
        }
        let want: Json = serde_json::from_str(
            &std::fs::read_to_string(format!("{DIR}/stale.questions.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(Json::Object(questions_for(SCHEMA_STALE, None)), want);
        let ids: Vec<String> = questions_for(SCHEMA_VERIFY, None)
            .into_iter()
            .map(|q| q.0)
            .collect();
        assert_eq!(ids, ["preserves", "adds", "g1_candidate"]);
    }

    #[test]
    fn helpers() {
        assert_eq!(
            py_splitlines("a\r\nb\rc\u{2028}d\x0ce\n"),
            ["a", "b", "c", "d", "e"]
        );
        assert_eq!(py_splitlines(""), Vec::<&str>::new());
        assert_eq!(py_splitlines("\n\nx"), ["", "", "x"]);
        assert_eq!(estimate_tokens("日本語の"), 1);
        assert_eq!(py_strip("\x1f a \u{3000}"), "a");
        let qs = questions_for(SCHEMA_STRICT, Some("inline"));
        assert_eq!(qs.first().map(|q| q.0.as_str()), Some("kind"));
        assert_eq!(qs.last().map(|q| q.0.as_str()), Some("verdict"));
        // choice criteria keep file order, not alphabetical
        let crit = qs[0].1.get("criteria").unwrap();
        let Json::Object(kv) = crit else { panic!() };
        assert_eq!(kv[0].0, "rationale");
        assert_eq!(
            serde_json::to_string(&Json::Object(qs))
                .unwrap()
                .find("\"kind\""),
            Some(1)
        );
    }
}
