"""flatcake_sdk — unified Python SDK for the FlatCake platform.

A thin, typed wrapper over the FlatCake local service. Supports context
managers, connection lifecycle, runtime registration, project identity,
metrics, diagnostics, and cache inspection.

Usage:
    from flatcake_sdk import FlatCake

    with FlatCake() as fc:
        print(fc.status())
        print(fc.kv_cache())
        fc.register_runtime("my-runtime", name="vllm", version="0.6.3")

Requires: FlatCake service running (`flatcake serve`). Stdlib only.
"""

import json
import os
import socket
from typing import Optional
from dataclasses import dataclass, field


class FlatCakeError(Exception):
    def __init__(self, message: str, method: str = ""):
        self.method = method
        super().__init__(f"flatcake {method}: {message}" if method else f"flatcake: {message}")


class ServiceNotRunning(FlatCakeError):
    def __init__(self):
        super().__init__("FlatCake service is not running")


@dataclass
class StatusInfo:
    status: str
    protocol: int
    project: str
    runtimes: int


@dataclass
class CacheInfo:
    project: str
    hbm_hits: int
    dram_hits: int
    ssd_hits: int
    misses: int
    mem_used: int
    runtimes: list


class FlatCake:
    """Connection to a running FlatCake service.

    Context manager:
        with FlatCake() as fc:
            print(fc.status())
    """

    def __init__(self, data_dir: str = "data", timeout_s: float = 10.0):
        self._data_dir = data_dir
        self._timeout = timeout_s
        self._sock: Optional[socket.socket] = None
        self._token: Optional[str] = None
        self._port: Optional[int] = None

    def _ensure_connection(self):
        if self._sock is not None:
            return
        path = os.path.join(self._data_dir, "flatcake-service.json")
        if not os.path.exists(path):
            raise ServiceNotRunning()
        with open(path) as f:
            meta = json.loads(f.read())
        self._port = meta["port"]
        self._token = meta["token"]
        self._sock = socket.create_connection(("127.0.0.1", self._port), timeout=self._timeout)

    def _request(self, body: dict) -> dict:
        self._ensure_connection()
        body["token"] = self._token
        line = json.dumps(body) + "\n"
        self._sock.sendall(line.encode())
        resp_line = b""
        while b"\n" not in resp_line:
            chunk = self._sock.recv(4096)
            if not chunk:
                raise FlatCakeError("service closed connection")
            resp_line += chunk
        resp = json.loads(resp_line.decode())
        if "error" in resp:
            raise FlatCakeError(resp["error"], method=body.get("method", ""))
        return resp

    def close(self):
        if self._sock:
            self._sock.close()
            self._sock = None

    def __enter__(self) -> "FlatCake":
        self._ensure_connection()
        return self

    def __exit__(self, *args):
        self.close()

    # ---- typed operations ----

    def status(self) -> StatusInfo:
        r = self._request({"method": "status"})
        return StatusInfo(
            status=r.get("status", ""), protocol=r.get("protocol", 0),
            project=r.get("project", ""), runtimes=r.get("runtimes", 0),
        )

    def kv_cache(self) -> CacheInfo:
        r = self._request({"method": "kv-cache"})
        return CacheInfo(
            project=r.get("project", ""),
            hbm_hits=r.get("cache", {}).get("hbm_hits", 0),
            dram_hits=r.get("ram", {}).get("hits", 0),
            ssd_hits=r.get("ssd", {}).get("hits", 0),
            misses=r.get("cache", {}).get("misses", 0),
            mem_used=r.get("ram", {}).get("used_bytes", 0),
            runtimes=r.get("runtimes", []),
        )

    def metrics(self) -> dict:
        return self._request({"method": "metrics"})

    def diagnostics(self) -> dict:
        return self._request({"method": "diagnostics"})

    def config(self) -> dict:
        return self._request({"method": "config"})

    def runtimes(self) -> list:
        return self._request({"method": "runtimes"}).get("runtimes", [])

    def register_runtime(
        self, runtime_id: str, project_id: str, pid: int,
        name: str, version: str, capabilities: int,
    ) -> dict:
        return self._request({
            "method": "register", "runtime_id": runtime_id,
            "project_id": project_id, "pid": pid, "name": name,
            "version": version, "capabilities": capabilities,
        })

    def heartbeat(self, runtime_id: str) -> dict:
        return self._request({"method": "heartbeat", "runtime_id": runtime_id})

    def disconnect(self, runtime_id: str) -> dict:
        return self._request({"method": "disconnect", "runtime_id": runtime_id})
