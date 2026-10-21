//! The routing decisions, as pure functions over paths and JSON: which
//! workspace a client works in, which window is that workspace's (as the
//! instance's profile describes its window list), what path a tool call may
//! name, and which project opens a workspace's window.
use crate::profile::{Kind, List, PathArguments, Project};
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::path::{Component, Path, PathBuf};

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

/// The windows a window-list result names, as (target, path). `list.items`
/// points at the array of windows in the result; a text item that parses as
/// JSON is read the same way, and, since MCP mirrors `structuredContent` in
/// a text item, also with the pointer's leading `/structuredContent`
/// dropped. With `text_target` and `text_path`, text lines name windows
/// too. All window parsing is here and in `filter_windows`, which shares its
/// pieces.
pub fn windows(result: &Value, list: &List) -> Vec<(String, String)> {
    let mut found: Vec<(String, String)> = vec![];
    let mut add = |window: (String, String)| {
        if !found.contains(&window) {
            found.push(window);
        }
    };
    if let Some(items) = result.pointer(&list.items).and_then(Value::as_array) {
        items
            .iter()
            .filter_map(|w| window_entry(w, list))
            .for_each(&mut add);
    }
    for text in texts(result) {
        match serde_json::from_str::<Value>(text) {
            Ok(json) if text_items(&json, list).is_some() => text_items(&json, list)
                .into_iter()
                .flatten()
                .filter_map(|w| window_entry(w, list))
                .for_each(&mut add),
            _ => text
                .lines()
                .filter_map(|line| window_line(line, list))
                .for_each(&mut add),
        }
    }
    found
}

/// Each window path a list names, resolved: what a window belongs to is
/// worked out from these before the broker's state is locked, so no
/// realpath runs under it.
pub fn resolved_paths(result: &Value, list: &List) -> HashMap<String, Option<PathBuf>> {
    windows(result, list)
        .into_iter()
        .map(|(_, path)| {
            let real = resolve(Path::new(&path));
            (path, real)
        })
        .collect()
}

/// Keep only the windows `keep` accepts, in every form of the result. Text
/// lines that name no window stay.
pub fn filter_windows(result: &mut Value, list: &List, keep: impl Fn(&str, &str) -> bool) {
    let retain = |items: &mut Vec<Value>| {
        items.retain(|window| {
            window_entry(window, list).is_some_and(|(target, path)| keep(&target, &path))
        })
    };
    if let Some(items) = result
        .pointer_mut(&list.items)
        .and_then(Value::as_array_mut)
    {
        retain(items);
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
                Ok(mut json) if text_items(&json, list).is_some() => {
                    if let Some(items) = text_items_mut(&mut json, list) {
                        retain(items);
                    }
                    json.to_string()
                }
                _ => text
                    .lines()
                    .filter(|line| {
                        window_line(line, list).is_none_or(|(target, path)| keep(&target, &path))
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            };
            item["text"] = Value::String(filtered);
        }
    }
}

/// The pointers a text item's JSON is read with: `list.items`, and that
/// pointer below `/structuredContent`, whose mirror a text item is.
fn text_pointers(list: &List) -> Vec<&str> {
    let mut pointers = vec![list.items.as_str()];
    if let Some(rest) = list.items.strip_prefix("/structuredContent") {
        if rest.is_empty() || rest.starts_with('/') {
            pointers.push(rest);
        }
    }
    pointers
}

fn text_items<'a>(json: &'a Value, list: &List) -> Option<&'a Vec<Value>> {
    text_pointers(list)
        .into_iter()
        .find_map(|pointer| json.pointer(pointer).and_then(Value::as_array))
}

fn text_items_mut<'a>(json: &'a mut Value, list: &List) -> Option<&'a mut Vec<Value>> {
    let pointer = text_pointers(list)
        .into_iter()
        .find(|pointer| json.pointer(pointer).is_some_and(Value::is_array))?;
    json.pointer_mut(pointer).and_then(Value::as_array_mut)
}

fn window_entry(window: &Value, list: &List) -> Option<(String, String)> {
    Some((
        window.get(&list.target_field)?.as_str()?.to_owned(),
        window.get(&list.path_field)?.as_str()?.to_owned(),
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

/// One text line's window: the token after `text_target` (up to a comma,
/// semicolon or space) and the rest of the line after `text_path`. None
/// when the profile declares no text form.
fn window_line(line: &str, list: &List) -> Option<(String, String)> {
    let (target_prefix, path_prefix) = list.text.as_ref()?;
    let target = line.split_once(target_prefix.as_str())?.1;
    let target: String = target
        .chars()
        .take_while(|c| !c.is_whitespace() && *c != ',' && *c != ';')
        .collect();
    let path = line.split_once(path_prefix.as_str())?.1.trim();
    let path = path.trim_matches('"');
    (!target.is_empty() && !path.is_empty()).then(|| (target, path.to_owned()))
}

/// Whether a tool works on one window: its inputSchema takes the routing's
/// target argument.
pub fn window_bound(schema: &Value, target_argument: &str) -> bool {
    schema
        .get("properties")
        .and_then(Value::as_object)
        .is_some_and(|properties| properties.contains_key(target_argument))
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
/// in one of `PATH_SUFFIXES`, or one the profile adds.
fn path_argument(name: &str, extra: &PathArguments) -> bool {
    PATH_NAMES.contains(&name)
        || PATH_SUFFIXES.iter().any(|suffix| name.ends_with(suffix))
        || extra.names.iter().any(|n| n == name)
        || extra
            .suffixes
            .iter()
            .any(|suffix| name.ends_with(suffix.as_str()))
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
/// The built-in names are the floor; `extra` only adds to them.
pub fn check_paths(
    arguments: &Value,
    root: Option<&Path>,
    extra: &PathArguments,
) -> Result<(), String> {
    match arguments {
        Value::Object(map) => {
            for (name, value) in map {
                if path_argument(name, extra) {
                    let mut paths = vec![];
                    strings(value, &mut paths);
                    for path in paths {
                        check_path(name, path, root)?;
                    }
                }
                check_paths(value, root, extra)?;
            }
            Ok(())
        }
        Value::Array(items) => items
            .iter()
            .try_for_each(|item| check_paths(item, root, extra)),
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

/// The project whose window a workspace opens: searched for below each of
/// `bases` in turn (the client's working directory, then the root), at most
/// `max_depth` levels deep. The kinds are in preference order: a single
/// match of the first kind wins, then of the next; more than one of a kind
/// is refused with the candidates listed relative to `root`.
pub fn discover(bases: &[&Path], root: &Path, project: &Project) -> Result<PathBuf, String> {
    let mut seen: Vec<&Path> = vec![];
    for base in bases {
        if seen.contains(base) || !inside(base, root) {
            continue;
        }
        seen.push(base);
        let mut found: Vec<Vec<PathBuf>> = vec![vec![]; project.kinds.len()];
        if let Some(kind) = matched(base, project) {
            found[kind].push(base.to_path_buf());
        }
        let mut budget = DISCOVERY_ENTRIES;
        scan(base, 0, project, &mut found, &mut budget);
        for kind in found {
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
                        "more than one project in this workspace: {}; start the proxy with --project REL",
                        names.join(", ")
                    ));
                }
            }
        }
    }
    Err(format!(
        "no project in this workspace (looked for {})",
        project.looked_for()
    ))
}

/// The first kind `dir` is, if any. A file is looked at without following
/// a symlink.
fn matched(dir: &Path, project: &Project) -> Option<usize> {
    let name = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    project.kinds.iter().position(|kind| match kind {
        Kind::DirSuffix(suffix) => name.len() > suffix.len() && name.ends_with(suffix.as_str()),
        Kind::DirWithFile(file) => {
            fs::symlink_metadata(dir.join(file)).is_ok_and(|info| info.file_type().is_file())
        }
    })
}

/// Look below `dir`: a directory that matches a kind is recorded and never
/// descended into; hidden directories (with `skip_hidden`), `skip_dirs` and
/// symlinks are passed over.
fn scan(
    dir: &Path,
    depth: usize,
    project: &Project,
    found: &mut [Vec<PathBuf>],
    budget: &mut usize,
) {
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
        if !kind.is_dir()
            || (project.skip_hidden && name.starts_with('.'))
            || project.skip_dirs.iter().any(|skip| *skip == name)
        {
            continue;
        }
        let path = entry.path();
        match matched(&path, project) {
            Some(kind) => found[kind].push(path),
            None if depth + 1 < project.max_depth => scan(&path, depth + 1, project, found, budget),
            None => {}
        }
    }
}

/// The project a proxy's `--project REL` names: inside `root`, and of one
/// of the kinds (a file named by a `dir_with_file` kind stands for its
/// directory). With no project kinds, any directory inside `root`.
pub fn chosen_project(
    rel: &str,
    root: &Path,
    project: Option<&Project>,
) -> Result<PathBuf, String> {
    let shown: String = rel.chars().take(512).collect();
    let refused = || match project {
        Some(project) => format!(
            "--project {shown:?} is not a project inside this workspace (looked for {})",
            project.looked_for()
        ),
        None => format!("--project {shown:?} is not a directory inside this workspace"),
    };
    let path = Path::new(rel);
    if rel.is_empty() || path.is_absolute() || path.components().any(|c| c == Component::ParentDir)
    {
        return Err(refused());
    }
    let real = fs::canonicalize(root.join(path)).map_err(|_| refused())?;
    if !inside(&real, root) {
        return Err(refused());
    }
    let Some(project) = project else {
        return if real.is_dir() {
            Ok(real)
        } else {
            Err(refused())
        };
    };
    if real.is_dir() && matched(&real, project).is_some() {
        return Ok(real);
    }
    let name = real
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let names_a_kind = project
        .kinds
        .iter()
        .any(|kind| matches!(kind, Kind::DirWithFile(file) if *file == name));
    if real.is_file() && names_a_kind {
        return real.parent().map(Path::to_path_buf).ok_or_else(refused);
    }
    Err(refused())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::Routing;
    use serde_json::json;

    /// The routing of tests/fixtures/mcp-broker/fake_profile.json.
    fn routing() -> Routing {
        let profile: Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/mcp-broker/fake_profile.json"
        ))
        .unwrap();
        Routing::parse(&profile["routing"]).unwrap()
    }

    #[test]
    fn windows_parse_both_forms_and_filter_keeps_other_lines() {
        let list = routing().list;
        let mut result = json!({
            "structuredContent": {"windows": [
                {"windowId": "t1", "root": "/w/a/A.proj"},
                {"windowId": "t2", "root": "/w/b/B.proj"}]},
            "content": [{"type": "text", "text":
                "Windows:\n* windowId: t3, root: /w/a/C.proj\n* windowId: t4, root: /w/b/D.proj"},
                {"type": "text", "text": "{\"windows\":[{\"windowId\":\"t5\",\"root\":\"/w/b/E\"}]}"}]});
        let all = windows(&result, &list);
        assert_eq!(all.len(), 5);
        assert!(all.contains(&("t3".into(), "/w/a/C.proj".into())));
        filter_windows(&mut result, &list, |_, path| path.starts_with("/w/a/"));
        let kept = windows(&result, &list);
        assert_eq!(kept.len(), 2, "{result}");
        assert!(
            result["content"][0]["text"]
                .as_str()
                .unwrap()
                .starts_with("Windows:")
        );
        // Without a text form, lines name no window.
        let mut bare = list.clone();
        bare.text = None;
        let shown = json!({"content": [{"type": "text", "text": "windowId: t9, root: /w/a/X"}]});
        assert!(windows(&shown, &bare).is_empty());
        // Another pointer reads another place.
        bare.items = "/structuredContent/open".into();
        let elsewhere = json!({"structuredContent": {"open": [{"windowId": "t8", "root": "/r"}]}});
        assert_eq!(windows(&elsewhere, &bare), [("t8".into(), "/r".into())]);
    }

    #[test]
    fn a_tool_is_window_bound_when_its_schema_takes_the_target() {
        let target = routing().target_argument;
        assert!(window_bound(
            &json!({"properties": {"windowId": {}, "filePath": {}}}),
            &target
        ));
        assert!(!window_bound(
            &json!({"properties": {"query": {}}}),
            &target
        ));
        assert!(!window_bound(&Value::Null, &target));
    }

    #[test]
    fn paths_stay_inside_by_component_and_never_climb() {
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap().join("ws");
        fs::create_dir_all(root.join("src")).unwrap();
        let sibling = root.with_file_name("ws2");
        fs::create_dir_all(&sibling).unwrap();
        std::os::unix::fs::symlink(&sibling, root.join("src/out")).unwrap();
        let inside_url = format!("file://{}", root.join("src/a%20b.txt").display());
        let none = PathArguments::default();
        for ok in [
            json!({"filePath": root.join("src/new.txt"), "query": "/etc/passwd"}),
            json!({"filePaths": [root.join("src/a.txt"), "src/b.txt"]}),
            json!({"files": [["src/a.txt"]], "target": "App"}),
            json!({"uri": inside_url, "url": "https://example.invalid/x"}),
            json!({"source": "src/../src/a.txt"}),
        ] {
            assert!(check_paths(&ok, Some(&root), &none).is_ok(), "{ok}");
        }
        for bad in [
            json!({"filePath": sibling.join("x")}),
            json!({"path": root.join("new/../../ws2/x")}),
            json!({"projectDir": "src/../../x"}),
            json!({"nested": {"outputDirectory": "/tmp"}}),
            json!({"paths": ["src/a.txt", sibling]}),
            json!({"files": [["src/a.txt", "/etc/passwd"]]}),
            json!({"file": "src/out/x.txt"}),
            json!({"destination": "../ws2"}),
            json!({"source": "/etc"}),
            json!({"target": sibling}),
            json!({"uri": format!("file://{}", sibling.display())}),
            json!({"url": "file:///etc/passwd"}),
            json!({"documentURL": "file://otherhost/x"}),
            json!({"uri": format!("file://{}/%2e%2e/ws2", root.display())}),
        ] {
            assert!(check_paths(&bad, Some(&root), &none).is_err(), "{bad}");
        }
        assert!(check_paths(&json!({"filePath": root.join("src")}), None, &none).is_err());
        assert!(check_paths(&json!({"filePath": "src/a.txt"}), None, &none).is_ok());
        assert!(check_paths(&json!({"files": ["src/../x"]}), None, &none).is_err());
        assert!(check_paths(&json!({"url": "file:///tmp/x"}), None, &none).is_err());
        // A profile adds names and suffixes; it never removes the floor.
        let extra = PathArguments {
            names: vec!["where".into()],
            suffixes: vec!["Location".into()],
        };
        for bad in [
            json!({"where": "/etc"}),
            json!({"saveLocation": sibling}),
            json!({"filePath": "/etc"}),
        ] {
            assert!(check_paths(&bad, Some(&root), &extra).is_err(), "{bad}");
            assert!(check_paths(&bad, None, &extra).is_err(), "{bad}");
        }
        assert!(check_paths(&json!({"where": "/etc"}), Some(&root), &none).is_ok());
    }

    #[test]
    fn discovery_follows_the_kinds_in_order_and_skips_noise() {
        let project = routing().project.unwrap();
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        for dir in [
            "App.proj/Inner.proj",
            "vendor/Vendored.proj",
            ".hidden/Hidden.proj",
            "pkg",
        ] {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
        fs::write(root.join("pkg/Fake.toml"), "").unwrap();
        // A .proj wins over a Fake.toml directory; nothing inside a matched
        // directory, under vendor, or hidden counts.
        assert_eq!(
            discover(&[&root], &root, &project).unwrap(),
            root.join("App.proj")
        );
        fs::create_dir_all(root.join("sub/Other.proj")).unwrap();
        let error = discover(&[&root], &root, &project).unwrap_err();
        assert_eq!(
            error,
            "more than one project in this workspace: App.proj, sub/Other.proj; start the proxy with --project REL"
        );
        // The client's own directory is searched first.
        let sub = root.join("sub");
        assert_eq!(
            discover(&[&sub, &root], &root, &project).unwrap(),
            sub.join("Other.proj")
        );
        // Only a Fake.toml directory: it is the project.
        let plain = root.join("plain");
        fs::create_dir_all(plain.join("lib")).unwrap();
        fs::write(plain.join("lib/Fake.toml"), "").unwrap();
        assert_eq!(
            discover(&[&plain], &plain, &project).unwrap(),
            plain.join("lib")
        );
        let empty = root.join("sub/Other.proj");
        assert_eq!(
            discover(&[&empty], &empty, &project).unwrap(),
            empty,
            "a base that is a project is one"
        );
        let nothing = root.join("vendor/Vendored.proj/x");
        fs::create_dir_all(&nothing).unwrap();
        assert_eq!(
            discover(&[&nothing], &nothing, &project).unwrap_err(),
            "no project in this workspace (looked for .proj, Fake.toml)"
        );
        // --project is checked against the same kinds.
        assert_eq!(
            chosen_project("App.proj", &root, Some(&project)).unwrap(),
            root.join("App.proj")
        );
        assert_eq!(
            chosen_project("pkg/Fake.toml", &root, Some(&project)).unwrap(),
            root.join("pkg")
        );
        assert!(chosen_project("../x", &root, Some(&project)).is_err());
        assert!(chosen_project("vendor", &root, Some(&project)).is_err());
        assert_eq!(
            chosen_project("vendor", &root, None).unwrap(),
            root.join("vendor")
        );
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
