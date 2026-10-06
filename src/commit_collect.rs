//! Commit selection and PR baseline resolution; suite acquisition stays shared.
use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::cli::{BaseArgs, CommitArgs, CommitOptions};
use crate::config::{ConfigOverride, ResolvedConfig};
use crate::error::AppError;
use crate::gha_collect::{
    self, CollectProgress, CollectResult, PinnedRun, ProgressEvent, StatusCheck, StatusIdentity,
    StatusSnapshot,
};
use crate::model::Suite;

const REPOSITORY: &str = "CUBRID/cubrid";
const MAX_API_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BaselineIdentity {
    pub pr_number: u64,
    pub pr_url: String,
    pub head_ref: String,
    pub head_sha: String,
    pub target_ref: String,
    pub target_sha: String,
    pub merge_base: String,
    pub observed_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CommitSelection {
    pub observed_at: DateTime<Utc>,
    pub mode: String,
    pub coverage: String,
    pub commit: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub baseline: Option<BaselineIdentity>,
    pub candidates: Vec<Candidate>,
    pub runs: BTreeMap<u64, Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requested_attempt: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Candidate {
    pub suite: Suite,
    pub source: String,
    pub id: u64,
    pub run_id: u64,
    pub reported_at: DateTime<Utc>,
    pub state: String,
    pub detail_url: String,
}

impl CommitSelection {
    pub(crate) fn pinned_run(&self, run_id: u64) -> Result<PinnedRun, AppError> {
        let run = self
            .runs
            .get(&run_id)
            .ok_or_else(|| AppError::Integrity("selected run metadata missing".to_owned()))?;
        Ok(PinnedRun {
            attempt: self
                .requested_attempt
                .unwrap_or(number(run, "run_attempt")?),
            json: Arc::new(serde_json::to_vec(run)?),
            context: Arc::new(Mutex::new(None)),
        })
    }

    pub(crate) fn verify_runs(&self) -> Result<(), AppError> {
        for (id, pinned) in &self.runs {
            let current = load_run(*id)?;
            if current["run_attempt"] != pinned["run_attempt"]
                || current["status"] != pinned["status"]
            {
                return Err(AppError::Integrity(format!(
                    "run {id} changed attempt or state during collection; retry with a fresh observation"
                )));
            }
        }
        Ok(())
    }

    pub(crate) fn is_old_attempt(&self, run_id: u64) -> bool {
        self.requested_attempt.is_some_and(|attempt| {
            self.runs
                .get(&run_id)
                .and_then(|run| run["run_attempt"].as_u64())
                .is_some_and(|current| attempt < current)
        })
    }
}

pub async fn run(
    args: &CommitArgs,
    progress: &dyn CollectProgress,
) -> Result<CollectResult, AppError> {
    progress.report(ProgressEvent::Starting {
        total: Suite::ALL
            .into_iter()
            .filter(|suite| args.options.suite.is_empty() || args.options.suite.contains(suite))
            .count(),
    });
    let commit = gha_collect::validate_full_sha(&args.sha)?;
    collect(&commit, &args.options, None, progress).await
}

pub async fn run_base(
    args: &BaseArgs,
    progress: &dyn CollectProgress,
) -> Result<CollectResult, AppError> {
    progress.report(ProgressEvent::Starting {
        total: Suite::ALL
            .into_iter()
            .filter(|suite| args.options.suite.is_empty() || args.options.suite.contains(suite))
            .count(),
    });
    let pr_url = gha_collect::canonical_pr(&args.pr)?;
    let pr_number: u64 = pr_url
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .parse()
        .map_err(|_| AppError::Input("invalid PR number".to_owned()))?;
    if pr_number == 0 {
        return Err(AppError::Input("PR number must be positive".to_owned()));
    }
    let observed_at = Utc::now();
    let pull = api(&format!("repos/{REPOSITORY}/pulls/{pr_number}"), false)?;
    if number(&pull, "number")? != pr_number
        || pull["base"]["repo"]["full_name"].as_str() != Some(REPOSITORY)
    {
        return Err(AppError::Integrity(
            "PR repository/number mismatch".to_owned(),
        ));
    }
    let head_sha = gha_collect::validate_full_sha(string(&pull["head"], "sha")?)?;
    let target_sha = gha_collect::validate_full_sha(string(&pull["base"], "sha")?)?;
    let config = ResolvedConfig::load(ConfigOverride {
        data_dir: args.options.data_dir.clone(),
        artifact_base: args.options.artifact_base.clone(),
        fetch_concurrency: args.options.fetch_concurrency,
    })?;
    let cache = args
        .git_cache
        .clone()
        .unwrap_or_else(|| config.data_dir.join("git-cache/CUBRID-cubrid.git"));
    let receipts = config.data_dir.join("baseline-resolutions");
    std::fs::create_dir_all(&receipts).map_err(|error| AppError::storage(&receipts, error))?;
    let receipt = tempfile::Builder::new()
        .prefix("resolution-")
        .tempdir_in(&receipts)
        .map_err(|error| AppError::storage(&receipts, error))?
        .keep();
    crate::storage::write_json_immutable(
        &receipt.join("request.json"),
        &json!({
            "repository": REPOSITORY, "pr_number": pr_number, "pr_url": pr_url,
            "head_sha": head_sha, "head_ref": string(&pull["head"], "ref")?,
            "target_sha": target_sha, "target_ref": string(&pull["base"], "ref")?, "observed_at": observed_at,
        }),
    )?;
    let merge_base = match resolve_base(&cache, &target_sha, &head_sha) {
        Ok(base) => base,
        Err(error) => {
            crate::storage::write_json_immutable(
                &receipt.join("result.json"),
                &json!({
                    "ok": false, "kind": error.kind().as_str(), "error": error.to_string(),
                }),
            )?;
            return Err(AppError::Baseline {
                receipt: receipt.join("request.json"),
                source: Box::new(error),
            });
        }
    };
    crate::storage::write_json_immutable(
        &receipt.join("result.json"),
        &json!({"ok": true, "merge_base": merge_base}),
    )?;
    let baseline = BaselineIdentity {
        pr_number,
        pr_url,
        head_ref: string(&pull["head"], "ref")?.to_owned(),
        head_sha,
        target_ref: string(&pull["base"], "ref")?.to_owned(),
        target_sha,
        merge_base: merge_base.clone(),
        observed_at,
    };
    collect(&merge_base, &args.options, Some(baseline), progress).await
}

async fn collect(
    commit: &str,
    options: &CommitOptions,
    baseline: Option<BaselineIdentity>,
    progress: &dyn CollectProgress,
) -> Result<CollectResult, AppError> {
    let observed_at = Utc::now();
    // Verify that an arbitrary SHA names a real Engine commit before recording absence.
    let commit_record = api(&format!("repos/{REPOSITORY}/commits/{commit}"), false)?;
    if commit_record["sha"].as_str() != Some(commit) {
        return Err(AppError::Integrity(
            "commit endpoint returned a different SHA".to_owned(),
        ));
    }
    let suites: Vec<_> = Suite::ALL
        .into_iter()
        .filter(|suite| options.suite.is_empty() || options.suite.contains(suite))
        .collect();
    let mut candidates = Vec::new();
    let mut runs = BTreeMap::new();
    if let Some(run_id) = options.run_id {
        let run = load_run(run_id)?;
        let attempt = options.attempt.unwrap_or(number(&run, "run_attempt")?);
        if attempt > number(&run, "run_attempt")? {
            return Err(AppError::Input(
                "requested attempt is newer than the observed run".to_owned(),
            ));
        }
        for suite in &suites {
            candidates.push(Candidate {
                suite: *suite,
                source: "explicit_run".to_owned(),
                id: run_id,
                run_id,
                reported_at: timestamp(&run, "updated_at")?,
                state: "UNREPORTED".to_owned(),
                detail_url: format!("https://github.com/{REPOSITORY}/actions/runs/{run_id}"),
            });
        }
        runs.insert(run_id, run);
    } else {
        let statuses = api(
            &format!("repos/{REPOSITORY}/commits/{commit}/statuses?per_page=100"),
            true,
        )?;
        let checks = api(
            &format!("repos/{REPOSITORY}/commits/{commit}/check-runs?per_page=100&filter=all"),
            true,
        )?;
        candidates = discover(&statuses, &checks, commit)?;
    }
    let mut checks = Vec::new();
    for suite in suites {
        if let Some(candidate) = latest(&candidates, suite) {
            if let std::collections::btree_map::Entry::Vacant(entry) = runs.entry(candidate.run_id)
            {
                entry.insert(load_run(candidate.run_id)?);
            }
            let run = &runs[&candidate.run_id];
            // A rerun may have started without a fresh commit status yet.
            let state = if options
                .attempt
                .is_some_and(|attempt| attempt < run["run_attempt"].as_u64().unwrap_or(0))
            {
                candidate.state.clone()
            } else if run["status"] != "completed" {
                "PENDING".to_owned()
            } else {
                candidate.state.clone()
            };
            checks.push(StatusCheck {
                name: format!("gha-ci: {}", suite.job_name()),
                provider: "GitHub Actions".to_owned(),
                freshness: "current".to_owned(),
                current: Some(StatusIdentity {
                    state,
                    reported_for_sha: commit.to_owned(),
                    detail_url: candidate.detail_url.clone(),
                    reported_at: candidate.reported_at,
                }),
            });
        }
    }
    let selection = CommitSelection {
        observed_at,
        mode: if options.run_id.is_some() {
            "explicit_run"
        } else {
            "exact_commit_links"
        }
        .to_owned(),
        coverage: if options.run_id.is_some() {
            "explicit run only; Engine identity requires suite validation"
        } else {
            "all paginated exact-commit statuses and check runs; no repository-wide workflow scan"
        }
        .to_owned(),
        commit: commit.to_owned(),
        baseline,
        candidates,
        runs,
        requested_attempt: options.attempt,
    };
    let raw_json = serde_json::to_vec(&selection)?;
    let snapshot = StatusSnapshot {
        repository: REPOSITORY.to_owned(),
        pr: None,
        checks,
        raw_json,
        selection: Some(selection),
    };
    gha_collect::collect_selected(
        &options.collection_args(),
        snapshot,
        commit.to_owned(),
        progress,
    )
    .await
}

fn discover(
    status_pages: &Value,
    check_pages: &Value,
    commit: &str,
) -> Result<Vec<Candidate>, AppError> {
    let mut candidates = Vec::new();
    for page in array(status_pages)? {
        for record in array(page)? {
            let Some(suite) = suite_named(record["context"].as_str().unwrap_or_default()) else {
                continue;
            };
            let detail_url = string(record, "target_url")?.to_owned();
            candidates.push(Candidate {
                suite,
                source: "commit_status".to_owned(),
                id: number(record, "id")?,
                run_id: checked_run_url(&detail_url)?,
                detail_url,
                reported_at: timestamp(record, "created_at")?,
                state: string(record, "state")?.to_ascii_uppercase(),
            });
        }
    }
    for page in array(check_pages)? {
        for record in array(&page["check_runs"])? {
            let Some(suite) = suite_named(record["name"].as_str().unwrap_or_default()) else {
                continue;
            };
            if record["head_sha"].as_str() != Some(commit)
                || record["app"]["slug"] != "github-actions"
            {
                continue;
            }
            let detail_url = string(record, "details_url")?.to_owned();
            let state = if record["status"] != "completed" {
                "PENDING"
            } else if record["conclusion"] == "success" {
                "SUCCESS"
            } else {
                "FAILURE"
            };
            let time = record
                .get("completed_at")
                .filter(|v| !v.is_null())
                .or_else(|| record.get("started_at"))
                .ok_or_else(|| AppError::Malformed("check timestamp is missing".to_owned()))?;
            candidates.push(Candidate {
                suite,
                source: "check_run".to_owned(),
                id: number(record, "id")?,
                run_id: checked_run_url(&detail_url)?,
                detail_url,
                reported_at: serde_json::from_value(time.clone())?,
                state: state.to_owned(),
            });
        }
    }
    // Deterministic ordering for retained evidence, independent of API page order.
    candidates.sort_by(|a, b| {
        (a.suite.job_name(), a.run_id, a.reported_at, &a.source, a.id).cmp(&(
            b.suite.job_name(),
            b.run_id,
            b.reported_at,
            &b.source,
            b.id,
        ))
    });
    Ok(candidates)
}

fn latest(candidates: &[Candidate], suite: Suite) -> Option<&Candidate> {
    // Run IDs order executions; late status updates on old runs must not replace newer runs.
    candidates
        .iter()
        .filter(|item| item.suite == suite)
        .max_by_key(|item| (item.run_id, item.reported_at, &item.source, item.id))
}

fn suite_named(name: &str) -> Option<Suite> {
    Suite::ALL
        .into_iter()
        .find(|suite| name == format!("gha-ci: {}", suite.job_name()))
}

fn checked_run_url(value: &str) -> Result<u64, AppError> {
    let url =
        url::Url::parse(value).map_err(|_| AppError::Integrity("invalid run URL".to_owned()))?;
    let path = url.path().to_ascii_lowercase();
    if !path.starts_with("/cubrid/cubrid/actions/runs/")
        || url.query().is_some()
        || url.fragment().is_some()
        || !url.username().is_empty()
    {
        return Err(AppError::Integrity(
            "status run URL does not identify CUBRID/cubrid".to_owned(),
        ));
    }
    gha_collect::actions_run_id(value)
}

fn load_run(run_id: u64) -> Result<Value, AppError> {
    let run = api(&format!("repos/{REPOSITORY}/actions/runs/{run_id}"), false)?;
    if number(&run, "id")? != run_id
        || number(&run, "run_attempt")? == 0
        || run["repository"]["full_name"].as_str() != Some(REPOSITORY)
        || run["path"].as_str() != Some(".github/workflows/gha-ci.yml")
        || !matches!(
            run["status"].as_str(),
            Some("queued" | "in_progress" | "completed" | "waiting" | "pending" | "requested")
        )
    {
        return Err(AppError::Integrity(
            "run metadata is not the selected CUBRID gha-ci execution".to_owned(),
        ));
    }
    Ok(run)
}

fn array(value: &Value) -> Result<&Vec<Value>, AppError> {
    value
        .as_array()
        .ok_or_else(|| AppError::Malformed("expected paginated JSON array".to_owned()))
}
fn string<'a>(value: &'a Value, key: &str) -> Result<&'a str, AppError> {
    value[key]
        .as_str()
        .ok_or_else(|| AppError::Malformed(format!("missing string field {key}")))
}
fn number(value: &Value, key: &str) -> Result<u64, AppError> {
    value[key]
        .as_u64()
        .ok_or_else(|| AppError::Malformed(format!("missing integer field {key}")))
}
fn timestamp(value: &Value, key: &str) -> Result<DateTime<Utc>, AppError> {
    serde_json::from_value(value[key].clone())
        .map_err(|_| AppError::Malformed(format!("invalid timestamp {key}")))
}

fn api(endpoint: &str, paginate: bool) -> Result<Value, AppError> {
    let mut command = Command::new("gh");
    command.args(["api", "--method", "GET", endpoint]);
    if paginate {
        command.args(["--paginate", "--slurp"]);
    }
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| AppError::Remote(format!("execute gh: {error}")))?;
    let mut bytes = Vec::new();
    let read = child
        .stdout
        .take()
        .unwrap()
        .take((MAX_API_BYTES + 1) as u64)
        .read_to_end(&mut bytes);
    if read.is_err() || bytes.len() > MAX_API_BYTES {
        let _ = child.kill();
        let _ = child.wait();
        return Err(AppError::Oversized(format!(
            "GitHub response for {endpoint} unavailable or exceeds {MAX_API_BYTES} bytes; discovery coverage incomplete"
        )));
    }
    let status = child
        .wait()
        .map_err(|error| AppError::Remote(format!("wait for gh: {error}")))?;
    if !status.success() {
        return Err(AppError::Remote(format!(
            "GET {endpoint} failed ({status}); discovery coverage incomplete"
        )));
    }
    serde_json::from_slice(&bytes).map_err(|source| AppError::Json {
        endpoint: endpoint.to_owned(),
        source,
    })
}

fn git(cache: &Path, args: &[&str]) -> Result<String, AppError> {
    let output = Command::new("git")
        .arg("--git-dir")
        .arg(cache)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .output()
        .map_err(|error| AppError::Unavailable(format!("run git: {error}")))?;
    if !output.status.success() {
        return Err(AppError::Unavailable(format!(
            "Git {} failed ({}); merge-base history unresolved",
            args[0], output.status
        )));
    }
    String::from_utf8(output.stdout)
        .map_err(|_| AppError::Malformed("Git returned non-UTF-8 output".to_owned()))
}

pub(crate) fn resolve_base(cache: &Path, target: &str, head: &str) -> Result<String, AppError> {
    if !cache.exists() {
        std::fs::create_dir_all(cache).map_err(|error| AppError::storage(cache, error))?;
        git(cache, &["init", "--bare"])?;
    }
    if git(cache, &["rev-parse", "--is-bare-repository"])?.trim() != "true" {
        return Err(AppError::Input(
            "Git cache must be a dedicated bare repository".to_owned(),
        ));
    }
    let shallow = git(cache, &["rev-parse", "--is-shallow-repository"])?;
    let present = [target, head]
        .iter()
        .all(|sha| git(cache, &["cat-file", "-e", &format!("{sha}^{{commit}}")]).is_ok());
    if !present || shallow.trim() == "true" {
        let mut args = vec!["fetch", "--no-tags", "--no-write-fetch-head"];
        if shallow.trim() == "true" {
            args.push("--unshallow");
        }
        args.extend(["https://github.com/CUBRID/cubrid.git", target, head]);
        git(cache, &args)?;
    }
    if git(cache, &["rev-parse", "--is-shallow-repository"])?.trim() != "false" {
        return Err(AppError::Unavailable(
            "Git cache is still shallow; merge base unresolved".to_owned(),
        ));
    }
    let bases = git(cache, &["merge-base", "--all", target, head])?;
    let bases: Vec<_> = bases.split_whitespace().collect();
    if bases.len() != 1 {
        return Err(AppError::Unavailable(format!(
            "expected one merge base, found {}",
            bases.len()
        )));
    }
    gha_collect::validate_full_sha(bases[0])
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA: &str = "1111111111111111111111111111111111111111";

    #[test]
    fn discovery_paginates_and_never_lets_late_old_run_status_replace_new_run() {
        let status = |id, run, time, state| json!({"id":id,"context":"gha-ci: test_sql","target_url":format!("https://github.com/CUBRID/cubrid/actions/runs/{run}"),"created_at":time,"state":state});
        let pages = json!([
            [status(2, 200, "2026-10-06T00:00:00Z", "pending")],
            [status(3, 100, "2026-10-07T00:00:00Z", "success")]
        ]);
        let candidates = discover(&pages, &json!([{"check_runs":[]}]), SHA).unwrap();
        let selected = latest(&candidates, Suite::Sql).unwrap();
        assert_eq!(selected.run_id, 200);
        assert_eq!(selected.state, "PENDING");
        assert_eq!(candidates.len(), 2);
    }

    #[test]
    fn checks_require_exact_engine_link_and_github_actions_app() {
        let record = json!({"name":"gha-ci: test_sql","head_sha":SHA,"app":{"slug":"github-actions"},"details_url":"https://github.com/CUBRID/cubrid/actions/runs/456","id":99,"status":"completed","conclusion":"failure","completed_at":"2026-10-06T00:00:00Z"});
        let mut unrelated = record.clone();
        unrelated["head_sha"] = json!("2222222222222222222222222222222222222222");
        let mut third_party = record.clone();
        third_party["app"]["slug"] = json!("other");
        let candidates = discover(
            &json!([[]]),
            &json!([{"check_runs":[unrelated,third_party,record]}]),
            SHA,
        )
        .unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].run_id, 456);
        assert!(checked_run_url("https://github.com/other/repo/actions/runs/456").is_err());
    }

    #[test]
    fn missing_links_and_malformed_pages_are_distinct() {
        assert!(
            discover(&json!([[]]), &json!([{"check_runs":[]}]), SHA)
                .unwrap()
                .is_empty()
        );
        assert!(discover(&json!([[]]), &json!([{}]), SHA).is_err());
    }

    fn bare_graph() -> (tempfile::TempDir, String) {
        let directory = tempfile::tempdir().unwrap();
        git(directory.path(), &["init", "--bare"]).unwrap();
        git(directory.path(), &["config", "user.name", "CI test"]).unwrap();
        git(
            directory.path(),
            &["config", "user.email", "ci@example.invalid"],
        )
        .unwrap();
        let tree = git(directory.path(), &["mktree"])
            .unwrap()
            .trim()
            .to_owned();
        (directory, tree)
    }
    fn commit(cache: &Path, tree: &str, name: &str, parents: &[&str]) -> String {
        let mut args = vec!["commit-tree", tree, "-m", name];
        for parent in parents {
            args.extend(["-p", parent]);
        }
        git(cache, &args).unwrap().trim().to_owned()
    }

    #[test]
    fn git_resolves_pinned_commits_instead_of_moving_target_tip() {
        let (cache, tree) = bare_graph();
        let base = commit(cache.path(), &tree, "base", &[]);
        let target = commit(cache.path(), &tree, "target", &[&base]);
        let head = commit(cache.path(), &tree, "head", &[&base]);
        assert_eq!(resolve_base(cache.path(), &target, &head).unwrap(), base);
        assert_ne!(base, target);
    }

    #[test]
    fn missing_objects_are_fetched_into_dedicated_cache_without_writing_refs() {
        let (source, tree) = bare_graph();
        let base = commit(source.path(), &tree, "base", &[]);
        let target = commit(source.path(), &tree, "target", &[&base]);
        let head = commit(source.path(), &tree, "head", &[&base]);
        let (cache, _) = bare_graph();
        let rewrite = format!("url.file://{}.insteadOf", source.path().display());
        git(
            cache.path(),
            &["config", &rewrite, "https://github.com/CUBRID/cubrid.git"],
        )
        .unwrap();
        assert_eq!(resolve_base(cache.path(), &target, &head).unwrap(), base);
        assert!(!cache.path().join("FETCH_HEAD").exists());
        assert!(
            git(cache.path(), &["for-each-ref"])
                .unwrap()
                .trim()
                .is_empty()
        );
    }

    #[test]
    fn git_preserves_multiple_base_and_unrelated_history_ambiguity() {
        let (cache, tree) = bare_graph();
        let root = commit(cache.path(), &tree, "root", &[]);
        let a = commit(cache.path(), &tree, "a", &[&root]);
        let b = commit(cache.path(), &tree, "b", &[&root]);
        let left = commit(cache.path(), &tree, "left", &[&a, &b]);
        let right = commit(cache.path(), &tree, "right", &[&b, &a]);
        let error = resolve_base(cache.path(), &left, &right).unwrap_err();
        assert!(error.to_string().contains("found 2"));
        let unrelated = commit(cache.path(), &tree, "unrelated", &[]);
        assert!(resolve_base(cache.path(), &left, &unrelated).is_err());
    }
}
