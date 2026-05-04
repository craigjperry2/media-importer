from pathlib import Path
from unittest import mock

from media_importer.catalog import Catalog
from media_importer.cli import _plan_scan_actions  # pyright: ignore[reportPrivateUsage]
from media_importer.executor import Executor
from media_importer.hashing import calculate_hash
from media_importer.planner import Planner


def test_atomic_copy_failure_recovery(workspace: Path) -> None:
    store_dir = workspace / "store"
    db_path = workspace / "db.sqlite"
    source_dir = workspace / "source"

    source_dir.mkdir(parents=True)
    with (source_dir / "test.txt").open("w") as f:
        f.write("hello world")

    catalog = Catalog(db_path)
    planner = Planner(catalog, store_dir)
    actions = _plan_scan_actions(catalog, planner, [source_dir])

    # Mock pathlib.Path.replace to raise an exception
    with mock.patch(
        "pathlib.Path.replace", autospec=True, side_effect=OSError("Disk full")
    ):
        executor = Executor(catalog, store_dir)
        success = executor.execute(actions)

    assert not success

    # The DB shouldn't contain the blob since the copy failed
    file_hash = calculate_hash(source_dir / "test.txt")
    assert catalog.get_blob(file_hash) is None


def test_partial_copy_failure_preserves_successful_rows(workspace: Path) -> None:
    store_dir = workspace / "store"
    db_path = workspace / "db.sqlite"
    source_dir = workspace / "source"

    source_dir.mkdir(parents=True)
    good_path = (source_dir / "good.txt").resolve()
    bad_path = (source_dir / "bad.txt").resolve()
    with good_path.open("w") as f:
        f.write("good")
    with bad_path.open("w") as f:
        f.write("bad")

    catalog = Catalog(db_path)
    planner = Planner(catalog, store_dir)
    actions = _plan_scan_actions(catalog, planner, [source_dir])
    bad_hash = calculate_hash(bad_path)
    good_hash = calculate_hash(good_path)
    original_replace = Path.replace

    def flaky_replace(src, dst):  # pyright: ignore[reportUnknownParameterType, reportMissingParameterType]
        dst_path = dst if isinstance(dst, Path) else Path(dst)  # pyright: ignore[reportUnknownArgumentType]
        if dst_path.name == f"{bad_hash}.txt":
            raise OSError("Disk full")
        original_replace(src, dst_path)  # pyright: ignore[reportUnknownArgumentType]

    with mock.patch("pathlib.Path.replace", autospec=True, side_effect=flaky_replace):
        executor = Executor(catalog, store_dir)
        success = executor.execute(actions)

    assert not success
    assert catalog.get_blob(good_hash) is not None
    assert catalog.get_blob(bad_hash) is None

    rows = catalog.conn.execute(
        "SELECT file_path, file_hash FROM source_files ORDER BY file_path"
    ).fetchall()
    assert [tuple(row) for row in rows] == [(str(good_path), good_hash)]
