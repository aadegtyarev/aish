//! Targeted text edits of the YAML config: the model of a role and the active
//! profile. Only the touched value changes; comments, key lines, ordering and
//! everything else stay byte for byte. Every function returns `None` when the
//! file does not have the expected shape (the caller then applies the change
//! for the session only).

use std::path::Path;

fn indent(l: &str) -> usize {
    l.len() - l.trim_start_matches(' ').len()
}

fn is_skip(l: &str) -> bool {
    let t = l.trim();
    t.is_empty() || t.starts_with('#')
}

fn body(l: &str) -> &str {
    l.trim_end_matches(['\n', '\r'])
}

/// Text after `key:` when the line is exactly that key, else None.
fn key_rest<'a>(l: &'a str, key: &str) -> Option<&'a str> {
    let r = body(l).trim_start().strip_prefix(key)?.strip_prefix(':')?;
    (r.is_empty() || r.starts_with([' ', '\t'])).then_some(r)
}

/// Child lines `[a, b)` of the key line `i` (deeper than it).
fn region(lines: &[String], i: usize) -> (usize, usize) {
    let k = indent(&lines[i]);
    let end = (i + 1..lines.len())
        .find(|&j| !is_skip(&lines[j]) && indent(&lines[j]) <= k)
        .unwrap_or(lines.len());
    (i + 1, end)
}

/// Where the value of `rest` (text after the colon) starts and ends: before the
/// trailing whitespace and `# comment`.
fn value_span(rest: &str) -> (usize, usize) {
    let lead = rest.len() - rest.trim_start().len();
    let (mut q, mut prev_ws, mut cut) = (None::<char>, true, rest.len());
    for (i, c) in rest.char_indices().skip_while(|(i, _)| *i < lead) {
        match q {
            Some(qc) => {
                if c == qc {
                    q = None;
                }
            }
            None if c == '"' || c == '\'' => q = Some(c),
            None if c == '#' && prev_ws => {
                cut = i;
                break;
            }
            None => {}
        }
        prev_ws = c.is_whitespace();
    }
    (lead, rest[..cut].trim_end().len().max(lead))
}

fn replace_value(rest: &str, new: &str) -> String {
    let (a, b) = value_span(rest);
    let lead = if a == 0 { " " } else { &rest[..a] };
    format!("{lead}{new}{}", &rest[b..])
}

/// Split the inside `[a, b)` of a flow collection at top-level commas.
fn segments(s: &str, a: usize, b: usize) -> Vec<(usize, usize)> {
    let (mut v, mut start, mut depth, mut q) = (Vec::new(), a, 0i32, None::<char>);
    for (i, c) in s[a..b].char_indices().map(|(i, c)| (i + a, c)) {
        match q {
            Some(qc) => {
                if c == qc {
                    q = None;
                }
            }
            None => match c {
                '"' | '\'' => q = Some(c),
                '[' | '{' => depth += 1,
                ']' | '}' => depth -= 1,
                ',' if depth == 0 => {
                    v.push((start, i));
                    start = i + 1;
                }
                _ => {}
            },
        }
    }
    v.push((start, b));
    v
}

/// Inside of a flow map `{...}` that spans all of `s` (after trimming).
fn flow_inner(s: &str) -> Option<(usize, usize)> {
    let a = s.len() - s.trim_start().len();
    let t = s.trim_end();
    (t[a..].starts_with('{') && t.ends_with('}') && t.len() > a + 1).then_some((a + 1, t.len() - 1))
}

/// Value range of `key` in the flow map `s`.
fn flow_get(s: &str, key: &str) -> Option<(usize, usize)> {
    let (a, b) = flow_inner(s)?;
    for (x, y) in segments(s, a, b) {
        let seg = &s[x..y];
        let lead = seg.len() - seg.trim_start().len();
        let Some(r) = seg[lead..].strip_prefix(key) else {
            continue;
        };
        let r2 = r.trim_start();
        if let Some(v) = r2.strip_prefix(':') {
            let vs = y - v.trim_start().len();
            return Some((vs, x + seg.trim_end().len()));
        }
    }
    None
}

/// `{id: x, ...}` keeps its other keys; anything else becomes `new`.
fn set_spec(spec: &str, new: &str) -> Option<String> {
    if spec.trim_start().starts_with('{') {
        let (a, b) = flow_get(spec, "id")?;
        Some(format!("{}{new}{}", &spec[..a], &spec[b..]))
    } else if spec.trim_start().starts_with('[') && !spec.trim_end().ends_with(']') {
        None
    } else {
        Some(new.to_owned())
    }
}

fn value_text(refs: &[String]) -> String {
    match refs {
        [one] => one.clone(),
        many => format!("[{}]", many.join(", ")),
    }
}

fn find_top(lines: &[String], key: &str) -> Option<usize> {
    lines
        .iter()
        .position(|l| indent(l) == 0 && key_rest(l, key).is_some())
}

fn split(src: &str) -> Vec<String> {
    src.split_inclusive('\n').map(str::to_owned).collect()
}

fn join(lines: &[String]) -> String {
    lines.concat()
}

fn push_line(lines: &mut Vec<String>, at: usize, text: String) {
    if at > 0 && !lines[at - 1].ends_with('\n') {
        lines[at - 1].push('\n');
    }
    lines.insert(at, format!("{text}\n"));
}

fn rewrite(lines: &mut [String], j: usize, f: impl FnOnce(&str) -> Option<String>) -> Option<()> {
    let eol = lines[j][body(&lines[j]).len()..].to_owned();
    let new = f(body(&lines[j]))?;
    lines[j] = format!("{new}{eol}");
    Some(())
}

/// Set the role `role` inside the block mapping that starts under key line `i`.
fn edit_mapping(
    lines: &mut Vec<String>,
    i: usize,
    role: &str,
    new: &str,
    wrap: bool,
) -> Option<()> {
    let (a, b) = region(lines, i);
    let c = (a..b)
        .find(|&j| !is_skip(&lines[j]))
        .map(|j| indent(&lines[j]))?;
    let hit = (a..b).find(|&j| indent(&lines[j]) == c && key_rest(&lines[j], role).is_some());
    let Some(j) = hit else {
        let last = (a..b).rev().find(|&j| !is_skip(&lines[j]))? + 1;
        let v = if wrap {
            format!("{{id: {new}}}")
        } else {
            new.to_owned()
        };
        push_line(lines, last, format!("{}{role}: {v}", " ".repeat(c)));
        return Some(());
    };
    let rest = key_rest(&lines[j], role)?.to_owned();
    let colon = body(&lines[j]).len() - rest.len();
    let (vs, ve) = value_span(&rest);
    if vs == ve {
        // block mapping: `id:` below
        let (a2, b2) = region(lines, j);
        let k = (a2..b2).find(|&k| key_rest(&lines[k], "id").is_some())?;
        let r2 = key_rest(&lines[k], "id")?.to_owned();
        let (s2, e2) = value_span(&r2);
        if s2 == e2 {
            return None; // block list
        }
        let c2 = body(&lines[k]).len() - r2.len();
        return rewrite(lines, k, |l| {
            Some(format!(
                "{}{}",
                &l[..c2],
                replace_value(&r2, &spec_for(&r2[s2..e2], new)?)
            ))
        });
    }
    let spec = rest[vs..ve].to_owned();
    let newspec = spec_for(&spec, new)?;
    rewrite(lines, j, |l| {
        Some(format!("{}{}", &l[..colon], replace_value(&rest, &newspec)))
    })
}

fn spec_for(old: &str, new: &str) -> Option<String> {
    // a plain `id:` value or a role spec: only flow maps keep other keys
    set_spec(old, new)
}

/// Make `refs` the model list of `role` (in the active `profile` if any).
pub fn set_role(src: &str, profile: Option<&str>, role: &str, refs: &[String]) -> Option<String> {
    let new = value_text(refs);
    let mut lines = split(src);
    match profile {
        None => match find_top(&lines, "roles") {
            None => {
                let at = lines.len();
                push_line(&mut lines, at, format!("roles:\n  {role}: {{id: {new}}}"));
            }
            Some(i) => {
                let rest = key_rest(&lines[i], "roles")?;
                let (a, b) = value_span(rest);
                if a != b {
                    return None;
                }
                edit_mapping(&mut lines, i, role, &new, true)?;
            }
        },
        Some(p) => {
            let top = find_top(&lines, "profiles")?;
            let (a, b) = region(&lines, top);
            let c = (a..b)
                .find(|&j| !is_skip(&lines[j]))
                .map(|j| indent(&lines[j]))?;
            let i = (a..b).find(|&j| indent(&lines[j]) == c && key_rest(&lines[j], p).is_some())?;
            let rest = key_rest(&lines[i], p)?.to_owned();
            let colon = body(&lines[i]).len() - rest.len();
            let (vs, ve) = value_span(&rest);
            if vs == ve {
                edit_mapping(&mut lines, i, role, &new, false)?;
            } else {
                let flow = rest[vs..ve].to_owned();
                let (ia, ib) = flow_inner(&flow)?;
                let edited = match flow_get(&flow, role) {
                    Some((x, y)) => format!(
                        "{}{}{}",
                        &flow[..x],
                        set_spec(&flow[x..y], &new)?,
                        &flow[y..]
                    ),
                    None if flow[ia..ib].trim().is_empty() => {
                        format!("{}{role}: {new}{}", &flow[..ia], &flow[ib..])
                    }
                    None => format!("{}, {role}: {new}{}", &flow[..ib].trim_end(), &flow[ib..]),
                };
                rewrite(&mut lines, i, |l| {
                    Some(format!("{}{}", &l[..colon], replace_value(&rest, &edited)))
                })?;
            }
        }
    }
    Some(join(&lines))
}

/// Set the active profile (`profile:`), adding the key when missing.
pub fn set_profile(src: &str, name: &str) -> Option<String> {
    let mut lines = split(src);
    match find_top(&lines, "profile") {
        Some(i) => {
            let rest = key_rest(&lines[i], "profile")?.to_owned();
            let (a, b) = value_span(&rest);
            if a == b {
                return None;
            }
            let colon = body(&lines[i]).len() - rest.len();
            rewrite(&mut lines, i, |l| {
                Some(format!("{}{}", &l[..colon], replace_value(&rest, name)))
            })?;
        }
        None => {
            let at = lines.len();
            push_line(&mut lines, at, format!("profile: {name}"));
        }
    }
    Some(join(&lines))
}

/// Replace the file atomically (temp + rename), keeping its mode.
pub fn write_atomic(path: &Path, content: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let mode = std::fs::metadata(path)
        .map(|m| m.permissions().mode() & 0o7777)
        .unwrap_or(0o600);
    let tmp = path.with_file_name(format!(
        ".{}.tmp{}",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("config"),
        std::process::id()
    ));
    let r = (|| {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(content.as_bytes())?;
        f.set_permissions(std::fs::Permissions::from_mode(mode))?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if r.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| (*s).to_owned()).collect()
    }

    const BLOCK: &str = "# top comment\nproviders: []   # none\n\nroles:\n  # the roles\n  complete:\n    id: local/coder   # small\n  chat:\n    id: cloud/main\n    effort: low\n\nkeys: {ask: ctrl-g}\n";

    #[test]
    fn block_role_keeps_everything_else() {
        let out = set_role(BLOCK, None, "chat", &v(&["ds"])).unwrap();
        assert_eq!(out, BLOCK.replace("id: cloud/main", "id: ds"));
        let out = set_role(BLOCK, None, "complete", &v(&["omni"])).unwrap();
        assert_eq!(out, BLOCK.replace("id: local/coder ", "id: omni "));
    }

    #[test]
    fn list_and_flow_forms() {
        let out = set_role(BLOCK, None, "chat", &v(&["ds", "mellum"])).unwrap();
        assert!(
            out.contains("    id: [ds, mellum]\n    effort: low\n"),
            "{out}"
        );
        let flow = "roles:\n  chat: {id: mock/m, effort: low}  # c\n  complete: {id: [a, b]}\n";
        let out = set_role(flow, None, "chat", &v(&["x"])).unwrap();
        assert_eq!(out, flow.replace("mock/m", "x"));
        let out = set_role(flow, None, "complete", &v(&["z"])).unwrap();
        assert_eq!(out, flow.replace("[a, b]", "z"));
        let plain = "roles:\n  chat: mock/m # k\n";
        assert_eq!(
            set_role(plain, None, "chat", &v(&["a", "b"])).unwrap(),
            "roles:\n  chat: [a, b] # k\n"
        );
    }

    #[test]
    fn missing_role_or_section_is_added() {
        let src = "roles:\n  chat: {id: a}\n\n# tail\n";
        assert_eq!(
            set_role(src, None, "complete", &v(&["b"])).unwrap(),
            "roles:\n  chat: {id: a}\n  complete: {id: b}\n\n# tail\n"
        );
        assert_eq!(
            set_role("providers: []", None, "chat", &v(&["a"])).unwrap(),
            "providers: []\nroles:\n  chat: {id: a}\n"
        );
    }

    #[test]
    fn unexpected_shapes_are_refused() {
        assert!(set_role("roles: {chat: {id: a}}\n", None, "chat", &v(&["b"])).is_none());
        assert!(set_role(
            "roles:\n  chat:\n    id:\n      - a\n",
            None,
            "chat",
            &v(&["b"])
        )
        .is_none());
        assert!(set_role("roles:\n", None, "chat", &v(&["b"])).is_none());
        assert!(set_role("roles:\n  chat: {effort: low}\n", None, "chat", &v(&["b"])).is_none());
        assert!(set_role("x: 1\n", Some("p"), "chat", &v(&["b"])).is_none());
    }

    const PROF: &str = "roles:\n  chat: {id: a}\nprofiles:\n  local: {chat: [mellum], complete: omni}\n  inet:\n    chat: [ds, mellum]  # c\n    complete: omni\n  bare: {}\nprofile: local  # active\n";

    #[test]
    fn profile_entries() {
        let out = set_role(PROF, Some("local"), "chat", &v(&["ds", "mellum"])).unwrap();
        assert_eq!(out, PROF.replace("chat: [mellum]", "chat: [ds, mellum]"));
        let out = set_role(PROF, Some("inet"), "chat", &v(&["mellum"])).unwrap();
        assert_eq!(
            out,
            PROF.replace("chat: [ds, mellum]  #", "chat: mellum  #")
        );
        let out = set_role(PROF, Some("inet"), "extra", &v(&["q"])).unwrap();
        assert!(
            out.contains("    complete: omni\n    extra: q\n  bare"),
            "{out}"
        );
        let out = set_role(PROF, Some("local"), "extra", &v(&["q"])).unwrap();
        assert!(out.contains("complete: omni, extra: q}"), "{out}");
        let out = set_role(PROF, Some("bare"), "chat", &v(&["q"])).unwrap();
        assert!(out.contains("bare: {chat: q}"), "{out}");
        assert!(set_role(PROF, Some("nope"), "chat", &v(&["q"])).is_none());
    }

    #[test]
    fn profile_switch_and_add() {
        let out = set_profile(PROF, "inet").unwrap();
        assert_eq!(out, PROF.replace("profile: local ", "profile: inet "));
        let src = "profiles:\n  a: {chat: x}\n";
        assert_eq!(
            set_profile(src, "a").unwrap(),
            "profiles:\n  a: {chat: x}\nprofile: a\n"
        );
        // `profiles:` is not `profile:`
        assert!(find_top(&split(src), "profile").is_none());
        assert_eq!(set_profile("a: 1", "p").unwrap(), "a: 1\nprofile: p\n");
    }

    #[test]
    fn atomic_write_keeps_mode() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::TempDir::new().unwrap();
        let p = d.path().join("config.yaml");
        std::fs::write(&p, "a: 1\n").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
        write_atomic(&p, "b: 2\n").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "b: 2\n");
        assert_eq!(
            std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(std::fs::read_dir(d.path()).unwrap().count(), 1);
    }
}
