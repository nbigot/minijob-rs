//! Regular expressions with the syntax and matching rules of Go's `regexp` package (RE2
//! syntax with Perl flags), for the patterns that the Go server compiles with
//! `regexp.Compile`: JSON schema `pattern`, `patternProperties` and `format: regex`.
//!
//! The `regex` crate has nearly the same syntax. A pattern is translated before compilation
//! so that:
//!
//! - `\d`, `\s`, `\w`, `\b` and their negations are ASCII-only, as in Go (`regex` makes them
//!   Unicode-aware, so `^\d+$` matched `"١٢٣"`);
//! - what Go accepts compiles: a `{` that does not start a repetition is a literal (`a{`,
//!   `{x}`), `\Q...\E`, octal escapes (`\012`), `[` inside a class, `\p{^Greek}`, `(?)`, named
//!   groups whose name starts with a digit;
//! - what Go refuses is refused: stacked repetitions (`a**`, `a{2}{3}`), counts above 1000,
//!   escapes Go does not know (`\u0041`, `\e`), flags other than `imsU`, unknown class names;
//! - `&&`, `--`, `~~` and `[` inside a class are literals (`regex` reads set operations and
//!   nested classes);
//! - Unicode classes use Go's names (Go 1.25 lookup rules) and `\p{Greek}` is the script, not
//!   the script extensions.

use regex::{Regex, RegexBuilder};

/// Go `regexp.Compile`. The error is Go's message (`error parsing regexp: ...`).
pub fn compile(pattern: &str) -> Result<Regex, String> {
    let translated = translate(pattern)?;
    RegexBuilder::new(&translated)
        // Go allows 1000 levels of nesting.
        .nest_limit(1000)
        .build()
        .map_err(|e| {
            let detail = e.to_string();
            let last = detail.lines().last().unwrap_or("").trim();
            let last = last.strip_prefix("error: ").unwrap_or(last);
            format!("error parsing regexp: {last}: `{pattern}`")
        })
}

fn err(code: &str, expr: &str) -> String {
    format!("error parsing regexp: {code}: `{expr}`")
}

/// Unicode categories of Go 1.25 (`unicode.Categories`).
const CATEGORIES: [&str; 38] = [
    "C", "Cc", "Cf", "Cn", "Co", "Cs", "L", "LC", "Ll", "Lm", "Lo", "Lt", "Lu", "M", "Mc", "Me",
    "Mn", "N", "Nd", "Nl", "No", "P", "Pc", "Pd", "Pe", "Pf", "Pi", "Po", "Ps", "S", "Sc", "Sk",
    "Sm", "So", "Z", "Zl", "Zp", "Zs",
];

/// Scripts of Go 1.25 (`unicode.Scripts`, Unicode 15.0).
const SCRIPTS: [&str; 163] = [
    "Adlam",
    "Ahom",
    "Anatolian_Hieroglyphs",
    "Arabic",
    "Armenian",
    "Avestan",
    "Balinese",
    "Bamum",
    "Bassa_Vah",
    "Batak",
    "Bengali",
    "Bhaiksuki",
    "Bopomofo",
    "Brahmi",
    "Braille",
    "Buginese",
    "Buhid",
    "Canadian_Aboriginal",
    "Carian",
    "Caucasian_Albanian",
    "Chakma",
    "Cham",
    "Cherokee",
    "Chorasmian",
    "Common",
    "Coptic",
    "Cuneiform",
    "Cypriot",
    "Cypro_Minoan",
    "Cyrillic",
    "Deseret",
    "Devanagari",
    "Dives_Akuru",
    "Dogra",
    "Duployan",
    "Egyptian_Hieroglyphs",
    "Elbasan",
    "Elymaic",
    "Ethiopic",
    "Georgian",
    "Glagolitic",
    "Gothic",
    "Grantha",
    "Greek",
    "Gujarati",
    "Gunjala_Gondi",
    "Gurmukhi",
    "Han",
    "Hangul",
    "Hanifi_Rohingya",
    "Hanunoo",
    "Hatran",
    "Hebrew",
    "Hiragana",
    "Imperial_Aramaic",
    "Inherited",
    "Inscriptional_Pahlavi",
    "Inscriptional_Parthian",
    "Javanese",
    "Kaithi",
    "Kannada",
    "Katakana",
    "Kawi",
    "Kayah_Li",
    "Kharoshthi",
    "Khitan_Small_Script",
    "Khmer",
    "Khojki",
    "Khudawadi",
    "Lao",
    "Latin",
    "Lepcha",
    "Limbu",
    "Linear_A",
    "Linear_B",
    "Lisu",
    "Lycian",
    "Lydian",
    "Mahajani",
    "Makasar",
    "Malayalam",
    "Mandaic",
    "Manichaean",
    "Marchen",
    "Masaram_Gondi",
    "Medefaidrin",
    "Meetei_Mayek",
    "Mende_Kikakui",
    "Meroitic_Cursive",
    "Meroitic_Hieroglyphs",
    "Miao",
    "Modi",
    "Mongolian",
    "Mro",
    "Multani",
    "Myanmar",
    "Nabataean",
    "Nag_Mundari",
    "Nandinagari",
    "New_Tai_Lue",
    "Newa",
    "Nko",
    "Nushu",
    "Nyiakeng_Puachue_Hmong",
    "Ogham",
    "Ol_Chiki",
    "Old_Hungarian",
    "Old_Italic",
    "Old_North_Arabian",
    "Old_Permic",
    "Old_Persian",
    "Old_Sogdian",
    "Old_South_Arabian",
    "Old_Turkic",
    "Old_Uyghur",
    "Oriya",
    "Osage",
    "Osmanya",
    "Pahawh_Hmong",
    "Palmyrene",
    "Pau_Cin_Hau",
    "Phags_Pa",
    "Phoenician",
    "Psalter_Pahlavi",
    "Rejang",
    "Runic",
    "Samaritan",
    "Saurashtra",
    "Sharada",
    "Shavian",
    "Siddham",
    "SignWriting",
    "Sinhala",
    "Sogdian",
    "Sora_Sompeng",
    "Soyombo",
    "Sundanese",
    "Syloti_Nagri",
    "Syriac",
    "Tagalog",
    "Tagbanwa",
    "Tai_Le",
    "Tai_Tham",
    "Tai_Viet",
    "Takri",
    "Tamil",
    "Tangsa",
    "Tangut",
    "Telugu",
    "Thaana",
    "Thai",
    "Tibetan",
    "Tifinagh",
    "Tirhuta",
    "Toto",
    "Ugaritic",
    "Vai",
    "Vithkuqi",
    "Wancho",
    "Warang_Citi",
    "Yezidi",
    "Yi",
    "Zanabazar_Square",
];

/// `unicode.CategoryAliases` of Go 1.25.
const CATEGORY_ALIASES: [(&str, &str); 42] = [
    ("Cased_Letter", "LC"),
    ("Close_Punctuation", "Pe"),
    ("Combining_Mark", "M"),
    ("Connector_Punctuation", "Pc"),
    ("Control", "Cc"),
    ("Currency_Symbol", "Sc"),
    ("Dash_Punctuation", "Pd"),
    ("Decimal_Number", "Nd"),
    ("Enclosing_Mark", "Me"),
    ("Final_Punctuation", "Pf"),
    ("Format", "Cf"),
    ("Initial_Punctuation", "Pi"),
    ("Letter", "L"),
    ("Letter_Number", "Nl"),
    ("Line_Separator", "Zl"),
    ("Lowercase_Letter", "Ll"),
    ("Mark", "M"),
    ("Math_Symbol", "Sm"),
    ("Modifier_Letter", "Lm"),
    ("Modifier_Symbol", "Sk"),
    ("Nonspacing_Mark", "Mn"),
    ("Number", "N"),
    ("Open_Punctuation", "Ps"),
    ("Other", "C"),
    ("Other_Letter", "Lo"),
    ("Other_Number", "No"),
    ("Other_Punctuation", "Po"),
    ("Other_Symbol", "So"),
    ("Paragraph_Separator", "Zp"),
    ("Private_Use", "Co"),
    ("Punctuation", "P"),
    ("Separator", "Z"),
    ("Space_Separator", "Zs"),
    ("Spacing_Mark", "Mc"),
    ("Surrogate", "Cs"),
    ("Symbol", "S"),
    ("Titlecase_Letter", "Lt"),
    ("Unassigned", "Cn"),
    ("Uppercase_Letter", "Lu"),
    ("cntrl", "Cc"),
    ("digit", "Nd"),
    ("punct", "P"),
];

/// POSIX classes (`[:alpha:]`), same names and ASCII sets in both syntaxes.
const POSIX: [&str; 14] = [
    "alnum", "alpha", "ascii", "blank", "cntrl", "digit", "graph", "lower", "print", "punct",
    "space", "upper", "word", "xdigit",
];

/// Go 1.25 `canonicalName`: first letter uppercase, the rest lowercase, without `_`, `-` and
/// spaces.
fn canonical_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut first = true;
    for c in name.chars() {
        match c {
            '_' | '-' | ' ' => {}
            c if first => {
                out.push(c.to_ascii_uppercase());
                first = false;
            }
            c => out.push(c.to_ascii_lowercase()),
        }
    }
    out
}

/// The `regex` class for a Go Unicode class name (`unicodeTable`), with its sign applied.
/// Go 1.25 matches names loosely, but its lookup misses the scripts whose name has an
/// underscore (`Old_Italic`); those are accepted under their exact name, as before Go 1.25.
fn unicode_class(name: &str, positive: bool) -> Option<String> {
    let (p, n) = if positive { ('p', 'P') } else { ('P', 'p') };
    let canon = canonical_name(name);
    let class =
        |spec: &str, positive: bool| format!("\\{}{{{spec}}}", if positive { p } else { n });
    let category = |c: &str| match c {
        // Go's C has no unassigned code points (Unicode's C includes Cn).
        "C" if positive => r"[\p{gc=Cc}\p{gc=Cf}\p{gc=Co}]".to_string(),
        "C" => r"[^\p{gc=Cc}\p{gc=Cf}\p{gc=Co}]".to_string(),
        // Surrogates never occur in a decoded string (`regex` has no Cs class).
        "Cs" if positive => r"[^\x{0}-\x{10FFFF}]".to_string(),
        "Cs" => r"\p{Any}".to_string(),
        c => class(&format!("gc={c}"), true),
    };
    match canon.as_str() {
        "Any" => return Some(class("Any", true)),
        "Assigned" => return Some(class("gc=Cn", false)),
        "Ascii" => return Some(class("ASCII", true)),
        "Lc" => return Some(category("LC")),
        _ => {}
    }
    if let Some(c) = CATEGORIES.iter().find(|c| **c == canon) {
        return Some(category(c));
    }
    if let Some(s) = SCRIPTS.iter().find(|s| **s == canon || **s == name) {
        return Some(class(&format!("sc={s}"), true));
    }
    CATEGORY_ALIASES
        .iter()
        .find(|(alias, _)| canonical_name(alias) == canon)
        .map(|(_, c)| category(c))
}

/// Perl classes (`\d \s \w` and negations): ASCII sets, as items of a `regex` class.
fn perl_class(c: char) -> Option<(&'static str, bool)> {
    match c {
        'd' => Some(("0-9", true)),
        'D' => Some(("0-9", false)),
        's' => Some((r"\t\n\x0C\r\x20", true)),
        'S' => Some((r"\t\n\x0C\r\x20", false)),
        'w' => Some(("0-9A-Za-z_", true)),
        'W' => Some(("0-9A-Za-z_", false)),
        _ => None,
    }
}

/// A literal character in `regex` syntax (ASCII punctuation and controls as `\x{..}`).
fn push_literal(out: &mut String, c: char) {
    if c.is_ascii_alphanumeric() || c == '_' || !c.is_ascii() {
        out.push(c);
    } else {
        out.push_str(&format!("\\x{{{:X}}}", c as u32));
    }
}

/// Go `parseEscape`: an escape that stands for one character. `s` starts with `\`.
fn parse_escape(s: &str) -> Result<(char, &str), String> {
    let t = &s[1..];
    let Some(c) = t.chars().next() else {
        return Err(err("trailing backslash at end of expression", ""));
    };
    let mut t = &t[c.len_utf8()..];
    let invalid = |t: &str| err("invalid escape sequence", &s[..s.len() - t.len()]);
    match c {
        '1'..='7' if !t.starts_with(|d: char| ('0'..='7').contains(&d)) => Err(invalid(t)),
        '0'..='7' => {
            let mut r = c as u32 - '0' as u32;
            for _ in 1..3 {
                match t.chars().next() {
                    Some(d @ '0'..='7') => {
                        r = r * 8 + (d as u32 - '0' as u32);
                        t = &t[1..];
                    }
                    _ => break,
                }
            }
            Ok((char::from_u32(r).unwrap_or('\0'), t))
        }
        'x' => {
            let Some(c1) = t.chars().next() else {
                return Err(invalid(t));
            };
            t = &t[c1.len_utf8()..];
            if c1 == '{' {
                let mut r: u32 = 0;
                let mut digits = 0;
                loop {
                    let Some(h) = t.chars().next() else {
                        return Err(invalid(t));
                    };
                    t = &t[h.len_utf8()..];
                    if h == '}' {
                        break;
                    }
                    let Some(v) = h.to_digit(16) else {
                        return Err(invalid(t));
                    };
                    r = r * 16 + v;
                    if r > 0x10FFFF {
                        return Err(invalid(t));
                    }
                    digits += 1;
                }
                if digits == 0 {
                    return Err(invalid(t));
                }
                // Surrogates: Go keeps them as runes that match nothing.
                return Ok((char::from_u32(r).unwrap_or('\u{FFFD}'), t));
            }
            let Some(c2) = t.chars().next() else {
                return Err(invalid(t));
            };
            t = &t[c2.len_utf8()..];
            match (c1.to_digit(16), c2.to_digit(16)) {
                (Some(x), Some(y)) => Ok((char::from_u32(x * 16 + y).unwrap_or('\0'), t)),
                _ => Err(invalid(t)),
            }
        }
        'a' => Ok(('\u{7}', t)),
        'f' => Ok(('\u{c}', t)),
        'n' => Ok(('\n', t)),
        'r' => Ok(('\r', t)),
        't' => Ok(('\t', t)),
        'v' => Ok(('\u{b}', t)),
        // Escaped ASCII punctuation is always the character itself.
        c if c.is_ascii() && !c.is_ascii_alphanumeric() => Ok((c, t)),
        _ => Err(invalid(t)),
    }
}

/// Go `parseUnicodeClass` for `s` starting with `\p` or `\P`: the `regex` class and the rest.
fn parse_unicode_class(s: &str) -> Result<(String, &str), String> {
    let mut positive = s.as_bytes()[1] == b'p';
    let t = &s[2..];
    let (seq, name, rest) = match t.chars().next() {
        Some('{') => {
            let Some(end) = s.find('}') else {
                return Err(err("invalid character class range", s));
            };
            (&s[..=end], &s[3..end], &s[end + 1..])
        }
        Some(c) => {
            let end = 2 + c.len_utf8();
            (&s[..end], &s[2..end], &s[end..])
        }
        None => (s, "", ""),
    };
    let name = match name.strip_prefix('^') {
        Some(n) => {
            positive = !positive;
            n
        }
        None => name,
    };
    match unicode_class(name, positive) {
        Some(class) => Ok((class, rest)),
        None => Err(err("invalid character class range", seq)),
    }
}

/// Go `parseRepeat`: `{n}`, `{n,}`, `{n,m}` without leading zeros. Returns (min, max, rest),
/// max = -1 for no maximum, -2 for a number too big; `None` when `{` is a literal.
fn parse_repeat(s: &str) -> Option<(i64, i64, &str)> {
    fn int(s: &str) -> Option<(i64, &str)> {
        let digits = s.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 || (digits >= 2 && s.starts_with('0')) {
            return None;
        }
        let n = s[..digits]
            .bytes()
            .try_fold(0i64, |n, d| {
                (n < 100_000_000).then(|| n * 10 + i64::from(d - b'0'))
            })
            .unwrap_or(-2);
        Some((n, &s[digits..]))
    }
    let s = s.strip_prefix('{')?;
    let (min, s) = int(s)?;
    let (max, s) = match s.strip_prefix(',') {
        None => (min, s),
        Some(s) if s.starts_with('}') => (-1, s),
        Some(s) => int(s)?,
    };
    Some((min, max, s.strip_prefix('}')?))
}

/// Go `parseClass` for `s` starting with `[`; appends the `regex` class.
fn parse_class<'a>(s: &'a str, out: &mut String) -> Result<&'a str, String> {
    let mut t = &s[1..];
    out.push('[');
    if let Some(rest) = t.strip_prefix('^') {
        out.push('^');
        t = rest;
    }
    let class_char = |t: &'a str| -> Result<(char, &'a str), String> {
        match t.chars().next() {
            None => Err(err("missing closing ]", s)),
            Some('\\') => parse_escape(t),
            Some(c) => Ok((c, &t[c.len_utf8()..])),
        }
    };
    let mut first = true;
    while first || !t.starts_with(']') {
        first = false;
        // POSIX class: [:alpha:], [:^alpha:].
        if t.len() > 2
            && t.starts_with("[:")
            && let Some(i) = t[2..].find(":]")
        {
            let name = &t[..i + 4];
            let bare = &t[2..i + 2];
            if !POSIX.contains(&bare.strip_prefix('^').unwrap_or(bare)) {
                return Err(err("invalid character class range", name));
            }
            out.push_str(name);
            t = &t[i + 4..];
            continue;
        }
        if t.starts_with("\\p") || t.starts_with("\\P") {
            let (class, rest) = parse_unicode_class(t)?;
            out.push_str(&class);
            t = rest;
            continue;
        }
        if let Some((set, positive)) = t
            .strip_prefix('\\')
            .and_then(|r| r.chars().next())
            .and_then(perl_class)
        {
            if positive {
                out.push_str(set);
            } else {
                out.push_str(&format!("[^{set}]"));
            }
            t = &t[2..];
            continue;
        }
        let range = t;
        let (lo, rest) = class_char(t)?;
        t = rest;
        let mut hi = lo;
        if t.len() >= 2 && t.starts_with('-') && !t[1..].starts_with(']') {
            let (h, rest) = class_char(&t[1..])?;
            t = rest;
            if h < lo {
                return Err(err(
                    "invalid character class range",
                    &range[..range.len() - t.len()],
                ));
            }
            hi = h;
        }
        push_literal(out, lo);
        if hi != lo {
            out.push('-');
            push_literal(out, hi);
        }
    }
    out.push(']');
    Ok(&t[1..])
}

/// Go `parsePerlFlags` for `s` starting with `(?`: named group or flags. Returns the rest
/// and whether a group was opened.
fn parse_perl_flags<'a>(s: &'a str, out: &mut String) -> Result<(&'a str, bool), String> {
    let b = s.as_bytes();
    if (s.len() > 4 && b[2] == b'P' && b[3] == b'<') || (s.len() > 3 && b[2] == b'<') {
        let begin = if b[2] == b'<' { 3 } else { 4 };
        let Some(end) = s.find('>') else {
            return Err(err("invalid named capture", s));
        };
        let name = &s[begin..end];
        if name.is_empty() || !name.bytes().all(|c| c == b'_' || c.is_ascii_alphanumeric()) {
            return Err(err("invalid named capture", &s[..=end]));
        }
        // Names are not used here; a plain group avoids `regex`'s stricter name rules.
        out.push('(');
        return Ok((&s[end + 1..], true));
    }
    let mut t = &s[2..];
    let mut flags = String::new();
    let mut negated = false;
    let mut saw_flag = false;
    while let Some(c) = t.chars().next() {
        t = &t[c.len_utf8()..];
        match c {
            'i' | 'm' | 's' | 'U' => {
                flags.push(c);
                saw_flag = true;
            }
            '-' if !negated => {
                flags.push('-');
                negated = true;
                saw_flag = false;
            }
            ':' | ')' if !negated || saw_flag => {
                if c == ':' {
                    out.push_str(&format!("(?{flags}:"));
                } else if !flags.is_empty() {
                    out.push_str(&format!("(?{flags})"));
                }
                return Ok((t, c == ':'));
            }
            _ => break,
        }
    }
    Err(err(
        "invalid or unsupported Perl syntax",
        &s[..s.len() - t.len()],
    ))
}

/// Translates a Go pattern into `regex` syntax; `Err` with Go's message when Go refuses it.
pub fn translate(pattern: &str) -> Result<String, String> {
    let mut out = String::with_capacity(pattern.len() + 16);
    let mut t = pattern;
    let mut depth = 0usize;
    // Text from the previous repetition operator to the end (Go `lastRepeat`).
    let mut last_repeat: Option<&str> = None;
    // Something precedes that a repetition can apply to (Go: the top of the parse stack is
    // not a `(` or a `|`).
    let mut operand = false;
    // Where flag groups (`(?i)`) emitted since the last operand start: Go repeats the
    // operand before them (`a(?i)*` is `a*` then the flags), which `regex` refuses.
    let mut flags_at: Option<usize> = None;
    while let Some(c) = t.chars().next() {
        let mut repeat = None;
        let mut is_operand = true;
        let mut pending_flags = None;
        match c {
            '(' if t.starts_with("(?") => {
                let start = out.len();
                let (rest, opened) = parse_perl_flags(t, &mut out)?;
                depth += usize::from(opened);
                // A flag group like `(?i)` adds nothing to repeat.
                is_operand = !opened && operand;
                if !opened {
                    pending_flags = Some(flags_at.unwrap_or(start));
                }
                t = rest;
            }
            '(' | '|' => {
                out.push(c);
                depth += usize::from(c == '(');
                is_operand = false;
                t = &t[1..];
            }
            ')' => {
                if depth == 0 {
                    return Err(err("unexpected )", pattern));
                }
                depth -= 1;
                out.push(')');
                t = &t[1..];
            }
            '^' | '$' | '.' => {
                out.push(c);
                t = &t[1..];
            }
            '[' => t = parse_class(t, &mut out)?,
            '*' | '+' | '?' | '{' => {
                let before = t;
                let after = if c == '{' {
                    match parse_repeat(t) {
                        None => {
                            // Not a repetition: `{` is a literal.
                            out.push_str("\\{");
                            t = &t[1..];
                            last_repeat = None;
                            operand = true;
                            flags_at = None;
                            continue;
                        }
                        Some((min, max, after)) => {
                            if !(0..=1000).contains(&min)
                                || max > 1000
                                || max == -2
                                || (max >= 0 && min > max)
                            {
                                return Err(err(
                                    "invalid repeat count",
                                    &before[..before.len() - after.len()],
                                ));
                            }
                            after
                        }
                    }
                } else {
                    &t[1..]
                };
                let after = after.strip_prefix('?').unwrap_or(after);
                if let Some(last) = last_repeat {
                    return Err(err(
                        "invalid nested repetition operator",
                        &last[..last.len() - after.len()],
                    ));
                }
                if !operand {
                    return Err(err(
                        "missing argument to repetition operator",
                        &before[..before.len() - after.len()],
                    ));
                }
                let op = &before[..before.len() - after.len()];
                match flags_at {
                    Some(at) => out.insert_str(at, op),
                    None => out.push_str(op),
                }
                repeat = Some(before);
                t = after;
            }
            '\\' => {
                t = match t[1..].chars().next() {
                    Some('A') => {
                        out.push_str(r"\A");
                        &t[2..]
                    }
                    Some('z') => {
                        out.push_str(r"\z");
                        &t[2..]
                    }
                    Some('b') => {
                        out.push_str(r"(?-u:\b)");
                        &t[2..]
                    }
                    Some('B') => {
                        out.push_str(r"(?-u:\B)");
                        &t[2..]
                    }
                    Some('C') => return Err(err("invalid escape sequence", r"\C")),
                    Some('Q') => {
                        // Literal text up to `\E`.
                        let (lit, rest) = match t[2..].find(r"\E") {
                            Some(i) => (&t[2..2 + i], &t[2 + i + 2..]),
                            None => (&t[2..], ""),
                        };
                        lit.chars().for_each(|c| push_literal(&mut out, c));
                        is_operand = operand || !lit.is_empty();
                        rest
                    }
                    Some('p' | 'P') => {
                        let (class, rest) = parse_unicode_class(t)?;
                        out.push_str(&class);
                        rest
                    }
                    Some(c) if perl_class(c).is_some() => {
                        let (set, positive) = perl_class(c).expect("perl class");
                        out.push_str(&format!("[{}{set}]", if positive { "" } else { "^" }));
                        &t[2..]
                    }
                    _ => {
                        let (lit, rest) = parse_escape(t)?;
                        push_literal(&mut out, lit);
                        rest
                    }
                };
            }
            c => {
                push_literal(&mut out, c);
                t = &t[c.len_utf8()..];
            }
        }
        last_repeat = repeat;
        operand = is_operand;
        flags_at = pending_flags;
    }
    if depth > 0 {
        return Err(err("missing closing )", pattern));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(p: &str) -> Regex {
        compile(p).unwrap_or_else(|e| panic!("{p}: {e}"))
    }

    #[test]
    fn ascii_perl_classes() {
        assert!(ok(r"^\d+$").is_match("123"));
        assert!(!ok(r"^\d+$").is_match("١٢٣"));
        assert!(ok(r"^\D+$").is_match("١٢٣"));
        assert!(!ok(r"^\w+$").is_match("é"));
        assert!(ok(r"^[^\w]+$").is_match("é"));
        assert!(!ok(r"^\s$").is_match("\u{a0}"));
        assert!(ok(r"^\S$").is_match("\u{2003}"));
        assert!(ok(r"^[\d.]+$").is_match("1.5"));
        assert!(ok(r"^[\D]$").is_match("é"));
        // ASCII word boundary: é is not a word character.
        assert!(ok(r"a\b").is_match("aé"));
        assert!(!ok(r"\bé").is_match("éé"));
        assert!(ok(r"\Bé").is_match("éé"));
        // Case folding still applies.
        assert!(ok(r"(?i)^\w$").is_match("\u{212a}"));
    }

    #[test]
    fn go_only_syntax_compiles() {
        assert!(ok("^a{$").is_match("a{"));
        assert!(ok("^{x}$").is_match("{x}"));
        assert!(ok("^a{1,$").is_match("a{1,"));
        assert!(ok("^a{,2}$").is_match("a{,2}"));
        assert!(ok("^a{01}$").is_match("a{01}"));
        assert!(ok("^a{2}$").is_match("aa"));
        assert!(ok(r"^\Q.*\E$").is_match(".*"));
        assert!(!ok(r"^\Q.*\E$").is_match("ab"));
        // Without `\E`, the rest of the pattern is literal (`$` included).
        assert!(ok(r"^\Q*a").is_match("*a"));
        assert!(ok(r"^\Q*a$").is_match("*a$"));
        assert!(!ok(r"^\Q*a$").is_match("*a"));
        assert!(ok(r"^\Qab\E*$").is_match("abbb"));
        assert!(ok(r"^\0\012\101$").is_match("\0\nA"));
        assert!(ok(r"^[[]$").is_match("["));
        assert!(ok(r"^[a[]+$").is_match("[a["));
        assert!(ok(r"^[]a]+$").is_match("]a"));
        assert!(ok(r"^[a&&b]+$").is_match("a&&b"));
        assert!(ok(r"^[a~~b]+$").is_match("~"));
        assert!(ok(r"^[!--]+$").is_match("!-,"));
        assert!(ok(r"^[\d-z]+$").is_match("1-z"));
        assert!(ok(r"^[a-]+$").is_match("a-"));
        assert!(ok(r"^\p{^Greek}$").is_match("a"));
        assert!(!ok(r"^\P{^Greek}$").is_match("a"));
        assert!(ok(r"^(?)a$").is_match("a"));
        assert!(ok(r"^(?P<1a>x)(?<b>y)(?P<b>z)$").is_match("xyz"));
        assert!(ok(r"^\<\>\_\-$").is_match("<>_-"));
        assert!(ok(r"^\x41\x{42}\a\f\t\n\r\v$").is_match("AB\u{7}\u{c}\t\n\r\u{b}"));
        assert!(ok(r"(?i-s:a.)").is_match("A_"));
        assert!(ok(r"(?U)a+").find("aaa").is_some_and(|m| m.len() == 1));
        assert!(ok(r"^[[:alpha:]-]+$").is_match("a-b"));
        assert!(ok(r"^[[:^digit:]]$").is_match("a"));
    }

    #[test]
    fn go_rejected_syntax_fails() {
        for (p, msg) in [
            ("a**", "invalid nested repetition operator: `**`"),
            ("a+*", "invalid nested repetition operator: `+*`"),
            ("a*?*", "invalid nested repetition operator: `*?*`"),
            ("x{2}{3}", "invalid nested repetition operator: `{2}{3}`"),
            ("a{1001}", "invalid repeat count: `{1001}`"),
            ("a{2,1}", "invalid repeat count: `{2,1}`"),
            ("a{999999999}", "invalid repeat count: `{999999999}`"),
            (r"\u0041", r"invalid escape sequence: `\u`"),
            (r"\U00000041", r"invalid escape sequence: `\U`"),
            (r"\e", r"invalid escape sequence: `\e`"),
            (r"\1", r"invalid escape sequence: `\1`"),
            (r"\8", r"invalid escape sequence: `\8`"),
            (r"\C", r"invalid escape sequence: `\C`"),
            (r"\é", r"invalid escape sequence: `\é`"),
            (r"\x{110000}", r"invalid escape sequence: `\x{110000`"),
            (r"\xg1", r"invalid escape sequence: `\xg1`"),
            (r"[\b]", r"invalid escape sequence: `\b`"),
            ("a\\", "trailing backslash at end of expression: ``"),
            ("(?x)a", "invalid or unsupported Perl syntax: `(?x`"),
            ("(?i-)a", "invalid or unsupported Perl syntax: `(?i-)`"),
            ("(?=a)", "invalid or unsupported Perl syntax: `(?=`"),
            ("(?#c)", "invalid or unsupported Perl syntax: `(?#`"),
            ("(?P=n)", "invalid or unsupported Perl syntax: `(?P`"),
            ("(?<é>x)", "invalid named capture: `(?<é>`"),
            ("(?P<n", "invalid named capture: `(?P<n`"),
            (
                r"\p{Alphabetic}",
                r"invalid character class range: `\p{Alphabetic}`",
            ),
            (
                r"\p{IsGreek}",
                r"invalid character class range: `\p{IsGreek}`",
            ),
            (r"\pX", r"invalid character class range: `\pX`"),
            (r"\p", r"invalid character class range: `\p`"),
            (r"\p{L", r"invalid character class range: `\p{L`"),
            ("[[:foo:]]", "invalid character class range: `[:foo:]`"),
            ("[a--]", "invalid character class range: `a--`"),
            ("[z-a]", "invalid character class range: `z-a`"),
            ("[a-[:alpha:]]", "invalid character class range: `a-[`"),
            ("[a", "missing closing ]: `[a`"),
            ("[]", "missing closing ]: `[]`"),
            ("(a", "missing closing ): `(a`"),
            ("a)", "unexpected ): `a)`"),
            ("*a", "missing argument to repetition operator: `*`"),
            ("a|+", "missing argument to repetition operator: `+`"),
            ("(??)", "invalid or unsupported Perl syntax: `(??`"),
            ("(*)", "missing argument to repetition operator: `*`"),
            ("(?i){2}", "missing argument to repetition operator: `{2}`"),
            (r"\Q\E*", "missing argument to repetition operator: `*`"),
            ("(?:*)", "missing argument to repetition operator: `*`"),
            (r"\x4", r"invalid escape sequence: `\x4`"),
            (r"\x", r"invalid escape sequence: `\x`"),
            (r"\p{}", r"invalid character class range: `\p{}`"),
            (r"\p{^}", r"invalid character class range: `\p{^}`"),
            (r"\pé", r"invalid character class range: `\pé`"),
            ("[[:a]b:]", "invalid character class range: `[:a]b:]`"),
            ("(?P<a", "invalid named capture: `(?P<a`"),
            ("(?<", "invalid or unsupported Perl syntax: `(?<`"),
            ("(?P<", "invalid or unsupported Perl syntax: `(?P`"),
            ("(?", "invalid or unsupported Perl syntax: `(?`"),
            ("(?i", "invalid or unsupported Perl syntax: `(?i`"),
            ("(?--i)", "invalid or unsupported Perl syntax: `(?--`"),
            ("(?-)", "invalid or unsupported Perl syntax: `(?-)`"),
        ] {
            assert_eq!(
                compile(p).unwrap_err(),
                format!("error parsing regexp: {msg}"),
                "{p}"
            );
        }
        // Errors found by `regex` keep Go's prefix.
        assert!(
            compile(r"\pL{1000}{1}")
                .unwrap_err()
                .starts_with("error parsing regexp: invalid nested")
        );
        for p in [
            "a*{",
            r"\Qa\E{2}",
            "a{1000}",
            "a{0,1000}",
            "a{,}",
            "^*",
            r"\b+",
        ] {
            ok(p);
        }
        // A repetition after a flag group repeats what precedes the group.
        assert_eq!(translate("a(?i)(?m)*b").unwrap(), "a*(?i)(?m)b");
        assert!(ok("^a(?i)*b$").is_match("aaaB"));
        assert!(!ok("^a(?i)*b$").is_match("AB"));
        assert_eq!(translate(r"a(?i){*").unwrap(), r"a(?i)\{*");
    }

    #[test]
    fn unicode_names_like_go_1_25() {
        for name in [
            "L",
            "l",
            "lu",
            "Letter",
            "letter",
            "Greek",
            "greek",
            "GREEK",
            " Gr-eek ",
            "Han",
            "Any",
            "ASCII",
            "Assigned",
            "LC",
            "Cn",
            "Old_Italic",
            "Yi",
            "cntrl",
            "Cased_Letter",
        ] {
            ok(&format!(r"\p{{{name}}}"));
        }
        // Every category and script of Go's tables is known to `regex`.
        for name in CATEGORIES.iter().chain(SCRIPTS.iter()) {
            ok(&format!(r"\p{{{name}}}[\P{{{name}}}]"));
        }
        assert!(ok(r"^\p{Greek}$").is_match("α"));
        // Script, not script extensions: U+0342 is Inherited (extensions: Greek).
        assert!(!ok(r"^\p{Greek}$").is_match("\u{342}"));
        assert!(ok(r"^\pL+$").is_match("éa"));
        assert!(ok(r"^\p{Assigned}$").is_match("a"));
        assert!(!ok(r"^\p{Assigned}$").is_match("\u{378}"));
        // Go's C has no unassigned code points.
        assert!(!ok(r"^\p{C}$").is_match("\u{378}"));
        assert!(ok(r"^\P{C}$").is_match("\u{378}"));
        assert!(ok(r"^\p{C}$").is_match("\u{1}"));
        assert!(ok(r"^[\p{C}a]$").is_match("a"));
        assert!(ok(r"^[^\p{C}]$").is_match("\u{378}"));
    }
}
