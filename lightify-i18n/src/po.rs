//! A gettext `.po` reader: just what Lightify's catalogs use — `msgctxt`, `msgid`,
//! `msgid_plural`, `msgstr` / `msgstr[n]`, multi-line strings, the usual escapes, and the
//! header's `Plural-Forms` rule. Fuzzy entries are skipped, as gettext itself does.

use std::collections::HashMap;

/// One language's strings, keyed by (context, English text). Lightify writes no context
/// for ordinary strings, so most keys have an empty context.
#[derive(Default, Debug)]
pub struct Catalog {
    map: HashMap<(String, String), Vec<String>>,
    plural: Option<Expr>,
    nplurals: Option<usize>,
}

/// One entry of a `.po` / `.pot` file as written, untranslated and fuzzy ones included —
/// what the catalog checks (`check`) read. The header is the entry with an empty `id`.
#[derive(Default, Debug, Clone)]
pub struct Entry {
    pub ctxt: String,
    pub id: String,
    pub plural: Option<String>,
    pub strs: Vec<String>,
    pub fuzzy: bool,
    /// The line its `msgid` is on (1-based), for messages.
    pub line: usize,
}

/// Every entry in `text`, in file order.
pub fn entries(text: &str) -> Result<Vec<Entry>, String> {
    let mut out = Vec::new();
    let mut entry = Partial::default();
    let mut field: Option<Field> = None;
    let mut fuzzy = false;
    for (n, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() {
            entry.finish(&mut fuzzy, &mut out);
            field = None;
            continue;
        }
        if let Some(c) = line.strip_prefix("#,") {
            fuzzy |= c.split(',').any(|f| f.trim() == "fuzzy");
            continue;
        }
        if line.starts_with('#') {
            continue;
        }
        let (key, rest) = match line.find(char::is_whitespace) {
            Some(i) if !line.starts_with('"') => (&line[..i], line[i..].trim()),
            _ => ("", line),
        };
        let at = |what: &str| format!("line {}: {what}", n + 1);
        if !key.is_empty() {
            // A new keyword after a msgstr starts the next entry.
            if matches!(field, Some(Field::Str(_))) && (key == "msgid" || key == "msgctxt") {
                entry.finish(&mut fuzzy, &mut out);
            }
            field = Some(match key {
                "msgctxt" => Field::Ctxt,
                "msgid" => Field::Id,
                "msgid_plural" => Field::IdPlural,
                "msgstr" => Field::Str(0),
                k if k.starts_with("msgstr[") && k.ends_with(']') => {
                    Field::Str(k[7..k.len() - 1].parse().map_err(|_| at("bad msgstr index"))?)
                }
                other => return Err(at(&format!("unknown keyword {other}"))),
            });
            if key == "msgid" {
                entry.line = n + 1;
            }
        }
        let s = unquote(rest).ok_or_else(|| at("expected a quoted string"))?;
        match field {
            Some(Field::Ctxt) => entry.ctxt.get_or_insert_with(String::new).push_str(&s),
            Some(Field::Id) => entry.id.get_or_insert_with(String::new).push_str(&s),
            Some(Field::IdPlural) => entry.plural.get_or_insert_with(String::new).push_str(&s),
            Some(Field::Str(i)) => {
                if entry.strs.len() <= i {
                    entry.strs.resize(i + 1, String::new());
                }
                entry.strs[i].push_str(&s);
            }
            None => return Err(at("text before any keyword")),
        }
    }
    entry.finish(&mut fuzzy, &mut out);
    Ok(out)
}

/// The header's `Plural-Forms`, as (nplurals, plural rule) — each `None` when absent.
pub(crate) fn plural_forms(header: &str) -> Result<(Option<usize>, Option<Expr>), String> {
    let Some(rule) = header.lines().find_map(|l| l.trim().strip_prefix("Plural-Forms:")) else {
        return Ok((None, None));
    };
    let field = |name: &str| rule.split(';').find_map(|p| p.trim().strip_prefix(name)).map(str::trim);
    let n = match field("nplurals=") {
        Some(v) => Some(v.parse::<usize>().map_err(|_| format!("Plural-Forms: nplurals={v} is not a number"))?),
        None => None,
    };
    let expr = Expr::parse(field("plural=").unwrap_or("n != 1"))?;
    Ok((n, Some(expr)))
}

impl Catalog {
    pub fn parse(text: &str) -> Result<Catalog, String> {
        let mut cat = Catalog::default();
        for e in entries(text)? {
            if e.id.is_empty() {
                // The header: only the plural rule matters here.
                let header = e.strs.first().cloned().unwrap_or_default();
                let (n, expr) = plural_forms(&header)?;
                if expr.is_some() {
                    cat.plural = expr;
                    cat.nplurals = n;
                }
                continue;
            }
            // Untranslated (all empty) and fuzzy entries fall back to English.
            if e.fuzzy || e.strs.iter().all(String::is_empty) {
                continue;
            }
            cat.map.insert((e.ctxt, e.id), e.strs);
        }
        Ok(cat)
    }

    /// Which plural form the header's rule picks for `n` (English's `n != 1` without one).
    pub fn plural_form(&self, n: u64) -> usize {
        self.plural.as_ref().map(|p| p.eval(n)).unwrap_or(if n == 1 { 0 } else { 1 }) as usize
    }

    /// The header's `nplurals`, when it says.
    pub fn nplurals(&self) -> Option<usize> {
        self.nplurals
    }

    /// The translation of `id`, or `None` when the catalog has none.
    pub fn get(&self, ctxt: &str, id: &str) -> Option<&str> {
        self.map.get(&(ctxt.to_string(), id.to_string())).and_then(|v| v.first()).map(String::as_str)
    }

    /// The plural form for `n` of `id`, or `None` when the catalog has none.
    pub fn get_plural(&self, ctxt: &str, id: &str, n: u64) -> Option<&str> {
        let forms = self.map.get(&(ctxt.to_string(), id.to_string()))?;
        forms.get(self.plural_form(n)).or_else(|| forms.last()).map(String::as_str).filter(|s| !s.is_empty())
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Every translated (context, English) key — for the catalog checks.
    pub fn entries(&self) -> impl Iterator<Item = (&str, &str, &[String])> {
        self.map.iter().map(|((c, i), v)| (c.as_str(), i.as_str(), v.as_slice()))
    }
}

/// An entry while it is being read.
#[derive(Default)]
struct Partial {
    ctxt: Option<String>,
    id: Option<String>,
    plural: Option<String>,
    strs: Vec<String>,
    line: usize,
}

impl Partial {
    fn finish(&mut self, fuzzy: &mut bool, out: &mut Vec<Entry>) {
        let done = std::mem::take(self);
        let fuzzy = std::mem::replace(fuzzy, false);
        if let Some(id) = done.id {
            out.push(Entry {
                ctxt: done.ctxt.unwrap_or_default(),
                id,
                plural: done.plural,
                strs: done.strs,
                fuzzy,
                line: done.line,
            });
        }
    }
}

#[derive(Clone, Copy)]
enum Field {
    Ctxt,
    Id,
    IdPlural,
    Str(usize),
}

fn unquote(s: &str) -> Option<String> {
    let inner = s.strip_prefix('"')?.strip_suffix('"')?;
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next()? {
            'n' => out.push('\n'),
            't' => out.push('\t'),
            'r' => out.push('\r'),
            '"' => out.push('"'),
            '\\' => out.push('\\'),
            other => {
                out.push('\\');
                out.push(other);
            }
        }
    }
    Some(out)
}

/// A gettext plural rule (`n != 1`, `n%10==1 && n%100!=11 ? 0 : …`): the C subset
/// gettext allows — `n`, integers, `! % * / + -`, comparisons, `&& ||` and `?:`.
#[derive(Debug, Clone)]
pub(crate) enum Expr {
    N,
    Num(u64),
    Not(Box<Expr>),
    Bin(Box<Expr>, Op, Box<Expr>),
    If(Box<Expr>, Box<Expr>, Box<Expr>),
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum Op {
    Mul,
    Div,
    Rem,
    Add,
    Sub,
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
    Ne,
    And,
    Or,
}

impl Expr {
    pub(crate) fn parse(s: &str) -> Result<Expr, String> {
        let toks = tokenize(s)?;
        let mut p = Parser { toks: &toks, at: 0 };
        let e = p.ternary()?;
        if p.at != toks.len() {
            return Err(format!("plural rule: unexpected '{}' in {s}", toks[p.at]));
        }
        Ok(e)
    }

    pub(crate) fn eval(&self, n: u64) -> u64 {
        match self {
            Expr::N => n,
            Expr::Num(v) => *v,
            Expr::Not(e) => (e.eval(n) == 0) as u64,
            Expr::If(c, a, b) => {
                if c.eval(n) != 0 {
                    a.eval(n)
                } else {
                    b.eval(n)
                }
            }
            Expr::Bin(a, op, b) => {
                let (x, y) = (a.eval(n), b.eval(n));
                match op {
                    Op::Mul => x.wrapping_mul(y),
                    Op::Div => x.checked_div(y).unwrap_or(0),
                    Op::Rem => x.checked_rem(y).unwrap_or(0),
                    Op::Add => x.wrapping_add(y),
                    Op::Sub => x.wrapping_sub(y),
                    Op::Lt => (x < y) as u64,
                    Op::Le => (x <= y) as u64,
                    Op::Gt => (x > y) as u64,
                    Op::Ge => (x >= y) as u64,
                    Op::Eq => (x == y) as u64,
                    Op::Ne => (x != y) as u64,
                    Op::And => (x != 0 && y != 0) as u64,
                    Op::Or => (x != 0 || y != 0) as u64,
                }
            }
        }
    }
}

fn tokenize(s: &str) -> Result<Vec<String>, String> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i] as char;
        if c.is_whitespace() {
            i += 1;
        } else if c.is_ascii_digit() {
            let start = i;
            while i < b.len() && (b[i] as char).is_ascii_digit() {
                i += 1;
            }
            out.push(s[start..i].to_string());
        } else {
            let two = s.get(i..i + 2).unwrap_or("");
            if ["&&", "||", "==", "!=", "<=", ">="].contains(&two) {
                out.push(two.to_string());
                i += 2;
            } else if "n!%*/+-<>?:()".contains(c) {
                out.push(c.to_string());
                i += 1;
            } else {
                return Err(format!("plural rule: unexpected '{c}' in {s}"));
            }
        }
    }
    Ok(out)
}

struct Parser<'a> {
    toks: &'a [String],
    at: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<&str> {
        self.toks.get(self.at).map(String::as_str)
    }
    fn eat(&mut self, t: &str) -> bool {
        if self.peek() == Some(t) {
            self.at += 1;
            true
        } else {
            false
        }
    }
    fn ternary(&mut self) -> Result<Expr, String> {
        let c = self.binary(0)?;
        if self.eat("?") {
            let a = self.ternary()?;
            if !self.eat(":") {
                return Err("plural rule: expected ':'".into());
            }
            let b = self.ternary()?;
            return Ok(Expr::If(Box::new(c), Box::new(a), Box::new(b)));
        }
        Ok(c)
    }
    // Precedence, loosest first: || && (== !=) (< <= > >=) (+ -) (* / %)
    fn binary(&mut self, level: usize) -> Result<Expr, String> {
        const LEVELS: &[&[(&str, Op)]] = &[
            &[("||", Op::Or)],
            &[("&&", Op::And)],
            &[("==", Op::Eq), ("!=", Op::Ne)],
            &[("<", Op::Lt), ("<=", Op::Le), (">", Op::Gt), (">=", Op::Ge)],
            &[("+", Op::Add), ("-", Op::Sub)],
            &[("*", Op::Mul), ("/", Op::Div), ("%", Op::Rem)],
        ];
        if level == LEVELS.len() {
            return self.unary();
        }
        let mut left = self.binary(level + 1)?;
        'more: loop {
            for (tok, op) in LEVELS[level] {
                if self.eat(tok) {
                    let right = self.binary(level + 1)?;
                    left = Expr::Bin(Box::new(left), *op, Box::new(right));
                    continue 'more;
                }
            }
            return Ok(left);
        }
    }
    fn unary(&mut self) -> Result<Expr, String> {
        if self.eat("!") {
            return Ok(Expr::Not(Box::new(self.unary()?)));
        }
        if self.eat("(") {
            let e = self.ternary()?;
            if !self.eat(")") {
                return Err("plural rule: expected ')'".into());
            }
            return Ok(e);
        }
        match self.peek() {
            Some("n") => {
                self.at += 1;
                Ok(Expr::N)
            }
            Some(t) if t.chars().all(|c| c.is_ascii_digit()) => {
                let v = t.parse().map_err(|_| "plural rule: number too big".to_string())?;
                self.at += 1;
                Ok(Expr::Num(v))
            }
            other => Err(format!("plural rule: unexpected {other:?}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DE: &str = r#"
# German
msgid ""
msgstr ""
"Content-Type: text/plain; charset=UTF-8\n"
"Plural-Forms: nplurals=2; plural=(n != 1);\n"

msgid "Settings"
msgstr "Einstellungen"

#, fuzzy
msgid "Restart"
msgstr "Neu starten"

msgid "Not yet"
msgstr ""

msgctxt "verb"
msgid "Play"
msgstr "Abspielen"

msgid "Added {} to queue"
msgstr "{} zur Warteschlange "
"hinzugefügt"

msgid "{n} track"
msgid_plural "{n} tracks"
msgstr[0] "{n} Titel"
msgstr[1] "{n} Titel (mehrere)"

msgid "Say \"hi\"\n"
msgstr "Sag \"hallo\"\n"
"#;

    #[test]
    fn reads_entries_context_continuations_and_escapes() {
        let c = Catalog::parse(DE).unwrap();
        assert_eq!(c.get("", "Settings"), Some("Einstellungen"));
        assert_eq!(c.get("verb", "Play"), Some("Abspielen"));
        assert_eq!(c.get("", "Play"), None, "context is part of the key");
        assert_eq!(c.get("", "Added {} to queue"), Some("{} zur Warteschlange hinzugefügt"));
        assert_eq!(c.get("", "Say \"hi\"\n"), Some("Sag \"hallo\"\n"));
    }

    #[test]
    fn fuzzy_and_untranslated_fall_back() {
        let c = Catalog::parse(DE).unwrap();
        assert_eq!(c.get("", "Restart"), None);
        assert_eq!(c.get("", "Not yet"), None);
    }

    #[test]
    fn plurals_follow_the_header_rule() {
        let c = Catalog::parse(DE).unwrap();
        assert_eq!(c.get_plural("", "{n} track", 1), Some("{n} Titel"));
        assert_eq!(c.get_plural("", "{n} track", 0), Some("{n} Titel (mehrere)"));
        assert_eq!(c.get_plural("", "{n} track", 5), Some("{n} Titel (mehrere)"));
    }

    #[test]
    fn polish_and_french_rules() {
        let pl = Expr::parse("(n==1 ? 0 : n%10>=2 && n%10<=4 && (n%100<10 || n%100>=20) ? 1 : 2)").unwrap();
        let got: Vec<u64> = [1, 2, 4, 5, 12, 22, 25, 101, 112].iter().map(|&n| pl.eval(n)).collect();
        assert_eq!(got, [0, 1, 1, 2, 2, 1, 2, 2, 2]);
        let fr = Expr::parse("n > 1").unwrap();
        assert_eq!([0, 1, 2].map(|n| fr.eval(n)), [0, 0, 1]);
    }

    #[test]
    fn bad_input_is_an_error_not_a_panic() {
        assert!(Catalog::parse("msgid \"x\"\nmsgstr unquoted\n").is_err());
        assert!(Catalog::parse("bogus \"x\"\n").is_err());
        assert!(Expr::parse("n ! = 1 )").is_err());
    }
}
