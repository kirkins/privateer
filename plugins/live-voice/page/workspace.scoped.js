// A read-only workspace has its own lifetime. It needs no audio or browser tools.
export function createWorkspace(api) {
  const $ = id => document.getElementById(id);
  let enabled = false, generation = 0, workspaceId = null, selected = null;
  let busy = false;

  function clearObservations() {
    $("fleetList").replaceChildren();
    $("fleetSummary").textContent = "Directory has not been checked.";
    $("directoryCoverage").textContent = "Not checked.";
    $("selectedShip").textContent = "Choose a ship";
    $("runtimeReport").textContent = "Select a ship to inspect its runtime observations.";
    $("workspaceError").textContent = "";
  }

  function setBusy(value) {
    busy = value;
    $("refreshWorkspace").disabled = value || !enabled;
    for (const button of $("fleetList").querySelectorAll("button")) button.disabled = value;
  }

  function reset() {
    generation++;
    workspaceId = null;
    selected = null;
    clearObservations();
    $("workspaceStatus").textContent = "Workspace disconnected.";
    setBusy(false);
  }

  function configure(features) {
    enabled = Array.isArray(features) && features.includes("workspace_reads");
    $("workspaceStatus").textContent = enabled ? "Ready to read fleet observations." : "Fleet workspace is unavailable with this door.";
    setBusy(false);
  }

  function checkResponse(result, ident, view) {
    if (!result || result.workspace_id !== ident || result.view !== view || !result.data)
      throw new Error("Could not check: workspace response did not match this reading.");
    return result.data;
  }

  function showCoverage(coverage) {
    const fragment = document.createDocumentFragment();
    const description = document.createElement("p");
    description.textContent = coverage.inventory_complete
      ? "Inventory is complete for this selection. Runtime coverage remains incomplete."
      : "Inventory is incomplete. Additional ships may be unavailable.";
    fragment.append(description);
    for (const source of coverage.sources) {
      const item = document.createElement("p");
      const stamp = source.observed_at === null ? "observation time unknown" : `observed ${new Date(source.observed_at).toLocaleString()}`;
      const age = source.age_ms === null ? "age unknown" : `reported age ${source.age_ms} ms`;
      item.textContent = `${source.machine}${source.ship ? "/" + source.ship : ""}: ${source.state}, ${source.mode}; ${stamp}; ${age}.`;
      fragment.append(item);
    }
    $("directoryCoverage").replaceChildren(fragment);
  }

  async function selectShip(name, current = generation) {
    if (busy || !workspaceId || current !== generation) return;
    selected = name;
    const ident = workspaceId;
    setBusy(true);
    $("selectedShip").textContent = name;
    $("runtimeReport").textContent = "Checking runtime observations…";
    $("workspaceError").textContent = "";
    for (const button of $("fleetList").querySelectorAll("button"))
      button.setAttribute("aria-pressed", String(button.dataset.ship === name));
    try {
      const response = await api("/api/workspace/read", {workspace_id: ident, view: "runtime", args: {ship: name}}, true);
      if (current !== generation || ident !== workspaceId) return;
      const data = checkResponse(response, ident, "runtime");
      if (typeof data.text !== "string" || typeof data.available !== "boolean")
        throw new Error("Could not check runtime observations.");
      $("runtimeReport").textContent = data.text;
      $("workspaceStatus").textContent = data.available ? "Reading received. See original observation times below." : "Runtime could not be checked.";
    } catch (error) {
      if (current !== generation || ident !== workspaceId) return;
      $("runtimeReport").textContent = "Runtime could not be checked. Refresh observations to try again.";
      $("workspaceError").textContent = error.message;
      $("workspaceStatus").textContent = "Runtime could not be checked.";
    } finally {
      if (current === generation && ident === workspaceId) setBusy(false);
    }
  }

  async function refresh() {
    if (!enabled || busy) return;
    const current = ++generation, previous = selected;
    workspaceId = null;
    clearObservations();
    setBusy(true);
    $("workspaceStatus").textContent = "Checking fleet directory…";
    try {
      const opened = await api("/api/workspace/open", {}, true);
      if (current !== generation) return;
      if (typeof opened.workspace_id !== "string" || !Array.isArray(opened.views))
        throw new Error("Could not open fleet workspace.");
      const ident = opened.workspace_id;
      workspaceId = ident;
      if (!opened.views.includes("directory")) {
        $("workspaceStatus").textContent = "Fleet reads are unavailable for this device’s grant.";
        $("fleetSummary").textContent = "Fleet directory was not checked.";
        return;
      }
      const response = await api("/api/workspace/read", {workspace_id: ident, view: "directory"}, true);
      if (current !== generation || ident !== workspaceId) return;
      const data = checkResponse(response, ident, "directory");
      if (!Array.isArray(data.names) || data.names.length > 200 || data.names.some(name => typeof name !== "string")
          || !data.coverage || !Array.isArray(data.coverage.sources))
        throw new Error("Could not check fleet directory.");
      const names = [...new Set(data.names)];
      $("fleetSummary").textContent = `${names.length} ship${names.length === 1 ? "" : "s"} listed in the observed directory.`;
      const fragment = document.createDocumentFragment();
      for (const name of names) {
        const item = document.createElement("li"), button = document.createElement("button");
        button.type = "button";
        button.textContent = name;
        button.dataset.ship = name;
        button.setAttribute("aria-pressed", "false");
        button.addEventListener("click", () => selectShip(name));
        item.append(button);
        fragment.append(item);
      }
      $("fleetList").replaceChildren(fragment);
      showCoverage(data.coverage);
      $("workspaceStatus").textContent = "Directory reading received. Select a ship to inspect runtime observations.";
      if (names.length === 0) $("runtimeReport").textContent = "No ships were listed. Coverage above describes what could be checked.";
      setBusy(false);
      if (previous && names.includes(previous)) await selectShip(previous, current);
    } catch (error) {
      if (current !== generation) return;
      clearObservations();
      $("workspaceError").textContent = error.message;
      $("workspaceStatus").textContent = "Fleet directory could not be checked.";
    } finally {
      if (current === generation) setBusy(false);
    }
  }

  $("refreshWorkspace").addEventListener("click", refresh);
  return {configure, refresh, reset, get id() { return workspaceId; }};
}
