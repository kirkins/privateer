//! The routing decisions, as pure functions over paths and JSON: which
//! workspace a client works in, which Xcode window is that workspace's, what
//! path a tool call may name, and which project opens a workspace's window.
use serde_json::Value;
use std::collections::HashMap;
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
/// It reads `structuredContent.windows[{tabIdentifier, workspacePath}]`, the
/// same JSON as a text item, and text lines that carry `tabIdentifier: X`
/// and `workspacePath: Y`. The real shape is only known on a Mac, so all the
/// parsing is here and in `filter_windows`, which shares its pieces.
pub fn windows(result: &Value) -> Vec<(String, String)> {
    let mut found: Vec<(String, String)> = vec![];
    let mut add = |window: (String, String)| {
        if !found.contains(&window) {
            found.push(window);
        }
    };
    if let Some(list) = result.get("structuredContent").and_then(window_list) {
        list.iter().filter_map(window_entry).for_each(&mut add);
    }
    for text in texts(result) {
        match serde_json::from_str::<Value>(text) {
            Ok(json) if window_list(&json).is_some() => window_list(&json)
                .into_iter()
                .flatten()
                .filter_map(window_entry)
                .for_each(&mut add),
            _ => text.lines().filter_map(window_line).for_each(&mut add),
        }
    }
    found
}

/// Each window path a list names, resolved: what a window belongs to is
/// worked out from these before the broker's state is locked, so no
/// realpath runs under it.
pub fn resolved_paths(result: &Value) -> HashMap<String, Option<PathBuf>> {
    windows(result)
        .into_iter()
        .map(|(_, path)| {
            let real = resolve(Path::new(&path));
            (path, real)
        })
        .collect()
}

/// Keep only the windows `keep` accepts, in every form of the result. Text
/// lines that name no window stay.
pub fn filter_windows(result: &mut Value, keep: impl Fn(&str, &str) -> bool) {
    let retain = |list: &mut Vec<Value>| {
        list.retain(|window| window_entry(window).is_some_and(|(tab, path)| keep(&tab, &path)))
    };
    if let Some(list) = result
        .get_mut("structuredContent")
        .and_then(window_list_mut)
    {
        retain(list);
    }
    if let Some(content) = result.get_mut("content").and_then(Value::as_array_mut) {
        for item in content {
            if item.get("type").and_then(Value::as_str) != Some("text") {
                continue;
            }
            let Some(text) = item.get("text").and_then(Value::as_str) else {
                continue;
            };
            let filtered = match serde_json::from_str::<Value>(text) {
                Ok(mut json) if window_list(&json).is_some() => {
                    if let Some(list) = window_list_mut(&mut json) {
                        retain(list);
                    }
                    json.to_string()
                }
                _ => text
                    .lines()
                    .filter(|line| window_line(line).is_none_or(|(tab, path)| keep(&tab, &path)))
                    .collect::<Vec<_>>()
                    .join("\n"),
            };
            item["text"] = Value::String(filtered);
        }
    }
}

/// The window list in a JSON value: its `windows` array, or the value itself
/// when it is an array.
fn window_list(value: &Value) -> Option<&Vec<Value>> {
    value
        .get("windows")
        .and_then(Value::as_array)
        .or_else(|| value.as_array())
}

fn window_list_mut(value: &mut Value) -> Option<&mut Vec<Value>> {
    if value.get("windows").is_some_and(Value::is_array) {
        return value.get_mut("windows").and_then(Value::as_array_mut);
    }
    value.as_array_mut()
}

fn window_entry(window: &Value) -> Option<(String, String)> {
    Some((
        window.get(TAB)?.as_str()?.to_owned(),
        window.get("workspacePath")?.as_str()?.to_owned(),
    ))
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

/// Argument names that hold a path, or a list of them.
const PATH_NAMES: [&str; 18] = [
    "path",
    "paths",
    "file",
    "files",
    "uri",
    "uris",
    "url",
    "urls",
    "source",
    "sources",
    "destination",
    "destinations",
    "target",
    "targets",
    "dir",
    "dirs",
    "directory",
    "directories",
];
/// Endings of argument names that hold a path (`filePath`, `outputDir`...).
const PATH_SUFFIXES: [&str; 14] = [
    "Path",
    "Paths",
    "path",
    "paths",
    "File",
    "Files",
    "Dir",
    "Dirs",
    "Directory",
    "Directories",
    "Uri",
    "URI",
    "Url",
    "URL",
];

/// Whether an argument names a path: one of `PATH_NAMES`, or a name ending
/// in one of `PATH_SUFFIXES`.
fn path_argument(name: &str) -> bool {
    PATH_NAMES.contains(&name) || PATH_SUFFIXES.iter().any(|suffix| name.ends_with(suffix))
}

/// Whether an argument names a URI: only a `file:` one is a path there.
fn uri_argument(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.ends_with("uri")
        || lower.ends_with("url")
        || lower.ends_with("uris")
        || lower.ends_with("urls")
}

/// Refuse a path argument that leaves the client's workspace. An absolute
/// path must resolve inside `root`; a relative one is taken from `root` and
/// must resolve inside it too. An unrouted client (no root) may pass no
/// absolute path at all, nor a relative one holding `..`. A `file:` URL is
/// checked as the path it names. Nested objects and arrays are checked too,
/// and so is every string in an array (at any depth) under a path name.
pub fn check_paths(arguments: &Value, root: Option<&Path>) -> Result<(), String> {
    match arguments {
        Value::Object(map) => {
            for (name, value) in map {
                if path_argument(name) {
                    let mut paths = vec![];
                    strings(value, &mut paths);
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

/// Every string in `value` and in the arrays under it.
fn strings<'a>(value: &'a Value, out: &mut Vec<&'a str>) {
    match value {
        Value::String(text) => out.push(text),
        Value::Array(items) => items.iter().for_each(|item| strings(item, out)),
        _ => {}
    }
}

fn check_path(name: &str, text: &str, root: Option<&Path>) -> Result<(), String> {
    let shown: String = text.chars().take(512).collect();
    let decoded;
    let text = match file_url(text) {
        Some(Ok(path)) => {
            decoded = path;
            decoded.as_str()
        }
        Some(Err(())) => return Err(format!("{name} {shown:?} is not a local file URL")),
        // Another scheme under a URI name (https:, for example) names no file.
        None if uri_argument(name) && has_scheme(text) => return Ok(()),
        None => text,
    };
    let path = Path::new(text);
    let outside = |root: &Path| {
        format!(
            "{name} {shown:?} lies outside this workspace ({})",
            root.display()
        )
    };
    match root {
        None if path.is_absolute() => Err(format!(
            "{name}: absolute paths work only from a Privateer task workspace"
        )),
        None if path.components().any(|c| c == Component::ParentDir) => {
            Err(format!("{name} {shown:?} may not contain `..`"))
        }
        None => Ok(()),
        Some(root) => match resolve(&root.join(path)) {
            Some(real) if inside(&real, root) => Ok(()),
            _ => Err(outside(root)),
        },
    }
}

/// Whether `text` starts with a URI scheme (`https:`, `file:`...).
fn has_scheme(text: &str) -> bool {
    let Some((scheme, _)) = text.split_once(':') else {
        return false;
    };
    let mut chars = scheme.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

/// The path a `file:` URL names, percent-decoded: `None` when `text` is not
/// one, an error when it names another host or does not decode.
fn file_url(text: &str) -> Option<Result<String, ()>> {
    let rest = text
        .get(..5)
        .filter(|scheme| scheme.eq_ignore_ascii_case("file:"))
        .map(|_| &text[5..])?;
    let path = match rest.strip_prefix("//") {
        Some(authority) => {
            let (host, path) = authority.split_at(authority.find('/').unwrap_or(authority.len()));
            if !(host.is_empty() || host.eq_ignore_ascii_case("localhost")) {
                return Some(Err(()));
            }
            path
        }
        None => rest,
    };
    let path = path.split(['?', '#']).next().unwrap_or("");
    let bytes = path.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] == b'%' {
            let hex = path
                .get(at + 1..at + 3)
                .and_then(|h| u8::from_str_radix(h, 16).ok());
            let Some(byte) = hex else {
                return Some(Err(()));
            };
            out.push(byte);
            at += 3;
        } else {
            out.push(bytes[at]);
            at += 1;
        }
    }
    Some(
        String::from_utf8(out)
            .map_err(|_| ())
            .and_then(|p| if p.contains('\0') { Err(()) } else { Ok(p) }),
    )
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
                "Windows:\n* tabIdentifier: t3, workspacePath: /w/a/C.xcworkspace\n* tabIdentifier: t4, workspacePath: /w/b/D.xcodeproj"},
                {"type": "text", "text": "{\"windows\":[{\"tabIdentifier\":\"t5\",\"workspacePath\":\"/w/b/E\"}]}"}]});
        let all = windows(&result);
        assert_eq!(all.len(), 5);
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
        std::os::unix::fs::symlink(&sibling, root.join("src/out")).unwrap();
        let inside_url = format!("file://{}", root.join("src/a%20b.swift").display());
        for ok in [
            json!({"filePath": root.join("src/new.swift"), "query": "/etc/passwd"}),
            json!({"filePaths": [root.join("src/a.swift"), "src/b.swift"]}),
            json!({"files": [["src/a.swift"]], "target": "App"}),
            json!({"uri": inside_url, "url": "https://developer.apple.com/x"}),
            json!({"source": "src/../src/a.swift"}),
        ] {
            assert!(check_paths(&ok, Some(&root)).is_ok(), "{ok}");
        }
        for bad in [
            json!({"filePath": sibling.join("x")}),
            json!({"path": root.join("new/../../ws2/x")}),
            json!({"projectDir": "src/../../x"}),
            json!({"nested": {"outputDirectory": "/tmp"}}),
            json!({"paths": ["src/a.swift", sibling]}),
            json!({"files": [["src/a.swift", "/etc/passwd"]]}),
            json!({"file": "src/out/x.swift"}),
            json!({"destination": "../ws2"}),
            json!({"source": "/etc"}),
            json!({"target": sibling}),
            json!({"uri": format!("file://{}", sibling.display())}),
            json!({"url": "file:///etc/passwd"}),
            json!({"documentURL": "file://otherhost/x"}),
            json!({"uri": format!("file://{}/%2e%2e/ws2", root.display())}),
        ] {
            assert!(check_paths(&bad, Some(&root)).is_err(), "{bad}");
        }
        assert!(check_paths(&json!({"filePath": root.join("src")}), None).is_err());
        assert!(check_paths(&json!({"filePath": "src/a.swift"}), None).is_ok());
        assert!(check_paths(&json!({"files": ["src/../x"]}), None).is_err());
        assert!(check_paths(&json!({"url": "file:///tmp/x"}), None).is_err());
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
