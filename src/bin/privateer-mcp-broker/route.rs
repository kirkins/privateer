//! The routing decisions, as pure functions over paths and JSON: which
//! workspace a client works in, which Xcode window is that workspace's, what
//! path a tool call may name, and which project opens a workspace's window.
use serde_json::Value;
use std::fs;
use std::path::{Component, Path, PathBuf};

/// The tool that lists Xcode's windows.
pub const LIST_WINDOWS: &str = "XcodeListWindows";
/// The argument that names a window.
pub const TAB: &str = "tabIdentifier";
/// How deep below a directory project discovery looks.
const DISCOVERY_DEPTH: usize = 3;
/// Directory entries discovery reads before giving up on a tree.
const DISCOVERY_ENTRIES: usize = 20_000;

/// `path` resolved: the realpath of its deepest existing ancestor, with the
/// rest appended. `None` when the rest holds `..`, which no realpath vouches
/// for, or when the path is relative.
pub fn resolve(path: &Path) -> Option<PathBuf> {
    if !path.is_absolute() {
        return None;
    }
    let parts: Vec<Component> = path.components().collect();
    for split in (1..=parts.len()).rev() {
        let existing: PathBuf = parts[..split].iter().collect();
        let Ok(mut out) = fs::canonicalize(&existing) else {
            continue;
        };
        for part in &parts[split..] {
            match part {
                Component::Normal(name) => out.push(name),
                Component::CurDir => {}
                _ => return None,
            }
        }
        return Some(out);
    }
    None
}

/// Whether `path` is `root` or below it, by path components (so `/a/bc` is
/// not inside `/a/b`). Both must already be resolved.
pub fn inside(path: &Path, root: &Path) -> bool {
    path.starts_with(root)
}

/// A client's workspace root: the nearest ancestor of its resolved working
/// directory (or that directory itself) that holds a `.git` entry and lies
/// strictly inside one of `pools`. `None` means the client is unrouted.
pub fn workspace_root(cwd: &Path, pools: &[PathBuf]) -> Option<PathBuf> {
    let within = |dir: &Path| pools.iter().any(|pool| dir != pool && inside(dir, pool));
    cwd.ancestors()
        .take_while(|dir| within(dir))
        .find(|dir| fs::symlink_metadata(dir.join(".git")).is_ok())
        .map(Path::to_path_buf)
}

/// The windows an XcodeListWindows result names, as (tab, workspace path).
/// It reads `structuredContent.windows[{tabIdentifier, workspacePath}]` and
/// text lines that carry `tabIdentifier: X` and `workspacePath: Y`. The real
/// shape is only known on a Mac, so all the parsing is here.
pub fn windows(result: &Value) -> Vec<(String, String)> {
    let mut found: Vec<(String, String)> = vec![];
    let structured = result
        .pointer("/structuredContent/windows")
        .and_then(Value::as_array);
    for window in structured.into_iter().flatten() {
        if let (Some(tab), Some(path)) = (
            window.get(TAB).and_then(Value::as_str),
            window.get("workspacePath").and_then(Value::as_str),
        ) {
            found.push((tab.to_owned(), path.to_owned()));
        }
    }
    for text in texts(result) {
        for line in text.lines() {
            if let Some(window) = window_line(line) {
                if !found.contains(&window) {
                    found.push(window);
                }
            }
        }
    }
    found
}

/// Keep only the windows `keep` accepts, in both forms of the result. Text
/// lines that name no window stay.
pub fn filter_windows(result: &mut Value, keep: impl Fn(&str, &str) -> bool) {
    if let Some(list) = result
        .pointer_mut("/structuredContent/windows")
        .and_then(Value::as_array_mut)
    {
        list.retain(|window| {
            match (
                window.get(TAB).and_then(Value::as_str),
                window.get("workspacePath").and_then(Value::as_str),
            ) {
                (Some(tab), Some(path)) => keep(tab, path),
                _ => false,
            }
        });
    }
    if let Some(content) = result.get_mut("content").and_then(Value::as_array_mut) {
        for item in content {
            if item.get("type").and_then(Value::as_str) != Some("text") {
                continue;
            }
            let Some(text) = item.get("text").and_then(Value::as_str) else {
                continue;
            };
            let kept: Vec<&str> = text
                .lines()
                .filter(|line| window_line(line).is_none_or(|(tab, path)| keep(&tab, &path)))
                .collect();
            item["text"] = Value::String(kept.join("\n"));
        }
    }
}

fn texts(result: &Value) -> impl Iterator<Item = &str> {
    result
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|item| item.get("text").and_then(Value::as_str))
}

/// One text line's window: the token after `tabIdentifier: ` (up to a
/// comma or space) and the rest of the line after `workspacePath: `.
fn window_line(line: &str) -> Option<(String, String)> {
    let tab = line.split_once("tabIdentifier: ")?.1;
    let tab: String = tab
        .chars()
        .take_while(|c| !c.is_whitespace() && *c != ',' && *c != ';')
        .collect();
    let path = line.split_once("workspacePath: ")?.1.trim();
    let path = path.trim_matches('"');
    (!tab.is_empty() && !path.is_empty()).then(|| (tab, path.to_owned()))
}

/// Whether a tool works on one window: its inputSchema takes `tabIdentifier`.
pub fn window_bound(schema: &Value) -> bool {
    schema
        .get("properties")
        .and_then(Value::as_object)
        .is_some_and(|properties| properties.contains_key(TAB))
}

/// Whether an argument names a path: `path`, or a name ending in `Path`,
/// `path`, `Dir` or `Directory`.
fn path_argument(name: &str) -> bool {
    ["Path", "path", "Dir", "Directory"]
        .iter()
        .any(|suffix| name.ends_with(suffix))
}

/// Refuse a path argument that leaves the client's workspace: an absolute
/// path must resolve inside `root`, a relative one may not hold `..`, and an
/// unrouted client (no root) may pass no absolute path at all. Nested
/// objects and arrays are checked too.
pub fn check_paths(arguments: &Value, root: Option<&Path>) -> Result<(), String> {
    match arguments {
        Value::Object(map) => {
            for (name, value) in map {
                if path_argument(name) {
                    let paths: Vec<&str> = match value {
                        Value::String(text) => vec![text.as_str()],
                        Value::Array(items) => items.iter().filter_map(Value::as_str).collect(),
                        _ => vec![],
                    };
                    for path in paths {
                        check_path(name, path, root)?;
                    }
                }
                check_paths(value, root)?;
            }
            Ok(())
        }
        Value::Array(items) => items.iter().try_for_each(|item| check_paths(item, root)),
        _ => Ok(()),
    }
}

fn check_path(name: &str, text: &str, root: Option<&Path>) -> Result<(), String> {
    let path = Path::new(text);
    let shown: String = text.chars().take(512).collect();
    if path.is_absolute() {
        let Some(root) = root else {
            return Err(format!(
                "{name}: absolute paths work only from a Privateer task workspace"
            ));
        };
        return match resolve(path) {
            Some(real) if inside(&real, root) => Ok(()),
            _ => Err(format!(
                "{name} {shown:?} lies outside this workspace ({})",
                root.display()
            )),
        };
    }
    if path.components().any(|c| c == Component::ParentDir) {
        return Err(format!("{name} {shown:?} may not contain `..`"));
    }
    Ok(())
}

/// What discovery found below one directory.
#[derive(Default)]
struct Found {
    workspaces: Vec<PathBuf>,
    projects: Vec<PathBuf>,
    packages: Vec<PathBuf>,
}

/// The project whose window a workspace opens: searched for below each of
/// `bases` in turn (the client's working directory, then the root), at most
/// three levels deep. A single `.xcworkspace` wins, then a single
/// `.xcodeproj`, then a single `Package.swift` directory; more than one is
/// refused with the candidates listed relative to `root`.
pub fn discover(bases: &[&Path], root: &Path) -> Result<PathBuf, String> {
    let mut seen: Vec<&Path> = vec![];
    for base in bases {
        if seen.contains(base) || !inside(base, root) {
            continue;
        }
        seen.push(base);
        let mut found = Found::default();
        let mut budget = DISCOVERY_ENTRIES;
        scan(base, 0, &mut found, &mut budget);
        for kind in [found.workspaces, found.projects, found.packages] {
            match kind.len() {
                0 => continue,
                1 => return Ok(kind.into_iter().next().unwrap_or_default()),
                _ => {
                    let names: Vec<String> = kind
                        .iter()
                        .map(|p| {
                            let rel = p.strip_prefix(root).unwrap_or(p);
                            if rel.as_os_str().is_empty() {
                                ".".to_owned()
                            } else {
                                rel.display().to_string()
                            }
                        })
                        .collect();
                    return Err(format!(
                        "more than one Xcode project in this workspace: {}; start the proxy with --project REL",
                        names.join(", ")
                    ));
                }
            }
        }
    }
    Err("no .xcworkspace, .xcodeproj or Package.swift in this workspace".into())
}

fn scan(dir: &Path, depth: usize, found: &mut Found, budget: &mut usize) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = entries.filter_map(Result::ok).collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        if *budget == 0 {
            return;
        }
        *budget -= 1;
        let name = entry.file_name().to_string_lossy().into_owned();
        // Never followed: a symlink could lead out of the workspace.
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_file() && name == "Package.swift" {
            found.packages.push(dir.to_path_buf());
            continue;
        }
        if !kind.is_dir()
            || name.starts_with('.')
            || matches!(name.as_str(), "Pods" | "DerivedData" | "node_modules")
        {
            continue;
        }
        if name.ends_with(".xcworkspace") {
            found.workspaces.push(entry.path());
        } else if name.ends_with(".xcodeproj") {
            found.projects.push(entry.path());
        } else if depth + 1 < DISCOVERY_DEPTH {
            scan(&entry.path(), depth + 1, found, budget);
        }
    }
}

/// The project a proxy's `--project REL` names: inside `root`, and a
/// `.xcworkspace`, a `.xcodeproj` or a directory holding `Package.swift`.
pub fn chosen_project(rel: &str, root: &Path) -> Result<PathBuf, String> {
    let shown: String = rel.chars().take(512).collect();
    let refused = || format!("--project {shown:?} is not an Xcode project inside this workspace");
    let path = Path::new(rel);
    if rel.is_empty() || path.is_absolute() || path.components().any(|c| c == Component::ParentDir)
    {
        return Err(refused());
    }
    let real = fs::canonicalize(root.join(path)).map_err(|_| refused())?;
    if !inside(&real, root) {
        return Err(refused());
    }
    let name = real
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    if real.is_dir()
        && (name.ends_with(".xcworkspace")
            || name.ends_with(".xcodeproj")
            || real.join("Package.swift").is_file())
    {
        return Ok(real);
    }
    if real.is_file() && name == "Package.swift" {
        return real.parent().map(Path::to_path_buf).ok_or_else(refused);
    }
    Err(refused())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn windows_parse_both_forms_and_filter_keeps_other_lines() {
        let mut result = json!({
            "structuredContent": {"windows": [
                {"tabIdentifier": "t1", "workspacePath": "/w/a/A.xcodeproj"},
                {"tabIdentifier": "t2", "workspacePath": "/w/b/B.xcodeproj"}]},
            "content": [{"type": "text", "text":
                "Windows:\n* tabIdentifier: t3, workspacePath: /w/a/C.xcworkspace\n* tabIdentifier: t4, workspacePath: /w/b/D.xcodeproj"}]});
        let all = windows(&result);
        assert_eq!(all.len(), 4);
        assert!(all.contains(&("t3".into(), "/w/a/C.xcworkspace".into())));
        filter_windows(&mut result, |_, path| path.starts_with("/w/a/"));
        let kept = windows(&result);
        assert_eq!(kept.len(), 2, "{result}");
        assert!(
            result["content"][0]["text"]
                .as_str()
                .unwrap()
                .starts_with("Windows:")
        );
    }

    #[test]
    fn paths_stay_inside_by_component_and_never_climb() {
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap().join("ws");
        fs::create_dir_all(root.join("src")).unwrap();
        let sibling = root.with_file_name("ws2");
        fs::create_dir_all(&sibling).unwrap();
        let ok = json!({"filePath": root.join("src/new.swift"), "query": "/etc/passwd"});
        assert!(check_paths(&ok, Some(&root)).is_ok());
        for bad in [
            json!({"filePath": sibling.join("x")}),
            json!({"path": root.join("new/../../ws2/x")}),
            json!({"projectDir": "src/../../x"}),
            json!({"nested": {"outputDirectory": "/tmp"}}),
        ] {
            assert!(check_paths(&bad, Some(&root)).is_err(), "{bad}");
        }
        assert!(check_paths(&json!({"filePath": root.join("src")}), None).is_err());
        assert!(check_paths(&json!({"filePath": "src/a.swift"}), None).is_ok());
    }

    #[test]
    fn discovery_prefers_a_workspace_and_skips_noise() {
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        for dir in [
            "App.xcodeproj/project.xcworkspace",
            "App.xcworkspace",
            "Pods/Pods.xcworkspace",
            "DerivedData/X.xcworkspace",
            ".hidden/H.xcworkspace",
        ] {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
        assert_eq!(
            discover(&[&root], &root).unwrap(),
            root.join("App.xcworkspace")
        );
        fs::create_dir_all(root.join("sub/Other.xcworkspace")).unwrap();
        let error = discover(&[&root], &root).unwrap_err();
        assert!(error.contains("App.xcworkspace") && error.contains("sub/Other.xcworkspace"));
        // The client's own directory is searched first.
        let sub = root.join("sub");
        assert_eq!(
            discover(&[&sub, &root], &root).unwrap(),
            sub.join("Other.xcworkspace")
        );
        assert_eq!(
            chosen_project("App.xcworkspace", &root).unwrap(),
            root.join("App.xcworkspace")
        );
        assert!(chosen_project("../x", &root).is_err());
        assert!(chosen_project("Pods", &root).is_err());
    }

    #[test]
    fn a_root_is_the_git_checkout_strictly_inside_a_pool() {
        let temp = tempfile::tempdir().unwrap();
        let pool = fs::canonicalize(temp.path()).unwrap();
        let ws = pool.join("h/1/repo");
        fs::create_dir_all(ws.join("src/deep")).unwrap();
        fs::write(ws.join(".git"), "gitdir: x").unwrap();
        let pools = [pool.clone()];
        assert_eq!(
            workspace_root(&ws.join("src/deep"), &pools),
            Some(ws.clone())
        );
        assert_eq!(workspace_root(&ws, &pools), Some(ws.clone()));
        fs::write(pool.join(".git"), "").unwrap();
        assert_eq!(workspace_root(&pool.join("h"), &pools), None);
        assert_eq!(workspace_root(Path::new("/"), &pools), None);
    }
}
