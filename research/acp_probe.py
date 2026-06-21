#!/usr/bin/env python3
"""ACP/MCP 探针:拉起 agent 子进程,走 NDJSON JSON-RPC 握手,抓真实报文。

仅用于调研(docs/ACP_RESEARCH.md 的证据来源),不是 ccwatch 运行时代码。

用法:
    python3 acp_probe.py claude    # claude --acp
    python3 acp_probe.py gemini    # gemini --acp
    python3 acp_probe.py codex     # codex mcp-server (MCP, 非 ACP)

行为:作为 client 拉起 agent,initialize -> session/new -> session/prompt,
把双向每一条报文带方向(>>> 发 / <<< 收)和时间戳打到 stdout。
对 agent 反向发来的 client 方法(fs/read_text_file、session/request_permission 等)
自动应答,避免 agent 卡死。
"""
import json
import os
import subprocess
import sys
import threading
import time

START = time.time()


def ts() -> str:
    return f"{time.time() - START:6.2f}s"


def log(direction: str, obj) -> None:
    """打一条报文。direction: '>>>' 发给 agent, '<<<' agent 发来, '###' 元信息。"""
    if isinstance(obj, (dict, list)):
        s = json.dumps(obj, ensure_ascii=False)
    else:
        s = str(obj)
    print(f"[{ts()}] {direction} {s}", flush=True)


class AcpClient:
    def __init__(self, cmd):
        self.cmd = cmd
        self.proc = None
        self._next_id = 0
        self._lock = threading.Lock()

    def start(self):
        log("###", f"spawn: {' '.join(self.cmd)}")
        self.proc = subprocess.Popen(
            self.cmd,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            bufsize=0,
        )
        threading.Thread(target=self._drain_stderr, daemon=True).start()

    def _drain_stderr(self):
        for line in iter(self.proc.stderr.readline, b""):
            txt = line.decode("utf-8", "replace").rstrip()
            if txt:
                log("ERR", txt)

    def new_id(self):
        with self._lock:
            self._next_id += 1
            return self._next_id

    def send(self, obj):
        log(">>>", obj)
        data = (json.dumps(obj, ensure_ascii=False) + "\n").encode("utf-8")
        self.proc.stdin.write(data)
        self.proc.stdin.flush()

    def request(self, method, params, req_id=None):
        if req_id is None:
            req_id = self.new_id()
        self.send({"jsonrpc": "2.0", "id": req_id, "method": method, "params": params})
        return req_id

    def notify(self, method, params):
        self.send({"jsonrpc": "2.0", "method": method, "params": params})

    def respond(self, req_id, result):
        self.send({"jsonrpc": "2.0", "id": req_id, "result": result})

    def read_loop(self, on_message, deadline):
        """阻塞读 agent 的 stdout(NDJSON,每行一条 JSON-RPC)。"""
        f = self.proc.stdout
        while time.time() < deadline:
            line = f.readline()
            if not line:
                log("###", "agent closed stdout (EOF)")
                return
            txt = line.decode("utf-8", "replace").strip()
            if not txt:
                continue
            try:
                obj = json.loads(txt)
            except json.JSONDecodeError:
                log("<<<RAW", txt)
                continue
            log("<<<", obj)
            on_message(obj)

    def stop(self):
        if self.proc and self.proc.poll() is None:
            self.proc.terminate()
            try:
                self.proc.wait(timeout=3)
            except subprocess.TimeoutExpired:
                self.proc.kill()


# ---- 各家 agent 的驱动 ----

CLIENT_CAPS = {
    "fs": {"readTextFile": True, "writeTextFile": True},
    "terminal": True,
}


def auto_reply_client_call(client: AcpClient, obj):
    """agent 反向调用 client 方法时自动应答,避免卡死。返回 True 表示已处理。"""
    if "method" not in obj or "id" not in obj:
        return False
    method = obj["method"]
    rid = obj["id"]
    params = obj.get("params", {})
    if method == "fs/read_text_file":
        path = params.get("path", "")
        try:
            content = open(path, "r", encoding="utf-8").read()
        except Exception as e:
            content = f"(probe could not read: {e})"
        client.respond(rid, {"content": content})
    elif method == "fs/write_text_file":
        client.respond(rid, {})
    elif method == "session/request_permission":
        # 关键:这就是 "waiting for permission" 信号。探针默认拒绝(reject_once),
        # 让我们能观察到完整的 turn 结束,而不真的执行工具。
        opts = params.get("options", [])
        log("###", f"PERMISSION REQUESTED, options={[o.get('optionId') for o in opts]}")
        # 选第一个 reject 选项,没有就 cancelled
        outcome = {"outcome": "cancelled"}
        for o in opts:
            if o.get("kind", "").startswith("reject"):
                outcome = {"outcome": "selected", "optionId": o["optionId"]}
                break
        client.respond(rid, outcome)
    else:
        # 未知 client 方法,回个空 result
        client.respond(rid, {})
    return True


def drive_acp(agent: str, prompt: str, timeout: float):
    if agent == "claude":
        cmd = ["claude", "--acp"]
    elif agent == "gemini":
        cmd = ["gemini", "--acp"]
    else:
        raise SystemExit(f"unknown acp agent {agent}")

    client = AcpClient(cmd)
    client.start()
    deadline = time.time() + timeout
    state = {"session_id": None, "init_done": False, "prompt_sent": False}

    def on_message(obj):
        # agent 反向请求
        if auto_reply_client_call(client, obj):
            return
        # initialize 的响应
        if obj.get("id") == 1 and "result" in obj and not state["init_done"]:
            state["init_done"] = True
            log("###", "initialize OK -> session/new")
            client.request("session/new", {
                "cwd": os.getcwd(),
                "mcpServers": [],
            }, req_id=2)
        # session/new 响应
        elif obj.get("id") == 2 and "result" in obj:
            sid = obj["result"].get("sessionId")
            state["session_id"] = sid
            log("###", f"session created: {sid} -> session/prompt")
            client.request("session/prompt", {
                "sessionId": sid,
                "prompt": [{"type": "text", "text": prompt}],
            }, req_id=3)
            state["prompt_sent"] = True
        # session/prompt 响应(turn 结束)
        elif obj.get("id") == 3 and "result" in obj:
            log("###", f"TURN ENDED, stopReason={obj['result'].get('stopReason')}")

    # initialize 先发
    client.request("initialize", {
        "protocolVersion": 1,
        "clientCapabilities": CLIENT_CAPS,
        "clientInfo": {"name": "ccwatch-probe", "version": "0.0.1"},
    }, req_id=1)

    try:
        client.read_loop(on_message, deadline)
    finally:
        log("###", "stopping agent")
        client.stop()


def drive_mcp(prompt: str, timeout: float):
    """Codex MCP server:MCP initialize -> tools/list -> tools/call codex。"""
    cmd = ["codex", "mcp-server"]
    client = AcpClient(cmd)
    client.start()
    deadline = time.time() + timeout
    state = {}

    def on_message(obj):
        if auto_reply_client_call(client, obj):
            return
        if obj.get("id") == 1 and "result" in obj:
            log("###", "MCP initialize OK -> notifications/initialized + tools/list")
            client.notify("notifications/initialized", {})
            client.request("tools/list", {}, req_id=2)
        elif obj.get("id") == 2 and "result" in obj:
            tools = [t.get("name") for t in obj["result"].get("tools", [])]
            log("###", f"MCP tools: {tools}")
            # 调 codex 工具(若存在),看 notifications 状态流
            tool = "codex" if "codex" in tools else (tools[0] if tools else None)
            if tool:
                log("###", f"calling tool {tool}")
                client.request("tools/call", {
                    "name": tool,
                    "arguments": {"prompt": prompt},
                }, req_id=3)
        elif obj.get("id") == 3 and "result" in obj:
            log("###", "tools/call returned (turn done)")

    client.request("initialize", {
        "protocolVersion": "2025-06-18",
        "capabilities": {},
        "clientInfo": {"name": "ccwatch-probe", "version": "0.0.1"},
    }, req_id=1)

    try:
        client.read_loop(on_message, deadline)
    finally:
        log("###", "stopping agent")
        client.stop()


if __name__ == "__main__":
    agent = sys.argv[1] if len(sys.argv) > 1 else "gemini"
    prompt = sys.argv[2] if len(sys.argv) > 2 else "Reply with exactly the word: pong"
    timeout = float(sys.argv[3]) if len(sys.argv) > 3 else 60.0
    log("###", f"=== probe agent={agent} timeout={timeout}s ===")
    if agent == "codex":
        drive_mcp(prompt, timeout)
    else:
        drive_acp(agent, prompt, timeout)
    log("###", "=== probe done ===")
