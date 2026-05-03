import os

from media_importer.catalog import Catalog
from media_importer.executor import Executor
from media_importer.hashing import calculate_hash
from media_importer.models import InsertObservationAction, MarkStaleAction
from media_importer.planner import Planner


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
    actions = planner.plan_scan([source_dir])
    assert len(actions) == 3  # AddBlob, CopyFile, InsertObservation

    executor = Executor(catalog, store_dir)
    assert executor.execute(actions)

    # Second scan
    actions2 = planner.plan_scan([source_dir])
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
    actions = planner.plan_scan([source_dir])

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
