"""Embedded Python wrapper backed by Engram's native Rust PyO3 extension."""

from __future__ import annotations

from pathlib import Path
from typing import Any, Dict, List, Optional, Union

from . import _native


class Engram:
    """Embedded single-file SQLite + Merkle Tree + Wiki-Graph memory engine."""

    def __init__(self, native: _native.NativeEngram) -> None:
        self._inner = native

    @classmethod
    def open(
        cls,
        db_path: Union[str, Path],
        *,
        provider: str = "disabled",
        base_url: Optional[str] = None,
        api_key: Optional[str] = None,
        embed_model: Optional[str] = None,
        llm_model: Optional[str] = None,
    ) -> "Engram":
        native = _native.NativeEngram.open(
            str(db_path),
            provider=provider,
            base_url=base_url,
            api_key=api_key,
            embed_model=embed_model,
            llm_model=llm_model,
        )
        return cls(native)

    @classmethod
    def open_in_memory(
        cls,
        *,
        provider: str = "disabled",
        base_url: Optional[str] = None,
        api_key: Optional[str] = None,
        embed_model: Optional[str] = None,
        llm_model: Optional[str] = None,
    ) -> "Engram":
        native = _native.NativeEngram.open_in_memory(
            provider=provider,
            base_url=base_url,
            api_key=api_key,
            embed_model=embed_model,
            llm_model=llm_model,
        )
        return cls(native)

    def write(
        self,
        path: str,
        content: str,
        *,
        branch: str = "main",
        tags: Optional[List[str]] = None,
        importance: float = 0.5,
    ) -> Dict[str, Any]:
        return self._inner.write(
            path, content, branch=branch, tags=tags, importance=importance
        )

    def read(
        self,
        path: str,
        *,
        branch: str = "main",
        commit_id: Optional[str] = None,
    ) -> Optional[Dict[str, Any]]:
        return self._inner.read(path, branch=branch, commit_id=commit_id)

    def delete(self, path: str, *, branch: str = "main") -> int:
        return self._inner.delete(path, branch=branch)

    def list(
        self,
        *,
        branch: str = "main",
        prefix: Optional[str] = None,
    ) -> List[Dict[str, Any]]:
        return self._inner.list(branch=branch, prefix=prefix)

    def commit(
        self,
        message: str,
        *,
        branch: str = "main",
        author: str = "python-sdk",
    ) -> str:
        return self._inner.commit(message, branch=branch, author=author)

    def log(self, *, branch: str = "main", limit: int = 20) -> List[Dict[str, Any]]:
        return self._inner.log(branch=branch, limit=limit)

    def create_branch(self, name: str, *, from_rev: str = "main") -> None:
        self._inner.create_branch(name, from_rev=from_rev)

    def branch(self, name: str, *, from_rev: str = "main") -> None:
        self.create_branch(name, from_rev=from_rev)

    def branches(self) -> List[Dict[str, Any]]:
        return self._inner.branches()

    def delete_branch(self, name: str) -> None:
        self._inner.delete_branch(name)

    def rewind(self, commit_id: str, *, branch: str = "main") -> None:
        self._inner.rewind(commit_id, branch=branch)

    def diff(self, base: str, target: str) -> List[Dict[str, Any]]:
        return self._inner.diff(base, target)

    def merge(
        self,
        source: str,
        target: str = "main",
        *,
        strategy: str = "auto",
    ) -> Dict[str, Any]:
        return self._inner.merge(source, target=target, strategy=strategy)

    def search(
        self,
        query: str,
        *,
        branch: str = "main",
        prefix: Optional[str] = None,
        top_k: int = 10,
    ) -> List[Dict[str, Any]]:
        return self._inner.search(query, branch=branch, prefix=prefix, top_k=top_k)

    def compile_core(
        self,
        *,
        branch: str = "main",
        max_tokens: int = 2000,
    ) -> Dict[str, Any]:
        return self._inner.compile_core(branch=branch, max_tokens=max_tokens)

    def ingest(
        self,
        text: str,
        *,
        role: str = "dev",
        branch: str = "main",
        draft: bool = False,
        title: Optional[str] = None,
        path: Optional[str] = None,
        tier: Optional[str] = None,
        tags: Optional[List[str]] = None,
        importance: Optional[float] = None,
        commit: bool = True,
        use_llm: bool = False,
    ) -> Dict[str, Any]:
        return self._inner.ingest(
            text,
            role=role,
            branch=branch,
            draft=draft,
            title=title,
            path=path,
            tier=tier,
            tags=tags,
            importance=importance,
            commit=commit,
            use_llm=use_llm,
        )

    def consolidate(
        self,
        *,
        branch: str = "main",
        consolidation_branch: Optional[str] = None,
        prefix: str = "/episodic",
        max_notes: int = 50,
        min_importance: float = 0.0,
        use_llm: bool = False,
        auto_merge: bool = False,
        strategy: str = "auto",
        archive_processed: bool = False,
        delete_branch: bool = False,
        dry_run: bool = False,
    ) -> Dict[str, Any]:
        return self._inner.consolidate(
            branch=branch,
            consolidation_branch=consolidation_branch,
            prefix=prefix,
            max_notes=max_notes,
            min_importance=min_importance,
            use_llm=use_llm,
            auto_merge=auto_merge,
            strategy=strategy,
            archive_processed=archive_processed,
            delete_branch=delete_branch,
            dry_run=dry_run,
        )

    def gc(self) -> Dict[str, Any]:
        return self._inner.gc()


def open(
    db_path: Union[str, Path],
    *,
    provider: str = "disabled",
    base_url: Optional[str] = None,
    api_key: Optional[str] = None,
    embed_model: Optional[str] = None,
    llm_model: Optional[str] = None,
) -> Engram:
    """Open or initialize a local single-file Engram SQLite memory repository."""
    return Engram.open(
        db_path,
        provider=provider,
        base_url=base_url,
        api_key=api_key,
        embed_model=embed_model,
        llm_model=llm_model,
    )


def open_in_memory(
    *,
    provider: str = "disabled",
    base_url: Optional[str] = None,
    api_key: Optional[str] = None,
    embed_model: Optional[str] = None,
    llm_model: Optional[str] = None,
) -> Engram:
    """Open an isolated in-memory Engram repository."""
    return Engram.open_in_memory(
        provider=provider,
        base_url=base_url,
        api_key=api_key,
        embed_model=embed_model,
        llm_model=llm_model,
    )
