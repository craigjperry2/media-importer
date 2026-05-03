import os

from media_importer.catalog import Catalog
from media_importer.planner import Planner


def test_dry_run_purity(workspace):
    store_dir = os.path.join(workspace, "store")
    db_path = os.path.join(workspace, "db.sqlite")
    source_dir = os.path.join(workspace, "source")

    os.makedirs(source_dir)
    with open(os.path.join(source_dir, "test.txt"), "w") as f:
        f.write("hello world")

    catalog = Catalog(db_path, read_only=True)
    planner = Planner(catalog, store_dir)
    actions = planner.plan_scan([source_dir])

    assert len(actions) > 0
    assert not os.path.exists(db_path)
    assert not os.path.exists(store_dir)
