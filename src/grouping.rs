//! Groups Herdr's sidebar by project with nothing but order and one head row
//! per project, the same in the agents and the Spaces list. The head is the
//! project's own row: its home Space and its coordinator show the project
//! name in bold, the rows after them are Herdr's own. Every row a card shows
//! is that item's own content, so focus and selection light only the item.
//!
//! The bold comes from a config rule keyed on an invisible mark in the name:
//! [`HOME_MARK`] ends every home Space label, [`HEAD_MARK`] every coordinator
//! display name. A mark in the name, not a list of names in the config,
//! because a client draws another machine's rows with its own config.
//!
//! Agents sort by `$hp_group` in the agent view. Spaces follow the global
//! `space_order` setting ([`SpaceOrder`]): `grouped` (the default) moves them
//! into one block per project with `workspace.move`, anything in no project
//! last; `homes-first` puts every home Space first, then each project's other
//! Spaces, keeping a linked worktree under its main checkout; `off` never
//! moves a Space.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use serde::Deserialize;

use crate::herdr::{Agent, CALL_TIMEOUT, Herdr, Workspace};

/// The values `space_order` takes, in the order the popup offers them.
pub const SPACE_ORDERS: [&str; 3] = ["grouped", "homes-first", "off"];

/// How the ticker orders the Spaces list, across every project.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SpaceOrder {
    /// One block per project: home, then its threads' Spaces.
    #[default]
    Grouped,
    /// Every home Space first, then each project's other Spaces.
    HomesFirst,
    /// Spaces stay where the user puts them.
    Off,
}

impl SpaceOrder {
    pub fn parse(value: &str) -> Result<SpaceOrder> {
        match value.trim() {
            "grouped" => Ok(SpaceOrder::Grouped),
            "homes-first" => Ok(SpaceOrder::HomesFirst),
            "off" => Ok(SpaceOrder::Off),
            other => bail!("unknown space_order `{other}`; one of: {}", SPACE_ORDERS.join(", ")),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            SpaceOrder::Grouped => SPACE_ORDERS[0],
            SpaceOrder::HomesFirst => SPACE_ORDERS[1],
            SpaceOrder::Off => SPACE_ORDERS[2],
        }
    }
}

/// The `space_order` part of `config.toml`.
#[derive(Deserialize, Default)]
struct OrderConfig {
    #[serde(default)]
    space_order: Option<String>,
}

/// `space_order` from `<config_dir>/config.toml`: `grouped` when the file is
/// missing, does not parse or holds an unknown value.
pub fn load(config_dir: &Path) -> SpaceOrder {
    let Ok(text) = std::fs::read_to_string(config_dir.join("config.toml")) else {
        return SpaceOrder::default();
    };
    let config: OrderConfig = toml::from_str(&text).unwrap_or_default();
    config.space_order.and_then(|v| SpaceOrder::parse(&v).ok()).unwrap_or_default()
}

/// Ends every home Space label; the Space rows make a label that contains it
/// the project's bold head.
pub const HOME_MARK: char = '\u{2800}';
/// Ends every coordinator's display name; the agent rows make a name that
/// contains it the project's bold head. Zero-width, so it takes no cell.
pub const HEAD_MARK: char = '\u{200B}';
/// The group key of whatever belongs to no project: sorts after every slug.
const OTHER: &str = "~";

/// Every token this module writes.
pub fn tokens() -> Vec<String> {
    ["hp_sub", "hp_group"].map(String::from).to_vec()
}

/// Tokens of earlier layouts: the 0.2.17/0.2.18 headings and the 0.2.19
/// rails. Cleared with a pane's or Space's own tokens, and otherwise left to
/// expire (they had a TTL and have no rows any more).
pub fn legacy() -> Vec<String> {
    let mut all: Vec<String> = ["hp_top", "hp_other", "hp_note", "hp_tail", "hp", "hp_state", "hp_activity", "hp_gap", "hp_home"].map(String::from).to_vec();
    for slot in ["top", "con", "sub", "end"] {
        for rail in ["n", "w", "i", "o"] {
            all.push(format!("hp_{slot}_{rail}"));
        }
    }
    all
}

/// One agent of a project: its pane, sort key and sub-line.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PaneRow {
    pub pane: String,
    pub key: String,
    /// `review · report`, `~40% · Writing tests`, or empty.
    pub sub: String,
}

/// One project's part of a session, as its tick saw it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProjectPart {
    /// The project's agents in panel order.
    pub panes: Vec<PaneRow>,
    /// The home Space.
    pub home: String,
    /// The project's Spaces, home first, then each thread's repository Space
    /// and its own.
    pub spaces: Vec<String>,
}

/// Every project's part in one Herdr session, keyed by slug.
pub type Parts = BTreeMap<String, ProjectPart>;

/// `!` sorts before every slug character, so keys sort as their slugs do.
pub fn coordinator_key(slug: &str, pane: &str) -> String {
    format!("{slug}!0!{pane}")
}

pub fn thread_key(slug: &str, rank: u8, id: &str) -> String {
    format!("{slug}!1!{rank}!{id}")
}

/// Tokens for one card: `Some` sets, `None` clears.
pub type Tokens = Vec<(String, Option<String>)>;

#[derive(Debug, Default, PartialEq)]
pub struct Plan {
    pub agents: Vec<(String, Tokens)>,
    /// The Space order Herdr should end up with.
    pub order: Vec<String>,
}

/// The whole layout of one session. Agents: projects by slug, each in its own
/// key order, then every other agent in Herdr's order. Spaces as `order`
/// says: the same blocks, then every other Space in Herdr's order
/// (`grouped`); homes, then the rest of each project, then the others
/// (`homes-first`); or none at all (`off`).
pub fn plan(parts: &Parts, agents: &[Agent], workspaces: &[Workspace], order: SpaceOrder) -> Plan {
    let mut out = Plan::default();
    if parts.is_empty() {
        return out;
    }
    let mut owner: HashMap<&str, &PaneRow> = HashMap::new();
    for part in parts.values() {
        for row in &part.panes {
            owner.entry(row.pane.as_str()).or_insert(row);
        }
    }
    let mut rows: Vec<(String, &str, &str)> = agents
        .iter()
        .enumerate()
        .map(|(i, a)| match owner.get(a.pane_id.as_str()) {
            Some(row) => (row.key.clone(), a.pane_id.as_str(), row.sub.as_str()),
            None => (format!("{OTHER}!{i:04}"), a.pane_id.as_str(), ""),
        })
        .collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    for (key, pane, sub) in rows {
        let tokens = vec![("hp_sub".to_string(), (!sub.is_empty()).then(|| sub.to_string())), ("hp_group".to_string(), Some(key))];
        out.agents.push((pane.to_string(), tokens));
    }

    match order {
        SpaceOrder::Grouped => out.order = grouped(parts, workspaces),
        SpaceOrder::HomesFirst => out.order = homes_first(parts, workspaces),
        SpaceOrder::Off => {}
    }
    out
}

/// `grouped`: a stable sort into one block per project.
fn grouped(parts: &Parts, workspaces: &[Workspace]) -> Vec<String> {
    let mut space_owner: HashMap<&str, (&str, usize)> = HashMap::new();
    for (slug, part) in parts {
        for (i, id) in part.spaces.iter().enumerate() {
            space_owner.entry(id.as_str()).or_insert((slug.as_str(), i));
        }
    }
    let mut ordered: Vec<(&str, usize, &Workspace)> = workspaces
        .iter()
        .enumerate()
        .map(|(i, w)| match space_owner.get(w.workspace_id.as_str()) {
            Some((slug, pos)) => (*slug, *pos, w),
            None => (OTHER, i, w),
        })
        .collect();
    ordered.sort_by(|a, b| (a.0, a.1).cmp(&(b.0, b.1)));
    ordered.iter().map(|(_, _, w)| w.workspace_id.clone()).collect()
}

/// `homes-first`: every project's home Space by slug, then each project's
/// other Spaces by slug, then every other Space in Herdr's order. A linked
/// worktree Space then moves directly under its primary Space (the first
/// unlinked Space of its repository), after its siblings, as Herdr nests it.
fn homes_first(parts: &Parts, workspaces: &[Workspace]) -> Vec<String> {
    let present: HashSet<&str> = workspaces.iter().map(|w| w.workspace_id.as_str()).collect();
    let mut seen: HashSet<&str> = HashSet::new();
    let mut list: Vec<&str> = Vec::new();
    for part in parts.values() {
        let home = if part.home.is_empty() { part.spaces.first().map(String::as_str) } else { Some(part.home.as_str()) };
        if let Some(home) = home.and_then(|h| present.get(h).copied())
            && seen.insert(home)
        {
            list.push(home);
        }
    }
    for part in parts.values() {
        for id in &part.spaces {
            if let Some(id) = present.get(id.as_str()).copied()
                && seen.insert(id)
            {
                list.push(id);
            }
        }
    }
    for w in workspaces {
        if seen.insert(w.workspace_id.as_str()) {
            list.push(w.workspace_id.as_str());
        }
    }

    // Nesting: each repository's primary Space, then its linked worktrees.
    let mut primary: HashMap<&str, &str> = HashMap::new();
    for w in workspaces {
        if let Some(t) = &w.worktree
            && !t.is_linked_worktree
            && !t.repo_key.is_empty()
        {
            primary.entry(t.repo_key.as_str()).or_insert(w.workspace_id.as_str());
        }
    }
    let by_id: HashMap<&str, &Workspace> = workspaces.iter().map(|w| (w.workspace_id.as_str(), w)).collect();
    let nested_repo = |id: &str| -> Option<&str> {
        let t = by_id.get(id)?.worktree.as_ref()?;
        (t.is_linked_worktree && primary.contains_key(t.repo_key.as_str())).then_some(t.repo_key.as_str())
    };
    let mut out = Vec::with_capacity(list.len());
    for &id in &list {
        if nested_repo(id).is_some() {
            continue;
        }
        out.push(id.to_string());
        let Some(t) = by_id.get(id).and_then(|w| w.worktree.as_ref()) else {
            continue;
        };
        if t.is_linked_worktree || primary.get(t.repo_key.as_str()) != Some(&id) {
            continue;
        }
        out.extend(list.iter().filter(|x| nested_repo(x) == Some(t.repo_key.as_str())).map(|x| x.to_string()));
    }
    out
}

/// The `workspace.move` calls that turn `current` into `wanted`: each Space
/// in turn goes to its index, so at most one call per misplaced Space.
pub fn moves(current: &[String], wanted: &[String]) -> Vec<(String, usize)> {
    let mut now = current.to_vec();
    let mut out = Vec::new();
    for (i, id) in wanted.iter().enumerate() {
        let Some(at) = now.iter().position(|x| x == id) else {
            continue;
        };
        if at != i && i < now.len() {
            let moved = now.remove(at);
            now.insert(i, moved);
            out.push((id.clone(), i));
        }
    }
    out
}

/// Home Spaces whose label lacks [`HOME_MARK`] (made before 0.2.19, or
/// renamed by hand): `(id, label with the mark)`.
pub fn unmarked_homes(parts: &Parts, workspaces: &[Workspace]) -> Vec<(String, String)> {
    let homes: HashSet<&str> = parts.values().map(|p| p.home.as_str()).filter(|h| !h.is_empty()).collect();
    workspaces
        .iter()
        .filter(|w| homes.contains(w.workspace_id.as_str()))
        .filter(|w| !w.label.is_empty() && !w.label.ends_with(HOME_MARK))
        .map(|w| (w.workspace_id.clone(), format!("{}{HOME_MARK}", w.label)))
        .collect()
}

/// Tokens sent per `(kind, id)`, so an unchanged card is re-sent only to keep
/// its TTL alive.
#[derive(Default)]
pub struct Sent(HashMap<(String, String), (Tokens, Instant)>);

const RESEND: Duration = Duration::from_millis(crate::sidebar::TOKEN_TTL_MS / 3);

impl Sent {
    fn due(&mut self, key: (String, String), tokens: &Tokens) -> bool {
        match self.0.get(&key) {
            Some((last, at)) if last == tokens && at.elapsed() < RESEND => false,
            _ => {
                self.0.insert(key, (tokens.clone(), Instant::now()));
                true
            }
        }
    }
}

/// Herdr takes at most 16 tokens per report.
const PER_REPORT: usize = 16;

pub fn report(herdr: &Herdr, kind: &str, id: &str, tokens: &Tokens) {
    let ttl = crate::sidebar::TOKEN_TTL_MS.to_string();
    for chunk in tokens.chunks(PER_REPORT) {
        let mut args: Vec<String> = [kind, "report-metadata", id, "--source", crate::herdr::SOURCE, "--ttl-ms", &ttl].iter().map(|s| s.to_string()).collect();
        for (name, value) in chunk {
            match value {
                Some(value) => args.extend(["--token".to_string(), format!("{name}={value}")]),
                None => args.extend(["--clear-token".to_string(), name.clone()]),
            }
        }
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let _ = herdr.call(&args, CALL_TIMEOUT);
    }
}

/// Clears every token this module and the earlier layouts wrote.
pub fn clear(herdr: &Herdr, kind: &str, id: &str) {
    if id.is_empty() {
        return;
    }
    let all: Tokens = tokens().into_iter().chain(legacy()).map(|t| (t, None)).collect();
    report(herdr, kind, id, &all);
}

/// Lays out one session: marks home Spaces, moves Spaces as `order` says
/// (never with `off`) and writes every agent's sort key and sub-line.
/// Nothing when no project lives in the session.
pub fn apply(herdr: &Herdr, socket: &str, parts: &Parts, agents: &[Agent], order: SpaceOrder, sent: &mut Sent) {
    if parts.is_empty() {
        return;
    }
    let mut workspaces = herdr.workspace_list().unwrap_or_default();
    for (id, label) in unmarked_homes(parts, &workspaces) {
        if herdr.workspace_rename(&id, &label).is_ok()
            && let Some(w) = workspaces.iter_mut().find(|w| w.workspace_id == id)
        {
            w.label = label;
        }
    }
    let layout = plan(parts, agents, &workspaces, order);
    if order != SpaceOrder::Off {
        let current: Vec<String> = workspaces.iter().map(|w| w.workspace_id.clone()).collect();
        for (id, index) in moves(&current, &layout.order) {
            if herdr.workspace_move(&id, index).is_err() {
                break;
            }
        }
    }
    for (pane, tokens) in &layout.agents {
        if sent.due((format!("{socket} pane"), pane.clone()), tokens) {
            report(herdr, "pane", pane, tokens);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::herdr::WorkspaceWorktree;

    fn agent(pane: &str) -> Agent {
        Agent { pane_id: pane.into(), ..Agent::default() }
    }

    fn space(id: &str, repo: &str, linked: bool) -> Workspace {
        let worktree = (!repo.is_empty()).then(|| WorkspaceWorktree { repo_key: repo.into(), checkout_path: String::new(), is_linked_worktree: linked });
        Workspace { workspace_id: id.into(), label: id.into(), worktree, ..Workspace::default() }
    }

    fn row(pane: &str, key: String, sub: &str) -> PaneRow {
        PaneRow { pane: pane.into(), key, sub: sub.into() }
    }

    fn parts() -> Parts {
        let mut parts = Parts::new();
        parts.insert("beta".into(), ProjectPart { panes: vec![row("b1", coordinator_key("beta", "b1"), "")], home: "wb".into(), spaces: vec!["wb".into()] });
        parts.insert(
            "alpha".into(),
            ProjectPart {
                panes: vec![
                    row("a2", thread_key("alpha", 4, "t-0002"), "~40% · Writing"),
                    row("a1", coordinator_key("alpha", "a1"), ""),
                    row("a3", thread_key("alpha", 1, "t-0003"), "review · report"),
                ],
                home: "wa".into(),
                spaces: vec!["wa".into(), "repo".into(), "wt2".into(), "wt3".into()],
            },
        );
        parts
    }

    fn get<'a>(tokens: &'a Tokens, name: &str) -> Option<&'a str> {
        tokens.iter().find(|(n, _)| n == name).and_then(|(_, v)| v.as_deref())
    }

    #[test]
    fn agents_sort_into_projects_with_their_own_sub_lines_and_others_last() {
        let agents: Vec<Agent> = ["x1", "a2", "b1", "a3", "x2", "a1"].into_iter().map(agent).collect();
        let layout = plan(&parts(), &agents, &[], SpaceOrder::Grouped);
        let order: Vec<&str> = layout.agents.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(order, ["a1", "a3", "a2", "b1", "x1", "x2"]);
        let t: HashMap<&str, &Tokens> = layout.agents.iter().map(|(p, t)| (p.as_str(), t)).collect();
        assert_eq!(get(t["a3"], "hp_sub"), Some("review · report"));
        assert_eq!(get(t["a1"], "hp_sub"), None, "an empty sub-line is cleared, so no row is drawn");
        assert!(t["a1"].iter().any(|(n, v)| n == "hp_sub" && v.is_none()));
        // No card carries a row of ours besides its own sub-line.
        assert!(layout.agents.iter().all(|(_, t)| t.iter().all(|(n, _)| n == "hp_sub" || n == "hp_group")));
        // The sort key the agent view uses reproduces this order.
        let keys: Vec<&str> = layout.agents.iter().map(|(_, t)| get(t, "hp_group").unwrap()).collect();
        let mut sorted = keys.clone();
        sorted.sort();
        assert_eq!(keys, sorted);
    }

    #[test]
    fn spaces_move_into_project_blocks_and_others_keep_their_order_last() {
        let workspaces = vec![
            space("mine", "", false),
            space("wt3", "r", true),
            space("wb", "", false),
            space("repo", "r", false),
            space("wa", "", false),
            space("wt2", "r", true),
            space("other2", "", false),
        ];
        let layout = plan(&parts(), &[], &workspaces, SpaceOrder::Grouped);
        assert_eq!(layout.order, ["wa", "repo", "wt2", "wt3", "wb", "mine", "other2"]);
    }

    fn fixture_spaces() -> Vec<Workspace> {
        vec![
            space("mine", "", false),
            space("wt3", "r", true),
            space("wb", "", false),
            space("repo", "r", false),
            space("wa", "", false),
            space("wt2", "r", true),
            space("other2", "", false),
        ]
    }

    #[test]
    fn space_order_defaults_to_grouped() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load(dir.path()), SpaceOrder::Grouped, "no config.toml");
        let file = dir.path().join("config.toml");
        for text in ["root = \"~/p\"\n", "this is = = not toml", "space_order = \"bogus\"\n", "space_order = 3\n"] {
            std::fs::write(&file, text).unwrap();
            assert_eq!(load(dir.path()), SpaceOrder::Grouped, "{text}");
        }
        std::fs::write(&file, "root = \"~/p\"\nspace_order = \"homes-first\"\n[defaults]\nthread_profile = \"claude\"\n").unwrap();
        assert_eq!(load(dir.path()), SpaceOrder::HomesFirst);
        std::fs::write(&file, "space_order = \"off\"\n").unwrap();
        assert_eq!(load(dir.path()), SpaceOrder::Off);
        for value in SPACE_ORDERS {
            assert_eq!(SpaceOrder::parse(value).unwrap().as_str(), value);
        }
        let error = SpaceOrder::parse("sideways").unwrap_err().to_string();
        assert!(error.contains("grouped, homes-first, off"), "{error}");
    }

    #[test]
    fn homes_first_puts_every_home_first_then_threads_by_project() {
        let layout = plan(&parts(), &[], &fixture_spaces(), SpaceOrder::HomesFirst);
        assert_eq!(layout.order, ["wa", "wb", "repo", "wt2", "wt3", "mine", "other2"]);
    }

    #[test]
    fn homes_first_keeps_linked_worktrees_under_their_main_checkout() {
        let mut parts = Parts::new();
        // alpha's thread opened repo r (primary `ra`); beta's thread has a
        // worktree of r too. gamma's home is the main checkout of repo h,
        // and its thread a worktree of it.
        parts.insert("alpha".into(), ProjectPart { home: "ha".into(), spaces: vec!["ha".into(), "ra".into(), "wa1".into()], ..ProjectPart::default() });
        parts.insert("beta".into(), ProjectPart { home: "hb".into(), spaces: vec!["hb".into(), "wb1".into()], ..ProjectPart::default() });
        parts.insert("gamma".into(), ProjectPart { home: "hg".into(), spaces: vec!["hg".into(), "wg1".into()], ..ProjectPart::default() });
        let workspaces = vec![
            space("wb1", "r", true),
            space("hb", "", false),
            space("wg1", "h", true),
            space("ra", "r", false),
            space("hg", "h", false),
            space("wa1", "r", true),
            space("ha", "", false),
            space("loose", "x", true),
        ];
        let layout = plan(&parts, &[], &workspaces, SpaceOrder::HomesFirst);
        // (a) beta's worktree follows its primary in alpha's block, after
        // alpha's own; (b) gamma's worktree follows gamma's home. A worktree
        // whose primary is not open stays where it falls.
        assert_eq!(layout.order, ["ha", "hb", "hg", "wg1", "ra", "wa1", "wb1", "loose"]);
    }

    #[test]
    fn off_never_moves_spaces() {
        let workspaces = fixture_spaces();
        let layout = plan(&parts(), &[], &workspaces, SpaceOrder::Off);
        assert!(layout.order.is_empty());
        let current: Vec<String> = workspaces.iter().map(|w| w.workspace_id.clone()).collect();
        assert!(moves(&current, &layout.order).is_empty());
    }

    #[test]
    fn agents_are_the_same_in_every_order() {
        let agents: Vec<Agent> = ["x1", "a2", "b1", "a3", "x2", "a1"].into_iter().map(agent).collect();
        let grouped = plan(&parts(), &agents, &fixture_spaces(), SpaceOrder::Grouped).agents;
        assert!(!grouped.is_empty());
        for order in [SpaceOrder::HomesFirst, SpaceOrder::Off] {
            assert_eq!(plan(&parts(), &agents, &fixture_spaces(), order).agents, grouped, "{order:?}");
        }
    }

    #[test]
    fn unmarked_home_labels_get_the_mark() {
        let mut marked = space("wb", "", false);
        marked.label = format!("Beta{HOME_MARK}");
        let mut plain = space("wa", "", false);
        plain.label = "Alpha".into();
        assert_eq!(unmarked_homes(&parts(), &[plain, marked, space("repo", "r", false)]), [("wa".to_string(), format!("Alpha{HOME_MARK}"))]);
    }

    #[test]
    fn clearing_covers_every_earlier_layout_and_fits_herdrs_token_limit_in_chunks() {
        let all = legacy();
        for old in ["hp_top", "hp_top_w", "hp_end_o", "hp_con_n", "hp_sub_i", "hp_gap", "hp_home", "hp_activity"] {
            assert!(all.iter().any(|t| t == old), "{old}");
        }
        assert!(tokens().len() + all.len() > PER_REPORT, "the chunking in report() is needed");
        assert!(all.iter().all(|t| !tokens().contains(t)));
    }

    #[test]
    fn moves_are_minimal_and_reach_the_wanted_order() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert!(moves(&s(&["a", "b", "c"]), &s(&["a", "b", "c"])).is_empty());
        let current = s(&["c", "a", "b", "d"]);
        let wanted = s(&["a", "b", "c", "d"]);
        let steps = moves(&current, &wanted);
        let mut now = current.clone();
        for (id, i) in &steps {
            // Herdr's insert index is a gap in the list before removal.
            let at = now.iter().position(|x| x == id).unwrap();
            let target = if at < *i { i - 1 } else { *i };
            let moved = now.remove(at);
            now.insert(target, moved);
        }
        assert_eq!(now, wanted);
        assert_eq!(steps.len(), 2);
    }

    #[test]
    fn no_project_means_no_layout() {
        for order in [SpaceOrder::Grouped, SpaceOrder::HomesFirst, SpaceOrder::Off] {
            assert_eq!(plan(&Parts::new(), &[agent("x")], &[space("w", "", false)], order), Plan::default());
        }
    }
}
