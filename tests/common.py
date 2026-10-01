"""Shared lifecycle and verifiable evidence for real gateway E2E suites."""

import argparse
from collections import Counter
from contextlib import ExitStack
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import platform
import socket
import subprocess
import tempfile
import threading
import time
import traceback
import urllib.request
import xml.etree.ElementTree as ET


ROOT = Path(__file__).resolve().parent
PLATFORM = "windows" if os.name == "nt" else "linux"
SUITES = {"http": "http_e2e.py", "history": "history_e2e.py", "proxy": "proxy_e2e.py", "gui": "gui_e2e.py", "pipeline": "pipeline_e2e.py"}
GROUPS = {"headless": ("http", "history", "proxy"), "gui": ("gui",)}

_windows_job = None


def contain_windows_children():
    """Let Windows kill every descendant when this suite exits, even mid-spawn."""
    global _windows_job
    if os.name != "nt" or _windows_job is not None:
        return
    import ctypes
    from ctypes import wintypes

    class BasicLimits(ctypes.Structure):
        _fields_ = [("process_time", ctypes.c_int64), ("job_time", ctypes.c_int64),
                    ("flags", wintypes.DWORD), ("min_working_set", ctypes.c_size_t),
                    ("max_working_set", ctypes.c_size_t), ("active_processes", wintypes.DWORD),
                    ("affinity", ctypes.c_size_t), ("priority", wintypes.DWORD),
                    ("scheduling", wintypes.DWORD)]

    class ExtendedLimits(ctypes.Structure):
        _fields_ = [("basic", BasicLimits), ("io", ctypes.c_uint64 * 6),
                    ("process_memory", ctypes.c_size_t), ("job_memory", ctypes.c_size_t),
                    ("peak_process_memory", ctypes.c_size_t), ("peak_job_memory", ctypes.c_size_t)]

    kernel = ctypes.WinDLL("kernel32", use_last_error=True)
    for name, arguments, result in [
        ("CreateJobObjectW", [ctypes.c_void_p, wintypes.LPCWSTR], wintypes.HANDLE),
        ("SetInformationJobObject", [wintypes.HANDLE, ctypes.c_int, ctypes.c_void_p, wintypes.DWORD], wintypes.BOOL),
        ("AssignProcessToJobObject", [wintypes.HANDLE, wintypes.HANDLE], wintypes.BOOL),
        ("GetCurrentProcess", [], wintypes.HANDLE),
        ("CloseHandle", [wintypes.HANDLE], wintypes.BOOL),
    ]:
        function = getattr(kernel, name)
        function.argtypes, function.restype = arguments, result
    job = kernel.CreateJobObjectW(None, None)
    if not job:
        raise ctypes.WinError(ctypes.get_last_error())
    limits = ExtendedLimits()
    limits.basic.flags = 0x2000  # JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
    if not kernel.SetInformationJobObject(job, 9, ctypes.byref(limits), ctypes.sizeof(limits)) or not kernel.AssignProcessToJobObject(job, kernel.GetCurrentProcess()):
        error = ctypes.WinError(ctypes.get_last_error())
        kernel.CloseHandle(job)
        raise error
    # Keep the non-inherited handle open until process exit. Closing it here
    # would terminate this suite as well as its descendants.
    _windows_job = job


def sha256(path):
    with Path(path).open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def write_json(path, value):
    path = Path(path)
    temporary = path.with_name(path.name + ".tmp")
    temporary.write_text(json.dumps(value, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    temporary.replace(path)


def expected_cases(suite, platform_name):
    contract = json.loads((ROOT / "cases.json").read_text(encoding="utf-8"))
    return contract[suite] + contract.get(f"{suite}_{platform_name}", [])


def check_report(report, suite, digest, platform_name, commit=None):
    if report["suite"] != suite or report["status"] != "passed":
        raise ValueError(f"{suite}: suite did not pass")
    if report["gateway_sha256"] != digest or report["platform"] != platform_name:
        raise ValueError(f"{suite}: binary or platform mismatch")
    if commit is not None and report["commit"] != commit:
        raise ValueError(f"{suite}: commit mismatch")
    names = [case["name"] for case in report["cases"] if case["status"] == "passed"]
    if len(names) != len(report["cases"]) or Counter(names) != Counter(expected_cases(suite, platform_name)):
        raise ValueError(f"{suite}: missing, duplicate, or unexpected cases")


def seal(directory, excluded=()):
    directory = Path(directory)
    files = [path for path in sorted(directory.rglob("*")) if path.is_file()
             and path.relative_to(directory).as_posix() != "sha256.json" and path.name not in excluded
             and "webview" not in path.relative_to(directory).parts]
    manifest = {path.relative_to(directory).as_posix(): sha256(path) for path in files}
    write_json(directory / "sha256.json", manifest)
    return manifest


def verify_manifest(directory):
    directory = Path(directory).resolve()
    manifest = json.loads((directory / "sha256.json").read_text(encoding="utf-8"))
    actual = {path.relative_to(directory).as_posix() for path in directory.rglob("*")
              if path.is_file() and path.relative_to(directory).as_posix() != "sha256.json"}
    if not manifest or set(manifest) != actual or "report.json" not in manifest:
        raise ValueError(f"{directory.name}: incomplete evidence manifest")
    for name, digest in manifest.items():
        path = (directory / name).resolve()
        if not path.is_relative_to(directory) or sha256(path) != digest:
            raise ValueError(f"{directory.name}: evidence checksum mismatch: {name}")


def write_junit(path, reports):
    root = ET.Element("testsuites")
    for report in reports:
        failed = report["status"] != "passed"
        suite = ET.SubElement(root, "testsuite", name=report["suite"],
            tests=str(len(report["cases"]) + int(failed)), failures=str(int(failed)),
            time=str(report.get("duration_seconds", 0)))
        for case in report["cases"]:
            ET.SubElement(suite, "testcase", name=case["name"], classname=report["suite"])
        if failed:
            case = ET.SubElement(suite, "testcase", name="suite execution", classname=report["suite"])
            ET.SubElement(case, "failure", message=report.get("error", "suite failed")).text = report.get("traceback", "")
    ET.indent(root)
    ET.ElementTree(root).write(path, encoding="utf-8", xml_declaration=True)


def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def stop_process(process, timeout=5):
    if process.poll() is None:
        if os.name == "nt":
            subprocess.run(["taskkill", "/PID", str(process.pid), "/T", "/F"],
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=15)
        else:
            process.terminate()
        try:
            process.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=timeout)


def wait_ready(process, port, timeout=30):
    deadline = time.monotonic() + timeout
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise RuntimeError(f"gateway exited during startup: {process.returncode}")
        try:
            with opener.open(f"http://127.0.0.1:{port}/health", timeout=0.5) as response:
                if response.read() == b"ok":
                    return
        except OSError:
            time.sleep(0.05)
    raise TimeoutError("gateway did not become ready")


class E2E:
    def __init__(self, suite, binary, output, checker=None):
        output.mkdir(parents=True, exist_ok=True)
        if any(output.iterdir()):
            raise ValueError("E2E output directory must be empty")
        self.output = output
        self.excluded = {binary.name, checker.name} if checker else {binary.name}
        self.resources = ExitStack()
        self.started = time.monotonic()
        self.report = {"schema_version": 1, "suite": suite, "status": "running", "cases": [], "processes": [],
            "commit": os.environ.get("GITHUB_SHA"), "platform": PLATFORM, "python": platform.python_version(),
            "started_at": datetime.now(timezone.utc).isoformat(), "gateway_sha256": sha256(binary)}
        if checker:
            self.report["grok_decoder_sha256"] = sha256(checker)

    def __enter__(self):
        contain_windows_children()
        write_json(self.output / "report.json", self.report)
        return self

    def __exit__(self, kind, error, stack):
        try:
            self.resources.close()
        except BaseException as cleanup_error:
            self.report["cleanup_error"] = repr(cleanup_error)
            if error is None:
                error = cleanup_error
        self.report["duration_seconds"] = round(time.monotonic() - self.started, 3)
        self.report["status"] = "failed" if error else "passed"
        if error:
            self.report.update(error=repr(error), traceback="".join(traceback.format_exception(error)))
        write_json(self.output / "report.json", self.report)
        seal(self.output, self.excluded)
        if error and kind is None:
            raise error
        return False

    def passed(self, name):
        self.report["cases"].append({"name": name, "status": "passed", "elapsed_seconds": round(time.monotonic() - self.started, 3)})
        write_json(self.output / "report.json", self.report)

    def server(self, server):
        def stop():
            server.shutdown()
            server.server_close()
        self.resources.callback(stop)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        return server

    def log(self, name):
        return self.resources.enter_context((self.output / name).open("wb"))

    def spawn(self, command, **options):
        process = subprocess.Popen(command, **options)
        self.resources.callback(stop_process, process)
        self.report["processes"].append({"pid": process.pid, "command": [str(part) for part in command]})
        write_json(self.output / "report.json", self.report)
        return process


def suite_cli(name, run, *, checker=False, extra_arguments=None):
    parser = argparse.ArgumentParser()
    parser.add_argument("--gateway", type=Path, required=True)
    parser.add_argument("--output", type=Path)
    if checker:
        parser.add_argument("--checker", type=Path, required=True)
    for option, settings in (extra_arguments or {}).items():
        parser.add_argument(f"--{option}", **settings)
    args = vars(parser.parse_args())
    binary = args.pop("gateway").resolve()
    decoder = args.pop("checker").resolve() if checker else None
    output = args.pop("output")
    if output is None:
        directory = Path("artifacts/e2e")
        directory.mkdir(parents=True, exist_ok=True)
        output = Path(tempfile.mkdtemp(prefix=f"{name}-", dir=directory))
    with E2E(name, binary, output, decoder) as test:
        positional = (binary, decoder, output, test) if checker else (binary, output, test)
        run(*positional, **args)
    print(f"{name} E2E passed: {len(test.report['cases'])} cases; evidence: {output}")
