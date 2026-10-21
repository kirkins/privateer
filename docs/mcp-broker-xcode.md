# The Xcode example profile

Xcode's MCP server, `xcrun mcpbridge`, asks the user to Allow each new client
binary, and its tools work in whichever Xcode window a call names. Both make it
a fit for the [MCP broker](mcp-broker.md): with the broker in between, Xcode
only ever sees one client, and each crew's calls go to its own workspace's
window. Nothing about Xcode is built into Privateer. It is one profile,
[`docs/examples/mcp-broker/xcode.json`](examples/mcp-broker/xcode.json), which
`install` copies into the pinned config.

## Install

On the Mac, as the operator:

```sh
privateer mcp-broker install --name xcode --profile docs/examples/mcp-broker/xcode.json
```

The profile carries the upstream, so no `--upstream` is needed. The command
prints the `bundle_command` and `sha256` a crew bundle pins. The machine
owner's dispatcher then starts the instance and keeps it running (see
[Lifetime](mcp-broker.md#lifetime)), and `privateer mcp-broker status --name
xcode` reports it. Step 6 adds the opt-in bundle and registry entry that pin it
for crews.

## Why each key is set as it is

| Key | Value | Why |
|---|---|---|
| `label` | `Xcode` | Busy answers and refusals read "Xcode tools busy: ...", "Xcode window tools work only from a Privateer task workspace". |
| `upstream` | `/usr/bin/xcrun mcpbridge` | Xcode's MCP server, through the `xcrun` shim, which honours `DEVELOPER_DIR` (the one variable the broker passes beyond the minimal environment). |
| `routing.target_argument` | `tabIdentifier` | The argument Xcode's window-bound tools take. A tool whose schema takes it is routed to its workspace's window. |
| `routing.list.tool` | `XcodeListWindows` | The tool that lists Xcode's open windows. |
| `routing.list.items` | `/structuredContent/windows` | Where the window list is expected: `structuredContent.windows`, and the same JSON in a text item (MCP mirrors `structuredContent` there). |
| `routing.list.target_field`, `path_field` | `tabIdentifier`, `workspacePath` | Each window entry's tab and the workspace or project it shows. |
| `routing.list.text_target`, `text_path` | `"tabIdentifier: "`, `"workspacePath: "` | In case Xcode answers in text lines only (`* tabIdentifier: X, workspacePath: Y`). |
| `routing.project.kinds` | `.xcworkspace`, then `.xcodeproj`, then a directory with `Package.swift` | Xcode's three kinds of project, in the order Xcode itself prefers them: a workspace includes its projects and pods. |
| `routing.project.max_depth` | 3 | Projects sit at the root or a level or two below (`ios/App.xcodeproj`), and a deeper walk costs time on every first call. |
| `routing.project.skip_dirs` | `Pods`, `DerivedData`, `node_modules` | They hold generated or vendored projects (`Pods/Pods.xcworkspace`) that are never the workspace's own. |
| `routing.project.skip_hidden` | true | Hidden directories (`.build`, `.swiftpm`, `.git`) hold build products and package checkouts. A matched directory is never descended into, so `App.xcodeproj/project.xcworkspace` is not a second candidate either. |
| `routing.open` | `/usr/bin/open -g -a Xcode` | Opens the project (appended as the last argument) in Xcode. `-g` keeps the captain's focus; drop it if the Mac shows that the window then fails to open. |
| `routing.close` | `/usr/bin/osascript` with a fixed script, one `-e` per line | See below. |

**The close script.** It takes the recorded project path as `item 1 of argv`:
the path is the script's argument, never part of the script. If Xcode is
running, it closes, `saving no`, every workspace document whose `path` is that
path or starts with it followed by `/` (Xcode may report an `X.xcodeproj` as
`X.xcodeproj/project.xcworkspace`). It walks the documents last first, so
closing one does not shift the rest, and exits 0 when none matches. It never
launches Xcode just to close a window (`if application "Xcode" is running`).
The script stays inline in the JSON, so the config pin covers it; a script file
beside the profile would be unpinned.

## Tests

On Linux, with fakes (`cargo test --test mcp_broker`):

- `the_xcode_example_profile_routes_builds_and_discovers_its_projects`
  installs `--name xcode --profile docs/examples/mcp-broker/xcode.json`, with
  the fake helpers passed as `--window-open` and `--window-close`, and the fake
  upstream started with `--profile` on the same file, so it lists
  `XcodeListWindows` and takes `tabIdentifier`. The owner's dispatcher starts
  it. A `Build` with no `tabIdentifier` reaches the upstream with its own
  workspace's tab; another workspace's tab is refused. Discovery prefers the
  `.xcworkspace`, and skips `Pods`, `DerivedData`, `.build` and the workspace
  inside an `.xcodeproj`.
- `the_xcode_examples_close_script_is_a_valid_helper_that_holds_no_path`
  runs the file through `check-profile`, and checks that `close` starts with
  `/usr/bin/osascript`, has at most 32 elements, takes the path as its
  argument, and holds no path or `do shell`.

## Not verified (Mac only)

None of these can be checked on Linux:

- the real `xcrun mcpbridge` behind the broker;
- whether the Allow survives a broker restart, a Privateer upgrade and a broker
  reinstall;
- whether mcpbridge takes concurrent calls (the broker sends them as they
  come);
- `DEVELOPER_DIR` and `xcode-select` selection through the broker's minimal
  environment;
- whether Xcode treats the copied broker binary differently from the release's
  original;
- the real XcodeListWindows result shape, which tools take `tabIdentifier`, and
  whether Xcode's file tools take absolute or project-relative paths;
- `open -g -a Xcode PROJECT` opening a window that XcodeListWindows then lists,
  and how long that takes;
- the AppleScript close (`workspace document`, `path`, `saving no`, `starts
  with`, and walking the documents last first);
- that the close exits 0 when no document matches, and when Xcode is not
  running;
- which document path Xcode reports for an opened `.xcodeproj` (the project
  itself, or `X.xcodeproj/project.xcworkspace`) and for a `Package.swift`
  directory;
- a denied Automation prompt: the close then fails, so the release of every
  workspace with a recorded window is refused (and the sweep alerts its owner)
  until the Allow is given in System Settings. `privateer mcp-broker stop
  --name xcode` is the way out: with the instance stopped, release goes ahead
  and audits the window left open;
- whether any Xcode tool passes source text or a build target name under one
  of the path argument names (`source`, `target`); such a value is checked as a
  path, and source text could be refused;
- the one-time Automation prompt ("privateer-mcp-broker wants to control
  Xcode"), and which binary macOS holds responsible for it: the stable broker,
  or the launchd dispatcher's release binary. In the second case the Allow
  would lapse at every upgrade;
- whether Xcode writes non-ignored files (`xcshareddata/IDEWorkspaceChecks.plist`)
  into the worktree, which would block release's clean check;
- whether Treehouse's process check counts Xcode as a process in the worktree;
- the cwd from which Claude and Pi spawn the stdio proxy (expected: the job's
  workspace).
