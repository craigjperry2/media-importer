import os
import sys
from pathlib import Path
from typing import Any

from media_importer.catalog import Catalog
from media_importer.cli import _plan_scan_actions, main
from media_importer.executor import Executor
from media_importer.hashing import calculate_hash
from media_importer.planner import Planner


def _write_file(path: str, content: str) -> None:
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w") as f:
        f.write(content)


def _browse_entries(root: str) -> list[str]:
    entries: list[str] = []
    if not os.path.exists(root):
        return entries
    for dirpath, _, filenames in os.walk(root):
        for filename in filenames:
            entries.append(os.path.relpath(os.path.join(dirpath, filename), root))
    return sorted(entries)


def _run_browse_scan(
    db_path: str, store_dir: str, browse_root: str, sources: list[str]
) -> Catalog:
    catalog = Catalog(Path(db_path))
    planner = _make_browse_planner(catalog, Path(store_dir), Path(browse_root))
    actions = _plan_scan_actions(catalog, planner, [Path(s) for s in sources])
    executor = _make_browse_executor(catalog, Path(store_dir), Path(browse_root))
    assert executor.execute(actions)
    return catalog


def _make_browse_planner(
    catalog: Catalog, store_dir: Path, browse_root: Path | None
) -> Planner:
    planner_cls: Any = Planner
    return planner_cls(catalog, store_dir, browse_root=browse_root)


def _make_browse_executor(
    catalog: Catalog, store_dir: Path, browse_root: Path | None
) -> Executor:
    executor_cls: Any = Executor
    return executor_cls(catalog, store_dir, browse_root=browse_root)


def test_scan_with_browse_root_creates_source_relative_symlink_tree(workspace):
    store_dir = os.path.join(workspace, "store")
    browse_root = os.path.join(workspace, "browse")
    db_path = os.path.join(workspace, "db.sqlite")
    source_dir = os.path.join(workspace, "source")
    file_path = os.path.join(source_dir, "Movies", "zabba", "zabba.mp4")
    _write_file(file_path, "video-one")

    _run_browse_scan(db_path, store_dir, browse_root, [source_dir])

    file_hash = calculate_hash(file_path)
    canonical_path = os.path.join(store_dir, file_hash[:2], f"{file_hash}.mp4")
    browse_path = os.path.join(browse_root, "Movies", "zabba", "zabba.mp4")

    assert os.path.islink(browse_path)
    assert os.path.samefile(browse_path, canonical_path)


def test_scan_with_relative_source_path_resolves_correctly(workspace, monkeypatch):
    store_dir = os.path.join(workspace, "store")
    browse_root = os.path.join(workspace, "browse")
    db_path = os.path.join(workspace, "db.sqlite")
    source_dir = os.path.join(workspace, "source")
    file_path = os.path.join(source_dir, "Movies", "zabba", "zabba.mp4")
    _write_file(file_path, "video-one")

    monkeypatch.chdir(workspace)
    rel_source_dir = os.path.relpath(source_dir, workspace)

    _run_browse_scan(db_path, store_dir, browse_root, [rel_source_dir])

    browse_path = os.path.join(browse_root, "Movies", "zabba", "zabba.mp4")
    assert os.path.islink(browse_path)


def test_rescan_with_browse_root_is_idempotent(workspace):
    store_dir = os.path.join(workspace, "store")
    browse_root = os.path.join(workspace, "browse")
    db_path = os.path.join(workspace, "db.sqlite")
    source_dir = os.path.join(workspace, "source")
    file_path = os.path.join(source_dir, "Movies", "zabba", "zabba.mp4")
    _write_file(file_path, "video-one")

    catalog = _run_browse_scan(db_path, store_dir, browse_root, [source_dir])
    catalog.close()
    catalog = _run_browse_scan(db_path, store_dir, browse_root, [source_dir])
    catalog.close()

    assert _browse_entries(browse_root) == ["Movies/zabba/zabba.mp4"]


def test_colliding_browse_paths_are_deconflicted_with_hash_suffixes(workspace):
    store_dir = os.path.join(workspace, "store")
    browse_root = os.path.join(workspace, "browse")
    db_path = os.path.join(workspace, "db.sqlite")
    source_one = os.path.join(workspace, "source-one")
    source_two = os.path.join(workspace, "source-two")
    first_path = os.path.join(source_one, "Movies", "zabba", "zabba.mp4")
    second_path = os.path.join(source_two, "Movies", "zabba", "zabba.mp4")
    _write_file(first_path, "first-video")
    _write_file(second_path, "second-video")

    _run_browse_scan(db_path, store_dir, browse_root, [source_one, source_two])

    first_hash = calculate_hash(first_path)
    second_hash = calculate_hash(second_path)
    first_store_path = os.path.join(store_dir, first_hash[:2], f"{first_hash}.mp4")
    second_store_path = os.path.join(store_dir, second_hash[:2], f"{second_hash}.mp4")
    original_browse_path = os.path.join(browse_root, "Movies", "zabba", "zabba.mp4")
    short_hash = second_hash[:7]
    hash_suffixed_browse_path = os.path.join(
        browse_root, "Movies", "zabba", f"zabba_{short_hash}.mp4"
    )

    assert os.path.islink(original_browse_path)
    assert os.path.islink(hash_suffixed_browse_path)
    assert os.path.samefile(original_browse_path, first_store_path)
    assert os.path.samefile(hash_suffixed_browse_path, second_store_path)


def test_colliding_browse_paths_across_separate_scans_are_deconflicted(workspace):
    store_dir = os.path.join(workspace, "store")
    browse_root = os.path.join(workspace, "browse")
    db_path = os.path.join(workspace, "db.sqlite")
    source_one = os.path.join(workspace, "source-one")
    source_two = os.path.join(workspace, "source-two")
    first_path = os.path.join(source_one, "Movies", "zabba", "zabba.mp4")
    second_path = os.path.join(source_two, "Movies", "zabba", "zabba.mp4")
    _write_file(first_path, "first-video")
    _write_file(second_path, "second-video")

    catalog = _run_browse_scan(db_path, store_dir, browse_root, [source_one])
    catalog.close()

    catalog = _run_browse_scan(db_path, store_dir, browse_root, [source_two])
    catalog.close()

    original_browse_path = os.path.join(browse_root, "Movies", "zabba", "zabba.mp4")
    second_hash = calculate_hash(second_path)
    short_hash = second_hash[:7]
    hash_suffixed_browse_path = os.path.join(
        browse_root, "Movies", "zabba", f"zabba_{short_hash}.mp4"
    )

    assert os.path.islink(original_browse_path)
    assert os.path.islink(hash_suffixed_browse_path)


def test_rescan_prunes_missing_browse_symlink_and_empty_directories(workspace):
    store_dir = os.path.join(workspace, "store")
    browse_root = os.path.join(workspace, "browse")
    db_path = os.path.join(workspace, "db.sqlite")
    source_dir = os.path.join(workspace, "source")
    file_path = os.path.join(source_dir, "Albums", "A", "track.mp3")
    _write_file(file_path, "track-one")

    catalog = _run_browse_scan(db_path, store_dir, browse_root, [source_dir])
    catalog.close()

    os.remove(file_path)

    catalog = _run_browse_scan(db_path, store_dir, browse_root, [source_dir])
    catalog.close()

    browse_path = os.path.join(browse_root, "Albums", "A", "track.mp3")
    leaf_dir = os.path.join(browse_root, "Albums", "A")

    assert not os.path.lexists(browse_path)
    assert not os.path.exists(leaf_dir)


def test_verify_store_removes_broken_browse_symlinks(workspace):
    store_dir = os.path.join(workspace, "store")
    browse_root = os.path.join(workspace, "browse")
    db_path = os.path.join(workspace, "db.sqlite")
    source_dir = os.path.join(workspace, "source")
    file_path = os.path.join(source_dir, "Movies", "zabba", "zabba.mp4")
    _write_file(file_path, "video-one")

    catalog = _run_browse_scan(db_path, store_dir, browse_root, [source_dir])

    file_hash = calculate_hash(file_path)
    canonical_path = os.path.join(store_dir, file_hash[:2], f"{file_hash}.mp4")
    browse_path = os.path.join(browse_root, "Movies", "zabba", "zabba.mp4")
    os.remove(canonical_path)

    # Verify store runs without a browse_root argument, relying on DB state
    planner = _make_browse_planner(catalog, Path(store_dir), None)
    actions = planner.plan_verify_store()
    executor = _make_browse_executor(catalog, Path(store_dir), None)

    assert executor.execute(actions)
    assert catalog.get_blob(file_hash) is None
    assert not os.path.lexists(browse_path)

    catalog.close()


def test_scan_dry_run_with_browse_root_leaves_filesystem_untouched(
    workspace, monkeypatch, capsys
):
    store_dir = os.path.join(workspace, "store")
    browse_root = os.path.join(workspace, "browse")
    db_path = os.path.join(workspace, "db.sqlite")
    source_dir = os.path.join(workspace, "source")
    file_path = os.path.join(source_dir, "Movies", "zabba", "zabba.mp4")
    _write_file(file_path, "video-one")

    monkeypatch.setattr(
        sys,
        "argv",
        [
            "media-importer",
            "scan",
            "--store",
            store_dir,
            "--browse-root",
            browse_root,
            "--db",
            db_path,
            "--source",
            source_dir,
            "--dry-run",
        ],
    )

    main()
    captured = capsys.readouterr()

    assert "Dry run:" in captured.out
    assert not os.path.exists(store_dir)
    assert not os.path.exists(browse_root)


def test_scan_dry_run_correctly_plans_collisions_against_existing_catalog(
    workspace, monkeypatch, capsys
):
    store_dir = os.path.join(workspace, "store")
    browse_root = os.path.join(workspace, "browse")
    db_path = os.path.join(workspace, "db.sqlite")
    source_one = os.path.join(workspace, "source-one")
    source_two = os.path.join(workspace, "source-two")
    first_path = os.path.join(source_one, "Movies", "zabba", "zabba.mp4")
    second_path = os.path.join(source_two, "Movies", "zabba", "zabba.mp4")
    _write_file(first_path, "first-video")
    _write_file(second_path, "second-video")

    catalog = _run_browse_scan(db_path, store_dir, browse_root, [source_one])
    catalog.close()

    monkeypatch.setattr(
        sys,
        "argv",
        [
            "media-importer",
            "scan",
            "--store",
            store_dir,
            "--browse-root",
            browse_root,
            "--db",
            db_path,
            "--source",
            source_two,
            "--dry-run",
        ],
    )

    main()
    captured = capsys.readouterr()

    second_hash = calculate_hash(second_path)
    short_hash = second_hash[:7]
    expected_suffixed_name = f"zabba_{short_hash}.mp4"

    assert "Dry run:" in captured.out
    assert expected_suffixed_name in captured.out
