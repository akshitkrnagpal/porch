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

/// Resolve one directory, without merging competing sources. Native settings win:
/// porch.json > package.json porch > portless.json > package.json portless.
/// Missing configuration is an empty object. Invalid selected files are errors.
pub fn load(cwd: &Path) -> Result<Value> {
    if let Some(config) = config_file(&cwd.join("porch.json"))? {
        return Ok(config);
    }
    let package = read_json(&cwd.join("package.json"));
    if let Some(config) = package.as_ref().and_then(|p| p.get("porch")) {
        return package_settings(config.clone());
    }
    if let Some(config) = config_file(&cwd.join("portless.json"))? {
        return Ok(config);
    }
    package_settings(
        package
            .and_then(|p| p.get("portless").cloned())
            .unwrap_or(Value::Null),
    )
}

fn package_settings(value: Value) -> Result<Value> {
    let value = match value {
        Value::String(s) if !s.trim().is_empty() => json!({"name":s.trim()}),
        Value::String(_) | Value::Null => json!({}),
        value => value,
    };
    validate_config(&value)?;
    Ok(value)
}

fn config_file(path: &Path) -> Result<Option<Value>> {
    match fs::read_to_string(path) {
        Ok(raw) => {
            let v = serde_json::from_str(&raw)
                .with_context(|| format!("Invalid JSON in {}", path.display()))?;
            validate_config(&v)?;
            Ok(Some(v))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
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
/// Root worktree defaults survive app overrides. Per-app root fields only apply
/// in the configuration directory; descendants inherit only a matching apps entry.
pub fn resolve_app_config(config: &Value, config_dir: &Path, package_dir: &Path) -> Value {
    let mut out = json!({});
    if config_dir == package_dir {
        for k in ["name", "script", "appPort", "proxy", "turbo"] {
            if let Some(v) = config.get(k) {
                out[k] = v.clone();
            }
        }
    } else if let Some(apps) = config.get("apps") {
        if let Ok(rel) = package_dir.strip_prefix(config_dir) {
            for p in rel.ancestors().filter(|p| !p.as_os_str().is_empty()) {
                if let Some(app) = apps.get(p.to_string_lossy().replace('\\', "/")) {
                    out = app.clone();
                    break;
                }
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

// Node process.cwd() uses ordinary Windows paths, while Rust canonicalize
// returns extended-length paths. Keep manifest keys in Node's representation.
fn canonical_cwd(cwd: &Path) -> std::io::Result<PathBuf> {
    let path = fs::canonicalize(cwd)?;
    #[cfg(windows)]
    {
        let value = path.to_string_lossy();
        if let Some(unc) = value.strip_prefix(r"\\?\UNC\") {
            return Ok(PathBuf::from(format!(r"\\{unc}")));
        }
        if let Some(local) = value.strip_prefix(r"\\?\") {
            return Ok(PathBuf::from(local));
        }
    }
    Ok(path)
}

/// A fully resolved app invocation. The lifecycle owner reserves the port and starts it.
#[derive(Debug, Clone)]
pub struct LaunchPlan {
    pub cwd: PathBuf,
    pub name: String,
    pub command: Vec<String>,
    pub app_port: Option<u16>,
    pub proxy: bool,
}

fn merge_config(base: &Value, leaf: &Value) -> Value {
    let mut out = base.clone();
    for (key, value) in leaf.as_object().into_iter().flatten() {
        if key == "worktree" {
            if !out[key].is_object() {
                out[key] = json!({});
            }
            for (k, v) in value.as_object().into_iter().flatten() {
                out[key][k] = v.clone();
            }
        } else {
            out[key] = value.clone();
        }
    }
    out
}

/// Resolve root app entries, leaf configuration and package overrides together.
pub fn effective_config(cwd: &Path) -> Result<Value> {
    let root = find_workspace_root(cwd).unwrap_or_else(|| cwd.to_owned());
    let config = load(&root)?;
    let root_app = resolve_app_config(&config, &root, cwd);
    let leaf = if root != cwd { load(cwd)? } else { json!({}) };
    let leaf = resolve_app_config(&leaf, cwd, cwd);
    let out = merge_config(&root_app, &leaf);
    validate_config(&out)?;
    Ok(out)
}

pub fn configured_name(cwd: &Path, base: &str, prefix: Option<&str>) -> Result<String> {
    let effective = effective_config(cwd)?;
    let worktree = prefix.map(sanitize_for_hostname).or_else(|| {
        detect_worktree_prefix(
            cwd,
            effective["worktree"]["prefix"]
                .as_str()
                .unwrap_or("last-segment"),
        )
    });
    let base = base
        .split('.')
        .map(truncate_label)
        .collect::<Vec<_>>()
        .join(".");
    let name = apply_worktree_prefix(
        &base,
        worktree.as_deref(),
        effective["worktree"]["hostnameTemplate"].as_str(),
    );
    if name.len() > 253 || !name.split('.').all(|s| s.len() <= 63 && valid_label(s)) {
        bail!("invalid hostname: {name}");
    }
    Ok(name.to_ascii_lowercase())
}

fn env_app_port() -> Result<Option<u16>> {
    std::env::var("PORCH_APP_PORT")
        .or_else(|_| std::env::var("PORTLESS_APP_PORT"))
        .ok()
        .map(|s| {
            let p: u16 = s
                .parse()
                .context("APP_PORT must be an integer between 1 and 65535")?;
            if p == 0 {
                bail!("APP_PORT must be an integer between 1 and 65535");
            }
            Ok(p)
        })
        .transpose()
}

fn workspace_package_name(root: &Path, pkg: &WorkspacePackage, project: &str) -> String {
    let name = pkg
        .name
        .as_deref()
        .map(sanitize_for_hostname)
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| {
            sanitize_for_hostname(
                &pkg.dir
                    .strip_prefix(root)
                    .unwrap_or(&pkg.dir)
                    .to_string_lossy(),
            )
        });
    if name == project {
        name
    } else {
        format!("{name}.{project}")
    }
}

fn runs_workspace_graph(command: &[String], cwd: &Path) -> bool {
    let Some(executable) = command.first().map(|s| basename(s)) else {
        return false;
    };
    if executable == "turbo" {
        return true;
    }
    if !["npm", "pnpm", "yarn", "bun", "npx", "pnpx", "bunx"].contains(&executable) {
        return false;
    }
    if command.iter().skip(1).any(|s| basename(s) == "turbo") {
        return true;
    }
    let script = if command.get(1).is_some_and(|s| s == "run") {
        command.get(2)
    } else if ["pnpm", "yarn", "bun"].contains(&executable) {
        command.get(1).filter(|s| !s.starts_with('-'))
    } else {
        None
    };
    script
        .and_then(|script| resolve_script_raw(script, cwd))
        .is_some_and(|raw| split_command(&raw).iter().any(|s| basename(s) == "turbo"))
}

/// Resolve bare, inferred and named execution into actual package-manager commands.
pub fn launch_plans(
    cwd: &Path,
    command: Option<Vec<String>>,
    name: Option<&str>,
    script: Option<&str>,
    prefix: Option<&str>,
) -> Result<Vec<LaunchPlan>> {
    let cwd = canonical_cwd(cwd).with_context(|| format!("reading {}", cwd.display()))?;
    let root = find_workspace_root(&cwd);
    let config = effective_config(&cwd)?;
    let env_port = env_app_port()?;
    if command.is_none() && name.is_none() && root.as_deref() == Some(cwd.as_path()) {
        let root_config = load(&cwd)?;
        let default_script = script.or(root_config["script"].as_str()).unwrap_or("dev");
        let packages = discover_workspace_packages(&cwd);
        let project = workspace_project_name(&cwd, &packages)?;
        let mut plans = Vec::new();
        for pkg in &packages {
            let app = effective_config(&pkg.dir)?;
            let target_script = script.or(app["script"].as_str()).unwrap_or(default_script);
            let Some(raw) = resolve_script_raw(target_script, &pkg.dir) else {
                continue;
            };
            let inferred = workspace_package_name(&cwd, pkg, &project);
            let base = app["name"].as_str().unwrap_or(&inferred);
            plans.push(LaunchPlan {
                cwd: pkg.dir.clone(),
                name: configured_name(&pkg.dir, base, prefix)?,
                command: resolve_script_command(target_script, &pkg.dir)
                    .context("workspace script disappeared")?,
                app_port: env_port.or_else(|| app["appPort"].as_u64().map(|p| p as u16)),
                proxy: app["proxy"]
                    .as_bool()
                    .unwrap_or_else(|| is_server_command(&split_command(&raw))),
            });
        }
        if !plans.is_empty() {
            plans.sort_by(|a, b| a.name.cmp(&b.name));
            let mut seen = std::collections::BTreeSet::new();
            for p in &plans {
                if p.proxy && !seen.insert(&p.name) {
                    bail!("duplicate workspace hostname: {}", p.name);
                }
            }
            return Ok(plans);
        }
    }
    let inferred = if let Some(root) = root.as_deref().filter(|root| *root != cwd) {
        let packages = discover_workspace_packages(root);
        if let Some(pkg) = packages.iter().find(|pkg| cwd.starts_with(&pkg.dir)) {
            let project = workspace_project_name(root, &packages)?;
            workspace_package_name(root, pkg, &project)
        } else {
            infer_name(&cwd)?
        }
    } else {
        infer_name(&cwd)?
    };
    let base = name
        .map(str::to_owned)
        .or_else(|| config["name"].as_str().map(str::to_owned))
        .unwrap_or(inferred);
    let script = script.or(config["script"].as_str()).unwrap_or("dev");
    let command = match command {
        Some(c) if !c.is_empty() => c,
        _ => resolve_script_command(script, &cwd).with_context(|| {
            format!(
                "no \"{script}\" script in {}/package.json; provide a command",
                cwd.display()
            )
        })?,
    };
    if root.as_deref() == Some(cwd.as_path())
        && discover_workspace_packages(&cwd).len() > 1
        && runs_workspace_graph(&command, &cwd)
    {
        bail!(
            "an explicit workspace Turbo command can start multiple apps with one PORT; \
             use bare `porch` so each app gets its own route and port, or \
             `porch -- --filter=<package>` to filter the workspace coordinator"
        );
    }
    Ok(vec![LaunchPlan {
        cwd: cwd.clone(),
        name: configured_name(&cwd, &base, prefix)?,
        command,
        app_port: env_port.or_else(|| config["appPort"].as_u64().map(|p| p as u16)),
        proxy: config["proxy"].as_bool().unwrap_or(true),
    }])
}

pub fn split_command(raw: &str) -> Vec<String> {
    let (mut single, mut double, mut escaped) = (false, false, false);
    let mut current = String::new();
    let mut out = Vec::new();
    for c in raw.chars() {
        if escaped {
            current.push(c);
            escaped = false;
        } else if c == '\\' && !single {
            escaped = true;
        } else if c == '\'' && !double {
            single = !single;
        } else if c == '"' && !single {
            double = !double;
        } else if c.is_whitespace() && !single && !double {
            if !current.is_empty() {
                out.push(std::mem::take(&mut current));
            }
        } else {
            current.push(c);
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

pub fn is_server_command(args: &[String]) -> bool {
    args.first().is_some_and(|c| {
        ![
            "tsup",
            "tsc",
            "esbuild",
            "rollup",
            "babel",
            "swc",
            "unbuild",
            "pkgroll",
            "ncc",
            "microbundle",
        ]
        .contains(&basename(c))
    })
}
fn basename(s: &str) -> &str {
    s.rsplit(['/', '\\'])
        .next()
        .unwrap_or(s)
        .trim_end_matches(".cmd")
        .trim_end_matches(".exe")
}
fn framework(s: &str) -> bool {
    [
        "vite",
        "vitepress",
        "vp",
        "react-router",
        "rsbuild",
        "astro",
        "ng",
        "react-native",
        "expo",
        "wrangler",
    ]
    .contains(&s)
}

#[derive(Debug)]
struct Invocation {
    name: String,
    index: usize,
    args: Vec<String>,
    insert: usize,
    separator: bool,
    uncertain: bool,
}
fn framework_invocation(command: &[String]) -> Option<Invocation> {
    let first = basename(command.first()?);
    let (mut index, mut separator, mut needs_separator) = (0, false, false);
    if !framework(first) {
        if !["npx", "bunx", "pnpx", "yarn", "pnpm", "npm"].contains(&first) {
            return None;
        }
        index = 1;
        let value_flags: &[&str] = match first {
            "npx" => &[
                "-c",
                "--call",
                "-p",
                "--package",
                "-w",
                "--workspace",
                "--allow-scripts",
            ],
            "pnpx" => &["-p", "--package"],
            "npm" => &["-c", "--call", "--package", "-w", "--workspace", "--prefix"],
            _ => &[],
        };
        let skip = |index: &mut usize, separator: &mut bool| {
            while let Some(arg) = command.get(*index).filter(|s| s.starts_with('-')) {
                *index += 1;
                if arg == "--" {
                    *separator = true;
                    break;
                }
                if !arg.contains('=') && value_flags.contains(&arg.as_str()) {
                    *index += 1;
                }
            }
        };
        skip(&mut index, &mut separator);
        if ["yarn", "pnpm", "npm"].contains(&first)
            && command
                .get(index)
                .is_some_and(|s| ["dlx", "exec", "x"].contains(&s.as_str()))
        {
            index += 1;
            skip(&mut index, &mut separator);
        }
        if !framework(basename(command.get(index)?)) {
            return None;
        }
        needs_separator = first == "npm" && !separator;
    }
    let name = basename(command.get(index)?).to_owned();
    let end = command
        .iter()
        .enumerate()
        .skip(index + 1)
        .find(|(_, s)| s.as_str() == "--")
        .map(|(i, _)| i);
    let insert = if needs_separator {
        command.len()
    } else {
        end.unwrap_or(command.len())
    };
    let mut args = command[index + 1..end.unwrap_or(command.len())].to_vec();
    let uncertain = needs_separator
        && args
            .windows(2)
            .any(|a| a[0].starts_with('-') && !a[0].contains('=') && !a[1].starts_with('-'));
    if needs_separator {
        args.retain(|a| !a.starts_with('-'));
        if let Some(e) = end {
            args.extend_from_slice(&command[e + 1..]);
        }
    }
    Some(Invocation {
        name,
        index,
        args,
        insert,
        separator: needs_separator && end.is_none(),
        uncertain,
    })
}

fn server_invocation(inv: &Invocation) -> bool {
    if inv.uncertain {
        return false;
    }
    let value_flags: Option<&[&str]> = match inv.name.as_str() {
        "vite" => Some(&[
            "--assetsDir",
            "--assetsInlineLimit",
            "--base",
            "--configLoader",
            "--host",
            "--manifest",
            "--minify",
            "--open",
            "--outDir",
            "--port",
            "--sourcemap",
            "--ssr",
            "--ssrManifest",
            "--target",
            "-c",
            "--config",
            "-d",
            "--debug",
            "-f",
            "--filter",
            "-l",
            "--logLevel",
            "-m",
            "--mode",
        ]),
        "rsbuild" => Some(&[
            "--base",
            "--config-loader",
            "--dist-path",
            "--env-dir",
            "--env-mode",
            "--environment",
            "--host",
            "--log-level",
            "--output",
            "--port",
            "-c",
            "--config",
            "-m",
            "--mode",
            "-o",
            "--open",
            "-r",
            "--root",
        ]),
        "astro" => Some(&[
            "--base", "--config", "--host", "--mode", "--open", "--outDir", "--port", "--root",
            "--site",
        ]),
        _ => None,
    };
    let mut i = 0;
    let sub = loop {
        let Some(a) = inv.args.get(i) else {
            break None;
        };
        if a == "--" {
            break None;
        }
        if !a.starts_with('-') {
            break Some(a.as_str());
        }
        if !a.contains('=') {
            match value_flags {
                Some(flags) if flags.contains(&a.as_str()) => i += 1,
                None => return false,
                _ => {}
            }
        }
        i += 1;
    };
    match inv.name.as_str() {
        "vite" => sub.is_none_or(|s| !["build", "optimize"].contains(&s)),
        "vitepress" => sub.is_none_or(|s| ["dev", "serve", "preview"].contains(&s)),
        "rsbuild" => sub.is_none_or(|s| ["dev", "preview"].contains(&s)),
        "vp" | "react-router" | "wrangler" => sub == Some("dev"),
        "astro" => matches!(sub, Some("dev" | "preview")),
        "ng" => matches!(sub, Some("serve" | "dev" | "s")),
        "react-native" => sub == Some("start"),
        "expo" => sub.is_none_or(|s| ["start", "serve"].contains(&s)),
        _ => false,
    }
}
fn has_option(args: &[String], flag: &str) -> bool {
    args.iter()
        .any(|a| a == flag || a.strip_prefix(flag).is_some_and(|s| s.starts_with('=')))
}
fn framework_flags(inv: &Invocation, port: u16, lan: bool) -> Vec<String> {
    if !server_invocation(inv) {
        return vec![];
    }
    let mut flags = Vec::new();
    if !has_option(&inv.args, "--port") {
        flags.extend(["--port".into(), port.to_string()]);
        if ["vite", "vitepress", "vp", "react-router"].contains(&inv.name.as_str()) {
            flags.push("--strictPort".into());
        }
    }
    let host_flag = if inv.name == "wrangler" {
        "--ip"
    } else {
        "--host"
    };
    let host_choice = has_option(&inv.args, host_flag)
        || inv.name == "expo"
            && ["--localhost", "--lan", "--tunnel"]
                .iter()
                .any(|f| has_option(&inv.args, f));
    if !(host_choice || inv.name == "expo" && lan) {
        flags.extend([
            host_flag.into(),
            if inv.name == "expo" {
                "localhost"
            } else {
                "127.0.0.1"
            }
            .into(),
        ]);
    }
    flags
}
fn unsafe_script(raw: &str) -> bool {
    let (mut single, mut double, mut escaped, mut word_start) = (false, false, false, true);
    let chars: Vec<_> = raw.chars().collect();
    for (i, c) in chars.iter().copied().enumerate() {
        if escaped {
            escaped = false;
            if c != '\n' && c != '\r' {
                word_start = false;
            }
            continue;
        }
        if c == '\\' && !single {
            escaped = true;
            continue;
        }
        if c == '\'' && !double {
            single = !single;
            word_start = false;
            continue;
        }
        if c == '"' && !single {
            double = !double;
            word_start = false;
            continue;
        }
        if single || double {
            continue;
        }
        if matches!(c, ';' | '\n' | '\r' | '|') || c == '#' && word_start {
            return true;
        }
        if c == '&'
            && !(i > 0
                && chars[i - 1] == '>'
                && chars
                    .get(i + 1)
                    .is_some_and(|c| c.is_ascii_digit() || *c == '-'))
        {
            return true;
        }
        word_start = c == ' ' || c == '\t';
    }
    false
}

/// Adapt direct frameworks and package scripts without retokenizing shell scripts.
pub fn adapt_command(
    command: &[String],
    cwd: &Path,
    port: u16,
    lan: bool,
) -> Result<(Vec<String>, Option<String>)> {
    if command.is_empty() {
        bail!("empty command");
    }
    let mut out = command.to_vec();
    let direct = framework_invocation(command);
    let package = if command.len() >= 3
        && ["npm", "pnpm", "yarn", "bun"].contains(&basename(&command[0]))
        && command[1] == "run"
        && !command[2].starts_with('-')
    {
        resolve_script_raw(&command[2], cwd)
    } else {
        None
    };
    let script_tokens = package.as_deref().map(split_command);
    // Keep the framework identity even for compound scripts whose argv cannot be altered.
    let resolved_name = direct.as_ref().map(|i| i.name.clone()).or_else(|| {
        script_tokens
            .as_deref()
            .and_then(framework_invocation)
            .map(|i| i.name)
    });
    if let Some(inv) = direct {
        let flags = framework_flags(&inv, port, lan);
        let mut inserted = Vec::new();
        if inv.separator && !flags.is_empty() {
            inserted.push("--".into());
        }
        inserted.extend(flags);
        out.splice(inv.insert..inv.insert, inserted);
    } else if let (Some(raw), Some(tokens)) = (package.as_deref(), script_tokens.as_deref()) {
        if !unsafe_script(raw) {
            if let Some(script_inv) = framework_invocation(tokens) {
                if !tokens.iter().skip(script_inv.index + 1).any(|s| s == "--") {
                    let mut probe = tokens.to_vec();
                    probe.extend(
                        command
                            .iter()
                            .skip(3)
                            .filter(|s| s.as_str() != "--")
                            .cloned(),
                    );
                    if let Some(inv) = framework_invocation(&probe) {
                        let flags = framework_flags(&inv, port, lan);
                        if !flags.is_empty() {
                            if basename(&command[0]) == "npm" && !out.iter().any(|s| s == "--") {
                                out.push("--".into());
                            }
                            out.extend(flags);
                        }
                    }
                }
            }
        }
    }
    Ok((
        out,
        if resolved_name.as_deref() == Some("expo") && lan {
            None
        } else {
            Some("127.0.0.1".into())
        },
    ))
}

/// Package-local executables win, then parent workspace bins, then the inherited PATH.
pub fn augmented_path(cwd: &Path) -> std::ffi::OsString {
    let mut paths: Vec<PathBuf> = cwd
        .ancestors()
        .map(|p| p.join("node_modules/.bin"))
        .collect();
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    std::env::join_paths(paths).unwrap_or_default()
}

fn is_porch_wrapper(raw: &str) -> bool {
    split_command(raw)
        .first()
        .is_some_and(|c| matches!(basename(c), "porch" | "portless"))
}

/// Turbo retains its own dependency graph and filters; the coordinator supplies app env via the loader.
pub fn turbo_command(cwd: &Path, script: Option<&str>) -> Result<Option<Vec<String>>> {
    if find_workspace_root(cwd).as_deref() != Some(cwd) {
        return Ok(None);
    }
    let config = load(cwd)?;
    if config["turbo"] == false
        || !(cwd.join("turbo.json").is_file() || cwd.join("turbo.jsonc").is_file())
    {
        return Ok(None);
    }
    let script = script.or(config["script"].as_str()).unwrap_or("dev");
    if resolve_script_raw(script, cwd).is_some_and(|raw| !is_porch_wrapper(&raw)) {
        return Ok(resolve_script_command(script, cwd));
    }
    let pm = detect_package_manager(cwd);
    Ok(Some(match pm.as_str() {
        "npm" => vec!["npx".into(), "turbo".into(), "run".into(), script.into()],
        "bun" => vec!["bunx".into(), "turbo".into(), "run".into(), script.into()],
        _ => vec![
            pm,
            "exec".into(),
            "turbo".into(),
            "run".into(),
            script.into(),
        ],
    }))
}

/// The Node preload chooses env by process.cwd(), preserving Turbo's graph and npm script hooks.
pub fn write_turbo_manifest(
    entries: &std::collections::BTreeMap<PathBuf, std::collections::BTreeMap<String, String>>,
) -> Result<(PathBuf, String)> {
    write_turbo_manifest_at(&crate::state::dir(), entries)
}

fn write_turbo_manifest_at(
    dir: &Path,
    entries: &std::collections::BTreeMap<PathBuf, std::collections::BTreeMap<String, String>>,
) -> Result<(PathBuf, String)> {
    fs::create_dir_all(dir)?;
    let manifest = dir.join(format!("dev-manifest-{}.json", std::process::id()));
    let loader = dir.join(format!("turbo-env-loader-{}.cjs", std::process::id()));
    let mut entries: std::collections::BTreeMap<_, _> = entries
        .iter()
        .map(|(cwd, entry)| {
            (
                canonical_cwd(cwd).unwrap_or_else(|_| cwd.clone()),
                entry.clone(),
            )
        })
        .collect();
    for (cwd, entry) in &mut entries {
        let command = entry
            .remove("_PORCH_COMMAND")
            .and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok());
        if let (Some(command), Some(port)) =
            (command, entry.get("PORT").and_then(|s| s.parse().ok()))
        {
            // When Turbo invokes a package's `dev: porch` wrapper while the
            // root already reserved its dev:app route, the inner launcher only
            // resolves/runs that script and inherits our assigned environment.
            let task = entry
                .remove("_PORCH_TURBO_SCRIPT")
                .unwrap_or_else(|| "dev".into());
            let wrapper = command.get(2) != Some(&task)
                && read_json(&cwd.join("package.json")).is_some_and(|pkg| {
                    pkg["scripts"][&task].as_str().is_some_and(is_porch_wrapper)
                });
            if wrapper {
                entry.insert("PORCH".into(), "0".into());
                entry.insert("PORTLESS".into(), "0".into());
            } else {
                entry.insert("PORCH_ACTIVE".into(), "1".into());
                entry.insert("PORTLESS_ACTIVE".into(), "1".into());
            }
            let lan = entry.get("PORTLESS_LAN").is_some_and(|s| s == "1");
            let direct = framework_invocation(&command);
            let name = direct.as_ref().map(|inv| inv.name.clone()).or_else(|| {
                if command.len() >= 3 && command[1] == "run" {
                    resolve_script_raw(&command[2], cwd)
                        .and_then(|s| framework_invocation(&split_command(&s)))
                        .map(|inv| inv.name)
                } else {
                    None
                }
            });
            let (adapted, _) = adapt_command(&command, cwd, port, lan)?;
            let count = adapted.len().saturating_sub(command.len());
            if let Some(name) = name.filter(|_| count > 0) {
                let start = direct.as_ref().map(|i| i.insert).unwrap_or(command.len());
                let flags: Vec<_> = adapted[start..start + count]
                    .iter()
                    .filter(|s| s.as_str() != "--")
                    .cloned()
                    .collect();
                entry.insert(
                    "_PORCH_FRAMEWORK".into(),
                    serde_json::to_string(&json!({"name":name,"flags":flags}))?,
                );
            }
        }
    }
    fs::write(&manifest, serde_json::to_vec_pretty(&entries)?)?;
    let source = format!(
        r#""use strict";
try {{
  var fs = require("fs"), path = require("path");
  var entries = JSON.parse(fs.readFileSync({}, "utf8"));
  var normalize = function(p) {{
    // The JS realpath implementation preserves Windows 8.3 aliases (RUNNER~1).
    // Native realpath expands them, matching Rust canonicalize.
    var resolved = fs.realpathSync.native(p);
    return process.platform === "win32" ? resolved.toLowerCase() : resolved;
  }};
  var cwd = normalize(process.cwd());
  var entry = Object.entries(entries).find(function(pair) {{ return normalize(pair[0]) === cwd; }});
  if (!entry && process.env.PORCH_DEBUG) console.error("Porch workspace lookup", cwd, Object.keys(entries));
  entry = entry && entry[1];
  if (entry) {{
    for (var key of Object.keys(entry)) if (!key.startsWith("_PORCH_")) process.env[key] = entry[key];
    var spec = entry._PORCH_FRAMEWORK && JSON.parse(entry._PORCH_FRAMEWORK);
    if (spec && process.argv[1]) {{
      var parts = process.argv[1].replace(/\\/g,"/").split("/");
      var bin = parts[parts.length-1].replace(/\.(?:c?js|mjs|cmd|exe)$/i, "");
      if (bin === spec.name || parts.includes(spec.name)) {{
        var args = process.argv.slice(2);
        var has = function(flag) {{ return args.some(function(a) {{return a === flag || a.startsWith(flag+"=");}}); }};
        for (var i=0; i<spec.flags.length; i++) {{
          var flag=spec.flags[i], value=spec.flags[i+1];
          if (value && !value.startsWith("--")) {{ i++; if (!has(flag)) process.argv.push(flag,value); }}
          else if (!has(flag)) process.argv.push(flag);
        }}
      }}
    }}
  }}
}} catch (error) {{ throw new Error("Porch could not load workspace environment: " + error.message); }}
"#,
        serde_json::to_string(&manifest)?
    );
    fs::write(&loader, source)?;
    let require = format!(
        "--require {}",
        serde_json::to_string(&loader.to_string_lossy())?
    );
    let options = match std::env::var("NODE_OPTIONS") {
        Ok(s) if !s.is_empty() => format!("{require} {s}"),
        _ => require,
    };
    Ok((manifest, options))
}

#[cfg(test)]
mod turbo_loader_tests {
    use super::*;
    use std::collections::BTreeMap;
    #[test]
    fn preload_injects_per_package_environment_and_framework_argv_only_into_framework() {
        let dir = std::env::temp_dir().join(format!("porch-turbo-config-{}", std::process::id()));
        let app = dir.join("apps/web");
        let state = dir.join("isolated-state with spaces");
        fs::create_dir_all(&app).unwrap();
        fs::write(
            app.join("package.json"),
            r#"{"scripts":{"dev":"vite dev"}}"#,
        )
        .unwrap();
        let record = app.join("vite.js");
        fs::write(&record,"console.log(JSON.stringify({port:process.env.PORT,url:process.env.PORTLESS_URL,args:process.argv.slice(2)}))").unwrap();
        let mut entries = BTreeMap::new();
        entries.insert(
            app.clone(),
            BTreeMap::from([
                ("PORT".into(), "4567".into()),
                ("PORTLESS_URL".into(), "https://web.localhost".into()),
                ("_PORCH_COMMAND".into(), r#"["npm","run","dev"]"#.into()),
            ]),
        );
        // On Windows TEMP commonly contains an 8.3 alias, while the manifest
        // canonicalizes it to its long name. Exercise both representations.
        let (_, options) = write_turbo_manifest_at(&state, &entries).unwrap();
        let result = Command::new("node")
            .arg(&record)
            .arg("dev")
            .current_dir(&app)
            .env("NODE_OPTIONS", &options)
            .env("PORCH_DEBUG", "1")
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let value: Value = serde_json::from_slice(&result.stdout).unwrap();
        assert_eq!(
            value["port"],
            "4567",
            "options={options}; stderr={}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(value["url"], "https://web.localhost");
        assert_eq!(
            value["args"],
            json!([
                "dev",
                "--port",
                "4567",
                "--strictPort",
                "--host",
                "127.0.0.1"
            ])
        );
        let other = app.join("npm.js");
        fs::copy(&record, &other).unwrap();
        let result = Command::new("node")
            .arg(&other)
            .arg("run")
            .arg("dev")
            .current_dir(&app)
            .env("NODE_OPTIONS", &options)
            .env("PORCH_DEBUG", "1")
            .output()
            .unwrap();
        let value: Value = serde_json::from_slice(&result.stdout).unwrap();
        assert_eq!(value["args"], json!(["run", "dev"]));
        fs::remove_dir_all(dir).unwrap();
    }
}
