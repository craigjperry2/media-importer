import os

from media_importer.catalog import Catalog
from media_importer.cli import _plan_scan_actions
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


def test_plan_observation_is_pure(workspace):
    store_dir = os.path.join(workspace, "store")
    db_path = os.path.join(workspace, "db.sqlite")

    catalog = Catalog(db_path)
    planner = Planner(catalog, store_dir)
    file_hash = "a" * 128
    obs = FileObservation(
        file_path="/tmp/test.txt",
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
    assert actions[0].blob.store_path == os.path.join(file_hash[:2], f"{file_hash}.txt")
    assert actions[1].store_path == actions[0].blob.store_path
    assert actions[2].observation.last_seen_at == 123.0
    assert obs.last_seen_at == 0.0


def test_idempotency(workspace):
    store_dir = os.path.join(workspace, "store")
    db_path = os.path.join(workspace, "db.sqlite")
    source_dir = os.path.join(workspace, "source")

    os.makedirs(source_dir)
    with open(os.path.join(source_dir, "test.txt"), "w") as f:
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


def test_store_drift(workspace):
    store_dir = os.path.join(workspace, "store")
    db_path = os.path.join(workspace, "db.sqlite")
    source_dir = os.path.join(workspace, "source")

    os.makedirs(source_dir)
    file_path = os.path.join(source_dir, "test.txt")
    with open(file_path, "w") as f:
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
    store_file = os.path.join(store_dir, shard, f"{file_hash}{ext}")
    os.remove(store_file)

    # Verify store
    actions = planner.plan_verify_store()
    assert len(actions) == 1
    assert isinstance(actions[0], MarkStaleAction)

    executor.execute(actions)

    assert catalog.get_blob(file_hash) is None
