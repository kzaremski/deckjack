//! A net or bus connection: its name, and for a bus, its members.
//!
//! This mirrors KiCad's `SCH_CONNECTION` closely, because net naming is
//! decided by how these objects are configured from labels, cloned from one
//! subgraph to another and prefixed with sheet paths. The rules that matter:
//!
//! * A name is `prefix + name + suffix`. The prefix is set only on members of
//!   a named bus group (`USB.` in `USB{DP DN}`); the suffix only when a weakly
//!   driven name collides and is renamed (`_1`).
//! * The full name has the sheet path (`/sub/`) in front, unless the driver
//!   is a global label, an ordinary pin, or a global power pin. A local power
//!   pin keeps the path.
//! * Cloning copies the name, sheet and driver, but not the vector index and
//!   not the local name once one is set; that is what lets a bus member be
//!   matched by position after its bus was renamed from above.

use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    None,
    Net,
    /// A vector bus, `D[0..7]`.
    Bus,
    /// A bus group, `USB{DP DN}`.
    Group,
}

/// What drives a connection, as far as naming cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriverKind {
    GlobalLabel,
    Pin { local_power: bool },
    Other,
}

#[derive(Debug, Clone)]
pub struct Conn {
    pub kind: Kind,
    pub name: String,
    pub local_name: String,
    pub local_prefix: String,
    pub prefix: String,
    pub bus_prefix: String,
    pub suffix: String,
    pub vector_prefix: String,
    pub members: Vec<Conn>,
    pub vector_index: i64,
    /// Sheet instance whose path prefixes the name.
    pub sheet: usize,
    pub driver: Option<DriverKind>,
}

/// Bus aliases by name.
pub type Aliases = BTreeMap<String, Vec<String>>;

/// At most this many connections below one label, across its groups, vectors
/// and aliases. Far past any real bus; a label over it is cut off.
pub const MAX_LABEL_MEMBERS: usize = 8192;

/// The state of one label's expansion.
struct Expansion<'a> {
    aliases: &'a Aliases,
    /// Aliases being expanded, outermost first.
    stack: Vec<String>,
    /// Connections still allowed below the label.
    budget: usize,
    complete: bool,
}

impl Conn {
    pub fn new(sheet: usize) -> Conn {
        Conn {
            kind: Kind::None,
            name: String::new(),
            local_name: String::new(),
            local_prefix: String::new(),
            prefix: String::new(),
            bus_prefix: String::new(),
            suffix: String::new(),
            vector_prefix: String::new(),
            members: Vec::new(),
            vector_index: 0,
            sheet,
            driver: None,
        }
    }

    pub fn is_bus(&self) -> bool {
        matches!(self.kind, Kind::Bus | Kind::Group)
    }
    pub fn is_net(&self) -> bool {
        self.kind == Kind::Net
    }

    fn base_name(&self) -> String {
        if self.name.is_empty() {
            "<NO NET>".to_string()
        } else {
            format!("{}{}{}", self.prefix, self.name, self.suffix)
        }
    }

    fn prepend_path(&self) -> bool {
        if self.kind == Kind::None {
            return false;
        }
        match self.driver {
            Some(DriverKind::GlobalLabel) => false,
            Some(DriverKind::Pin { local_power }) => local_power,
            _ => true,
        }
    }

    /// `Name(aIgnoreSheet)`: the full name, or without the sheet path.
    pub fn full_name(&self, paths: &[String]) -> String {
        if self.prepend_path() {
            format!(
                "{}{}",
                paths.get(self.sheet).map(String::as_str).unwrap_or("/"),
                self.base_name()
            )
        } else {
            self.base_name()
        }
    }

    pub fn short_name(&self) -> String {
        self.base_name()
    }

    pub fn full_local_name(&self) -> String {
        format!("{}{}{}", self.local_prefix, self.local_name, self.suffix)
    }

    pub fn set_driver(&mut self, d: Option<DriverKind>) {
        self.driver = d;
        for m in &mut self.members {
            m.set_driver(d);
        }
    }

    pub fn set_prefix(&mut self, prefix: &str) {
        self.prefix = prefix.to_string();
        for m in &mut self.members {
            m.set_prefix(prefix);
        }
    }

    /// `ConfigureFromLabel`. Returns `false` when the expansion was cut
    /// short: a bus alias that refers to itself, nesting deeper than 8
    /// levels, or more than [`MAX_LABEL_MEMBERS`] members in all. KiCad has
    /// no such limits (a self-referencing alias overflows its stack), so a
    /// label cut short here is one KiCad cannot load either.
    pub fn configure_from_label(&mut self, label: &str, aliases: &Aliases) -> bool {
        let mut ex = Expansion {
            aliases,
            stack: Vec::new(),
            budget: MAX_LABEL_MEMBERS,
            complete: true,
        };
        self.configure_inner(label, &mut ex, 0);
        ex.complete
    }

    /// Every connection in this tree, itself included.
    pub fn tree_size(&self) -> usize {
        1 + self.members.iter().map(Conn::tree_size).sum::<usize>()
    }

    fn configure_inner(&mut self, label: &str, ex: &mut Expansion, depth: usize) {
        self.members.clear();
        self.name = label.to_string();
        self.local_name = label.to_string();
        self.local_prefix = self.prefix.clone();
        let unescaped = unescape(label);
        if let Some((prefix, members)) = parse_bus_vector(&unescaped) {
            self.kind = Kind::Bus;
            self.vector_prefix = prefix;
            if members.len() > ex.budget {
                ex.complete = false;
            }
            let take = members.len().min(ex.budget);
            ex.budget -= take;
            for (i, m) in members.into_iter().take(take).enumerate() {
                let mut c = Conn::new(self.sheet);
                c.kind = Kind::Net;
                c.prefix = self.prefix.clone();
                c.local_name = m.clone();
                c.local_prefix = self.prefix.clone();
                c.vector_index = i as i64;
                c.name = m;
                self.members.push(c);
            }
        } else if let Some((prefix, members)) = parse_bus_group(&unescaped) {
            self.kind = Kind::Group;
            self.bus_prefix = prefix.clone();
            let member_prefix = if prefix.is_empty() {
                String::new()
            } else {
                format!("{prefix}.")
            };
            // A nested group or alias deeper than this is malformed or
            // hostile; stop expanding rather than recurse without end.
            if depth > 8 {
                ex.complete = false;
                return;
            }
            for gm in members {
                if ex.budget == 0 {
                    ex.complete = false;
                    break;
                }
                // An alias already being expanded refers to itself, directly
                // or through another alias: its name stays a plain member.
                let alias = if ex.stack.contains(&gm) {
                    ex.complete = false;
                    None
                } else {
                    ex.aliases.get(&gm)
                };
                let expanded: Vec<String> = match alias {
                    Some(alias) => alias.iter().map(|am| escape_netname(am)).collect(),
                    None => vec![gm.clone()],
                };
                if alias.is_some() {
                    ex.stack.push(gm.clone());
                }
                for name in expanded {
                    if ex.budget == 0 {
                        ex.complete = false;
                        break;
                    }
                    ex.budget -= 1;
                    let mut c = Conn::new(self.sheet);
                    c.set_prefix(&member_prefix);
                    c.configure_inner(&name, ex, depth + 1);
                    self.members.push(c);
                }
                if alias.is_some() {
                    ex.stack.pop();
                }
            }
        } else {
            self.kind = Kind::Net;
        }
    }

    /// `Clone`: become a copy of `other`, keeping this object's vector index,
    /// and its local name if it has one.
    pub fn clone_from_conn(&mut self, other: &Conn) {
        self.driver = other.driver;
        self.sheet = other.sheet;
        self.name = other.name.clone();
        let orig = self.kind;
        self.kind = other.kind;
        if self.local_name.is_empty() {
            self.local_name = other.local_name.clone();
            self.local_prefix = other.prefix.clone();
        }
        self.prefix = other.prefix.clone();
        self.suffix = other.suffix.clone();
        self.vector_prefix = other.vector_prefix.clone();
        let clone_member = |src: &Conn, sheet: usize| {
            let mut c = Conn::new(sheet);
            c.clone_from_conn(src);
            c.vector_index = src.vector_index;
            c
        };
        if orig == Kind::Bus && other.kind == Kind::Bus {
            if self.members.is_empty() {
                self.members = other
                    .members
                    .iter()
                    .map(|m| clone_member(m, self.sheet))
                    .collect();
            } else {
                let n = self.members.len().min(other.members.len());
                for i in 0..n {
                    self.members[i].clone_from_conn(&other.members[i]);
                }
            }
        } else if orig == Kind::Group && other.kind == Kind::Group {
            if self.members.is_empty() {
                self.members = other
                    .members
                    .iter()
                    .map(|m| clone_member(m, self.sheet))
                    .collect();
            } else {
                for m in &mut self.members {
                    if let Some(src) = other.members.iter().find(|o| o.local_name == m.local_name) {
                        m.clone_from_conn(src);
                    }
                }
            }
        } else if other.is_bus() {
            self.members = other
                .members
                .iter()
                .map(|m| clone_member(m, self.sheet))
                .collect();
        }
        self.kind = other.kind;
    }

    /// `IsSubsetOf`: every member name of `self` is a member name of `other`.
    pub fn is_subset_of(&self, other: &Conn) -> bool {
        if !other.is_bus() {
            return false;
        }
        let names: std::collections::HashSet<String> =
            other.members.iter().map(Conn::full_local_name).collect();
        if !self.is_bus() {
            return names.contains(&self.full_local_name());
        }
        self.members
            .iter()
            .all(|m| names.contains(&m.full_local_name()))
    }
}

/// `matchBusMember`: the path (indices into `members`, then into a nested
/// vector's members) of the member of `bus` that corresponds to `search`.
pub fn match_bus_member(bus: &Conn, search: &Conn) -> Option<Vec<usize>> {
    match bus.kind {
        Kind::Bus => bus
            .members
            .iter()
            .position(|m| m.vector_index == search.vector_index)
            .map(|i| vec![i]),
        Kind::Group => {
            for (i, c) in bus.members.iter().enumerate() {
                if c.kind == Kind::Bus {
                    if let Some(j) = c
                        .members
                        .iter()
                        .position(|m| m.local_name == search.local_name)
                    {
                        return Some(vec![i, j]);
                    }
                } else if c.local_name == search.local_name {
                    return Some(vec![i]);
                }
            }
            if search.vector_index >= 0 {
                let mut flat = 0;
                for (i, c) in bus.members.iter().enumerate() {
                    if c.kind == Kind::Bus {
                        for j in 0..c.members.len() {
                            if flat == search.vector_index {
                                return Some(vec![i, j]);
                            }
                            flat += 1;
                        }
                    } else {
                        if flat == search.vector_index {
                            return Some(vec![i]);
                        }
                        flat += 1;
                    }
                }
            }
            None
        }
        _ => None,
    }
}

pub fn member_at<'a>(bus: &'a Conn, path: &[usize]) -> Option<&'a Conn> {
    let mut at = bus;
    for &i in path {
        at = at.members.get(i)?;
    }
    Some(at)
}

pub fn member_at_mut<'a>(bus: &'a mut Conn, path: &[usize]) -> Option<&'a mut Conn> {
    let mut at = bus;
    for &i in path {
        at = at.members.get_mut(i)?;
    }
    Some(at)
}

/// KiCad's `EscapeString(..., CTX_NETNAME)`.
pub fn escape_netname(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '/' => out.push_str("{slash}"),
            '\n' | '\r' => {}
            c => out.push(c),
        }
    }
    out
}

/// KiCad's `UnescapeString`: the `{token}` escapes back to characters.
pub fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let token_end = after.find('}');
        let replaced = token_end.and_then(|end| {
            let token = &after[..end];
            let ch = match token {
                "dblquote" => "\"",
                "quote" => "'",
                "lt" => "<",
                "gt" => ">",
                "backslash" => "\\",
                "slash" => "/",
                "bar" => "|",
                "comma" => ",",
                "colon" => ":",
                "space" => " ",
                "dollar" => "$",
                "tab" => "\t",
                "return" => "\n",
                "brace" => "{",
                _ => return None,
            };
            Some((ch, end))
        });
        match replaced {
            Some((ch, end)) => {
                out.push_str(ch);
                rest = &after[end + 1..];
            }
            None => {
                out.push('{');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

fn is_fmt(c: char) -> bool {
    matches!(c, '_' | '^' | '~')
}

/// `NET_SETTINGS::ParseBusVector`, without the quoted-name extension.
pub fn parse_bus_vector(bus: &str) -> Option<(String, Vec<String>)> {
    let chars: Vec<char> = bus.chars().collect();
    let mut i = 0;
    let mut prefix = String::new();
    let mut suffix = String::new();
    let mut nesting: i32 = 0;
    let mut fmt_wraps_name = false;
    while i < chars.len() {
        let c = chars[i];
        if c == '{' {
            if i > 0 && is_fmt(chars[i - 1]) {
                nesting += 1;
                prefix.push('{');
                i += 1;
                continue;
            }
            return None;
        } else if c == '}' {
            nesting -= 1;
            prefix.push('}');
            i += 1;
            continue;
        }
        if c == '\\' && chars.get(i + 1) == Some(&' ') {
            prefix.push(' ');
            i += 2;
            continue;
        }
        if c == ' ' || c == ']' {
            return None;
        }
        if c == '[' {
            if nesting > 0
                && let Some(fs) = prefix.rfind('{')
            {
                let before = prefix[..fs].chars().last();
                if before.is_some_and(is_fmt) {
                    if fs + 1 == prefix.len() {
                        let cut = fs - before.map(char::len_utf8).unwrap_or(0);
                        prefix.truncate(cut);
                    } else {
                        fmt_wraps_name = true;
                    }
                }
            }
            break;
        }
        prefix.push(c);
        i += 1;
    }
    i += 1;
    if i >= chars.len() {
        return None;
    }
    let mut tmp = String::new();
    let mut begin: i64 = 0;
    let mut found_dots = false;
    while i < chars.len() {
        if chars[i] == '.' && chars.get(i + 1) == Some(&'.') {
            begin = tmp.parse().unwrap_or(0);
            i += 2;
            found_dots = true;
            break;
        }
        if !chars[i].is_ascii_digit() {
            return None;
        }
        tmp.push(chars[i]);
        i += 1;
    }
    if !found_dots || i >= chars.len() {
        return None;
    }
    tmp.clear();
    let mut end: i64 = 0;
    let mut closed = false;
    while i < chars.len() {
        if chars[i] == ']' {
            end = tmp.parse().unwrap_or(0);
            i += 1;
            closed = true;
            break;
        }
        if !chars[i].is_ascii_digit() {
            return None;
        }
        tmp.push(chars[i]);
        i += 1;
    }
    if !closed {
        return None;
    }
    while i < chars.len() {
        match chars[i] {
            '}' => {
                nesting -= 1;
                if fmt_wraps_name {
                    suffix.push('}');
                }
            }
            c @ ('+' | '-' | 'P' | 'N') => suffix.push(c),
            _ => return None,
        }
        i += 1;
    }
    if nesting != 0 || begin == end {
        return None;
    }
    let (lo, hi) = if begin > end {
        (end, begin)
    } else {
        (begin, end)
    };
    // A hostile range would otherwise allocate without bound.
    if hi - lo > 4096 {
        return None;
    }
    Some((
        prefix.clone(),
        (lo..=hi).map(|n| format!("{prefix}{n}{suffix}")).collect(),
    ))
}

/// `NET_SETTINGS::ParseBusGroup`, without the quoted-name extension.
pub fn parse_bus_group(group: &str) -> Option<(String, Vec<String>)> {
    let chars: Vec<char> = group.chars().collect();
    let mut i = 0;
    let mut prefix = String::new();
    let mut nesting: i32 = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '{' {
            if i > 0 && is_fmt(chars[i - 1]) {
                nesting += 1;
                prefix.push('{');
                i += 1;
                continue;
            }
            break;
        } else if c == '}' {
            nesting -= 1;
            prefix.push('}');
            i += 1;
            continue;
        }
        if c == '\\' && chars.get(i + 1) == Some(&' ') {
            prefix.push(' ');
            i += 2;
            continue;
        }
        if c == ' ' || c == '[' || c == ']' {
            return None;
        }
        prefix.push(c);
        i += 1;
    }
    if nesting != 0 {
        return None;
    }
    i += 1;
    if i >= chars.len() {
        return None;
    }
    let escape_spaces = |s: &str| s.replace(' ', "\\ ");
    let mut members = Vec::new();
    let mut tmp = String::new();
    while i < chars.len() {
        let c = chars[i];
        if c == '{' {
            if i > 0 && is_fmt(chars[i - 1]) {
                nesting += 1;
                tmp.push('{');
                i += 1;
                continue;
            }
            return None;
        } else if c == '}' {
            if nesting > 0 {
                nesting -= 1;
                tmp.push('}');
                i += 1;
                continue;
            }
            if !tmp.is_empty() {
                members.push(escape_netname(&escape_spaces(&tmp)));
            }
            return Some((prefix, members));
        }
        if c == '\\' && chars.get(i + 1) == Some(&' ') {
            tmp.push(' ');
            i += 2;
            continue;
        }
        if c == ' ' || c == ',' {
            if !tmp.is_empty() {
                members.push(escape_netname(&escape_spaces(&tmp)));
            }
            tmp.clear();
            i += 1;
            continue;
        }
        tmp.push(c);
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bus_vectors() {
        assert_eq!(
            parse_bus_vector("D[0..2]"),
            Some(("D".into(), vec!["D0".into(), "D1".into(), "D2".into()]))
        );
        assert_eq!(
            parse_bus_vector("D[2..0]").unwrap().1,
            vec!["D0", "D1", "D2"]
        );
        assert_eq!(parse_bus_vector("D[1..1]"), None);
        assert_eq!(parse_bus_vector("D0"), None);
        assert_eq!(parse_bus_vector("A B[0..1]"), None);
        assert_eq!(parse_bus_vector("X[0..1]+").unwrap().1, vec!["X0+", "X1+"]);
        assert_eq!(parse_bus_vector("D[0..99999999]"), None);
    }

    #[test]
    fn bus_groups() {
        assert_eq!(
            parse_bus_group("USB{DP DN}"),
            Some(("USB".into(), vec!["DP".into(), "DN".into()]))
        );
        assert_eq!(
            parse_bus_group("{A,B}"),
            Some((String::new(), vec!["A".into(), "B".into()]))
        );
        assert_eq!(parse_bus_group("~{X}"), None);
        assert_eq!(parse_bus_group("G{A"), None);
    }

    #[test]
    fn names_and_paths() {
        let paths = vec!["/".to_string(), "/sub/".to_string()];
        let aliases = Aliases::new();
        let mut c = Conn::new(1);
        c.configure_from_label("SIG", &aliases);
        assert_eq!(c.full_name(&paths), "/sub/SIG");
        c.set_driver(Some(DriverKind::GlobalLabel));
        assert_eq!(c.full_name(&paths), "SIG");
        let mut g = Conn::new(0);
        g.configure_from_label("USB{DP DN}", &aliases);
        assert_eq!(g.members[0].full_name(&paths), "/USB.DP");
        assert_eq!(unescape("A{slash}B{x}"), "A/B{x}");
        assert_eq!(escape_netname("a/b\n"), "a{slash}b");
    }

    #[test]
    fn self_referencing_aliases_stop() {
        // Six members that each name the alias itself: without the alias
        // stack this grows as 6^9 and exhausts memory.
        let mut aliases = Aliases::new();
        aliases.insert("B".into(), vec!["{B B B B B B}".into(); 6]);
        let mut c = Conn::new(0);
        assert!(!c.configure_from_label("{B}", &aliases));
        assert!(c.tree_size() <= MAX_LABEL_MEMBERS + 1);
        // Two aliases that refer to each other.
        let mut aliases = Aliases::new();
        aliases.insert("A".into(), vec!["{B B B B}".into(); 4]);
        aliases.insert("B".into(), vec!["{A A A A}".into(); 4]);
        let mut c = Conn::new(0);
        assert!(!c.configure_from_label("{A}", &aliases));
        assert!(c.tree_size() <= MAX_LABEL_MEMBERS + 1);
        // An ordinary alias still expands, and reports a complete expansion.
        let mut aliases = Aliases::new();
        aliases.insert("USB".into(), vec!["DP".into(), "DN".into()]);
        let mut c = Conn::new(0);
        assert!(c.configure_from_label("{USB}", &aliases));
        let names: Vec<&str> = c.members.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, ["DP", "DN"]);
    }

    #[test]
    fn a_group_of_many_vectors_is_capped() {
        let label = format!(
            "{{{}}}",
            (1..=1000)
                .map(|i| format!("A{i}[0..4000]"))
                .collect::<Vec<_>>()
                .join(" ")
        );
        let mut c = Conn::new(0);
        assert!(!c.configure_from_label(&label, &Aliases::new()));
        assert!(c.tree_size() <= MAX_LABEL_MEMBERS + 1, "{}", c.tree_size());
        // One full-width vector still fits.
        let mut c = Conn::new(0);
        assert!(c.configure_from_label("{A[0..4096]}", &Aliases::new()));
        assert_eq!(c.members[0].members.len(), 4097);
    }
}
