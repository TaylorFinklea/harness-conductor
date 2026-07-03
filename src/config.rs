//! conductor.toml load + validation (incl. roster) and `config check` preflight.

// The config surface (structs, enums, HARDCODED_EXCLUDE) is built ahead of its
// consumers in scan/triage/dispatch/verify (milestones M1+); silence dead-code
// until those modules read these fields.
#![allow(dead_code)]

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub(crate) struct ConfigError {
    pub(crate) message: String,
}

impl ConfigError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ConfigError {}

pub(crate) type Result<T> = std::result::Result<T, ConfigError>;

// ---------------------------------------------------------------------------
// Closed enums (invariant 1: closed roster; fail closed, never default)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tier {
    Lead,
    Senior,
    Junior,
}

impl FromStr for Tier {
    type Err = ConfigError;
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "lead" => Ok(Tier::Lead),
            "senior" => Ok(Tier::Senior),
            "junior" => Ok(Tier::Junior),
            _ => Err(ConfigError::new(format!(
                "unknown tier {s:?} (expected lead|senior|junior)"
            ))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Ceiling {
    S,
    M,
    L,
    Xl,
}

impl FromStr for Ceiling {
    type Err = ConfigError;
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "S" => Ok(Ceiling::S),
            "M" => Ok(Ceiling::M),
            "L" => Ok(Ceiling::L),
            "XL" => Ok(Ceiling::Xl),
            _ => Err(ConfigError::new(format!(
                "unknown ceiling {s:?} (expected S|M|L|XL)"
            ))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Efficiency {
    Lean,
    Std,
    Heavy,
}

impl FromStr for Efficiency {
    type Err = ConfigError;
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "lean" => Ok(Efficiency::Lean),
            "std" => Ok(Efficiency::Std),
            "heavy" => Ok(Efficiency::Heavy),
            _ => Err(ConfigError::new(format!(
                "unknown efficiency {s:?} (expected lean|std|heavy)"
            ))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Backend {
    Claude,
    Pi,
    Agy,
}

impl FromStr for Backend {
    type Err = ConfigError;
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "claude" => Ok(Backend::Claude),
            "pi" => Ok(Backend::Pi),
            "agy" => Ok(Backend::Agy),
            _ => Err(ConfigError::new(format!(
                "unknown backend {s:?} (expected claude|pi|agy)"
            ))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Autonomy {
    Propose,
    Ratchet,
}

impl FromStr for Autonomy {
    type Err = ConfigError;
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "propose" => Ok(Autonomy::Propose),
            "ratchet" => Ok(Autonomy::Ratchet),
            _ => Err(ConfigError::new(format!(
                "unknown autonomy {s:?} (expected propose|ratchet)"
            ))),
        }
    }
}

// ---------------------------------------------------------------------------
// Config structs
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub(crate) struct RosterEntry {
    pub(crate) name: String,
    pub(crate) tier: Tier,
    pub(crate) ceiling: Ceiling,
    pub(crate) efficiency: Efficiency,
    pub(crate) backend: Backend,
    pub(crate) dispatch_id: String,
}

#[derive(Debug, Clone)]
pub(crate) struct ScanConfig {
    pub(crate) root: String,
    pub(crate) exclude: Vec<String>,
}

impl Default for ScanConfig {
    fn default() -> Self {
        Self {
            root: "~/git".to_string(),
            exclude: Vec::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Budgets {
    pub(crate) max_dispatches_per_cycle: u32,
    pub(crate) max_active_per_repo: u32,
    pub(crate) max_external_dispatches: u32,
    pub(crate) item_wall_clock_mins: u32,
    pub(crate) cycle_wall_clock_mins: u32,
}

impl Default for Budgets {
    fn default() -> Self {
        Self {
            max_dispatches_per_cycle: 8,
            max_active_per_repo: 1,
            max_external_dispatches: 4,
            item_wall_clock_mins: 45,
            cycle_wall_clock_mins: 90,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct VerifyConfig {
    pub(crate) judge: String,
    pub(crate) always_orchestra: bool,
}

impl Default for VerifyConfig {
    fn default() -> Self {
        Self {
            judge: "opencode-go/qwen3.7-max".to_string(),
            always_orchestra: false,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ReviewConfig {
    pub(crate) enabled: bool,
    pub(crate) min_tier_gap: u32,
}

impl Default for ReviewConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            min_tier_gap: 1,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Config {
    pub(crate) autonomy: Autonomy,
    pub(crate) scan: ScanConfig,
    pub(crate) budgets: Budgets,
    pub(crate) verify: VerifyConfig,
    pub(crate) review: ReviewConfig,
    pub(crate) roster: Vec<RosterEntry>,
}

// Repos hard-excluded from scanning regardless of the config `[scan] exclude`
// list (invariant 5: never scan/dispatch chezmoi-config).
pub(crate) const HARDCODED_EXCLUDE: &[&str] = &["chezmoi-config"];

// ---------------------------------------------------------------------------
// Minimal TOML subset parser
//
// Supports only the surface this config file needs: bare keys, `[table]` and
// `[[array-of-tables]]` headers, and string / integer / boolean / string-array
// values. Comments (`#...` to end of line), quoted-string escapes, and
// multi-line arrays are handled. Dotted keys, inline tables, multi-line
// strings, datetimes, and non-string array elements are intentionally out of
// scope and rejected. Keeping the subset small keeps the parser auditable.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum Node {
    Str(String),
    Int(i64),
    Bool(bool),
    StrArr(Vec<String>),
    Table(HashMap<String, Node>),
    Tables(Vec<HashMap<String, Node>>),
}

type Doc = HashMap<String, Node>;

#[derive(Debug, Clone, Copy)]
enum Target {
    Root,
    Table,
    ArrayTable,
}

struct Parser<'a> {
    chars: std::iter::Peekable<std::str::Chars<'a>>,
    line: usize,
    table_name: String,
}

fn is_bare_key_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

impl<'a> Parser<'a> {
    fn new(src: &'a str) -> Self {
        Self {
            chars: src.chars().peekable(),
            line: 1,
            table_name: String::new(),
        }
    }

    fn peek(&mut self) -> Option<char> {
        self.chars.peek().copied()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.chars.next()?;
        if c == '\n' {
            self.line += 1;
        }
        Some(c)
    }

    fn bump_expect(&mut self, expected: char) -> Result<()> {
        match self.bump() {
            Some(c) if c == expected => Ok(()),
            Some(c) => Err(ConfigError::new(format!(
                "line {}: expected {:?}, found {:?}",
                self.line, expected, c
            ))),
            None => Err(ConfigError::new(format!(
                "line {}: expected {:?}, found end of input",
                self.line, expected
            ))),
        }
    }

    fn skip_inline_ws(&mut self) {
        loop {
            match self.peek() {
                Some(' ' | '\t' | '\r') => {
                    self.bump();
                }
                Some('#') => {
                    while let Some(c) = self.peek() {
                        if c == '\n' {
                            break;
                        }
                        self.bump();
                    }
                }
                _ => break,
            }
        }
    }

    fn skip_blanks(&mut self) {
        loop {
            self.skip_inline_ws();
            if self.peek() == Some('\n') {
                self.bump();
            } else {
                break;
            }
        }
    }

    fn expect_line_end(&mut self) -> Result<()> {
        self.skip_inline_ws();
        match self.peek() {
            None | Some('\n') => Ok(()),
            Some(c) => Err(ConfigError::new(format!(
                "line {}: unexpected trailing character {:?}",
                self.line, c
            ))),
        }
    }

    fn parse_bare_key(&mut self) -> Result<String> {
        let mut s = String::new();
        while let Some(c) = self.peek() {
            if is_bare_key_char(c) {
                s.push(c);
                self.bump();
            } else {
                break;
            }
        }
        if s.is_empty() {
            let found = self.peek();
            return Err(ConfigError::new(format!(
                "line {}: expected a key, found {:?}",
                self.line, found
            )));
        }
        Ok(s)
    }

    fn parse_document(&mut self, doc: &mut Doc) -> Result<()> {
        let mut target = Target::Root;
        loop {
            self.skip_blanks();
            match self.peek() {
                None => return Ok(()),
                Some('[') => target = self.parse_header(doc)?,
                Some(c) if is_bare_key_char(c) => self.parse_kv(doc, target)?,
                Some(c) => {
                    return Err(ConfigError::new(format!(
                        "line {}: unexpected character {:?} (expected key or table header)",
                        self.line, c
                    )))
                }
            }
        }
    }

    fn parse_header(&mut self, doc: &mut Doc) -> Result<Target> {
        self.bump_expect('[')?;
        let array = self.peek() == Some('[');
        if array {
            self.bump();
        }
        self.skip_inline_ws();
        let name = self.parse_bare_key()?;
        self.skip_inline_ws();
        self.bump_expect(']')?;
        if array {
            self.bump_expect(']')?;
        }
        self.expect_line_end()?;

        if array {
            if let Some(Node::Tables(v)) = doc.get_mut(&name) {
                v.push(HashMap::new());
            } else if doc.contains_key(&name) {
                return Err(ConfigError::new(format!(
                    "line {}: [{name}] redefined as array-table conflicts with existing table",
                    self.line
                )));
            } else {
                doc.insert(name.clone(), Node::Tables(vec![HashMap::new()]));
            }
            self.table_name = name;
            Ok(Target::ArrayTable)
        } else {
            if doc.contains_key(&name) {
                return Err(ConfigError::new(format!(
                    "line {}: duplicate table [{name}]",
                    self.line
                )));
            }
            doc.insert(name.clone(), Node::Table(HashMap::new()));
            self.table_name = name;
            Ok(Target::Table)
        }
    }

    fn parse_kv(&mut self, doc: &mut Doc, target: Target) -> Result<()> {
        let key = self.parse_bare_key()?;
        self.skip_inline_ws();
        self.bump_expect('=')?;
        self.skip_inline_ws();
        let value = self.parse_value()?;
        self.expect_line_end()?;
        match target {
            Target::Root => insert_unique(doc, key, value),
            Target::Table => match doc.get_mut(&self.table_name) {
                Some(Node::Table(t)) => insert_unique(t, key, value),
                _ => Err(ConfigError::new("internal: target table missing")),
            },
            Target::ArrayTable => match doc.get_mut(&self.table_name) {
                Some(Node::Tables(v)) => match v.last_mut() {
                    Some(t) => insert_unique(t, key, value),
                    None => Err(ConfigError::new("internal: empty array-table")),
                },
                _ => Err(ConfigError::new("internal: target array-table missing")),
            },
        }
    }

    fn parse_value(&mut self) -> Result<Node> {
        match self.peek() {
            Some('"') => self.parse_string().map(Node::Str),
            Some('[') => self.parse_string_array().map(Node::StrArr),
            Some('t') => self.parse_keyword("true", Node::Bool(true)),
            Some('f') => self.parse_keyword("false", Node::Bool(false)),
            Some(c) if c == '-' || c.is_ascii_digit() => self.parse_int(),
            Some(c) => Err(ConfigError::new(format!(
                "line {}: unexpected value starting with {:?}",
                self.line, c
            ))),
            None => Err(ConfigError::new(format!(
                "line {}: expected a value, found end of input",
                self.line
            ))),
        }
    }

    fn parse_keyword(&mut self, kw: &str, node: Node) -> Result<Node> {
        for expected in kw.chars() {
            match self.bump() {
                Some(c) if c == expected => {}
                Some(c) => {
                    return Err(ConfigError::new(format!(
                        "line {}: expected keyword {kw:?}, found {:?}",
                        self.line, c
                    )))
                }
                None => {
                    return Err(ConfigError::new(format!(
                        "line {}: expected keyword {kw:?}, found end of input",
                        self.line
                    )))
                }
            }
        }
        Ok(node)
    }

    fn parse_string(&mut self) -> Result<String> {
        self.bump_expect('"')?;
        let mut s = String::new();
        loop {
            match self.bump() {
                None => {
                    return Err(ConfigError::new(format!(
                        "line {}: unterminated string",
                        self.line
                    )))
                }
                Some('"') => break,
                Some('\\') => match self.bump() {
                    Some('"') => s.push('"'),
                    Some('\\') => s.push('\\'),
                    Some('n') => s.push('\n'),
                    Some('t') => s.push('\t'),
                    Some('r') => s.push('\r'),
                    Some(other) => {
                        return Err(ConfigError::new(format!(
                            "line {}: invalid escape \\{other}",
                            self.line
                        )))
                    }
                    None => {
                        return Err(ConfigError::new(format!(
                            "line {}: unterminated string after escape",
                            self.line
                        )))
                    }
                },
                Some('\n') => {
                    return Err(ConfigError::new(format!(
                        "line {}: newline in string",
                        self.line
                    )))
                }
                Some(c) => s.push(c),
            }
        }
        Ok(s)
    }

    fn parse_int(&mut self) -> Result<Node> {
        let mut s = String::new();
        if self.peek() == Some('-') {
            s.push('-');
            self.bump();
        }
        let mut digits = 0u32;
        while let Some(c) = self.peek() {
            if c.is_ascii_digit() {
                s.push(c);
                self.bump();
                digits += 1;
            } else {
                break;
            }
        }
        if digits == 0 {
            return Err(ConfigError::new(format!(
                "line {}: expected digits after sign",
                self.line
            )));
        }
        s.parse::<i64>()
            .map(Node::Int)
            .map_err(|e| ConfigError::new(format!("line {}: invalid integer: {e}", self.line)))
    }

    fn parse_string_array(&mut self) -> Result<Vec<String>> {
        self.bump_expect('[')?;
        let mut items = Vec::new();
        loop {
            self.skip_blanks();
            match self.peek() {
                None => {
                    return Err(ConfigError::new(format!(
                        "line {}: unterminated array",
                        self.line
                    )))
                }
                Some(']') => {
                    self.bump();
                    break;
                }
                Some('"') => {
                    items.push(self.parse_string()?);
                    self.skip_blanks();
                    match self.peek() {
                        Some(',') => {
                            self.bump();
                        }
                        Some(']') => {
                            self.bump();
                            break;
                        }
                        Some(c) => {
                            return Err(ConfigError::new(format!(
                                "line {}: expected ',' or ']' in array, found {:?}",
                                self.line, c
                            )))
                        }
                        None => {
                            return Err(ConfigError::new(format!(
                                "line {}: unterminated array",
                                self.line
                            )))
                        }
                    }
                }
                Some(c) => {
                    return Err(ConfigError::new(format!(
                        "line {}: expected string or ']' in array, found {:?}",
                        self.line, c
                    )))
                }
            }
        }
        Ok(items)
    }
}

fn insert_unique(table: &mut HashMap<String, Node>, key: String, value: Node) -> Result<()> {
    if table.contains_key(&key) {
        return Err(ConfigError::new(format!("duplicate key: {key}")));
    }
    table.insert(key, value);
    Ok(())
}

// ---------------------------------------------------------------------------
// Doc -> Config with strict validation
// ---------------------------------------------------------------------------

pub(crate) fn parse_str(src: &str) -> Result<Config> {
    let mut doc = HashMap::new();
    let mut parser = Parser::new(src);
    parser.parse_document(&mut doc)?;
    from_doc(&doc)
}

pub(crate) fn load(path: &Path) -> Result<Config> {
    let src = std::fs::read_to_string(path)
        .map_err(|e| ConfigError::new(format!("failed to read {}: {e}", path.display())))?;
    parse_str(&src)
}

fn from_doc(doc: &Doc) -> Result<Config> {
    for key in doc.keys() {
        if !matches!(
            key.as_str(),
            "autonomy" | "scan" | "budgets" | "verify" | "review" | "roster"
        ) {
            return Err(ConfigError::new(format!("unknown config key: {key}")));
        }
    }
    let autonomy = match doc.get("autonomy") {
        None => Autonomy::Propose,
        Some(Node::Str(s)) => s.parse::<Autonomy>()?,
        Some(_) => return Err(ConfigError::new("autonomy must be a string")),
    };
    let scan = parse_scan(doc.get("scan"))?;
    let budgets = parse_budgets(doc.get("budgets"))?;
    let verify = parse_verify(doc.get("verify"))?;
    let review = parse_review(doc.get("review"))?;
    let roster = parse_roster(doc.get("roster"))?;
    Ok(Config {
        autonomy,
        scan,
        budgets,
        verify,
        review,
        roster,
    })
}

fn parse_scan(node: Option<&Node>) -> Result<ScanConfig> {
    let t = match node {
        None => return Ok(ScanConfig::default()),
        Some(Node::Table(t)) => t,
        Some(_) => return Err(ConfigError::new("scan must be a table")),
    };
    let mut s = ScanConfig::default();
    for (key, val) in t {
        match key.as_str() {
            "root" => s.root = expect_str("scan.root", val)?,
            "exclude" => s.exclude = expect_str_arr("scan.exclude", val)?,
            other => return Err(ConfigError::new(format!("unknown scan key: {other}"))),
        }
    }
    Ok(s)
}

fn parse_budgets(node: Option<&Node>) -> Result<Budgets> {
    let t = match node {
        None => return Ok(Budgets::default()),
        Some(Node::Table(t)) => t,
        Some(_) => return Err(ConfigError::new("budgets must be a table")),
    };
    let mut b = Budgets::default();
    for (key, val) in t {
        match key.as_str() {
            "max_dispatches_per_cycle" => {
                b.max_dispatches_per_cycle = expect_u32("budgets.max_dispatches_per_cycle", val)?;
            }
            "max_active_per_repo" => {
                b.max_active_per_repo = expect_u32("budgets.max_active_per_repo", val)?;
            }
            "max_external_dispatches" => {
                b.max_external_dispatches = expect_u32("budgets.max_external_dispatches", val)?;
            }
            "item_wall_clock_mins" => {
                b.item_wall_clock_mins = expect_u32("budgets.item_wall_clock_mins", val)?;
            }
            "cycle_wall_clock_mins" => {
                b.cycle_wall_clock_mins = expect_u32("budgets.cycle_wall_clock_mins", val)?;
            }
            other => return Err(ConfigError::new(format!("unknown budgets key: {other}"))),
        }
    }
    Ok(b)
}

fn parse_verify(node: Option<&Node>) -> Result<VerifyConfig> {
    let t = match node {
        None => return Ok(VerifyConfig::default()),
        Some(Node::Table(t)) => t,
        Some(_) => return Err(ConfigError::new("verify must be a table")),
    };
    let mut v = VerifyConfig::default();
    for (key, val) in t {
        match key.as_str() {
            "judge" => v.judge = expect_str("verify.judge", val)?,
            "always_orchestra" => v.always_orchestra = expect_bool("verify.always_orchestra", val)?,
            other => return Err(ConfigError::new(format!("unknown verify key: {other}"))),
        }
    }
    Ok(v)
}

fn parse_review(node: Option<&Node>) -> Result<ReviewConfig> {
    let t = match node {
        None => return Ok(ReviewConfig::default()),
        Some(Node::Table(t)) => t,
        Some(_) => return Err(ConfigError::new("review must be a table")),
    };
    let mut r = ReviewConfig::default();
    for (key, val) in t {
        match key.as_str() {
            "enabled" => r.enabled = expect_bool("review.enabled", val)?,
            "min_tier_gap" => r.min_tier_gap = expect_u32("review.min_tier_gap", val)?,
            other => return Err(ConfigError::new(format!("unknown review key: {other}"))),
        }
    }
    Ok(r)
}

fn parse_roster(node: Option<&Node>) -> Result<Vec<RosterEntry>> {
    let entries = match node {
        None => return Ok(Vec::new()),
        Some(Node::Tables(v)) => v,
        Some(_) => {
            return Err(ConfigError::new(
                "roster must be an array of tables ([[roster]])",
            ))
        }
    };
    let mut seen = HashSet::new();
    let mut out = Vec::with_capacity(entries.len());
    for (i, t) in entries.iter().enumerate() {
        for key in t.keys() {
            if !matches!(
                key.as_str(),
                "name" | "tier" | "ceiling" | "efficiency" | "backend" | "dispatch_id"
            ) {
                return Err(ConfigError::new(format!(
                    "unknown roster key in entry {i}: {key}"
                )));
            }
        }
        let name = get_required_str(t, i, "name")?;
        let tier = get_required_str(t, i, "tier")?.parse::<Tier>()?;
        let ceiling = get_required_str(t, i, "ceiling")?.parse::<Ceiling>()?;
        let efficiency = get_required_str(t, i, "efficiency")?.parse::<Efficiency>()?;
        let backend = get_required_str(t, i, "backend")?.parse::<Backend>()?;
        let dispatch_id = get_required_str(t, i, "dispatch_id")?;
        if !seen.insert(name.clone()) {
            return Err(ConfigError::new(format!("duplicate roster name: {name}")));
        }
        out.push(RosterEntry {
            name,
            tier,
            ceiling,
            efficiency,
            backend,
            dispatch_id,
        });
    }
    Ok(out)
}

fn get_required_str(t: &HashMap<String, Node>, i: usize, key: &str) -> Result<String> {
    match t.get(key) {
        Some(Node::Str(s)) => Ok(s.clone()),
        Some(_) => Err(ConfigError::new(format!(
            "roster entry {i} field {key} must be a string"
        ))),
        None => Err(ConfigError::new(format!(
            "roster entry {i} missing required field: {key}"
        ))),
    }
}

fn expect_str(loc: &str, node: &Node) -> Result<String> {
    match node {
        Node::Str(s) => Ok(s.clone()),
        _ => Err(ConfigError::new(format!("{loc} must be a string"))),
    }
}

fn expect_str_arr(loc: &str, node: &Node) -> Result<Vec<String>> {
    match node {
        Node::StrArr(v) => Ok(v.clone()),
        _ => Err(ConfigError::new(format!(
            "{loc} must be an array of strings"
        ))),
    }
}

fn expect_u32(loc: &str, node: &Node) -> Result<u32> {
    match node {
        Node::Int(i) => u32::try_from(*i)
            .map_err(|_| ConfigError::new(format!("{loc} must fit in u32 (got {i})"))),
        _ => Err(ConfigError::new(format!("{loc} must be an integer"))),
    }
}

fn expect_bool(loc: &str, node: &Node) -> Result<bool> {
    match node {
        Node::Bool(b) => Ok(*b),
        _ => Err(ConfigError::new(format!("{loc} must be a boolean"))),
    }
}

// ---------------------------------------------------------------------------
// Preflight (`conductor config check`)
// ---------------------------------------------------------------------------

const PATH_TOOLS: &[&str] = &["bd", "pi", "agy", "claude", "orchestra", "bun", "harness-deck"];

#[derive(Debug, Clone)]
pub(crate) struct PreflightCheck {
    pub(crate) name: String,
    pub(crate) ok: bool,
    pub(crate) message: String,
}

pub(crate) fn preflight_checks(path_var: &str, state_dir: Option<&Path>) -> Vec<PreflightCheck> {
    let mut checks: Vec<PreflightCheck> = Vec::with_capacity(PATH_TOOLS.len() + 1);
    for tool in PATH_TOOLS.iter().copied() {
        match find_in_path(tool, path_var) {
            Some(found) => checks.push(PreflightCheck {
                name: tool.to_string(),
                ok: true,
                message: format!("found ({})", found.display()),
            }),
            None => checks.push(PreflightCheck {
                name: tool.to_string(),
                ok: false,
                message: "not found on PATH".to_string(),
            }),
        }
    }
    match state_dir {
        Some(dir) => match check_state_dir(dir) {
            Ok(()) => checks.push(PreflightCheck {
                name: "state dir".to_string(),
                ok: true,
                message: format!("writable ({})", dir.display()),
            }),
            Err(e) => checks.push(PreflightCheck {
                name: "state dir".to_string(),
                ok: false,
                message: format!("not writable: {e}"),
            }),
        },
        None => checks.push(PreflightCheck {
            name: "state dir".to_string(),
            ok: false,
            message: "HOME not set; cannot locate ~/.local/state/conductor".to_string(),
        }),
    }
    checks
}

fn find_in_path(name: &str, path_var: &str) -> Option<PathBuf> {
    for dir in path_var.split(':') {
        if dir.is_empty() {
            continue;
        }
        let candidate = Path::new(dir).join(name);
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }
    None
}

fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let Ok(md) = std::fs::metadata(path) else {
            return false;
        };
        md.is_file() && (md.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

fn check_state_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let probe = dir.join("conductor-preflight-probe");
    std::fs::write(&probe, b"")?;
    std::fs::remove_file(&probe)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fmt::Write;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    // --- test helpers ---

    fn full_entry() -> String {
        entry_with("name", "x")
    }

    fn entry_with(field: &str, value: &str) -> String {
        let mut s = String::from("[[roster]]\n");
        for (k, v) in [
            ("name", "x"),
            ("tier", "senior"),
            ("ceiling", "M"),
            ("efficiency", "lean"),
            ("backend", "pi"),
            ("dispatch_id", "opencode-go/x"),
        ] {
            let val = if k == field { value } else { v };
            let _ = writeln!(s, "{k} = \"{val}\"");
        }
        s
    }

    fn entry_without(omit: &str) -> String {
        let mut s = String::from("[[roster]]\n");
        for (k, v) in [
            ("name", "x"),
            ("tier", "senior"),
            ("ceiling", "M"),
            ("efficiency", "lean"),
            ("backend", "pi"),
            ("dispatch_id", "opencode-go/x"),
        ] {
            if k != omit {
                let _ = writeln!(s, "{k} = \"{v}\"");
            }
        }
        s
    }

    fn assert_err(label: &str, src: &str) {
        let res = parse_str(src);
        assert!(
            res.is_err(),
            "expected error for {label}, got Ok: {res:?}"
        );
    }

    struct TempDir(PathBuf);
    impl TempDir {
        fn new(label: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos();
            let d = std::env::temp_dir().join(format!("conductor-test-{label}-{nanos}"));
            std::fs::create_dir_all(&d).expect("mkdir");
            TempDir(d)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn make_exec(path: &Path) {
        std::fs::write(path, b"#!/bin/sh\nexit 0\n").expect("write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(path).expect("stat").permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(path, perms).expect("chmod");
        }
    }

    fn check<'a>(checks: &'a [PreflightCheck], name: &str) -> &'a PreflightCheck {
        checks
            .iter()
            .find(|c| c.name == name)
            .unwrap_or_else(|| panic!("missing check named {name}"))
    }

    // --- the checked-in config ---

    #[test]
    fn checked_in_config_parses_and_has_seven_entries() {
        let cfg = parse_str(include_str!("../conductor.toml"))
            .expect("checked-in conductor.toml must parse");
        assert_eq!(cfg.roster.len(), 7);
        // spec's exact roster table, in order.
        assert_eq!(cfg.roster[0].name, "sonnet-5");
        assert_eq!(cfg.roster[0].tier, Tier::Lead);
        assert_eq!(cfg.roster[0].ceiling, Ceiling::L);
        assert_eq!(cfg.roster[0].efficiency, Efficiency::Std);
        assert_eq!(cfg.roster[0].backend, Backend::Claude);
        assert_eq!(cfg.roster[0].dispatch_id, "claude-sonnet-5");
        assert_eq!(cfg.roster[1].name, "opus-4.8");
        assert_eq!(cfg.roster[1].ceiling, Ceiling::Xl);
        assert_eq!(cfg.roster[1].efficiency, Efficiency::Heavy);
        assert_eq!(cfg.roster[6].name, "gemini-3.5-flash");
        assert_eq!(cfg.roster[6].tier, Tier::Junior);
        assert_eq!(cfg.roster[6].ceiling, Ceiling::S);
        assert_eq!(cfg.roster[6].backend, Backend::Agy);
        assert_eq!(cfg.roster[6].dispatch_id, "Gemini 3.5 Flash (High)");
        // defaults
        assert_eq!(cfg.autonomy, Autonomy::Propose);
        assert_eq!(cfg.scan.root, "~/git");
        assert!(cfg.scan.exclude.is_empty());
        assert_eq!(cfg.budgets.max_dispatches_per_cycle, 8);
        assert_eq!(cfg.budgets.max_active_per_repo, 1);
        assert_eq!(cfg.budgets.max_external_dispatches, 4);
        assert_eq!(cfg.budgets.item_wall_clock_mins, 45);
        assert_eq!(cfg.budgets.cycle_wall_clock_mins, 90);
        assert_eq!(cfg.verify.judge, "opencode-go/qwen3.7-max");
        assert!(!cfg.verify.always_orchestra);
        assert!(cfg.review.enabled);
        assert_eq!(cfg.review.min_tier_gap, 1);
    }

    // --- valid configs ---

    #[test]
    fn full_config_overrides_defaults() {
        let src = "\
autonomy = \"ratchet\"

[scan]
root = \"~/code\"
exclude = [\"chezmoi-config\", \"scratch\"]

[budgets]
max_dispatches_per_cycle = 3
max_active_per_repo = 2
max_external_dispatches = 1
item_wall_clock_mins = 20
cycle_wall_clock_mins = 60

[verify]
judge = \"opencode-go/kimi-k2.7-code\"
always_orchestra = true

[review]
enabled = false
min_tier_gap = 2

[[roster]]
name = \"sonnet-5\"
tier = \"lead\"
ceiling = \"L\"
efficiency = \"std\"
backend = \"claude\"
dispatch_id = \"claude-sonnet-5\"
";
        let cfg = parse_str(src).expect("valid full config");
        assert_eq!(cfg.autonomy, Autonomy::Ratchet);
        assert_eq!(cfg.scan.root, "~/code");
        assert_eq!(
            cfg.scan.exclude,
            vec!["chezmoi-config".to_string(), "scratch".to_string()]
        );
        assert_eq!(cfg.budgets.max_dispatches_per_cycle, 3);
        assert_eq!(cfg.budgets.max_active_per_repo, 2);
        assert_eq!(cfg.budgets.max_external_dispatches, 1);
        assert_eq!(cfg.budgets.item_wall_clock_mins, 20);
        assert_eq!(cfg.budgets.cycle_wall_clock_mins, 60);
        assert_eq!(cfg.verify.judge, "opencode-go/kimi-k2.7-code");
        assert!(cfg.verify.always_orchestra);
        assert!(!cfg.review.enabled);
        assert_eq!(cfg.review.min_tier_gap, 2);
        assert_eq!(cfg.roster.len(), 1);
        assert_eq!(cfg.roster[0].tier, Tier::Lead);
    }

    #[test]
    fn minimal_config_uses_defaults() {
        let cfg = parse_str(&full_entry()).expect("minimal config");
        assert_eq!(cfg.autonomy, Autonomy::Propose);
        assert_eq!(cfg.scan.root, "~/git");
        assert!(cfg.scan.exclude.is_empty());
        assert_eq!(cfg.budgets.max_dispatches_per_cycle, 8);
        assert_eq!(cfg.budgets.max_active_per_repo, 1);
        assert_eq!(cfg.budgets.max_external_dispatches, 4);
        assert_eq!(cfg.budgets.item_wall_clock_mins, 45);
        assert_eq!(cfg.budgets.cycle_wall_clock_mins, 90);
        assert_eq!(cfg.verify.judge, "opencode-go/qwen3.7-max");
        assert!(!cfg.verify.always_orchestra);
        assert!(cfg.review.enabled);
        assert_eq!(cfg.review.min_tier_gap, 1);
        assert_eq!(cfg.roster.len(), 1);
    }

    #[test]
    fn empty_input_is_a_valid_empty_roster() {
        let cfg = parse_str("").expect("empty input");
        assert!(cfg.roster.is_empty());
        assert_eq!(cfg.scan.root, "~/git");
    }

    #[test]
    fn comments_and_blank_lines_are_ignored() {
        let src = "# leading comment\n\nautonomy = \"ratchet\" # trailing\n\n[[roster]] # header comment\nname = \"solo\" # name\ntier = \"junior\"\nceiling = \"S\"\nefficiency = \"lean\"\nbackend = \"agy\"\ndispatch_id = \"G\"\n";
        let cfg = parse_str(src).expect("commented config");
        assert_eq!(cfg.autonomy, Autonomy::Ratchet);
        assert_eq!(cfg.roster[0].name, "solo");
    }

    // --- closed enums (table-driven) ---

    #[test]
    fn enum_parsing_round_trips() {
        let tiers: [(Tier, &str); 3] = [
            (Tier::Lead, "lead"),
            (Tier::Senior, "senior"),
            (Tier::Junior, "junior"),
        ];
        for (exp, s) in tiers {
            assert_eq!(s.parse::<Tier>().unwrap(), exp);
        }
        let ceilings: [(Ceiling, &str); 4] = [
            (Ceiling::S, "S"),
            (Ceiling::M, "M"),
            (Ceiling::L, "L"),
            (Ceiling::Xl, "XL"),
        ];
        for (exp, s) in ceilings {
            assert_eq!(s.parse::<Ceiling>().unwrap(), exp);
        }
        let effs: [(Efficiency, &str); 3] = [
            (Efficiency::Lean, "lean"),
            (Efficiency::Std, "std"),
            (Efficiency::Heavy, "heavy"),
        ];
        for (exp, s) in effs {
            assert_eq!(s.parse::<Efficiency>().unwrap(), exp);
        }
        let backs: [(Backend, &str); 3] = [
            (Backend::Claude, "claude"),
            (Backend::Pi, "pi"),
            (Backend::Agy, "agy"),
        ];
        for (exp, s) in backs {
            assert_eq!(s.parse::<Backend>().unwrap(), exp);
        }
        assert_eq!("propose".parse::<Autonomy>().unwrap(), Autonomy::Propose);
        assert_eq!("ratchet".parse::<Autonomy>().unwrap(), Autonomy::Ratchet);
    }

    // --- invalid configs (table-driven) ---

    #[test]
    fn invalid_configs_are_rejected() {
        let cases: &[(&str, String)] = &[
            // closed enums
            ("unknown tier", entry_with("tier", "boss")),
            ("unknown ceiling", entry_with("ceiling", "XX")),
            ("unknown efficiency", entry_with("efficiency", "fast")),
            ("unknown backend", entry_with("backend", "foo")),
            ("unknown autonomy", "autonomy = \"yolo\"\n".to_string()),
            // missing required roster fields
            ("missing name", entry_without("name")),
            ("missing tier", entry_without("tier")),
            ("missing ceiling", entry_without("ceiling")),
            ("missing efficiency", entry_without("efficiency")),
            ("missing backend", entry_without("backend")),
            ("missing dispatch_id", entry_without("dispatch_id")),
            // duplicate roster name
            (
                "duplicate roster name",
                format!("{}{}", entry_with("name", "dup"), entry_with("name", "dup")),
            ),
            // wrong types inside roster entries
            (
                "non-string name",
                "[[roster]]\nname = 123\ntier = \"senior\"\nceiling = \"M\"\nefficiency = \"lean\"\nbackend = \"pi\"\ndispatch_id = \"d\"\n".to_string(),
            ),
            // unknown keys (fail closed)
            ("unknown top-level key", "wat = 1\n".to_string()),
            ("unknown scan key", "[scan]\nfoo = \"bar\"\n".to_string()),
            ("unknown budgets key", "[budgets]\nfoo = 1\n".to_string()),
            ("unknown verify key", "[verify]\nfoo = \"x\"\n".to_string()),
            ("unknown review key", "[review]\nfoo = 1\n".to_string()),
            (
                "unknown roster key",
                format!("{}extra = \"x\"\n", full_entry()),
            ),
            // wrong value types
            ("wrong type root", "[scan]\nroot = 123\n".to_string()),
            (
                "wrong type budget",
                "[budgets]\nmax_dispatches_per_cycle = \"eight\"\n".to_string(),
            ),
            (
                "wrong type always_orchestra",
                "[verify]\nalways_orchestra = \"yes\"\n".to_string(),
            ),
            (
                "wrong type review enabled",
                "[review]\nenabled = \"yes\"\n".to_string(),
            ),
            (
                "wrong type review min_tier_gap",
                "[review]\nmin_tier_gap = \"one\"\n".to_string(),
            ),
            ("wrong type autonomy", "autonomy = 1\n".to_string()),
            ("wrong type exclude", "[scan]\nexclude = \"x\"\n".to_string()),
            ("negative budget", "[budgets]\nmax_dispatches_per_cycle = -1\n".to_string()),
            // structural errors
            (
                "duplicate table",
                "[scan]\nroot = \"a\"\n[scan]\nroot = \"b\"\n".to_string(),
            ),
            (
                "array-table after table",
                "[roster]\n[[roster]]\nname = \"x\"\n".to_string(),
            ),
            (
                "table after array-table",
                format!("{}[roster]\nfoo = \"x\"\n", full_entry()),
            ),
            // syntax errors
            ("unclosed string", "autonomy = \"propose\n".to_string()),
            ("bad array element", "[scan]\nexclude = [123]\n".to_string()),
            ("missing array comma", "[scan]\nexclude = [\"a\" \"b\"]\n".to_string()),
            ("unclosed header", "[scan\n".to_string()),
            ("assignment without key", "= 1\n".to_string()),
            (
                "trailing garbage after value",
                "autonomy = \"propose\" extra\n".to_string(),
            ),
        ];
        for (label, src) in cases {
            assert_err(label, src);
        }
    }

    // --- hardcoded exclude (invariant 5) ---

    #[test]
    fn chezmoi_config_is_hardcoded_excluded() {
        assert!(HARDCODED_EXCLUDE.contains(&"chezmoi-config"));
    }

    // --- preflight ---

    #[test]
    fn preflight_detects_present_and_absent_tools() {
        let dir = TempDir::new("present");
        make_exec(&dir.path().join("bd"));
        make_exec(&dir.path().join("pi"));
        // agy, claude, orchestra, bun, harness-deck intentionally absent
        let state = TempDir::new("present-state");
        let checks = preflight_checks(
            &dir.path().display().to_string(),
            Some(state.path()),
        );
        assert!(check(&checks, "bd").ok);
        assert!(check(&checks, "pi").ok);
        assert!(!check(&checks, "agy").ok);
        assert!(!check(&checks, "claude").ok);
        assert!(!check(&checks, "orchestra").ok);
        assert!(!check(&checks, "bun").ok);
        assert!(!check(&checks, "harness-deck").ok);
        assert!(check(&checks, "state dir").ok);
    }

    #[test]
    fn preflight_state_dir_is_writable() {
        let state = TempDir::new("writable");
        let checks = preflight_checks("", Some(state.path()));
        assert!(check(&checks, "state dir").ok);
    }

    #[test]
    fn preflight_state_dir_fails_when_blocked() {
        // A path whose parent is a file cannot be created -> not writable.
        let parent = TempDir::new("blocked");
        let blocker = parent.path().join("blocker");
        std::fs::write(&blocker, b"").expect("write");
        let target = blocker.join("conductor");
        let checks = preflight_checks("", Some(&target));
        assert!(!check(&checks, "state dir").ok);
    }

    #[test]
    fn preflight_state_dir_fails_when_home_unset() {
        let checks = preflight_checks("", None);
        assert!(!check(&checks, "state dir").ok);
    }
}
