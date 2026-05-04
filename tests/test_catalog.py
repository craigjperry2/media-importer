from pathlib import Path

from media_importer.catalog import Catalog
from media_importer.cli import _plan_scan_actions  # pyright: ignore[reportPrivateUsage]
from media_importer.planner import Planner


def test_dry_run_purity(workspace: Path):
    store_dir = workspace / "store"
    db_path = workspace / "db.sqlite"
    source_dir = workspace / "source"

    source_dir.mkdir(parents=True, exist_ok=True)
    with (source_dir / "test.txt").open("w") as f:
        f.write("hello world")

    catalog = Catalog(db_path, read_only=True)
    planner = Planner(catalog, store_dir)
    actions = _plan_scan_actions(catalog, planner, [source_dir])

    assert len(actions) > 0
    assert not db_path.exists()
    assert not store_dir.exists()
