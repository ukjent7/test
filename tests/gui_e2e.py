"""Exercise the real GUI and gateway; run in GitHub Actions only.

Frontend refactor failure scenarios, specified before implementation:
- Missing module assets or script errors leave the application uninitialized.
- Tabs cannot be reached by keyboard, or focus and selected panel disagree.
- Closing settings retains secret drafts; repeated saves or Escape interrupt a save.
- Model refresh failures erase the catalog; empty/filter results leave copy enabled.
- Generated client configuration ignores the selected model or protocol.
- A disconnected status leaves write controls enabled or cannot recover on refresh.
- An older usage response overwrites a newly selected period.
Existing cases cover themes, narrow layouts, literal diff content, clipboard,
provider/proxy persistence, request filtering, and complete wire/history details.
"""

import argparse
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import threading
import time
import tomllib
import urllib.request

from playwright.sync_api import sync_playwright, expect
from e2e import EVENTS, decode_sse, frame


def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def run(binary, output):
    output.mkdir(parents=True, exist_ok=True)
    report = {"status": "running", "cases": [], "commit": os.environ.get("GITHUB_SHA"),
              "gateway_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
              "renderer": "native-WebView2" if os.name == "nt" else "Chromium + native-WebKit-smoke"}
    copied_binary = output.resolve() / binary.name
    shutil.copy2(binary, copied_binary)
    binary = copied_binary
    launch_cwd = output.resolve() / "other-launch-directory"
    launch_cwd.mkdir(exist_ok=True)
    upstream_calls = []
    model_calls = []

    class Upstream(BaseHTTPRequestHandler):
        def log_message(self, *_args):
            pass

        def do_GET(self):
            provider = "opencode" if "/zen/" in self.path else "stepfun"
            model_calls.append({"path": self.path, "authorization": self.headers.get("Authorization")})
            wire = json.dumps({"object": "list", "data": [{"id": "mimo-v2.5-free" if provider == "opencode" else "step-5-preview",
                               "object": "model"}]}).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(wire)))
            self.end_headers()
            self.wfile.write(wire)

        def do_POST(self):
            body = json.loads(self.rfile.read(int(self.headers["content-length"])))
            upstream_calls.append({"path": self.path, "body": body})
            message = {"id": "gui-e2e", "type": "message", "role": "assistant",
                       "model": body["model"], "stop_reason": "end_turn",
                       "content": [{"type": "thinking", "thinking": "fixture reasoning"},
                                   {"type": "text", "text": "已连接"}],
                       "usage": {"input_tokens": 100, "output_tokens": 4,
                                 "cache_read_input_tokens": 700, "cache_creation_input_tokens": 200}}
            if body["model"] == "cache-unknown":
                message["usage"] = {"input_tokens": 100}
            elif body["model"] == "cache-zero":
                message["usage"] = {"input_tokens": 100, "cache_read_input_tokens": 0,
                                    "cache_creation_input_tokens": 0}
            if body["model"] == "diff-json":
                message["content"] = [
                    {"type": "thinking", "thinking": None, "signature": None},
                    {"type": "thinking", "thinking": "PRIVATE_RESPONSE_REASONING", "signature": "KEEP_SIGNATURE"},
                    {"type": "text", "text": "PRIVATE_RESPONSE_BODY"},
                ]
            if body["model"] == "history-gui-error":
                wire = b'{ "error" : "GUI error \\t \\n" } \r\n'
                self.send_response(429)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(wire)))
                self.end_headers()
                self.wfile.write(wire)
                return
            streaming = body.get("stream", False)
            wire = b"".join(frame(event) for event in EVENTS) if streaming else json.dumps(message).encode()
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream" if streaming else "application/json")
            self.send_header("Content-Length", str(len(wire)))
            self.end_headers()
            self.wfile.write(wire)

    upstream = ThreadingHTTPServer(("127.0.0.1", 0), Upstream)
    threading.Thread(target=upstream.serve_forever, daemon=True).start()
    port, debug_port = free_port(), free_port()
    address = f"http://127.0.0.1:{port}"
    settings_path = output.resolve() / "settings.json"
    settings_path.unlink(missing_ok=True)
    database = output.resolve() / "requests.sqlite3"
    database.unlink(missing_ok=True)
    env = {**os.environ, "GATEWAY_LISTEN": f"127.0.0.1:{port}",
           "WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS": f"--remote-debugging-port={debug_port} --remote-allow-origins=*"}
    for name in ("GATEWAY_CONFIG", "GATEWAY_DB", "WEBVIEW2_USER_DATA_FOLDER"):
        env.pop(name, None)
    env.pop("GATEWAY_UPSTREAM_BASE_URL", None)
    for name in ("GATEWAY_OPENCODE_BASE_URL", "GATEWAY_STEPFUN_API_KEY", "GATEWAY_OPENCODE_API_KEY"):
        env.pop(name, None)
    log = (output / "desktop.log").open("wb")
    process = None
    browser = None
    context = None
    page = None
    playwright = None
    tracing = False

    def launch(headless=False):
        return subprocess.Popen([str(binary), *(["--headless"] if headless else [])],
                                env=env, cwd=launch_cwd, stdout=log, stderr=log)

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
        script_errors = []
        page.on("pageerror", lambda error: script_errors.append(str(error)))
        expect(page.get_by_role("heading", name="Messages Gateway", exact=True)).to_be_visible()
        expect(page.get_by_test_id("gateway-state")).to_have_text("运行中")
        expect(page.get_by_test_id("endpoint")).to_have_text(address + "/v1")
        assert (output / "webview").is_dir(), "native WebView must use the program directory"
        assert not list(launch_cwd.iterdir()), "runtime files must not follow the working directory"
        (output / "runtime-paths.json").write_text(json.dumps({"program": str(binary), "cwd": str(launch_cwd),
            "settings": str(settings_path), "database": str(database), "webview": str(output.resolve() / "webview")}, indent=2))
        passed("portable-default-webview-config-and-database-paths")
        passed("native-window-and-live-state")
        page.get_by_role("tab", name="网关", exact=True).focus()
        page.keyboard.press("ArrowRight")
        expect(page.get_by_role("tab", name="活动", exact=True)).to_be_focused()
        expect(page.locator("#activity")).to_be_visible()
        page.keyboard.press("End")
        expect(page.get_by_role("tab", name="统计", exact=True)).to_be_focused()
        expect(page.locator("#usage")).to_be_visible()
        page.keyboard.press("Home")
        expect(page.get_by_role("tab", name="网关", exact=True)).to_be_focused()
        assert page.locator('[role="tab"][tabindex="0"]').count() == 1
        passed("keyboard-tabs-focus-and-panel-agree")
        page.get_by_role("button", name="复制地址", exact=True).click()
        expect(page.get_by_role("status")).to_contain_text("已复制")
        # WebView2 clipboard permissions are controlled by the host renderer.
        assert page.evaluate("navigator.clipboard.readText()") == address + "/v1"
        passed("copy-endpoint")
        page.get_by_role("button", name="编辑上游", exact=True).click()
        expect(page.get_by_label("网络代理模式", exact=True)).to_have_value("system")
        expect(page.get_by_label("StepFun 使用网络代理", exact=True)).to_be_checked()
        expect(page.get_by_label("OpenCode Zen 使用网络代理", exact=True)).to_be_checked()
        page.get_by_label("StepFun 上游基地址", exact=True).fill("file:///invalid")
        page.get_by_role("button", name="保存", exact=True).click()
        expect(page.get_by_test_id("settings-error")).to_be_visible()
        assert not settings_path.exists()
        passed("invalid-settings-do-not-overwrite")
        page.keyboard.press("Escape")
        expect(page.get_by_role("dialog")).not_to_be_visible()
        passed("keyboard-dismiss")
        original_upstream = context.request.get(address + "/ui/status").json()["upstream_base_url"]
        page.get_by_role("button", name="编辑上游", exact=True).click()
        expect(page.get_by_label("OpenCode Zen 上游基地址", exact=True)).to_have_value("https://opencode.ai/zen/v1")
        page.get_by_label("OpenCode Zen 上游基地址", exact=True).fill("https://cancelled.example/v1")
        page.get_by_label("StepFun API key", exact=True).fill("DISCARDED_SECRET_DRAFT")
        page.get_by_role("button", name="取消", exact=True).click()
        expect(page.get_by_label("StepFun API key", exact=True)).to_have_value("")
        assert context.request.get(address + "/ui/status").json()["upstream_base_url"] == original_upstream
        assert not settings_path.exists()
        passed("cancel-keeps-both-upstreams")
        upstream_url = f"http://127.0.0.1:{upstream.server_port}/step_plan/v1"
        opencode_url = f"http://127.0.0.1:{upstream.server_port}/zen/v1"
        page.get_by_role("button", name="编辑上游", exact=True).click()
        page.get_by_label("StepFun 上游基地址", exact=True).fill(upstream_url)
        page.get_by_label("OpenCode Zen 上游基地址", exact=True).fill(opencode_url)
        page.get_by_label("StepFun API key", exact=True).fill("GUI_STEP_KEY")
        page.get_by_label("OpenCode Zen API key", exact=True).fill("GUI_ZEN_KEY")
        page.screenshot(path=str(output / "dual-upstream-settings.png"))
        page.get_by_role("button", name="保存", exact=True).click()
        expect(page.get_by_role("dialog")).not_to_be_visible()
        expect(page.get_by_test_id("upstream")).to_have_text(upstream_url)
        assert json.loads(settings_path.read_text())["upstream_base_url"] == upstream_url
        expect(page.get_by_test_id("opencode-upstream")).to_have_text(opencode_url)
        passed("save-upstream")
        page.get_by_role("button", name="编辑上游", exact=True).click()
        expect(page.get_by_label("StepFun API key", exact=True)).to_have_value("")
        expect(page.get_by_label("OpenCode Zen API key", exact=True)).to_have_value("")
        expect(page.get_by_label("StepFun API key", exact=True)).to_have_attribute("placeholder", "已设置，留空保留")
        page.get_by_role("button", name="保存", exact=True).click()
        expect(page.get_by_role("dialog")).not_to_be_visible()
        saved = json.loads(settings_path.read_text())
        assert saved["stepfun_api_key"] == "GUI_STEP_KEY" and saved["opencode_api_key"] == "GUI_ZEN_KEY"
        pending_saves = []
        page.route("**/ui/settings", lambda route: pending_saves.append(route))
        page.get_by_role("button", name="编辑上游", exact=True).click()
        page.get_by_role("button", name="保存", exact=True).click()
        expect(page.get_by_role("button", name="保存", exact=True)).to_be_disabled()
        expect(page.get_by_label("StepFun 上游基地址", exact=True)).to_be_disabled()
        page.keyboard.press("Escape")
        expect(page.locator("#settings-dialog")).to_be_visible()
        expect(page.get_by_role("button", name="取消", exact=True)).to_be_disabled()
        assert len(pending_saves) == 1
        pending_saves[0].continue_()
        expect(page.locator("#settings-dialog")).not_to_be_visible()
        page.unroute("**/ui/settings")
        assert json.loads(settings_path.read_text()) == saved
        passed("settings-drafts-cleared-and-pending-save-serialized")
        page.get_by_role("button", name="设置网络代理", exact=True).click()
        page.get_by_label("网络代理模式", exact=True).select_option("custom")
        expect(page.get_by_label("代理地址", exact=True)).to_be_visible()
        page.get_by_label("代理地址", exact=True).fill("http://127.0.0.1:1")
        page.get_by_label("StepFun 使用网络代理", exact=True).uncheck()
        page.screenshot(path=str(output / "proxy-settings-light.png"))
        page.get_by_role("button", name="保存", exact=True).click()
        expect(page.get_by_role("dialog")).not_to_be_visible()
        expect(page.locator("#proxy-status")).to_contain_text("自定义代理 · StepFun 直连")
        proxy_saved = json.loads(settings_path.read_text())
        assert proxy_saved["proxy"] == "http://127.0.0.1:1" and not proxy_saved["stepfun_use_proxy"] and proxy_saved["opencode_use_proxy"]
        page.get_by_role("button", name="设置网络代理", exact=True).click()
        expect(page.get_by_label("网络代理模式", exact=True)).to_have_value("custom")
        expect(page.get_by_label("代理地址", exact=True)).to_have_value("http://127.0.0.1:1")
        expect(page.get_by_label("StepFun 使用网络代理", exact=True)).not_to_be_checked()
        page.get_by_label("代理地址", exact=True).fill("file:///invalid")
        page.get_by_role("button", name="保存", exact=True).click()
        expect(page.get_by_test_id("settings-error")).to_contain_text("代理地址须使用")
        assert json.loads(settings_path.read_text()) == proxy_saved
        page.get_by_label("代理地址", exact=True).fill("socks5://127.0.0.1:1080")
        page.evaluate("document.documentElement.dataset.theme = 'dark'")
        page.set_viewport_size({"width": 460, "height": 740})
        assert page.evaluate("document.querySelector('#settings-dialog').scrollWidth <= document.querySelector('#settings-dialog').clientWidth")
        page.screenshot(path=str(output / "proxy-settings-dark-narrow.png"))
        page.get_by_role("button", name="取消", exact=True).click()
        assert json.loads(settings_path.read_text()) == proxy_saved
        page.set_viewport_size({"width": 760, "height": 760})
        page.evaluate("document.documentElement.dataset.theme = 'light'")
        page.get_by_role("button", name="设置网络代理", exact=True).click()
        page.get_by_label("网络代理模式", exact=True).select_option("direct")
        expect(page.get_by_label("代理地址", exact=True)).not_to_be_visible()
        page.get_by_role("button", name="保存", exact=True).click()
        expect(page.get_by_role("dialog")).not_to_be_visible()
        assert json.loads(settings_path.read_text())["proxy"] == "direct"
        page.get_by_role("button", name="设置网络代理", exact=True).click()
        page.get_by_label("网络代理模式", exact=True).select_option("system")
        page.get_by_label("StepFun 使用网络代理", exact=True).check()
        page.get_by_role("button", name="保存", exact=True).click()
        expect(page.get_by_role("dialog")).not_to_be_visible()
        assert json.loads(settings_path.read_text())["proxy"] == ""
        passed("proxy-defaults-provider-switches-validation-cancel-and-narrow-settings")
        page.get_by_role("button", name="拉取模型", exact=True).click()
        expect(page.get_by_label("可用模型", exact=True)).to_be_enabled()
        assert page.locator("#model-select option").all_text_contents() == ["stepfun/step-5-preview", "opencode/mimo-v2.5-free"]
        assert {call["authorization"] for call in model_calls} == {"Bearer GUI_STEP_KEY", "Bearer GUI_ZEN_KEY"}
        page.get_by_label("可用模型", exact=True).select_option("opencode/mimo-v2.5-free")
        page.get_by_role("button", name="复制模型 ID", exact=True).click()
        expect(page.get_by_role("status")).to_contain_text("已复制")
        assert page.evaluate("navigator.clipboard.readText()") == "opencode/mimo-v2.5-free"
        page.screenshot(path=str(output / "prefixed-model-catalog.png"))
        state = context.request.get(address + "/ui/status").json()
        assert "GUI_STEP_KEY" not in json.dumps(state) and "GUI_ZEN_KEY" not in json.dumps(state)
        (output / "models.json").write_text(json.dumps(context.request.get(address + "/v1/models").json(), indent=2))
        passed("configured-keys-private-model-fetch-and-copy")
        page.get_by_label("筛选模型", exact=True).fill("STEPFUN")
        expect(page.get_by_label("可用模型", exact=True)).to_have_value("stepfun/step-5-preview")
        page.get_by_label("筛选模型", exact=True).fill("no-model-matches")
        expect(page.get_by_role("button", name="复制模型 ID", exact=True)).to_be_disabled()
        page.get_by_label("筛选模型", exact=True).fill("")
        page.get_by_label("可用模型", exact=True).select_option("opencode/mimo-v2.5-free")
        page.route("**/v1/models", lambda route: route.fulfill(status=502, content_type="application/json",
                   body=json.dumps({"error": {"message": "MODEL_REFRESH_FAILURE"}})))
        page.get_by_role("button", name="拉取模型", exact=True).click()
        expect(page.locator("#model-error")).to_contain_text("MODEL_REFRESH_FAILURE")
        expect(page.get_by_label("可用模型", exact=True)).to_have_value("opencode/mimo-v2.5-free")
        expect(page.get_by_role("button", name="复制模型 ID", exact=True)).to_be_enabled()
        page.unroute("**/v1/models")
        page.route("**/v1/models", lambda route: route.fulfill(content_type="application/json",
                   body=json.dumps({"data": [], "upstream_errors": []})))
        page.get_by_role("button", name="拉取模型", exact=True).click()
        expect(page.get_by_label("可用模型", exact=True)).to_be_disabled()
        expect(page.get_by_role("button", name="复制模型 ID", exact=True)).to_be_disabled()
        page.unroute("**/v1/models")
        page.get_by_role("button", name="拉取模型", exact=True).click()
        expect(page.get_by_label("可用模型", exact=True)).to_be_enabled()
        page.get_by_label("可用模型", exact=True).select_option("opencode/mimo-v2.5-free")
        page.get_by_label("接入协议", exact=True).select_option("responses")
        page.locator("#client-config").evaluate("element => element.open = true")
        page.get_by_role("button", name="复制配置", exact=True).click()
        configuration = page.evaluate("navigator.clipboard.readText()")
        model_configuration = tomllib.loads(configuration)["model"]["gateway"]
        assert model_configuration == {"model": "opencode/mimo-v2.5-free", "base_url": address + "/v1",
                                       "api_key": "", "api_backend": "responses"}
        (output / "client-config.txt").write_text(configuration, encoding="utf-8")
        page.screenshot(path=str(output / "model-recovery-and-client-config.png"))
        passed("model-search-refresh-failure-empty-recovery-and-client-config")
        page.route("**/ui/status", lambda route: route.abort())
        page.get_by_role("button", name="刷新状态", exact=True).click()
        expect(page.get_by_test_id("gateway-state")).to_have_text("连接已断开")
        expect(page.get_by_role("switch", name="网关转发", exact=True)).to_be_disabled()
        expect(page.get_by_role("button", name="编辑上游", exact=True)).to_be_disabled()
        page.screenshot(path=str(output / "connection-disconnected.png"))
        page.unroute("**/ui/status")
        page.get_by_role("button", name="刷新状态", exact=True).click()
        expect(page.get_by_test_id("gateway-state")).to_have_text("运行中")
        expect(page.get_by_role("button", name="编辑上游", exact=True)).to_be_enabled()
        passed("connection-failure-disables-writes-and-manual-refresh-recovers")
        request_body = {"model": "step-5-preview", "max_tokens": 64, "stream": False,
                        "messages": [{"role": "user", "content": "PRIVATE_USER_MESSAGE"}]}
        response = context.request.post(address + "/v1/messages", data=request_body,
                                         headers={"Authorization": "Bearer PRIVATE_API_KEY", "x-grok-conv-id": "PRIVATE_GUI_SESSION"})
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
        assert "PRIVATE_GUI_SESSION" not in json.dumps(status)
        expect(page.get_by_test_id("cache-usage").first).to_contain_text("缓存命中 70%")
        expect(page.get_by_test_id("cache-usage").first).to_contain_text("输入 1,000 · 读取 700 · 写入 200")
        expect(page.get_by_test_id("routing-identity").first).to_contain_text("x-grok-conv-id")
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
        page.set_viewport_size({"width": 760, "height": 760})
        page.get_by_role("tab", name="活动", exact=True).click()

        for model, label in (("cache-zero", "缓存命中 0%"), ("cache-unknown", "缓存命中未报告")):
            response = context.request.post(address + "/v1/messages", data={**request_body, "model": model})
            assert response.status == 200
            expect(page.get_by_test_id("activity-list")).to_contain_text(model)
            expect(page.get_by_test_id("cache-usage").first).to_contain_text(label)
        page.screenshot(path=str(output / "cache-activity-light.png"))
        page.get_by_role("button", name="切换主题", exact=True).click()
        page.screenshot(path=str(output / "cache-activity-dark.png"))
        page.set_viewport_size({"width": 460, "height": 740})
        assert page.evaluate("document.documentElement.scrollWidth <= innerWidth")
        page.screenshot(path=str(output / "cache-activity-narrow.png"))
        page.set_viewport_size({"width": 760, "height": 760})
        page.get_by_role("button", name="切换主题", exact=True).click()
        (output / "cache-activity.json").write_text(json.dumps(context.request.get(address + "/ui/status").json(),
            ensure_ascii=False, indent=2), encoding="utf-8")
        passed("cache-usage-zero-unknown-routing-and-themes")

        response = context.request.post(address + "/v1/messages", data={**request_body, "model": "opencode/step-5-preview"})
        assert response.status == 200
        page.get_by_role("tab", name="统计", exact=True).click()
        expect(page.get_by_test_id("usage-total")).to_contain_text("67.7%")
        expect(page.get_by_test_id("usage-total")).to_contain_text("3,200")
        expect(page.get_by_test_id("usage-providers")).to_contain_text("StepFun")
        expect(page.get_by_test_id("usage-providers")).to_contain_text("OpenCode Zen")
        expect(page.get_by_test_id("usage-models")).to_contain_text("StepFun / step-5-preview")
        expect(page.get_by_test_id("usage-models")).to_contain_text("OpenCode Zen / step-5-preview")
        expect(page.get_by_test_id("usage-models")).to_contain_text("未报告")
        for name in ("今天", "近 7 天", "近 30 天", "全部"):
            page.get_by_role("button", name=name, exact=True).click()
            expect(page.get_by_role("button", name=name, exact=True)).to_have_attribute("aria-pressed", "true")
            expect(page.get_by_test_id("usage-total")).to_contain_text("67.7%")
        page.screenshot(path=str(output / "usage-light.png"))
        page.get_by_role("button", name="切换主题", exact=True).click()
        page.screenshot(path=str(output / "usage-dark.png"))
        page.set_viewport_size({"width": 460, "height": 740})
        assert page.evaluate("document.documentElement.scrollWidth <= innerWidth")
        page.screenshot(path=str(output / "usage-narrow.png"))
        page.set_viewport_size({"width": 760, "height": 760})
        page.get_by_role("button", name="切换主题", exact=True).click()
        (output / "usage.json").write_text(json.dumps(context.request.get(address + "/ui/usage").json(), indent=2), encoding="utf-8")
        passed("usage-periods-three-levels-weighted-hit-rate-and-themes")
        pending_usage = []
        usage_fixture = context.request.get(address + "/ui/usage").json()
        page.route("**/ui/usage?*", lambda route: pending_usage.append(route))
        page.get_by_role("button", name="今天", exact=True).click()
        expect(page.locator("#usage")).to_have_attribute("aria-busy", "true")
        page.get_by_role("button", name="全部", exact=True).click()
        expect(page.get_by_role("button", name="全部", exact=True)).to_have_attribute("aria-pressed", "true")
        for _ in range(50):
            if len(pending_usage) >= 2:
                break
            page.wait_for_timeout(20)
        assert len(pending_usage) == 2
        newest = json.loads(json.dumps(usage_fixture))
        newest["total"]["requests"] = 222
        pending_usage[1].fulfill(content_type="application/json", body=json.dumps(newest))
        expect(page.get_by_test_id("usage-total")).to_contain_text("222")
        oldest = json.loads(json.dumps(usage_fixture))
        oldest["total"]["requests"] = 111
        pending_usage[0].fulfill(content_type="application/json", body=json.dumps(oldest))
        page.wait_for_timeout(100)
        expect(page.get_by_test_id("usage-total")).not_to_contain_text("111")
        page.unroute("**/ui/usage?*")
        page.get_by_role("button", name="近 30 天", exact=True).click()
        expect(page.get_by_test_id("usage-total")).to_contain_text("67.7%")
        (output / "frontend-transitions.json").write_text(json.dumps({"pending_saves": len(pending_saves),
            "period_requests": len(pending_usage), "client_config": model_configuration}, indent=2), encoding="utf-8")
        passed("usage-latest-period-wins-over-out-of-order-responses")
        page.get_by_role("tab", name="活动", exact=True).click()

        def latest_detail():
            calls = context.request.get(address + "/ui/status").json()["calls"]
            assert all("diff" not in call for call in calls), "polling must not download diff bodies"
            return context.request.get(address + f"/ui/calls/{calls[0]['id']}").json()

        def open_diff(detail):
            name = f"查看差异 #{detail['call']['id']}"
            expect(page.get_by_role("button", name=name, exact=True)).to_be_visible()
            page.get_by_role("button", name=name, exact=True).click()
            expect(page.get_by_test_id("diff-request")).to_be_visible()
            expect(page.get_by_test_id("diff-response")).to_be_visible()

        # No response repair and disabled forwarding must not invent changes.
        disabled_call = next(call for call in context.request.get(address + "/ui/status").json()["calls"]
                             if call["status"] == 503)
        disabled_diff = context.request.get(address + f"/ui/calls/{disabled_call['id']}").json()
        assert disabled_diff["diff"] == {"request": [], "response": []}
        assert context.request.get(address + "/ui/calls/99999").status == 404
        assert context.request.get(address + "/ui/calls/1", headers={"Origin": "https://evil.example"}).status == 403
        open_diff(disabled_diff)
        expect(page.get_by_test_id("diff-request")).to_have_text("请求 · Grok Build → 上游未记录主动修补；完整字节差异见下方")
        expect(page.get_by_test_id("diff-response")).to_have_text("响应 · 上游 → Grok Build未记录主动修补；完整字节差异见下方")
        page.keyboard.press("Escape")
        passed("diff-empty-expired-and-local-only")

        attack = '<img src=x onerror="window.DIFF_XSS=true">思考文本'
        diff_body = {"model": "diff-json", "max_tokens": 64, "stream": False, "messages": [
            {"role": "user", "content": "PRIVATE_USER_MESSAGE"},
            {"role": "assistant", "content": [{"type": "thinking", "thinking": ""}]},
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "", "signature": ""},
                {"type": "thinking", "thinking": attack, "signature": "", "cache_control": {"type": "ephemeral"}},
                {"type": "thinking", "thinking": "PRIVATE_SIGNED_HISTORY", "signature": "KEEP_SIGNATURE"},
                {"type": "tool_use", "id": "tool-1", "name": "read_file", "input": {"path": "README.md"}},
            ]},
        ]}
        response = context.request.post(address + "/v1/messages", data=diff_body,
                                        headers={"Authorization": "Bearer PRIVATE_API_KEY"})
        assert response.status == 200
        forwarded = upstream_calls[-1]["body"]
        assert len(forwarded["messages"]) == 2
        assert forwarded["messages"][1]["content"] == [
            {"type": "text", "text": attack, "cache_control": {"type": "ephemeral"}},
            *diff_body["messages"][2]["content"][2:],
        ]
        assert response.json()["content"][0] == {"type": "thinking", "thinking": "", "signature": ""}
        detail = latest_detail()
        request_diff, response_diff = detail["diff"]["request"], detail["diff"]["response"]
        assert len(request_diff) == 3 and len(response_diff) == 1
        assert request_diff[0]["path"] == "/messages/1" and "after" not in request_diff[0]
        assert request_diff[1]["path"] == "/messages/2/content/0" and "after" not in request_diff[1]
        assert request_diff[2]["path"] == "/messages/2/content/1"
        assert request_diff[2]["after_path"] == "/messages/1/content/0"
        assert request_diff[2]["after"] == {"type": "text", "text": attack}
        assert response_diff[0]["path"] == "/content/0"
        assert response_diff[0]["before"] == {"thinking": None, "signature": None}
        assert response_diff[0]["after"] == {"thinking": "", "signature": ""}
        for private in ("PRIVATE_USER_MESSAGE", "PRIVATE_API_KEY", "PRIVATE_SIGNED_HISTORY", "KEEP_SIGNATURE",
                        "PRIVATE_RESPONSE_REASONING", "PRIVATE_RESPONSE_BODY", "cache_control"):
            assert private not in json.dumps(detail["diff"])
        assert "PRIVATE_USER_MESSAGE" in bytes(detail["exchange"]["request"]["body"]).decode()
        assert "PRIVATE_RESPONSE_BODY" in bytes(detail["exchange"]["attempts"][0]["response"]["body"]).decode()
        assert "PRIVATE_API_KEY" not in json.dumps(detail)
        open_diff(detail)
        expect(page.get_by_test_id("diff-request")).to_contain_text(json.dumps(attack, ensure_ascii=False).replace(" ", "\\u0020"))
        expect(page.get_by_test_id("diff-request")).to_contain_text("/messages/2/content/1 → /messages/1/content/0")
        expect(page.get_by_test_id("diff-response")).to_contain_text("− signature: null")
        expect(page.get_by_test_id("diff-response")).to_contain_text('+ signature: ""')
        assert page.evaluate("window.DIFF_XSS === undefined")
        assert page.locator("#diff-content img").count() == 0
        page.wait_for_timeout(1200)  # Status polling must preserve the inspected request.
        expect(page.get_by_role("dialog", name="请求 / 响应差异")).to_be_visible()
        expect(page.get_by_test_id("diff-request")).to_contain_text(json.dumps(attack, ensure_ascii=False).replace(" ", "\\u0020"))
        page.get_by_role("button", name="复制差异", exact=True).click()
        assert json.loads(page.evaluate("navigator.clipboard.readText()")) == detail
        page.get_by_test_id("diff-response").scroll_into_view_if_needed()
        page.screenshot(path=str(output / "diff-json-light.png"))
        page.keyboard.press("Escape")
        passed("diff-json-request-response-and-literal-content")
        (output / "diff-json.json").write_text(json.dumps({"request": diff_body, "forwarded": forwarded,
              "response": response.json(), "detail": detail}, ensure_ascii=False, indent=2), encoding="utf-8")

        streaming_body = {**request_body, "model": "diff-stream", "stream": True}
        response = context.request.post(address + "/v1/messages", data=streaming_body)
        assert response.status == 200
        events = decode_sse(response.body())
        assert events[1]["content_block"]["signature"] == ""
        assert events[6]["content_block"]["thinking"] == ""
        detail = latest_detail()
        assert detail["diff"]["request"] == []
        changes = detail["diff"]["response"]
        assert len(changes) == 2 and detail["call"]["repairs"] == 2
        assert changes[0]["event"] == "SSE #2 · content_block_start · index 0"
        assert changes[0]["before"] == {} and changes[0]["after"] == {"signature": ""}
        assert changes[1]["event"] == "SSE #7 · content_block_start · index 1"
        assert changes[1]["before"] == {"signature": None}
        assert changes[1]["after"] == {"signature": "", "thinking": ""}
        open_diff(detail)
        expect(page.get_by_test_id("diff-response")).to_contain_text("（字段不存在）")
        expect(page.get_by_test_id("diff-response")).to_contain_text("index 0")
        expect(page.get_by_test_id("diff-response")).to_contain_text("index 1")
        page.keyboard.press("Escape")
        # Repeat opening another record; then capture dark and narrow diff views.
        page.get_by_role("button", name="切换主题", exact=True).click()
        open_diff(detail)
        expect(page.get_by_role("status")).not_to_be_visible()
        page.screenshot(path=str(output / "diff-sse-dark.png"))
        page.set_viewport_size({"width": 460, "height": 740})
        assert page.evaluate("document.querySelector('#diff-dialog').scrollWidth <= document.querySelector('#diff-dialog').clientWidth")
        page.screenshot(path=str(output / "diff-sse-narrow.png"))
        (output / "diff-sse.json").write_text(json.dumps({"events": events, "detail": detail},
              ensure_ascii=False, indent=2), encoding="utf-8")
        page.keyboard.press("Escape")
        passed("diff-sse-event-index-and-theme")

        raw_request = '{ \t"model" : "history-gui-whitespace", "stream": false, "messages": [] }\r\n'
        response = context.request.post(address + "/v1/messages", data=raw_request,
                                         headers={"Content-Type": "application/json"})
        assert response.status == 200
        whitespace_detail = latest_detail()
        assert bytes(whitespace_detail["exchange"]["request"]["body"]).decode() == raw_request
        open_diff(whitespace_detail)
        expect(page.get_by_test_id("wire-request-1")).to_contain_text("\\u0020")
        expect(page.get_by_test_id("wire-request-1")).to_contain_text("\\t")
        expect(page.get_by_test_id("wire-request-1")).to_contain_text("\\r\\n")
        expect(page.get_by_test_id("wire-request-1")).to_contain_text("字节")
        expect(page.get_by_test_id("wire-response-1")).to_contain_text("字节")
        page.get_by_test_id("wire-original-request").locator("summary").click()
        expect(page.get_by_test_id("wire-original-request")).to_contain_text("history-gui-whitespace")
        page.screenshot(path=str(output / "diff-byte-whitespace-narrow.png"))
        page.get_by_role("button", name="复制差异", exact=True).click()
        assert json.loads(page.evaluate("navigator.clipboard.readText()")) == whitespace_detail
        page.keyboard.press("Escape")
        page.get_by_label("筛选请求", exact=True).fill(str(whitespace_detail["call"]["id"]))
        expect(page.locator("#activity-list .call")).to_have_count(1)
        expect(page.get_by_test_id("activity-list")).to_contain_text("history-gui-whitespace")
        page.get_by_label("筛选请求", exact=True).fill("")
        response = context.request.post(address + "/v1/messages", data={"model": "history-gui-error"})
        assert response.status == 429
        error_detail = latest_detail()
        page.get_by_label("仅错误", exact=True).check()
        expect(page.get_by_test_id("activity-list")).not_to_contain_text("history-gui-whitespace")
        expect(page.get_by_test_id("activity-list")).to_contain_text("history-gui-error")
        open_diff(error_detail)
        page.get_by_test_id("wire-downstream-response").locator("summary").click()
        expect(page.get_by_test_id("wire-downstream-response")).to_contain_text("GUI\\u0020error")
        expect(page.get_by_test_id("wire-response-1")).to_contain_text("正文逐字节相同")
        page.screenshot(path=str(output / "history-error-detail-narrow.png"))
        page.keyboard.press("Escape")
        page.get_by_label("仅错误", exact=True).uncheck()
        retained = context.request.get(address + "/ui/status").json()["calls"]
        retained_usage = context.request.get(address + "/ui/usage").json()
        passed("byte-diff-whitespace-full-error-filter-and-copy")
        assert script_errors == [], script_errors
        (output / "script-errors.json").write_text(json.dumps(script_errors), encoding="utf-8")
        passed("all-frontend-workflows-without-script-errors")
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
        assert restarted["opencode_base_url"] == opencode_url
        assert restarted["stepfun_key_configured"] and restarted["opencode_key_configured"]
        assert restarted["proxy"] == "" and restarted["stepfun_use_proxy"] and restarted["opencode_use_proxy"]
        assert restarted["calls"] == retained, "request history must survive restart"
        with urllib.request.urlopen(address + "/ui/usage") as response:
            assert json.load(response) == retained_usage
        with urllib.request.urlopen(address + f"/ui/calls/{error_detail['call']['id']}") as response:
            assert json.load(response) == error_detail
        passed("restart-loads-settings-and-complete-request-history")
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
                    for path in sorted(output.iterdir()) if path.is_file() and path.name != "sha256.json" and path.resolve() != binary}
        (output / "sha256.json").write_text(json.dumps(manifest, indent=2), encoding="utf-8")
    print(f"GUI E2E passed: {len(report['cases'])} cases; evidence: {output}")


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--gateway", type=Path, required=True)
    parser.add_argument("--output", type=Path, default=Path("artifacts/gui"))
    args = parser.parse_args()
    run(args.gateway.resolve(), args.output)
