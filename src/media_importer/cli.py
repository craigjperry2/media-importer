import argparse
import sys
import logging
from .catalog import Catalog
from .planner import Planner
from .executor import Executor

logging.basicConfig(level=logging.INFO, format='%(levelname)s: %(message)s')

def main():
    parser = argparse.ArgumentParser(description="Media Consolidator & Cataloger")
    subparsers = parser.add_subparsers(dest='command', required=True)

    # scan
    scan_parser = subparsers.add_parser('scan')
    scan_parser.add_argument('--store', required=True, help='Path to store directory')
    scan_parser.add_argument('--db', required=True, help='Path to SQLite database')
    scan_parser.add_argument('--source', required=True, action='append', help='Source directories to scan')
    scan_parser.add_argument('--dry-run', action='store_true', help='Plan only, make no changes')
    scan_parser.add_argument('--rehash-all', action='store_true', help='Force rehashing of all files')

    # verify-store
    verify_parser = subparsers.add_parser('verify-store')
    verify_parser.add_argument('--store', required=True, help='Path to store directory')
    verify_parser.add_argument('--db', required=True, help='Path to SQLite database')
    verify_parser.add_argument('--dry-run', action='store_true', help='Plan only, make no changes')

    # query
    query_parser = subparsers.add_parser('query')
    query_parser.add_argument('--db', required=True, help='Path to SQLite database')
    query_parser.add_argument('--ext', help='Filter by extension')
    query_parser.add_argument('--name', help='Filter by file name')
    query_parser.add_argument('--hash', help='Filter by file hash')

    args = parser.parse_args()

    if args.command == 'scan':
        catalog = Catalog(args.db, read_only=args.dry_run)
        planner = Planner(catalog, args.store)
        actions = planner.plan_scan(args.source, args.rehash_all)
        
        if args.dry_run:
            print(f"Dry run: Planned {len(actions)} actions.")
            for action in actions:
                print(action)
        else:
            executor = Executor(catalog, args.store)
            success = executor.execute(actions)
            if not success:
                sys.exit(1)
                
    elif args.command == 'verify-store':
        catalog = Catalog(args.db, read_only=args.dry_run)
        planner = Planner(catalog, args.store)
        actions = planner.plan_verify_store()
        
        if args.dry_run:
            print(f"Dry run: Planned {len(actions)} actions.")
            for action in actions:
                print(action)
        else:
            executor = Executor(catalog, args.store)
            success = executor.execute(actions)
            if not success:
                sys.exit(1)
                
    elif args.command == 'query':
        catalog = Catalog(args.db, read_only=True)
        query = "SELECT * FROM source_files WHERE 1=1"
        params = []
        if args.ext:
            query += " AND file_format = ?"
            params.append(args.ext)
        if args.name:
            query += " AND file_name LIKE ?"
            params.append(f"%{args.name}%")
        if args.hash:
            query += " AND file_hash = ?"
            params.append(args.hash)
            
        cursor = catalog.conn.execute(query, params)
        for row in cursor:
            print(dict(row))

if __name__ == '__main__':
    main()
