//! Catalog OCR hard-phrase guard (Phase 15.4 Plan 01 Task 2 — D-02,
//! research Pitfall 2).
//!
//! The AppImageHub catalog's own screenshot check (`check-screenshot.sh`)
//! OCRs the running window in the C locale and HARD-FAILS the test (not a
//! warning) if it finds any of a fixed list of phrases: "not installed",
//! "unable to start", "failed to ...", etc. Two of CleanMic's own existing
//! msgids collided with that list before this plan: the Khip-unavailable row
//! title ("Khip (not installed)") and the engine-fallback notice ("Unable to
//! start the selected engine — using instead:"). The Khip row is visible in
//! EVERY catalog run, because the catalog's own test host never has
//! libkhip.so — so this wasn't a rare edge case, it would trip on every
//! single re-test.
//!
//! This test is the LASTING guard, not a one-off fix verification: it reads
//! the French .po file (which carries every msgid, translated or not) at
//! compile time and asserts no msgid contains any hard-list phrase. A future
//! PR that reintroduces one of these phrases (or a new one) in a *new*
//! user-facing string will fail this test, not just a re-run of the real
//! catalog bot weeks or months later.

use std::collections::HashMap;

const PO_FILE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/locale/fr/LC_MESSAGES/cleanmic.po"
));

/// The catalog's own `check-screenshot.sh` hard-phrase list (research
/// Pitfall 2), with its regex alternations
/// (`could not (load|find|open|start|initiali)`, etc.) expanded into plain
/// phrases. Matched case-insensitively against every msgid.
const HARD_PHRASES: &[&str] = &[
    "traceback",
    "exception",
    "segmentation fault",
    "fatal",
    "error while loading",
    "glibc",
    "not installed",
    "cannot open display",
    "permission denied",
    "no such file",
    "could not load",
    "could not find",
    "could not open",
    "could not start",
    "could not initiali", // covers "initialise"/"initialize"
    "failed to load",
    "failed to start",
    "failed to open",
    "failed to initiali",
    "failed to create",
    "cannot load",
    "cannot find",
    "cannot open",
    "cannot execute",
    "cannot configure",
    "unable to load",
    "unable to find",
    "unable to open",
    "unable to start",
    "command not found",
    "core dumped",
];

/// One `msgid`/`msgstr` pair extracted from a .po file.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PoEntry {
    msgid: String,
    msgstr: String,
}

/// Strip a leading `msgid `/`msgstr ` keyword (if present) and the
/// surrounding quotes from one physical .po line, unescaping `\"`, `\\`,
/// `\n`, `\t`. Returns `None` for a line that isn't a quoted string at all.
fn extract_quoted(line: &str) -> Option<String> {
    let rest = line
        .strip_prefix("msgid ")
        .or_else(|| line.strip_prefix("msgstr "))
        .unwrap_or(line);
    let rest = rest.trim();
    let inner = rest.strip_prefix('"')?.strip_suffix('"')?;
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('"') => out.push('"'),
                Some('\\') => out.push('\\'),
                Some(other) => out.push(other),
                None => {}
            }
        } else {
            out.push(c);
        }
    }
    Some(out)
}

/// Parse a .po file's text into its `msgid`/`msgstr` entries, joining
/// multi-line (wrapped) msgids/msgstrs — a wrapped entry is a `msgid ""` (or
/// `msgstr ""`) header line followed by one or more bare `"..."`
/// continuation lines, per the standard gettext .po wrapping convention.
/// The file header entry (`msgid ""` at the very top, whose msgstr carries
/// the `Content-Type`/`Plural-Forms` metadata block) parses like any other
/// entry — harmless, since an empty msgid never contains a non-empty phrase.
fn parse_po(text: &str) -> Vec<PoEntry> {
    #[derive(PartialEq)]
    enum State {
        None,
        Msgid,
        Msgstr,
    }

    let mut entries = Vec::new();
    let mut state = State::None;
    let mut msgid = String::new();
    let mut msgstr = String::new();
    let mut have_msgid = false;

    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.starts_with("msgid ") {
            if have_msgid {
                entries.push(PoEntry {
                    msgid: std::mem::take(&mut msgid),
                    msgstr: std::mem::take(&mut msgstr),
                });
            }
            msgid = extract_quoted(line).unwrap_or_default();
            have_msgid = true;
            state = State::Msgid;
        } else if line.starts_with("msgstr ") {
            msgstr = extract_quoted(line).unwrap_or_default();
            state = State::Msgstr;
        } else if line.starts_with('"') && line.len() >= 2 && line.ends_with('"') {
            // Bare continuation line — folds into whichever of
            // msgid/msgstr is currently open.
            let piece = extract_quoted(line).unwrap_or_default();
            match state {
                State::Msgid => msgid.push_str(&piece),
                State::Msgstr => msgstr.push_str(&piece),
                State::None => {}
            }
        } else {
            // Blank line, comment, or an unrelated keyword ends this
            // entry's continuation. The entry itself is only flushed at
            // the next `msgid` (or at EOF below), so a msgstr's own
            // continuation lines are never lost between here and there.
            state = State::None;
        }
    }
    if have_msgid {
        entries.push(PoEntry { msgid, msgstr });
    }

    entries
}

#[test]
fn parse_po_joins_wrapped_msgid_and_msgstr() {
    let text = concat!(
        "msgid \"\"\n",
        "\"hello \"\n",
        "\"world\"\n",
        "msgstr \"\"\n",
        "\"bonjour \"\n",
        "\"monde\"\n",
    );
    let entries = parse_po(text);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].msgid, "hello world");
    assert_eq!(entries[0].msgstr, "bonjour monde");
}

#[test]
fn no_msgid_contains_a_catalog_hard_phrase() {
    let entries = parse_po(PO_FILE);
    let mut violations = Vec::new();
    for entry in &entries {
        let lower = entry.msgid.to_lowercase();
        for phrase in HARD_PHRASES {
            if lower.contains(phrase) {
                violations.push(format!(
                    "msgid {:?} contains catalog hard phrase {:?}",
                    entry.msgid, phrase
                ));
            }
        }
    }
    assert!(
        violations.is_empty(),
        "the following msgids would trip the catalog's screenshot OCR check \
         (research Pitfall 2) — reword them without losing meaning:\n{}",
        violations.join("\n")
    );
}

#[test]
fn d02_no_pipewire_strings_are_present_with_a_french_translation() {
    let entries = parse_po(PO_FILE);
    let by_msgid: HashMap<&str, &str> = entries
        .iter()
        .map(|e| (e.msgid.as_str(), e.msgstr.as_str()))
        .collect();

    for msgid in [
        "Audio is off: PipeWire isn't running",
        "CleanMic needs PipeWire, the standard Linux audio service, to clean your microphone. Start or install PipeWire, then open CleanMic again.",
    ] {
        let msgstr = by_msgid
            .get(msgid)
            .unwrap_or_else(|| panic!("missing D-02 msgid in cleanmic.po: {msgid:?}"));
        assert!(
            !msgstr.is_empty(),
            "D-02 msgid {msgid:?} has an empty French msgstr"
        );
    }
}
