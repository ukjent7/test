"""Exercise the E2E runner and evidence gate using real CI binaries and reports.

Failure scenarios specified before the harness and CI implementation:
Missing suites/cases, modified evidence, a different gateway, nonzero suite exits,
stale output, startup failures, and timeouts must never produce a passing gate.
One failed suite must not skip the others; partial reports and logs must survive.
Windows shutdown must release WebView cache files; a timeout concurrent with
gateway startup must kill every gateway recorded by the interrupted suite.
"""

import csv
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import time


ROOT = Path(__file__).resolve().parent


def run(binary, checker, output, test, *, evidence, platform):
    def verify(directory=evidence, gateway=binary):
        return subprocess.run([sys.executable, str(ROOT / "ci.py"), "verify-evidence",
            "--gateway", str(gateway), "--evidence", str(directory), "--group", "headless",
            "--platform", platform], capture_output=True, text=True, timeout=30)

    result = verify()
    assert result.returncode == 0, result.stderr
    test.passed("valid-evidence-binary-coverage-and-checksums")
    rejections = {}
    with tempfile.TemporaryDirectory(prefix="pipeline-e2e-", dir=output.parent) as temporary:
        workspace = Path(temporary)

        def mutated(name, change):
            directory = workspace / name
            shutil.copytree(evidence, directory)
            change(directory)
            result = verify(directory)
            assert result.returncode != 0, f"{name} incorrectly passed"
            rejections[name] = result.stderr
            test.passed(name)

        def tamper(directory):
            with (directory / "http/report.json").open("ab") as target:
                target.write(b" ")

        mutated("tampered-evidence-rejected", tamper)

        def omit_case(directory):
            from common import write_json, seal
            path = directory / "http/report.json"
            report = json.loads(path.read_text(encoding="utf-8"))
            report["cases"].pop()
            write_json(path, report)
            seal(directory / "http")

        mutated("missing-case-rejected-despite-valid-checksums", omit_case)
        mutated("missing-suite-rejected", lambda directory: shutil.rmtree(directory / "proxy"))

        def fail_exit(directory):
            from common import write_json
            path = directory / "summary.json"
            report = json.loads(path.read_text(encoding="utf-8"))
            report["suites"][0]["exit_code"] = 1
            write_json(path, report)

        mutated("nonzero-suite-exit-rejected-despite-passed-reports", fail_exit)
        corrupt = workspace / binary.name
        shutil.copy2(binary, corrupt)
        with corrupt.open("r+b") as target:
            target.write(b"INVALID")
        result = verify(gateway=corrupt)
        assert result.returncode != 0, "different gateway incorrectly passed"
        rejections["different-binary-rejected"] = result.stderr
        test.passed("different-binary-rejected")

        failed_output = workspace / "failed-run"
        command = [sys.executable, str(ROOT / "run_e2e.py"), "--gateway", str(corrupt),
            "--checker", str(checker), "--group", "headless", "--output", str(failed_output)]
        failed = subprocess.run(command, capture_output=True, text=True, timeout=90)
        assert failed.returncode != 0
        summary = json.loads((failed_output / "summary.json").read_text(encoding="utf-8"))
        assert {suite["name"] for suite in summary["suites"]} == {"http", "history", "proxy"}
        assert all(suite["exit_code"] != 0 for suite in summary["suites"])
        assert all((failed_output / suite["name"] / "report.json").is_file() for suite in summary["suites"])
        (output / "failed-run.log").write_text(failed.stdout + failed.stderr, encoding="utf-8")
        shutil.copytree(failed_output, output / "failed-run")
        test.passed("failed-suite-does-not-skip-other-suites-and-retains-evidence")
        rerun = subprocess.run(command, capture_output=True, text=True, timeout=30)
        assert rerun.returncode != 0 and "empty" in rerun.stderr.lower()
        test.passed("rerun-refuses-stale-output")

        timed_output = workspace / "timed-out-run"
        timeout_command = [sys.executable, str(ROOT / "run_e2e.py"), "--gateway", str(binary),
            "--checker", str(checker), "--group", "headless", "--output", str(timed_output),
            "--suite-timeout", "1"]
        timed = subprocess.run(timeout_command, capture_output=True, text=True, timeout=60)
        assert timed.returncode != 0
        timed_summary = json.loads((timed_output / "summary.json").read_text(encoding="utf-8"))
        assert any(suite["timed_out"] for suite in timed_summary["suites"])
        processes = [process for suite in timed_summary["suites"]
                     for process in json.loads((timed_output / suite["name"] / "report.json").read_text(encoding="utf-8"))["processes"]]
        assert processes, "timeout must exercise a running gateway"

        def alive(pid):
            if os.name == "nt":
                listing = subprocess.run(["tasklist", "/FI", f"PID eq {pid}", "/FO", "CSV", "/NH"],
                                         capture_output=True, text=True, check=True).stdout
                return any(len(row) > 1 and row[1] == str(pid) for row in csv.reader(listing.splitlines()))
            path = Path(f"/proc/{pid}/stat")
            return path.exists() and path.read_text().rsplit(")", 1)[1].strip()[0] != "Z"

        for _ in range(20):
            if not any(alive(process["pid"]) for process in processes):
                break
            time.sleep(0.1)
        assert not any(alive(process["pid"]) for process in processes), "timeout leaked a gateway process"
        shutil.copytree(timed_output, output / "timed-out-run")
        test.passed("timeout-kills-process-tree-and-retains-partial-evidence")

    (output / "rejections.json").write_text(json.dumps(rejections, indent=2), encoding="utf-8")


if __name__ == "__main__":
    from common import suite_cli
    suite_cli("pipeline", run, checker=True, extra_arguments={
        "evidence": {"type": Path, "required": True},
        "platform": {"choices": ["linux", "windows"], "required": True},
    })
