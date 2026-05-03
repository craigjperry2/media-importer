import os
from unittest import mock

from media_importer.catalog import Catalog
from media_importer.executor import Executor
from media_importer.hashing import calculate_hash
from media_importer.planner import Planner


def test_atomic_copy_failure_recovery(workspace):
    store_dir = os.path.join(workspace, "store")
    db_path = os.path.join(workspace, "db.sqlite")
    source_dir = os.path.join(workspace, "source")

    os.makedirs(source_dir)
    with open(os.path.join(source_dir, "test.txt"), "w") as f:
        f.write("hello world")

    catalog = Catalog(db_path)
    planner = Planner(catalog, store_dir)
    actions = planner.plan_scan([source_dir])

    # Mock os.replace to raise an exception
    with mock.patch("os.replace", side_effect=OSError("Disk full")):
        executor = Executor(catalog, store_dir)
        success = executor.execute(actions)

    assert not success

    # The DB shouldn't contain the blob since the copy failed
    file_hash = calculate_hash(os.path.join(source_dir, "test.txt"))
    assert catalog.get_blob(file_hash) is None
