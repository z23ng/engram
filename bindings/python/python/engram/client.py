"""Zero-dependency HTTP REST & SSE client for connecting to `mem-daemon`."""

from __future__ import annotations

import json
import urllib.error
import urllib.parse
import urllib.request
from typing import Any, Dict, Iterator, List, Optional


class EngramError(RuntimeError):
    """Error returned by a remote `mem-daemon` HTTP endpoint."""

    def __init__(
        self,
        message: str,
        status_code: Optional[int] = None,
        code: Optional[str] = None,
    ) -> None:
        super().__init__(message)
        self.status_code = status_code
        self.code = code


class EngramClient:
    """HTTP REST + Server-Sent Events client for `mem-daemon`."""

    def __init__(
        self,
        base_url: str = "http://127.0.0.1:7700",
        *,
        token: Optional[str] = None,
        timeout: float = 30.0,
    ) -> None:
        self.base_url = base_url.rstrip("/")
        self.token = token
        self.timeout = timeout

    def _request(
        self,
        method: str,
        path: str,
        *,
        query: Optional[Dict[str, Any]] = None,
        body: Optional[Dict[str, Any]] = None,
    ) -> Any:
        url = f"{self.base_url}{path}"
        if query:
            clean = {k: str(v) for k, v in query.items() if v is not None}
            if clean:
                url = f"{url}?{urllib.parse.urlencode(clean)}"

        headers: Dict[str, str] = {"Accept": "application/json"}
        if self.token:
            headers["Authorization"] = f"Bearer {self.token}"

        data: Optional[bytes] = None
        if body is not None:
            headers["Content-Type"] = "application/json"
            data = json.dumps(body).encode("utf-8")

        req = urllib.request.Request(url, data=data, headers=headers, method=method)
        try:
            with urllib.request.urlopen(req, timeout=self.timeout) as resp:
                raw = resp.read().decode("utf-8")
                return json.loads(raw) if raw else {}
        except urllib.error.HTTPError as exc:
            raw_err = exc.read().decode("utf-8", errors="replace")
            try:
                parsed = json.loads(raw_err)
                raise EngramError(
                    parsed.get("error", raw_err),
                    status_code=exc.code,
                    code=parsed.get("code"),
                ) from exc
            except json.JSONDecodeError:
                raise EngramError(raw_err or str(exc), status_code=exc.code) from exc

    def health(self) -> Dict[str, Any]:
        return self._request("GET", "/health")

    def status(self, *, branch: str = "main") -> Dict[str, Any]:
        return self._request("GET", "/v1/status", query={"branch": branch})

    def list(
        self, *, branch: str = "main", prefix: Optional[str] = None
    ) -> List[Dict[str, Any]]:
        return self._request(
            "GET", "/v1/nodes", query={"branch": branch, "prefix": prefix}
        )

    def read(
        self,
        path: str,
        *,
        branch: str = "main",
        commit_id: Optional[str] = None,
    ) -> Dict[str, Any]:
        return self._request(
            "GET",
            "/v1/node",
            query={"path": path, "branch": branch, "commit_id": commit_id},
        )

    def write(
        self,
        path: str,
        content: str,
        *,
        branch: str = "main",
        tags: Optional[List[str]] = None,
        importance: Optional[float] = None,
    ) -> Dict[str, Any]:
        payload: Dict[str, Any] = {"path": path, "content": content, "branch": branch}
        if tags is not None:
            payload["tags"] = tags
        if importance is not None:
            payload["importance"] = importance
        return self._request("PUT", "/v1/node", body=payload)

    def delete(self, path: str, *, branch: str = "main") -> Dict[str, Any]:
        return self._request("DELETE", "/v1/node", query={"path": path, "branch": branch})

    def commit(
        self,
        message: str,
        *,
        branch: str = "main",
        author: str = "python-client",
    ) -> Dict[str, Any]:
        return self._request(
            "POST",
            "/v1/commits",
            body={"message": message, "branch": branch, "author": author},
        )

    def log(self, *, branch: str = "main", limit: int = 20) -> List[Dict[str, Any]]:
        return self._request(
            "GET", "/v1/commits", query={"branch": branch, "limit": limit}
        )

    def branches(self) -> List[Dict[str, Any]]:
        return self._request("GET", "/v1/branches")

    def create_branch(self, name: str, *, from_rev: str = "main") -> Dict[str, Any]:
        return self._request(
            "POST", "/v1/branches", body={"name": name, "from": from_rev}
        )

    def delete_branch(self, name: str) -> Dict[str, Any]:
        return self._request("DELETE", "/v1/branches", query={"name": name})

    def rewind(self, commit_id: str, *, branch: str = "main") -> Dict[str, Any]:
        return self._request(
            "POST", "/v1/rewind", body={"commit_id": commit_id, "branch": branch}
        )

    def diff(self, base: str, target: str) -> List[Dict[str, Any]]:
        return self._request("GET", "/v1/diff", query={"base": base, "target": target})

    def merge(
        self,
        source: str,
        target: str = "main",
        *,
        strategy: str = "auto",
    ) -> Dict[str, Any]:
        return self._request(
            "POST",
            "/v1/merge",
            body={"source": source, "target": target, "strategy": strategy},
        )

    def gc(self) -> Dict[str, Any]:
        return self._request("POST", "/v1/gc")

    def search(
        self,
        query: str,
        *,
        branch: str = "main",
        prefix: Optional[str] = None,
        top_k: int = 10,
    ) -> List[Dict[str, Any]]:
        payload: Dict[str, Any] = {"query": query, "branch": branch, "top_k": top_k}
        if prefix is not None:
            payload["prefix"] = prefix
        return self._request("POST", "/v1/search", body=payload)

    def compile_core(
        self, *, branch: str = "main", max_tokens: int = 2000
    ) -> Dict[str, Any]:
        return self._request(
            "POST", "/v1/compile", body={"branch": branch, "max_tokens": max_tokens}
        )

    def ingest(
        self,
        text: str,
        *,
        role: str = "dev",
        branch: str = "main",
        draft: bool = False,
        title: Optional[str] = None,
        tags: Optional[List[str]] = None,
        commit: bool = True,
        use_llm: bool = False,
    ) -> Dict[str, Any]:
        payload: Dict[str, Any] = {
            "text": text,
            "role": role,
            "branch": branch,
            "draft": draft,
            "commit": commit,
            "use_llm": use_llm,
        }
        if title is not None:
            payload["title"] = title
        if tags is not None:
            payload["tags"] = tags
        return self._request("POST", "/v1/ingest", body=payload)

    def consolidate(
        self,
        *,
        branch: str = "main",
        auto_merge: bool = False,
        strategy: str = "auto",
        archive_processed: bool = False,
        delete_branch: bool = False,
        dry_run: bool = False,
    ) -> Dict[str, Any]:
        return self._request(
            "POST",
            "/v1/consolidate",
            body={
                "branch": branch,
                "auto_merge": auto_merge,
                "strategy": strategy,
                "archive_processed": archive_processed,
                "delete_branch": delete_branch,
                "dry_run": dry_run,
            },
        )

    def stream_events(self, *, branch: Optional[str] = None) -> Iterator[Dict[str, Any]]:
        """Yields live SSE events emitted by `mem-daemon`."""
        query: Dict[str, str] = {}
        if branch:
            query["branch"] = branch
        url = f"{self.base_url}/v1/events"
        if query:
            url = f"{url}?{urllib.parse.urlencode(query)}"
        headers: Dict[str, str] = {"Accept": "text/event-stream"}
        if self.token:
            headers["Authorization"] = f"Bearer {self.token}"

        req = urllib.request.Request(url, headers=headers, method="GET")
        with urllib.request.urlopen(req, timeout=self.timeout) as resp:
            event_name: Optional[str] = None
            data_lines: List[str] = []
            for raw_line in resp:
                line = raw_line.decode("utf-8", errors="replace").rstrip("\r\n")
                if line.startswith(":"):
                    continue
                if line == "":
                    if data_lines:
                        raw_data = "\n".join(data_lines)
                        try:
                            parsed = json.loads(raw_data)
                        except json.JSONDecodeError:
                            parsed = {"raw": raw_data}
                        if event_name and "event" not in parsed:
                            parsed["event"] = event_name
                        yield parsed
                        event_name = None
                        data_lines.clear()
                elif line.startswith("event:"):
                    event_name = line[6:].strip()
                elif line.startswith("data:"):
                    data_lines.append(line[5:].strip())
