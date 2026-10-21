# Review gate setup: captain kit

This kit sets up the top-tier review gate and routes for GitHub and Forgejo repositories, as specified in the [review gate design](design/review-gate-and-route-strength.md).

Every step requiring direct manual operator execution is marked **[hands]**. Non-[hands] steps are performed by the runtime or crew PRs.

## Pending captain decisions (request `01ff0c50`)

Before or during rollout, four decisions are pending the captain's direction:

1. **Review list order (`["sol", "opus"]` vs `["opus", "sol"]`).**
   - *Recommendation:* `["sol", "opus"]`. This spares the weekly Opus plan cap that the captain shares. Because of the author overlap rule (§2), the order only matters when both models qualify.
   - *Status:* Pending decision.

2. **Requiring the macOS status check on GitHub (`runtime (macos-latest)`).**
   - *Context:* The macOS runner is known to fail intermittently. Requiring it in branch protection will block merges when it flakes; omitting it leaves macOS verification advisory.
   - *Status:* Pending decision.

3. **App repository rollout list.**
   - *Context:* Which Forgejo and GitHub repositories receive branch protection and review gating first (for example, `kirkins/privateer`, `kirkins/quarterdeck`, and `git.lobbykit.net/hermes/*`).
   - *Status:* Pending decision.

4. **Sol setup on the laptop.**
   - *Context:* Whether to set up the Pi `chatgpt-home` provider on this laptop, or have reviews of privateer's PRs run on the server or Mac where Sol is already configured.
   - *Status:* Pending decision.

---

## Part 1: Sol provider recognition and routes configuration

Configures a second top-tier review route (`sol`) running GPT-6.1 Sol on the captain's ChatGPT plan through Pi, and establishes the `review` route kind.

### 1. [hands] Copy the `chatgpt-home` provider configuration to the laptop (if Sol runs here)
*Pending captain decision 4.* If reviews of privateer's PRs run on this laptop, copy the provider configuration and credentials from the server:
- Copy the `chatgpt-home` provider definition from the server's `~/.pi/agent/models.json` to `~/.pi/agent/models.json` on the laptop.
- Copy the auth credentials from the server's `~/.pi/agent/auth.json` to `~/.pi/agent/auth.json` on the laptop.

### 2. [hands] Test Sol via Pi CLI
Verify that the `chatgpt-home` provider can reach the model on this machine:
```sh
pi --provider chatgpt-home --model gpt-6.1-sol --print hi
```
Ensure it returns a successful response with no auth or connection errors.

### 3. [hands] Update fleet routes with `sol` and the `review` list
Add `sol` to the fleet route registry at tier 1 with uses `["review", "diagnose", "design", "implement"]`, and set the `review` list.

*Note on list order:* Pending captain decision 1; design recommends `["sol", "opus"]`.

Export, update, and apply the route configuration:
```sh
privateer routes show | jq 'del(.applied)
 | .routes.sol = {
     "manifest": "bundled:pi",
     "options": {
       "model": "gpt-6.1-sol",
       "provider": "chatgpt-home"
     },
     "cost": "plan",
     "tier": 1,
     "uses": ["review", "diagnose", "design", "implement"],
     "note": "Pi + GPT-6.1 Sol on the captain'"'"'s ChatGPT plan. Second top-tier review route."
   }
 | .review = ["sol", "opus"]' > ~/routes-new.json && privateer routes set ~/routes-new.json
```

On machines where `sol` already exists (such as the server and the Mac), update only its `tier`, `uses`, and the `review` list instead of replacing the whole entry:
```sh
privateer routes show | jq 'del(.applied)
 | .routes.sol.tier = 1
 | .routes.sol.uses = ["review", "diagnose", "design", "implement"]
 | .review = ["sol", "opus"]' > ~/routes-new.json && privateer routes set ~/routes-new.json
```

> [!NOTE]
> - Sol is kept off all `crew` lists so that it never runs as an author by accident.
> - `privateer routes set` validates routes and warns if any route on a `review` list lacks `tier: 1` or the `review` use, or if any note retains a legacy `STRENGTH` prefix.
> - Models are normalised via `routes::model_key` (stripping `[...]` context tags, `-YYYYMMDD` date stamps, and `-free` variants).

---

## Part 2: Tokens, credentials, and branch protection

Configures forge credentials, repository branch protections, and forge adapter settings.

### Section A: GitHub setup (per repository)
*Repositories selected per pending captain decision 3 (e.g. `kirkins/privateer`, `kirkins/quarterdeck`).*

#### 1. [hands] Create reviewer machine account and store credential
1. Create a dedicated machine account on GitHub (for example, `<reviewer-account>`).
2. Add the machine account to the repository as a collaborator with **Write** access.
3. Generate a **classic** personal access token for the reviewer account with `repo` scope (classic tokens ensure access across personal repositories).
4. Store the token securely in Privateer's credential store (run in a terminal and paste the token at the prompt without echo; never pass secrets in command line arguments or pipes):
   ```sh
   privateer credential.set --json '{"ref":"forge-reviewer-github"}'
   ```

#### 2. [hands] Create fine-grained ship token and restrict administration
1. On the captain's account (`kirkins`), generate a fine-grained personal access token:
   - **Repository access:** Selected repositories only.
   - **Repository permissions:**
     - Contents: Read and write
     - Pull requests: Read and write
     - Workflows: Read and write
     - Actions: Read-only
     - Checks: Read-only
     - Commit statuses: Read-only
     - **Administration: No access (None)**
2. Replace the ship user's CLI login with this restricted token via interactive login (never pipe tokens with `--with-token`):
   ```sh
   gh auth login
   ```
   Follow the interactive prompts (choose `GitHub.com`, preferred protocol, and `Paste an authentication token`, then paste the token at the hidden prompt).
3. Store the token in Privateer's credential store for ship operations (run in a terminal and paste at the no-echo prompt):
   ```sh
   privateer credential.set --json '{"ref":"forge-ship-github"}'
   ```
4. Administrative repository changes (such as adjusting branch protection) now take place in the browser. The ship's SSH key remains for git pushes.

#### 3. Add Code Owners file (crew PR)
In a repository branch, add `.github/CODEOWNERS`:
```
* @<reviewer-account>
```
Submit as a PR and merge to `main`. Because GitHub enforces `CODEOWNERS` from the base branch, PRs cannot alter their own review requirement.

#### 4. [hands] Configure GitHub branch protection on `main`
In GitHub repository settings under **Branches** -> **Branch protection rules** for `main`, enable:
- **Require a pull request before merging:**
  - Required approvals: `1`
  - **Require review from Code Owners** (`require_code_owner_reviews`)
  - **Dismiss stale pull request approvals when new commits are pushed** (`dismiss_stale_reviews`)
  - **Require approval of the most recent push** (`require_last_push_approval`)
- **Require status checks to pass before merging:**
  - `runtime (ubuntu-latest)`
  - `runtime (macos-latest)` *(Pending captain decision 2: evaluate runner reliability)*
- **Do not allow bypassing the above settings:**
  - **Enforce for admins** (`enforce_admins: true`)
- **Block force pushes** (`allow_force_pushes: false`)
- **Block deletions** (`allow_deletions: false`)

Verify protection settings via Privateer: `forges.set` (Section C) reads each repository's protection back through `plugins/forge.py protection.read` and lists what is missing under `warnings`.

---

### Section B: Forgejo setup (per repository)
*Repositories selected per pending captain decision 3 (e.g. `git.lobbykit.net/hermes/*`).*

#### 1. [hands] Create service users and store credentials
1. In Forgejo, create three dedicated accounts:
   - `reviewer`: machine account for posting reviews.
   - `merger`: machine account holding exclusive merge permissions.
   - `ships`: machine account for ship API operations.
   All three accounts must be standard non-admin users added as write collaborators to the repository.
2. Store the tokens in Privateer's credential store (run in a terminal and paste each token at the no-echo prompt):
   ```sh
   privateer credential.set --json '{"ref":"forge-reviewer-forgejo"}'
   privateer credential.set --json '{"ref":"forge-merger-forgejo"}'
   ```
3. Move ships' forge API configuration to the `ships` user token.

#### 2. [hands] Configure Forgejo branch protection on default branch
In Forgejo repository settings under **Branch Protection** for the default branch (`main`), configure:
- **Enable push:** Unchecked (`enable_push: false`)
- **Apply to admins:** Checked (`apply_to_admins: true` — closes `force_merge`)
- **Required approvals:** `1` (`required_approvals: 1`)
- **Enable approvals whitelist:** Checked (`enable_approvals_whitelist: true`)
  - Whitelist usernames: `["reviewer"]`
- **Enable merge whitelist:** Checked (`enable_merge_whitelist: true`)
  - Whitelist usernames: `["merger"]`
- **Dismiss stale approvals:** Checked (`dismiss_stale_approvals: true`)
- **Block on rejected reviews:** Checked (`block_on_rejected_reviews: true`)
- **Enable status checks:** Checked (`enable_status_check: true`) with repository CI contexts.

#### 3. [hands] Test merge whitelist on a scratch repository
Before enabling on production repositories, verify on a scratch Forgejo repo:
1. Confirm a direct merge attempt using the ship token is refused by the forge whitelist.
2. Confirm `merge_when_checks_succeed` respects the merge whitelist.

---

### Section C: Fleet forge configuration (`forges.json`) and runtime adapter

#### 1. [hands] Fleet forge configuration (`forges.json`)
The fleet file `forges.json` sits beside the routes file in Privateer's fleet directory (`fleet.forges.json`, `crate::fleet::registry_path().with_extension("forges.json")`). It is captain-only and holds repository entries and credential refs, never secrets. A repository missing from it is not reviewed or merged by `review.start` and `pr.merge`.

Write it with the captain command [hands]; the reply's `warnings` list credentials not stored yet and branch protection settings that are missing:
```sh
privateer forges.set --file forges.json
```
Inspect with:
```sh
privateer forges.show --json '{}'
```

Each entry is keyed `host/owner/name` in lowercase and needs:
- `forge`: `github` or `forgejo`; `api`: the https API base; `branch`: the protected branch PRs merge into (never taken from a clone's `origin/HEAD`).
- `required_checks` (optional): the checks a merge needs green. Without it, the checks branch protection requires are used; a repository with neither is refused. Every check is never required, so a check the captain left out (such as `runtime (macos-latest)`) never blocks a merge.
- `reviewer` and `merger`: `{account, credential}`, two different accounts. The reviewer's token reads the PR and posts reviews. The merger's token merges: on GitHub the ship's own fine-grained token (`forge-ship-github`), on Forgejo the `merger` account's.

Example configuration schema:
```json
{
  "repos": {
    "github.com/kirkins/privateer": {
      "forge": "github",
      "api": "https://api.github.com",
      "branch": "main",
      "required_checks": ["runtime (ubuntu-latest)"],
      "reviewer": {
        "account": "<reviewer-account>",
        "credential": "forge-reviewer-github"
      },
      "merger": {
        "account": "kirkins",
        "credential": "forge-ship-github"
      }
    },
    "git.lobbykit.net/hermes/app": {
      "forge": "forgejo",
      "api": "https://git.lobbykit.net/api/v1",
      "branch": "main",
      "reviewer": {
        "account": "reviewer",
        "credential": "forge-reviewer-forgejo"
      },
      "merger": {
        "account": "merger",
        "credential": "forge-merger-forgejo"
      }
    }
  }
}
```

#### 2. Forge adapter protocol (`plugins/forge.py`)
The adapter implements the external interface:
- **Protocol:** `forge.py <op>` with JSON request on `stdin` and JSON response on `stdout`.
- **Operations:**
  - `pr.view`: Inspects PR state, base, head SHA, mergeability, and required checks at head.
  - `review.post`: Posts an approval or changes request. Searches for `<!-- privateer-review <id> -->` to guarantee idempotency.
  - `pr.merge`: Executes merge passing expected head SHA with `merge_method: "merge"` on GitHub and `Do: "merge"` on Forgejo (plain merge is the only permitted method).
  - `protection.read`: Validates that branch protection settings match required constraints.
- **Security & Token Isolation:**
  - The token is read strictly from the file path named by `PRIVATEER_FORGE_TOKEN_FILE`. Other token environment variables (`PRIVATEER_FORGE_TOKEN`, `GITHUB_TOKEN`, `FORGEJO_TOKEN`) are ignored; the request keys `token` and `token_file` are refused.
  - Any credential reference named in `forges.json` is banned from tool bundles (`tool_bundles::configure`) and native launch environments (`plugins/bundles.py`), preventing agent access.

#### 3. Residuals and threat model
Operating under a single OS user cannot protect against direct filesystem inspection by a rogue local process. The review gate prevents automated workflows from skipping or downgrading reviews. Complete credential and token isolation requires the future separate reviewer OS account.
