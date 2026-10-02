//! TM4.2 operator views (plan doc 08 §2, doc 15 §3): the project, models and
//! roles, reviews and fixes, cost and operational-health views behind
//! `herdr-farm telemetry <slug> view ...` and the fleet pane's view
//! sections. Contract: docs/telemetry/operator-views.md.
//!
//! Every value is read through the TM4.1 query service
//! (`super::analytics::query::{request, run}`), never from a store directly,
//! so the CLI, its JSON and the pane show the query's own values. Each row
//! carries its basis, coverage, sample size, observation lag and projection.
//! Read-only: nothing here writes, launches, releases budget or grants
//! authority, and switching the views off (`[telemetry] views = false` in
//! `config.toml`) touches none of collection, the ticker, admission or budgets.
use super::analytics::query;
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

mod render;

pub const CONTRACT: &str = "telemetry-views.v1";
pub const SCHEMA_VERSION: u32 = 1;
/// Default drill-down page: a screenful in a Herdr pane.
pub const DEFAULT_DRILL_PAGE: u32 = 20;
/// Largest `config.toml` the switch reads (the same bound as the coordinator's control reads).
const CONFIG_LIMIT: u64 = 1024 * 1024;
const MAX_SLUG: usize = 40;

/// The five views of plan doc 08 §2 this card builds.
#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum View { Project, Models, Reviews, Cost, Health }

impl View {
    pub const ALL: [View; 5] = [View::Project, View::Models, View::Reviews, View::Cost, View::Health];
    pub fn as_str(self) -> &'static str {
        match self { View::Project => "project", View::Models => "models", View::Reviews => "reviews", View::Cost => "cost", View::Health => "health" }
    }
    /// The rows of the view, in order: `(metric, label)`. Bounded: each view is a fixed list.
    pub fn items(self) -> &'static [(&'static str, &'static str)] {
        match self {
            View::Project => &[("M01", "accepted tasks"), ("M02", "acceptance rate"), ("M06", "lead time p95"), ("M07", "attempt amplification"),
                ("M36", "integration conflict rate")],
            View::Models => &[("M15", "effective model reported"), ("M08", "input tokens"), ("M09", "output tokens"), ("M13", "usage coverage"),
                ("M41", "candidate win rate"), ("M42", "paired acceptance difference")],
            View::Reviews => &[("M20", "review completion"), ("M21", "validated unique findings"), ("M22", "proposal validation rate"),
                ("M23", "duplicate report share"), ("M25", "fix verified"), ("M26", "currently resolved"), ("M27", "reopen rate"),
                ("M29", "attribution coverage"), ("M45", "first-candidate CI pass"), ("M47", "code survival"), ("M48", "revert rate")],
            View::Cost => &[("M11", "provider-billed spend"), ("M12", "estimated spend"), ("M14", "cost coverage"), ("M04", "cost per accepted task"),
                ("M34", "coordinator overhead"), ("M37", "overlap waste share")],
            View::Health => &[("M13", "usage coverage"), ("M15", "effective model coverage"), ("M14", "cost coverage"), ("M29", "attribution coverage"),
                ("M38", "throttled time share"), ("M39", "provider error rate"), ("M40", "quota headroom at dispatch"), ("M49", "replay suite pass rate"),
                ("M50", "evidence freshness")],
        }
    }
    /// Native metrics shown again per requested agent (the `agent_kind` dimension).
    fn by_agent(self) -> &'static [&'static str] { if self == View::Models { &["M02", "M07"] } else { &[] } }
}

/// `herdr-farm telemetry <slug> view ...`
#[derive(clap::Args, Clone, Debug)]
pub struct Args {
    /// Which view: project, models, reviews, cost or health.
    #[arg(value_enum)]
    pub view: View,
    /// Print JSON (`telemetry-views.v1`) instead of text.
    #[arg(long)]
    pub json: bool,
    /// Window start, inclusive, UTC Unix ms (lane metrics read it as their `since`).
    #[arg(long)]
    pub from: Option<i64>,
    /// Window end, exclusive, UTC Unix ms (lane metrics are since-only: `n/a (window_end_unsupported)`).
    #[arg(long)]
    pub to: Option<i64>,
    /// Knowledge time (Unix ms): values from the analytics revisions recorded at or before it.
    #[arg(long)]
    pub as_of: Option<i64>,
    /// Page the identities behind one of this view's native metrics (the query service's drill-down).
    #[arg(long)]
    pub drill: Option<String>,
    /// Drill-down bucket: `numerator` (default), `denominator`, `outcome.<o>` or `excluded.<reason>`.
    #[arg(long, requires = "drill")]
    pub bucket: Option<String>,
    /// Drill-down page size, 1-500.
    #[arg(long, default_value_t = DEFAULT_DRILL_PAGE, requires = "drill")]
    pub page_size: u32,
    /// Opaque cursor from the previous drill-down page.
    #[arg(long, requires = "drill")]
    pub cursor: Option<String>,
}

/// Window and knowledge time shared by every query a view makes.
#[derive(Clone, Copy, Default, Debug)]
pub struct Window { pub from: Option<i64>, pub to: Option<i64>, pub as_of: Option<i64> }

// ---------------------------------------------------------------------------
// Switch and scope

/// `[telemetry] views` in `<config_dir>/config.toml`; default on. Read by the
/// views and the fleet pane only: collection, the ticker, admission and
/// budgets never read it.
pub fn enabled(config_dir: &Path) -> Result<bool> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    let path = config_dir.join("config.toml");
    let file = match std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(error) => return Err(error).with_context(|| format!("cannot read {}", path.display())),
    };
    anyhow::ensure!(file.metadata()?.is_file(), "{} is not a regular file", path.display());
    let mut bytes = Vec::new();
    file.take(CONFIG_LIMIT + 1).read_to_end(&mut bytes)?;
    anyhow::ensure!(bytes.len() as u64 <= CONFIG_LIMIT, "{} exceeds its {CONFIG_LIMIT}-byte limit", path.display());
    let text = String::from_utf8(bytes).map_err(|_| anyhow::anyhow!("{} is not UTF-8 (contents withheld)", path.display()))?;
    let table: toml::Table = toml::from_str(&text).map_err(|_| anyhow::anyhow!("{} does not parse (contents withheld)", path.display()))?;
    match table.get("telemetry").and_then(|t| t.get("views")) {
        None => Ok(true),
        Some(toml::Value::Boolean(on)) => Ok(*on),
        Some(_) => bail!("[telemetry] views in {} must be true or false", path.display()),
    }
}

/// The message both surfaces print when the switch is off.
pub fn disabled_message(config_dir: &Path) -> String {
    format!("telemetry views are disabled ([telemetry] views = false in {}); collection, ticker telemetry, admission and budgets are unaffected",
        config_dir.join("config.toml").display())
}

/// One project a view may read: its slug and its own directory, confined to the root.
#[derive(Clone, Debug)]
pub struct Scope { pub slug: String, pub dir: PathBuf }

/// Resolve `slug` under `root` for a read, refusing anything that could reach
/// another project: a malformed slug, a project directory or `.state` that is
/// a symlink or resolves elsewhere, or a store file that is a symlink.
pub fn scope(root: &Path, slug: &str) -> Result<Scope> {
    let mut chars = slug.chars();
    let valid = chars.next().is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-') && slug.len() <= MAX_SLUG;
    anyhow::ensure!(valid, "`{slug}` is not a valid project slug; a view reads exactly one project by its slug");
    let root = std::fs::canonicalize(root).with_context(|| format!("projects root {} is not readable", root.display()))?;
    let dir = root.join(slug);
    let meta = std::fs::symlink_metadata(&dir).with_context(|| format!("no project `{slug}` under {}", root.display()))?;
    anyhow::ensure!(meta.is_dir() && !meta.file_type().is_symlink(), "project `{slug}` is not a directory of this root (a symlink could name another project)");
    anyhow::ensure!(std::fs::canonicalize(&dir)? == dir, "project `{slug}` resolves outside its own directory");
    let state = dir.join(".state");
    let meta = std::fs::symlink_metadata(&state).with_context(|| format!("project `{slug}` has no canonical store"))?;
    anyhow::ensure!(meta.is_dir() && !meta.file_type().is_symlink(), "project `{slug}`: .state is not its own directory");
    for name in ["state.db", "telemetry.db"] {
        match std::fs::symlink_metadata(state.join(name)) {
            Ok(meta) => anyhow::ensure!(meta.is_file() && !meta.file_type().is_symlink(), "project `{slug}`: .state/{name} is not its own regular file"),
            Err(_) if name == "telemetry.db" => {}
            Err(_) => bail!("project `{slug}` has no canonical store (.state/state.db)"),
        }
    }
    Ok(Scope { slug: slug.to_owned(), dir })
}

/// Identity of the project's canonical store (`device:inode` of `state.db`):
/// what a pane handoff binds, so a handoff cannot be redirected to another project.
pub fn store_identity(scope: &Scope) -> Result<String> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::symlink_metadata(scope.dir.join(".state/state.db"))?;
    Ok(format!("{}:{}", meta.dev(), meta.ino()))
}

// ---------------------------------------------------------------------------
// Evaluation through the query service

fn query_args(metrics: Vec<String>, window: Window, by: Option<&str>) -> query::Args {
    query::Args { metrics, cohort: None, from: window.from, to: window.to, as_of: window.as_of, as_of_seq: None, by: by.map(str::to_owned),
        horizon_ms: None, drill: None, page_size: query::DEFAULT_PAGE, cursor: None, json: true }
}

fn ask(project: &Path, args: &query::Args) -> Result<Value> { query::run(project, &query::request(args)?) }

/// The query outputs a set of views needs: one request for every row metric,
/// one for the per-agent cells. The pane asks for all five views at once, so
/// it makes the same two requests a single CLI view makes.
pub struct Answers { main: Value, by_agent: Option<Value> }

pub fn answer(project: &Path, views: &[View], window: Window) -> Result<Answers> {
    let mut metrics: Vec<String> = Vec::new();
    let mut agents: Vec<String> = Vec::new();
    for view in views {
        for (id, _) in view.items() { if !metrics.iter().any(|m| m == id) { metrics.push((*id).to_owned()); } }
        for id in view.by_agent() { if !agents.iter().any(|m| m == id) { agents.push((*id).to_owned()); } }
    }
    let main = ask(project, &query_args(metrics, window, None))?;
    let by_agent = if agents.is_empty() { None } else { Some(ask(project, &query_args(agents, window, Some("agent_kind")))?) };
    Ok(Answers { main, by_agent })
}

fn result<'a>(out: &'a Value, metric: &str) -> Option<&'a Value> {
    out["results"].as_array()?.iter().find(|r| r["metric_id"] == metric)
}

/// One view's JSON body (`telemetry-views.v1`), built from the query answers.
pub fn body(view: View, slug: &str, answers: &Answers, window: Window) -> Value {
    let rows: Vec<Value> = view.items().iter().filter_map(|(id, label)| result(&answers.main, id).map(|r| render::row(r, label))).collect();
    let mut out = json!({"schema_version": SCHEMA_VERSION, "contract": CONTRACT, "view": view.as_str(), "project": slug,
        "window": {"from_unix_ms": window.from, "to_unix_ms": window.to}, "as_of_unix_ms": window.as_of,
        "query": {"contract": answers.main["contract"], "request": answers.main["request"], "query_unix_ms": answers.main["query_unix_ms"]},
        "rows": rows});
    match view {
        View::Models => {
            let cells: Vec<Value> = answers.by_agent.iter().flat_map(|out| view.by_agent().iter().filter_map(move |id| result(out, id)))
                .map(render::agent_cells).collect();
            out["by_agent"] = json!(cells);
            out["identity"] = render::identity(result(&answers.main, "M15"), answers.by_agent.as_ref().and_then(|o| result(o, "M02")));
        }
        View::Reviews => out["fixes"] = render::fixes(result(&answers.main, "M25"), result(&answers.main, "M27"), result(&answers.main, "M26")),
        View::Health => {
            // Only this view's own results, so the pane (one union query) prints what the CLI prints.
            let own: Vec<Value> = view.items().iter().filter_map(|(id, _)| result(&answers.main, id).cloned()).collect();
            out["sources"] = render::sources(&own);
        }
        _ => {}
    }
    out
}

/// The text rows of one view (also the pane's section for it).
pub fn rows_text(body: &Value) -> String { render::rows_text(body) }

/// One drill-down page through the query service for `metric` of `view`.
pub fn drill(project: &Path, view: View, metric: &str, bucket: Option<&str>, page_size: u32, cursor: Option<&str>, window: Window) -> Result<Value> {
    if !view.items().iter().any(|(id, _)| *id == metric) && !view.by_agent().contains(&metric) {
        bail!("metric `{metric}` is not part of the {} view (its metrics: {})", view.as_str(),
            view.items().iter().map(|(id, _)| *id).chain(view.by_agent().iter().copied()).collect::<Vec<_>>().join(", "));
    }
    let mut args = query_args(vec![metric.to_owned()], window, None);
    args.drill = Some(bucket.unwrap_or("numerator").to_owned());
    args.page_size = page_size;
    args.cursor = cursor.map(str::to_owned);
    ask(project, &args)
}

/// `telemetry <slug> view ...`: the switch, the scope, then one view (or one drill-down page).
pub fn run(root: &Path, slug: &str, config_dir: &Path, args: &Args) -> Result<String> {
    if !enabled(config_dir)? { bail!("{}", disabled_message(config_dir)); }
    let scope = scope(root, slug)?;
    let window = Window { from: args.from, to: args.to, as_of: args.as_of };
    if let Some(metric) = &args.drill {
        let out = drill(&scope.dir, args.view, metric, args.bucket.as_deref(), args.page_size, args.cursor.as_deref(), window)?;
        let label = args.view.items().iter().find(|(id, _)| *id == metric.as_str()).map_or(metric.as_str(), |(_, label)| *label);
        let body = json!({"schema_version": SCHEMA_VERSION, "contract": CONTRACT, "view": args.view.as_str(), "project": scope.slug,
            "row": render::row(&out["results"][0], label), "drill": out["drill"],
            "query": {"contract": out["contract"], "request": out["request"], "query_unix_ms": out["query_unix_ms"]}});
        return Ok(if args.json { serde_json::to_string_pretty(&body)? + "\n" } else { render::drill_text(&body) });
    }
    let answers = answer(&scope.dir, &[args.view], window)?;
    let body = body(args.view, &scope.slug, &answers, window);
    Ok(if args.json { serde_json::to_string_pretty(&body)? + "\n" } else { render::header(&body) + &render::rows_text(&body) })
}

/// The fleet pane's view sections for one project: every view, through the
/// same two query requests, each row line exactly as `telemetry <slug> view` prints it.
pub fn pane(project: &Path, slug: &str) -> Result<String> {
    let answers = answer(project, &View::ALL, Window::default())?;
    let mut out = format!("views (as `telemetry {slug} view <name>`; live, all time)\n");
    for view in View::ALL {
        let body = body(view, slug, &answers, Window::default());
        out += &format!(" {}\n", view.as_str());
        out += &render::rows_text(&body);
    }
    Ok(out)
}
