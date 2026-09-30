"""Exercise the real GUI and gateway; run in GitHub Actions only."""

import argparse
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import socket
import subprocess
import threading
import time
import urllib.request

from playwright.sync_api import sync_playwright, expect


def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def run(binary, output):
    output.mkdir(parents=True, exist_ok=True)
    report = {"status": "running", "cases": [], "commit": os.environ.get("GITHUB_SHA"),
              "gateway_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
              "renderer": "native-WebView2" if os.name == "nt" else "Chromium + native-WebKit-smoke"}
    upstream_calls = []

    class Upstream(BaseHTTPRequestHandler):
        def log_message(self, *_args):
            pass

        def do_POST(self):
            body = json.loads(self.rfile.read(int(self.headers["content-length"])))
            upstream_calls.append({"path": self.path, "body": body})
            message = {"id": "gui-e2e", "type": "message", "role": "assistant",
                       "model": body["model"], "stop_reason": "end_turn",
                       "content": [{"type": "thinking", "thinking": "fixture reasoning"},
                                   {"type": "text", "text": "已连接"}],
                       "usage": {"input_tokens": 8, "output_tokens": 4}}
            wire = json.dumps(message).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(wire)))
            self.end_headers()
            self.wfile.write(wire)

    upstream = ThreadingHTTPServer(("127.0.0.1", 0), Upstream)
    threading.Thread(target=upstream.serve_forever, daemon=True).start()
    port, debug_port = free_port(), free_port()
    address = f"http://127.0.0.1:{port}"
    settings_path = output.resolve() / "settings.json"
    env = {**os.environ, "GATEWAY_LISTEN": f"127.0.0.1:{port}",
           "GATEWAY_CONFIG": str(settings_path),
           "WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS": f"--remote-debugging-port={debug_port} --remote-allow-origins=*",
           "WEBVIEW2_USER_DATA_FOLDER": str(output.resolve() / "webview-profile")}
    env.pop("GATEWAY_UPSTREAM_BASE_URL", None)
    log = (output / "desktop.log").open("wb")
    process = None
    browser = None
    context = None
    page = None
    playwright = None
    tracing = False

    def launch(headless=False):
        return subprocess.Popen([str(binary), *(["--headless"] if headless else [])],
                                env=env, stdout=log, stderr=log)

    def wait_ready():
        for _ in range(300):
            if process.poll() is not None:
                raise RuntimeError(f"desktop process exited: {process.returncode}")
            try:
                with urllib.request.urlopen(address + "/health", timeout=0.5) as response:
                    if response.read() == b"ok":
                        return
            except OSError:
                time.sleep(0.1)
        raise RuntimeError("desktop gateway not ready")

    def passed(name):
        report["cases"].append({"name": name, "status": "passed"})

    try:
        process = launch()
        wait_ready()
        playwright = sync_playwright().start()
        if os.name == "nt":
            for _ in range(300):
                try:
                    browser = playwright.chromium.connect_over_cdp(f"http://127.0.0.1:{debug_port}", timeout=1000)
                    break
                except Exception:
                    if process.poll() is not None:
                        raise RuntimeError("native window exited before CDP became available")
                    time.sleep(0.1)
            if browser is None:
                raise RuntimeError("actual desktop WebView2 did not expose CDP")
            context = browser.contexts[0]
            page = context.pages[0] if context.pages else context.wait_for_event("page")
            page.wait_for_url(address + "/")
        else:
            # Process above is an actual native GTK/WebKit window under Xvfb.
            for _ in range(100):
                window_tree = subprocess.run(["xwininfo", "-root", "-tree"], check=True,
                                             capture_output=True, text=True).stdout
                if "Messages Gateway" in window_tree:
                    (output / "native-window-tree.txt").write_text(window_tree)
                    break
                time.sleep(0.1)
            else:
                (output / "native-window-tree.txt").write_text(window_tree)
                raise RuntimeError("native GTK window did not appear")
            browser = playwright.chromium.launch()
            context = browser.new_context(viewport={"width": 760, "height": 760},
                                          permissions=["clipboard-read", "clipboard-write"])
            page = context.new_page()
            page.goto(address)
        context.tracing.start(screenshots=True, snapshots=True, sources=True)
        tracing = True
        expect(page.get_by_role("heading", name="Messages Gateway", exact=True)).to_be_visible()
        expect(page.get_by_test_id("gateway-state")).to_have_text("运行中")
        expect(page.get_by_test_id("endpoint")).to_have_text(address + "/v1")
        passed("native-window-and-live-state")
        page.get_by_role("button", name="复制地址", exact=True).click()
        expect(page.get_by_role("status")).to_contain_text("已复制")
        # WebView2 clipboard permissions are controlled by the host renderer.
        assert page.evaluate("navigator.clipboard.readText()") == address + "/v1"
        passed("copy-endpoint")
        page.get_by_role("button", name="编辑上游", exact=True).click()
        page.get_by_label("上游基地址", exact=True).fill("file:///invalid")
        page.get_by_role("button", name="保存", exact=True).click()
        expect(page.get_by_test_id("settings-error")).to_be_visible()
        assert not settings_path.exists()
        passed("invalid-settings-do-not-overwrite")
        page.keyboard.press("Escape")
        expect(page.get_by_role("dialog")).not_to_be_visible()
        passed("keyboard-dismiss")
        upstream_url = f"http://127.0.0.1:{upstream.server_port}/step_plan/v1"
        page.get_by_role("button", name="编辑上游", exact=True).click()
        page.get_by_label("上游基地址", exact=True).fill(upstream_url)
        page.get_by_role("button", name="保存", exact=True).click()
        expect(page.get_by_role("dialog")).not_to_be_visible()
        expect(page.get_by_test_id("upstream")).to_have_text(upstream_url)
        assert json.loads(settings_path.read_text())["upstream_base_url"] == upstream_url
        passed("save-upstream")
        request_body = {"model": "step-5-preview", "max_tokens": 64, "stream": False,
                        "messages": [{"role": "user", "content": "PRIVATE_USER_MESSAGE"}]}
        response = context.request.post(address + "/v1/messages", data=request_body,
                                         headers={"Authorization": "Bearer PRIVATE_API_KEY"})
        assert response.status == 200
        assert response.json()["content"][0]["signature"] == ""
        assert upstream_calls[-1]["path"] == "/step_plan/v1/messages"
        expect(page.get_by_test_id("request-count")).to_have_text("1")
        expect(page.get_by_test_id("repair-count")).to_have_text("1")
        page.get_by_role("tab", name="活动", exact=True).click()
        expect(page.get_by_test_id("activity-list")).to_contain_text("step-5-preview")
        status = context.request.get(address + "/ui/status").json()
        assert "PRIVATE_USER_MESSAGE" not in json.dumps(status)
        assert "PRIVATE_API_KEY" not in json.dumps(status)
        (output / "request-status.json").write_text(json.dumps(status, ensure_ascii=False, indent=2), encoding="utf-8")
        page.screenshot(path=str(output / "activity-light.png"))
        passed("real-request-repair-and-private-activity")
        page.get_by_role("tab", name="网关", exact=True).click()
        page.get_by_role("switch", name="网关转发", exact=True).click()
        expect(page.get_by_test_id("gateway-state")).to_have_text("已停止")
        calls_before = len(upstream_calls)
        assert context.request.post(address + "/v1/messages", data=request_body).status == 503
        assert len(upstream_calls) == calls_before
        page.get_by_role("switch", name="网关转发", exact=True).click()
        expect(page.get_by_test_id("gateway-state")).to_have_text("运行中")
        assert context.request.post(address + "/v1/messages", data=request_body).status == 200
        passed("stop-and-start-forwarding")
        rejected = context.request.post(address + "/ui/settings", data={"upstream_base_url": "https://evil.example"},
                                        headers={"Origin": "https://evil.example"})
        assert rejected.status == 403
        assert context.request.get(address + "/ui/status").json()["upstream_base_url"] == upstream_url
        passed("cross-origin-control-rejected")
        expect(page.get_by_test_id("request-count")).to_have_text("3")
        expect(page.get_by_role("status")).not_to_be_visible()
        page.get_by_role("button", name="切换主题", exact=True).click()
        expect(page.locator("html")).to_have_attribute("data-theme", "dark")
        page.screenshot(path=str(output / "gateway-dark.png"))
        page.reload()
        expect(page.locator("html")).to_have_attribute("data-theme", "dark")
        page.get_by_role("button", name="切换主题", exact=True).click()
        page.screenshot(path=str(output / "gateway-light.png"))
        page.set_viewport_size({"width": 460, "height": 740})
        assert page.evaluate("document.documentElement.scrollWidth <= innerWidth")
        page.screenshot(path=str(output / "gateway-narrow.png"))
        passed("theme-persistence-and-small-window")
        context.tracing.stop(path=str(output / "gui-trace.zip"))
        tracing = False
        browser.close()
        browser = None
        if process.poll() is None:
            process.terminate()
            process.wait(timeout=10)
        # Verify the persisted file is used by a new process, independent of the UI.
        process = launch(headless=True)
        wait_ready()
        with urllib.request.urlopen(address + "/ui/status") as response:
            restarted = json.load(response)
        assert restarted["upstream_base_url"] == upstream_url
        passed("restart-loads-settings")
        report["status"] = "passed"
    except BaseException as error:
        report.update(status="failed", error=repr(error))
        if page is not None:
            try:
                page.screenshot(path=str(output / "failure.png"))
            except Exception:
                pass
        raise
    finally:
        if tracing:
            try:
                context.tracing.stop(path=str(output / "gui-trace.zip"))
            except Exception:
                pass
        if browser is not None:
            try:
                browser.close()
            except Exception:
                pass
        if playwright is not None:
            playwright.stop()
        if process is not None and process.poll() is None:
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
        upstream.shutdown()
        upstream.server_close()
        log.close()
        (output / "report.json").write_text(json.dumps(report, ensure_ascii=False, indent=2), encoding="utf-8")
        manifest = {path.name: hashlib.sha256(path.read_bytes()).hexdigest()
                    for path in sorted(output.iterdir()) if path.is_file() and path.name != "sha256.json"}
        (output / "sha256.json").write_text(json.dumps(manifest, indent=2), encoding="utf-8")
    print(f"GUI E2E passed: {len(report['cases'])} cases; evidence: {output}")


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--gateway", type=Path, required=True)
    parser.add_argument("--output", type=Path, default=Path("artifacts/gui"))
    args = parser.parse_args()
    run(args.gateway.resolve(), args.output)
