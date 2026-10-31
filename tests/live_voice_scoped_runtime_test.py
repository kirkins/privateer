"""Focused scoped runtime speech contract; no provider or production access."""
import copy
import pathlib
import sys
import unittest

sys.dont_write_bytecode = True
sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent.parent / "plugins/live-voice"))
import scoped_runtime as runtime

SNAPSHOT = (("fleet:read",), ("alpha",))


def projection(mode="local_live", working=False):
    current = mode in ("local_live", "remote_live", "reused")
    census = dict(observed_at=120, executing=working is True, activity_complete=working != "unknown",
                  workers_complete=working != "unknown", sessions_complete=working != "unknown",
                  active_workers=int(working is True), first_mate=int(working is True), crew=0,
                  errand=0, commands=0, uncertain_jobs=int(working == "unknown"),
                  sessions=dict(live=1, idle=int(working is False), asleep=0, blocked=0, viewer=0,
                                unknown=int(working == "unknown")),
                  session_details=[dict(reference="opaque-reference-canary", role="first_mate",
                                        state="working" if working is True else "idle" if working is False else "unknown",
                                        harness="pi", host_mode="hosted")],
                  reason="reason-canary", anomalies={})
    facts = dict(name="alpha", harness="pi", host_mode="headless", census=census,
                 jobs_total=0, jobs=[], job_states={}, jobs_truncated=False, open_tasks=40,
                 recorded_running_jobs=0, authored="authored-canary", path="path-canary")
    sources = [dict(id="local:alpha:" + facet, machine="local", ship="alpha", facet=facet,
                    state="covered" if current else "cached", mode=mode,
                    observed_at=120, received_at=150, age_ms=30, barrier_mode=None,
                    max_source_age_ms=5000, reason="reason-canary")
               for facet in ("inventory", "workers", "sessions", "jobs")]
    totals = dict(scope="local")
    for key in runtime.WORKERS + ("sessions", "recorded_running_jobs", "nonterminal_jobs", "job_states"):
        facet = "sessions" if key == "sessions" else "jobs" if key in (
            "recorded_running_jobs", "nonterminal_jobs", "job_states") else "workers"
        totals[key] = dict(value=copy.deepcopy(census.get(key, {} if key == "job_states" else 0)),
                           complete=working != "unknown" and current,
                           sources=["local:alpha:" + facet] if current else [])
    return dict(schema="runtime.v1", selection=dict(machine=None, ship=None), machines=["local"],
                requested_machines=[], queried_machines=[], reused_machines=[], cached_machines=[],
                omitted_machines=[], ships=[dict(name="alpha", machine="local", working=working,
                    runtime=dict(current=current, mode=mode, observed_at=123, received_at=150, age_ms=27,
                                 facts=facts, text="withheld"))], observed_totals=totals,
                coverage=dict(schema="coverage.v1", scope="runtime-domain", complete=False, domain=None,
                    state="unknown_domain", inventory_complete=True, other_machines="not_queried", sources=sources))


class RuntimeSpeechTest(unittest.TestCase):
    def render(self, raw=None, selectors=None):
        return runtime.render(raw if raw is not None else projection(), SNAPSHOT, selectors or {},
                              clock=lambda: 180123)

    def test_mapping_is_fixed_and_no_escape(self):
        self.assertEqual(runtime.request({}), ("status", {"schema": "runtime.v1"}))
        self.assertEqual(runtime.request({"machine": "beta"}), ("status", {"schema": "runtime.v1", "machine": "beta"}))
        self.assertEqual(runtime.request({"ship": "alpha"}), ("ship_runtime", {"ship": "alpha"}))
        for args in ({"schema": "runtime.v1"}, {"include_text": True}, {"ship": None},
                     {"machine": "beta\nINJECT"}, {"ship": "../file/alpha"}, [], {"command": "send"}):
            with self.subTest(args=args), self.assertRaises(ValueError):
                runtime.request(args)

    def test_active_idle_unknown_and_backlog_is_not_workers(self):
        for working, phrase in ((True, "working yes"), (False, "working no, checked idle"),
                                ("unknown", "working could not check")):
            with self.subTest(working=working):
                text = self.render(projection(working=working))
                self.assertIn(phrase, text)
                self.assertIn("open task backlog: 40", text)
                self.assertIn("Backlog and recorded running jobs do not establish workers", text)
                self.assertIn("Commands and uncertain jobs are separate", text)
                self.assertIn("not additive workers", text)
                self.assertLess(text.index("Runtime coverage"), text.index("active workers"))
                self.assertLess(text.index("observed at 123"), text.index("active workers"))
                self.assertLess(text.index("census observed at 120"), text.index("active workers"))
                self.assertIn(runtime.MAIL, text)

    def test_fresh_reuse_cache_carry_keep_original_age(self):
        for mode in ("local_live", "remote_live", "reused", "cache", "carried"):
            with self.subTest(mode=mode):
                text = self.render(projection(mode=mode, working=True))
                self.assertIn(mode, text)
                self.assertIn("observed at 123, age 27 milliseconds, received at 150", text)
                if mode in ("cache", "carried"):
                    self.assertIn("last observed", text)
                    self.assertIn("Last observed (not current) workers", text)
                    self.assertNotIn("working yes", text)
                    self.assertIn("could not check current counts", text)
                else:
                    self.assertIn("working yes", text)
        raw = projection(mode="cache", working=True)
        raw["observed_totals"]["recorded_running_jobs"] = dict(value=10, complete=False, sources=["local:alpha:jobs"])
        text = self.render(raw)
        self.assertNotIn("10; contributors", text)

    def test_optional_details_unknown_labels_and_recorded_configuration(self):
        raw = projection()
        census = raw["ships"][0]["runtime"]["facts"]["census"]
        text = self.render(raw)
        self.assertIn("session: first mate, idle, harness pi, host mode hosted", text)
        self.assertIn("Recorded configuration (not liveness): harness pi, host mode headless", text)
        del census["session_details"]
        text = self.render(raw)
        self.assertIn("Session details omitted or unsupported; this does not mean no sessions", text)
        self.assertIn("live 1", text)
        census["session_details"] = []
        self.assertIn("live 1", self.render(raw))  # absent rows never reset the summary
        census["session_details"] = [dict(role="viewer", state="viewer", harness="label-canary", host_mode="mode-canary")]
        text = self.render(raw)
        self.assertIn("viewer, viewer, harness unknown, host mode unknown", text)
        self.assertNotIn("canary", text)

    def test_malformed_coverage_counts_and_selectors_fail_closed(self):
        mutations = [lambda r: r.pop("coverage"),
                     lambda r: r["coverage"].update(complete=True),
                     lambda r: r["coverage"].update(inventory_complete="yes"),
                     lambda r: r["coverage"]["sources"][0].update(age_ms=-1),
                     lambda r: r["coverage"]["sources"][0].update(mode="invented"),
                     lambda r: r["observed_totals"]["crew"].update(value=False),
                     lambda r: r["observed_totals"]["crew"].update(sources=["hidden"]),
                     lambda r: r["ships"][0].update(name="beta"),
                     lambda r: r["ships"][0]["runtime"]["facts"]["census"].update(crew="count-canary")]
        for mutate in mutations:
            raw = projection()
            mutate(raw)
            self.assertEqual(self.render(raw), runtime.UNAVAILABLE)
        raw = projection()
        raw["ships"] = []
        self.assertEqual(self.render(raw, {"ship": "alpha"}), runtime.UNAVAILABLE)
        self.assertEqual(self.render(projection(), {"ship": "beta"}), runtime.UNAVAILABLE)

    def test_incomplete_zero_missing_census_and_unknown_ages(self):
        raw = projection(working="unknown")
        raw["coverage"]["inventory_complete"] = False
        text = self.render(raw)
        self.assertIn("active workers: partial; could not check all; could not check;", text)
        self.assertIn("Checked workers partial; could not check all: active workers could not check", text)
        self.assertIn("uncertain jobs at least 1 observed", text)
        self.assertNotIn("partial; could not check all; 0", text)
        self.assertNotIn("checked idle", text)
        facts = raw["ships"][0]["runtime"]["facts"]
        del facts["census"]
        raw["ships"][0]["runtime"].update(observed_at=None, age_ms=None, received_at=None)
        text = self.render(raw)
        self.assertIn("observation time unknown, age unknown", text)
        self.assertIn("Workers and sessions: could not check", text)
        facts["jobs_total"] = None
        text = self.render(raw)
        self.assertIn("recorded job states: could not check", text)
        self.assertNotIn("no states observed", text)
        raw["coverage"]["sources"] = []
        for total in raw["observed_totals"].values():
            if isinstance(total, dict):
                total["sources"] = []
        self.assertEqual(self.render(raw), runtime.UNAVAILABLE)

    def test_contributor_list_omissions_are_explicit(self):
        raw = projection()
        original = copy.deepcopy(raw)
        raw["ships"], raw["coverage"]["sources"] = [], []
        for index in range(runtime.LIMIT + 1):
            name = "alpha-" + str(index)
            row = copy.deepcopy(original["ships"][0])
            row["name"] = row["runtime"]["facts"]["name"] = name
            raw["ships"].append(row)
            for source in copy.deepcopy(original["coverage"]["sources"]):
                source["ship"] = name
                source["id"] = "local:" + name + ":" + source["facet"]
                raw["coverage"]["sources"].append(source)
        for key, total in raw["observed_totals"].items():
            if isinstance(total, dict):
                facet = original["observed_totals"][key]["sources"][0].rsplit(":", 1)[1]
                total["sources"] = [s["id"] for s in raw["coverage"]["sources"] if s["facet"] == facet]
        text = runtime.render(raw, (("fleet:read",), None), {}, clock=lambda: 180123)
        self.assertIn("1 additional contributors omitted from speech", text)
        self.assertLess(text.index("more ships not read out"), text.index("active workers"))

    def test_reverse_only_omissions_precede_counts_and_canaries_withheld(self):
        raw = projection()
        raw["coverage"]["sources"][1].update(state="reverse_only", mode="directory")
        raw["omitted_machines"] = ["gamma"]
        text = self.render(raw)
        self.assertIn("Unsupported read direction / incomplete feature", text)
        self.assertLess(text.index("omitted machines: gamma"), text.index("active workers"))
        for canary in ("opaque-reference-canary", "authored-canary", "path-canary", "reason-canary"):
            self.assertNotIn(canary, text)
        self.assertNotIn("mailbox repaired", text.lower())

    def test_partial_positive_totals_and_workers_are_lower_bounds(self):
        raw = projection(working=True)
        for total in raw["observed_totals"].values():
            if isinstance(total, dict):
                total["complete"] = False
        census = raw["ships"][0]["runtime"]["facts"]["census"]
        census["workers_complete"] = census["sessions_complete"] = False
        text = self.render(raw)
        self.assertIn("active workers: partial; could not check all; at least 1 observed;", text)
        self.assertIn("Checked workers partial; could not check all: active workers at least 1 observed", text)
        self.assertIn("crew could not check", text)
        self.assertIn("sessions: partial; could not check all; live at least 1 observed, idle could not check", text)
        self.assertNotIn("crew 0", text)
        self.assertNotIn("idle 0", text)

    def test_noncovered_sources_take_precedence_over_cache_and_carry(self):
        for mode in ("cache", "carried"):
            for state in ("unavailable", "unreadable", "timeout"):
                with self.subTest(mode=mode, state=state):
                    raw = projection(mode=mode)
                    raw["coverage"]["sources"][1]["state"] = state
                    text = self.render(raw)
                    self.assertIn(f"local alpha workers: could not check, {mode}, observed at 120", text)
                    self.assertNotIn(f"local alpha workers: last observed, {mode}", text)

    def test_relative_age_uses_injected_clock_and_original_observation(self):
        for mode in ("local_live", "reused", "cache", "carried"):
            with self.subTest(mode=mode):
                raw = projection(mode=mode)
                text = runtime.render(raw, SNAPSHOT, {}, clock=lambda: 180123)
                self.assertIn("observed at 123, age 27 milliseconds, received at 150, about 3 minutes ago", text)
                later = runtime.render(raw, SNAPSHOT, {}, clock=lambda: 3600123)
                self.assertIn("observed at 123, age 27 milliseconds, received at 150, about 1 hour ago", later)
                if mode in ("cache", "carried"):
                    self.assertIn(f"last observed, {mode}", later)
                    self.assertNotIn("Checked workers", later)
        raw = projection()
        raw["ships"][0]["runtime"]["observed_at"] = 180124
        self.assertIn("relative age unknown", self.render(raw))  # clock skew is not fresh

    def test_twelve_ships_with_twelve_sessions_fit_transcript_budget(self):
        raw = projection()
        original = copy.deepcopy(raw)
        raw["ships"], raw["coverage"]["sources"] = [], []
        for index in range(12):
            name = "alpha-" + str(index)
            row = copy.deepcopy(original["ships"][0])
            row["name"] = row["runtime"]["facts"]["name"] = name
            row["runtime"]["facts"]["census"]["session_details"] *= 12
            raw["ships"].append(row)
            for source in copy.deepcopy(original["coverage"]["sources"]):
                source["ship"] = name
                source["id"] = "local:" + name + ":" + source["facet"]
                raw["coverage"]["sources"].append(source)
        for key, total in raw["observed_totals"].items():
            if isinstance(total, dict):
                facet = original["observed_totals"][key]["sources"][0].rsplit(":", 1)[1]
                total["sources"] = [s["id"] for s in raw["coverage"]["sources"] if s["facet"] == facet]
        text = runtime.render(raw, (("fleet:read",), None), {}, clock=lambda: 180123)
        self.assertLessEqual(len(text), runtime.SPEECH_BUDGET)
        self.assertLess(len(text), 8000)
        self.assertTrue(text.startswith("Runtime coverage: domain unknown; incomplete."))
        self.assertRegex(text, r"[1-9][0-9]* more ships not read out; ask about one by name")
        self.assertIn("Session rows not read out in overview", text)
        self.assertTrue(text.endswith(runtime.MAIL))
        self.assertNotIn("session: first mate", text)
        # Even maximum-length valid labels/contributor lists must not clip mail.
        for field in ("requested_machines", "queried_machines", "reused_machines", "cached_machines", "omitted_machines"):
            raw[field] = ["gamma-" + str(i) + "a" * 118 for i in range(12)]
        text = runtime.render(raw, (("fleet:read",), None), {}, clock=lambda: 180123)
        self.assertLessEqual(len(text), runtime.SPEECH_BUDGET)
        self.assertIn("metadata lines not read out", text)
        self.assertTrue(text.endswith(runtime.MAIL))
        self.assertNotEqual(text, runtime.UNAVAILABLE)

    def test_bounded_session_omission_is_before_counts(self):
        raw = projection()
        census = raw["ships"][0]["runtime"]["facts"]["census"]
        census["session_details"] *= 20
        text = self.render(raw)
        self.assertLess(text.index("8 session rows omitted"), text.index("active workers"))
        self.assertEqual(text.count("session: first mate"), runtime.LIMIT)


if __name__ == "__main__":
    unittest.main()
