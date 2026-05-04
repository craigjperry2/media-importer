# Agent Directives: Media Importer

## 🚀 Commands
Execute these from the project root after entering the dev environment (`direnv allow` or `nix develop`).
* **Test:** `pytest`
* **Run CLI:** `media-importer` (e.g., `media-importer scan --store store-root/ --db db.sqlite --source src/`)
* **Dry Run:** Add `--dry-run` to the CLI to preview actions without side-effects.

## 🧱 Boundaries (Dos and Don'ts)
* **DO keep Planner Pure:** `planner.py` must remain purely functional. It compares states and yields plans. No side-effects (no DB writes, no disk writes).
* **DO isolate IO to Executor:** All file copies and DB transactions happen exclusively in `executor.py`.
* **DO use git "Conventional Commits":** git commit messages follow conventional commits `feat: behaviour added, changed or removed` or `refactor: why thing needed to change` 
* **DON'T use ORMs:** Stick to raw `sqlite3` in `catalog.py`. Keep schemas deterministic.
* **DON'T disable typing/linting:** Maintain full type safety and adhere to the existing conventions.

## 📂 Project Structure
* `cli.py`: Argparse entrypoint.
* `models.py`: Domain dataclasses.
* `hashing.py`: Cryptographic content hashing.
* `scanner.py`: Filesystem traversal (ignores symlinks).
* `catalog.py`: SQLite connection/schema management.
* `planner.py`: Purely functional state comparison.
* `executor.py`: Effectful processing of action plans.

## 📋 Procedural Workflows

### Adding a new CLI command
1. Define any new data models required in `models.py`.
2. Add the sub-parser and argument handling in `cli.py`.
3. If querying the store, add the query function in `catalog.py`.
4. If mutating state, define pure planning logic in `planner.py` and the execution step in `executor.py`.
5. Add corresponding tests in the `tests/` directory.

## 💻 Code Style: Pure vs Effectful Boundary
```python
# planner.py (Pure) - yields descriptions of work
def plan_import(source_files, catalog_state):
    for f in source_files:
        if f.hash not in catalog_state:
            yield CopyAction(f.path, f.hash)

# executor.py (Effectful) - executes descriptions
def execute_plan(plan, db_conn):
    for action in plan:
        shutil.copy(action.src, action.dest)
        db_conn.execute("INSERT ...", (action.hash,))
```
