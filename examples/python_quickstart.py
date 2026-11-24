#!/usr/bin/env python3
"""Engram Python SDK quickstart: 5-line branch-isolated memory + sleep consolidation."""

import tempfile
from pathlib import Path
import engram

with tempfile.TemporaryDirectory() as tmp:
    db_path = Path(tmp) / "quickstart.db"

    # 5-line quickstart
    mem = engram.open(str(db_path))
    mem.create_branch("exp/wal-tuning", from_rev="main")
    mem.write("/semantic/wal.md", "---\naliases: [\"BUSY_TIMEOUT\"]\n---\nSet busy_timeout=5000.", branch="exp/wal-tuning")
    hits = mem.search("BUSY_TIMEOUT", branch="exp/wal-tuning")
    print("Isolated branch hits:", len(hits), "| main hits:", len(mem.search("BUSY_TIMEOUT", branch="main")))

    # Cold-path sleep consolidation
    mem.write("/episodic/session-1.md", "- Always verify WAL busy_timeout=5000 on startup.", tags=["episodic", "sqlite"])
    mem.commit("record session observation")
    report = mem.consolidate(auto_merge=True, archive_processed=True, delete_branch=True)
    print("Distilled cards:", [c["path"] for c in report["distilled_cards"]])
