# Rust Implementation Guide: Milestone 9

This guide records implementation decisions for milestone 9. `SPEC.md` remains
the source of product intent. Milestone 9 adds first-class, many-to-many semantic
relationships between blobs and makes integrity inspection and garbage
collection relationship-aware.

The Python implementation is deprecated historical context and is not a
behavior oracle.

## Companion Instructions

- `RUST.instructions.md`
- `RUST-ARCHITECTURE.instructions.md`
- `RUST-TESTING.instructions.md`
- `SQLITE.instructions.md`

## Milestone 9: Semantic Blob Relationships

Add schema version 2 with directed, typed relationships between existing blobs.
Examples include `thumbnail_of` and `transcode_of`.

Do not add a fifth CLI command. Relationship mutation is a deep catalog/domain
API for current and future import/media-processing code. The four-command CLI
remains `import`, `build-tree`, `audit`, and `gc`.

## Resolved Domain Model

A relationship is a directed assertion:

```text
subject_blob --relationship_kind--> object_blob
```

For example, `thumbnail_hash --thumbnail_of--> original_hash`.

Use typed values:

```rust
pub struct RelationshipKind(String);

pub struct BlobRelationship {
    pub subject_hash: BlobHash,
    pub kind: RelationshipKind,
    pub object_hash: BlobHash,
    pub created_at_ms: i64,
}
```

`RelationshipKind` must be non-empty lowercase ASCII snake case, start with a
letter, and have a documented maximum length no greater than 64 bytes. Reject
self-relationships. Direction and kind are part of identity.

The same two blobs may have multiple different relationship kinds. The same
kind may connect many subjects to many objects.

## Schema Version 2

Add a forward-only v1-to-v2 migration and update the current schema validator.
A suitable table is:

```sql
CREATE TABLE blob_relationships (
    subject_hash TEXT NOT NULL REFERENCES blobs(hash) ON DELETE CASCADE,
    relationship_kind TEXT NOT NULL,
    object_hash TEXT NOT NULL REFERENCES blobs(hash) ON DELETE CASCADE,
    created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
    PRIMARY KEY (subject_hash, relationship_kind, object_hash),
    CHECK(subject_hash != object_hash),
    CHECK(
        length(relationship_kind) BETWEEN 1 AND 64
        AND relationship_kind GLOB '[a-z]*'
        AND relationship_kind NOT GLOB '*[^a-z0-9_]*'
        AND relationship_kind NOT GLOB '*__*'
        AND substr(relationship_kind, -1, 1) != '_'
    )
);

CREATE INDEX idx_blob_relationships_object_hash
ON blob_relationships(object_hash);
```

Keep migration SQL deterministic and colocated. Apply it only through the
milestone-7 writer thread. Set `user_version=2` inside the migration transaction.
Do not support down migrations.

Read-only commands must require schema version 2 once this milestone lands and
fail clearly on v1 without mutating it. A writable import or relationship API
open is the supported migration route; document this operationally.

## Relationship Mutation Interface

Extend the catalog writer with typed requests:

```rust
pub fn upsert_relationship(&self, relationship: BlobRelationship) -> Result<()>;
pub fn remove_relationship(&self, key: BlobRelationshipKey) -> Result<RemoveOutcome>;
```

Required behavior:

- both endpoint blob rows must already exist;
- neither endpoint may be marked for deletion when a relationship is created;
- an identical upsert is idempotent and does not rewrite `created_at_ms`;
- removal distinguishes removed from already absent;
- foreign-key and domain violations return contextual typed/domain errors;
- all mutation goes through the single writer and participates in managed
  checkpoint policy;
- batch APIs may be added when they preserve all-or-nothing behavior for one
  supplied batch.

Do not expose raw relationship SQL or accept unchecked hash/kind strings outside
the catalog boundary.

## Reachability Semantics

For garbage collection, relationships are conservative retention edges. A blob
is reachable when:

1. it is directly referenced by any `source_files` row; or
2. it belongs to the undirected connected component of a directly referenced
   blob when relationship direction is ignored for retention.

This means either endpoint retains the other, regardless of semantic direction.
The choice is deliberately non-destructive: generic relationship kinds do not
encode ownership direction strongly enough to delete one endpoint safely.

An unrooted relationship component is unreachable as a whole. Its unmarked
blobs are marked in one GC run and may be swept in a later run. Relationship
rows disappear through `ON DELETE CASCADE` as endpoints are swept.

Compute the transitive closure in bounded, cycle-safe logic. Prefer a recursive
CTE with `UNION` deduplication or an equivalently bounded Rust graph traversal
over validated rows. Do not recurse on the Rust call stack. Preserve the fixed
run-start GC snapshot.

## Import Resurrection And Relationship State

Import resurrection makes its blob live as today. Existing relationships to a
resurrected blob again participate in reachability at the next GC snapshot.
Import does not automatically infer or create relationships from filenames,
extensions, directories, or media metadata.

Creating a relationship to a marked blob fails rather than implicitly
resurrecting it. Callers must establish a live source reference or explicitly
resurrect through an appropriate future workflow before adding the relationship.

## Audit Integration

Extend audit schema and domain checks to cover:

- exact table, column, primary-key, foreign-key, check, and index structure;
- malformed relationship kind storage;
- missing or invalid endpoint hashes;
- self-relationships;
- invalid timestamps;
- foreign-key violations; and
- duplicate logical edges if schema corruption bypasses constraints.

Add structured findings such as `INVALID_RELATIONSHIP_ROW` without emitting raw
unbounded database content. Audit remains read-only and accumulates all
inspectable findings deterministically.

Audit need not treat a relationship cycle, high fan-in, or high fan-out as an
error. Those are valid many-to-many graphs.

## Build-Tree Behavior

Do not materialize relationship-only paths. Browse-tree naming remains derived
from `source_files.relative_path`. Relationship-retained blobs without source
paths remain in the CAS but have no browse-tree entry.

Build-tree must accept schema v2 and continue selecting live source-backed
entries. It must not accidentally produce duplicate links because of joins to
relationships.

## Garbage Collection Behavior

Update GC snapshot and counters so that:

- direct source roots and transitively relationship-retained blobs are
  reachable;
- marked relationship-retained blobs are resurrected;
- unrooted components follow the existing two-run mark/sweep boundary;
- a safety finding in any relationship row blocks all mutation;
- sweep ordering remains deterministic;
- cascaded relationship deletion occurs in the same writer-side transaction as
  deleting a swept blob row; and
- partial-progress recovery remains idempotent.

Consider reporting direct and relationship-retained counts separately in the
domain report, while preserving the existing total `reachable_blobs` meaning.

## Failure And Migration Semantics

- Migration is transactional and leaves a valid v1 database on failure.
- A newer-than-supported schema still fails clearly.
- Audit, build-tree, and dry-run never migrate.
- Relationship mutation failure does not partially apply one requested batch.
- GC never deletes a relationship row independently as a way to make a blob
  collectible.
- Catalog corruption remains a finding/preflight blocker, not an excuse for
  cascade deletion.

## Testing Requirements

Add contract and integration tests proving:

- v1 migrates exactly to v2 through a writable writer open;
- failed migration rolls back and preserves v1;
- read-only commands reject v1 without changing bytes or sidecars;
- relationship-kind validation and self-edge rejection;
- idempotent upsert and exact removal outcomes;
- many subjects/objects and multiple kinds between one pair;
- endpoint foreign keys and cascade deletion behavior;
- cycles terminate and retain their entire component when rooted;
- unrooted cycles are marked together and swept only on a later run;
- a source-rooted blob retains connected subjects and objects transitively;
- removing the final bridge/reference makes the expected component collectible;
- a marked relationship-retained blob is resurrected by GC;
- invalid relationship rows are deterministic audit findings and block GC;
- build-tree output remains source-path based;
- all relationship mutations use the writer and emit checkpoint events under
  the configured policy.

Use behavior APIs or direct read-only SQLite inspection in tests. Do not mutate
production databases through ad hoc test SQL except when deliberately creating
corruption fixtures.

## Recommended Implementation Sequence

1. Add typed relationship kind/key/value types and pure validation tests.
2. Add schema-v2 migration and schema-contract checks.
3. Add writer requests and catalog read APIs.
4. Update read-only version policies and build-tree queries.
5. Implement relationship-aware reachability and GC integration.
6. Extend audit findings and schema inspection.
7. Add migration, graph, audit, GC, and writer integration tests.
8. Update README schema/reachability documentation and run all checks.

## Explicit Non-Goals

- media metadata extraction;
- automatic relationship inference;
- relationship editing through a new CLI command;
- relationship-specific deletion ownership semantics;
- relationship visualization in the browse tree;
- arbitrary user-defined properties on an edge; or
- network/distributed graph storage.

## Definition Of Done

Milestone 9 is complete when schema v2 stores validated many-to-many semantic
edges through the single writer, audit validates them, GC conservatively follows
them transitively, build-tree remains source-path based, migration behavior is
safe and documented, and all repository checks pass.
