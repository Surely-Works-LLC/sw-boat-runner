"""Compare GitHub Actions jobs before and after moving from boat's per-job machines
to persistent self-hosted runners. Reads `boat ci --json` output for start counts
and samples the GitHub Actions API through `gh api` for queue and run times."""
import argparse
import hashlib
import json
import math
import re
import subprocess
import sys
import time
from datetime import datetime, timedelta, timezone
from pathlib import Path

CACHE = Path(__file__).resolve().parent / ".cache"
MIN_CORE_REMAINING = 1500


def ts(s):
    return datetime.fromisoformat(s.replace("Z", "+00:00"))


def iso(d):
    return d.strftime("%Y-%m-%dT%H:%M:%SZ")


class Api:
    def __init__(self, max_calls):
        # requests counts cached reads too, so the sample does not depend on cache state.
        self.max_calls, self.calls, self.requests, self.checked = max_calls, 0, 0, False

    def get(self, path):
        self.requests += 1
        f = CACHE / (hashlib.sha256(path.encode()).hexdigest()[:24] + ".json")
        if f.exists():
            return json.loads(f.read_text())
        if self.requests > self.max_calls:
            raise RuntimeError("REST call budget spent")
        if not self.checked:
            # The token is shared with other automation; leave it headroom.
            rl = json.loads(gh("rate_limit"))["resources"]["core"]
            if rl["remaining"] < MIN_CORE_REMAINING:
                sys.exit("core rate limit remaining %d < %d, aborting" % (rl["remaining"], MIN_CORE_REMAINING))
            self.checked = True
        self.calls += 1
        try:
            out = gh(path)
        except subprocess.CalledProcessError as e:
            # One retry for transient 5xx and network errors; a second failure is real.
            print("gh api %s failed: %s; retrying once" % (path, e.stderr.strip()), file=sys.stderr)
            time.sleep(2)
            self.calls += 1
            out = gh(path)  # may exceed --max-calls by the retries, at most one per request
        data = json.loads(out)
        CACHE.mkdir(exist_ok=True)
        f.write_text(json.dumps(data))
        return data


def gh(path):
    return subprocess.run(["gh", "api", path], check=True, capture_output=True, text=True, timeout=60).stdout


def pct(xs, p):
    return sorted(xs)[max(0, math.ceil(p / 100 * len(xs)) - 1)] if xs else None


def fmt(s):
    return "-" if s is None else "%ds" % s if s < 120 else "%.1fm" % (s / 60)


def even(items, k):
    if len(items) <= k:
        return list(items)
    return [items[round(i * (len(items) - 1) / (k - 1))] for i in range(k)] if k > 1 else items[:1]


def resolve_workflows(api, repo, names):
    # Deleted workflows and ones that never reached the default branch are missing
    # from the workflows listing, but their runs are still reachable by file name.
    found = {n: [n] for n in names if n.endswith((".yml", ".yaml"))}
    for page in range(1, 4):
        data = api.get("repos/%s/actions/workflows?per_page=100&page=%d" % (repo, page))
        for w in data["workflows"]:
            if w["name"] in names:
                found.setdefault(w["name"], []).append(str(w["id"]))
        if page * 100 >= data["total_count"]:
            break
    missing = sorted(set(names) - set(found))
    if missing:
        print("workflows not found, skipped: %s" % ", ".join(missing), file=sys.stderr)
    return sorted(i for ids in found.values() for i in ids)


def list_runs(api, repo, wf, start, end, pages_cap):
    base = "repos/%s/actions/workflows/%s/runs?per_page=100&created=%s..%s" % (repo, wf, iso(start), iso(end))
    first = api.get(base + "&page=1")
    pages = math.ceil(min(first["total_count"], 1000) / 100)
    runs = list(first["workflow_runs"])
    # Spread the extra pages over the window: the API returns newest first.
    for page in sorted(set(even(list(range(2, pages + 1)), pages_cap - 1))) if pages_cap > 1 else []:
        runs += api.get(base + "&page=%d" % page)["workflow_runs"]
    return runs


def table(head, rows):
    print("\n| " + " | ".join(head) + " |\n|" + "---|" * len(head))
    for r in rows:
        print("| " + " | ".join(str(c) for c in r) + " |")


def kind(job, boat_re, persistent):
    labels = job.get("labels") or []
    if persistent in labels:
        return "persistent"
    if any(boat_re.match(l) for l in labels):
        return "boat"
    return "other"


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--repo", required=True)
    ap.add_argument("--cutover", required=True, help="ISO time the jobs moved, e.g. 2026-11-05T21:52:45Z")
    ap.add_argument("--boat-ci", required=True, help="output of `boat ci --json --limit 500`")
    ap.add_argument("--max-calls", type=int, default=250)
    ap.add_argument("--persistent-label", default="sw-boat-runner")
    ap.add_argument("--boat-labels", default="boat,boat-small,boat-large,boat-xlarge")
    ap.add_argument("--workflow", action="append", help="workflow name or file name; default: names in the boat ci jobs")
    ap.add_argument("--before-start", help="default: 00:00Z the day before cutover")
    ap.add_argument("--after-start", help="default: 00:00Z the day after cutover")
    ap.add_argument("--after-end", help="default: now, truncated to the hour so reruns hit the cache")
    a = ap.parse_args()

    boat = json.loads(Path(a.boat_ci).read_text())
    cut = ts(a.cutover)
    day0 = cut.replace(hour=0, minute=0, second=0, microsecond=0)
    now = datetime.now(timezone.utc).replace(minute=0, second=0, microsecond=0)
    windows = [
        ("before", ts(a.before_start) if a.before_start else day0 - timedelta(days=1), cut),
        ("after", ts(a.after_start) if a.after_start else day0 + timedelta(days=1), ts(a.after_end) if a.after_end else now),
    ]
    names = a.workflow or sorted({j["workflowName"] for j in boat["jobs"] if j["repo"] == a.repo})
    boat_re = re.compile(r"^(%s)(/.*)?$" % "|".join(re.escape(l) for l in a.boat_labels.split(",")))

    api = Api(a.max_calls)
    wfs = resolve_workflows(api, a.repo, names)
    list_budget = a.max_calls // 5
    pages_cap = max(1, (list_budget - api.requests) // max(1, len(wfs) * len(windows)))
    sampled = {}
    for wname, start, end in windows:
        runs = [r for wf in wfs for r in list_runs(api, a.repo, wf, start, end, pages_cap)]
        sampled[wname] = sorted({r["id"]: r for r in runs}.values(), key=lambda r: r["created_at"])
    per_window = (a.max_calls - api.requests) // len(windows)

    rows, runners, by_day = [], {}, {}
    for wname, start, end in windows:
        groups = {}
        for run in even(sampled[wname], per_window):
            for j in api.get("repos/%s/actions/runs/%d/jobs?per_page=100" % (a.repo, run["id"]))["jobs"]:
                if j["status"] != "completed" or j["conclusion"] == "skipped":
                    continue
                k = kind(j, boat_re, a.persistent_label)
                g = groups.setdefault(k, {"q": [], "r": [], "never": 0})
                if not j.get("runner_name"):
                    g["never"] += 1
                    continue
                q = (ts(j["started_at"]) - ts(j["created_at"])).total_seconds()
                g["q"].append(q)
                g["r"].append((ts(j["completed_at"]) - ts(j["started_at"])).total_seconds())
                if k == "persistent":
                    r = runners.setdefault(j["runner_name"], [0, ""])
                    r[0], r[1] = r[0] + 1, max(r[1], j["started_at"])
                    by_day.setdefault(j["created_at"][:10], []).append(q)
        days = [d for d in boat["stats"]["perDay"] if start.date() <= ts(d["day"] + "T00:00:00Z").date() <= end.date()]
        per_day = sum(d["success"] + d["failure"] + d["other"] for d in days) / max(1, len(days))
        label = "%s (%s to %s, %d runs)" % (wname, iso(start), iso(end), min(len(sampled[wname]), per_window))
        for k, g in sorted(groups.items()):
            rows.append([label, k, len(g["q"]), g["never"], fmt(pct(g["q"], 50)), fmt(pct(g["q"], 90)),
                         fmt(pct(g["r"], 50)), fmt(pct(g["r"], 90)), "%.0f" % per_day])

    bj = [j for j in boat["jobs"] if j["repo"] == a.repo and j.get("runStartedAt")]
    if bj:
        w, r = [j["waitSeconds"] for j in bj], [j["runSeconds"] for j in bj]
        rows.append(["boat ci log (%d jobs, %s to %s)" % (len(bj), min(j["queuedAt"] for j in bj)[:16],
                     max(j["queuedAt"] for j in bj)[:16]), "boat", len(bj), "-", fmt(pct(w, 50)), fmt(pct(w, 90)),
                     fmt(pct(r, 50)), fmt(pct(r, 90)), "-"])

    table(["window", "runner kind", "jobs", "no runner", "queue p50", "queue p90",
           "run p50", "run p90", "boat starts/day"], rows)
    table(["persistent runner", "jobs", "last job started"], [[n, c, t] for n, (c, t) in sorted(runners.items())])
    table(["day (persistent jobs)", "jobs", "queue p50", "queue p90"],
          [[d, len(q), fmt(pct(q, 50)), fmt(pct(q, 90))] for d, q in sorted(by_day.items())])
    print("\nREST calls made: %d (cache: %s)" % (api.calls, CACHE), file=sys.stderr)


if __name__ == "__main__":
    main()
