//! The catalog checks (LANGUAGE-PLAN.md, "Checks that keep it from breaking"): what a
//! translated `.po` must get right for the app to show it safely. Each crate runs them
//! over its bundled catalogs in a unit test, so a translator finds out with
//! `cargo test`, not from a garbled screen.
//!
//! A problem is something that would show wrong text or lose a value: a catalog that
//! doesn't parse, a placeholder dropped or added, the wrong number of plural forms. A
//! string the catalog doesn't have is not a problem — it shows in English — so those
//! are only counted (`Report::fallback`), as are entries the template no longer has
//! (`Report::obsolete`: the English changed, so the old translation is no longer used).

use crate::po::{self, Entry, Expr};

/// What [`catalog`] found for one language.
#[derive(Debug, Default)]
pub struct Report {
    /// Things to fix, each a sentence naming the entry and its line in the `.po`.
    pub problems: Vec<String>,
    /// Template strings with a translation.
    pub translated: usize,
    /// Template strings without one (empty, fuzzy or missing): shown in English.
    pub fallback: Vec<String>,
    /// Translated entries the template doesn't have any more (never shown).
    pub obsolete: Vec<String>,
    /// Entries whose English became a count (msgid_plural) since they were translated,
    /// or stopped being one: still shown (the first form), but due a re-translation.
    pub stale: Vec<String>,
}

/// The placeholders in `s` as Slint and `fill` read them: how many `{}`, and the
/// numbered / named ones (`{0}`, `{n}`), sorted. `{{` and `}}` are literal braces.
fn placeholders(s: &str) -> (usize, Vec<String>) {
    let (mut next, mut named) = (0, Vec::new());
    let mut rest = s;
    while let Some(i) = rest.find(['{', '}']) {
        let tail = &rest[i..];
        if tail.starts_with("{{") || tail.starts_with("}}") {
            rest = &tail[2..];
            continue;
        }
        match (tail.starts_with('{'), tail.find('}')) {
            (true, Some(end)) => {
                let key = &tail[1..end];
                if key.is_empty() {
                    next += 1;
                } else {
                    named.push(key.to_string());
                }
                rest = &tail[end + 1..];
            }
            _ => rest = &tail[1..],
        }
    }
    named.sort();
    (next, named)
}

fn describe((next, named): &(usize, Vec<String>)) -> String {
    let mut parts = Vec::new();
    if *next > 0 {
        parts.push(format!("{next} × {{}}"));
    }
    parts.extend(named.iter().map(|k| format!("{{{k}}}")));
    if parts.is_empty() {
        "no placeholders".to_string()
    } else {
        parts.join(", ")
    }
}

/// Which plural forms the rule only ever picks for n = 1 (a form like German's
/// singular): those may write "one track" without the number.
fn only_for_one(rule: &Expr, forms: usize) -> Vec<bool> {
    let mut picked_other = vec![false; forms];
    for n in (0..=1000u64).filter(|&n| n != 1) {
        if let Some(f) = picked_other.get_mut(rule.eval(n) as usize) {
            *f = true;
        }
    }
    picked_other.iter().map(|o| !o).collect()
}

/// Checks the catalog `po` (language `code`) against the template `pot` it was made from.
pub fn catalog(code: &str, po: &str, pot: &str) -> Report {
    let mut r = Report::default();
    let template = match po::entries(pot) {
        Ok(t) => t,
        Err(e) => {
            r.problems.push(format!("the template itself doesn't parse ({e})"));
            return r;
        }
    };
    let entries = match po::entries(po) {
        Ok(e) => e,
        Err(e) => {
            r.problems.push(format!("{code}: the file doesn't parse — {e}. Nothing in it would be shown."));
            return r;
        }
    };
    // The header: Plural-Forms decides which form each count gets.
    let header = entries.iter().find(|e| e.id.is_empty()).and_then(|e| e.strs.first()).cloned().unwrap_or_default();
    let (nplurals, rule) = match po::plural_forms(&header) {
        Ok(v) => v,
        Err(e) => {
            r.problems.push(format!(
                "{code}: the header's Plural-Forms line can't be read ({e}). Copy the line for this language from \
                 https://www.gnu.org/software/gettext/manual/html_node/Plural-forms.html"
            ));
            (None, None)
        }
    };
    let has_plurals = template.iter().any(|e| e.plural.is_some());
    if has_plurals && (nplurals.is_none() || rule.is_none()) && r.problems.is_empty() {
        r.problems.push(format!(
            "{code}: the header has no \"Plural-Forms: nplurals=…; plural=…;\" line, so counts can't pick a form"
        ));
    }
    if let Some(lang) = header.lines().find_map(|l| l.trim().strip_prefix("Language:")).map(str::trim) {
        if !lang.is_empty() && !lang.replace('_', "-").eq_ignore_ascii_case(code) {
            r.problems.push(format!("{code}: the header says \"Language: {lang}\", but the folder is {code}"));
        }
    }
    let one = match (&rule, nplurals) {
        (Some(rule), Some(n)) => only_for_one(rule, n),
        _ => Vec::new(),
    };

    let find = |e: &Entry| entries.iter().find(|x| !x.id.is_empty() && x.ctxt == e.ctxt && x.id == e.id);
    for t in template.iter().filter(|t| !t.id.is_empty()) {
        let Some(e) = find(t) else {
            r.fallback.push(t.id.clone());
            continue;
        };
        if e.fuzzy || e.strs.iter().all(String::is_empty) {
            r.fallback.push(t.id.clone());
            continue;
        }
        let at = format!("{code}.po line {}: \"{}\"", e.line, t.id);
        if t.plural.is_some() != e.plural.is_some() {
            r.stale.push(t.id.clone());
            continue;
        }
        r.translated += 1;
        match &t.plural {
            None => {
                let (want, got) = (placeholders(&t.id), placeholders(&e.strs[0]));
                if want != got {
                    r.problems.push(format!(
                        "{at}: the translation has {}, the English has {} — every placeholder must stay, as written",
                        describe(&got),
                        describe(&want)
                    ));
                }
            }
            Some(english_plural) => {
                if let Some(n) = nplurals {
                    if e.strs.len() != n {
                        r.problems.push(format!(
                            "{at}: {} plural forms (msgstr[0]…), but the header says nplurals={n}",
                            e.strs.len()
                        ));
                    }
                }
                let want = placeholders(english_plural);
                for (i, form) in e.strs.iter().enumerate() {
                    if form.is_empty() {
                        r.problems.push(format!("{at}: msgstr[{i}] is empty while other forms are filled in"));
                        continue;
                    }
                    let got = placeholders(form);
                    // A form used only for exactly 1 may leave the number out ("one track").
                    let mut relaxed = want.clone();
                    if one.get(i).copied().unwrap_or(false) && !got.1.iter().any(|k| k == "n") {
                        relaxed.1.retain(|k| k != "n");
                    }
                    if got != relaxed {
                        r.problems.push(format!(
                            "{at}: msgstr[{i}] has {}, the English has {} — every placeholder must stay, as written",
                            describe(&got),
                            describe(&want)
                        ));
                    }
                }
            }
        }
    }
    for e in entries.iter().filter(|e| !e.id.is_empty() && !e.fuzzy && e.strs.iter().any(|s| !s.is_empty())) {
        if !template.iter().any(|t| t.ctxt == e.ctxt && t.id == e.id) {
            r.obsolete.push(e.id.clone());
        }
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    const POT: &str = "msgid \"\"\nmsgstr \"\"\n\"Plural-Forms: nplurals=INTEGER; plural=EXPRESSION;\\n\"\n\n\
        msgid \"Settings\"\nmsgstr \"\"\n\n\
        msgid \"Added {} to {}\"\nmsgstr \"\"\n\n\
        msgid \"{n} track\"\nmsgid_plural \"{n} tracks\"\nmsgstr[0] \"\"\nmsgstr[1] \"\"\n\n\
        msgid \"Untranslated\"\nmsgstr \"\"\n";

    fn po(header: &str, body: &str) -> String {
        format!("msgid \"\"\nmsgstr \"\"\n\"{header}\\n\"\n\n{body}")
    }

    #[test]
    fn a_good_catalog_passes() {
        let p = po(
            "Plural-Forms: nplurals=2; plural=(n != 1);",
            "msgid \"Settings\"\nmsgstr \"Einstellungen\"\n\n\
             msgid \"Added {} to {}\"\nmsgstr \"{} zu {} hinzugefügt\"\n\n\
             msgid \"{n} track\"\nmsgid_plural \"{n} tracks\"\nmsgstr[0] \"ein Titel\"\nmsgstr[1] \"{n} Titel\"\n\n\
             msgid \"Gone\"\nmsgstr \"Weg\"\n",
        );
        let r = catalog("de", &p, POT);
        assert!(r.problems.is_empty(), "{:?}", r.problems);
        assert_eq!(r.translated, 3);
        assert_eq!(r.fallback, ["Untranslated"]);
        assert_eq!(r.obsolete, ["Gone"]);
    }

    #[test]
    fn placeholders_and_forms_are_checked() {
        // Polish: three forms; form 0 is only ever n = 1, forms 1 and 2 cover many numbers.
        let p = po(
            "Plural-Forms: nplurals=3; plural=(n==1 ? 0 : n%10>=2 && n%10<=4 && (n%100<10 || n%100>=20) ? 1 : 2);",
            "msgid \"Added {} to {}\"\nmsgstr \"Dodano {}\"\n\n\
             msgid \"{n} track\"\nmsgid_plural \"{n} tracks\"\nmsgstr[0] \"utwór\"\nmsgstr[1] \"utwory\"\nmsgstr[2] \"{n} utworów\"\n",
        );
        let r = catalog("pl", &p, POT);
        assert_eq!(r.problems.len(), 2, "{:?}", r.problems);
        assert!(r.problems[0].contains("Added {} to {}") && r.problems[0].contains("1 × {}"), "{}", r.problems[0]);
        assert!(r.problems[1].contains("msgstr[1]"), "form 1 is used for 2, 3, 4… and needs {{n}}: {}", r.problems[1]);

        let p = po("Plural-Forms: nplurals=2; plural=(n != 1);", "msgid \"{n} track\"\nmsgid_plural \"{n} tracks\"\nmsgstr[0] \"{n} x\"\n");
        let r = catalog("de", &p, POT);
        assert!(r.problems.iter().any(|p| p.contains("nplurals=2")), "{:?}", r.problems);
    }

    #[test]
    fn broken_files_and_headers_are_reported() {
        let r = catalog("fr", "msgid \"x\"\nmsgstr unquoted\n", POT);
        assert!(r.problems[0].contains("doesn't parse"), "{:?}", r.problems);
        let r = catalog("fr", &po("Language: de", "msgid \"Settings\"\nmsgstr \"Paramètres\"\n"), POT);
        assert!(r.problems.iter().any(|p| p.contains("Plural-Forms")), "{:?}", r.problems);
        assert!(r.problems.iter().any(|p| p.contains("Language: de")), "{:?}", r.problems);
    }

    #[test]
    fn a_string_that_became_a_count_is_stale_not_broken() {
        let p = po("Plural-Forms: nplurals=2; plural=(n != 1);", "msgid \"{n} track\"\nmsgstr \"{n} Titel\"\n");
        let r = catalog("de", &p, POT);
        assert!(r.problems.is_empty(), "{:?}", r.problems);
        assert_eq!(r.stale, ["{n} track"]);
    }

    #[test]
    fn braces_and_named_placeholders() {
        assert_eq!(placeholders("{{literal}} {} {0} {n} {}"), (2, vec!["0".to_string(), "n".to_string()]));
        assert_eq!(placeholders("odd } and { brace"), (0, vec![]));
    }
}
