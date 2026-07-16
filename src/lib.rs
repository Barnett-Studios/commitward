use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub mod gitdiff;

/// Raw deserialized checkpoint entry from a checkpoints.yaml file.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Checkpoint {
    pub name: String,
    pub summary: String,
    #[serde(default)]
    pub standards_doc: Option<String>,
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default)]
    pub content: Vec<String>,
    #[serde(default)]
    pub content_exempt_paths: Vec<String>,
    #[serde(default)]
    pub semantic: Option<String>,
}

/// Compiled matching mode for a checkpoint.
pub enum Mode {
    Path(Vec<regex::Regex>),
    Content {
        patterns: Vec<regex::Regex>,
        exempt: Vec<regex::Regex>,
    },
    Semantic(SemanticKind),
}

/// Discriminant for code-driven semantic checks.
pub enum SemanticKind {
    CheckpointRemoved,
}

/// A checkpoint with all patterns compiled and mode resolved.
pub struct CompiledCheckpoint {
    pub name: String,
    pub summary: String,
    pub standards_doc: Option<String>,
    pub mode: Mode,
}

#[derive(Debug, thiserror::Error)]
pub enum CheckpointError {
    #[error("{0}: {1}")]
    Parse(PathBuf, String),
    #[error("checkpoint '{0}': pattern: {1}")]
    Regex(String, regex::Error),
    #[error("checkpoint '{0}': unknown semantic '{1}'")]
    UnknownSemantic(String, String),
    #[error("checkpoint '{0}': set exactly one of paths|content|semantic")]
    AmbiguousMode(String),
}

/// A file entry produced by `git diff --name-status`.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct FileEntry {
    /// Status character from git (e.g. 'M', 'A', 'D', 'R', …).
    pub status: char,
    /// Repository-relative path of the file.
    pub path: String,
}

/// One fired checkpoint: which checkpoint fired and which paths/names matched.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Fired {
    pub name: String,
    pub summary: String,
    pub matched: Vec<String>,
}

/// Run all compiled checkpoints against the provided diff information.
///
/// - **Path** mode: `matched` = files whose `path` matches any compiled regex.
/// - **Content** mode: for each file whose path is not matched by any `exempt`
///   regex, if any entry in `added_lines[path]` matches any content pattern →
///   include the file path in `matched`.
/// - **Semantic `CheckpointRemoved`**: fires only when `base_checkpoint_names`
///   is `Some`, at least one file path ends with `checkpoints.yaml`, and the
///   set difference `base_names − current_names` is non-empty. `matched` = the
///   removed names.
///
/// Returns one `Fired` per checkpoint with a non-empty match list.
pub fn detect(
    checkpoints: &[CompiledCheckpoint],
    files: &[FileEntry],
    added_lines: &HashMap<String, Vec<String>>,
    base_checkpoint_names: Option<&[String]>,
) -> Vec<Fired> {
    let current_names: std::collections::HashSet<&str> =
        checkpoints.iter().map(|c| c.name.as_str()).collect();

    checkpoints
        .iter()
        .filter_map(|cp| {
            let matched = match &cp.mode {
                Mode::Path(patterns) => files
                    .iter()
                    .filter(|f| patterns.iter().any(|re| re.is_match(&f.path)))
                    .map(|f| f.path.clone())
                    .collect::<Vec<_>>(),

                Mode::Content { patterns, exempt } => files
                    .iter()
                    .filter(|f| !exempt.iter().any(|re| re.is_match(&f.path)))
                    .filter_map(|f| {
                        let lines = added_lines.get(&f.path)?;
                        let hit = lines
                            .iter()
                            .any(|line| patterns.iter().any(|re| re.is_match(line)));
                        if hit {
                            Some(f.path.clone())
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>(),

                Mode::Semantic(SemanticKind::CheckpointRemoved) => match base_checkpoint_names {
                    None => vec![],
                    Some(base_names) => {
                        let has_registry_touch =
                            files.iter().any(|f| f.path.ends_with("checkpoints.yaml"));
                        if !has_registry_touch {
                            vec![]
                        } else {
                            base_names
                                .iter()
                                .filter(|n| !current_names.contains(n.as_str()))
                                .cloned()
                                .collect()
                        }
                    }
                },
            };

            if matched.is_empty() {
                None
            } else {
                Some(Fired {
                    name: cp.name.clone(),
                    summary: cp.summary.clone(),
                    matched,
                })
            }
        })
        .collect()
}

/// Wrapper matching the top-level YAML structure.
#[derive(serde::Deserialize)]
struct CheckpointsFile {
    #[allow(dead_code)]
    version: String,
    checkpoints: Vec<Checkpoint>,
}

/// Load checkpoints from a YAML file.
///
/// Returns `Ok(vec![])` when the file is absent. Returns `Err(Parse)` on
/// malformed YAML or unexpected structure.
pub fn load_checkpoints(path: &Path) -> Result<Vec<Checkpoint>, CheckpointError> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(CheckpointError::Parse(path.to_path_buf(), e.to_string())),
    };
    let file: CheckpointsFile = serde_yaml::from_str(&text)
        .map_err(|e| CheckpointError::Parse(path.to_path_buf(), e.to_string()))?;
    Ok(file.checkpoints)
}

/// Merge global and repo-local checkpoint lists.
///
/// The union is keyed by name; a repo entry with the same name as a global
/// entry replaces the global one. Order: global entries first (preserving
/// order), then repo entries whose name did not appear in global.
pub fn merge(global: Vec<Checkpoint>, repo: Vec<Checkpoint>) -> Vec<Checkpoint> {
    let mut out: Vec<Checkpoint> = Vec::with_capacity(global.len() + repo.len());
    // Collect repo entries that override by name.
    let repo_names: std::collections::HashMap<&str, usize> = repo
        .iter()
        .enumerate()
        .map(|(i, cp)| (cp.name.as_str(), i))
        .collect();

    for cp in &global {
        if let Some(&idx) = repo_names.get(cp.name.as_str()) {
            out.push(repo[idx].clone());
        } else {
            out.push(cp.clone());
        }
    }
    // Append repo entries not present in global.
    let global_names: std::collections::HashSet<&str> =
        global.iter().map(|cp| cp.name.as_str()).collect();
    for cp in repo {
        if !global_names.contains(cp.name.as_str()) {
            out.push(cp);
        }
    }
    out
}

/// Compile raw checkpoints into their matched representations.
///
/// Each checkpoint must declare exactly one of `paths`, `content`, or
/// `semantic`; mixed declarations produce `Err(AmbiguousMode)`. All regex
/// patterns are compiled through the private `compile_ci` helper (case-insensitive).
pub fn compile(cps: Vec<Checkpoint>) -> Result<Vec<CompiledCheckpoint>, CheckpointError> {
    cps.into_iter().map(compile_one).collect()
}

fn compile_one(cp: Checkpoint) -> Result<CompiledCheckpoint, CheckpointError> {
    let has_paths = !cp.paths.is_empty();
    let has_content = !cp.content.is_empty();
    let has_semantic = cp.semantic.is_some();

    let mode_count = usize::from(has_paths) + usize::from(has_content) + usize::from(has_semantic);
    if mode_count != 1 {
        return Err(CheckpointError::AmbiguousMode(cp.name));
    }

    let mode = if has_paths {
        let patterns = compile_patterns(&cp.name, &cp.paths)?;
        Mode::Path(patterns)
    } else if has_content {
        let patterns = compile_patterns(&cp.name, &cp.content)?;
        let exempt = compile_patterns(&cp.name, &cp.content_exempt_paths)?;
        Mode::Content { patterns, exempt }
    } else {
        let sem_str = cp.semantic.as_deref().unwrap_or("");
        let kind = match sem_str {
            "checkpoint_removed" => SemanticKind::CheckpointRemoved,
            other => return Err(CheckpointError::UnknownSemantic(cp.name, other.to_string())),
        };
        Mode::Semantic(kind)
    };

    Ok(CompiledCheckpoint {
        name: cp.name,
        summary: cp.summary,
        standards_doc: cp.standards_doc,
        mode,
    })
}

/// Case-insensitive, unanchored regex. Inlined verbatim from the framework's
/// `verify::patterns::compile_ci` — the sole intra-crate dependency of the
/// original `detect_hitl` module, dropped on extraction (ADR-0050).
fn compile_ci(pat: &str) -> Result<regex::Regex, regex::Error> {
    regex::RegexBuilder::new(pat).case_insensitive(true).build()
}

fn compile_patterns(
    checkpoint_name: &str,
    patterns: &[String],
) -> Result<Vec<regex::Regex>, CheckpointError> {
    patterns
        .iter()
        .map(|p| compile_ci(p).map_err(|e| CheckpointError::Regex(checkpoint_name.to_string(), e)))
        .collect()
}

// ── ACK regex: fixed pattern, compiled once ────────────────────────────────────

static ACK_RE: once_cell::sync::Lazy<regex::Regex> = once_cell::sync::Lazy::new(|| {
    // Fixed compile-time pattern; panic here is a programming bug, not runtime input.
    regex::Regex::new(r"(?m)^HITL-ACK:\s*([\w-]+)(?:\s+(.*))?$")
        .expect("ACK_RE is a fixed, lookaround-free pattern that must compile")
});

/// A parsed `HITL-ACK:` acknowledgement line from a commit message.
#[derive(Debug, Clone, PartialEq)]
pub struct Ack {
    pub name: String,
    /// Remainder of the line after the name; empty string when no reason was supplied.
    pub reason: String,
}

/// Extract all `HITL-ACK:` lines from a commit message.
///
/// Pattern: `(?m)^HITL-ACK:\s*([\w-]+)(?:\s+(.*))?$`
///
/// The reason is the text following the name on the same line; absent → empty string.
pub fn extract_acks(commit_msg: &str) -> Vec<Ack> {
    ACK_RE
        .captures_iter(commit_msg)
        .map(|cap| {
            let name = cap.get(1).map_or("", |m| m.as_str()).to_string();
            let reason = cap.get(2).map_or("", |m| m.as_str()).to_string();
            Ack { name, reason }
        })
        .collect()
}

/// Partition fired checkpoints into acknowledged and unacknowledged.
///
/// A fired checkpoint is acked when its `name` appears in `acks`. Returns
/// `(acked, unacked)` as slices of references into `fired`, preserving order.
pub fn partition_ack<'a>(fired: &'a [Fired], acks: &[Ack]) -> (Vec<&'a Fired>, Vec<&'a Fired>) {
    let ack_names: std::collections::HashSet<&str> = acks.iter().map(|a| a.name.as_str()).collect();

    fired
        .iter()
        .partition(|f| ack_names.contains(f.name.as_str()))
}

/// Compute the process exit class from fired and unacked checkpoint counts.
///
/// - `0`: no checkpoints fired.
/// - `1`: checkpoints fired, all acknowledged.
/// - `2`: at least one fired checkpoint remains unacknowledged.
pub fn exit_class(fired_len: usize, unacked_len: usize) -> i32 {
    if fired_len == 0 {
        0
    } else if unacked_len == 0 {
        1
    } else {
        2
    }
}

/// Extract the `name` values from a checkpoints YAML text.
///
/// Deserialises the text into the same `CheckpointsFile` structure used by
/// `load_checkpoints`. Empty or unparseable input returns `vec![]` without
/// panicking; callers treat the result as a best-effort baseline.
pub fn extract_checkpoint_names(yaml_text: &str) -> Vec<String> {
    if yaml_text.is_empty() {
        return vec![];
    }
    match serde_yaml::from_str::<CheckpointsFile>(yaml_text) {
        Ok(file) => file.checkpoints.into_iter().map(|cp| cp.name).collect(),
        Err(_) => vec![],
    }
}

// ──────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    // ── Ack corpus helpers ─────────────────────────────────────────────────────

    #[derive(Debug, PartialEq, serde::Deserialize)]
    struct AckExpected {
        name: String,
        reason: String,
    }

    #[derive(serde::Deserialize)]
    struct ExitInput {
        fired_len: usize,
        unacked_len: usize,
    }

    #[test]
    fn corpus_ack_cases() {
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/corpus");
        let cat_dir = root.join("conformance/detect-hitl/ack");

        let mut entries: Vec<_> = std::fs::read_dir(&cat_dir)
            .unwrap_or_else(|_| panic!("could not read ack corpus dir: {}", cat_dir.display()))
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_dir())
            .collect();
        entries.sort_by_key(|e| e.file_name());

        let mut case_count = 0usize;

        for entry in entries {
            let case_dir = entry.path();
            let case_name = format!("ack/{}", case_dir.file_name().unwrap().to_string_lossy());

            let msg: String = {
                let raw = std::fs::read_to_string(case_dir.join("input.json"))
                    .unwrap_or_else(|_| panic!("{case_name}: missing input.json"));
                serde_json::from_str(&raw)
                    .unwrap_or_else(|e| panic!("{case_name}: input.json parse error: {e}"))
            };

            let mut expected: Vec<AckExpected> = {
                let raw = std::fs::read_to_string(case_dir.join("expected.json"))
                    .unwrap_or_else(|_| panic!("{case_name}: missing expected.json"));
                serde_json::from_str(&raw)
                    .unwrap_or_else(|e| panic!("{case_name}: expected.json parse error: {e}"))
            };
            expected.sort_by(|a, b| a.name.cmp(&b.name));

            let acks = extract_acks(&msg);
            let mut got: Vec<AckExpected> = acks
                .into_iter()
                .map(|a| AckExpected {
                    name: a.name,
                    reason: a.reason,
                })
                .collect();
            got.sort_by(|a, b| a.name.cmp(&b.name));

            assert_eq!(got, expected, "corpus case {case_name}: acks mismatch");

            case_count += 1;
        }

        assert!(
            case_count >= 4,
            "expected >= 4 ack corpus cases, ran {case_count}"
        );
    }

    #[test]
    fn corpus_exit_cases() {
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/corpus");
        let cat_dir = root.join("conformance/detect-hitl/exit");

        let mut entries: Vec<_> = std::fs::read_dir(&cat_dir)
            .unwrap_or_else(|_| panic!("could not read exit corpus dir: {}", cat_dir.display()))
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_dir())
            .collect();
        entries.sort_by_key(|e| e.file_name());

        let mut case_count = 0usize;

        for entry in entries {
            let case_dir = entry.path();
            let case_name = format!("exit/{}", case_dir.file_name().unwrap().to_string_lossy());

            let input: ExitInput = {
                let raw = std::fs::read_to_string(case_dir.join("input.json"))
                    .unwrap_or_else(|_| panic!("{case_name}: missing input.json"));
                serde_json::from_str(&raw)
                    .unwrap_or_else(|e| panic!("{case_name}: input.json parse error: {e}"))
            };

            let expected: i32 = {
                let raw = std::fs::read_to_string(case_dir.join("expected.json"))
                    .unwrap_or_else(|_| panic!("{case_name}: missing expected.json"));
                serde_json::from_str(&raw)
                    .unwrap_or_else(|e| panic!("{case_name}: expected.json parse error: {e}"))
            };

            let got = exit_class(input.fired_len, input.unacked_len);
            assert_eq!(
                got, expected,
                "corpus case {case_name}: exit_class mismatch"
            );

            case_count += 1;
        }

        assert!(
            case_count >= 3,
            "expected >= 3 exit corpus cases, ran {case_count}"
        );
    }

    #[test]
    fn partition_ack_semantics() {
        let fired = vec![
            Fired {
                name: "gate-self-mod".to_string(),
                summary: "s1".to_string(),
                matched: vec![],
            },
            Fired {
                name: "framework-core-change".to_string(),
                summary: "s2".to_string(),
                matched: vec![],
            },
            Fired {
                name: "destructive-ops".to_string(),
                summary: "s3".to_string(),
                matched: vec![],
            },
        ];

        // Ack covers two of the three fired checkpoints.
        let acks = vec![
            Ack {
                name: "gate-self-mod".to_string(),
                reason: "ADR-0010".to_string(),
            },
            Ack {
                name: "framework-core-change".to_string(),
                reason: "deliberate".to_string(),
            },
        ];

        let (acked, unacked) = partition_ack(&fired, &acks);
        let mut acked_names: Vec<&str> = acked.iter().map(|f| f.name.as_str()).collect();
        let mut unacked_names: Vec<&str> = unacked.iter().map(|f| f.name.as_str()).collect();
        acked_names.sort_unstable();
        unacked_names.sort_unstable();

        assert_eq!(acked_names, ["framework-core-change", "gate-self-mod"]);
        assert_eq!(unacked_names, ["destructive-ops"]);

        // Empty acks — everything unacked.
        let (acked2, unacked2) = partition_ack(&fired, &[]);
        assert!(acked2.is_empty());
        assert_eq!(unacked2.len(), 3);

        // All acked.
        let all_acks = vec![
            Ack {
                name: "gate-self-mod".to_string(),
                reason: String::new(),
            },
            Ack {
                name: "framework-core-change".to_string(),
                reason: String::new(),
            },
            Ack {
                name: "destructive-ops".to_string(),
                reason: String::new(),
            },
        ];
        let (acked3, unacked3) = partition_ack(&fired, &all_acks);
        assert_eq!(acked3.len(), 3);
        assert!(unacked3.is_empty());
    }

    // ── Detect corpus helper ───────────────────────────────────────────────────

    /// Corpus input shape for detect-hitl fixture files.
    #[derive(serde::Deserialize)]
    struct CorpusInput {
        files: Vec<FileEntry>,
        added_lines: HashMap<String, Vec<String>>,
        base_checkpoint_names: Option<Vec<String>>,
    }

    #[test]
    fn corpus_detect_cases() {
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/corpus");

        let global = load_checkpoints(&root.join("promise/checkpoints.yaml"))
            .expect("load global checkpoints");
        let repo = load_checkpoints(&root.join(".dotclaude/checkpoints.yaml"))
            .expect("load repo checkpoints");
        let merged = merge(global, repo);
        let compiled = compile(merged).expect("compile merged checkpoints");

        let corpus_root = root.join("conformance/detect-hitl");
        let categories = ["path", "content", "semantic", "merge"];
        let mut case_count = 0usize;

        for category in &categories {
            let cat_dir = corpus_root.join(category);
            let mut entries: Vec<_> = std::fs::read_dir(&cat_dir)
                .unwrap_or_else(|_| panic!("could not read category dir: {}", cat_dir.display()))
                .filter_map(|e| e.ok())
                .filter(|e| e.path().is_dir())
                .collect();
            entries.sort_by_key(|e| e.file_name());

            for entry in entries {
                let case_dir = entry.path();
                let case_name = format!(
                    "{}/{}",
                    category,
                    case_dir.file_name().unwrap().to_string_lossy()
                );

                let input_text = std::fs::read_to_string(case_dir.join("input.json"))
                    .unwrap_or_else(|_| panic!("{case_name}: missing input.json"));
                let input: CorpusInput = serde_json::from_str(&input_text)
                    .unwrap_or_else(|e| panic!("{case_name}: input.json parse error: {e}"));

                let expected_text = std::fs::read_to_string(case_dir.join("expected.json"))
                    .unwrap_or_else(|_| panic!("{case_name}: missing expected.json"));
                let mut expected: Vec<String> = serde_json::from_str(&expected_text)
                    .unwrap_or_else(|e| panic!("{case_name}: expected.json parse error: {e}"));
                expected.sort();

                let base_names_ref = input.base_checkpoint_names.as_deref();
                let fired = detect(&compiled, &input.files, &input.added_lines, base_names_ref);

                let mut fired_names: Vec<String> = fired.iter().map(|f| f.name.clone()).collect();
                fired_names.sort();

                assert_eq!(
                    fired_names, expected,
                    "corpus case {case_name}: expected {expected:?}, got {fired_names:?}"
                );

                case_count += 1;
            }
        }

        assert!(
            case_count >= 9,
            "expected >= 9 corpus cases, ran {case_count}"
        );
    }

    #[test]
    fn real_checkpoints_patterns_compile() {
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/corpus");
        let mut checked = 0usize;
        for rel in ["promise/checkpoints.yaml", ".dotclaude/checkpoints.yaml"] {
            let cps = load_checkpoints(&root.join(rel)).expect("load");
            let compiled = compile(cps).expect("all patterns compile under compile_ci");
            checked += compiled.len();
        }
        assert!(checked >= 5, "expected >=5 checkpoints, got {checked}");
    }

    #[test]
    fn load_absent_returns_empty() {
        let absent = Path::new("/nonexistent/no/such/path/checkpoints.yaml");
        let result = load_checkpoints(absent).expect("absent file returns Ok");
        assert!(result.is_empty());
    }

    #[test]
    fn load_malformed_returns_parse_error() {
        let dir = std::env::temp_dir().join("hitl_test_malformed");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("checkpoints.yaml");
        std::fs::write(&p, "not: valid: yaml: [\n").unwrap();
        assert!(matches!(
            load_checkpoints(&p),
            Err(CheckpointError::Parse(_, _))
        ));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn ambiguous_mode_rejected() {
        let cp = Checkpoint {
            name: "both".to_string(),
            summary: "test".to_string(),
            standards_doc: None,
            paths: vec!["(^|/)foo$".to_string()],
            content: vec!["bar".to_string()],
            content_exempt_paths: vec![],
            semantic: None,
        };
        assert!(matches!(
            compile(vec![cp]),
            Err(CheckpointError::AmbiguousMode(_))
        ));
    }

    #[test]
    fn unknown_semantic_rejected() {
        let cp = Checkpoint {
            name: "sem".to_string(),
            summary: "test".to_string(),
            standards_doc: None,
            paths: vec![],
            content: vec![],
            content_exempt_paths: vec![],
            semantic: Some("totally_unknown".to_string()),
        };
        assert!(matches!(
            compile(vec![cp]),
            Err(CheckpointError::UnknownSemantic(_, _))
        ));
    }

    #[test]
    fn merge_repo_overrides_global() {
        let global = vec![
            Checkpoint {
                name: "a".to_string(),
                summary: "global-a".to_string(),
                standards_doc: None,
                paths: vec!["(^|/)a$".to_string()],
                content: vec![],
                content_exempt_paths: vec![],
                semantic: None,
            },
            Checkpoint {
                name: "b".to_string(),
                summary: "global-b".to_string(),
                standards_doc: None,
                paths: vec!["(^|/)b$".to_string()],
                content: vec![],
                content_exempt_paths: vec![],
                semantic: None,
            },
        ];
        let repo = vec![Checkpoint {
            name: "a".to_string(),
            summary: "repo-a".to_string(),
            standards_doc: None,
            paths: vec!["(^|/)repo_a$".to_string()],
            content: vec![],
            content_exempt_paths: vec![],
            semantic: None,
        }];
        let merged = merge(global, repo);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].name, "a");
        assert_eq!(merged[0].summary, "repo-a");
        assert_eq!(merged[1].name, "b");
    }

    #[test]
    fn merge_repo_extends_global() {
        let global = vec![Checkpoint {
            name: "g".to_string(),
            summary: "global".to_string(),
            standards_doc: None,
            paths: vec!["(^|/)g$".to_string()],
            content: vec![],
            content_exempt_paths: vec![],
            semantic: None,
        }];
        let repo = vec![Checkpoint {
            name: "r".to_string(),
            summary: "repo-only".to_string(),
            standards_doc: None,
            paths: vec!["(^|/)r$".to_string()],
            content: vec![],
            content_exempt_paths: vec![],
            semantic: None,
        }];
        let merged = merge(global, repo);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].name, "g");
        assert_eq!(merged[1].name, "r");
    }

    // ── Task 6: extract_checkpoint_names ──────────────────────────────────────

    #[test]
    fn extract_checkpoint_names_parses_names() {
        // Raw string so no Rust escape processing; plain YAML path strings
        // are fine here since Checkpoint only stores them as Vec<String>.
        let yaml = r#"version: "1"
checkpoints:
  - name: guard-a
    summary: Guards A
    paths:
      - "(^|/)a.txt$"
  - name: guard-b
    summary: Guards B
    paths:
      - "(^|/)b.txt$"
  - name: checkpoint-removed
    summary: Checkpoint removed
    semantic: checkpoint_removed
"#;

        let names = extract_checkpoint_names(yaml);
        assert_eq!(
            names.len(),
            3,
            "expected 3 names, got {}: {names:?}",
            names.len()
        );
        assert!(names.contains(&"guard-a".to_string()));
        assert!(names.contains(&"guard-b".to_string()));
        assert!(names.contains(&"checkpoint-removed".to_string()));

        // Empty input → empty vec.
        assert!(extract_checkpoint_names("").is_empty());
        // Unparseable → empty vec, no panic.
        assert!(extract_checkpoint_names("not: valid: yaml: [\n").is_empty());
    }

    // ── Task 6: ADR-0010 residual gap ─────────────────────────────────────────

    #[test]
    fn residual_gap_adr0010_checkpoint_removed_itself_removed() {
        // ADR-0010 known limitation: when the `checkpoint_removed`-semantic entry
        // is itself removed from the registry, no SemanticKind::CheckpointRemoved
        // checkpoint remains in the compiled set to trigger detection. Consequently
        // no `checkpoint-removed` Fired is produced. Path-based `gate-self-mod`
        // (guarding checkpoints.yaml) is the practical backstop for this scenario.
        let base_names = vec![
            "guard-a".to_string(),
            "guard-b".to_string(),
            "checkpoint-removed".to_string(),
        ];

        // Only guard-a survives in the compiled set; guard-b AND checkpoint-removed
        // were both removed from the YAML.
        let guard_a_cp = Checkpoint {
            name: "guard-a".to_string(),
            summary: "Guards A".to_string(),
            standards_doc: None,
            paths: vec!["(^|/)a\\.txt$".to_string()],
            content: vec![],
            content_exempt_paths: vec![],
            semantic: None,
        };
        let compiled = compile(vec![guard_a_cp]).expect("compile guard-a");

        let files = vec![FileEntry {
            status: 'M',
            path: ".dotclaude/checkpoints.yaml".to_string(),
        }];

        let fired = detect(&compiled, &files, &HashMap::new(), Some(&base_names));

        // SemanticKind::CheckpointRemoved is absent from `compiled` because the
        // entry was removed, so the detection logic never runs and no fire occurs.
        // This is the irreducible residual gap documented in ADR-0010.
        assert!(
            fired.iter().all(|f| f.name != "checkpoint-removed"),
            "ADR-0010 residual gap: checkpoint-removed must not fire when its own \
             entry was removed (SemanticKind::CheckpointRemoved absent from compiled \
             set); fired: {fired:?}"
        );
    }
}
