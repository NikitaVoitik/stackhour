# Database migrations

Stackhour has two SQLite databases:

- The tracker database in `stackhour-store`.
- The control-plane database in `stackhour-domain`.

Both databases use ordered, forward-only migrations. Each database records the
version, name, and application time of every migration. Each migration runs in
one `BEGIN IMMEDIATE` transaction.

The application rejects a database with a newer migration version. This
prevents an older binary from writing data to a schema that it does not
understand.

## Add a migration

1. Increase the latest schema version constant.
2. Append one migration. Never reorder or edit an applied migration.
3. Make the migration safe for existing data.
4. Add a fixture for the prior schema.
5. Test data preservation, repeated open, rollback on failure, and resume.
6. Run `dev/verify-full`.

Tracker migrations are in `crates/stackhour-store/src/db.rs`. Control-plane
migrations are in `crates/stackhour-domain/src/store.rs`.

Stackhour does not run automatic down migrations. Restore a verified backup
when a deployment must return to an earlier schema.
