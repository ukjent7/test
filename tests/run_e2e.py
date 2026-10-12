"""Run independent gateway suites and export bounded, repeatable evidence."""

import argparse
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import tempfile
import time

from common import GROUPS, PLATFORM, ROOT, SUITES, check_report, seal, sha256, write_json, write_junit


def kill_tree(process):
    if os.name == "nt":
        subprocess.run(["taskkill", "/PID", str(process.pid), "/T", "/F"],
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=15)
    else:
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
    process.wait(timeout=15)


def run(args):
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    if any(output.iterdir()):
        raise ValueError("E2E output directory must be empty")
    binary = args.gateway.resolve()
    checker = args.checker.resolve() if args.checker else None
    group = args.group or ("gui" if args.suite == "gui" else "headless")
    if group == "headless" and not checker and (not args.suite or args.suite in ("http", "pipeline")):
        raise ValueError("headless E2E requires --checker")
    summary = {"schema_version": 1, "group": group, "platform": PLATFORM,
               "commit": os.environ.get("GITHUB_SHA"), "gateway_sha256": sha256(binary), "suites": []}
    reports = []

    def checkpoint():
        write_json(output / "summary.json", summary)
        write_junit(output / "junit.xml", reports)

    checkpoint()
    with tempfile.TemporaryDirectory(prefix="e2e-work-", dir=output.parent) as temporary:
        workspace = Path(temporary)

        def execute(suite):
            work = workspace / suite
            command = [sys.executable, str(ROOT / SUITES[suite]), "--gateway", str(binary), "--output", str(work)]
            if args.case:
                command.extend(["--case", args.case])
            if suite in ("http", "pipeline"):
                command.extend(["--checker", str(checker)])
            if suite == "pipeline":
                command.extend(["--evidence", str(output), "--platform", PLATFORM])
            log = workspace / f"{suite}.log"
            started = time.monotonic()
            timed_out = False
            with log.open("wb") as stream:
                process = subprocess.Popen(command, stdout=stream, stderr=stream,
                                           start_new_session=os.name != "nt")
                try:
                    exit_code = process.wait(timeout=args.suite_timeout)
                except subprocess.TimeoutExpired:
                    timed_out = True
                    kill_tree(process)
                    exit_code = process.returncode
            work.mkdir(exist_ok=True)
            report_path = work / "report.json"
            report = json.loads(report_path.read_text(encoding="utf-8")) if report_path.exists() else {
                "schema_version": 1, "suite": suite, "cases": [], "processes": [], "status": "failed",
                "platform": PLATFORM, "commit": summary["commit"], "gateway_sha256": summary["gateway_sha256"]}
            if exit_code or timed_out:
                report.update(status="failed", error=report.get("error", "suite timed out" if timed_out else f"suite exited {exit_code}"))
            else:
                try:
                    check_report(report, suite, summary["gateway_sha256"], PLATFORM, summary["commit"], case_filter=args.case)
                except (ValueError, KeyError) as error:
                    report.update(status="failed", error=str(error))
                    exit_code = 1
            report["duration_seconds"] = round(time.monotonic() - started, 3)
            write_json(report_path, report)
            shutil.copy2(log, work / "runner.log")
            manifest = seal(work, {binary.name, checker.name} if checker else {binary.name})
            exported = output / suite
            exported.mkdir()
            for name in (*manifest, "sha256.json"):
                destination = exported / name
                destination.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy2(work / name, destination)
            reports.append(report)
            summary["suites"].append({"name": suite, "exit_code": exit_code, "timed_out": timed_out})
            checkpoint()
            print(f"{suite}: {report['status']} ({len(report['cases'])} cases, {report['duration_seconds']}s)", flush=True)
            if report["status"] != "passed":
                print(log.read_text(encoding="utf-8", errors="replace"), flush=True)

        suites_to_run = [args.suite] if args.suite else GROUPS[group]
        for suite in suites_to_run:
            execute(suite)
        if args.check_pipeline and all(report["status"] == "passed" for report in reports):
            execute("pipeline")
    return 0 if all(report["status"] == "passed" for report in reports) else 1


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--gateway", type=Path, required=True)
    parser.add_argument("--checker", type=Path)
    parser.add_argument("--group", choices=GROUPS)
    parser.add_argument("--suite", choices=list(SUITES.keys()), help="Run only a specific suite")
    parser.add_argument("--case", type=str, help="Filter to a specific test case")
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--suite-timeout", type=float, default=300)
    parser.add_argument("--check-pipeline", action="store_true")
    args = parser.parse_args()
    if not args.group and not args.suite:
        parser.error("either --group or --suite must be specified")
    effective_group = args.group or ("gui" if args.suite == "gui" else "headless")
    if args.suite_timeout <= 0 or (args.check_pipeline and effective_group != "headless"):
        parser.error("timeout must be positive; pipeline checks require the headless group")
    sys.exit(run(args))
