//! The query language (SPEC §8.7): what lists, searches and the home
//! view's buckets are written in, of mail, rooms, to-dos and events.
//! Parsing only; each kind's crate resolves names and runs it.
//!
//! Terms are words (each a prefix of a word in the item), "exact phrases"
//! and `operator:value`s; terms side by side must all match, and `OR`,
//! `NOT` (or `-`) and parentheses combine them. An operator uplink does
//! not know is an error rather than text, so that adding one never
//! changes what a query already meant. A term a kind of item does not
//! have (`from:` of a room, `due:` of mail) is false of it, and so its
//! negation true.

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Query {
    And(Vec<Query>),
    Or(Vec<Query>),
    Not(Box<Query>),
    Term(Term),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Term {
    /// Words in the message's `fields`, or any field.
    Text {
        fields: Option<Fields>,
        text: String,
        /// A word is matched as a prefix, a quoted phrase exactly.
        exact: bool,
    },
    /// The entry's own account's address, as sender or recipient.
    Me(Fields),
    /// Labels by name, in every account: `label:`, `in:`, `is:`.
    Label(String),
    /// Trash and Spam too: `in:anywhere`.
    Anywhere,
    /// The account, by id or address.
    Account(String),
    HasAttachment,
    /// Received at or after this time, in unix ms.
    After(i64),
    /// Received before this time, in unix ms.
    Before(i64),
    Larger(u64),
    Smaller(u64),
    /// The message's Message-ID, without angle brackets.
    MessageId(String),
    /// Of a kind: `is:mail`, `is:room`, `is:todo`, `is:event`.
    Kind(Kind),
    /// Unread: mail with `UNREAD`, a room with messages unread.
    Unread,
    /// A room whose unread messages mention the account (or a keyword).
    Mention,
    /// A room with unread messages that notify, by the push rules.
    Notifying,
    /// A direct chat.
    Direct,
    /// A room the account is invited to.
    Invited,
    /// A room in a space by this name, however deeply.
    Space(String),
    /// A to-do due on the day.
    Due(Day),
    /// A to-do due before today.
    Overdue,
    /// A to-do with a due date.
    HasDue,
    /// A to-do done.
    Done,
}

/// The kinds of item a query lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Mail,
    Room,
    Todo,
    Event,
}

/// A day, as to-dos are due: of the user's calendar, not UTC, so that
/// `today` is the user's (the request says which day that is).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Day {
    Today,
    Tomorrow,
    /// Days since 1970-01-01.
    Date(i64),
}

impl Day {
    /// Days since 1970-01-01, `today` being that.
    pub fn days(self, today: i64) -> i64 {
        match self {
            Day::Today => today,
            Day::Tomorrow => today + 1,
            Day::Date(days) => days,
        }
    }
}

/// A date, `YYYY-MM-DD`, as days since 1970-01-01.
pub fn days(date: &str) -> Option<i64> {
    let (year, rest) = date.split_once('-')?;
    let (month, day) = rest.split_once('-')?;
    let number = |s: &str, len: usize| {
        (s.len() == len && s.bytes().all(|b| b.is_ascii_digit())).then(|| s.parse::<i64>().unwrap())
    };
    let (year, month, day) = (number(year, 4)?, number(month, 2)?, number(day, 2)?);
    ((1..=12).contains(&month) && (1..=days_in_month(year, month)).contains(&day))
        .then(|| days_from_civil(year, month, day))
}

/// Fields of the message that `from:`, `to:` and the like search.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fields {
    From,
    /// To or Cc, as in Gmail and notmuch.
    To,
    Cc,
    Subject,
    Body,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invalid(pub String);

impl fmt::Display for Invalid {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The operators, as an unknown one's error lists them.
pub const OPERATORS: &[&str] = &[
    "from",
    "to",
    "cc",
    "subject",
    "body",
    "label",
    "tag",
    "in",
    "is",
    "category",
    "has",
    "account",
    "after",
    "before",
    "newer_than",
    "older_than",
    "larger",
    "smaller",
    "id",
    "rfc822msgid",
    "space",
    "due",
];

/// Parses `query`; `now` (unix ms) is what `newer_than:` and
/// `older_than:` count back from. Dates are of UTC.
pub fn parse(query: &str, now: i64) -> Result<Query, Invalid> {
    let tokens = tokens(query)?;
    let mut parser = Parser {
        tokens: &tokens,
        at: 0,
        now,
    };
    let query = parser.or()?;
    match parser.tokens.get(parser.at) {
        None => Ok(query),
        Some(Token::Close) => Err(invalid("unmatched )")),
        Some(token) => unreachable!("the parser stops only at ) or the end, not at {token:?}"),
    }
}

fn invalid(message: impl Into<String>) -> Invalid {
    Invalid(message.into())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Open,
    Close,
    Or,
    And,
    Not,
    /// `-`, before a term or a group: NOT.
    Minus,
    Word(String),
    Phrase(String),
    Operator {
        name: String,
        value: String,
        quoted: bool,
    },
}

/// Splits on whitespace and parentheses; a quoted phrase is one token
/// whatever it holds, with `\"` and `\\` for a quote and a backslash.
fn tokens(query: &str) -> Result<Vec<Token>, Invalid> {
    if query.contains('\0') {
        return Err(invalid("a query cannot hold NUL"));
    }
    let mut tokens = vec![];
    let mut chars = query.chars().peekable();
    let delimits = |c: char| c.is_whitespace() || matches!(c, '(' | ')' | '"');
    while let Some(&c) = chars.peek() {
        match c {
            c if c.is_whitespace() => {
                chars.next();
            }
            '(' => {
                chars.next();
                tokens.push(Token::Open);
            }
            ')' => {
                chars.next();
                tokens.push(Token::Close);
            }
            '"' => {
                chars.next();
                tokens.push(Token::Phrase(quoted(&mut chars)?));
            }
            _ => {
                let mut word = String::new();
                while let Some(&c) = chars.peek() {
                    if delimits(c) {
                        break;
                    }
                    word.push(c);
                    chars.next();
                }
                // A leading `-` negates what follows it, unless nothing does.
                let word = match word.strip_prefix('-') {
                    Some(rest)
                        if !rest.is_empty()
                            || chars.peek().is_some_and(|&c| matches!(c, '(' | '"')) =>
                    {
                        tokens.push(Token::Minus);
                        rest.to_owned()
                    }
                    _ => word,
                };
                if word.is_empty() {
                    continue;
                }
                let operator = word.split_once(':').filter(|(name, _)| {
                    name.starts_with(|c: char| c.is_ascii_alphabetic())
                        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                });
                tokens.push(match operator {
                    Some((name, value)) => {
                        let (value, quoted) = match (value.is_empty(), chars.peek()) {
                            (true, Some('"')) => {
                                chars.next();
                                (self::quoted(&mut chars)?, true)
                            }
                            (true, _) => return Err(invalid(format!("{name}: needs a value"))),
                            (false, _) => (value.to_owned(), false),
                        };
                        Token::Operator {
                            name: name.to_ascii_lowercase(),
                            value,
                            quoted,
                        }
                    }
                    None => match word.as_str() {
                        "OR" => Token::Or,
                        "AND" => Token::And,
                        "NOT" => Token::Not,
                        _ => Token::Word(word),
                    },
                });
            }
        }
    }
    Ok(tokens)
}

/// The rest of a quoted phrase, after its opening quote.
fn quoted(chars: &mut impl Iterator<Item = char>) -> Result<String, Invalid> {
    let mut phrase = String::new();
    loop {
        match chars.next() {
            None => return Err(invalid("unterminated \"")),
            Some('"') => return Ok(phrase),
            Some('\\') => match chars.next() {
                Some(c @ ('"' | '\\')) => phrase.push(c),
                _ => return Err(invalid("in quotes, \\ escapes only \" and \\")),
            },
            Some(c) => phrase.push(c),
        }
    }
}

struct Parser<'a> {
    tokens: &'a [Token],
    at: usize,
    now: i64,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<&'a Token> {
        self.tokens.get(self.at)
    }

    fn next(&mut self) -> Option<&'a Token> {
        let token = self.tokens.get(self.at);
        self.at += 1;
        token
    }

    /// Terms joined by OR, to a `)` or the end.
    fn or(&mut self) -> Result<Query, Invalid> {
        let mut any = vec![self.and()?];
        while self.peek() == Some(&Token::Or) {
            self.next();
            let and = self.and()?;
            if and == Query::And(vec![]) {
                return Err(invalid("OR needs a term after it"));
            }
            any.push(and);
        }
        Ok(match any.len() {
            1 => any.pop().unwrap(),
            _ => {
                if any.contains(&Query::And(vec![])) {
                    return Err(invalid("OR needs a term before it"));
                }
                Query::Or(any)
            }
        })
    }

    /// Terms side by side, or joined by AND, to an OR, a `)` or the end.
    fn and(&mut self) -> Result<Query, Invalid> {
        let mut all = vec![];
        loop {
            match self.peek() {
                None | Some(Token::Or | Token::Close) => break,
                Some(Token::And) => {
                    self.next();
                    if all.is_empty()
                        || matches!(self.peek(), None | Some(Token::Or | Token::Close))
                    {
                        return Err(invalid("AND needs a term on each side"));
                    }
                }
                Some(_) => all.push(self.unary()?),
            }
        }
        Ok(match all.len() {
            1 => all.pop().unwrap(),
            _ => Query::And(all),
        })
    }

    fn unary(&mut self) -> Result<Query, Invalid> {
        match self.next() {
            Some(Token::Not | Token::Minus) => {
                if matches!(
                    self.peek(),
                    None | Some(Token::Or | Token::And | Token::Close)
                ) {
                    return Err(invalid("NOT needs a term after it"));
                }
                Ok(Query::Not(Box::new(self.unary()?)))
            }
            Some(Token::Open) => {
                let group = self.or()?;
                if self.next() != Some(&Token::Close) {
                    return Err(invalid("unmatched ("));
                }
                if group == Query::And(vec![]) {
                    return Err(invalid("() is empty"));
                }
                Ok(group)
            }
            Some(Token::Word(word)) => Ok(Query::Term(Term::Text {
                fields: None,
                text: word.clone(),
                exact: false,
            })),
            Some(Token::Phrase(phrase)) => Ok(Query::Term(Term::Text {
                fields: None,
                text: phrase.clone(),
                exact: true,
            })),
            Some(Token::Operator {
                name,
                value,
                quoted,
            }) => operator(name, value, *quoted, self.now),
            token => unreachable!("and() stops before {token:?}"),
        }
    }
}

fn operator(name: &str, value: &str, quoted: bool, now: i64) -> Result<Query, Invalid> {
    let term = |term| Ok(Query::Term(term));
    let label = |name: &str| term(Term::Label(name.to_owned()));
    let text = |fields| {
        term(
            match (fields, value.eq_ignore_ascii_case("me") && !quoted) {
                (Fields::From | Fields::To | Fields::Cc, true) => Term::Me(fields),
                _ => Term::Text {
                    fields: Some(fields),
                    text: value.to_owned(),
                    exact: quoted,
                },
            },
        )
    };
    match name {
        "from" => text(Fields::From),
        "to" => text(Fields::To),
        "cc" => text(Fields::Cc),
        "subject" => text(Fields::Subject),
        "body" => text(Fields::Body),
        "label" | "tag" => label(value),
        "in" => match value.to_ascii_lowercase().as_str() {
            "anywhere" => term(Term::Anywhere),
            // Gmail's name for its label `DRAFT`, and M365's.
            "drafts" => label("DRAFT"),
            _ => label(value),
        },
        "is" => match value.to_ascii_lowercase().as_str() {
            "unread" => term(Term::Unread),
            "read" => Ok(Query::Not(Box::new(Query::Term(Term::Unread)))),
            "starred" => label("STARRED"),
            "important" => label("IMPORTANT"),
            "mail" => term(Term::Kind(Kind::Mail)),
            "room" => term(Term::Kind(Kind::Room)),
            "todo" => term(Term::Kind(Kind::Todo)),
            "event" => term(Term::Kind(Kind::Event)),
            "mention" => term(Term::Mention),
            "notifying" => term(Term::Notifying),
            "dm" => term(Term::Direct),
            "invite" => term(Term::Invited),
            "overdue" => term(Term::Overdue),
            "done" => term(Term::Done),
            _ => Err(invalid(format!(
                "is:{value}: is: takes unread, read, starred, important, mail, room, todo, \
                 event, mention, notifying, dm, invite, overdue or done"
            ))),
        },
        "space" => term(Term::Space(value.to_owned())),
        "due" => term(Term::Due(match value.to_ascii_lowercase().as_str() {
            "today" => Day::Today,
            "tomorrow" => Day::Tomorrow,
            _ => Day::Date(date(name, value)? / DAY_MS),
        })),
        "category" => label(&format!("CATEGORY_{}", value.to_ascii_uppercase())),
        "has" => match value.to_ascii_lowercase().as_str() {
            "attachment" => term(Term::HasAttachment),
            "due" => term(Term::HasDue),
            _ => Err(invalid(format!(
                "has:{value}: has: takes attachment or due"
            ))),
        },
        "account" => term(Term::Account(value.to_owned())),
        "after" => term(Term::After(date(name, value)?)),
        "before" => term(Term::Before(date(name, value)?)),
        "newer_than" => term(Term::After(now - age(name, value)?)),
        "older_than" => term(Term::Before(now - age(name, value)?)),
        "larger" => term(Term::Larger(size(name, value)?)),
        "smaller" => term(Term::Smaller(size(name, value)?)),
        "id" | "rfc822msgid" => {
            let id = value.strip_prefix('<').unwrap_or(value);
            let id = id.strip_suffix('>').unwrap_or(id);
            term(Term::MessageId(id.to_owned()))
        }
        _ => Err(invalid(format!(
            "unknown operator {name}: (quote \"{name}:{value}\" to search for it as text); \
             operators: {}",
            OPERATORS.join(", ")
        ))),
    }
}

/// Whether `query` matches an item, each term by `term`: for kinds
/// matched outside SQL, as rooms, to-dos and events are.
pub fn matches(query: &Query, term: &impl Fn(&Term) -> bool) -> bool {
    match query {
        Query::And(all) => all.iter().all(|q| matches(q, term)),
        Query::Or(any) => any.iter().any(|q| matches(q, term)),
        Query::Not(q) => !matches(q, term),
        Query::Term(t) => term(t),
    }
}

/// The terms of `query`, negated or not.
pub fn terms(query: &Query) -> Vec<&Term> {
    match query {
        Query::And(all) | Query::Or(all) => all.iter().flat_map(terms).collect(),
        Query::Not(q) => terms(q),
        Query::Term(t) => vec![t],
    }
}

/// Whether `query` names a term `term` holds of, outside a negation: as
/// `in:trash` lists Trash, which is otherwise left out, and `is:done`
/// to-dos done.
pub fn names(query: &Query, term: &impl Fn(&Term) -> bool) -> bool {
    fn within(query: &Query, term: &impl Fn(&Term) -> bool, positive: bool) -> bool {
        match query {
            Query::And(all) | Query::Or(all) => all.iter().any(|q| within(q, term, positive)),
            Query::Not(q) => within(q, term, !positive),
            Query::Term(t) => positive && term(t),
        }
    }
    within(query, term, true)
}

/// Whether `text`, a term's, is in `haystack`, as FTS5 matches it in mail:
/// by words, ignoring case; the last word as a prefix unless `exact`.
pub fn text_matches(haystack: &str, text: &str, exact: bool) -> bool {
    let needle = words(text);
    let Some((last, before)) = needle.split_last() else {
        // Of no words, as punctuation: FTS5 matches nothing.
        return false;
    };
    let words = words(haystack);
    words.windows(needle.len()).any(|window| {
        let (end, start) = window.split_last().expect("windows are not empty");
        start == before
            && if exact {
                end == last
            } else {
                end.starts_with(last.as_str())
            }
    })
}

/// Words, as FTS5's unicode61 splits them: runs of letters and digits,
/// lowercased.
fn words(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect()
}

const DAY_MS: i64 = 86_400_000;

/// A date, `2026-11-09` or `2026/11/9`: its start, UTC, in unix ms.
fn date(name: &str, value: &str) -> Result<i64, Invalid> {
    let bad = || {
        invalid(format!(
            "{name}:{value}: dates are written 2026-11-09 or 2026/11/09"
        ))
    };
    let parts: Vec<&str> = value.split(['-', '/']).collect();
    let [year, month, day] = parts[..] else {
        return Err(bad());
    };
    let number = |s: &str| -> Result<i64, Invalid> {
        if s.is_empty() || !s.chars().all(|c| c.is_ascii_digit()) || s.len() > 4 {
            return Err(bad());
        }
        Ok(s.parse().unwrap())
    };
    let (year, month, day) = (number(year)?, number(month)?, number(day)?);
    if !(1..=12).contains(&month) || day < 1 || day > days_in_month(year, month) {
        return Err(bad());
    }
    Ok(days_from_civil(year, month, day) * DAY_MS)
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        2 if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's
/// `days_from_civil`).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// The date, `YYYY-MM-DD`, `days` after 1970-01-01: as to-dos are due,
/// so that dates compare as strings.
pub fn date_of(days: i64) -> String {
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let mp = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

/// An age, `3d`, `2w`, `6m` (of 30 days) or `1y` (of 365): in ms.
fn age(name: &str, value: &str) -> Result<i64, Invalid> {
    let bad = || invalid(format!("{name}:{value}: ages are written 3d, 2w, 6m or 1y"));
    let (count, unit) = value.split_at(value.len() - value.chars().last().unwrap().len_utf8());
    let days = match unit {
        "d" => 1,
        "w" => 7,
        "m" => 30,
        "y" => 365,
        _ => return Err(bad()),
    };
    if count.is_empty() || !count.chars().all(|c| c.is_ascii_digit()) {
        return Err(bad());
    }
    let count: i64 = count.parse().map_err(|_| bad())?;
    count.checked_mul(days * DAY_MS).ok_or_else(bad)
}

/// A size, in bytes or with K, M or G (of 1024).
fn size(name: &str, value: &str) -> Result<u64, Invalid> {
    let bad = || {
        invalid(format!(
            "{name}:{value}: sizes are written 500000, 500K, 10M or 1G"
        ))
    };
    let (count, unit) = match value.char_indices().last() {
        Some((at, c)) if c.is_ascii_alphabetic() => (&value[..at], c.to_ascii_uppercase()),
        _ => (value, 'B'),
    };
    let unit: u64 = match unit {
        'B' => 1,
        'K' => 1 << 10,
        'M' => 1 << 20,
        'G' => 1 << 30,
        _ => return Err(bad()),
    };
    if count.is_empty() || !count.chars().all(|c| c.is_ascii_digit()) {
        return Err(bad());
    }
    let count: u64 = count.parse().map_err(|_| bad())?;
    count.checked_mul(unit).ok_or_else(bad)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(text: &str) -> Query {
        Query::Term(Term::Text {
            fields: None,
            text: text.into(),
            exact: false,
        })
    }

    fn label(name: &str) -> Query {
        Query::Term(Term::Label(name.into()))
    }

    fn not(query: Query) -> Query {
        Query::Not(Box::new(query))
    }

    fn ok(query: &str) -> Query {
        parse(query, 0).unwrap_or_else(|e| panic!("{query}: {e}"))
    }

    fn err(query: &str) -> String {
        parse(query, 0).unwrap_err().0
    }

    #[test]
    fn words_side_by_side_must_all_match() {
        assert_eq!(ok(""), Query::And(vec![]));
        assert_eq!(ok("  "), Query::And(vec![]));
        assert_eq!(ok("a"), text("a"));
        assert_eq!(
            ok("a b AND c"),
            Query::And(vec![text("a"), text("b"), text("c")])
        );
        // Lowercase keywords, and what looks like FTS5 syntax, are words.
        assert_eq!(
            ok("a or NEAR b*"),
            Query::And(vec![text("a"), text("or"), text("NEAR"), text("b*")])
        );
    }

    #[test]
    fn or_not_and_groups() {
        assert_eq!(
            ok("a b OR c"),
            Query::Or(vec![Query::And(vec![text("a"), text("b")]), text("c")])
        );
        assert_eq!(
            ok("a (b OR c)"),
            Query::And(vec![text("a"), Query::Or(vec![text("b"), text("c")])])
        );
        assert_eq!(
            ok("-a NOT b"),
            Query::And(vec![not(text("a")), not(text("b"))])
        );
        assert_eq!(ok("-(a b)"), not(Query::And(vec![text("a"), text("b")])));
        assert_eq!(ok("- a"), Query::And(vec![text("-"), text("a")]));
        assert_eq!(ok("a-b"), text("a-b"));
        assert_eq!(ok("NOT NOT a"), not(not(text("a"))));
        for (query, error) in [
            ("(a", "unmatched ("),
            ("a)", "unmatched )"),
            ("()", "() is empty"),
            ("OR a", "OR needs a term before it"),
            ("a OR", "OR needs a term after it"),
            ("a OR OR b", "OR needs a term after it"),
            ("AND a", "AND needs a term on each side"),
            ("a AND", "AND needs a term on each side"),
            ("NOT", "NOT needs a term after it"),
            ("a -)", "unmatched )"),
            ("NOT OR a", "NOT needs a term after it"),
        ] {
            assert_eq!(err(query), error, "{query}");
        }
    }

    #[test]
    fn phrases_are_exact() {
        assert_eq!(
            ok(r#""a b" -"c\"d\\""#),
            Query::And(vec![
                Query::Term(Term::Text {
                    fields: None,
                    text: "a b".into(),
                    exact: true
                }),
                not(Query::Term(Term::Text {
                    fields: None,
                    text: r#"c"d\"#.into(),
                    exact: true
                })),
            ])
        );
        assert_eq!(err(r#""a"#), "unterminated \"");
        assert_eq!(err(r#""\n""#), "in quotes, \\ escapes only \" and \\");
        assert_eq!(err("a\0"), "a query cannot hold NUL");
    }

    #[test]
    fn operators() {
        let from = |text: &str, exact| {
            Query::Term(Term::Text {
                fields: Some(Fields::From),
                text: text.into(),
                exact,
            })
        };
        assert_eq!(ok("from:alice@x.org"), from("alice@x.org", false));
        assert_eq!(ok(r#"FROM:"Alice Smith""#), from("Alice Smith", true));
        assert_eq!(ok("from:me"), Query::Term(Term::Me(Fields::From)));
        assert_eq!(ok("to:ME"), Query::Term(Term::Me(Fields::To)));
        assert_eq!(
            ok(r#"to:"me""#),
            Query::Term(Term::Text {
                fields: Some(Fields::To),
                text: "me".into(),
                exact: true
            })
        );
        assert_eq!(
            ok("subject:me"),
            Query::Term(Term::Text {
                fields: Some(Fields::Subject),
                text: "me".into(),
                exact: false
            })
        );
        assert_eq!(ok("-label:work"), not(label("work")));
        assert_eq!(ok(r#"tag:"Project X""#), label("Project X"));
        assert_eq!(ok("in:Inbox"), label("Inbox"));
        assert_eq!(ok("in:drafts"), label("DRAFT"));
        assert_eq!(ok("in:anywhere"), Query::Term(Term::Anywhere));
        assert_eq!(ok("is:unread"), Query::Term(Term::Unread));
        assert_eq!(ok("is:read"), not(Query::Term(Term::Unread)));
        assert_eq!(ok("category:social"), label("CATEGORY_SOCIAL"));
        assert_eq!(ok("has:attachment"), Query::Term(Term::HasAttachment));
        assert_eq!(
            ok("account:a@x.org"),
            Query::Term(Term::Account("a@x.org".into()))
        );
        assert_eq!(ok("id:<1@x>"), Query::Term(Term::MessageId("1@x".into())));
        assert_eq!(
            ok("rfc822msgid:1@x"),
            Query::Term(Term::MessageId("1@x".into()))
        );
        // Not operators: a colon after other than letters.
        assert_eq!(ok("10:30"), text("10:30"));
        assert_eq!(ok(":a"), text(":a"));
        assert!(err("re:hello").starts_with("unknown operator re: (quote \"re:hello\""));
        assert_eq!(err("from:"), "from: needs a value");
        assert_eq!(err("from: alice"), "from: needs a value");
        assert!(err("is:muted").starts_with("is:muted: is: takes unread, read,"));
        assert_eq!(err("has:drive"), "has:drive: has: takes attachment or due");
    }

    #[test]
    fn operators_of_rooms_todos_and_events() {
        let term = |t| Query::Term(t);
        assert_eq!(ok("is:room"), term(Term::Kind(Kind::Room)));
        assert_eq!(ok("is:TODO"), term(Term::Kind(Kind::Todo)));
        assert_eq!(ok("is:event"), term(Term::Kind(Kind::Event)));
        assert_eq!(ok("is:mail"), term(Term::Kind(Kind::Mail)));
        assert_eq!(ok("is:mention"), term(Term::Mention));
        assert_eq!(ok("is:notifying"), term(Term::Notifying));
        assert_eq!(ok("is:dm"), term(Term::Direct));
        assert_eq!(ok("is:invite"), term(Term::Invited));
        assert_eq!(ok("is:overdue"), term(Term::Overdue));
        assert_eq!(ok("is:done"), term(Term::Done));
        assert_eq!(ok("has:due"), term(Term::HasDue));
        assert_eq!(
            ok(r#"space:"Work stuff""#),
            term(Term::Space("Work stuff".into()))
        );
        assert_eq!(ok("due:today"), term(Term::Due(Day::Today)));
        assert_eq!(ok("due:Tomorrow"), term(Term::Due(Day::Tomorrow)));
        assert_eq!(ok("due:1970-01-03"), term(Term::Due(Day::Date(2))));
        assert!(err("due:soon").starts_with("due:soon: dates are written"));
        assert_eq!(Day::Tomorrow.days(10), 11);
        assert_eq!(days("1970-01-02"), Some(1));
        assert_eq!(days("2026-02-30"), None);
        assert_eq!(days("2026-2-3"), None);
        for date in [
            "1970-01-01",
            "2026-11-11",
            "2024-02-29",
            "2000-03-01",
            "1969-12-31",
        ] {
            assert_eq!(date_of(days(date).unwrap()), date);
        }
    }

    #[test]
    fn text_matches_words_as_fts5_does() {
        assert!(text_matches("Lunch on Friday", "fri", false));
        assert!(!text_matches("Lunch on Friday", "fri", true));
        assert!(text_matches("Lunch on Friday", "LUNCH ON", false));
        assert!(text_matches("Lunch on Friday", "\"lunch on\"", true));
        assert!(!text_matches("Lunch on Friday", "on lunch", false));
        assert!(text_matches("mail alice@x.org now", "alice@x.org", true));
        assert!(!text_matches("anything", "...", false));
    }

    #[test]
    fn queries_match_items_by_their_terms() {
        let unread = |query: &str, unread: bool| {
            matches(&ok(query), &|t| match t {
                Term::Unread => unread,
                _ => false,
            })
        };
        assert!(unread("is:unread", true));
        assert!(!unread("is:unread", false));
        // A term the item lacks is false, its negation true.
        assert!(!unread("is:unread from:alice", true));
        assert!(unread("is:unread -from:alice", true));
        assert!(unread("from:alice OR is:unread", true));
        let done = |query: &str| names(&ok(query), &|t| *t == Term::Done);
        assert!(done("is:todo (is:done OR x)"));
        assert!(!done("-is:done"));
        assert!(done("-(-is:done)"));
    }

    #[test]
    fn dates_ages_and_sizes() {
        let day = 86_400_000;
        assert_eq!(ok("after:1970-01-02"), Query::Term(Term::After(day)));
        assert_eq!(
            ok("before:2026/11/9"),
            Query::Term(Term::Before(20_766 * day))
        );
        assert_eq!(
            ok("after:2024-02-29"),
            Query::Term(Term::After(19_782 * day))
        );
        assert_eq!(ok("after:1969-12-31"), Query::Term(Term::After(-day)));
        for bad in [
            "2023-02-29",
            "2026-13-01",
            "2026-11",
            "2026-11-0",
            "x",
            "2026-11-09-1",
        ] {
            assert_eq!(
                err(&format!("after:{bad}")),
                format!("after:{bad}: dates are written 2026-11-09 or 2026/11/09")
            );
        }
        let now = 100 * day;
        assert_eq!(
            parse("newer_than:3d", now).unwrap(),
            Query::Term(Term::After(97 * day))
        );
        assert_eq!(
            parse("older_than:2w", now).unwrap(),
            Query::Term(Term::Before(86 * day))
        );
        assert_eq!(
            parse("newer_than:1m", now).unwrap(),
            Query::Term(Term::After(70 * day))
        );
        assert_eq!(
            parse("older_than:1y", now).unwrap(),
            Query::Term(Term::Before(-265 * day))
        );
        for bad in ["d", "3", "3h", "-3d", "99999999999999999d"] {
            assert_eq!(
                err(&format!("newer_than:{bad}")),
                format!("newer_than:{bad}: ages are written 3d, 2w, 6m or 1y")
            );
        }
        assert_eq!(ok("larger:500"), Query::Term(Term::Larger(500)));
        assert_eq!(ok("larger:10M"), Query::Term(Term::Larger(10 << 20)));
        assert_eq!(ok("smaller:2k"), Query::Term(Term::Smaller(2 << 10)));
        for bad in ["M", "10X", "1.5M", "99999999999G"] {
            assert_eq!(
                err(&format!("larger:{bad}")),
                format!("larger:{bad}: sizes are written 500000, 500K, 10M or 1G")
            );
        }
    }
}
