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
        self.max_calls = max_calls
        # `gh api` calls made, retries included.
        self.calls = 0
        # Every read, cached ones too, so the sample does not depend on cache state.
        self.requests = 0
        self.checked = False

    def get(self, path):
        self.requests += 1
        cached = CACHE / (hashlib.sha256(path.encode()).hexdigest()[:24] + ".json")
        if cached.exists():
            return json.loads(cached.read_text())
        if self.requests > self.max_calls:
            raise RuntimeError("REST call budget spent")
        if not self.checked:
            # The token may be shared with other automation; leave it headroom.
            remaining = json.loads(gh("rate_limit"))["resources"]["core"]["remaining"]
            if remaining < MIN_CORE_REMAINING:
                sys.exit(f"core rate limit remaining {remaining} < {MIN_CORE_REMAINING}, aborting")
            self.checked = True
        self.calls += 1
        try:
            out = gh(path)
        except subprocess.CalledProcessError as e:
            # One retry for transient 5xx and network errors; a second failure is real.
            print(f"gh api {path} failed: {e.stderr.strip()}; retrying once", file=sys.stderr)
            time.sleep(2)
            self.calls += 1
            out = gh(path)
        data = json.loads(out)
        CACHE.mkdir(exist_ok=True)
        cached.write_text(json.dumps(data))
        return data


def gh(path):
    return subprocess.run(["gh", "api", path], check=True, capture_output=True, text=True, timeout=60).stdout


def pct(xs, p):
    """The p-th percentile by the nearest-rank method, or None for no values."""
    if not xs:
        return None
    return sorted(xs)[max(0, math.ceil(p / 100 * len(xs)) - 1)]


def fmt(seconds):
    if seconds is None:
        return "-"
    if seconds < 120:
        return f"{int(seconds)}s"
    return f"{seconds / 60:.1f}m"


def even(items, k):
    """k items spread evenly over `items`, first and last included."""
    if len(items) <= k:
        return list(items)
    if k <= 1:
        return items[:1]
    return [items[round(i * (len(items) - 1) / (k - 1))] for i in range(k)]


def resolve_workflows(api, repo, names):
    # Deleted workflows and ones that never reached the default branch are missing
    # from the workflows listing, but their runs are still reachable by file name.
    found = {n: [n] for n in names if n.endswith((".yml", ".yaml"))}
    for page in range(1, 4):
        data = api.get(f"repos/{repo}/actions/workflows?per_page=100&page={page}")
        for workflow in data["workflows"]:
            if workflow["name"] in names:
                found.setdefault(workflow["name"], []).append(str(workflow["id"]))
        if page * 100 >= data["total_count"]:
            break
    missing = sorted(set(names) - set(found))
    if missing:
        print(f"workflows not found, skipped: {', '.join(missing)}", file=sys.stderr)
    return sorted(i for ids in found.values() for i in ids)


def list_runs(api, repo, workflow, start, end, pages_cap):
    base = f"repos/{repo}/actions/workflows/{workflow}/runs?per_page=100&created={iso(start)}..{iso(end)}"
    first = api.get(base + "&page=1")
    pages = math.ceil(min(first["total_count"], 1000) / 100)
    runs = list(first["workflow_runs"])
    if pages_cap > 1:
        # Spread the extra pages over the window: the API returns newest first.
        for page in sorted(set(even(list(range(2, pages + 1)), pages_cap - 1))):
            runs += api.get(f"{base}&page={page}")["workflow_runs"]
    return runs


def table(head, rows):
    print("\n| " + " | ".join(head) + " |\n|" + "---|" * len(head))
    for row in rows:
        print("| " + " | ".join(str(cell) for cell in row) + " |")


def kind(job, boat_re, persistent):
    labels = job.get("labels") or []
    if persistent in labels:
        return "persistent"
    if any(boat_re.match(label) for label in labels):
        return "boat"
    return "other"


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--repo", required=True, help="OWNER/REPO")
    ap.add_argument("--cutover", required=True, help="ISO time the jobs moved, e.g. 2026-11-05T21:52:45Z")
    ap.add_argument("--boat-ci", required=True, help="output of `boat ci --json --limit 500`")
    ap.add_argument("--max-calls", type=int, default=250, help="gh api reads, cached ones included (default 250)")
    ap.add_argument("--persistent-label", default="sw-boat-runner", help="label of the persistent runners")
    ap.add_argument("--boat-labels", default="boat,boat-small,boat-large,boat-xlarge",
                    help="comma-separated labels of boat per-job machines; a /options suffix also matches")
    ap.add_argument("--workflow", action="append", help="workflow name or file name; default: names in the boat ci jobs")
    ap.add_argument("--before-start", help="default: 00:00Z the day before cutover")
    ap.add_argument("--after-start", help="default: 00:00Z the day after cutover")
    ap.add_argument("--after-end", help="default: now, truncated to the hour so reruns hit the cache")
    args = ap.parse_args()

    boat = json.loads(Path(args.boat_ci).read_text())
    cut = ts(args.cutover)
    day0 = cut.replace(hour=0, minute=0, second=0, microsecond=0)
    now = datetime.now(timezone.utc).replace(minute=0, second=0, microsecond=0)
    windows = [
        ("before", ts(args.before_start) if args.before_start else day0 - timedelta(days=1), cut),
        ("after", ts(args.after_start) if args.after_start else day0 + timedelta(days=1),
         ts(args.after_end) if args.after_end else now),
    ]
    names = args.workflow or sorted({j["workflowName"] for j in boat["jobs"] if j["repo"] == args.repo})
    boat_re = re.compile(r"^(%s)(/.*)?$" % "|".join(re.escape(label) for label in args.boat_labels.split(",")))

    api = Api(args.max_calls)
    workflows = resolve_workflows(api, args.repo, names)
    list_budget = args.max_calls // 5
    pages_cap = max(1, (list_budget - api.requests) // max(1, len(workflows) * len(windows)))
    sampled = {}
    for window, start, end in windows:
        runs = [r for wf in workflows for r in list_runs(api, args.repo, wf, start, end, pages_cap)]
        sampled[window] = sorted({r["id"]: r for r in runs}.values(), key=lambda r: r["created_at"])
    per_window = (args.max_calls - api.requests) // len(windows)

    rows, runners, queue_by_day = [], {}, {}
    for window, start, end in windows:
        groups = {}
        for run in even(sampled[window], per_window):
            jobs = api.get(f"repos/{args.repo}/actions/runs/{run['id']}/jobs?per_page=100")["jobs"]
            for job in jobs:
                if job["status"] != "completed" or job["conclusion"] == "skipped":
                    continue
                runner_kind = kind(job, boat_re, args.persistent_label)
                group = groups.setdefault(runner_kind, {"queue": [], "run": [], "never": 0})
                if not job.get("runner_name"):
                    group["never"] += 1
                    continue
                queue_s = (ts(job["started_at"]) - ts(job["created_at"])).total_seconds()
                group["queue"].append(queue_s)
                group["run"].append((ts(job["completed_at"]) - ts(job["started_at"])).total_seconds())
                if runner_kind == "persistent":
                    count, last = runners.get(job["runner_name"], (0, ""))
                    runners[job["runner_name"]] = (count + 1, max(last, job["started_at"]))
                    queue_by_day.setdefault(job["created_at"][:10], []).append(queue_s)
        days = [d for d in boat["stats"]["perDay"]
                if start.date() <= ts(d["day"] + "T00:00:00Z").date() <= end.date()]
        starts_per_day = sum(d["success"] + d["failure"] + d["other"] for d in days) / max(1, len(days))
        label = f"{window} ({iso(start)} to {iso(end)}, {min(len(sampled[window]), per_window)} runs)"
        for runner_kind, group in sorted(groups.items()):
            rows.append([label, runner_kind, len(group["queue"]), group["never"],
                         fmt(pct(group["queue"], 50)), fmt(pct(group["queue"], 90)),
                         fmt(pct(group["run"], 50)), fmt(pct(group["run"], 90)), f"{starts_per_day:.0f}"])

    boat_jobs = [j for j in boat["jobs"] if j["repo"] == args.repo and j.get("runStartedAt")]
    if boat_jobs:
        waits = [j["waitSeconds"] for j in boat_jobs]
        run_times = [j["runSeconds"] for j in boat_jobs]
        first = min(j["queuedAt"] for j in boat_jobs)[:16]
        last = max(j["queuedAt"] for j in boat_jobs)[:16]
        rows.append([f"boat ci log ({len(boat_jobs)} jobs, {first} to {last})", "boat", len(boat_jobs), "-",
                     fmt(pct(waits, 50)), fmt(pct(waits, 90)), fmt(pct(run_times, 50)), fmt(pct(run_times, 90)), "-"])

    table(["window", "runner kind", "jobs", "no runner", "queue p50", "queue p90",
           "run p50", "run p90", "boat starts/day"], rows)
    table(["persistent runner", "jobs", "last job started"],
          [[name, count, last] for name, (count, last) in sorted(runners.items())])
    table(["day (persistent jobs)", "jobs", "queue p50", "queue p90"],
          [[day, len(q), fmt(pct(q, 50)), fmt(pct(q, 90))] for day, q in sorted(queue_by_day.items())])
    print(f"\nREST calls made: {api.calls} (cache: {CACHE})", file=sys.stderr)


if __name__ == "__main__":
    main()
