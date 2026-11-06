//! Configuration and hostname helpers, following the Portless reference.
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_str(&fs::read_to_string(path).ok()?).ok()
}
fn git(cwd: &Path, args: &[&str]) -> Option<String> {
    let o = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .ok()?;
    o.status
        .success()
        .then(|| String::from_utf8_lossy(&o.stdout).trim().to_owned())
}

/// Missing configuration is an empty object. Invalid explicit files are errors.
pub fn load(cwd: &Path) -> Result<Value> {
    let path = cwd.join("portless.json");
    match fs::read_to_string(&path) {
        Ok(raw) => {
            let v = serde_json::from_str(&raw)
                .with_context(|| format!("Invalid JSON in {}", path.display()))?;
            validate_config(&v)?;
            Ok(v)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let v = read_json(&cwd.join("package.json"))
                .and_then(|p| p.get("portless").cloned())
                .unwrap_or(Value::Null);
            let v = match v {
                Value::String(s) if !s.trim().is_empty() => json!({"name":s.trim()}),
                Value::String(_) | Value::Null => json!({}),
                v => v,
            };
            validate_config(&v)?;
            Ok(v)
        }
        Err(e) => Err(e).with_context(|| format!("Reading {}", path.display())),
    }
}

pub fn validate_config(v: &Value) -> Result<()> {
    validate_app(v, true)
}
fn validate_app(v: &Value, top: bool) -> Result<()> {
    let o = v
        .as_object()
        .context("configuration must be a JSON object")?;
    for k in ["name", "script"] {
        if let Some(v) = o.get(k) {
            if v.as_str().is_none_or(|s| s.trim().is_empty()) {
                bail!("{k} must be a non-empty string");
            }
        }
    }
    if let Some(p) = o.get("appPort") {
        if p.as_u64().is_none_or(|p| !(1..=65535).contains(&p)) {
            bail!("appPort must be an integer between 1 and 65535");
        }
    }
    for k in ["proxy", "turbo"] {
        if (top || k == "proxy") && o.get(k).is_some_and(|v| !v.is_boolean()) {
            bail!("{k} must be a boolean");
        }
    }
    if let Some(w) = o.get("worktree") {
        let w = w.as_object().context("worktree must be an object")?;
        if w.get("prefix")
            .is_some_and(|v| !matches!(v.as_str(), Some("branch" | "last-segment")))
        {
            bail!("worktree.prefix must be branch or last-segment");
        }
        if let Some(t) = w.get("hostnameTemplate") {
            let t = t.as_str().context("hostnameTemplate must be a string")?;
            if !t.contains("{worktree}")
                || !t
                    .replace("{worktree}", "worktree")
                    .replace("{name}", "name")
                    .split('.')
                    .all(valid_label)
            {
                bail!(
                    "hostnameTemplate must contain {{worktree}} and render valid hostname labels"
                );
            }
        }
    }
    if top {
        if let Some(apps) = o.get("apps") {
            for (_, app) in apps.as_object().context("apps must be an object")? {
                validate_app(app, false)?;
            }
        }
    }
    Ok(())
}
fn valid_label(s: &str) -> bool {
    !s.is_empty()
        && s.as_bytes()[0].is_ascii_alphanumeric()
        && s.as_bytes()[s.len() - 1].is_ascii_alphanumeric()
        && s.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
}

pub fn truncate_label(s: &str) -> String {
    use sha2::{Digest, Sha256};
    if s.len() <= 63 {
        return s.to_owned();
    }
    let hash = format!("{:x}", Sha256::digest(s.as_bytes()));
    let mut end = 56;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}-{}", s[..end].trim_end_matches('-'), &hash[..6])
}
pub fn sanitize_for_hostname(s: &str) -> String {
    let mut out = String::new();
    for c in s.to_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    truncate_label(out.trim_end_matches('-'))
}
fn short_name(s: &str) -> &str {
    if s.starts_with('@') {
        s.split_once('/').map_or(s, |(_, n)| n)
    } else {
        s
    }
}
pub fn infer_name(cwd: &Path) -> Result<String> {
    for d in cwd.ancestors() {
        if let Some(p) = read_json(&d.join("package.json")) {
            if let Some(n) = p["name"].as_str().filter(|s| !s.is_empty()) {
                let n = sanitize_for_hostname(short_name(n));
                if !n.is_empty() {
                    return Ok(n);
                }
                break;
            }
        }
    }
    let root = git(cwd, &["rev-parse", "--show-toplevel"])
        .map(PathBuf::from)
        .or_else(|| {
            cwd.ancestors()
                .find(|p| p.join(".git").exists())
                .map(Path::to_path_buf)
        });
    for p in root.as_deref().into_iter().chain(std::iter::once(cwd)) {
        let n = sanitize_for_hostname(&p.file_name().unwrap_or_default().to_string_lossy());
        if !n.is_empty() {
            return Ok(n);
        }
    }
    bail!("Could not infer a project name")
}
pub fn apply_worktree_prefix(base: &str, prefix: Option<&str>, template: Option<&str>) -> String {
    match prefix {
        None | Some("") => base.to_owned(),
        Some(p) => template
            .map(|t| {
                t.replace("{worktree}", p)
                    .replace("{name}", base)
                    .to_lowercase()
                    .split('.')
                    .map(truncate_label)
                    .collect::<Vec<_>>()
                    .join(".")
            })
            .unwrap_or_else(|| format!("{p}.{base}")),
    }
}
fn branch_prefix(branch: &str, style: &str) -> Option<String> {
    if matches!(branch, "" | "HEAD" | "main" | "master") {
        return None;
    }
    let p = sanitize_for_hostname(if style == "branch" {
        branch
    } else {
        branch.rsplit('/').next()?
    });
    (!p.is_empty()).then_some(p)
}
pub fn detect_worktree_prefix(cwd: &Path, style: &str) -> Option<String> {
    if let Some(p) =
        std::env::var_os("PORCH_PREFIX").or_else(|| std::env::var_os("PORTLESS_PREFIX"))
    {
        let p = sanitize_for_hostname(&p.to_string_lossy());
        return (!p.is_empty()).then_some(p);
    }
    if let Some(list) = git(cwd, &["worktree", "list", "--porcelain"]) {
        if list.lines().filter(|l| l.starts_with("worktree ")).count() <= 1 {
            return None;
        }
        if let (Some(a), Some(b)) = (
            git(cwd, &["rev-parse", "--git-dir"]),
            git(cwd, &["rev-parse", "--git-common-dir"]),
        ) {
            if fs::canonicalize(cwd.join(a)).ok() == fs::canonicalize(cwd.join(b)).ok() {
                return None;
            }
        }
        if let Some(b) = git(cwd, &["rev-parse", "--abbrev-ref", "HEAD"]) {
            return branch_prefix(&b, style);
        }
    }
    for d in cwd.ancestors() {
        let p = d.join(".git");
        if p.is_dir() {
            return None;
        }
        if let Ok(raw) = fs::read_to_string(p) {
            let target = raw
                .trim()
                .strip_prefix("gitdir:")?
                .trim()
                .replace('\\', "/");
            let t = Path::new(&target);
            if t.parent()?.file_name()? != "worktrees" {
                return None;
            }
            let head = fs::read_to_string(d.join(t).join("HEAD")).ok()?;
            return branch_prefix(head.trim().strip_prefix("ref: refs/heads/")?, style);
        }
    }
    None
}
/// Root worktree defaults survive app overrides; other root fields do not when apps exists.
pub fn resolve_app_config(config: &Value, config_dir: &Path, package_dir: &Path) -> Value {
    let mut out = json!({});
    if let Some(apps) = config.get("apps") {
        if let Ok(rel) = package_dir.strip_prefix(config_dir) {
            for p in rel.ancestors().filter(|p| !p.as_os_str().is_empty()) {
                if let Some(app) = apps.get(p.to_string_lossy().replace('\\', "/")) {
                    out = app.clone();
                    break;
                }
            }
        }
    } else {
        for k in ["name", "script", "appPort", "proxy"] {
            if let Some(v) = config.get(k) {
                out[k] = v.clone();
            }
        }
    }
    let mut w = config.get("worktree").cloned().unwrap_or(json!({}));
    if let Some(app) = out.get("worktree").and_then(Value::as_object) {
        for (k, v) in app {
            w[k] = v.clone();
        }
    }
    if w.as_object().is_some_and(|o| !o.is_empty()) {
        out["worktree"] = w;
    }
    out
}

pub fn detect_package_manager(cwd: &Path) -> String {
    for d in cwd.ancestors() {
        if let Some(p) = read_json(&d.join("package.json")) {
            if let Some(pm) = p["packageManager"]
                .as_str()
                .and_then(|s| s.split('@').next())
            {
                if ["npm", "pnpm", "yarn", "bun"].contains(&pm) {
                    return pm.into();
                }
            }
        }
        for (file, pm) in [
            ("pnpm-lock.yaml", "pnpm"),
            ("yarn.lock", "yarn"),
            ("bun.lockb", "bun"),
            ("bun.lock", "bun"),
            ("package-lock.json", "npm"),
        ] {
            if d.join(file).exists() {
                return pm.into();
            }
        }
    }
    "npm".into()
}
pub fn resolve_script_raw(name: &str, dir: &Path) -> Option<String> {
    let p = read_json(&dir.join("package.json"))?;
    p["scripts"][name]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .map(str::to_owned)
}
pub fn resolve_script_command(name: &str, dir: &Path) -> Option<Vec<String>> {
    let p = read_json(&dir.join("package.json"))?;
    p["scripts"][name].as_str()?;
    Some(vec![detect_package_manager(dir), "run".into(), name.into()])
}
pub fn find_workspace_root(cwd: &Path) -> Option<PathBuf> {
    cwd.ancestors()
        .find(|d| d.join("pnpm-workspace.yaml").is_file() || workspace_patterns(d).is_some())
        .map(Path::to_path_buf)
}
fn workspace_patterns(dir: &Path) -> Option<Vec<String>> {
    if let Ok(raw) = fs::read_to_string(dir.join("pnpm-workspace.yaml")) {
        let v: serde_yaml::Value = serde_yaml::from_str(&raw).ok()?;
        return Some(
            v.get("packages")?
                .as_sequence()?
                .iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect(),
        );
    }
    let p = read_json(&dir.join("package.json"))?;
    let w = p.get("workspaces")?;
    Some(
        w.as_array()
            .or_else(|| w.get("packages")?.as_array())?
            .iter()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect(),
    )
}
#[derive(Debug, Clone)]
pub struct WorkspacePackage {
    pub dir: PathBuf,
    pub name: Option<String>,
    pub scope: Option<String>,
    pub scripts: Value,
}
pub fn discover_workspace_packages(root: &Path) -> Vec<WorkspacePackage> {
    use std::collections::BTreeSet;
    fn expand(base: &Path, parts: &[&str], out: &mut BTreeSet<PathBuf>) {
        if parts.is_empty() {
            if base.is_dir() {
                out.insert(base.to_owned());
            }
            return;
        }
        let pat = parts[0];
        if let Some((pre, post)) = pat.split_once('*') {
            if let Ok(entries) = fs::read_dir(base) {
                for e in entries.flatten() {
                    let n = e.file_name();
                    let n = n.to_string_lossy();
                    if e.file_type().is_ok_and(|t| t.is_dir())
                        && n.starts_with(pre)
                        && n.ends_with(post.trim_end_matches('*'))
                    {
                        expand(&e.path(), &parts[1..], out);
                    }
                }
            }
        } else {
            expand(&base.join(pat), &parts[1..], out);
        }
    }
    let mut included = BTreeSet::new();
    let mut excluded = BTreeSet::new();
    for p in workspace_patterns(root).unwrap_or_default() {
        let (neg, p) = p
            .strip_prefix('!')
            .map_or((false, p.as_str()), |p| (true, p));
        expand(
            root,
            &p.split('/').collect::<Vec<_>>(),
            if neg { &mut excluded } else { &mut included },
        );
    }
    included
        .difference(&excluded)
        .filter_map(|d| {
            let p = read_json(&d.join("package.json"))?;
            let raw = p["name"].as_str();
            Some(WorkspacePackage {
                dir: d.clone(),
                name: raw.map(|s| short_name(s).into()),
                scope: raw
                    .and_then(|s| s.strip_prefix('@')?.split_once('/').map(|(s, _)| s.into())),
                scripts: p
                    .get("scripts")
                    .filter(|v| v.is_object())
                    .cloned()
                    .unwrap_or(json!({})),
            })
        })
        .collect()
}
pub fn workspace_project_name(root: &Path, packages: &[WorkspacePackage]) -> Result<String> {
    let config = load(root)?;
    if let Some(n) = config["name"].as_str() {
        return Ok(n.into());
    }
    let mut counts: Vec<(&str, usize)> = Vec::new();
    for s in packages.iter().filter_map(|p| p.scope.as_deref()) {
        if let Some(c) = counts.iter_mut().find(|c| c.0 == s) {
            c.1 += 1;
        } else {
            counts.push((s, 1));
        }
    }
    let mut best = None;
    let mut max = 0;
    for (s, c) in counts {
        if c > max {
            best = Some(s);
            max = c;
        }
    }
    if let Some(s) = best {
        let n = sanitize_for_hostname(s);
        if !n.is_empty() {
            return Ok(n);
        }
    }
    infer_name(root)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn validation() {
        for v in [
            json!(null),
            json!([]),
            json!({"appPort":0}),
            json!({"appPort":1.5}),
            json!({"proxy":null}),
            json!({"apps":{"x":[]}}),
            json!({"worktree":{"hostnameTemplate":"{name}"}}),
            json!({"worktree":{"hostnameTemplate":"{worktree}.{other}"}}),
        ] {
            assert!(validate_config(&v).is_err(), "{v}");
        }
        assert!(validate_config(
            &json!({"unknown":true,"worktree":{"hostnameTemplate":"UPPER.{worktree}-{name}"}})
        )
        .is_ok());
    }
    #[test]
    fn names_and_templates() {
        assert_eq!(sanitize_for_hostname(" @Foo / Bar__ "), "foo-bar");
        assert_eq!(
            apply_worktree_prefix("web", None, Some("{worktree}")),
            "web"
        );
        assert_eq!(
            apply_worktree_prefix("web", Some("login"), Some("TENANT.{worktree}-{name}")),
            "tenant.login-web"
        );
        assert_eq!(
            branch_prefix("team/login", "branch"),
            Some("team-login".into())
        );
        assert_eq!(
            branch_prefix("team/login", "last-segment"),
            Some("login".into())
        );
        assert_eq!(branch_prefix("main", "branch"), None);
        let long = truncate_label(&"a".repeat(100));
        assert_eq!(long.len(), 63);
        assert_eq!(&long[57..], "281659");
    }
    #[test]
    fn inheritance() {
        let c = json!({"name":"ignored","worktree":{"prefix":"branch","hostnameTemplate":"{worktree}-{name}"},"apps":{"apps/web":{"name":"web","worktree":{"prefix":"last-segment"}}}});
        let v = resolve_app_config(&c, Path::new("/repo"), Path::new("/repo/apps/web/src"));
        assert_eq!(v["name"], "web");
        assert_eq!(v["worktree"]["prefix"], "last-segment");
        assert_eq!(v["worktree"]["hostnameTemplate"], "{worktree}-{name}");
        assert!(
            resolve_app_config(&c, Path::new("/repo"), Path::new("/elsewhere"))
                .get("name")
                .is_none()
        );
    }
    #[test]
    fn filesystem_configuration() {
        let dir = std::env::temp_dir().join(format!("porch-config-test-{}", std::process::id()));
        fs::create_dir_all(dir.join("apps/web")).unwrap();
        fs::write(
            dir.join("package.json"),
            r#"{"portless":" site ","workspaces":["apps/*"]}"#,
        )
        .unwrap();
        fs::write(
            dir.join("apps/web/package.json"),
            r#"{"name":"@org/web","scripts":{"dev":"vite"}}"#,
        )
        .unwrap();
        assert_eq!(load(&dir).unwrap()["name"], "site");
        assert_eq!(infer_name(&dir.join("apps/web")).unwrap(), "web");
        assert_eq!(discover_workspace_packages(&dir).len(), 1);
        fs::write(dir.join("portless.json"), "{").unwrap();
        assert!(load(&dir).is_err());
        fs::remove_dir_all(dir).unwrap();
    }
}
