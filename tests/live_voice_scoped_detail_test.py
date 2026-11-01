"""PR6b metadata renderers: exact refs, honest coverage, no authored speech."""
import copy
import pathlib
import sys
import unittest

sys.dont_write_bytecode = True
sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent.parent / "plugins/live-voice"))
import scoped_detail as detail

SNAPSHOT = (("fleet:read", "mail:read"), ("alpha",))
TOKEN = "11111111-1111-4111-8111-111111111111"
REF = dict(origin="alpha-origin", incarnation="alpha-incarnation", ship="alpha", task_id="alpha-task", id="alpha-job")


def age(basis="creation", at=100, elapsed=20):
    return dict(basis=basis, at_ms=at, age_ms=elapsed)


def job(state="queued"):
    completed = state in detail.JOB_STATES[6:]
    return dict(reference=copy.deepcopy(REF), owner="alpha", worker=None, harness="pi", kind="command",
                state=state, completed=completed, liveness="unknown_recorded_state",
                wait=dict(kind="capacity", job=None, task_id=None, reference_withheld=True,
                          since=age(), next=dict(text="next-canary")),
                created=age(), spawned=age("payload_spawn", None, None), finished=age("finish", None, None),
                state_since=age("state_transition", None, None), exit=state if completed else "not_recorded",
                progress=dict(text="progress-canary", at=age("recorded_text")), error=None,
                parent=None, retry_of=None, resume_of=None, relaunch_of=None, resumed_as=None,
                command=["argv-canary"], path="path-canary")


def projection(command="ship_jobs"):
    work = command == "ship_work"
    needs = command == "runtime_needs"
    cap = command == "machine_capacity"
    inspection = not (needs or cap)
    c = dict(schema="coverage.v1", scope="selected-ship", complete=False, machine="beta", ship="alpha",
             facet="work" if work else "needs" if needs else "capacity" if cap else "jobs",
             state="covered", mode="remote_live", observed_at=120, other_machines="not_requested")
    raw = dict(schema="inspection.v1" if inspection else "capacity-needs.v1", coverage=c,
               more=False, continuation=None, writes="none", cache_writes="bounded_registry_continuations_only")
    if inspection:
        c.update(counts_complete=False, count_basis="authorized_scan_window",
                 history="recorded_tasks" if work else "retained_only_prior_history_unknown")
        raw.update(ship="alpha", source=REF["origin"], snapshot=TOKEN, counts=dict(queued=2, running=0) if not work else {},
                   jobs=[job()] if command == "ship_jobs" else [], job=job() if command == "job_inspect" else None,
                   work=[])
    else:
        c.update(mail_complete=False)
        raw.update(capacity=None, needs=[], unknown=["aggregate_mail_coverage", "provider_health"], action_authority="none")
    if needs:
        raw["needs"] = [dict(id="alpha-need", ship="alpha", kind="decision", source="mail", evidence=None,
                             created_ms=None, deadline_ms=None, observed_ms=120, question="question-canary",
                             choices=["choices-canary"], text_withheld=False)]
    if cap:
        raw["capacity"] = dict(ledger="selected_ship", shared_host="withheld", policy=None, readings=None,
            route_availability=None, route_state="withheld", charter_heavy_cap=None,
            charter_occupied_heavy_slots=None, free_heavy_slots=None, charter_policy_state="withheld",
            charter_policy_source="unknown", headroom_basis="heavy_slots_only_not_agent_capacity",
            occupied_heavy_slots=0, reservation_count=0, memory_reservation_count=0, holds=[],
            queue_depth=1, queue=[dict(job_id="alpha-job", created_ms=100, blocked="capacity", blocked_at_ms=None,
                                    blocker_basis="unattributed_capacity_wait")], capacity_blocked=True)
    return raw


class DetailSpeechTest(unittest.TestCase):
    def render(self, command="ship_jobs", raw=None, args=None, snapshot=SNAPSHOT):
        return detail.render(raw if raw is not None else projection(command), snapshot, command,
                             args if args is not None else dict(ship="alpha", **({"machine": "beta"} if command == "machine_capacity" else {})),
                             clock=lambda: 180120)

    def test_tools_and_requests_fixed_metadata_only(self):
        self.assertEqual({s["name"] for s in detail.tools()}, {"privateer_" + c for c in detail.COMMANDS})
        for command in detail.COMMANDS:
            args = dict(machine="beta") if command == "machine_capacity" else dict(ship="alpha")
            if command == "job_inspect":
                args["reference"] = REF
            _, actual = detail.request(command, args)
            self.assertEqual(actual, args if command == "machine_capacity" else dict(args, include_text=False))
            for key, value in (("include_text", True), ("include_text", False), ("command", "send"), ("path", "/canary")):
                with self.assertRaises(ValueError):
                    detail.request(command, dict(args, **{key: value}))
        for args in ({"ship": "alpha", "limit": 0}, {"ship": "alpha", "limit": True},
                     {"ship": "alpha", "continuation": "guess"}, {"ship": "alpha", "state": "idle"}):
            with self.assertRaises(ValueError):
                detail.request("ship_jobs", args)
        for r in (dict(REF, ship="beta"), dict(REF, command="send"), {"id": REF["id"]}):
            with self.assertRaises((KeyError, ValueError)):
                detail.request("job_inspect", dict(ship="alpha", reference=r))

    def test_counts_scan_window_history_ages_and_text_withheld(self):
        text, metadata = self.render()
        self.assertIn("Count basis: authorized scan window; partial; could not check all", text)
        self.assertIn("queued at least 2 observed, running could not check", text)
        self.assertIn("prior pruned history unknown", text)
        self.assertIn("time unknown, age unknown, basis state transition", text)
        self.assertIn("at 100, age 20 milliseconds, basis creation, about 3 minutes ago", text)
        self.assertLess(text.index("coverage:"), text.index("Recorded counts"))
        self.assertIn("Progress or question text is withheld", text)
        self.assertIn("Wait capacity", text)
        self.assertEqual(metadata["references"], [REF])
        for canary in ("progress-canary", "next-canary", "argv-canary", "path-canary"):
            self.assertNotIn(canary, text + str(metadata))
        self.assertTrue(text.endswith(detail.MAIL))

    def test_exact_detail_and_terminal_lineage(self):
        raw = projection("job_inspect")
        for state in detail.JOB_STATES:
            raw["job"] = job(state)
            raw["job"]["resume_of"] = dict(REF, id="alpha-parent")
            text, metadata = self.render("job_inspect", raw, dict(ship="alpha", reference=REF))
            self.assertIn("state " + state.replace("_", " "), text)
            self.assertIn("resume of recorded job", text)
            self.assertEqual(metadata["references"], [REF, dict(REF, id="alpha-parent")])
            self.assertNotIn("alpha-parent", text)
            if state == "resumed":
                self.assertIn("Resumed is lineage, not success", text)
        raw["job"]["reference"]["incarnation"] = "replacement"
        self.assertEqual(self.render("job_inspect", raw, dict(ship="alpha", reference=REF)), (detail.UNAVAILABLE, {}))

    def test_page_and_speech_omissions_preserve_exact_metadata(self):
        raw = projection()
        raw.update(more=True, continuation=TOKEN)
        raw["jobs"] = [dict(job(), reference=dict(REF, id="alpha-" + str(i))) for i in range(50)]
        text, metadata = self.render(raw=raw)
        self.assertLessEqual(len(text), detail.SPEECH_BUDGET)
        self.assertIn("metadata rows not read out", text)
        self.assertLess(text.index("metadata rows not read out"), text.index("Recorded counts"))
        self.assertIn("More recorded pages available", text)
        self.assertEqual(metadata["continuation"], TOKEN)
        self.assertEqual([r["id"] for r in metadata["references"]], ["alpha-" + str(i) for i in range(50)])
        self.assertNotIn(TOKEN, text)
        self.assertNotIn("alpha-incarnation", text)

    def test_work_dependency_claims_and_ages_are_not_workers(self):
        raw = projection("ship_work")
        raw["work"] = [dict(task_id="alpha-task", owner="alpha", task_state="open", state="waiting",
            reference=dict(origin=REF["origin"], ship="alpha", task_id="alpha-task", revision=9),
            scheduled=True, backlog=True, claimed=True, offered=False, job=None,
            created=age("schedule_creation"), last_checked=age("schedule_check"), next_check=age("not_before", None, None),
            wait=dict(kind="task", task_id="alpha-dependency", job=None, reference_withheld=False,
                      since=age("wait_transition_unknown", None, None), next=None), reason=dict(text="reason-canary"))]
        text, metadata = self.render("ship_work", raw)
        self.assertIn("Recorded tasks only; not workers", text)
        self.assertIn("recorded dependency task", text)
        self.assertEqual(metadata["task_ids"], ["alpha-task", "alpha-dependency"])
        self.assertNotIn("alpha-task", text)
        self.assertNotIn("alpha-dependency", text)
        self.assertIn("claimed yes; offered no", text)
        self.assertIn("basis wait transition unknown", text)
        self.assertEqual(metadata["work_references"][0]["revision"], 9)
        self.assertNotIn("reason-canary", text + str(metadata))

    def test_restricted_host_withholds_shared_facts(self):
        raw = projection("machine_capacity")
        text, _ = self.render("machine_capacity", raw)
        self.assertIn("Shared host policy, headroom, readings, totals and routes withheld", text)
        self.assertIn("Selected-ship recorded occupied heavy slots 0", text)
        self.assertIn("unattributed capacity wait", text)
        self.assertNotIn("Heavy-slot headroom 0", text)
        for blocked in (None, "cpu", "heavy_slot"):
            raw["capacity"]["queue"][0]["blocked"] = blocked
            self.assertEqual(self.render("machine_capacity", raw), (detail.UNAVAILABLE, {}))
        raw["capacity"]["queue"][0]["blocked"] = "capacity"
        raw["capacity"]["free_heavy_slots"] = 12
        self.assertEqual(self.render("machine_capacity", raw), (detail.UNAVAILABLE, {}))

    def test_unrestricted_finite_null_unbounded_and_unknown_caps(self):
        raw = projection("machine_capacity")
        cap = raw["capacity"]
        cap.update(shared_host="visible", policy=dict(configured_host_heavy_cap=None, resolved_host_heavy_cap=None,
                   sources=dict(max_heavy_jobs="runtime_default")),
                   charter_policy_state="known", charter_policy_source="active_task_charter", charter_heavy_cap=None,
                   queue=[], queue_depth=0)
        text, _ = self.render("machine_capacity", raw, snapshot=(("fleet:read",), None))
        self.assertIn("Host configured heavy cap not configured; runtime default", text)
        self.assertNotIn("Host configured heavy cap unbounded", text)
        self.assertIn("resolved heavy cap unbounded by this policy", text)
        cap["policy"]["sources"]["max_heavy_jobs"] = "host_configuration"
        configured_text, _ = self.render("machine_capacity", raw, snapshot=(("fleet:read",), None))
        self.assertIn("Host configured heavy cap unbounded by this policy", configured_text)
        self.assertNotIn("not configured", configured_text)
        self.assertIn("Charter policy known, source active task charter; heavy cap unbounded by this policy", text)
        self.assertIn("could not check finite headroom", text)
        self.assertNotIn("free agents", text)
        cap.update(free_heavy_slots=2, charter_policy_state="unknown")
        cap["policy"].update(configured_host_heavy_cap=4, resolved_host_heavy_cap=4)
        text, _ = self.render("machine_capacity", raw, snapshot=(("fleet:read",), None))
        self.assertIn("Host configured heavy cap 4; resolved heavy cap 4", text)
        self.assertIn("Charter policy unknown", text)
        self.assertIn("not a promise of admission", text)
        cap["readings"] = dict(cpus=2, load=1.5, released_load=0, recent_cpus=1,
            memory_available_bytes=200, memory_total_bytes=400, recent_memory_bytes=100,
            disks=[dict(state="unknown", available_bytes=None, total_bytes=None, committed_bytes=None)])
        text, _ = self.render("machine_capacity", raw, snapshot=(("fleet:read",), None))
        self.assertIn("Host reading load 1.5", text)
        self.assertIn("Filesystem reading: available bytes could not check", text)

    def test_identifiers_are_tool_only_for_all_five_tools(self):
        for command in detail.COMMANDS:
            with self.subTest(command=command):
                raw = projection(command)
                args = dict(ship="alpha")
                if command == "machine_capacity":
                    args["machine"] = "beta"
                if command == "job_inspect":
                    args["reference"] = REF
                if command in ("ship_jobs", "job_inspect"):
                    row = raw["jobs"][0] if command == "ship_jobs" else raw["job"]
                    row["wait"].update(job=dict(REF, id="alpha-child"), task_id="alpha-dependency")
                    for key in ("parent", "retry_of", "resume_of", "relaunch_of", "resumed_as"):
                        row[key] = dict(REF, id="alpha-" + key)
                if command == "runtime_needs":
                    raw["needs"][0]["id"] = REF["origin"] + ":mail:alpha-mail"
                    raw["needs"].append(dict(raw["needs"][0], id=REF["origin"] + ":job:" + REF["id"], source="job"))
                text, metadata = self.render(command, raw, args)
                self.assertNotEqual(text, detail.UNAVAILABLE)
                for identifier in (REF["origin"], REF["incarnation"], REF["id"], REF["task_id"],
                                   "alpha-child", "alpha-dependency", "alpha-mail", "alpha-need",
                                   "alpha-parent", "alpha-retry_of", "alpha-resume_of", "alpha-relaunch_of", "alpha-resumed_as"):
                    self.assertNotIn(identifier, text)
                if command == "runtime_needs":
                    self.assertEqual(metadata["need_ids"], [row["id"] for row in raw["needs"]])

    def test_partial_needs_withhold_question_even_if_backend_returns_text(self):
        text, metadata = self.render("runtime_needs")
        self.assertIn("Needs partial; aggregate mail coverage could not check", text)
        self.assertIn("Reading cannot answer or approve a need", text)
        self.assertIn("created time unknown, age unknown", text)
        self.assertNotIn("canary", text + str(metadata))

    def test_unavailable_targets_unknowns_and_reverse_only(self):
        for state in ("unavailable", "unsupported", "reverse_only"):
            raw = projection()
            raw["coverage"]["state"] = state
            text, metadata = self.render(raw=raw)
            self.assertIn(detail.UNAVAILABLE, text)
            self.assertEqual(metadata, {})
            if state == "reverse_only":
                self.assertIn("Unsupported read direction / incomplete feature", text)
        for mutate in (lambda r: r["coverage"].update(observed_at="age-canary"),
                       lambda r: r["coverage"].update(ship="beta"),
                       lambda r: r["coverage"].update(count_basis="fleet"),
                       lambda r: r.update(continuation="stale"),
                       lambda r: r["jobs"][0]["reference"].update(origin="another-home")):
            raw = projection()
            mutate(raw)
            self.assertEqual(self.render(raw=raw), (detail.UNAVAILABLE, {}))


if __name__ == "__main__":
    unittest.main()
