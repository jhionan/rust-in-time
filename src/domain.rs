use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Signal {
    pub source: String,
    pub bundle_id: String,
    pub window_title: String,
    pub binary_name: String,
    pub cwd: String,
    pub space_id: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Rule {
    pub id: i64,
    pub project_id: i64,
    pub priority: i64,
    pub match_bundle_id: Option<String>,
    pub match_title_substr: Option<String>,
    pub match_binary_name: Option<String>,
    pub match_cwd_prefix: Option<String>,
    pub match_space_id: Option<String>,
}

/// Windows paths are recognized independently of the host running the CLI.
fn is_windows_path(path: &str) -> bool {
    path.contains('\\')
        || path.starts_with("//")
        || (path.as_bytes().get(1) == Some(&b':')
            && path.as_bytes().first().is_some_and(u8::is_ascii_alphabetic))
}

fn slash_path(path: &str) -> String {
    if is_windows_path(path) {
        path.replace('\\', "/")
    } else {
        path.to_owned()
    }
}

/// A literal directory prefix, or a terminal wildcard within its final segment.
/// A wildcard never skips a directory between the literal parent and the match.
pub fn matches_cwd(pattern: &str, cwd: &str) -> bool {
    if validate_cwd_pattern(pattern).is_err() || cwd.is_empty() {
        return false;
    }
    let windows = is_windows_path(pattern) || is_windows_path(cwd);
    let normalize = |value: &str| {
        let slashes = slash_path(value);
        let trimmed = slashes.trim_end_matches('/');
        if windows {
            trimmed.to_lowercase()
        } else {
            trimmed.to_owned()
        }
    };
    let pattern = normalize(pattern);
    let cwd = normalize(cwd);
    if let Some(prefix) = pattern.strip_suffix('*') {
        let (parent, name_prefix) = prefix.rsplit_once('/').unwrap_or(("", prefix));
        let Some(rest) = cwd.strip_prefix(&format!("{parent}/")) else {
            return false;
        };
        let segment = rest.split('/').next().unwrap_or("");
        return !segment.is_empty() && segment.starts_with(name_prefix);
    }
    cwd == pattern || cwd.starts_with(&format!("{pattern}/"))
}

pub fn validate_cwd_pattern(pattern: &str) -> anyhow::Result<()> {
    let slashes = slash_path(pattern);
    let p = slashes.trim_end_matches('/');
    anyhow::ensure!(!p.is_empty(), "cwd pattern is empty: {pattern:?}");
    anyhow::ensure!(
        p.contains('/'),
        "cwd pattern must contain a path separator: {pattern:?}"
    );
    if let Some(index) = p.find('*') {
        anyhow::ensure!(
            index == p.len() - 1,
            "'*' is only allowed as the final character: {pattern:?}"
        );
    }
    anyhow::ensure!(
        !p.contains(['?', '[', ']']),
        "only a terminal '*' wildcard is supported: {pattern:?}"
    );
    Ok(())
}

/// Every specified clause is required. Empty space metadata never matches.
pub fn matches_rule(signal: &Signal, rule: &Rule) -> bool {
    rule.match_bundle_id
        .as_ref()
        .is_none_or(|v| signal.bundle_id == *v)
        && rule
            .match_title_substr
            .as_ref()
            .is_none_or(|v| signal.window_title.contains(v))
        && rule
            .match_binary_name
            .as_ref()
            .is_none_or(|v| signal.binary_name == *v)
        && rule
            .match_cwd_prefix
            .as_ref()
            .is_none_or(|v| matches_cwd(v, &signal.cwd))
        && rule
            .match_space_id
            .as_ref()
            .is_none_or(|v| !signal.space_id.is_empty() && signal.space_id == *v)
}

pub fn match_rules(signal: &Signal, rules: &[Rule]) -> Option<i64> {
    rules
        .iter()
        .filter(|rule| matches_rule(signal, rule))
        .min_by_key(|rule| (rule.priority, rule.id))
        .map(|rule| rule.project_id)
}

/// Select the deepest directory named for the project, otherwise its parent.
/// Forward slashes keep a Windows proposal portable between CLI platforms.
pub fn propose_cwd(cwd: &str, project: &str) -> String {
    let path = slash_path(cwd);
    let prefix = if path.starts_with("//") {
        "//"
    } else if path.starts_with('/') {
        "/"
    } else {
        ""
    };
    let drive_root = path.as_bytes().get(1) == Some(&b':') && path.as_bytes().get(2) == Some(&b'/');
    let rooted = !prefix.is_empty() || drive_root;
    let floor = if prefix == "//" {
        2
    } else {
        usize::from(drive_root)
    };
    let mut components: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." if components.len() > floor && components.last() != Some(&"..") => {
                components.pop();
            }
            ".." if rooted => {}
            _ => components.push(part),
        }
    }
    let mut clean = format!("{prefix}{}", components.join("/"));
    if drive_root && components.len() == 1 {
        clean.push('/');
    }
    let parts: Vec<&str> = clean.split('/').collect();
    if let Some(index) = parts
        .iter()
        .rposition(|part| !project.is_empty() && part.eq_ignore_ascii_case(project))
    {
        return format!("{}/", parts[..=index].join("/"));
    }
    if prefix == "//" && components.len() <= 2 {
        return format!("{}/", clean.trim_end_matches('/'));
    }
    match clean.rsplit_once('/') {
        Some((parent, _)) => format!("{parent}/"),
        None => "./".to_owned(),
    }
}
