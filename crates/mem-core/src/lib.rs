//! Pure domain models, strongly typed BLAKE3 hashes, and validated VFS paths for `mem`.

pub mod error;
pub mod hash;
pub mod node;
pub mod path;
pub mod vcs;

pub use error::{MemError, Result};
pub use hash::{BlobId, CommitId, TreeId};
pub use node::{MemoryMetadata, MemoryNode};
pub use path::{MemoryTier, VirtualPath};
pub use vcs::{
    CommitInfo, DiffEntry, DiffKind, GcStats, MergeConflict, MergeOutcome, MergeStrategy, Revision,
    TreeEntry, TreeEntryType,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn virtual_path_normalization_and_tiers() {
        let p = VirtualPath::new("//core/sub//rules.md/").unwrap();
        assert_eq!(p.as_str(), "/core/sub/rules.md");
        assert_eq!(p.tier(), MemoryTier::Core);
        assert_eq!(p.segments(), vec!["core", "sub", "rules.md"]);
        assert_eq!(p.file_name(), Some("rules.md"));
        assert_eq!(p.parent().unwrap().as_str(), "/core/sub");
        assert!(p.validate_file_path().is_ok());

        let tier_root = VirtualPath::new("/semantic").unwrap();
        assert_eq!(tier_root.tier(), MemoryTier::Semantic);
        assert!(tier_root.parent().is_none());
        assert!(tier_root.validate_file_path().is_err());

        assert_eq!(
            VirtualPath::new("/episodic/2026/session.md")
                .unwrap()
                .tier(),
            MemoryTier::Episodic
        );
        assert_eq!(
            VirtualPath::new("/procedural/debug.md").unwrap().tier(),
            MemoryTier::Procedural
        );
    }

    #[test]
    fn virtual_path_rejects_invalid_and_traversal() {
        for bad in [
            "",
            "core/rules.md",
            "/",
            "/unknown/file.md",
            "/core/../secret.md",
            "/core/./rules.md",
            "/core\\rules.md",
            "/core/bad\0name.md",
            "/core/bad\nname.md",
        ] {
            assert!(
                VirtualPath::new(bad).is_err(),
                "expected invalid path error for {bad:?}"
            );
        }
    }

    #[test]
    fn virtual_path_prefix_and_ancestry_respect_segment_boundaries() {
        let prefix = VirtualPath::new("/semantic/a").unwrap();
        let child = VirtualPath::new("/semantic/a/b.md").unwrap();
        let sibling_prefix = VirtualPath::new("/semantic/abc").unwrap();

        assert!(prefix.starts_with(&prefix));
        assert!(child.starts_with(&prefix));
        assert!(!sibling_prefix.starts_with(&prefix));
        assert!(prefix.is_strict_ancestor_of(&child));
        assert!(!prefix.is_strict_ancestor_of(&prefix));
        assert!(!prefix.is_strict_ancestor_of(&sibling_prefix));
    }

    #[test]
    fn hash_ids_round_trip_hex_and_serde() {
        let blob = BlobId::hash_bytes(b"hello ai memory");
        let tree = TreeId::hash_bytes(b"tree bytes");
        let commit = CommitId::hash_bytes(b"commit bytes");

        assert_eq!(BlobId::from_hex(&blob.to_hex()).unwrap(), blob);
        assert_eq!(TreeId::from_hex(&tree.to_hex()).unwrap(), tree);
        assert_eq!(CommitId::from_hex(&commit.to_hex()).unwrap(), commit);

        let serialized = serde_json::to_string(&commit).unwrap();
        let parsed: CommitId = serde_json::from_str(&serialized).unwrap();
        assert_eq!(parsed, commit);

        assert!(BlobId::from_hex("not-hex").is_err());
        assert!(BlobId::from_hex("abcd").is_err());
    }

    #[test]
    fn metadata_canonical_json_sorts_and_dedups_tags() {
        let meta = MemoryMetadata {
            content_type: "text/markdown".to_string(),
            importance: 0.9,
            tags: vec!["zeta".into(), "alpha".into(), "zeta".into()],
            created_at_ms: 100,
            updated_at_ms: 200,
            ttl_seconds: Some(3600),
        };
        let meta2 = MemoryMetadata {
            tags: vec!["alpha".into(), "zeta".into()],
            ..meta.clone()
        };
        assert_eq!(
            meta.canonical_json().unwrap(),
            meta2.canonical_json().unwrap()
        );
    }
}
