import os
from unittest import mock

from media_importer.catalog import Catalog
from media_importer.cli import _plan_scan_actions
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
    actions = _plan_scan_actions(catalog, planner, [source_dir])

    # Mock os.replace to raise an exception
    with mock.patch("os.replace", side_effect=OSError("Disk full")):
        executor = Executor(catalog, store_dir)
        success = executor.execute(actions)

    assert not success

    # The DB shouldn't contain the blob since the copy failed
    file_hash = calculate_hash(os.path.join(source_dir, "test.txt"))
    assert catalog.get_blob(file_hash) is None


def test_partial_copy_failure_preserves_successful_rows(workspace):
    store_dir = os.path.join(workspace, "store")
    db_path = os.path.join(workspace, "db.sqlite")
    source_dir = os.path.join(workspace, "source")

    os.makedirs(source_dir)
    good_path = os.path.realpath(os.path.join(source_dir, "good.txt"))
    bad_path = os.path.realpath(os.path.join(source_dir, "bad.txt"))
    with open(good_path, "w") as f:
        f.write("good")
    with open(bad_path, "w") as f:
        f.write("bad")

    catalog = Catalog(db_path)
    planner = Planner(catalog, store_dir)
    actions = _plan_scan_actions(catalog, planner, [source_dir])
    bad_hash = calculate_hash(bad_path)
    good_hash = calculate_hash(good_path)
    original_replace = os.replace

    def flaky_replace(src, dst):
        if dst.endswith(f"{bad_hash}.txt"):
            raise OSError("Disk full")
        original_replace(src, dst)

    with mock.patch("os.replace", side_effect=flaky_replace):
        executor = Executor(catalog, store_dir)
        success = executor.execute(actions)

    assert not success
    assert catalog.get_blob(good_hash) is not None
    assert catalog.get_blob(bad_hash) is None

    rows = catalog.conn.execute(
        "SELECT file_path, file_hash FROM source_files ORDER BY file_path"
    ).fetchall()
    assert [tuple(row) for row in rows] == [(good_path, good_hash)]
