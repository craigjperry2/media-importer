import os

from media_importer.catalog import Catalog
from media_importer.cli import _execute_scan, _plan_scan_actions
from media_importer.executor import Executor
from media_importer.models import AddBlobAction, CopyFileAction, InsertObservationAction
from media_importer.planner import Planner


def test_plan_scan_actions_dedupes_duplicate_content(workspace):
    store_dir = os.path.join(workspace, "store")
    db_path = os.path.join(workspace, "db.sqlite")
    source_dir = os.path.join(workspace, "source")

    os.makedirs(source_dir)
    with open(os.path.join(source_dir, "one.txt"), "w") as f:
        f.write("same data")
    with open(os.path.join(source_dir, "two.txt"), "w") as f:
        f.write("same data")

    catalog = Catalog(db_path)
    planner = Planner(catalog, store_dir)

    actions = _plan_scan_actions(catalog, planner, [source_dir])

    assert sum(isinstance(action, AddBlobAction) for action in actions) == 1
    assert sum(isinstance(action, CopyFileAction) for action in actions) == 1
    assert sum(isinstance(action, InsertObservationAction) for action in actions) == 2


def test_execute_scan_batches_dedupe_across_batches(workspace):
    store_dir = os.path.join(workspace, "store")
    db_path = os.path.join(workspace, "db.sqlite")
    source_dir = os.path.join(workspace, "source")

    os.makedirs(source_dir)
    with open(os.path.join(source_dir, "one.txt"), "w") as f:
        f.write("same data")
    with open(os.path.join(source_dir, "two.txt"), "w") as f:
        f.write("same data")

    catalog = Catalog(db_path)
    planner = Planner(catalog, store_dir)
    executor = Executor(catalog, store_dir)

    assert _execute_scan(catalog, planner, executor, [source_dir], max_batch_bytes=1)
    assert len(catalog.get_all_blobs()) == 1

    row = catalog.conn.execute("SELECT COUNT(*) FROM source_files").fetchone()
    assert row is not None
    assert row[0] == 2
