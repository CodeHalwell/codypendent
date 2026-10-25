#!/usr/bin/env python3
"""Read-only macOS CPU/RSS sampling for explicitly identified GUI process IDs.

Supply the app and its identified WebKit GPU, WebContent and Networking PIDs.
This script never discovers, launches, attaches to, or terminates applications.
CPU is measured from cumulative process CPU-time deltas, not ps %cpu. Summed
RSS counts shared pages in each process; it is not unique physical memory.
"""
from __future__ import annotations

import argparse
from datetime import datetime, timezone
import json
import math
import os
from pathlib import Path
import platform
import re
import statistics
import subprocess
import sys
import time


def utc_now() -> str:
    return datetime.now(timezone.utc).isoformat()


def command(*args: str) -> str:
    return subprocess.check_output(args, text=True, env={**os.environ, "LC_ALL": "C"}, timeout=10)


def cpu_seconds(value: str) -> float:
    days, separator, clock = value.partition("-")
    result = float(days) * 86400 if separator else 0.0
    parts = (clock if separator else value).split(":")
    for index, part in enumerate(reversed(parts)):
        result += float(part) * 60 ** index
    return result


def host_snapshot() -> dict:
    raw = command("/usr/bin/vm_stat")
    page_size = int(re.search(r"page size of (\d+) bytes", raw).group(1))
    pages = {key.strip().strip('"'): int(value)
             for key, value in re.findall(r"^([^:\n]+):\s+(\d+)\.", raw, re.MULTILINE)}
    return {"at_utc": utc_now(), "load_average_1_5_15": os.getloadavg(),
            "physical_ram_bytes": int(command("/usr/sbin/sysctl", "-n", "hw.memsize")),
            "logical_cpus": int(command("/usr/sbin/sysctl", "-n", "hw.logicalcpu")),
            "vm_page_size_bytes": page_size, "vm_stat_counters": pages}


def sample(processes: dict[int, str]) -> dict:
    before = time.monotonic()
    result = subprocess.run(
        ["/bin/ps", "-p", ",".join(map(str, processes)),
         "-o", "pid=,ppid=,lstart=,time=,rss=,comm="],
        text=True, capture_output=True, env={**os.environ, "LC_ALL": "C"}, timeout=10,
    )
    after = time.monotonic()
    if result.returncode not in (0, 1):
        raise ValueError(f"ps failed: {result.stderr.strip()}")
    rows = []
    for line in result.stdout.splitlines():
        fields = line.split(maxsplit=9)
        if len(fields) != 10:
            raise ValueError(f"Unexpected ps row: {line!r}")
        pid = int(fields[0])
        if pid not in processes:
            raise ValueError(f"ps returned an unrequested PID: {pid}")
        rows.append({"pid": pid, "label": processes[pid], "ppid": int(fields[1]),
                     "started_local": " ".join(fields[2:7]), "command": fields[9],
                     "cpu_seconds": cpu_seconds(fields[7]), "rss_bytes": int(fields[8]) * 1024})
    return {"at_utc": utc_now(), "monotonic_seconds": (before + after) / 2,
            "ps_capture_seconds": after - before, "load_average_1_5_15": os.getloadavg(),
            "processes": sorted(rows, key=lambda row: row["pid"]),
            "total_rss_bytes": sum(row["rss_bytes"] for row in rows),
            "total_cpu_seconds": sum(row["cpu_seconds"] for row in rows)}


def summarize(samples: list[dict], logical_cpus: int) -> dict:
    elapsed = samples[-1]["monotonic_seconds"] - samples[0]["monotonic_seconds"]
    if elapsed <= 0:
        return {}

    def metrics(rows: list[dict]) -> dict:
        delta = rows[-1]["cpu_seconds"] - rows[0]["cpu_seconds"]
        return {"cpu_seconds_delta": delta, "cpu_percent_one_core": delta / elapsed * 100,
                "cpu_percent_host_capacity": delta / elapsed * 100 / logical_cpus,
                "rss_first_bytes": rows[0]["rss_bytes"], "rss_last_bytes": rows[-1]["rss_bytes"],
                "rss_peak_bytes": max(row["rss_bytes"] for row in rows),
                "rss_sample_mean_bytes": statistics.mean(row["rss_bytes"] for row in rows)}

    per_process = []
    for first in samples[0]["processes"]:
        rows = [next(row for row in sample["processes"] if row["pid"] == first["pid"])
                for sample in samples]
        per_process.append({"pid": first["pid"], "label": first["label"],
                            "command": first["command"], **metrics(rows)})
    total_rows = [{"cpu_seconds": sample["total_cpu_seconds"], "rss_bytes": sample["total_rss_bytes"]}
                  for sample in samples]
    return {"sample_seconds": elapsed, "sample_count": len(samples),
            "processes": per_process, "total": metrics(total_rows)}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--process", action="append", required=True, metavar="LABEL=PID",
                        help="Repeat for each previously identified process; no discovery is performed")
    parser.add_argument("--seconds", type=float, default=30)
    parser.add_argument("--interval", type=float, default=1)
    parser.add_argument("--output", type=Path, required=True, help="New JSON evidence file (never overwritten)")
    args = parser.parse_args()
    if platform.system() != "Darwin":
        parser.error("this sampler uses macOS ps, sysctl and vm_stat")
    if not all(map(math.isfinite, (args.seconds, args.interval))) or not .1 <= args.seconds <= 300 or not .05 <= args.interval <= args.seconds:
        parser.error("use 0.1–300 seconds and an interval between 0.05 seconds and the duration")
    processes: dict[int, str] = {}
    for declaration in args.process:
        label, separator, pid_text = declaration.partition("=")
        if not separator or not label.strip() or not pid_text.isdecimal() or int(pid_text) <= 0:
            parser.error("each --process must be LABEL=positive-PID")
        pid = int(pid_text)
        if pid in processes or label in processes.values():
            parser.error("process IDs and labels must be unique")
        processes[pid] = label
    if args.output.exists():
        parser.error(f"output already exists: {args.output}")
    report = {"platform": platform.platform(), "started_at_utc": utc_now(),
              "requested_seconds": args.seconds, "requested_interval_seconds": args.interval,
              "requested_processes": [{"pid": pid, "label": label} for pid, label in processes.items()],
              "method": "ps cumulative CPU deltas; 100% is one CPU core; RSS sums include shared pages",
              "samples": [], "valid": False}
    try:
        report["host_before"] = host_snapshot()
        deadline = None
        while True:
            current = sample(processes)
            report["samples"].append(current)
            rows = current["processes"]
            if {row["pid"] for row in rows} != set(processes):
                raise ValueError("A selected process exited or is inaccessible; totals are invalid")
            if len(report["samples"]) > 1:
                for first, row in zip(report["samples"][0]["processes"], rows):
                    if (row["started_local"], row["command"]) != (first["started_local"], first["command"]) or row["cpu_seconds"] < first["cpu_seconds"]:
                        raise ValueError(f"PID {row['pid']} changed identity or CPU counter; totals are invalid")
            else:
                deadline = current["monotonic_seconds"] + args.seconds
            if current["monotonic_seconds"] >= deadline:
                break
            time.sleep(max(0, min(args.interval, deadline - time.monotonic())))
        report["host_after"] = host_snapshot()
        report["summary"] = summarize(report["samples"], report["host_before"]["logical_cpus"])
        report["valid"] = True
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        report["error"] = str(error)
    report["finished_at_utc"] = utc_now()
    with args.output.open("x") as output:
        json.dump(report, output, indent=2)
        output.write("\n")
    print(json.dumps({"output": str(args.output), "valid": report["valid"],
                      "summary": report.get("summary"), "error": report.get("error")}, indent=2))
    return 0 if report["valid"] else 1


if __name__ == "__main__":
    sys.exit(main())
