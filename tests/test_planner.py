from pathlib import Path

from media_importer.catalog import Catalog
from media_importer.cli import _plan_scan_actions  # pyright: ignore[reportPrivateUsage]
from media_importer.executor import Executor
from media_importer.hashing import calculate_hash
from media_importer.models import (
    AddBlobAction,
    CopyFileAction,
    FileObservation,
    InsertObservationAction,
    MarkStaleAction,
)
from media_importer.planner import Planner


def test_plan_observation_is_pure(workspace: Path) -> None:
    store_dir = workspace / "store"
    db_path = workspace / "db.sqlite"

    catalog = Catalog(db_path)
    planner = Planner(catalog, store_dir)
    file_hash = "a" * 128
    obs = FileObservation(
        file_path=Path("/tmp/test.txt"),
        file_name="test.txt",
        file_format=".txt",
        size_bytes=11,
        mtime=10.0,
        file_hash=file_hash,
        last_seen_at=0.0,
    )

    actions = planner.plan_observation(obs, blob_exists=False, now=123.0)

    assert [type(action) for action in actions] == [
        AddBlobAction,
        CopyFileAction,
        InsertObservationAction,
    ]
    add_blob_action = actions[0]
    copy_file_action = actions[1]
    insert_observation_action = actions[2]
    assert isinstance(add_blob_action, AddBlobAction)
    assert isinstance(copy_file_action, CopyFileAction)
    assert isinstance(insert_observation_action, InsertObservationAction)

    expected_store_path = Path(file_hash[:2]) / f"{file_hash}.txt"
    assert add_blob_action.blob.store_path == expected_store_path
    assert copy_file_action.store_path == add_blob_action.blob.store_path
    assert insert_observation_action.observation.last_seen_at == 123.0
    assert obs.last_seen_at == 0.0


def test_idempotency(workspace: Path) -> None:
    store_dir = workspace / "store"
    db_path = workspace / "db.sqlite"
    source_dir = workspace / "source"

    source_dir.mkdir(parents=True)
    with (source_dir / "test.txt").open("w") as f:
        f.write("hello world")

    catalog = Catalog(db_path)
    planner = Planner(catalog, store_dir)

    # First scan
    actions = _plan_scan_actions(catalog, planner, [source_dir])
    assert len(actions) == 3  # AddBlob, CopyFile, InsertObservation

    executor = Executor(catalog, store_dir)
    assert executor.execute(actions)

    # Second scan
    actions2 = _plan_scan_actions(catalog, planner, [source_dir])
    # Should only insert/update observation
    assert len(actions2) == 1
    assert isinstance(actions2[0], InsertObservationAction)


def test_store_drift(workspace: Path) -> None:
    store_dir = workspace / "store"
    db_path = workspace / "db.sqlite"
    source_dir = workspace / "source"

    source_dir.mkdir(parents=True)
    file_path = source_dir / "test.txt"
    with file_path.open("w") as f:
        f.write("hello world")

    catalog = Catalog(db_path)
    planner = Planner(catalog, store_dir)
    actions = _plan_scan_actions(catalog, planner, [source_dir])

    executor = Executor(catalog, store_dir)
    executor.execute(actions)

    # Simulate drift: delete file from store
    file_hash = calculate_hash(file_path)
    shard = file_hash[:2]
    ext = ".txt"
    store_file = store_dir / shard / f"{file_hash}{ext}"
    store_file.unlink()

    # Verify store
    actions = planner.plan_verify_store()
    assert len(actions) == 1
    assert isinstance(actions[0], MarkStaleAction)

    executor.execute(actions)

    assert catalog.get_blob(file_hash) is None
