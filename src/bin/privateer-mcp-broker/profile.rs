//! The routing profile: the one validator of what an instance declares
//! about its upstream (its label, extra path arguments, and how its windows
//! are listed, matched, opened and closed). `check-profile` runs it for
//! `privateer mcp-broker install`, and `serve` runs it again on what the
//! dispatcher passes. Unknown keys are refused at every level: a typo must
//! never silently turn routing off.
use serde_json::{Map, Value, json};
use std::io::Read;
use std::path::{Component, Path};

/// Executables a helper may not be: a shell would run whatever it is told.
const SHELLS: [&str; 12] = [
    "sh", "bash", "zsh", "dash", "ksh", "mksh", "fish", "csh", "tcsh", "env", "busybox", "xargs",
];
/// The most of anything a list may hold.
const MAX_ITEMS: usize = 32;

/// A whole profile file, as `check-profile` reads it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Profile {
    pub label: Option<String>,
    pub upstream: Option<Vec<String>>,
    pub path_arguments: PathArguments,
    pub routing: Option<Routing>,
}

/// Argument names that hold paths, beyond the built-in ones.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PathArguments {
    pub names: Vec<String>,
    pub suffixes: Vec<String>,
}

/// How an instance binds tools to windows.
#[derive(Clone, Debug, PartialEq)]
pub struct Routing {
    /// The argument that names a window; a tool whose inputSchema takes it
    /// is window-bound.
    pub target_argument: String,
    pub list: List,
    /// What a workspace's window is opened on; `None` opens the root.
    pub project: Option<Project>,
    pub open: Option<Vec<String>>,
    pub close: Option<Vec<String>>,
}

/// The tool that lists windows, and where its result names them.
#[derive(Clone, Debug, PartialEq)]
pub struct List {
    pub tool: String,
    /// A JSON pointer to the array of windows.
    pub items: String,
    pub target_field: String,
    pub path_field: String,
    /// The text form: the prefixes before a window's target and its path.
    pub text: Option<(String, String)>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Project {
    /// In preference order.
    pub kinds: Vec<Kind>,
    pub max_depth: usize,
    pub skip_dirs: Vec<String>,
    pub skip_hidden: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Kind {
    /// A directory whose name ends in this (".proj").
    DirSuffix(String),
    /// A directory holding a file of this name.
    DirWithFile(String),
}

impl Kind {
    /// How a refusal names it.
    pub fn shown(&self) -> &str {
        match self {
            Kind::DirSuffix(suffix) => suffix,
            Kind::DirWithFile(file) => file,
        }
    }
}

/// `check-profile`: a profile on stdin, its canonical JSON on stdout; or one
/// error line and exit 2.
pub fn check_stdin() -> i32 {
    let mut text = String::new();
    let read = std::io::stdin().take(1 << 20).read_to_string(&mut text);
    let parsed = read
        .map_err(|error| format!("cannot read the profile: {error}"))
        .and_then(|_| {
            serde_json::from_str::<Value>(&text)
                .map_err(|error| format!("the profile is not JSON: {error}"))
        })
        .and_then(|value| Profile::parse(&value));
    match parsed {
        Ok(profile) => {
            println!("{}", profile.json());
            0
        }
        Err(error) => {
            eprintln!("{}", error.replace('\n', " "));
            2
        }
    }
}

impl Profile {
    pub fn parse(value: &Value) -> Result<Profile, String> {
        let map = object(value, "the profile")?;
        known(map, "", &["label", "upstream", "path_arguments", "routing"])?;
        let label = map.get("label").map(check_label).transpose()?;
        let upstream = map
            .get("upstream")
            .map(|argv| {
                let argv = strings(argv, "upstream", MAX_ITEMS)?;
                check_argv(&argv, "upstream")?;
                Ok::<_, String>(argv)
            })
            .transpose()?;
        let path_arguments = map
            .get("path_arguments")
            .map(PathArguments::parse)
            .transpose()?
            .unwrap_or_default();
        let routing = map.get("routing").map(Routing::parse).transpose()?;
        Ok(Profile {
            label,
            upstream,
            path_arguments,
            routing,
        })
    }

    /// The canonical form: every default spelled out, keys in order.
    pub fn json(&self) -> Value {
        let mut out = Map::new();
        if let Some(label) = &self.label {
            out.insert("label".into(), json!(label));
        }
        if let Some(upstream) = &self.upstream {
            out.insert("upstream".into(), json!(upstream));
        }
        out.insert("path_arguments".into(), self.path_arguments.json());
        if let Some(routing) = &self.routing {
            out.insert("routing".into(), routing.json());
        }
        Value::Object(out)
    }
}

impl PathArguments {
    pub fn parse(value: &Value) -> Result<PathArguments, String> {
        let map = object(value, "path_arguments")?;
        known(map, "path_arguments.", &["names", "suffixes"])?;
        let list = |key: &str| -> Result<Vec<String>, String> {
            let at = format!("path_arguments.{key}");
            let names = match map.get(key) {
                Some(names) => strings(names, &at, MAX_ITEMS)?,
                None => vec![],
            };
            for name in &names {
                check_name(name, &at, 64)?;
            }
            Ok(names)
        };
        Ok(PathArguments {
            names: list("names")?,
            suffixes: list("suffixes")?,
        })
    }

    pub fn json(&self) -> Value {
        json!({"names": self.names, "suffixes": self.suffixes})
    }
}

impl Routing {
    pub fn parse(value: &Value) -> Result<Routing, String> {
        let map = object(value, "routing")?;
        known(
            map,
            "routing.",
            &["target_argument", "list", "project", "open", "close"],
        )?;
        let target_argument = required_name(map, "routing.", "target_argument")?;
        let list = List::parse(map.get("list").ok_or("routing.list is required")?)?;
        let project = map.get("project").map(Project::parse).transpose()?;
        let helper = |key: &str| -> Result<Option<Vec<String>>, String> {
            map.get(key)
                .map(|argv| {
                    let at = format!("routing.{key}");
                    let argv = strings(argv, &at, MAX_ITEMS)?;
                    check_helper(&argv, &at)?;
                    Ok(argv)
                })
                .transpose()
        };
        let open = helper("open")?;
        let close = helper("close")?;
        if open.is_some() && close.is_none() {
            return Err("routing.open requires routing.close: a window the broker opens must be closed on release".into());
        }
        Ok(Routing {
            target_argument,
            list,
            project,
            open,
            close,
        })
    }

    pub fn json(&self) -> Value {
        let mut out = Map::new();
        out.insert("target_argument".into(), json!(self.target_argument));
        out.insert("list".into(), self.list.json());
        if let Some(project) = &self.project {
            out.insert("project".into(), project.json());
        }
        if let Some(open) = &self.open {
            out.insert("open".into(), json!(open));
        }
        if let Some(close) = &self.close {
            out.insert("close".into(), json!(close));
        }
        Value::Object(out)
    }
}

impl List {
    fn parse(value: &Value) -> Result<List, String> {
        let map = object(value, "routing.list")?;
        known(
            map,
            "routing.list.",
            &[
                "tool",
                "items",
                "target_field",
                "path_field",
                "text_target",
                "text_path",
            ],
        )?;
        let at = "routing.list.";
        let tool = required_name(map, at, "tool")?;
        let items = match map.get("items") {
            Some(Value::String(pointer))
                if pointer.len() <= 256
                    && (pointer.is_empty() || pointer.starts_with('/'))
                    && !pointer.chars().any(char::is_control) =>
            {
                pointer.clone()
            }
            Some(_) => {
                return Err(
                    "routing.list.items is a JSON pointer: empty or starting with /, at most 256 characters"
                        .into(),
                );
            }
            None => return Err("routing.list.items is required".into()),
        };
        let target_field = required_name(map, at, "target_field")?;
        let path_field = required_name(map, at, "path_field")?;
        let text = match (map.get("text_target"), map.get("text_path")) {
            (None, None) => None,
            (Some(target), Some(path)) => Some((
                check_prefix(target, "routing.list.text_target")?,
                check_prefix(path, "routing.list.text_path")?,
            )),
            _ => {
                return Err(
                    "routing.list.text_target and routing.list.text_path come as a pair".into(),
                );
            }
        };
        Ok(List {
            tool,
            items,
            target_field,
            path_field,
            text,
        })
    }

    fn json(&self) -> Value {
        let mut out = json!({"tool": self.tool, "items": self.items,
            "target_field": self.target_field, "path_field": self.path_field});
        if let Some((target, path)) = &self.text {
            out["text_target"] = json!(target);
            out["text_path"] = json!(path);
        }
        out
    }
}

impl Project {
    fn parse(value: &Value) -> Result<Project, String> {
        let map = object(value, "routing.project")?;
        known(
            map,
            "routing.project.",
            &["kinds", "max_depth", "skip_dirs", "skip_hidden"],
        )?;
        let kinds = match map.get("kinds") {
            Some(Value::Array(kinds)) if (1..=16).contains(&kinds.len()) => kinds
                .iter()
                .map(Kind::parse)
                .collect::<Result<Vec<_>, _>>()?,
            Some(_) => return Err("routing.project.kinds is a list of 1 to 16 kinds".into()),
            None => return Err("routing.project.kinds is required".into()),
        };
        let max_depth = match map.get("max_depth") {
            None => 3,
            Some(depth) => depth
                .as_u64()
                .filter(|d| (1..=6).contains(d))
                .ok_or("routing.project.max_depth is 1 to 6")? as usize,
        };
        let skip_dirs = match map.get("skip_dirs") {
            Some(dirs) => strings(dirs, "routing.project.skip_dirs", MAX_ITEMS)?,
            None => vec![],
        };
        for dir in &skip_dirs {
            check_plain(dir, "routing.project.skip_dirs")?;
        }
        let skip_hidden = match map.get("skip_hidden") {
            None => false,
            Some(Value::Bool(skip)) => *skip,
            Some(_) => return Err("routing.project.skip_hidden is true or false".into()),
        };
        Ok(Project {
            kinds,
            max_depth,
            skip_dirs,
            skip_hidden,
        })
    }

    fn json(&self) -> Value {
        let kinds: Vec<Value> = self
            .kinds
            .iter()
            .map(|kind| match kind {
                Kind::DirSuffix(suffix) => json!({"dir_suffix": suffix}),
                Kind::DirWithFile(file) => json!({"dir_with_file": file}),
            })
            .collect();
        json!({"kinds": kinds, "max_depth": self.max_depth,
            "skip_dirs": self.skip_dirs, "skip_hidden": self.skip_hidden})
    }

    /// What discovery looks for, as a refusal lists it.
    pub fn looked_for(&self) -> String {
        self.kinds
            .iter()
            .map(Kind::shown)
            .collect::<Vec<_>>()
            .join(", ")
    }
}

impl Kind {
    fn parse(value: &Value) -> Result<Kind, String> {
        let at = "routing.project.kinds";
        let map = object(value, at)?;
        known(
            map,
            "routing.project.kinds[].",
            &["dir_suffix", "dir_with_file"],
        )?;
        match (map.get("dir_suffix"), map.get("dir_with_file")) {
            (Some(Value::String(suffix)), None) => {
                let rest = suffix.strip_prefix('.').unwrap_or("");
                let valid = suffix.len() <= 64
                    && !rest.is_empty()
                    && !suffix.contains('/')
                    && !suffix.chars().any(char::is_control);
                if !valid {
                    return Err(format!(
                        "{at}: dir_suffix {suffix:?} must start with \".\", hold no \"/\", and be at most 64 characters"
                    ));
                }
                Ok(Kind::DirSuffix(suffix.clone()))
            }
            (None, Some(Value::String(file))) => {
                check_plain(file, &format!("{at}: dir_with_file"))?;
                Ok(Kind::DirWithFile(file.clone()))
            }
            _ => Err(format!(
                "{at}: each kind is {{\"dir_suffix\": \".EXT\"}} or {{\"dir_with_file\": \"NAME\"}}"
            )),
        }
    }
}

fn object<'a>(value: &'a Value, what: &str) -> Result<&'a Map<String, Value>, String> {
    value
        .as_object()
        .ok_or_else(|| format!("{what} must be a JSON object"))
}

/// Refuse a key the profile does not define.
fn known(map: &Map<String, Value>, at: &str, keys: &[&str]) -> Result<(), String> {
    match map.keys().find(|key| !keys.contains(&key.as_str())) {
        Some(key) => Err(format!(
            "unknown profile key {at}{} (known here: {})",
            shown(key),
            keys.join(", ")
        )),
        None => Ok(()),
    }
}

fn shown(text: &str) -> String {
    let text: String = text.chars().take(64).collect();
    if text.chars().all(|c| c.is_ascii_graphic()) {
        text
    } else {
        format!("{text:?}")
    }
}

fn strings(value: &Value, at: &str, most: usize) -> Result<Vec<String>, String> {
    let list = value
        .as_array()
        .filter(|list| list.len() <= most)
        .ok_or_else(|| format!("{at} is a list of at most {most} strings"))?;
    list.iter()
        .map(|item| {
            item.as_str()
                .map(str::to_owned)
                .ok_or_else(|| format!("{at} is a list of strings"))
        })
        .collect()
}

fn required_name(map: &Map<String, Value>, at: &str, key: &str) -> Result<String, String> {
    let at = format!("{at}{key}");
    match map.get(key) {
        Some(Value::String(name)) => {
            check_name(name, &at, 128)?;
            Ok(name.clone())
        }
        Some(_) => Err(format!("{at} must be a string")),
        None => Err(format!("{at} is required")),
    }
}

/// A tool, argument or field name: 1 to `most` characters, no control
/// characters and no surrounding spaces.
fn check_name(name: &str, at: &str, most: usize) -> Result<(), String> {
    let valid = !name.is_empty()
        && name.chars().count() <= most
        && name.trim() == name
        && !name.chars().any(char::is_control);
    if valid {
        Ok(())
    } else {
        Err(format!(
            "{at}: {} is not a name of 1 to {most} characters",
            shown(name)
        ))
    }
}

fn check_prefix(value: &Value, at: &str) -> Result<String, String> {
    match value.as_str() {
        Some(prefix)
            if !prefix.trim().is_empty()
                && prefix.chars().count() <= 64
                && !prefix.chars().any(char::is_control) =>
        {
            Ok(prefix.to_owned())
        }
        _ => Err(format!("{at} is a string of 1 to 64 characters")),
    }
}

/// A plain file or directory name: no "/", not "." or "..".
fn check_plain(name: &str, at: &str) -> Result<(), String> {
    let valid = !name.is_empty()
        && name.len() <= 255
        && !name.contains('/')
        && name != "."
        && name != ".."
        && !name.chars().any(char::is_control);
    if valid {
        Ok(())
    } else {
        Err(format!("{at}: {} is not a plain name", shown(name)))
    }
}

fn check_label(value: &Value) -> Result<String, String> {
    match value.as_str() {
        Some(label)
            if !label.trim().is_empty()
                && label.chars().count() <= 32
                && !label.chars().any(char::is_control) =>
        {
            Ok(label.to_owned())
        }
        _ => Err("label is 1 to 32 printable characters".into()),
    }
}

/// The label `--label` gives serve, checked as a profile's.
pub fn label(text: &str) -> Result<String, String> {
    check_label(&json!(text))
}

/// An argv whose executable is an absolute, normalized path: 1 to 32
/// arguments, none longer than 4096 bytes or holding a control character.
pub fn check_argv(argv: &[String], what: &str) -> Result<(), String> {
    let valid = (1..=MAX_ITEMS).contains(&argv.len())
        && argv
            .iter()
            .all(|a| a.len() <= 4096 && !a.chars().any(char::is_control));
    if !valid {
        return Err(format!(
            "{what} has 1 to 32 arguments of at most 4096 bytes, without control characters"
        ));
    }
    let program = Path::new(&argv[0]);
    let normalized = program.is_absolute()
        && program
            .components()
            .all(|c| matches!(c, Component::RootDir | Component::Normal(_)))
        && !argv[0].ends_with('/')
        && !argv[0].contains("//");
    if !normalized {
        return Err(format!(
            "{what} must start with an absolute, normalized path"
        ));
    }
    Ok(())
}

/// A window helper: checked like the upstream, and never a shell.
pub fn check_helper(argv: &[String], what: &str) -> Result<(), String> {
    check_argv(argv, what)?;
    let base = Path::new(&argv[0])
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    if SHELLS.contains(&base.as_str()) {
        return Err(format!("{what} may not be {base}: a shell runs anything"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake() -> Value {
        json!({"label": "Fake", "routing": {
            "target_argument": "windowId",
            "list": {"tool": "ListWindows", "items": "/structuredContent/windows",
                     "target_field": "windowId", "path_field": "root",
                     "text_target": "windowId: ", "text_path": "root: "},
            "project": {"kinds": [{"dir_suffix": ".proj"}, {"dir_with_file": "Fake.toml"}],
                        "skip_dirs": ["vendor"]},
            "open": ["/usr/bin/true"], "close": ["/usr/bin/true"]}})
    }

    #[test]
    fn a_profile_round_trips_with_its_defaults_spelled_out() {
        let profile = Profile::parse(&fake()).unwrap();
        let canonical = profile.json();
        assert_eq!(canonical["routing"]["project"]["max_depth"], 3);
        assert_eq!(canonical["routing"]["project"]["skip_hidden"], false);
        assert_eq!(
            canonical["path_arguments"],
            json!({"names": [], "suffixes": []})
        );
        assert_eq!(Profile::parse(&canonical).unwrap(), profile);
        assert_eq!(
            profile.routing.unwrap().project.unwrap().looked_for(),
            ".proj, Fake.toml"
        );
    }

    #[test]
    fn typos_shells_and_bad_kinds_are_refused_with_their_cause() {
        let refused = |change: &dyn Fn(&mut Value), cause: &str| {
            let mut profile = fake();
            change(&mut profile);
            let error = Profile::parse(&profile).unwrap_err();
            assert!(error.contains(cause), "{error}");
        };
        refused(&|p| p["routng"] = json!({}), "unknown profile key routng");
        refused(
            &|p| p["routing"]["list"]["tol"] = json!("x"),
            "routing.list.tol",
        );
        refused(
            &|p| p["routing"]["project"]["kinds"][0]["dir_sufix"] = json!(".x"),
            "dir_sufix",
        );
        refused(
            &|p| p["routing"]["open"] = json!(["/bin/sh", "-c", "x"]),
            "may not be sh",
        );
        refused(
            &|p| {
                p["routing"].as_object_mut().unwrap().remove("close");
            },
            "requires routing.close",
        );
        refused(
            &|p| p["routing"]["project"]["kinds"] = json!([{"dir_suffix": "proj"}]),
            "dir_suffix",
        );
        refused(
            &|p| p["routing"]["project"]["kinds"] = json!([{"dir_suffix": "./x"}]),
            "dir_suffix",
        );
        refused(
            &|p| p["routing"]["project"]["max_depth"] = json!(7),
            "max_depth",
        );
        refused(
            &|p| {
                p["routing"]["list"]
                    .as_object_mut()
                    .unwrap()
                    .remove("text_path");
            },
            "pair",
        );
        refused(&|p| p["label"] = json!("x".repeat(33)), "label");
        refused(&|p| p["upstream"] = json!(["relative"]), "upstream");
        refused(
            &|p| p["path_arguments"] = json!({"names": vec!["a"; 33]}),
            "path_arguments.names",
        );
    }
}
