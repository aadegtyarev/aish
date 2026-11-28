//! The one shell lexer: the command line split the way bash reads it (quotes,
//! escapes, operators, redirections), with the facts both the allowlist and
//! the hard list need. It never runs anything and fails closed: `None` for
//! what it cannot read (command substitution, an unterminated quote ...).

/// A word with its quoting facts.
#[derive(Debug, Clone)]
pub struct Word {
    pub text: String,
    /// `$` or a brace outside single quotes: the real value is not known.
    pub expands: bool,
    /// Unquoted `*?[`.
    pub glob: bool,
    /// Starts with an unquoted `~`.
    pub tilde: bool,
}

pub enum Tok {
    Word(Word),
    /// `|`, or `|&` (`amp`)
    Pipe {
        amp: bool,
    },
    /// `;` `&&` `||` `&` newline, grouping
    Sep,
    /// `>` `>>` `>|` `&>` `<>`; `dup` for `>&`; `fd` is the number right
    /// before it (`2>&1`); `clean`: `>&` is followed at once by a number
    /// and then a blank or the end.
    Out {
        dup: bool,
        fd: Option<String>,
        clean: bool,
    },
    /// `<` `<<` `<<<` `<&`: the next word is read, never written
    In,
}

/// Split into tokens; `None` = not understood (command substitution, an
/// unterminated quote, a function definition ...). The flag is set when bash
/// reads something a plain split would not: a comment, a line continuation, `\r`.
pub fn lex(s: &str) -> Option<(Vec<Tok>, bool)> {
    let cs: Vec<char> = s.chars().collect();
    let mut toks = Vec::new();
    let mut cur: Option<Word> = None;
    let mut odd = false;
    let mut i = 0;
    fn flush(cur: &mut Option<Word>, toks: &mut Vec<Tok>) {
        if let Some(w) = cur.take() {
            toks.push(Tok::Word(w));
        }
    }
    fn word(cur: &mut Option<Word>) -> &mut Word {
        cur.get_or_insert(Word {
            text: String::new(),
            expands: false,
            glob: false,
            tilde: false,
        })
    }
    while i < cs.len() {
        let c = cs[i];
        let next = cs.get(i + 1).copied();
        match c {
            '\'' => {
                let w = word(&mut cur);
                i += 1;
                loop {
                    match *cs.get(i)? {
                        '\'' => break,
                        x => w.text.push(x),
                    }
                    i += 1;
                }
            }
            '"' => {
                let w = word(&mut cur);
                i += 1;
                loop {
                    match *cs.get(i)? {
                        '"' => break,
                        '`' => return None,
                        '$' => {
                            if cs.get(i + 1) == Some(&'(') {
                                return None;
                            }
                            w.expands = true;
                            w.text.push('$');
                        }
                        '\\' => {
                            i += 1;
                            match *cs.get(i)? {
                                x @ ('"' | '\\' | '$' | '`') => w.text.push(x),
                                '\n' => odd = true,
                                x => {
                                    w.text.push('\\');
                                    w.text.push(x);
                                }
                            }
                        }
                        x => w.text.push(x),
                    }
                    i += 1;
                }
            }
            '\\' => {
                i += 1;
                match *cs.get(i)? {
                    '\n' => odd = true,
                    x => word(&mut cur).text.push(x),
                }
            }
            '`' => return None,
            '$' => {
                if next == Some('(') {
                    return None;
                }
                let w = word(&mut cur);
                w.expands = true;
                w.text.push('$');
            }
            ' ' | '\t' | '\r' => {
                odd |= c == '\r';
                flush(&mut cur, &mut toks);
            }
            '\n' | ';' => {
                flush(&mut cur, &mut toks);
                toks.push(Tok::Sep);
            }
            '#' if cur.is_none() => {
                odd = true;
                while i < cs.len() && cs[i] != '\n' {
                    i += 1;
                }
                continue;
            }
            '(' => {
                // `name()` is a function definition: not understood
                if cur.is_some() && cs[i + 1..].iter().find(|c| !c.is_whitespace()) == Some(&')') {
                    return None;
                }
                flush(&mut cur, &mut toks);
                toks.push(Tok::Sep);
            }
            ')' => {
                flush(&mut cur, &mut toks);
                toks.push(Tok::Sep);
            }
            '{' | '}' if cur.is_none() && next.is_none_or(|n| n.is_whitespace() || n == ';') => {
                toks.push(Tok::Sep);
            }
            // brace expansion: the real words are not known
            '{' | '}' => {
                let w = word(&mut cur);
                w.expands = true;
                w.text.push(c);
            }
            '|' => {
                flush(&mut cur, &mut toks);
                match next {
                    Some('|') => {
                        i += 1;
                        toks.push(Tok::Sep);
                    }
                    Some('&') => {
                        i += 1;
                        toks.push(Tok::Pipe { amp: true });
                    }
                    _ => toks.push(Tok::Pipe { amp: false }),
                }
            }
            '&' => {
                flush(&mut cur, &mut toks);
                match next {
                    Some('&') => {
                        i += 1;
                        toks.push(Tok::Sep);
                    }
                    Some('>') => {
                        i += 1;
                        if cs.get(i + 1) == Some(&'>') {
                            i += 1;
                        }
                        toks.push(Tok::Out {
                            dup: false,
                            fd: None,
                            clean: false,
                        });
                    }
                    _ => toks.push(Tok::Sep),
                }
            }
            '>' | '<' => {
                if next == Some('(') {
                    return None;
                }
                // a number right before is the file descriptor, not a word
                let mut fd = None;
                match &cur {
                    Some(w) if !w.expands && w.text.chars().all(|d| d.is_ascii_digit()) => {
                        fd = cur.take().map(|w| w.text);
                    }
                    _ => flush(&mut cur, &mut toks),
                }
                if c == '>' {
                    let (mut dup, mut clean) = (false, false);
                    match next {
                        Some('>' | '|') => i += 1,
                        Some('&') => {
                            i += 1;
                            dup = true;
                            let n = cs[i + 1..]
                                .iter()
                                .take_while(|d| d.is_ascii_digit())
                                .count();
                            clean = n > 0 && cs.get(i + 1 + n).is_none_or(|x| x.is_whitespace());
                        }
                        _ => {}
                    }
                    toks.push(Tok::Out { dup, fd, clean });
                } else {
                    match next {
                        Some('>') => {
                            i += 1;
                            toks.push(Tok::Out {
                                dup: false,
                                fd,
                                clean: false,
                            });
                        }
                        Some('<') => {
                            i += 1;
                            if cs.get(i + 1) == Some(&'<') || cs.get(i + 1) == Some(&'-') {
                                i += 1;
                            }
                            toks.push(Tok::In);
                        }
                        Some('&') => {
                            i += 1;
                            toks.push(Tok::In);
                        }
                        _ => toks.push(Tok::In),
                    }
                }
            }
            '*' | '?' | '[' => {
                let w = word(&mut cur);
                w.glob = true;
                w.text.push(c);
            }
            '~' if cur.is_none() => {
                let w = word(&mut cur);
                w.tilde = true;
                w.text.push(c);
            }
            x => word(&mut cur).text.push(x),
        }
        i += 1;
    }
    flush(&mut cur, &mut toks);
    Some((toks, odd))
}
