import os
import socket
import subprocess
import tempfile
import threading
import time
import unittest
from pathlib import Path

import engram


def _free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.bind(("127.0.0.1", 0))
        return int(s.getsockname()[1])


class NativeEngramTests(unittest.TestCase):
    def test_five_line_quickstart_and_branch_isolation(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            db_path = Path(tmp) / "quickstart.db"

            mem = engram.open(str(db_path))
            mem.create_branch("exp/test", from_rev="main")
            mem.write("/semantic/tip.md", "---\naliases: [\"WAL_TIP\"]\n---\nUse SQLite WAL mode.", branch="exp/test")
            hits = mem.search("WAL_TIP", branch="exp/test")
            self.assertEqual(len(hits), 1)

            # Main branch remains completely isolated from exp/test.
            self.assertEqual(mem.search("WAL_TIP", branch="main"), [])

    def test_vcs_rewind_merge_ingest_consolidate_and_gc(self) -> None:
        mem = engram.open_in_memory()
        mem.write("/core/rules.md", "Never leak unmerged branches.", importance=1.0)
        c1 = mem.commit("seed core")

        compiled = mem.compile_core(max_tokens=500)
        self.assertIn("Never leak unmerged branches.", compiled["rendered"])

        # Write a bad node then rewind cleanly back to c1.
        mem.write("/semantic/hallucination.md", "Bad fact")
        self.assertIsNotNone(mem.read("/semantic/hallucination.md"))
        mem.rewind(c1)
        self.assertIsNone(mem.read("/semantic/hallucination.md"))

        # Ingest draft + consolidate isolated branch + merge.
        mem.write("/episodic/session.md", "- Always configure PRAGMA busy_timeout = 5000.", tags=["episodic", "sqlite"])
        mem.commit("seed episode")

        report = mem.consolidate(auto_merge=True, archive_processed=True, delete_branch=True)
        self.assertEqual(report["episodes_scanned"], 1)
        self.assertTrue(report["consolidation_branch_deleted"])
        self.assertGreaterEqual(len(report["distilled_cards"]), 1)

        gc_stats = mem.gc()
        self.assertIn("removed_blobs", gc_stats)


class DaemonClientTests(unittest.TestCase):
    def test_http_client_and_sse_events(self) -> None:
        daemon_bin = os.environ.get("MEM_DAEMON_BIN")
        if not daemon_bin or not Path(daemon_bin).exists():
            repo_root = Path(__file__).resolve().parents[3]
            candidate = repo_root / "target" / "release" / "mem-daemon"
            if not candidate.exists():
                candidate = repo_root / "target" / "debug" / "mem-daemon"
            daemon_bin = str(candidate)
        if not Path(daemon_bin).exists():
            self.skipTest("mem-daemon binary not built")

        with tempfile.TemporaryDirectory() as tmp:
            db_path = Path(tmp) / "daemon_sdk.db"
            port = _free_port()
            token = "py-sdk-token"
            proc = subprocess.Popen(
                [
                    daemon_bin,
                    "--host",
                    "127.0.0.1",
                    "--port",
                    str(port),
                    "--db",
                    str(db_path),
                    "--provider",
                    "disabled",
                    "--token",
                    token,
                ],
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
            )
            try:
                client = engram.EngramClient(f"http://127.0.0.1:{port}", token=token)
                deadline = time.time() + 8.0
                while time.time() < deadline:
                    try:
                        if client.health()["status"] == "ok":
                            break
                    except Exception:
                        time.sleep(0.1)
                else:
                    self.fail("mem-daemon failed to become healthy")

                # Verify unauthorized client fails with 401.
                unauth = engram.EngramClient(f"http://127.0.0.1:{port}")
                with self.assertRaises(engram.EngramError) as ctx:
                    unauth.status()
                self.assertEqual(ctx.exception.status_code, 401)

                received = []
                stop_evt = threading.Event()

                def consume_sse() -> None:
                    for ev in client.stream_events(branch="main"):
                        received.append(ev)
                        if stop_evt.is_set() or len(received) >= 1:
                            break

                t = threading.Thread(target=consume_sse, daemon=True)
                t.start()
                time.sleep(0.25)

                client.write("/core/goal.md", "Ship Engram Python SDK", importance=1.0)
                client.commit("python sdk commit")
                t.join(timeout=3.0)
                stop_evt.set()
                self.assertGreaterEqual(len(received), 1)
                self.assertEqual(received[0]["branch"], "main")

                hits = client.search("Engram Python SDK")
                self.assertEqual(len(hits), 1)
                self.assertEqual(hits[0]["path"], "/core/goal.md")
            finally:
                proc.terminate()
                proc.wait(timeout=5)


if __name__ == "__main__":
    unittest.main()
