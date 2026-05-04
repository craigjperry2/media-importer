from pathlib import Path
import sys

from pytest import CaptureFixture, MonkeyPatch

from media_importer.catalog import Catalog
from media_importer.cli import _execute_scan, _plan_scan_actions, main  # pyright: ignore[reportPrivateUsage]
from media_importer.executor import Executor
from media_importer.models import AddBlobAction, CopyFileAction, InsertObservationAction
from media_importer.planner import Planner


def test_scan_dry_run_reports_progress(
    workspace: Path, monkeypatch: MonkeyPatch, capsys: CaptureFixture[str]
) -> None:
    store_dir = workspace / "store"
    db_path = workspace / "db.sqlite"
    source_dir = workspace / "source"

    source_dir.mkdir(parents=True)
    for name in ("one.txt", "two.txt"):
        with (source_dir / name).open("w") as f:
            f.write(name)

    monkeypatch.setattr(
        sys,
        "argv",
        [
            "media-importer",
            "scan",
            "--store",
            str(store_dir),
            "--db",
            str(db_path),
            "--source",
            str(source_dir),
            "--dry-run",
        ],
    )

    main()

    captured = capsys.readouterr()
    assert "Dry run: Planned 6 actions." in captured.out
    assert "Starting dry-run planning across 1 source(s)" in captured.err
    assert f"Scanning source: {source_dir}" in captured.err
    assert (
        "Planning complete: 2 files processed, 2 new files to copy, "
        "2 observations to record, 6 planned actions"
    ) in captured.err
    assert (
        "Dry run complete: 2 files processed, 2 new files to copy, "
        "2 observations to record, 6 planned actions"
    ) in captured.err


def test_scan_reports_execution_progress(
    workspace: Path, monkeypatch: MonkeyPatch, capsys: CaptureFixture[str]
) -> None:
    store_dir = workspace / "store"
    db_path = workspace / "db.sqlite"
    source_dir = workspace / "source"

    source_dir.mkdir(parents=True)
    for index in range(26):
        with (source_dir / f"file-{index}.txt").open("w") as f:
            f.write(f"file {index}")

    monkeypatch.setattr(
        sys,
        "argv",
        [
            "media-importer",
            "scan",
            "--store",
            str(store_dir),
            "--db",
            str(db_path),
            "--source",
            str(source_dir),
        ],
    )

    main()

    captured = capsys.readouterr()
    assert captured.out == ""
    assert "Starting scan across 1 source(s)" in captured.err
    assert f"Scanning source: {source_dir}" in captured.err
    assert "Executing scan batches" in captured.err
    assert "Execution progress: processed 25 file copies" in captured.err
    assert (
        "Planning complete: 26 files processed, 26 new files to copy, "
        "26 observations to record, 78 planned actions"
    ) in captured.err
    assert (
        "Scan complete: 26 files processed, 26 new files to copy, "
        "26 observations to record, 78 planned actions; copied 26 new files"
    ) in captured.err


def test_plan_scan_actions_dedupes_duplicate_content(workspace: Path) -> None:
    store_dir = workspace / "store"
    db_path = workspace / "db.sqlite"
    source_dir = workspace / "source"

    source_dir.mkdir(parents=True)
    with (source_dir / "one.txt").open("w") as f:
        f.write("same data")
    with (source_dir / "two.txt").open("w") as f:
        f.write("same data")

    catalog = Catalog(db_path)
    planner = Planner(catalog, store_dir)

    actions = _plan_scan_actions(catalog, planner, [source_dir])

    assert sum(isinstance(action, AddBlobAction) for action in actions) == 1
    assert sum(isinstance(action, CopyFileAction) for action in actions) == 1
    assert sum(isinstance(action, InsertObservationAction) for action in actions) == 2


def test_execute_scan_batches_dedupe_across_batches(workspace: Path) -> None:
    store_dir = workspace / "store"
    db_path = workspace / "db.sqlite"
    source_dir = workspace / "source"

    source_dir.mkdir(parents=True)
    with (source_dir / "one.txt").open("w") as f:
        f.write("same data")
    with (source_dir / "two.txt").open("w") as f:
        f.write("same data")

    catalog = Catalog(db_path)
    planner = Planner(catalog, store_dir)
    executor = Executor(catalog, store_dir)

    result, _, _ = _execute_scan(
        catalog, planner, executor, [source_dir], max_batch_bytes=1
    )
    assert result.success
    assert len(catalog.get_all_blobs()) == 1

    row = catalog.conn.execute("SELECT COUNT(*) FROM source_files").fetchone()
    assert row is not None
    assert row[0] == 2
